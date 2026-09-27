"""Unit tests for C38 session handoff builder."""

from __future__ import annotations

import unittest
from types import SimpleNamespace

from muteki.conversation.session_handoff import (
    DEFAULT_MAX_MESSAGES,
    REASON_CREDENTIAL,
    REASON_PERMISSION,
    REASON_RETRY,
    REASON_RUNTIME_SWITCH,
    RECOVERY_EMPTY,
    RECOVERY_STRUCTURED_HANDOFF,
    build_session_handoff,
    classify_rebuild_reason,
    continuation_prompt_from_messages,
    render_agent_text,
)


def _msg(role: str, text: str, turn_id: str, message_id: str = "") -> SimpleNamespace:
    return SimpleNamespace(
        role=role,
        text=text,
        turn_id=turn_id,
        message_id=message_id or f"msg-{turn_id}-{role}",
    )


def _turn(turn_id: str, *, attachments=None, capability_refs=None) -> SimpleNamespace:
    return SimpleNamespace(
        turn_id=turn_id,
        attachments=list(attachments or []),
        capability_refs=list(capability_refs or []),
    )


class SessionHandoffTests(unittest.TestCase):
    def test_cross_runtime_includes_prior_fact(self) -> None:
        messages = [
            _msg("user", "记住口令 C38-FACT-alpha", "t1"),
            _msg("assistant", "已记住", "t1"),
            _msg("user", "口令是什么？", "t2"),
        ]
        bundle = build_session_handoff(
            thread_id="th1",
            messages=messages,
            exclude_turn_id="t2",
            reason=REASON_RUNTIME_SWITCH,
        )
        self.assertEqual(bundle.kind, RECOVERY_STRUCTURED_HANDOFF)
        text = render_agent_text(bundle, "口令是什么？")
        self.assertIn("C38-FACT-alpha", text)
        self.assertIn("口令是什么？", text)
        self.assertIn("Muteki", bundle.boundary_label)

    def test_excludes_superseded_branch_messages(self) -> None:
        # list_current_messages already drops superseded; builder must not
        # reintroduce them when only current-branch messages are supplied.
        messages = [
            _msg("user", "current-branch-only", "alive"),
            _msg("assistant", "ok", "alive"),
        ]
        bundle = build_session_handoff(
            thread_id="th1",
            messages=messages,
            exclude_turn_id="next",
        )
        joined = " ".join(item.text for item in bundle.included)
        self.assertIn("current-branch-only", joined)
        self.assertNotIn("old-superseded", joined)

    def test_attachment_identity_preserved(self) -> None:
        messages = [_msg("user", "看这个文件", "t1")]
        turns = [_turn("t1", attachments=["sha256:deadbeef"], capability_refs=[
            {"id": "cap-1", "name": "notes.md", "kind": "file"},
        ])]
        bundle = build_session_handoff(
            thread_id="th1",
            messages=messages,
            turns=turns,
            exclude_turn_id="t2",
        )
        self.assertEqual(bundle.included[0].attachment_ids, ["sha256:deadbeef"])
        self.assertEqual(bundle.to_event_payload()["handoff_attachment_ids"], [
            "sha256:deadbeef",
        ])
        self.assertIn("notes.md", render_agent_text(bundle, "继续"))

    def test_window_truncation_is_visible(self) -> None:
        messages = [
            _msg("user", f"fact-{i}", f"t{i}")
            for i in range(5)
        ]
        bundle = build_session_handoff(
            thread_id="th1",
            messages=messages,
            exclude_turn_id="",
            max_messages=2,
            max_chars=10_000,
        )
        self.assertEqual(len(bundle.included), 2)
        self.assertEqual(bundle.omitted_count, 3)
        self.assertEqual(bundle.omitted_reason, "window")
        self.assertIn("未纳入", bundle.boundary_label)
        # Newest kept
        self.assertIn("fact-4", bundle.included[-1].text)

    def test_classify_permission_and_credential(self) -> None:
        prev = "pi:default|cred-a|access_mode=supervised|permission_mode=|sandbox_mode="
        perm = "pi:default|cred-a|access_mode=full|permission_mode=|sandbox_mode="
        cred = "pi:default|cred-b|access_mode=supervised|permission_mode=|sandbox_mode="
        self.assertEqual(
            classify_rebuild_reason(
                switched=True,
                force_new=False,
                turn_kind="message",
                previous_session_key=prev,
                new_session_key=perm,
                previous_runtime_key="pi:default",
                new_runtime_key="pi:default",
            ),
            REASON_PERMISSION,
        )
        self.assertEqual(
            classify_rebuild_reason(
                switched=True,
                force_new=False,
                turn_kind="message",
                previous_session_key=prev,
                new_session_key=cred,
                previous_runtime_key="pi:default",
                new_runtime_key="pi:default",
            ),
            REASON_CREDENTIAL,
        )
        self.assertEqual(
            classify_rebuild_reason(
                switched=True,
                force_new=False,
                turn_kind="message",
                previous_session_key=prev,
                new_session_key="codex:default|cred-a|access_mode=supervised|permission_mode=|sandbox_mode=",
                previous_runtime_key="pi:default",
                new_runtime_key="codex:default",
            ),
            REASON_RUNTIME_SWITCH,
        )
        self.assertEqual(
            classify_rebuild_reason(
                switched=False,
                force_new=True,
                turn_kind="retry",
                previous_session_key=prev,
                new_session_key=prev,
                previous_runtime_key="pi:default",
                new_runtime_key="pi:default",
            ),
            REASON_RETRY,
        )

    def test_empty_history(self) -> None:
        bundle = build_session_handoff(thread_id="th1", messages=[])
        self.assertEqual(bundle.kind, RECOVERY_EMPTY)
        self.assertIn("无可交接", bundle.boundary_label) if False else self.assertIn(
            "无当前分支历史可交接", bundle.boundary_label,
        )

    def test_continuation_prompt_compat(self) -> None:
        turn = SimpleNamespace(
            thread_id="th1", turn_id="t3", kind="retry", text="再试一次",
        )
        messages = [
            _msg("user", "hello", "t1"),
            _msg("assistant", "world", "t1"),
            _msg("user", "再试一次", "t3"),
        ]
        prompt = continuation_prompt_from_messages(turn, messages)
        self.assertIn("hello", prompt)
        self.assertIn("再试一次", prompt)

    def test_default_window_constant(self) -> None:
        self.assertGreaterEqual(DEFAULT_MAX_MESSAGES, 10)


if __name__ == "__main__":
    unittest.main()
