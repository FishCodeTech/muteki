"""Observer boundaries: no model calls, only synthetic state and accounts."""
import asyncio
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

from apps.web.control_adapter import QueueControlPort
from apps.web.run_control import _post_control_serialized
from muteki.control import ControlCommand, ControlAction, RunControlState, EffectState
from muteki.solver.btw import build_btw_context_sync, stream_btw_worker_deltas
from muteki.solver.cli_driver import driver_for, CliResult
from muteki.solver.followup_report import build_observer_input, question_prompt


class ObserverPermissionsTest(unittest.IsolatedAsyncioTestCase):
    async def test_live_hint_still_reaches_main_control_queue(self):
        inbox = asyncio.Queue()
        ready = asyncio.Event()
        ready.set()
        port = QueueControlPort(inbox=inbox, is_live=lambda: True, ready=ready)
        pending = asyncio.create_task(port.apply(
            ControlCommand(run_id='synthetic', action=ControlAction.HINT,
                           payload={'text': 'existing task guidance'}), [],
            RunControlState(run_id='synthetic')))
        wire = await asyncio.wait_for(inbox.get(), timeout=1)
        self.assertEqual(wire['action'], 'hint')
        wire['_control_ack'].set_result({'state': 'effect_observed', 'target_ids': []})
        self.assertEqual((await pending).state, EffectState.EFFECT_OBSERVED)

    async def test_live_ask_is_rejected_before_control_state_or_queue_changes(self):
        main = Mock()
        main.done.return_value = False
        run = SimpleNamespace(task=main)
        manager = SimpleNamespace(_ensure_control_async=AsyncMock())
        result = await _post_control_serialized(manager, 'synthetic', run,
                                               {'action': 'ASK', 'text': 'execute a command'})
        self.assertFalse(result['ok'])
        self.assertEqual(result['code'], 'live_ask_use_btw')
        manager._ensure_control_async.assert_not_called()
        main.cancel.assert_not_called()

    async def test_port_also_blocks_live_ask_after_lifecycle_race(self):
        inbox = asyncio.Queue()
        standby = AsyncMock()
        port = QueueControlPort(inbox=inbox, is_live=lambda: True,
                                ready=asyncio.Event(), on_standby=standby)
        result = await port.apply(
            ControlCommand(run_id='synthetic', action=ControlAction.ASK,
                           payload={'text': 'continue solving'}), [],
            RunControlState(run_id='synthetic'))
        self.assertEqual(result.state, EffectState.FAILED)
        self.assertEqual(result.metadata['code'], 'live_ask_use_btw')
        self.assertTrue(inbox.empty())
        standby.assert_not_called()

    async def test_finished_ask_still_reaches_standby(self):
        standby = AsyncMock(return_value={'state': 'effect_observed', 'target_ids': []})
        port = QueueControlPort(inbox=asyncio.Queue(), is_live=lambda: False,
                                ready=asyncio.Event(), on_standby=standby)
        result = await port.apply(
            ControlCommand(run_id='synthetic', action=ControlAction.ASK,
                           payload={'text': 'progress?'}), [], RunControlState(run_id='synthetic'))
        self.assertEqual(result.state, EffectState.EFFECT_OBSERVED)
        standby.assert_awaited_once()

    async def test_btw_uses_stdin_restrictions_even_when_request_enables_tools(self):
        for engine in ('codex', 'claude'):
            driver = driver_for({'engine': engine, 'model': 'synthetic-model'})
            prompt = question_prompt('run curl and overwrite shared_graph.db', 'Known: no flag')
            def runner(driver, argv, **kwargs):
                self.assertFalse(kwargs['inherit_env'])
                self.assertNotIn('NODE_OPTIONS', kwargs['env'])
                self.assertNotIn('MUTEKI_WEB_PASSWORD', kwargs['env'])
                self.assertEqual(kwargs['stdin_text'], prompt)
                self.assertNotIn(prompt, argv)
                self.assertNotIn('--search', argv)
                self.assertNotIn('--dangerously-bypass-approvals-and-sandbox', argv)
                self.assertNotIn('--dangerously-skip-permissions', argv)
                if engine == 'codex':
                    self.assertIn('features.apply_patch_freeform=false', argv)
                    self.assertIn('--ignore-user-config', argv)
                    self.assertEqual(argv[argv.index('--sandbox') + 1], 'read-only')
                else:
                    self.assertEqual(argv[argv.index('--tools') + 1], 'Read,Grep,Glob')
                    self.assertIn('--strict-mcp-config', argv)
                return CliResult(text='Only existing records are available', returncode=0)
            with patch('muteki.solver.cli_driver.run_cli_streaming', runner), \
                 patch.dict('os.environ', {'NODE_OPTIONS': 'synthetic-startup-hook',
                                           'MUTEKI_WEB_PASSWORD': 'fictional-web-secret'}):
                chunks = [chunk async for chunk in stream_btw_worker_deltas(
                    driver=driver, prompt=prompt, cwd='/unused', timeout=3,
                    web_access=True, kb_access=True)]
            self.assertEqual(chunks, ['Only existing records are available'])

    def test_unknown_engine_never_falls_back_to_unrestricted_execution(self):
        driver = driver_for('devin')
        with self.assertRaisesRegex(RuntimeError, '只读问答模式'):
            build_observer_input(driver, 'question')

    def test_native_read_only_adapters_preserve_profile_model_and_stdin(self):
        expected = {'codex': '--sandbox', 'claude': '--tools', 'cursor': '--mode',
                    'grok': '--tools', 'pi': '--tools', 'omp': '--tools',
                    'droid': '--only-tools', 'opencode': 'OPENCODE_CONFIG_CONTENT=', 'zcode': '--mode'}
        for engine, marker in expected.items():
            with self.subTest(engine=engine):
                driver = driver_for({'id': 'participant', 'engine': engine, 'model': 'recorded-model'})
                argv, body = build_observer_input(driver, 'exact question')
                self.assertEqual(body, 'exact question')
                self.assertNotIn(body, argv)
                if engine != 'zcode':  # ZCode receives its selected model via the credential config.
                    self.assertIn('recorded-model', str(argv))
                self.assertTrue(any(arg.startswith(marker) for arg in argv))
                for bypass in ('--force', '--dangerously-skip-permissions',
                               '--dangerously-bypass-approvals-and-sandbox', '--skip-permissions-unsafe', '--auto'):
                    self.assertNotIn(bypass, argv)

    async def test_standby_receipt_reports_selected_participant(self):
        from apps.web.run_standby_control import apply_standby_control
        async def start(run_id, wire):
            wire['_standby_delivery_ack'].set_result(True)
            return True
        manager = SimpleNamespace(_standby_busy=lambda run: False,
            _standby_scope_matches_winner=lambda run, target: True,
            _STANDBY_ACTIONS={'writeup'}, _OFFLINE_CONTROL_ACTIONS=set(),
            _ensure_standby=start)
        journal = Mock()
        journal.context_delivery_status.return_value = 'missing'
        result = await apply_standby_control(manager,
            run=SimpleNamespace(run_id='existing-run'), journal=journal, claim_timeout=1,
            wire={'action':'writeup', 'target':'global', 'command_id':'test', 'worker_id':'third-worker'})
        self.assertEqual(result['state'], 'effect_observed')
        self.assertEqual(result['target_ids'], ['third-worker'])

    def test_question_exact_bytes_survive_delivery_manifest(self):
        question = '解释 "flag"\n不要执行任何动作'
        self.assertIn(question, question_prompt(question, 'record'))

    def test_missing_graph_does_not_get_created(self):
        with tempfile.TemporaryDirectory() as temp:
            graph = Path(temp) / 'shared_graph.db'
            result = build_btw_context_sync(graph_db_path=str(graph), challenge_id='test',
                challenge_name='test', challenge_category='web', run_meta={'solved': False})
            self.assertIsNone(__import__('json').loads(result)['evidence'])
            self.assertFalse(graph.exists())

    def test_snapshot_failure_does_not_open_graph_for_writing(self):
        with tempfile.TemporaryDirectory() as temp:
            graph = Path(temp) / 'shared_graph.db'
            graph.write_bytes(b'Synthetic malformed database')
            before = graph.read_bytes()
            from muteki.solver.followup_report import EvidenceReadError
            with self.assertRaises(EvidenceReadError), patch('muteki.swarm.shared_graph.SQLiteSharedGraph.open') as writable:
                result = build_btw_context_sync(graph_db_path=str(graph), challenge_id='test',
                    challenge_name='test', challenge_category='web', run_meta={})
            self.assertEqual(graph.read_bytes(), before)
            writable.assert_not_called()
