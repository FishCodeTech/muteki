"""C07: full-text message search (body-only, CJK, archived, superseded)."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.conversation.manager import ConversationManager
from muteki.conversation.models import (
    TURN_COMPLETED,
    TURN_SUPERSEDED,
    ConversationMessage,
    ThreadState,
    TurnRecord,
)
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.objects import Project, Thread, Workspace
from muteki.platform.store import PlatformStore


def _mgr(tmp: Path) -> tuple[ConversationManager, PlatformStore, ConversationStore]:
    store = PlatformStore(db_path=tmp / "platform.db")
    conv = ConversationStore(store)
    bindings = CapabilityBindingService(store)
    projection = ConversationProjection(store, conv)
    manager = ConversationManager(
        store,
        conv,
        bindings,
        projection,
        workspace_root=tmp / "workspaces",
    )
    return manager, store, conv


def _project_ws(store: PlatformStore, tmp: Path) -> tuple[Project, Workspace]:
    project = Project(project_id=new_id("proj"), name="C07 Search")
    store.save(project)
    root = tmp / "ws"
    root.mkdir(parents=True, exist_ok=True)
    workspace = Workspace(
        workspace_id=new_id("ws"),
        project_id=project.project_id,
        kind="local",
        root_path=str(root),
    )
    store.save(workspace)
    return project, workspace


def _thread(
    store: PlatformStore,
    *,
    project: Project,
    workspace: Workspace,
    title: str,
    summary: str = "",
) -> Thread:
    thread = Thread(
        thread_id=new_id("thr"),
        project_id=project.project_id,
        workspace_id=workspace.workspace_id,
        title=title,
        summary=summary,
        mode="conversation",
    )
    store.save(thread)
    return thread


def _turn(conv: ConversationStore, thread_id: str, seq: int, status: str) -> TurnRecord:
    turn = TurnRecord(
        turn_id=new_id("turn"),
        thread_id=thread_id,
        seq=seq,
        status=status,
        kind="message",
        text="",
    )
    conv.save_turn(turn)
    return turn


def _msg(
    conv: ConversationStore,
    *,
    thread_id: str,
    turn_id: str | None,
    role: str,
    text: str,
    stream_seq: int,
) -> ConversationMessage:
    message = ConversationMessage(
        message_id=new_id("msg"),
        thread_id=thread_id,
        turn_id=turn_id,
        role=role,
        text=text,
        stream_seq=stream_seq,
    )
    conv.save_message(message)
    return message


class MessageSearchTest(unittest.TestCase):
    def test_body_only_cjk_archived_superseded(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_s:
            tmp = Path(tmp_s)
            manager, store, conv = _mgr(tmp)
            project, workspace = _project_ws(store, tmp)

            body = _thread(
                store, project=project, workspace=workspace,
                title="AlphaSession", summary="no-keyword",
            )
            turn_b = _turn(conv, body.thread_id, 1, TURN_COMPLETED)
            body_msg = _msg(
                conv, thread_id=body.thread_id, turn_id=turn_b.turn_id,
                role="user", text="prefix ZEBRA_BODY_ONLY_9971 suffix",
                stream_seq=1,
            )
            conv.save_state(ThreadState(thread_id=body.thread_id, status="active"))

            zh = _thread(
                store, project=project, workspace=workspace,
                title="中文标题无关", summary="无关摘要",
            )
            turn_z = _turn(conv, zh.thread_id, 1, TURN_COMPLETED)
            _msg(
                conv, thread_id=zh.thread_id, turn_id=turn_z.turn_id,
                role="assistant", text="上下文 量子纠缠验收短语 结尾",
                stream_seq=1,
            )
            conv.save_state(ThreadState(thread_id=zh.thread_id, status="active"))

            archived = _thread(
                store, project=project, workspace=workspace,
                title="OldBox", summary="old",
            )
            turn_a = _turn(conv, archived.thread_id, 1, TURN_COMPLETED)
            _msg(
                conv, thread_id=archived.thread_id, turn_id=turn_a.turn_id,
                role="user", text="ARCHIVE_HIT_4422",
                stream_seq=1,
            )
            conv.save_state(ThreadState(thread_id=archived.thread_id, status="archived"))

            retry = _thread(
                store, project=project, workspace=workspace,
                title="RetryLab", summary="retry",
            )
            old = _turn(conv, retry.thread_id, 1, TURN_SUPERSEDED)
            new = _turn(conv, retry.thread_id, 2, TURN_COMPLETED)
            _msg(
                conv, thread_id=retry.thread_id, turn_id=old.turn_id,
                role="user", text="SUPERSEDED_HIT_3310",
                stream_seq=1,
            )
            _msg(
                conv, thread_id=retry.thread_id, turn_id=new.turn_id,
                role="user", text="CURRENT_HIT_3310",
                stream_seq=2,
            )
            conv.save_state(ThreadState(thread_id=retry.thread_id, status="active"))

            hits = manager.search_messages("ZEBRA_BODY_ONLY_9971")
            self.assertEqual(hits["count"], 1)
            self.assertEqual(hits["hits"][0]["message_id"], body_msg.message_id)
            self.assertIn("ZEBRA_BODY_ONLY_9971", hits["hits"][0]["snippet"])
            self.assertNotIn("ZEBRA", body.title)
            self.assertNotIn("ZEBRA", body.summary or "")

            zh_hits = manager.search_messages("量子纠缠")
            self.assertEqual(zh_hits["count"], 1)
            self.assertEqual(zh_hits["hits"][0]["thread_id"], zh.thread_id)

            self.assertEqual(manager.search_messages("ARCHIVE_HIT_4422")["count"], 0)
            arch_hits = manager.search_messages(
                "ARCHIVE_HIT_4422", include_archived=True,
            )
            self.assertEqual(arch_hits["count"], 1)
            self.assertTrue(arch_hits["hits"][0]["archived"])

            self.assertEqual(manager.search_messages("SUPERSEDED_HIT_3310")["count"], 0)
            self.assertEqual(manager.search_messages("CURRENT_HIT_3310")["count"], 1)
            super_hits = manager.search_messages(
                "SUPERSEDED_HIT_3310", include_superseded=True,
            )
            self.assertEqual(super_hits["count"], 1)
            self.assertTrue(super_hits["hits"][0]["superseded"])
            superseded_msg_id = super_hits["hits"][0]["message_id"]
            superseded_thread_id = super_hits["hits"][0]["thread_id"]
            superseded_page = manager.messages_page(
                superseded_thread_id, around_message_id=superseded_msg_id,
            )
            superseded_ids = {
                row["message_id"] for row in superseded_page["messages"]
            }
            self.assertIn(
                superseded_msg_id,
                superseded_ids,
                "around window must include superseded search hits",
            )

            page = manager.messages_page(
                body.thread_id, around_message_id=body_msg.message_id,
            )
            ids = {row["message_id"] for row in page["messages"]}
            self.assertIn(body_msg.message_id, ids)
            self.assertEqual(
                page["messages_page"].get("around_message_id"),
                body_msg.message_id,
            )


if __name__ == "__main__":
    unittest.main()
