"""No real CLI, credentials, network requests, or task mutations."""
import shutil
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

from muteki.models.solve_graph import Challenge
from muteki.solver.cli_driver import CliResult
from muteki.solver.cli_engines.registry import driver_for
from muteki.solver.cli_solver import CliSolver
from muteki.solver.followup_report import followup_status, report_snapshot


class WriteupReportingTest(unittest.IsolatedAsyncioTestCase):
    async def test_ask_execution_request_cannot_restore_tools_or_main_workdir(self):
        for engine in ('codex', 'claude'):
            worker = self.worker(action='ask', engine=engine)
            worker.hitl_cmd['text'] = 'Run commands and change the task files'
            task_file = self.root / 'task.txt'
            task_file.write_text('original')
            _, call = await self.run_report(worker)
            self.assertNotEqual(Path(call.kwargs['cwd']), self.root)
            self.assertIn('禁止修改任务文件', call.kwargs['stdin_text'])
            worker._write_board_file.assert_not_called()
            worker._board_context.assert_not_called()
            self.assertEqual(task_file.read_text(), 'original')
            if engine == 'claude':
                self.assertEqual(call.args[0][call.args[0].index('--tools') + 1], 'Read,Grep,Glob')
            else:
                self.assertIn('features.apply_patch_freeform=false', call.args[0])

    def test_direct_ask_resume_attempt_is_converted_to_fresh_observation(self):
        worker = self.worker(action='ask')
        argv, _ = worker._resume_invocation('question', 'old-solve-session')
        self.assertNotIn('resume', argv)
        self.assertNotIn('old-solve-session', argv)
        self.assertEqual(argv.session, '')
        self.assertIn('features.apply_patch_freeform=false', argv)

    def test_ask_does_not_stage_mutation_skills(self):
        worker = self.worker(action='ask')
        worker.shared_graph.db_path = '/synthetic/graph.db'
        with patch('muteki.solver.cli_process.stage_blackboard_skill') as skill, \
             patch.object(worker, '_ensure_role_contract') as role:
            env = worker._worker_env(str(self.root))
        skill.assert_not_called()
        role.assert_not_called()
        self.assertEqual(env['MUTEKI_BLACKBOARD_DB'], '')
        self.assertEqual(env['MUTEKI_BLACKBOARD_SCRIPT'], '')

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def worker(self, *, engine='codex', action='writeup', flags=(), solved=False,
               finished=True, resume='old-solve-session'):
        worker = CliSolver(
            None, Challenge(id='test', name='Synthetic challenge', category='web',
                            description='Synthetic challenge'),
            engine=engine, driver=driver_for({'id': 'synthetic', 'engine': engine,
                                             'model': 'fake-report-model'}),
            mode='respond', workdir=str(self.root), resume_session=resume,
            hitl_cmd={'action': action, 'text': '总结当前进度',
                      'task_status': {'solved': solved, 'finished': finished}},
            found_flags=list(flags))
        worker.shared_graph = Mock()
        worker.shared_graph.snapshot.return_value = Challenge(id='test', name='接口返回 403', category='web')
        worker.shared_graph.events.return_value = [{'seq': 1, 'payload': {'fact': '接口返回 403'}}]
        worker._emit = AsyncMock()
        worker._write_board_file = Mock()
        worker._board_context = Mock(return_value='SOLVE-ONLY: query the LIVE board and write-fact')
        worker._note_cli_session = AsyncMock()
        worker._emit_empty_stderr_diagnostic = AsyncMock()
        worker._stream_cost = AsyncMock()
        worker._note_worker_stop = Mock()
        worker._run_invocation = AsyncMock(return_value=CliResult(text='报告正文', returncode=0))
        return worker

    async def run_report(self, worker):
        try:
            result = await worker._run_respond()
            call = worker._run_invocation.call_args
            evidence = Path(call.kwargs['cwd']) / 'followup-records.json'
            self.evidence = evidence.read_text()
            return result, call
        finally:
            if getattr(worker, '_owned_scratch', None):
                shutil.rmtree(worker._owned_scratch, ignore_errors=True)

    async def test_unsolved_report_is_fresh_and_inline(self):
        worker = self.worker()
        result, call = await self.run_report(worker)
        argv = call.args[0]
        prompt = call.kwargs['stdin_text']
        self.assertEqual(result.reply, '报告正文')
        self.assertIsNone(result.flag)
        self.assertFalse(result.solved)
        self.assertIsNone(result.session)
        self.assertIn('尚未获得 Flag，未解决', self.evidence)
        self.assertIn('接口返回 403', self.evidence)
        self.assertIn('建议下一步（仅建议，不执行）', prompt)
        self.assertNotIn('SOLVE-ONLY', prompt)
        self.assertNotIn('old-solve-session', argv)
        self.assertEqual(argv.session, '')
        self.assertEqual(argv.prompt, prompt)
        self.assertNotEqual(call.kwargs['cwd'], str(self.root))
        worker._board_context.assert_not_called()
        worker._write_board_file.assert_not_called()
        worker.shared_graph.snapshot.assert_called_once()
        self.assertEqual(worker._already_found, set())

    async def test_completed_report_uses_accepted_flags(self):
        worker = self.worker(flags=['flag{synthetic}'], solved=True)
        _, call = await self.run_report(worker)
        self.assertIn('已解决', self.evidence)
        self.assertIn('flag{synthetic}', self.evidence)
        self.assertEqual(worker._already_found, {'flag{synthetic}'})

    async def test_partial_flags_are_not_a_completed_solve(self):
        worker = self.worker(flags=['flag{one}'])
        worker.challenge.expected_flags = 2
        _, call = await self.run_report(worker)
        self.assertIn('已有部分 Flag，任务尚未确认解决', self.evidence)
        self.assertIn('"目标 Flag 数": 2', __import__('json').loads(self.evidence)['status'])

    async def test_live_task_uses_same_observational_report(self):
        worker = self.worker(finished=False)
        _, call = await self.run_report(worker)
        self.assertIn('"任务已结束": false', __import__('json').loads(self.evidence)['status'])
        self.assertEqual(worker.shared_graph.method_calls[0][0], 'snapshot')
        self.assertEqual(len(worker.shared_graph.method_calls), 2)

    async def test_container_report_scratch_stays_inside_mount(self):
        worker = self.worker()
        worker.container = SimpleNamespace(image='synthetic-container', to_container_path=lambda path: path)
        with patch('muteki.solver.container_exec._chown_tree_to_worker') as chown:
            _, call = await self.run_report(worker)
        cwd = Path(call.kwargs['cwd'])
        self.assertEqual(cwd.parent, self.root)
        chown.assert_called_with(str(cwd), image='synthetic-container')
        self.assertFalse(cwd.exists())

    def test_missing_status_is_not_assumed_solved(self):
        worker = self.worker(flags=['flag{one}'])
        worker.hitl_cmd.pop('task_status')
        self.assertIn('任务尚未确认解决', followup_status(worker))

    def test_inconsistent_historical_solved_bit_does_not_invent_a_flag(self):
        self.assertIn('尚未获得 Flag，未解决', followup_status(self.worker(solved=True)))

    def test_unavailable_snapshot_has_typed_error_without_fallback(self):
        from muteki.solver.followup_report import EvidenceReadError
        worker = self.worker()
        worker.shared_graph.snapshot.side_effect = OSError('private detail')
        with self.assertRaises(EvidenceReadError) as caught:
            report_snapshot(worker)
        self.assertEqual(caught.exception.code, 'followup.evidence_unreadable')
        self.assertNotIn('private detail', str(caught.exception))
        worker._board_context.assert_not_called()

    def test_evidence_is_complete_beyond_old_summary_limits(self):
        worker = self.worker()
        worker.shared_graph.events.return_value = [{'payload': {'fact': 'x' * 100000 + 'last-evidence'}}]
        self.assertIn('last-evidence', report_snapshot(worker))

    def test_codex_report_restricts_commands_and_preserves_provider(self):
        worker = self.worker()
        argv, body = worker._execute_invocation('Synthetic report', None)
        argv = worker._apply_runtime_argv(argv, {'OPENAI_API_KEY': 'fictional-key',
                                               'OPENAI_BASE_URL': 'https://synthetic.invalid/v1'})
        self.assertIn('model_providers.muteki.base_url=https://synthetic.invalid/v1', argv)
        self.assertIn('fake-report-model', argv)
        self.assertIn('features.apply_patch_freeform=false', argv)
        self.assertIn('code_mode', argv)
        self.assertIn('web_search="disabled"', argv)
        self.assertIn('--ignore-user-config', argv)
        self.assertIn('approval_policy="never"', argv)
        self.assertEqual(argv[argv.index('--sandbox') + 1], 'read-only')
        self.assertNotIn('--dangerously-bypass-approvals-and-sandbox', argv)
        self.assertEqual(argv.count('--ephemeral'), 1)
        self.assertIn('-', argv)
        self.assertEqual(argv[argv.index('--model') + 1], 'fake-report-model')
        self.assertEqual(body, 'Synthetic report')
        self.assertNotIn('fictional-key', str(argv))

    def test_claude_report_has_no_tools_or_mcp(self):
        worker = self.worker(engine='claude')
        argv, _ = worker._execute_invocation('Synthetic report', None)
        self.assertEqual(argv[argv.index('--tools') + 1], 'Read,Grep,Glob')
        self.assertIn('--strict-mcp-config', argv)
        self.assertEqual(argv.count('--no-session-persistence'), 1)
        self.assertIn('fake-report-model', argv)
        self.assertNotIn('--dangerously-skip-permissions', argv)

    def test_main_workers_keep_existing_execution_capabilities(self):
        for engine, flag in [('codex', '--dangerously-bypass-approvals-and-sandbox'),
                             ('claude', '--dangerously-skip-permissions')]:
            worker = self.worker(engine=engine)
            worker.mode = 'bootstrap'
            argv, _ = worker._execute_invocation('Ordinary worker', None)
            self.assertIn(flag, argv)
            self.assertNotIn('features.apply_patch_freeform=false', argv)
            self.assertNotIn('--tools', argv)

    async def test_ask_sees_flag_state_without_restoring_solve_session(self):
        for flags, expected in [((), '尚未获得 Flag'), (('flag{one}',), '已有部分 Flag')]:
            worker = self.worker(action='ask', flags=flags)
            _, call = await self.run_report(worker)
            self.assertIn(expected, self.evidence)
            self.assertNotIn('old-solve-session', call.args[0])
            worker._board_context.assert_not_called()
            self.assertIn('features.apply_patch_freeform=false', call.args[0])

    def test_cursor_uses_native_read_only_mode(self):
        worker = self.worker(engine='cursor')
        argv, body = worker._execute_invocation('Synthetic report', None)
        self.assertEqual(argv[argv.index('--mode') + 1], 'ask')
        self.assertNotIn('--force', argv)
        self.assertEqual(body, 'Synthetic report')

    def test_missing_read_only_capability_fails_explicitly(self):
        from muteki.solver.followup_report import ObserverCapabilityError
        worker = self.worker(engine='devin')
        with self.assertRaises(ObserverCapabilityError):
            worker._execute_invocation('Synthetic report', None)

    def test_report_environment_has_no_solve_skills_or_board_access(self):
        worker = self.worker()
        worker.shared_graph.db_path = '/synthetic/shared_graph.db'
        worker.plugin_service = Mock()
        with patch('muteki.solver.cli_process.stage_blackboard_skill') as skill, \
             patch('muteki.solver.cli_process.stage_ctf_auxiliary_skills') as auxiliary, \
             patch.object(worker, '_ensure_role_contract') as role:
            env = worker._worker_env(str(self.root))
        skill.assert_not_called()
        auxiliary.assert_not_called()
        role.assert_not_called()
        worker.plugin_service.stage_worker.assert_not_called()
        worker.plugin_service.stage_native_worker.assert_not_called()
        self.assertEqual(env['MUTEKI_BLACKBOARD_DB'], '')
        self.assertEqual(env['MUTEKI_BLACKBOARD_SCRIPT'], '')
        self.assertEqual(env['MUTEKI_BLACKBOARD_INGRESS_DIR'], '')

    def test_secret_preflight_is_not_bypassed(self):
        worker = self.worker()
        worker._control_secret_values = ['fictional-secret']
        with patch.object(worker.driver, 'secure_prompt_preflight', return_value=(False, 'unsupported')):
            with self.assertRaisesRegex(Exception, 'unsupported'):
                worker._execute_invocation('Report with fictional-secret', None)
        worker._run_invocation.assert_not_called()

    def test_case_insensitive_action_cannot_bypass_report_restrictions(self):
        worker = self.worker(action='WRITEUP')
        argv, _ = worker._execute_invocation('Synthetic report', None)
        self.assertIn('features.apply_patch_freeform=false', argv)
        self.assertNotIn('--dangerously-bypass-approvals-and-sandbox', argv)

    def test_mark_false_keeps_real_solve_capabilities(self):
        worker = self.worker(action='mark_false')
        argv, _ = worker._execute_invocation('Reopen rejected flag', None)
        self.assertIn('--dangerously-bypass-approvals-and-sandbox', argv)
        self.assertNotIn('features.apply_patch_freeform=false', argv)


if __name__ == '__main__':
    unittest.main()
