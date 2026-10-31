"""Regression coverage for platform tools and failed-message runtime rebinding."""

import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock

from muteki.conversation.commands import ConversationCommandHandler
from muteki.conversation.models import QueuedTurnRequest
from muteki.conversation.store import ConversationStore
from muteki.platform.capability_catalog import DEFAULT_CATALOG
from muteki.platform.capability_gateway import AgentCapabilityGatewayImpl
from muteki.platform.contracts.capabilities import (
    CapabilityBinding, CapabilityInvocation, ThreadMode,
)
from muteki.platform.contracts.commands import CommandEnvelope, QueryResult
from muteki.platform.store import PlatformStore
from muteki.src.store import SrcStoreError


class CapabilityFallbackTest(unittest.IsolatedAsyncioTestCase):
    async def test_unbound_conversation_dispatches_but_src_still_fails_closed(self):
        store = PlatformStore(':memory:')
        self.addCleanup(store.close)
        api = SimpleNamespace(query=AsyncMock(return_value=QueryResult(
            result={'projects': [{'project_id': 'test-project'}]})))
        gateway = AgentCapabilityGatewayImpl(store, api)
        gateway.src = SimpleNamespace(
            task_for_thread=Mock(side_effect=SrcStoreError(
                'task_binding_invalid', 'Thread has no SRC task')),
            platform=SimpleNamespace(get=Mock()),
        )
        invocation = CapabilityInvocation(tool_name='muteki_list_projects')
        spec = DEFAULT_CATALOG.get(invocation.tool_name)
        for binding_mode, thread_mode, allowed in (
            (ThreadMode.CONVERSATION, 'conversation', True),
            (ThreadMode.SRC, 'src', False),
            (ThreadMode.CONVERSATION, 'src', False),
        ):
            with self.subTest(binding_mode=binding_mode, thread_mode=thread_mode):
                api.query.reset_mock()
                gateway.src.platform.get.return_value = SimpleNamespace(mode=thread_mode)
                binding = CapabilityBinding(thread_id='thread-test', mode=binding_mode)
                result = await gateway._dispatch_with_src_policy(
                    binding, spec, invocation, 'test-correlation')
                self.assertEqual(result.ok, allowed)
                if allowed:
                    self.assertEqual(result.result, {'projects': [{'project_id': 'test-project'}]})
                    api.query.assert_awaited_once()
                    query = api.query.call_args.args[0]
                    self.assertEqual(query.query_type, 'project.list')
                    self.assertEqual(query.actor.thread_id, binding.thread_id)
                else:
                    self.assertEqual(result.error.code, 'task_binding_invalid')
                    api.query.assert_not_awaited()


class QueueRebindTest(unittest.TestCase):
    def test_failed_message_rebind_is_persisted_and_can_be_dispatched(self):
        store = PlatformStore(':memory:')
        self.addCleanup(store.close)
        conv = ConversationStore(store)
        thread = SimpleNamespace(thread_id='thread-test', workspace_id=None)
        item, _ = conv.enqueue_turn(QueuedTurnRequest(
            thread_id=thread.thread_id, text='Keep this question',
            attachments=['attachment-test'], status='failed',
            error={'code': 'runtime_unavailable'},
            runtime={'adapter_id': 'codex_app_server', 'model': 'old-model'},
        ))
        manager = SimpleNamespace(
            store=store, conv=conv, get_thread=lambda _: thread,
            _build_runtime_selection=lambda _, raw: SimpleNamespace(
                model_dump=lambda **kwargs: dict(raw)),
            session_tool_set=lambda _: [], extension_service=None,
            list_threads=lambda: [],
        )
        handler = ConversationCommandHandler(manager, None)
        command = CommandEnvelope(
            command_type='conversation.queue.update', aggregate_id=thread.thread_id,
            payload={'queue_id': item.queue_id, 'runtime': {'model': 'new-model'}},
        )
        plan = handler._plan_queue_update(command)
        plan.local_commit()
        saved = conv.get_queue_item(item.queue_id)
        self.assertEqual(saved.runtime['model'], 'new-model')
        self.assertEqual(saved.runtime['adapter_id'], 'codex_app_server')
        self.assertEqual(saved.text, item.text)
        self.assertEqual(saved.attachments, item.attachments)
        self.assertEqual(saved.capability_refs, item.capability_refs)
        self.assertEqual(saved.status, 'queued')
        self.assertEqual(saved.error, {})
        self.assertEqual(plan.events[0].payload['runtime'], saved.runtime)
        self.assertEqual(conv.mark_queue_dispatching(item.queue_id).status, 'dispatching')
