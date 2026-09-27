"""C23 approval queue: isolation, binding, expiry, preview normalization."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.conversation import events as ev
from muteki.conversation.approval_queue import (
    has_actionable_approvals,
    expire_approvals,
    hydrate_approvals,
    lookup_approval,
    normalize_approval_payload,
    remove_approval,
    upsert_approval,
)
from muteki.conversation.models import ThreadState
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.platform.store import PlatformStore


class ApprovalQueueHelpersTest(unittest.TestCase):
    def test_normalize_command_and_diff(self) -> None:
        cmd = normalize_approval_payload({
            "approval_id": "a-cmd",
            "approval_kind": "command_execution",
            "command": ["npm", "test"],
            "cwd": "/tmp/ws",
        })
        self.assertEqual(cmd["command"], "npm test")
        self.assertEqual(cmd["cwd"], "/tmp/ws")
        self.assertIn(cmd["approval_kind"], {"command_execution", "command"})

        diff = normalize_approval_payload({
            "approval_id": "a-diff",
            "kind": "file_change",
            "native": {"diff": "--- a/x\n+++ b/x\n@@\n-a\n+b\n"},
        })
        self.assertEqual(diff["approval_kind"], "file_change")
        self.assertIn("+++ b/x", diff["diff"])


    def test_normalize_file_change_path_and_diff(self) -> None:
        row = normalize_approval_payload({
            "approval_id": "a-path",
            "approval_kind": "file_change",
            "files": [{
                "path": "sample.py",
                "kind": "update",
                "diff": "@@\n-return a - b\n+return a + b\n",
            }],
        })
        self.assertEqual(row["path"], "sample.py")
        self.assertEqual(row["paths"], ["sample.py"])
        self.assertIn("sample.py", row["diff"])
        self.assertIn("+return a + b", row["diff"])

    def test_normalize_file_change_missing_preview_reason(self) -> None:
        row = normalize_approval_payload({
            "approval_id": "a-empty",
            "approval_kind": "file_change",
        })
        self.assertIn("暂未取得", row.get("reason") or "")

    def test_normalize_v1_file_changes_map(self) -> None:
        row = normalize_approval_payload({
            "approval_id": "a-v1",
            "kind": "apply_patch",
            "file_changes": {
                "new.txt": {"type": "add", "content": "hello\n"},
                "old.txt": {"type": "delete", "content": "bye\n"},
            },
        })
        self.assertEqual(row["approval_kind"], "file_change")
        paths = {item["path"] for item in row["files"]}
        self.assertEqual(paths, {"new.txt", "old.txt"})
        self.assertTrue(row["diff"])

    def test_queue_isolation_and_stale_lookup(self) -> None:
        q, _ = upsert_approval({}, {
            "approval_id": "a1",
            "command": "echo one",
            "cwd": "/tmp",
        })
        q, primary = upsert_approval(q, {
            "approval_id": "a2",
            "command": "echo two",
            "cwd": "/tmp",
        })
        self.assertEqual(set(q), {"a1", "a2"})
        self.assertEqual(primary["approval_id"], "a1")
        q, primary = remove_approval(q, "a1")
        self.assertEqual(set(q), {"a2"})
        self.assertEqual(primary["approval_id"], "a2")
        self.assertIsNone(lookup_approval(q, "a1"))

    def test_expire_marks_pending(self) -> None:
        q, _ = upsert_approval({}, {"approval_id": "a1", "command": "x"})
        q, _ = upsert_approval(q, {"approval_id": "a2", "command": "y"})
        q, primary = expire_approvals(q, reason="回合结束")
        self.assertTrue(all(row["status"] == "expired" for row in q.values()))
        self.assertIsNone(primary)  # expired must not stay actionable


class ApprovalQueueProjectionTest(unittest.TestCase):
    def test_projection_keeps_siblings(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / "platform.db"
            store = PlatformStore(db_path=db)
            conv = ConversationStore(store)
            projection = ConversationProjection(store, conv)
            thread_id = "thr-c23"
            conv.save_state(ThreadState(thread_id=thread_id))

            e1 = ev.thread_event(thread_id, ev.EV_APPROVAL_REQUESTED, {
                "approval_id": "appr-a",
                "command": "ls -la",
                "cwd": "/workspace",
                "approval_kind": "command_execution",
            })
            e1 = e1.model_copy(update={"stream_seq": 1})
            projection.apply(e1)

            e2 = ev.thread_event(thread_id, ev.EV_APPROVAL_REQUESTED, {
                "approval_id": "appr-b",
                "diff": "--- a/f\n+++ b/f\n@@\n-old\n+new\n",
                "approval_kind": "file_change",
                "title": "edit f",
            })
            e2 = e2.model_copy(update={"stream_seq": 2})
            projection.apply(e2)

            state = conv.get_state(thread_id)
            self.assertEqual(set(state.pending_approvals), {"appr-a", "appr-b"})
            self.assertEqual(state.pending_approval["approval_id"], "appr-a")
            self.assertEqual(state.pending_approvals["appr-a"]["command"], "ls -la")
            self.assertIn("+++ b/f", state.pending_approvals["appr-b"]["diff"])

            e3 = ev.thread_event(thread_id, ev.EV_APPROVAL_RESOLVED, {
                "approval_id": "appr-a",
                "decision": "allow",
            })
            e3 = e3.model_copy(update={"stream_seq": 3})
            projection.apply(e3)
            state = conv.get_state(thread_id)
            self.assertEqual(set(state.pending_approvals), {"appr-b"})
            self.assertEqual(state.pending_approval["approval_id"], "appr-b")

            e4 = ev.thread_event(thread_id, ev.EV_APPROVAL_RESOLVED, {
                "approval_id": "appr-a",
                "decision": "allow",
            })
            e4 = e4.model_copy(update={"stream_seq": 4})
            projection.apply(e4)
            state = conv.get_state(thread_id)
            self.assertEqual(set(state.pending_approvals), {"appr-b"})

            e5 = ev.thread_event(thread_id, ev.EV_TURN_COMPLETED, {
                "turn_id": "turn-1",
            })
            e5 = e5.model_copy(update={"stream_seq": 5})
            projection.apply(e5)
            state = conv.get_state(thread_id)
            self.assertEqual(state.pending_approvals["appr-b"]["status"], "expired")


    def test_expired_not_actionable(self) -> None:
        q, primary = upsert_approval({}, {"approval_id": "a1", "command": "x"})
        self.assertTrue(has_actionable_approvals(q, primary))
        q, primary = expire_approvals(q, reason="回合结束")
        self.assertIsNone(primary)
        self.assertFalse(has_actionable_approvals(q, primary))
        self.assertFalse(has_actionable_approvals(q, {"approval_id": "a1", "status": "expired"}))


class ApprovalInjectGateTest(unittest.TestCase):
    def test_inject_requires_env_gate(self) -> None:
        import os
        from types import SimpleNamespace
        from muteki.conversation.commands import ConversationCommandHandler
        from muteki.platform.command_handlers.base import CommandFailed

        handler = ConversationCommandHandler(
            SimpleNamespace(), SimpleNamespace(),
        )
        command = SimpleNamespace(
            command_type="conversation.approval.inject",
            command_id="cmd-gate",
            idempotency_key="idem-gate",
            correlation_id="corr-gate",
            actor=SimpleNamespace(id="actor-1"),
            payload={"approvals": [{"approval_id": "x", "command": "echo"}]},
        )
        os.environ.pop("MUTEKI_ALLOW_APPROVAL_FIXTURES", None)
        with self.assertRaises(CommandFailed) as ctx:
            handler._plan_approval_inject(command)
        self.assertEqual(
            ctx.exception.error.code,
            "conversation.approval.fixture_disabled",
        )

if __name__ == "__main__":
    unittest.main()
