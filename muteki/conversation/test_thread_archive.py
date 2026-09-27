"""C31: batch archive sequencing depends on idempotent archive + side-effect-free unarchive."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

from muteki.conversation import events as ev
from muteki.conversation.commands import ConversationCommandHandler
from muteki.conversation.manager import ConversationManager
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.store import PlatformStore


class ThreadArchiveUnarchiveTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        root = Path(self._tmp.name)
        self.store = PlatformStore(db_path=root / "platform.db")
        self.conv = ConversationStore(self.store)
        self.bindings = CapabilityBindingService(self.store)
        self.projection = ConversationProjection(self.store, self.conv)
        self.manager = ConversationManager(
            self.store,
            self.conv,
            self.bindings,
            self.projection,
            workspace_root=root / "workspaces",
        )
        self.executor = SimpleNamespace(close_thread=AsyncMock(return_value=True))
        self.handler = ConversationCommandHandler(self.manager, self.executor)
        project = self.manager.create_project(name="c31", description="")
        self.thread = self.manager.create_thread(project_id=project.project_id, title="C31-A")

    async def asyncTearDown(self) -> None:
        self._tmp.cleanup()

    def _command(self, command_type: str, *, command_id: str = "cmd-1") -> SimpleNamespace:
        return SimpleNamespace(
            command_type=command_type,
            command_id=command_id,
            aggregate_id=self.thread.thread_id,
            payload={},
            actor=SimpleNamespace(id="local-user"),
            idempotency_key=command_id,
        )

    def _apply(self, event_type: str, seq: int,
               payload: dict | None = None) -> None:
        event = ev.thread_event(
            self.thread.thread_id,
            event_type,
            {"thread_id": self.thread.thread_id, **(payload or {})},
            actor_id="local-user",
        ).model_copy(update={"stream_seq": seq})
        self.projection.apply(event)

    async def test_rename_does_not_create_unread_attention(self) -> None:
        state = self.conv.get_state(self.thread.thread_id)
        self.conv.save_state(state.model_copy(update={
            "head_stream_seq": 1,
            "read_stream_seq": 1,
        }))

        self._apply(ev.EV_THREAD_RENAME_REQUESTED, 2)
        self._apply(ev.EV_THREAD_RENAMED, 3)
        renamed = self.conv.get_state(self.thread.thread_id)
        self.assertEqual(renamed.head_stream_seq, 1)
        self.assertFalse(renamed.unread)

        self._apply(ev.EV_TURN_REQUESTED, 4, {
            "turn_id": "qa-rename-unread-turn",
            "text": "new message",
            "seq": 1,
        })
        self.assertTrue(self.conv.get_state(self.thread.thread_id).unread)

    async def test_archive_then_unarchive_restores_visibility_without_queue_resume(self) -> None:
        plan = self.handler._plan_thread_archive(self._command("conversation.thread.archive"))
        self.assertEqual(plan.events[0].event_type, ev.EV_THREAD_ARCHIVED)
        self.assertIsNotNone(plan.side_effect)
        await plan.side_effect()
        self.executor.close_thread.assert_awaited()
        self._apply(ev.EV_THREAD_ARCHIVED, 1)

        state = self.conv.get_state(self.thread.thread_id)
        self.assertEqual(state.status, "archived")
        # Simulate queue paused by archive side effect semantics.
        self.conv.save_state(state.model_copy(update={
            "queue_paused": True,
            "queue_pause_reason": "thread_archived",
            "queue_count": 2,
        }))

        unplan = self.handler._plan_thread_unarchive(
            self._command("conversation.thread.unarchive", command_id="cmd-2"),
        )
        self.assertEqual(unplan.events[0].event_type, ev.EV_THREAD_UNARCHIVED)
        self.assertIsNone(unplan.side_effect)
        self._apply(ev.EV_THREAD_UNARCHIVED, 2)

        restored = self.conv.get_state(self.thread.thread_id)
        self.assertEqual(restored.status, "active")
        self.assertIsNone(restored.archived_at)
        self.assertTrue(restored.queue_paused)
        self.assertEqual(restored.queue_pause_reason, "thread_archived")
        self.assertEqual(restored.queue_count, 2)

    async def test_archive_is_idempotent_when_already_archived(self) -> None:
        self._apply(ev.EV_THREAD_ARCHIVED, 1)
        self.executor.close_thread.reset_mock()
        plan = self.handler._plan_thread_archive(
            self._command("conversation.thread.archive", command_id="cmd-retry"),
        )
        self.assertEqual(plan.events[0].event_type, ev.EV_THREAD_ARCHIVED)
        self.assertTrue(plan.events[0].payload.get("noop"))
        await plan.side_effect()
        self.executor.close_thread.assert_not_awaited()


    async def test_archived_thread_rejects_turn_send(self) -> None:
        """Archived sessions must refuse turn.send with a clear STATE error."""
        from muteki.platform.command_handlers.base import CommandFailed

        self._apply(ev.EV_THREAD_ARCHIVED, 1)
        state = self.conv.get_state(self.thread.thread_id)
        self.assertEqual(state.status, "archived")

        command = SimpleNamespace(
            command_type="conversation.turn.send",
            command_id="cmd-send-archived",
            aggregate_id=self.thread.thread_id,
            payload={"text": "should not enqueue"},
            actor=SimpleNamespace(id="local-user"),
            idempotency_key="cmd-send-archived",
        )
        with self.assertRaises(CommandFailed) as ctx:
            await self.handler._plan_turn_send(command)
        err = ctx.exception.error
        self.assertEqual(err.code, "conversation.thread.archived")
        self.assertIn("归档", err.message)
        self.assertEqual(self.conv.list_queue(self.thread.thread_id), [])

    async def test_request_turn_rejects_archived_thread(self) -> None:
        from muteki.conversation.manager import ConversationError

        self._apply(ev.EV_THREAD_ARCHIVED, 1)
        with self.assertRaises(ConversationError) as ctx:
            self.manager.request_turn(
                self.thread.thread_id,
                text="bypass",
                command_id="cmd-direct",
                idempotency_key="cmd-direct",
            )
        self.assertIn("归档", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
