"""C13: workspace Diff sections, staging split, and path filters."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

from muteki.conversation.workspace_surfaces import (
    read_workspace_diff,
    split_unified_diff_files,
)


def _git(cwd: Path, *args: str) -> None:
    subprocess.run(["git", "-C", str(cwd), *args], check=True, capture_output=True, text=True)


def _init_repo(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init")
    _git(path, "config", "user.email", "c13@example.com")
    _git(path, "config", "user.name", "C13")
    (path / "app.py").write_text("print('hello')\n", encoding="utf-8")
    (path / "readme.md").write_text("# readme\n", encoding="utf-8")
    (path / "blob.bin").write_bytes(b"\x00\x01\x02\xff")
    _git(path, "add", "app.py", "readme.md", "blob.bin")
    _git(path, "commit", "-m", "init")
    _git(path, "branch", "-M", "main")


class WorkspaceDiffSectionsTest(unittest.TestCase):
    def test_staged_then_modify_produces_two_staging_rows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            _init_repo(root)
            (root / "app.py").write_text("print('staged')\n", encoding="utf-8")
            _git(root, "add", "app.py")
            (root / "app.py").write_text("print('unstaged')\n", encoding="utf-8")

            data = read_workspace_diff(str(root))
            self.assertTrue(data["is_repo"])
            self.assertEqual(data["baseline"], "worktree")
            self.assertTrue(data["head_sha"])
            self.assertTrue(data["captured_at"])
            self.assertTrue(data["sections"]["staged"]["patch"])
            self.assertTrue(data["sections"]["unstaged"]["patch"])
            self.assertIn("# staged", data["patch"])
            self.assertIn("# unstaged", data["patch"])

            rows = [item for item in data["files"] if item["path"] == "app.py"]
            stagings = {item["staging"] for item in rows}
            self.assertEqual(stagings, {"staged", "unstaged"})

    def test_path_filter_and_rename_delete_untracked_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            _init_repo(root)
            (root / "gone.txt").write_text("bye\n", encoding="utf-8")
            _git(root, "add", "gone.txt")
            _git(root, "commit", "-m", "add gone")
            # Keep rename staged (do not commit it with later deletes).
            _git(root, "mv", "readme.md", "readme2.md")
            _git(root, "rm", "gone.txt")
            (root / "new.py").write_text("x = 1\n", encoding="utf-8")
            (root / "blob.bin").write_bytes(b"\x00\x01\x02\xff\xee")

            data = read_workspace_diff(str(root))
            paths = {(item["path"], item["staging"], item["status"]) for item in data["files"]}
            self.assertTrue(any(path == "readme2.md" and status == "R" for path, _staging, status in paths))
            self.assertTrue(any(path == "gone.txt" and status == "D" for path, _staging, status in paths))
            self.assertTrue(any(path == "new.py" and staging == "untracked" for path, staging, _status in paths))
            self.assertTrue(any(path == "blob.bin" for path, _staging, _status in paths))

            filtered = read_workspace_diff(str(root), path="new.py", staging="untracked")
            self.assertEqual(filtered["selected_path"], "new.py")
            self.assertEqual(filtered["selected_staging"], "untracked")
            self.assertTrue(all(item["path"] == "new.py" for item in filtered["files"]))
            self.assertIn("new.py", filtered["patch"])
            self.assertNotIn("# staged", filtered["patch"])

    def test_split_unified_diff_files_parses_multi_file_artifact(self) -> None:
        patch = (
            "diff --git a/a.py b/a.py\n"
            "--- a/a.py\n"
            "+++ b/a.py\n"
            "@@ -1 +1 @@\n"
            "-old\n"
            "+new\n"
            "diff --git a/b.md b/b.md\n"
            "new file mode 100644\n"
            "--- /dev/null\n"
            "+++ b/b.md\n"
            "@@ -0,0 +1 @@\n"
            "+hello\n"
        )
        files = split_unified_diff_files(patch)
        self.assertEqual([item["path"] for item in files], ["a.py", "b.md"])
        self.assertEqual(files[0]["additions"], 1)
        self.assertEqual(files[0]["deletions"], 1)
        self.assertEqual(files[1]["status"], "A")


if __name__ == "__main__":
    unittest.main()
