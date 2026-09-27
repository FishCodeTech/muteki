"""#188: capability refresh failures must end loading with diagnostics + backoff."""

from __future__ import annotations

import asyncio
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

from muteki.conversation import events as ev
from muteki.conversation.executor import (
    ExternalAgentSessionExecutor,
    _CAPABILITY_REFRESH_BACKOFF_BASE_S,
)
from muteki.conversation.manager import ConversationManager
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.external_agents.runtime_capabilities import RuntimeCapabilitySnapshot
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.store import PlatformStore


def _mgr(tmp: str, *, registry: object | None = None) -> ConversationManager:
    store = PlatformStore(Path(tmp) / "platform.db")
    conv = ConversationStore(store)
    projection = ConversationProjection(store, conv, sessions_root=Path(tmp))
    bindings = CapabilityBindingService(store)
    return ConversationManager(
        store,
        conv,
        bindings,
        projection,
        workspace_root=Path(tmp) / "ws",
        adapter_registry=registry,
    )


def _thread(manager: ConversationManager) -> str:
    project = manager.create_project("c188")
    workspace = manager.plan_workspace(
        project_id=project.project_id,
        kind="local",
        root_path=str(Path(tempfile.mkdtemp())),
    )
    manager.save_workspace(workspace)
    thread = manager.create_thread(
        project_id=project.project_id,
        workspace_id=workspace.workspace_id,
        title="c188",
    )
    return thread.thread_id


class CapabilityRefreshFailureTest(unittest.IsolatedAsyncioTestCase):
    async def test_refresh_failure_persists_diagnostic_and_backs_off(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = PlatformStore(Path(tmp) / "platform.db")
            conv = ConversationStore(store)
            manager = SimpleNamespace(
                runtime_selection=lambda _tid: SimpleNamespace(
                    adapter_id="codex.app_server",
                    instance_id="default",
                ),
                bindings=None,
            )
            registry = SimpleNamespace(
                record=lambda *_a, **_k: SimpleNamespace(last_probe=None),
                get=lambda *_a, **_k: None,
            )
            executor = ExternalAgentSessionExecutor(
                store=store,
                conv=conv,
                manager=manager,  # type: ignore[arg-type]
                registry=registry,  # type: ignore[arg-type]
                sessions_root=Path(tmp),
            )
            thread_id = "thr-188"
            executor.runtime_capabilities = AsyncMock(  # type: ignore[method-assign]
                side_effect=RuntimeError("Invalid params: missing auth"),
            )

            started = executor.refresh_runtime_capabilities(thread_id)
            self.assertTrue(started)
            task = executor._capability_tasks[thread_id]
            with self.assertRaises(RuntimeError):
                await task
            # Let done-callback finish.
            await asyncio.sleep(0)

            snapshot = executor.cached_runtime_capabilities(thread_id)
            self.assertIsNotNone(snapshot)
            assert snapshot is not None
            self.assertTrue(snapshot.stale)
            self.assertTrue(
                any("能力刷新失败" in item for item in snapshot.diagnostics),
                snapshot.diagnostics,
            )
            failure = executor.capability_refresh_failure(thread_id)
            self.assertIsNotNone(failure)
            assert failure is not None
            self.assertEqual(failure["attempts"], 1)
            self.assertIn("Invalid params", failure["error"])
            self.assertGreaterEqual(
                failure["retry_after_seconds"],
                _CAPABILITY_REFRESH_BACKOFF_BASE_S - 0.5,
            )

            events = store.read_events("thread", thread_id, limit=50)
            error_events = [
                item for item in events
                if item.event_type == ev.EV_RUNTIME_ERROR
            ]
            updated_events = [
                item for item in events
                if item.event_type == ev.EV_RUNTIME_CAPABILITIES_UPDATED
            ]
            self.assertEqual(len(error_events), 1)
            self.assertEqual(
                error_events[0].payload.get("code"),
                "conversation.runtime.capability_refresh_failed",
            )
            self.assertEqual(len(updated_events), 1)

            # Immediate second refresh must not restart while backoff holds.
            self.assertFalse(executor.refresh_runtime_capabilities(thread_id))
            self.assertEqual(
                executor.capability_refresh_failure(thread_id)["attempts"],
                1,
            )
            self.assertEqual(
                executor.runtime_capabilities.await_count,  # type: ignore[attr-defined]
                1,
            )

    async def test_thread_view_does_not_restart_failed_refresh_within_backoff(
        self,
    ) -> None:
        adapter = SimpleNamespace(
            id="codex.app_server",
            identity=SimpleNamespace(instance_id="default"),
        )
        registry = SimpleNamespace(
            record=lambda *_a, **_k: SimpleNamespace(last_probe=None),
            get=lambda *_a, **_k: adapter,
        )
        with tempfile.TemporaryDirectory() as tmp:
            manager = _mgr(tmp, registry=registry)
            thread_id = _thread(manager)
            manager.save_runtime_selection(
                thread_id,
                {
                    "adapter_id": "codex.app_server",
                    "instance_id": "default",
                    "model": "gpt-5",
                },
                validate_credential=False,
            )
            failure_snap = RuntimeCapabilitySnapshot(
                adapter_id="codex.app_server",
                revision=0,
                stale=True,
                diagnostics=["Runtime 能力刷新失败：RuntimeError: boom"],
            )
            manager.bind_capability_snapshot_provider(lambda _tid: failure_snap)
            manager.bind_capability_failure_provider(
                lambda _tid: {
                    "error": "RuntimeError: boom",
                    "attempts": 1,
                    "retry_after_seconds": 30.0,
                    "failed_at": 1.0,
                    "backoff_seconds": 30.0,
                },
            )
            triggered: list[str] = []

            def _trigger(tid: str) -> bool:
                triggered.append(tid)
                return False  # simulate executor backoff no-op

            manager.bind_capability_refresh_trigger(_trigger)

            view1 = manager.thread_view(thread_id)
            view2 = manager.thread_view(thread_id)
            conn = view2["runtime_connection"]
            self.assertEqual(conn.get("capability_refresh_status"), "failed")
            self.assertIn("boom", str(conn.get("capability_last_error") or ""))
            self.assertEqual(conn.get("capability_refresh_attempts"), 1)
            # Trigger is invoked (executor decides), but both reads see one
            # recorded failure — not an unbounded restart loop.
            self.assertEqual(triggered, [thread_id, thread_id])
            self.assertTrue(
                any("boom" in str(item) for item in conn["matrix_diagnostics"]),
            )
            self.assertEqual(
                view1["runtime_connection"]["capability_refresh_attempts"],
                view2["runtime_connection"]["capability_refresh_attempts"],
            )

    async def test_successful_refresh_clears_failure_and_updates_revision(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            store = PlatformStore(Path(tmp) / "platform.db")
            conv = ConversationStore(store)
            manager = SimpleNamespace(
                runtime_selection=lambda _tid: SimpleNamespace(
                    adapter_id="codex.app_server",
                    instance_id="default",
                ),
                bindings=None,
            )
            registry = SimpleNamespace(
                record=lambda *_a, **_k: SimpleNamespace(last_probe=None),
                get=lambda *_a, **_k: None,
            )
            executor = ExternalAgentSessionExecutor(
                store=store,
                conv=conv,
                manager=manager,  # type: ignore[arg-type]
                registry=registry,  # type: ignore[arg-type]
                sessions_root=Path(tmp),
            )
            thread_id = "thr-188-recover"
            ok_snapshot = RuntimeCapabilitySnapshot(
                adapter_id="codex.app_server",
                revision=4,
                stale=False,
                diagnostics=[],
            )
            executor.runtime_capabilities = AsyncMock(  # type: ignore[method-assign]
                side_effect=[
                    RuntimeError("config broken"),
                    ok_snapshot,
                ],
            )
            self.assertTrue(executor.refresh_runtime_capabilities(thread_id))
            with self.assertRaises(RuntimeError):
                await executor._capability_tasks[thread_id]
            await asyncio.sleep(0)
            self.assertIsNotNone(executor.capability_refresh_failure(thread_id))

            # Expire backoff so recovery refresh may start.
            executor._capability_failures[thread_id]["next_retry_at"] = 0.0
            self.assertTrue(executor.refresh_runtime_capabilities(thread_id))
            recovered = await executor._capability_tasks[thread_id]
            await asyncio.sleep(0)
            self.assertIsNone(executor.capability_refresh_failure(thread_id))
            self.assertEqual(recovered.revision, 4)
            cached = executor.cached_runtime_capabilities(thread_id)
            self.assertIsNotNone(cached)
            assert cached is not None
            self.assertEqual(cached.revision, 4)
            self.assertFalse(cached.stale)


if __name__ == "__main__":
    unittest.main()
