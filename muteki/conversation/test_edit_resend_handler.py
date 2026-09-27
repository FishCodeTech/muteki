"""#130: conversation.turn.edit_resend must be registered so Command API finds a handler."""

from __future__ import annotations

import unittest
from types import SimpleNamespace

from muteki.conversation.commands import COMMAND_TYPES, ConversationCommandHandler
from muteki.platform.command_handlers.base import CommandFailed, HandlerRegistry


class EditResendHandlerRegistrationTest(unittest.TestCase):
    def test_edit_resend_in_command_types(self) -> None:
        self.assertIn("conversation.turn.edit_resend", COMMAND_TYPES)
        self.assertIn(
            "conversation.turn.edit_resend",
            ConversationCommandHandler.command_types,
        )

    def test_registry_resolves_edit_resend_handler(self) -> None:
        registry = HandlerRegistry()
        registry.register_command(
            ConversationCommandHandler(SimpleNamespace(), SimpleNamespace()),
        )
        handler = registry.command_handler("conversation.turn.edit_resend")
        self.assertIsInstance(handler, ConversationCommandHandler)

    def test_edit_resend_requires_text(self) -> None:
        handler = ConversationCommandHandler(SimpleNamespace(), SimpleNamespace())
        command = SimpleNamespace(
            command_type="conversation.turn.edit_resend",
            command_id="cmd-edit",
            idempotency_key="idem-edit",
            correlation_id="corr-edit",
            actor=SimpleNamespace(id="actor-1"),
            payload={"turn_id": "turn-1"},
        )
        with self.assertRaises(CommandFailed) as ctx:
            handler._plan_turn_edit_resend(command)
        self.assertEqual(ctx.exception.error.code, "conversation.turn.text_required")


if __name__ == "__main__":
    unittest.main()
