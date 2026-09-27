"""#119: capability matrix must refresh without opening the command menu."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock

from muteki.conversation.manager import ConversationManager
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.external_agents.interaction_matrix import build_matrix_from_probe
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
    project = manager.create_project("c119")
    workspace = manager.plan_workspace(
        project_id=project.project_id,
        kind="local",
        root_path=str(Path(tempfile.mkdtemp())),
    )
    manager.save_workspace(workspace)
    thread = manager.create_thread(
        project_id=project.project_id,
        workspace_id=workspace.workspace_id,
        title="c119",
    )
    return thread.thread_id


class CapabilityMatrixRefreshTest(unittest.TestCase):
    def test_thread_view_triggers_refresh_when_snapshot_missing(self) -> None:
        adapter = SimpleNamespace(id="codex.app_server", identity=SimpleNamespace(instance_id="default"))
        registry = MagicMock()
        registry.record.return_value = SimpleNamespace(last_probe=None)
        registry.get.return_value = adapter

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
            manager.bind_capability_snapshot_provider(lambda _tid: None)
            triggered: list[str] = []
            manager.bind_capability_refresh_trigger(triggered.append)

            view = manager.thread_view(thread_id)
            conn = view["runtime_connection"]
            self.assertTrue(conn["configured"])
            self.assertTrue(conn["capability_stale"])
            self.assertEqual(conn["capability_revision"], 0)
            self.assertIn("会话能力目录尚未加载", conn["matrix_diagnostics"])
            self.assertEqual(triggered, [thread_id])

    def test_thread_view_skips_refresh_when_snapshot_cached(self) -> None:
        adapter = SimpleNamespace(id="codex.app_server", identity=SimpleNamespace(instance_id="default"))
        registry = MagicMock()
        registry.record.return_value = SimpleNamespace(last_probe=None)
        registry.get.return_value = adapter

        snapshot = RuntimeCapabilitySnapshot(
            adapter_id="codex.app_server",
            revision=3,
            stale=False,
            diagnostics=[],
        )
        snapshot = snapshot.model_copy(
            update={"matrix": build_matrix_from_probe(
                None, revision=3, stale=False, snapshot=snapshot,
            )},
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
            manager.bind_capability_snapshot_provider(lambda _tid: snapshot)
            triggered: list[str] = []
            manager.bind_capability_refresh_trigger(triggered.append)

            view = manager.thread_view(thread_id)
            conn = view["runtime_connection"]
            self.assertEqual(conn["capability_revision"], 3)
            self.assertFalse(conn["capability_stale"])
            self.assertEqual(triggered, [])

    def test_thread_view_skips_refresh_when_not_configured(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manager = _mgr(tmp, registry=None)
            thread_id = _thread(manager)
            triggered: list[str] = []
            manager.bind_capability_refresh_trigger(triggered.append)
            view = manager.thread_view(thread_id)
            self.assertFalse(view["runtime_connection"]["configured"])
            self.assertEqual(triggered, [])


if __name__ == "__main__":
    unittest.main()
