"""C11 — impact preview, edit-resend, rewind refuse / fail-atomic tests."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.conversation.impact import build_impact_preview, resolve_rewind_capability
from muteki.conversation.manager import ConversationError, ConversationManager
from muteki.conversation.models import (
    TURN_COMPLETED,
    TURN_KIND_EDIT_RESEND,
    TURN_KIND_RETRY,
    TURN_SUPERSEDED,
    ConversationMessage,
    TurnRecord,
)
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.store import PlatformStore


def _mgr(tmp: str) -> ConversationManager:
    store = PlatformStore(Path(tmp) / "platform.db")
    conv = ConversationStore(store)
    projection = ConversationProjection(store, conv, sessions_root=Path(tmp))
    bindings = CapabilityBindingService(store)
    return ConversationManager(
        store, conv, bindings, projection, workspace_root=Path(tmp) / "ws",
    )


def _seed_three_turns(manager: ConversationManager, root: Path) -> tuple[str, list[TurnRecord]]:
    project = manager.create_project("c11")
    workspace = manager.plan_workspace(
        project_id=project.project_id,
        kind="local",
        root_path=str(root),
    )
    manager.save_workspace(workspace)
    thread = manager.create_thread(
        project_id=project.project_id,
        workspace_id=workspace.workspace_id,
        title="c11",
    )
    turns: list[TurnRecord] = []
    for index in range(1, 4):
        turn, _run, _task, _created = manager.request_turn(
            thread.thread_id,
            text=f"prompt-{index}",
            kind="message",
            idempotency_key=f"seed-{index}",
        )
        manager.conv.save_turn(turn.model_copy(update={
            "status": TURN_COMPLETED,
        }))
        manager.conv.save_message(
            ConversationMessage(
                thread_id=thread.thread_id,
                turn_id=turn.turn_id,
                role="user",
                text=f"prompt-{index}",
                stream_seq=index * 2 - 1,
            )
        )
        manager.conv.save_message(
            ConversationMessage(
                thread_id=thread.thread_id,
                turn_id=turn.turn_id,
                role="assistant",
                text=f"answer-{index}",
                stream_seq=index * 2,
            )
        )
        turns.append(manager.conv.get_turn(turn.turn_id) or turn)
    return thread.thread_id, turns


class ImpactPreviewUnitTest(unittest.TestCase):
    def test_retry_lists_exact_superseded_ids(self) -> None:
        t1 = TurnRecord(turn_id="t1", seq=1, text="a", status=TURN_COMPLETED)
        t2 = TurnRecord(turn_id="t2", seq=2, text="b", status=TURN_COMPLETED)
        t3 = TurnRecord(turn_id="t3", seq=3, text="c", status=TURN_COMPLETED)
        preview = build_impact_preview(
            mode="retry",
            target=t2,
            current_turns=[t1, t2, t3],
            workspace_root="",
        )
        self.assertEqual(preview["superseded_turn_ids"], ["t2", "t3"])
        self.assertEqual(preview["workspace"]["policy"], "keep_files")
        self.assertEqual(preview["external_side_effects"], "cannot_undo")

    def test_rewind_capability_unknown_without_matrix(self) -> None:
        cap = resolve_rewind_capability({})
        self.assertEqual(cap["rewind_level"], "unknown")
        self.assertFalse(cap["invocable"])


class C11ManagerTest(unittest.TestCase):
    def test_retry_keep_files_and_preview(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            root.mkdir()
            touched = root / "c11-touched.txt"
            touched.write_text("keep-me\n", encoding="utf-8")
            manager = _mgr(tmp)
            thread_id, turns = _seed_three_turns(manager, root)
            preview = manager.preview_turn_impact(
                thread_id, turns[1].turn_id, mode="retry",
            )
            self.assertEqual(
                set(preview["superseded_turn_ids"]),
                {turns[1].turn_id, turns[2].turn_id},
            )
            self.assertEqual(preview["workspace"]["policy"], "keep_files")

            turn, _run, _task, superseded, created = manager.retry_turn(
                thread_id, turns[1].turn_id, idempotency_key="retry-1",
            )
            self.assertTrue(created)
            self.assertEqual(turn.kind, TURN_KIND_RETRY)
            self.assertEqual(set(superseded), {turns[1].turn_id, turns[2].turn_id})
            self.assertEqual(touched.read_text(encoding="utf-8"), "keep-me\n")
            current = manager.conv.list_current_turns(thread_id)
            self.assertTrue(all(item.status != TURN_SUPERSEDED for item in current))
            self.assertEqual(
                {item.status for item in manager.conv.list_turns(thread_id)
                 if item.turn_id in superseded},
                {TURN_SUPERSEDED},
            )

    def test_edit_resend_creates_edited_turn(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            root.mkdir()
            manager = _mgr(tmp)
            thread_id, turns = _seed_three_turns(manager, root)
            turn, _run, _task, superseded, created = manager.retry_turn(
                thread_id,
                turns[1].turn_id,
                text="edited-prompt",
                idempotency_key="edit-1",
            )
            self.assertTrue(created)
            self.assertEqual(turn.kind, TURN_KIND_EDIT_RESEND)
            self.assertEqual(turn.text, "edited-prompt")
            self.assertEqual(set(superseded), {turns[1].turn_id, turns[2].turn_id})
            view = manager.thread_view(thread_id, include_superseded=True)
            self.assertTrue(view["superseded_turns"])
            self.assertTrue(view["superseded_messages"])

    def test_rewind_unsupported_no_state_change(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            root.mkdir()
            dirty = root / "dirty.txt"
            dirty.write_text("x\n", encoding="utf-8")
            manager = _mgr(tmp)
            thread_id, turns = _seed_three_turns(manager, root)
            before = [item.turn_id for item in manager.conv.list_current_turns(thread_id)]
            with self.assertRaises(ConversationError):
                manager.native_rewind_turn(thread_id, turns[1].turn_id)
            after = [item.turn_id for item in manager.conv.list_current_turns(thread_id)]
            self.assertEqual(before, after)
            self.assertEqual(dirty.read_text(encoding="utf-8"), "x\n")

    def test_rewind_provider_failure_atomic(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            root.mkdir()
            dirty = root / "dirty.txt"
            dirty.write_text("x\n", encoding="utf-8")
            manager = _mgr(tmp)
            thread_id, turns = _seed_three_turns(manager, root)
            before = [item.status for item in manager.conv.list_turns(thread_id)]

            def boom(**_kwargs: object) -> bool:
                raise RuntimeError("provider rewind exploded")

            with self.assertRaises(ConversationError) as ctx:
                manager.native_rewind_turn(
                    thread_id,
                    turns[1].turn_id,
                    capability_override={
                        "invocable": True,
                        "reason": "",
                    },
                    provider_rewind=boom,
                )
            self.assertIn("未改动", str(ctx.exception))
            after = [item.status for item in manager.conv.list_turns(thread_id)]
            self.assertEqual(before, after)
            self.assertEqual(dirty.read_text(encoding="utf-8"), "x\n")

    def test_rewind_success_supersedes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            root.mkdir()
            manager = _mgr(tmp)
            thread_id, turns = _seed_three_turns(manager, root)

            def ok(**_kwargs: object) -> bool:
                return True

            superseded, applied = manager.native_rewind_turn(
                thread_id,
                turns[1].turn_id,
                capability_override={"invocable": True},
                provider_rewind=ok,
                idempotency_key="rewind-ok",
            )
            self.assertTrue(applied)
            self.assertEqual(set(superseded), {turns[1].turn_id, turns[2].turn_id})
            current = manager.conv.list_current_turns(thread_id)
            self.assertEqual([item.turn_id for item in current], [turns[0].turn_id])

    def test_fork_leaves_source_intact(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            root.mkdir()
            manager = _mgr(tmp)
            thread_id, turns = _seed_three_turns(manager, root)
            forked, _task = manager.fork_thread(
                thread_id, from_turn_id=turns[1].turn_id, title="forked",
            )
            source = manager.conv.list_current_turns(thread_id)
            self.assertEqual(len(source), 3)
            self.assertEqual(forked.workspace_id, manager.get_thread(thread_id).workspace_id)
            msgs = manager.conv.list_current_messages(forked.thread_id)
            self.assertGreaterEqual(len(msgs), 2)


if __name__ == "__main__":
    unittest.main()
