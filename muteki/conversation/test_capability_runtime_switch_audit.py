"""Capability cache/failure state must follow the selected Runtime identity."""
from __future__ import annotations

import asyncio
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from muteki.conversation.test_capability_refresh_failure import _mgr, _thread
from muteki.conversation.test_provider_matrix_cache_identity_189 import _executor, _snapshot


class CapabilityRuntimeSwitchAuditTest(unittest.IsolatedAsyncioTestCase):
    def make_executor(self, tmp):
        registry = SimpleNamespace(record=lambda *_a: SimpleNamespace(last_probe=None))
        executor = _executor(tmp, registry=registry)
        selection = SimpleNamespace(adapter_id="codex.app_server", instance_id="default")
        executor._manager.runtime_selection = lambda _tid: selection
        return executor, selection

    async def test_runtime_switch_does_not_inherit_failure_backoff(self):
        with tempfile.TemporaryDirectory() as tmp:
            executor, selection = self.make_executor(tmp)
            executor._record_capability_refresh_failure("thread", RuntimeError("old config"))
            self.assertFalse(executor._capability_refresh_allowed("thread"))
            selection.instance_id = "second"
            self.assertIsNone(executor.capability_refresh_failure("thread"))
            self.assertTrue(executor._capability_refresh_allowed("thread"))
            snapshot = executor._record_capability_refresh_failure("thread", RuntimeError("new config"))
            self.assertEqual(snapshot.instance_id, "second")
            self.assertEqual(snapshot.revision, 0)
            self.assertEqual(executor.capability_refresh_failure("thread")["attempts"], 1)
            executor._store.close()

    async def test_late_failed_probe_does_not_poison_new_runtime(self):
        with tempfile.TemporaryDirectory() as tmp:
            executor, selection = self.make_executor(tmp)
            release = asyncio.Event()
            async def old_probe(_tid):
                await release.wait()
                raise RuntimeError("old config")
            executor.runtime_capabilities = old_probe
            executor.refresh_runtime_capabilities("thread")
            task = executor._capability_tasks["thread"]
            selection.adapter_id = "claude.agent_sdk"
            release.set()
            with self.assertRaises(RuntimeError):
                await task
            await asyncio.sleep(0)
            self.assertIsNone(executor.capability_refresh_failure("thread"))
            self.assertIsNone(executor.cached_runtime_capabilities("thread"))
            executor._store.close()

    async def test_thread_view_discards_snapshot_for_old_instance(self):
        adapter = SimpleNamespace()
        registry = SimpleNamespace(
            record=lambda *_a: SimpleNamespace(last_probe=None),
            get=lambda *_a: adapter,
        )
        with tempfile.TemporaryDirectory() as tmp:
            manager = _mgr(tmp, registry=registry)
            thread_id = _thread(manager)
            manager.save_runtime_selection(thread_id, {
                "adapter_id": "codex.app_server", "instance_id": "second", "model": "gpt-5",
            }, validate_credential=False)
            manager.bind_capability_snapshot_provider(lambda _tid: _snapshot(adapter_id="codex.app_server"))
            view = manager.thread_view(thread_id)
            matrix = view["runtime_connection"]["matrix"]
            self.assertEqual(matrix["adapter_id"], "codex.app_server")
            self.assertEqual(matrix["instance_id"], "second")
            self.assertTrue(matrix["stale"])
            self.assertFalse(any(row["level"] == "supported" for row in matrix["rows"]))
            manager._store.close()


if __name__ == "__main__":
    unittest.main()
