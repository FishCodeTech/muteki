import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from fastapi import FastAPI
from fastapi.testclient import TestClient
from apps.web.routes.runs import register
from muteki.solver.cli_engines.types import CliResult


class BtwRouteTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.profile = {'id': 'codex-one', 'name': 'codex-one', 'engine': 'codex',
                        'credential_account': 'fake-account', 'model': 'fake-model', 'enabled': True}
        self.config = {'worker_profiles': [self.profile], 'worker_backend': 'local',
                       'stage_policy': {'coordinator': {'btw_profile': 'codex-one'}}}
        self.app = FastAPI()
        self.app.state.route_helpers = SimpleNamespace(
            _reject_temporarily_disabled_engine=lambda body: None,
            _dispatch_run_command=None, _run_receipt_status=None)
        self.app.state.manager = SimpleNamespace(
            get=lambda run_id: SimpleNamespace(name='synthetic', category='web', execution_generation=1),
            _safe_run_id=lambda run_id: run_id, workspace_dir=lambda run_id: self.root,
            graph_dir=lambda run_id: self.root, event_root=self.root, sessions_root=self.root,
            coordinator_control_dir=lambda run_id: self.root / "control" / run_id,
            state_root=self.root, load_winner_continuation=lambda run_id: {},
            worker_config=SimpleNamespace(resolve=lambda category: self.config),
            usage=SimpleNamespace(record=Mock()))
        register(self.app)

    def test_http_stream_delivers_progress_and_answer_using_selected_credentials(self):
        account = self.root / '_secrets' / 'accounts' / 'fake-account'
        account.mkdir(parents=True)
        (account / 'API_KEY').write_text('fictional-route-key')
        def runner(driver, argv, *, on_step, on_proc, env, **kwargs):
            self.assertEqual(driver.name, 'codex')
            self.assertEqual(env['OPENAI_API_KEY'], 'fictional-route-key')
            on_proc(None)
            on_step(SimpleNamespace(kind='tool', tool='shell', text='fictional-command-secret'))
            on_step(SimpleNamespace(kind='tool_result', text='fictional-output-secret'))
            return CliResult(text='synthetic answer', returncode=0)
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner), \
             patch('apps.web.routes.runs.is_web_container', create=True, return_value=False):
            response = TestClient(self.app).post('/api/runs/run-test/btw', json={'question': 'summarize'})
        self.assertEqual(response.status_code, 200)
        frames = [json.loads(line[6:]) for line in response.text.splitlines() if line.startswith('data: ')]
        self.assertTrue(any(frame.get('progress', {}).get('phase') == 'tool' for frame in frames))
        self.assertIn({'delta': 'synthetic answer'}, frames)
        self.assertEqual(frames[-1], {'done': True})
        for secret in ('fictional-route-key', 'fictional-command-secret', 'fictional-output-secret'):
            self.assertNotIn(secret, response.text)
        self.app.state.manager.usage.record.assert_called_once()

    def test_invalid_config_returns_400_before_cli_start(self):
        self.config['stage_policy']['coordinator']['btw_profile'] = 'deleted'
        with patch('muteki.solver.cli_driver.run_cli_streaming') as runner:
            response = TestClient(self.app).post('/api/runs/run-test/btw', json={'question': 'summarize'})
        self.assertEqual(response.status_code, 400)
        runner.assert_not_called()

    def test_no_profiles_does_not_fall_back_to_host_claude(self):
        self.config.clear()
        with patch('muteki.solver.cli_driver.run_cli_streaming') as runner:
            response = TestClient(self.app).post('/api/runs/run-test/btw', json={'question': 'summarize'})
        self.assertEqual(response.status_code, 400)
        runner.assert_not_called()

    def test_history_api_persists_response_and_deletes_question_answer_pair(self):
        client = TestClient(self.app)
        account = self.root / '_secrets' / 'accounts' / 'fake-account'
        account.mkdir(parents=True)
        (account / 'API_KEY').write_text('fictional-route-key')
        def runner(*a, **kw):
            return CliResult(text='saved answer', returncode=0)
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner):
            response = client.post('/api/runs/run-test/btw', json={'question': 'saved question'})
        self.assertEqual(response.status_code, 200)
        rows = client.get('/api/runs/run-test/btw/history').json()['turns']
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]['answer'], 'saved answer')
        self.assertEqual(rows[0]['status'], 'completed')
        self.assertEqual(client.get('/api/runs/another-run/btw/history').json()['turns'], [])
        self.assertEqual(client.delete('/api/runs/run-test/btw/history/' + rows[0]['id']).status_code, 200)
        self.assertEqual(client.get('/api/runs/run-test/btw/history').json()['turns'], [])
        self.assertEqual(client.delete('/api/runs/run-test/btw/history/' + rows[0]['id']).status_code, 404)

    def test_stale_client_transcript_does_not_restore_deleted_history(self):
        from muteki.solver.btw_history import BtwHistory
        history = BtwHistory(self.root / 'control' / 'run-test')
        turn = history.begin('deleted question', engine='codex', model='fake')
        history.update(turn, 'deleted answer', 'completed')
        client = TestClient(self.app)
        self.assertEqual(client.delete('/api/runs/run-test/btw/history').json()['deleted'], 1)
        account = self.root / '_secrets' / 'accounts' / 'fake-account'
        account.mkdir(parents=True)
        (account / 'API_KEY').write_text('fictional-route-key')
        prompts = []
        def runner(driver, argv, **kw):
            prompts.append(str(argv))
            return CliResult(text='new answer', returncode=0)
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner):
            client.post('/api/runs/run-test/btw', json={'question': 'new question',
                'transcript': [{'role': 'assistant', 'content': 'deleted answer'}]})
        self.assertNotIn('deleted answer', prompts[0])
        self.assertEqual(len(history.list()), 1)

    def test_unknown_run_cannot_access_history(self):
        self.app.state.manager.get = lambda run_id: None
        client = TestClient(self.app)
        for method, suffix in (('get', ''), ('delete', ''), ('delete', '/missing')):
            response = getattr(client, method)('/api/runs/unknown/btw/history' + suffix)
            self.assertEqual(response.status_code, 404)
        self.assertFalse((self.root / 'control' / 'unknown').exists())

    def test_btw_survives_main_stop_and_is_available_after_stop_in_both_modes(self):
        account = self.root / '_secrets' / 'accounts' / 'fake-account'
        account.mkdir(parents=True)
        (account / 'API_KEY').write_text('fictional-route-key')
        for mode in ('ctf', 'pentest'):
            for initially_finished in (False, True):
                with self.subTest(mode=mode, finished=initially_finished):
                    main_task = Mock()
                    main_task.done.return_value = initially_finished
                    main_cost = Mock()
                    run = SimpleNamespace(name='synthetic', category='web', mode=mode,
                        execution_generation=1, finished=initially_finished,
                        solved=False, flags=['existing-flag'], task=main_task,
                        cost=main_cost)
                    self.app.state.manager.get = lambda run_id: run
                    def runner(*args, **kwargs):
                        # A main-task stop occurs while this separate CLI is running.
                        run.finished = True
                        main_task.done.return_value = True
                        kwargs['on_step'](SimpleNamespace(kind='tool_result', text='done'))
                        return CliResult(text='answer after main stop', returncode=0)
                    with patch('muteki.solver.cli_driver.run_cli_streaming', runner):
                        response = TestClient(self.app).post('/api/runs/run-test/btw',
                            json={'question': 'progress'})
                    self.assertEqual(response.status_code, 200)
                    self.assertIn('answer after main stop', response.text)
                    self.assertIn('"done": true', response.text)
                    self.assertTrue(run.finished)
                    self.assertFalse(run.solved)
                    self.assertEqual(run.flags, ['existing-flag'])
                    main_task.cancel.assert_not_called()
                    self.assertEqual(main_cost.mock_calls, [])

    def test_worker_exception_cannot_leak_secret_to_response_or_logs(self):
        async def fail(**kwargs):
            raise RuntimeError('Authorization: Bearer fictional-secret-not-for-output')
            yield
        with patch('muteki.solver.btw.stream_btw_worker_deltas', fail), \
             patch('apps.web.drivers._standby_worker_env', return_value={}), \
             self.assertLogs('apps.web.routes.runs', level='WARNING') as logs:
            response = TestClient(self.app).post('/api/runs/run-test/btw',
                json={'question': 'test'})
        self.assertIn('RuntimeError', response.text)
        self.assertNotIn('fictional-secret-not-for-output', response.text)
        self.assertNotIn('fictional-secret-not-for-output', '\n'.join(logs.output))

    def test_container_and_home_are_owned_by_each_request(self):
        self.config['worker_backend'] = 'container'
        self.app.state.manager.account_projection_root = self.root / 'projection'
        self.app.state.manager.container_bootstrap_root = self.root / 'bootstrap'
        async def answer(**kwargs):
            kwargs['runtime_exit_callback'](True)
            yield {'answer': 'synthetic'}
        with patch('muteki.solver.btw.stream_btw_worker_deltas', answer), \
             patch('apps.web.drivers._standby_worker_env', return_value={}) as env, \
             patch('muteki.solver.container_exec.ensure_container', return_value=SimpleNamespace()) as create, \
             patch('muteki.solver.container_exec._chown_tree_to_worker'), \
             patch('muteki.solver.container_exec.teardown_container', return_value=True) as remove:
            for _ in range(2):
                response = TestClient(self.app).post('/api/runs/run-test/btw',
                    json={'question': 'test'})
                self.assertIn('synthetic', response.text)
        ids = [call.args[0] for call in create.call_args_list]
        self.assertEqual(len(ids), 2)
        self.assertNotEqual(ids[0], ids[1])
        self.assertEqual(ids, [call.args[0] for call in remove.call_args_list])
        labels = [call.kwargs['label'] for call in env.call_args_list]
        self.assertNotEqual(labels[0], labels[1])

    def test_successful_turn_cleans_scratch_but_keeps_history_and_main_workspace(self):
        account = self.root / '_secrets' / 'accounts' / 'fake-account'
        account.mkdir(parents=True)
        (account / 'API_KEY').write_text('fictional-key')
        main = self.root / 'workers' / 'main'
        main.mkdir(parents=True)
        (main / 'important').write_text('main task')
        seen = []
        def runner(*args, cwd, **kwargs):
            seen.append(Path(cwd))
            (Path(cwd) / 'temporary').write_text('scratch')
            return CliResult(text='saved answer', returncode=0)
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner):
            client = TestClient(self.app)
            response = client.post('/api/runs/run-test/btw', json={'question': 'test'})
        self.assertIn('saved answer', response.text)
        self.assertFalse(seen[0].exists())
        self.assertEqual((main / 'important').read_text(), 'main task')
        self.assertEqual(client.get('/api/runs/run-test/btw/history').json()['turns'][0]['answer'], 'saved answer')
        self.assertTrue((account / 'API_KEY').exists())

    def test_unconfirmed_runtime_retains_scratch(self):
        async def answer(**kwargs):
            kwargs['runtime_exit_callback'](False)
            yield {'answer': 'synthetic'}
        with patch('muteki.solver.btw.stream_btw_worker_deltas', answer), \
             patch('apps.web.drivers._standby_worker_env', return_value={}):
            TestClient(self.app).post('/api/runs/run-test/btw', json={'question': 'test'})
        self.assertEqual(len(list((self.root / 'workers' / '_btw').iterdir())), 1)

    def test_http_disconnect_keeps_background_answer_and_durable_history(self):
        import asyncio
        import threading
        from muteki.solver.btw_history import BtwHistory
        release = threading.Event()
        def runner(*args, **kwargs):
            if not release.wait(3):
                raise AssertionError('test did not release runner')
            return CliResult(text='answer after navigation', returncode=0)
        async def exercise():
            disconnected = asyncio.Event()
            delivered_body = False
            async def receive():
                nonlocal delivered_body
                if not delivered_body:
                    delivered_body = True
                    return {'type': 'http.request', 'body': b'{"question":"test"}', 'more_body': False}
                await disconnected.wait()
                return {'type': 'http.disconnect'}
            async def send(message):
                if message['type'] == 'http.response.body' and message.get('body'):
                    disconnected.set()
            scope = {'type': 'http', 'asgi': {'version': '3.0'}, 'http_version': '1.1',
                     'method': 'POST', 'scheme': 'http', 'path': '/api/runs/run-test/btw',
                     'raw_path': b'/api/runs/run-test/btw', 'query_string': b'',
                     'root_path': '', 'headers': [(b'content-type', b'application/json')],
                     'client': ('127.0.0.1', 1234), 'server': ('test', 80)}
            try:
                await asyncio.wait_for(self.app(scope, receive, send), 2)
                job = self.app.state.manager.btw_jobs.jobs['run-test']
                self.assertFalse(job.task.done())
                release.set()
                await asyncio.wait_for(job.task, 2)
                rows = BtwHistory(self.root / 'control' / 'run-test').list()
                self.assertEqual(rows[0]['answer'], 'answer after navigation')
                self.assertEqual(rows[0]['status'], 'completed')
            finally:
                release.set()
                await self.app.state.manager.btw_jobs.shutdown()
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner), \
             patch('apps.web.drivers._standby_worker_env', return_value={}):
            asyncio.run(exercise())

    def test_late_process_exit_retains_owner_then_cleans_scratch(self):
        process = SimpleNamespace(returncode=None)
        process.poll = lambda: process.returncode
        workdirs = []
        def runner(*args, cwd, on_proc, **kwargs):
            workdirs.append(Path(cwd))
            on_proc(process)
            return CliResult(text='answer', returncode=0)
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner), \
             patch('apps.web.drivers._standby_worker_env', return_value={}), \
             TestClient(self.app) as client:
            response = client.post('/api/runs/run-test/btw', json={'question': 'test'})
            self.assertIn('answer', response.text)
            self.assertTrue(workdirs[0].exists())
            async def verify():
                jobs = self.app.state.manager.btw_jobs
                self.assertEqual(jobs.status('run-test')['progress']['phase'], 'cleanup')
                self.assertFalse(await jobs.stop('run-test', timeout=0.01))
                process.returncode = 0
                self.assertTrue(await jobs.stop('run-test', timeout=2))
            client.portal.call(verify)
            self.assertFalse(workdirs[0].exists())
