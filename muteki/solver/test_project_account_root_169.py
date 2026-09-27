"""#169: project_account_root only copies selected accounts; static files are 0444."""
from __future__ import annotations

import os
import stat
import subprocess
from unittest.mock import patch
import tempfile
import unittest
from pathlib import Path

# Import via package path (avoid running from solver/ which shadows stdlib types).
from muteki.solver.credential_accounts import project_account_root


class ProjectAccountRootIsolationTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.src = self.root / "accounts"
        self.dst = self.root / "projection"
        for account_id, secret in (("alice", "a-secret"), ("bob", "b-secret")):
            d = self.src / account_id
            d.mkdir(parents=True)
            (d / "api_key").write_text(secret, encoding="utf-8")
            os.chmod(d / "api_key", 0o600)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_empty_account_ids_projects_nothing(self) -> None:
        project_account_root(self.src, self.dst, account_ids=())
        self.assertTrue(self.dst.is_dir())
        self.assertEqual(list(self.dst.iterdir()), [])

    def test_selected_account_only(self) -> None:
        project_account_root(self.src, self.dst, account_ids=["alice"])
        names = sorted(p.name for p in self.dst.iterdir())
        self.assertEqual(names, ["alice"])
        key = self.dst / "alice" / "api_key"
        self.assertEqual(key.read_text(encoding="utf-8"), "a-secret")
        mode = stat.S_IMODE(key.stat().st_mode)
        self.assertEqual(mode, 0o444)

    def test_omitted_selection_does_not_expose_accounts(self) -> None:
        project_account_root(self.src, self.dst)
        self.assertEqual(list(self.dst.iterdir()), [])

    def test_reprojection_preserves_live_mount_and_refresh(self) -> None:
        project_account_root(self.src, self.dst, account_ids=["alice", "bob"])
        inode = self.dst.stat().st_ino
        state = self.dst / "alice" / "codex-home"
        state_inode = state.stat().st_ino
        (state / "auth.json").write_text("refreshed")
        project_account_root(self.src, self.dst, account_ids=["alice"])
        self.assertEqual(self.dst.stat().st_ino, inode)
        self.assertEqual(state.stat().st_ino, state_inode)
        self.assertEqual((state / "auth.json").read_text(), "refreshed")
        self.assertFalse((self.dst / "bob").exists())

    def test_source_symlink_is_not_followed(self) -> None:
        (self.src / "alice" / "external").symlink_to(self.src / "bob")
        with self.assertRaises(ValueError):
            project_account_root(self.src, self.dst, account_ids=["alice"])

    def test_container_has_readonly_secrets_and_no_new_privileges(self) -> None:
        from muteki.solver import container_exec as ce
        calls = []
        def docker(*args, **kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 0, "", "")
        with patch.object(ce, "_docker", side_effect=docker), \
             patch.object(ce, "_container_state", side_effect=[None, "running"]), \
             patch.object(ce, "_chown_tree_to_worker"), \
             patch.object(ce, "_USE_DOCKEREXEC", True):
            ce.ensure_container("test-run", str(self.root / "workspace"),
                                account_root=str(self.src), account_ids=["alice"],
                                account_projection_root=str(self.root / "projections"))
        args = next(args for args in calls if args[0] == "run")
        self.assertIn("no-new-privileges=true", args)
        mounts = [args[i+1] for i, arg in enumerate(args[:-1]) if arg == "--mount"]
        root_mount = next(m for m in mounts if f"target={ce.CONTAINER_ACCOUNTS_ROOT}," in m)
        self.assertTrue(root_mount.endswith(",readonly"))
        self.assertTrue(any(f"target={ce.CONTAINER_ACCOUNTS_ROOT}/alice/codex-home" in m
                            and not m.endswith(",readonly") for m in mounts))
        self.assertFalse(any("/bob/" in m for m in mounts))

    def test_exited_legacy_container_can_be_recreated(self) -> None:
        from muteki.solver import container_exec as ce
        with patch.object(ce, "_docker", return_value=subprocess.CompletedProcess([], 0, "", "")), \
             patch.object(ce, "_container_state", side_effect=["exited", "running"]), \
             patch.object(ce, "_container_absence_proven", return_value=True), \
             patch.object(ce, "_container_bootstrap_source", return_value=None), \
             patch.object(ce, "_chown_tree_to_worker"), \
             patch.object(ce, "_USE_DOCKEREXEC", True):
            ce.ensure_container("legacy-test", str(self.root / "workspace"))

    def test_live_policy_change_rejected_before_projection(self) -> None:
        from muteki.solver import container_exec as ce
        with patch.object(ce, "_docker", return_value=subprocess.CompletedProcess([], 0, "old-policy", "")), \
             patch.object(ce, "_container_state", return_value="running"), \
             patch.object(ce, "_chown_tree_to_worker"), \
             patch.object(ce, "_USE_DOCKEREXEC", True):
            with self.assertRaisesRegex(RuntimeError, "policy changed"):
                ce.ensure_container("test-run", str(self.root / "workspace"),
                                    account_root=str(self.src), account_ids=["alice"],
                                    account_projection_root=str(self.root / "projections"))
        self.assertFalse((self.root / "projections").exists())

    def test_custom_projection_base_cleanup_uses_run_identity(self) -> None:
        from muteki.solver import container_exec as ce
        owner_id = "custom-projection-run"
        projection = self.root / "projections" / ce._run_identity(owner_id)
        projection.mkdir(parents=True)
        (projection / "secret").write_text("synthetic")
        unrelated = self.root / "projections" / "unrelated"
        unrelated.mkdir()
        ce._cleanup_account_projection(str(unrelated), owner_id)
        self.assertTrue(unrelated.exists())
        ce._cleanup_account_projection(str(projection), owner_id)
        self.assertFalse(projection.exists())



if __name__ == "__main__":
    unittest.main()
