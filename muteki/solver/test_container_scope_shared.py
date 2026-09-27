"""Trusted shared container ownership and isolated Run workspace contracts."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from apps.web.run_bindings import prepare_shared_workspace
from apps.web.storage_layout import StorageLayout
from apps.web.worker_config import WorkerConfigStore
from muteki.solver import container_exec as ce


class SharedScopeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.layout = StorageLayout.resolve(
            sessions_root=self.base / "sessions", state_root=self.base / "state")
        self.layout.prepare()
        self.mgr = SimpleNamespace(storage=self.layout)

    def test_config_round_trip_and_dedicated_mount(self) -> None:
        settings = WorkerConfigStore(str(self.base / "worker-settings"))
        settings.set(worker_container_scope="shared")
        self.assertEqual(settings.get()["worker_container_scope"], "shared")
        unrelated = self.layout.workspace("run-only")
        unrelated.mkdir(parents=True)
        (unrelated / "private.txt").write_text("not shared")
        first = prepare_shared_workspace(self.mgr, "a")
        second = prepare_shared_workspace(self.mgr, "b")
        self.assertTrue(first.is_symlink())
        self.assertTrue(second.is_symlink())
        self.assertEqual(first.resolve(), self.layout.shared_workspace("a").resolve())
        self.assertEqual(second.resolve(), self.layout.shared_workspace("b").resolve())
        self.assertEqual(sorted(p.name for p in self.layout.shared_worker_mount().iterdir()),
                         ["a", "b"])
        self.assertEqual((unrelated / "private.txt").read_text(), "not shared")

    def test_existing_run_data_cannot_be_moved_into_shared_pool(self) -> None:
        old = self.layout.workspace("existing")
        old.mkdir(parents=True)
        (old / "note.txt").write_text("keep")
        with self.assertRaisesRegex(RuntimeError, "not empty"):
            prepare_shared_workspace(self.mgr, "existing")
        self.assertFalse(old.is_symlink())
        self.assertEqual((old / "note.txt").read_text(), "keep")

    def test_owner_policy_token_and_scoped_teardown(self) -> None:
        root = str(self.layout.runtime_root)
        token_a = ce._register_shared_owner("a", root, "same-policy")
        token_b = ce._register_shared_owner("b", root, "same-policy")
        self.assertNotEqual(token_a, token_b)
        self.assertEqual(ce._register_shared_owner("a", root, "same-policy"), token_a)
        with self.assertRaisesRegex(RuntimeError, "policy changed"):
            ce._register_shared_owner("a", root, "different-policy")
        with patch.object(ce, "_container_state", return_value="running"), \
             patch("muteki.solver.control_client.health", return_value={"scoped_teardown": True}), \
             patch("muteki.solver.control_client.teardown_owner", return_value=True) as stop, \
             patch("muteki.solver.control_receiver.ControlReceiver.instance",
                   return_value=SimpleNamespace(has_link=lambda _run_id: True)), \
             patch.object(ce, "_docker") as docker:
            self.assertTrue(ce.teardown_container("a", container_scope="shared",
                                                  bootstrap_root=root))
            stop.assert_called_once_with(ce._shared_runtime_id(root), "a", token_a)
            docker.assert_not_called()
        self.assertFalse(ce._shared_lease_path("a", root).exists())
        self.assertTrue(ce._shared_lease_path("b", root).exists())
        with patch.object(ce, "_container_state", return_value="running"), \
             patch.object(ce, "_container_absence_proven", return_value=True), \
             patch.object(ce, "_container_bootstrap_source", return_value=None), \
             patch.object(ce, "_docker", return_value=subprocess.CompletedProcess([], 0, "", "")) as docker, \
             patch("muteki.solver.control_client.confirm_run_absent"), \
             patch("muteki.solver.control_receiver.ControlReceiver.instance",
                   return_value=SimpleNamespace(has_link=lambda _run_id: True,
                                                forget=lambda _run_id: None)):
            self.assertTrue(ce.teardown_container("b", container_scope="shared",
                                                  bootstrap_root=root))
            docker.assert_called_once_with("rm", "-f",
                                           ce._run_container_name(ce._shared_runtime_id(root)),
                                           timeout=20)
        self.assertFalse(ce._shared_lease_path("b", root).exists())
        next_token = ce._register_shared_owner("a", root, "same-policy")
        self.assertNotEqual(next_token, token_a)
        stale = ce.ContainerHandle(
            run_id="a", run_workspace=str(self.layout.shared_workspace("a")),
            host_workspace=str(self.layout.shared_worker_mount()),
            container="unused", container_scope="shared",
            bootstrap_root=root, owner_token=token_a)
        with self.assertRaisesRegex(RuntimeError, "older ownership generation"):
            ce._ensure_alive(stale)


if __name__ == "__main__":
    unittest.main()
