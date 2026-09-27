"""C12: Git worktree lifecycle + occupancy helpers."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

from muteki.conversation.git_workspace import (
    GitWorkspaceError,
    add_worktree,
    inspect_git_workspace,
    list_worktrees,
    remove_worktree,
)
from muteki.conversation.manager import ConversationManager, WS_MODE_NEW
from muteki.platform.store import PlatformStore


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(["git", "-C", str(cwd), *args], check=True, capture_output=True, text=True)


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init")
    _git(path, "config", "user.email", "c12@example.com")
    _git(path, "config", "user.name", "C12")
    (path / "README.md").write_text("main\n", encoding="utf-8")
    _git(path, "add", "README.md")
    _git(path, "commit", "-m", "init")
    # Ensure branch is named main
    _git(path, "branch", "-M", "main")


class GitWorktreeHelpersTest(unittest.TestCase):
    def test_two_worktrees_isolated_and_dirty_remove_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            _init_repo(root)
            a = Path(tmp) / "wt-a"
            b = Path(tmp) / "wt-b"
            add_worktree(str(root), str(a), branch="c12-a", base_ref="main")
            add_worktree(str(root), str(b), branch="c12-b", base_ref="main")

            (a / "a-only.txt").write_text("A\n", encoding="utf-8")
            self.assertFalse((b / "a-only.txt").exists())
            status_a = inspect_git_workspace(str(a))
            status_b = inspect_git_workspace(str(b))
            self.assertEqual(status_a["current_branch"], "c12-a")
            self.assertEqual(status_b["current_branch"], "c12-b")
            self.assertTrue(status_a["dirty"])

            rows = list_worktrees(str(root))
            paths = {row["path"] for row in rows}
            self.assertIn(str(a.resolve()), paths)
            self.assertIn(str(b.resolve()), paths)

            with self.assertRaises(GitWorkspaceError) as ctx:
                remove_worktree(str(a))
            self.assertEqual(ctx.exception.code, "conversation.git.worktree_dirty")

            # clean then remove
            (a / "a-only.txt").unlink()
            remove_worktree(str(a))
            self.assertFalse(a.exists())

    def test_failed_add_leaves_no_orphan(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            _init_repo(root)
            target = Path(tmp) / "bad-wt"
            with self.assertRaises(GitWorkspaceError):
                add_worktree(
                    str(root),
                    str(target),
                    branch="c12-bad",
                    base_ref="does-not-exist-ref",
                )
            self.assertFalse(target.exists())


class WorkspaceBindNewWorktreeTest(unittest.TestCase):
    def test_bind_new_worktree_and_archive_keeps_tree(self) -> None:
        from muteki.conversation.projections import ConversationProjection
        from muteki.conversation.store import ConversationStore
        from muteki.platform.capability_bindings import CapabilityBindingService

        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            _init_repo(repo)
            db = Path(tmp) / "platform.db"
            store = PlatformStore(db_path=db)
            conv = ConversationStore(store)
            bindings = CapabilityBindingService(store)
            projection = ConversationProjection(store, conv)
            manager = ConversationManager(
                store,
                conv,
                bindings,
                projection,
                workspace_root=Path(tmp) / "workspaces",
            )
            project = manager.create_project(name="c12", description="")
            primary = manager.bind_workspace(
                project_id=project.project_id,
                kind="local",
                root_path=str(repo),
            )
            planned = manager.plan_workspace(
                project_id=project.project_id,
                mode=WS_MODE_NEW,
                branch="c12-a",
                base_ref="main",
                parent_root=primary.root_path,
            )
            saved = manager.save_workspace(planned)
            self.assertEqual(saved.settings.get("mode"), WS_MODE_NEW)
            self.assertTrue(Path(saved.root_path).is_dir())
            self.assertEqual(
                inspect_git_workspace(saved.root_path)["current_branch"],
                "c12-a",
            )

            thread = manager.create_thread(
                project_id=project.project_id,
                workspace_id=saved.workspace_id,
                title="A",
            )
            occupants = manager.active_threads_for_workspace_root(saved.root_path)
            self.assertEqual([t.thread_id for t in occupants], [thread.thread_id])

            (Path(saved.root_path) / "dirty.txt").write_text("x\n", encoding="utf-8")
            manager.archive_thread(thread.thread_id)
            self.assertTrue(Path(saved.root_path).is_dir())
            with self.assertRaises(Exception):
                manager.delete_worktree_workspace(saved.workspace_id)


if __name__ == "__main__":
    unittest.main()
