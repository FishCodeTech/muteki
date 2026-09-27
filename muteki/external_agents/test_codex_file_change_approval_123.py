"""#123: Codex fileChange approval joins item/started path + Diff."""

from __future__ import annotations

import unittest
from typing import Any

from muteki.external_agents.codex import (
    _cache_file_change_item,
    _file_change_preview_from_ctx,
)


class CodexFileChangeApprovalPreviewTest(unittest.TestCase):
    def test_joins_item_started_changes_into_approval_preview(self) -> None:
        ctx: dict[str, Any] = {}
        _cache_file_change_item(ctx, {
            "id": "call_sample",
            "type": "fileChange",
            "status": "inProgress",
            "changes": [{
                "path": "sample.py",
                "kind": "update",
                "diff": "@@\n-return a - b\n+return a + b\n",
            }],
        })
        preview = _file_change_preview_from_ctx(
            {
                "itemId": "call_sample",
                "threadId": "thr-1",
                "turnId": "1",
                "grantRoot": "/workspace/demo",
                "reason": None,
            },
            ctx,
        )
        self.assertEqual(preview["path"], "sample.py")
        self.assertEqual(preview["paths"], ["sample.py"])
        self.assertEqual(preview["cwd"], "/workspace/demo")
        self.assertIn("sample.py", preview["diff"])
        self.assertIn("+return a + b", preview["diff"])
        self.assertEqual(preview["files"][0]["path"], "sample.py")

    def test_multi_file_keeps_paths_and_composed_diff(self) -> None:
        ctx: dict[str, Any] = {}
        _cache_file_change_item(ctx, {
            "id": "call_multi",
            "type": "fileChange",
            "changes": [
                {"path": "a.py", "kind": "add", "diff": "+a\n"},
                {"path": "b.py", "kind": "delete", "diff": "-b\n"},
            ],
        })
        preview = _file_change_preview_from_ctx({"itemId": "call_multi"}, ctx)
        self.assertNotIn("path", preview)  # multi-file: no single path
        self.assertEqual(preview["paths"], ["a.py", "b.py"])
        self.assertIn("a.py", preview["diff"])
        self.assertIn("b.py", preview["diff"])

    def test_v1_file_changes_map_on_params(self) -> None:
        preview = _file_change_preview_from_ctx(
            {
                "call_id": "legacy",
                "file_changes": {
                    "demo.txt": {
                        "type": "update",
                        "unified_diff": "@@\n-old\n+new\n",
                    }
                },
            },
            {},
        )
        self.assertEqual(preview["path"], "demo.txt")
        self.assertIn("+new", preview["diff"])


if __name__ == "__main__":
    unittest.main()
