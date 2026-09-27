"""#183: turn.send accepts attachment-only / cite-only drafts (empty text OK)."""

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
from muteki.platform.command_handlers.base import CommandFailed
from muteki.platform.store import PlatformStore


class TurnSendAttachmentOnlyTest(unittest.IsolatedAsyncioTestCase):
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
        self.executor = SimpleNamespace(
            start_next_queued=AsyncMock(return_value=None),
            cached_runtime_capabilities=lambda _tid: None,
        )
        self.handler = ConversationCommandHandler(self.manager, self.executor)
        project = self.manager.create_project(name="183", description="")
        self.thread = self.manager.create_thread(
            project_id=project.project_id, title="attachment-only",
        )
        self.manager.save_runtime_selection(
            self.thread.thread_id,
            {
                "adapter_id": "codex.app_server",
                "instance_id": "default",
                "model": "gpt-5",
            },
            validate_credential=False,
        )

    async def asyncTearDown(self) -> None:
        self._tmp.cleanup()

    def _command(self, payload: dict, *, command_id: str = "cmd-send") -> SimpleNamespace:
        return SimpleNamespace(
            command_type="conversation.turn.send",
            command_id=command_id,
            aggregate_id=self.thread.thread_id,
            payload=payload,
            actor=SimpleNamespace(id="local-user"),
            idempotency_key=command_id,
        )

    async def test_empty_text_with_attachments_enqueues(self) -> None:
        digest = "a" * 64
        plan = await self.handler._plan_turn_send(self._command({
            "text": "   ",
            "attachments": [digest, "", "  "],
        }))
        self.assertEqual(plan.events[0].event_type, ev.EV_QUEUE_ADDED)
        self.assertEqual(plan.events[0].payload["text"], "")
        self.assertEqual(plan.events[0].payload["attachments"], [digest])
        queue = self.conv.list_queue(self.thread.thread_id)
        self.assertEqual(len(queue), 1)
        self.assertEqual(queue[0].text, "")
        self.assertEqual(queue[0].attachments, [digest])

    async def test_cite_only_enqueues_without_text(self) -> None:
        ref = {
            "context_schema": 2,
            "node_id": "cite-183",
            "id": "message_span:183",
            "kind": "message_span",
            "name": "摘录",
            "description": "hello",
            "source": "对话",
            "scope": "thread",
            "locator": {
                "message_id": "missing",
                "start_offset": 0,
                "end_offset": 5,
            },
            "snapshot": {
                "label": "摘录",
                "text": "hello world",
                "captured_at": "2026-01-01T00:00:00Z",
            },
        }
        plan = await self.handler._plan_turn_send(self._command({
            "text": "",
            "capability_refs": [ref],
        }, command_id="cmd-cite"))
        self.assertEqual(plan.events[0].event_type, ev.EV_QUEUE_ADDED)
        self.assertEqual(plan.events[0].payload["text"], "")
        self.assertTrue(plan.events[0].payload["capability_refs"])
        queue = self.conv.list_queue(self.thread.thread_id)
        self.assertEqual(len(queue), 1)
        self.assertEqual(queue[0].text, "")
        self.assertTrue(queue[0].capability_refs)

    async def test_text_plus_attachments_enqueues(self) -> None:
        digest = "b" * 64
        plan = await self.handler._plan_turn_send(self._command({
            "text": "请看附件",
            "attachments": [digest],
        }, command_id="cmd-both"))
        self.assertEqual(plan.events[0].payload["text"], "请看附件")
        self.assertEqual(plan.events[0].payload["attachments"], [digest])
        queue = self.conv.list_queue(self.thread.thread_id)
        self.assertEqual(queue[0].text, "请看附件")
        self.assertEqual(queue[0].attachments, [digest])

    async def test_empty_payload_still_requires_content(self) -> None:
        with self.assertRaises(CommandFailed) as ctx:
            await self.handler._plan_turn_send(self._command({
                "text": "",
                "attachments": [],
                "capability_refs": [],
            }, command_id="cmd-empty"))
        err = ctx.exception.error
        self.assertEqual(err.code, "conversation.turn.text_required")
        self.assertIn("正文", err.message)
        self.assertEqual(self.conv.list_queue(self.thread.thread_id), [])


if __name__ == "__main__":
    unittest.main()
