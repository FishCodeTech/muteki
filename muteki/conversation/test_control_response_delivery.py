"""Control rejection events must not be mistaken for delivered decisions."""
from __future__ import annotations

import asyncio
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock

from muteki.conversation.commands import ConversationCommandHandler
from muteki.conversation.executor import ExternalAgentSessionExecutor
from muteki.conversation.models import ThreadState
from muteki.external_agents.codex import CodexAppServerAdapter
from muteki.platform.contracts.external_agents import (
    AgentEvent, AgentEventType, AgentSessionRef,
)
from muteki.platform.contracts.receipts import ReceiptState


class ControlResponseDeliveryTest(unittest.IsolatedAsyncioTestCase):
    def executor(self, adapter):
        executor = ExternalAgentSessionExecutor.__new__(ExternalAgentSessionExecutor)
        ref = AgentSessionRef(agent_session_id="session", adapter_id="codex.app_server")
        executor._ref_for = lambda _: (adapter, ref)
        executor._manager = SimpleNamespace(capture_user_input_fixture=lambda *a, **k: False)
        executor._stop_detached_runtime = AsyncMock()
        translated = []
        executor._translate = lambda tid, turn, event: translated.append(event) or False
        return executor, translated

    def handler(self, executor):
        state = ThreadState(thread_id="thread", pending_approvals={
            "live": {"approval_id": "live", "status": "pending"},
            "stale": {"approval_id": "stale", "status": "pending"},
        })
        handler = ConversationCommandHandler(
            SimpleNamespace(conv=SimpleNamespace(get_state=lambda _: state)), executor)
        handler._require_thread = lambda _: SimpleNamespace(thread_id="thread")
        return handler, state

    def command(self, approval_id):
        return SimpleNamespace(command_id="command", actor=SimpleNamespace(id="qa"),
                               idempotency_key="qa-control", expected_version=None,
                               payload={"approval_id": approval_id, "decision": "deny"})

    async def test_real_codex_rejection_keeps_both_pending_items(self):
        adapter = CodexAppServerAdapter(schema_probe=False)
        live = asyncio.get_running_loop().create_future()
        adapter._runs["session"] = {"approvals": {"live": live}}
        executor, translated = self.executor(adapter)
        handler, state = self.handler(executor)
        result = await handler._plan_approval_resolve(self.command("stale")).side_effect()
        self.assertIs(result.state, ReceiptState.FAILED)
        self.assertEqual(result.error.code, "conversation.approval.delivery_failed")
        self.assertEqual(result.events, [])
        self.assertEqual(set(state.pending_approvals), {"live", "stale"})
        self.assertFalse(live.done())
        self.assertFalse(translated, "a control error belongs to its command receipt")
        executor._stop_detached_runtime.assert_not_awaited()
        live.cancel()

    async def test_real_codex_success_resolves_only_requested_future(self):
        adapter = CodexAppServerAdapter(schema_probe=False)
        live = asyncio.get_running_loop().create_future()
        sibling = asyncio.get_running_loop().create_future()
        adapter._runs["session"] = {"approvals": {"live": live, "sibling": sibling}}
        executor, _ = self.executor(adapter)
        handler, _ = self.handler(executor)
        result = await handler._plan_approval_resolve(self.command("live")).side_effect()
        self.assertIs(result.state, ReceiptState.COMPLETED)
        self.assertEqual(live.result(), {"decision": "decline"})
        self.assertFalse(sibling.done())
        self.assertEqual(len(result.events), 1)
        self.assertEqual(result.events[0].payload["approval_id"], "live")
        sibling.cancel()

    async def test_error_only_user_input_response_raises(self):
        async def send(*_):
            yield AgentEvent(event_type=AgentEventType.RUNTIME_ERROR,
                             payload={"detail": "reply rejected"})
        executor, translated = self.executor(SimpleNamespace(send=send))
        with self.assertRaisesRegex(RuntimeError, "待处理请求已保留"):
            await executor.resolve_user_input("thread", "request", text="answer")
        self.assertFalse(translated)

    async def test_early_resolution_does_not_escape_a_failed_stream(self):
        async def send(*_):
            yield AgentEvent(event_type=AgentEventType.APPROVAL_RESOLVED,
                             payload={"approval_id": "stale"})
            yield AgentEvent(event_type=AgentEventType.RUNTIME_ERROR,
                             payload={"detail": "late failure"})
        executor, translated = self.executor(SimpleNamespace(send=send))
        handler, _ = self.handler(executor)
        result = await handler._plan_approval_resolve(self.command("stale")).side_effect()
        self.assertIs(result.state, ReceiptState.FAILED)
        self.assertEqual(result.events, [])
        self.assertFalse(translated)

    async def test_warning_does_not_reject_success(self):
        async def send(*_):
            yield AgentEvent(event_type=AgentEventType.RUNTIME_WARNING,
                             payload={"detail": "diagnostic"})
        executor, translated = self.executor(SimpleNamespace(send=send))
        await executor.resolve_approval("thread", "live", "deny")
        self.assertEqual([e.event_type for e in translated], [AgentEventType.RUNTIME_WARNING])

    async def test_control_exit_reports_failure_and_releases_session(self):
        async def send(*_):
            yield AgentEvent(event_type=AgentEventType.RUNTIME_EXITED)
        executor, _ = self.executor(SimpleNamespace(send=send))
        with self.assertRaises(RuntimeError):
            await executor.resolve_approval("thread", "live", "deny")
        executor._stop_detached_runtime.assert_awaited_once()


if __name__ == "__main__":
    unittest.main()
