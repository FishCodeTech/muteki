import asyncio
import json
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from muteki.solver.btw import stream_btw_worker_deltas
from muteki.solver.cli_engines.types import CliResult


class BtwProgressTest(unittest.IsolatedAsyncioTestCase):
    def driver(self):
        from muteki.solver.cli_driver import driver_for
        return driver_for({'engine': 'codex', 'model': 'synthetic-model'})

    async def collect(self, runner, **options):
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner):
            return [item async for item in stream_btw_worker_deltas(
                driver=self.driver(), prompt='fake prompt', cwd='/unused', timeout=240, **options)]

    async def test_tool_progress_streams_before_final_answer_without_secrets(self):
        release = threading.Event()
        secret = 'fictional-tool-secret'
        def runner(*args, on_step, on_proc, **kwargs):
            on_proc(None)
            on_step(SimpleNamespace(kind='tool', text=secret, tool=secret))
            if not release.wait(2):
                raise AssertionError('progress was buffered until completion')
            on_step(SimpleNamespace(kind='tool_result', text=secret))
            on_step(SimpleNamespace(kind='reasoning', text='Answer', thinking=False))
            return CliResult(text='Answer', returncode=0)
        items = []
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner):
            async for item in stream_btw_worker_deltas(driver=self.driver(), prompt='fake', cwd='/unused',
                                                       timeout=240, include_progress=True):
                items.append(item)
                if isinstance(item, dict) and item.get('progress', {}).get('phase') == 'tool':
                    release.set()
        self.assertTrue(release.is_set())
        self.assertIn('Answer', items)
        self.assertNotIn(secret, json.dumps(items))
        self.assertIn('tool_result', [i['progress']['phase'] for i in items if isinstance(i, dict) and 'progress' in i])

    async def test_old_text_only_interface_is_preserved(self):
        def runner(*a, **kw):
            return CliResult(text='final', returncode=0)
        self.assertEqual(await self.collect(runner), ['final'])

    async def test_timeout_propagates_to_http_error_path(self):
        def runner(*a, **kw):
            return CliResult(text='', timed_out=True)
        with self.assertRaises(TimeoutError):
            await self.collect(runner, include_progress=True)

    async def test_failed_cli_is_not_reported_as_success(self):
        def runner(*a, **kw):
            return CliResult(text='', returncode=1, error='fictional-sensitive-error')
        with self.assertRaisesRegex(RuntimeError, 'BTW Worker failed') as caught:
            await self.collect(runner, include_progress=True)
        self.assertNotIn('fictional-sensitive-error', str(caught.exception))

    async def test_empty_response_is_an_error(self):
        with self.assertRaisesRegex(RuntimeError, 'no answer'):
            await self.collect(lambda *a, **kw: CliResult(text='', returncode=0))

    async def test_disconnect_signals_worker_cancellation(self):
        stopped = threading.Event()
        def runner(*a, cancel_event, **kw):
            cancel_event.wait(2)
            if cancel_event.is_set():
                stopped.set()
            return CliResult(text='', cancelled=True)
        await self.collect(runner, request_disconnected=lambda: True, include_progress=True)
        self.assertTrue(stopped.is_set())

    async def test_final_answer_is_not_lost_after_partial_stream_text(self):
        def runner(*a, on_step, **kw):
            on_step(SimpleNamespace(kind='reasoning', text='Checking...', thinking=False))
            return CliResult(text='Final answer from completed parser', returncode=0)
        items = await self.collect(runner, include_progress=True)
        self.assertIn('Checking...', items)
        self.assertIn({'answer': 'Final answer from completed parser'}, items)

    async def test_diagnostic_distinguishes_tool_activity_and_answer_without_contents(self):
        records = []
        def runner(*a, on_step, **kw):
            on_step(SimpleNamespace(kind='tool', text='fictional-command-secret'))
            on_step(SimpleNamespace(kind='tool_result', text='fictional-result-secret'))
            return CliResult(text='fictional-answer-secret', returncode=0)
        await self.collect(runner, diagnostic=records.append, include_progress=True)
        result = next(row for row in records if row['phase'] == 'cli_returned')
        self.assertEqual(result['tools'], 1)
        self.assertEqual(result['tool_results'], 1)
        self.assertEqual(result['text_chars'], 0)
        self.assertGreater(result['final_chars'], 0)
        self.assertNotIn('fictional-', json.dumps(records))

    async def test_closing_stream_joins_runner_before_exit_confirmation(self):
        exited = []
        stopped = threading.Event()
        def runner(*args, cancel_event, **kwargs):
            if not cancel_event.wait(2):
                raise AssertionError('runner was not cancelled')
            stopped.set()
            return CliResult(text='', returncode=0, cancelled=True)
        with patch('muteki.solver.cli_driver.run_cli_streaming', runner):
            stream = stream_btw_worker_deltas(driver=self.driver(), prompt='fake', cwd='/unused',
                timeout=240, include_progress=True, runtime_exit_callback=exited.append)
            await anext(stream)
            await stream.aclose()
        self.assertTrue(stopped.is_set())
        self.assertEqual(exited, [True])

    async def test_live_process_never_reports_safe_to_delete(self):
        exited = []
        def runner(*args, on_proc, **kwargs):
            on_proc(SimpleNamespace(poll=lambda: None))
            return CliResult(text='answer', returncode=0)
        await self.collect(runner, runtime_exit_callback=exited.append)
        self.assertEqual(exited, [False])

    async def test_retained_exit_query_observes_late_process_exit(self):
        queries = []
        process = SimpleNamespace(returncode=None)
        process.poll = lambda: process.returncode
        def runner(*args, on_proc, **kwargs):
            on_proc(process)
            return CliResult(text='answer', returncode=0)
        await self.collect(runner, runtime_query_callback=queries.append)
        self.assertFalse(queries[0]())
        process.returncode = 0
        self.assertTrue(queries[0]())
