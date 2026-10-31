import asyncio
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock
from apps.web.run_standby import shutdown
from apps.web.run_bindings import _delete_owned_run


class ShutdownIsolationTest(unittest.IsolatedAsyncioTestCase):
    async def test_btw_failure_does_not_skip_main_cleanup_in_either_mode(self):
        for mode in ('ctf', 'pentest'):
            with self.subTest(mode=mode):
                async def worker():
                    await asyncio.Event().wait()
                main = asyncio.create_task(worker())
                await asyncio.sleep(0)
                run = SimpleNamespace(run_id='run-test', mode=mode,
                    runtime_incomplete=False, task=main, standby_task=None,
                    recovery_task=None, title_task=None,
                    progress_publisher=Mock(), control_actor=None, control_journal=Mock())
                manager = SimpleNamespace(_lifecycle_lock=asyncio.Lock(),
                    runs={'run-test': run}, _standby_busy=lambda run: False,
                    _standby_cancel_timeout=lambda: 0.1,
                    btw_jobs=SimpleNamespace(shutdown=AsyncMock(side_effect=RuntimeError('failure'))))
                with self.assertRaisesRegex(RuntimeError, 'BTW'):
                    await shutdown(manager)
                self.assertTrue(main.cancelled())
                run.progress_publisher.close.assert_called_once()
                run.control_journal.close.assert_called_once()

    async def test_delete_refuses_to_touch_main_or_files_with_unsettled_btw(self):
        run = SimpleNamespace(task=Mock())
        manager = SimpleNamespace(btw_jobs=SimpleNamespace(stop=AsyncMock(return_value=False)))
        self.assertFalse(await _delete_owned_run(manager, 'run-test', run))
        run.task.cancel.assert_not_called()
