"""Offline credential regressions. All secrets and endpoints are synthetic."""
import asyncio
import contextlib
import io
import json
import os
import shutil
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

from apps.web.drivers import _standby_profile_for, _standby_worker_env
from muteki.models.solve_graph import Challenge
from muteki.solver.cli_driver import driver_for
from muteki.solver.cli_engines.types import CliResult
from muteki.solver.cli_solver import CliSolver
from muteki.solver.credential_accounts import runtime_env_for_engine
from muteki.swarm.coordinator_race import _RaceRunMixin


class StandbyCredentialsTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.accounts = self.root / 'accounts'
        account = self.accounts / 'fake-claude'
        account.mkdir(parents=True)
        self.secret = 'fictional-standby-test-key'
        (account / 'API_KEY').write_text(self.secret)
        (account / 'BASE_URL').write_text('https://provider.invalid')
        self.profile = dict(id='claude-test', name='claude-test', engine='claude',
                            credential_account='fake-claude', model='gemini-3.8-flash',
                            base_url='https://provider.invalid', credential_mode='api')
        self.host = {'HOME': str(self.root), 'PATH': os.environ.get('PATH', ''),
                     'ANTHROPIC_API_KEY': 'fictional-host-key',
                     'ANTHROPIC_AUTH_TOKEN': 'fictional-host-token',
                     'ANTHROPIC_BASE_URL': 'https://host.invalid'}
        self.env_patch = patch.dict(os.environ, self.host, clear=True)
        self.env_patch.start()
        self.addCleanup(self.env_patch.stop)

    def standby(self, profile=None, engine='claude'):
        return _standby_worker_env(root=self.root, label='standby', engine=engine,
                                   profile=self.profile if profile is None else profile,
                                   account_root=self.accounts, container=None)

    def test_explicit_account_overrides_host_and_resets_oauth(self):
        env = self.standby()
        self.assertEqual(env['ANTHROPIC_API_KEY'], self.secret)
        self.assertEqual(env['ANTHROPIC_AUTH_TOKEN'], self.secret)
        self.assertEqual(env['ANTHROPIC_BASE_URL'], 'https://provider.invalid')
        self.assertEqual(env['CLAUDE_CODE_OAUTH_TOKEN'], '')

    def test_normal_worker_matches_standby(self):
        owner = SimpleNamespace(workspace_root=self.root, worker_root=None,
                                credential_accounts_root=self.accounts, _agent_state_dirs=set())
        ordinary = _RaceRunMixin._runtime_env_for(owner, 'claude', 'ordinary',
                                                container=None, profile=self.profile)
        self.assertEqual(ordinary, self.standby())

    def test_host_api_key_and_bearer_are_resolved(self):
        for name in ('ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN'):
            with self.subTest(auth=name):
                source = {name: 'fictional-host-secret', 'ANTHROPIC_BASE_URL': 'https://host.invalid'}
                env = runtime_env_for_engine('claude', account_id='', env=source).env
                self.assertEqual(env.get(name), source[name])
                self.assertEqual(env.get('ANTHROPIC_BASE_URL'), source['ANTHROPIC_BASE_URL'])

    def test_missing_account_does_not_borrow_host_credentials(self):
        profile = {**self.profile, 'credential_account': 'missing'}
        with self.assertRaisesRegex(RuntimeError, 'credential account is unavailable'):
            self.standby(profile)

    def test_empty_account_does_not_borrow_host_credentials(self):
        (self.accounts / 'fake-claude' / 'API_KEY').write_text('')
        with self.assertRaisesRegex(RuntimeError, 'credential account is unavailable'):
            self.standby()

    def test_explicit_empty_source_stays_empty(self):
        env = runtime_env_for_engine('claude', account_id='', env={}).env
        self.assertFalse(env.get('ANTHROPIC_API_KEY'))
        self.assertFalse(env.get('ANTHROPIC_AUTH_TOKEN'))

    def test_oauth_account_remains_isolated(self):
        account = self.accounts / 'fake-claude'
        (account / 'API_KEY').unlink()
        (account / 'CLAUDE_CODE_OAUTH_TOKEN').write_text('fictional-oauth-token')
        env = self.standby()
        self.assertEqual(env['CLAUDE_CODE_OAUTH_TOKEN'], 'fictional-oauth-token')
        self.assertFalse(env['ANTHROPIC_API_KEY'])
        self.assertFalse(env['ANTHROPIC_AUTH_TOKEN'])
        self.assertFalse(env['ANTHROPIC_BASE_URL'])

    def test_container_resolver_keeps_file_backed_secrets(self):
        env = runtime_env_for_engine('claude', account_id='fake-claude',
                                     account_root=self.accounts, container=True).env
        self.assertEqual(env['ANTHROPIC_API_KEY_FILE'], '/run/muteki/accounts/fake-claude/API_KEY')
        self.assertNotIn('ANTHROPIC_API_KEY', env)
        self.assertNotIn(self.secret, json.dumps(env))

    def test_profile_identity_survives_json_reload(self):
        profiles = json.loads(json.dumps([self.profile, {**self.profile, 'id': 'other', 'name': 'other'}]))
        self.assertEqual(_standby_profile_for('claude-test', profiles), self.profile)
        self.assertIsNone(_standby_profile_for('removed', profiles))

    def test_codex_account_is_unchanged(self):
        account = self.accounts / 'fake-codex'
        account.mkdir()
        (account / 'API_KEY').write_text('fictional-codex-key')
        profile = dict(id='codex-test', engine='codex', credential_account='fake-codex')
        env = self.standby(profile, engine='codex')
        self.assertEqual(env['OPENAI_API_KEY'], 'fictional-codex-key')
        self.assertNotIn('ANTHROPIC_API_KEY', env)

    def followup(self, action, *, inherited=False):
        profile = {**self.profile, 'credential_account': ''} if inherited else self.profile
        worker = CliSolver(None, Challenge(id='test', name='test', category='web', description='test'),
                           engine='claude', driver=driver_for(profile), mode='respond',
                           workdir=str(self.root), worker_env=self.standby(profile),
                           resume_session='fake-session', hitl_cmd={'action': action, 'text': 'summarize'})
        events = []
        async def emit(event_type, **payload):
            events.append((str(event_type), payload))
        worker._emit = emit
        worker._write_board_file = lambda wd: None
        worker._board_context = lambda: ''
        worker._note_cli_session = AsyncMock()
        worker._emit_empty_stderr_diagnostic = AsyncMock()
        worker._stream_cost = AsyncMock()
        worker._note_worker_stop = lambda reason: None
        async def invoke(argv, *, cwd, timeout, stdin_text=None):
            # Use the real final environment and argv builders; stop before any CLI/API call.
            if action in {'ask', 'writeup'}:
                self.addCleanup(shutil.rmtree, cwd, ignore_errors=True)
            env = worker._worker_env(cwd)
            argv = worker._apply_runtime_argv(argv, env)
            expected = self.host['ANTHROPIC_API_KEY'] if inherited else self.secret
            self.assertEqual(env.get('ANTHROPIC_API_KEY'), expected)
            self.assertEqual(env['ANTHROPIC_MODEL'], profile['model'])
            self.assertEqual(argv[argv.index('--model') + 1], profile['model'])
            if action in {'ask', 'writeup'}:
                self.assertNotIn('fake-session', argv)
                self.assertEqual(argv[argv.index('--tools') + 1], 'Read,Grep,Glob')
            else:
                self.assertIn('fake-session', argv)
            self.assertEqual(env['CLAUDE_CONFIG_DIR'], str(Path(cwd).resolve() / '.muteki-claude-config'))
            self.assertNotIn(expected, str(argv))
            return CliResult(text='Synthetic response', returncode=0)
        worker._run_invocation = invoke
        with contextlib.ExitStack() as stack:
            for name in ('stage_blackboard_skill', 'stage_ctf_auxiliary_skills', 'stage_agent_browser_skill'):
                stack.enter_context(patch('muteki.solver.cli_process.' + name))
            stack.enter_context(patch.object(worker, '_ensure_role_contract'))
            outcome = asyncio.run(worker._run_respond())
        self.assertEqual(outcome.reply, 'Synthetic response')
        self.assertNotIn(self.secret, json.dumps(events))
        self.assertNotIn(self.host['ANTHROPIC_API_KEY'], json.dumps(events))

    def test_ask_followup_explicit(self):
        self.followup('ask')

    def test_writeup_followup_explicit(self):
        self.followup('writeup')

    def test_ask_followup_inherited(self):
        self.followup('ask', inherited=True)

    def test_writeup_followup_inherited(self):
        self.followup('writeup', inherited=True)

    def test_credentials_not_logged(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            with self.assertNoLogs(level='DEBUG'):
                self.standby()
                with self.assertRaises(RuntimeError) as caught:
                    self.standby({**self.profile, 'credential_account': 'missing'})
        self.assertNotIn(self.secret, output.getvalue() + str(caught.exception))
        self.assertNotIn(self.host['ANTHROPIC_API_KEY'], output.getvalue() + str(caught.exception))


if __name__ == '__main__':
    unittest.main()
