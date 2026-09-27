"""Queue draft edits/rebinds stay coherent without invoking any Runtime."""
from __future__ import annotations
import asyncio
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from muteki.conversation.commands import ConversationCommandHandler
from muteki.conversation.composer_capabilities import ComposerCapabilityError
from muteki.conversation.manager import ConversationManager
from muteki.conversation.executor import ExternalAgentSessionExecutor
from muteki.conversation.models import QueuedTurnRequest, TurnRecord
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.command_handlers.base import CommandFailed
from muteki.platform.store import PlatformStore


class QueueRebindAuditTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = PlatformStore(Path(self.tmp.name) / "platform.db")
        self.conv = ConversationStore(self.store)
        bindings = CapabilityBindingService(self.store)
        self.manager = ConversationManager(self.store, self.conv, bindings, ConversationProjection(self.store, self.conv), workspace_root=Path(self.tmp.name) / "ws")
        self.manager._validate_credential_binding = lambda **kwargs: None
        project = self.manager.create_project("queue-audit")
        self.thread = self.manager.create_thread(project_id=project.project_id)
        self.handler = ConversationCommandHandler(self.manager, SimpleNamespace())
        self.runtime = {"adapter_id": "codex.app_server", "instance_id": "old", "credential_id": "system:codex", "model": "synthetic", "permission_mode": "old-permission", "sandbox_mode": "old-sandbox"}

    def tearDown(self):
        self.store.close()
        self.tmp.cleanup()

    def item(self, **kwargs):
        return self.conv.enqueue_turn(QueuedTurnRequest(thread_id=self.thread.thread_id, text="original body", attachments=["a" * 64], runtime=self.runtime, status="failed", **kwargs))[0]

    def command(self, item, **payload):
        return SimpleNamespace(command_id="synthetic-update", command_type="conversation.queue.update", aggregate_id=self.thread.thread_id, payload={"queue_id": item.queue_id, **payload}, actor=SimpleNamespace(id="local-user"), idempotency_key="synthetic-update")

    def test_runtime_only_rebind_preserves_body_and_attachment(self):
        item = self.item()
        self.handler._plan_queue_update(self.command(item, runtime={"adapter_id": "codex.app_server", "instance_id": "new", "credential_id": "system:codex", "model": "new-model"}))
        updated = self.conv.get_queue_item(item.queue_id)
        self.assertEqual(updated.text, "original body")
        self.assertEqual(updated.attachments, item.attachments)
        self.assertEqual(updated.runtime["instance_id"], "new")
        self.assertEqual(updated.runtime["permission_mode"], "")
        self.assertEqual(updated.runtime["sandbox_mode"], "")

    def test_invalid_refs_leave_failed_item_unchanged(self):
        item = self.item(capability_refs=[{"id": "old-reference"}])
        with patch("muteki.conversation.commands.resolve_capability_refs", side_effect=ComposerCapabilityError("reference unavailable")):
            with self.assertRaises(CommandFailed) as caught:
                self.handler._plan_queue_update(self.command(item, runtime={"instance_id": "new"}))
        self.assertEqual(caught.exception.error.code, "conversation.queue.reference_invalid")
        self.assertEqual(self.conv.get_queue_item(item.queue_id), item)

    def test_compatible_refs_revalidate_against_selected_runtime(self):
        refs = [{"id": "source", "context_schema": 2, "node_id": "n1"}]
        item = self.item(capability_refs=refs)
        with patch("muteki.conversation.commands.resolve_capability_refs", return_value=(refs, "synthetic context")) as resolve:
            self.handler._plan_queue_update(self.command(item, runtime={"instance_id": "new"}))
        self.assertEqual(resolve.call_args.kwargs["engine"], "codex")
        self.assertEqual(self.conv.get_queue_item(item.queue_id).capability_refs, refs)

    def test_edit_to_plain_text_discards_old_command_metadata(self):
        item = self.item(runtime_invocation={"id": "old-command", "revision": 4})
        self.handler._plan_queue_update(self.command(item, text="a new ordinary question"))
        self.assertEqual(self.conv.get_queue_item(item.queue_id).runtime_invocation, {})

    def test_rebind_runtime_command_requires_fresh_catalog_identity(self):
        item = self.item(runtime_invocation={"id": "old-id", "revision": 4})
        self.handler._plan_queue_update(self.command(item, text="/status details", runtime={"instance_id": "new"}))
        invocation = self.conv.get_queue_item(item.queue_id).runtime_invocation
        self.assertEqual(invocation, {"id": "", "wire_text": "/status", "arguments": "details", "pending_resolution": True})

    def test_consumed_or_cancelled_item_cannot_be_resurrected(self):
        item = self.item()
        self.conv.mark_queue_consumed(item.queue_id, "turn-synthetic")
        with self.assertRaises(ValueError):
            self.conv.update_queue_item(self.thread.thread_id, item.queue_id, text="new")
        with self.assertRaises(ValueError):
            self.conv.mark_queue_dispatching(item.queue_id)

    def test_dispatch_uses_latest_atomically_claimed_draft(self):
        item = self.item()
        stale = self.conv.update_queue_item(self.thread.thread_id, item.queue_id, text="stale body")
        self.conv.update_queue_item(self.thread.thread_id, item.queue_id, text="updated body", runtime={**self.runtime, "model": "updated-model"})
        self.conv.next_queue_item = lambda _tid: stale
        turn = TurnRecord(turn_id="synthetic-turn", thread_id=self.thread.thread_id)
        self.manager.save_runtime_selection = Mock()
        self.manager.request_turn = Mock(return_value=(turn, None, None, True))
        executor = ExternalAgentSessionExecutor(self.store, self.conv, self.manager, SimpleNamespace(), sessions_root=Path(self.tmp.name))
        executor.start_turn = Mock()
        self.assertTrue(asyncio.run(executor.start_next_queued(self.thread.thread_id)))
        self.assertEqual(self.manager.request_turn.call_args.kwargs["text"], "updated body")
        self.assertEqual(self.manager.save_runtime_selection.call_args.args[1]["model"], "updated-model")
        self.assertEqual(self.conv.get_queue_item(item.queue_id).status, "consumed")


    def test_concurrent_dispatch_does_not_overwrite_a_successful_edit(self):
        item = self.item()
        self.conv.update_queue_item(self.thread.thread_id, item.queue_id, text="old")
        original_get = self.conv.get_queue_item
        observed, release, updated = threading.Event(), threading.Event(), threading.Event()
        outcomes = []
        def delayed_get(queue_id):
            row = original_get(queue_id)
            if threading.current_thread().name == "dispatcher":
                observed.set()
                release.wait(2)
            return row
        self.conv.get_queue_item = delayed_get
        def dispatch():
            self.conv.mark_queue_dispatching(item.queue_id)
        def update():
            try:
                self.conv.update_queue_item(self.thread.thread_id, item.queue_id, text="new")
                outcomes.append("saved")
            except ValueError:
                outcomes.append("conflict")
            finally:
                updated.set()
        first = threading.Thread(target=dispatch, name="dispatcher")
        second = threading.Thread(target=update, name="editor")
        first.start(); self.assertTrue(observed.wait(1)); second.start()
        updated.wait(0.05); release.set()
        first.join(2); second.join(2)
        self.assertFalse(first.is_alive()); self.assertFalse(second.is_alive())
        final = original_get(item.queue_id)
        self.assertEqual(final.status, "dispatching")
        if outcomes == ["saved"]:
            self.assertEqual(final.text, "new")
        else:
            self.assertEqual(outcomes, ["conflict"])


if __name__ == "__main__":
    unittest.main()
