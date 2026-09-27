"""#122: cross-runtime / mid-turn HITL must keep session + pending approvals."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

from muteki.conversation import events as ev
from muteki.conversation.commands import ConversationCommandHandler
from muteki.conversation.models import TURN_RUNNING, ThreadState, TurnRecord
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.platform.command_handlers.base import SideEffectResult
from muteki.platform.contracts.objects import AgentSession
from muteki.platform.contracts.receipts import ReceiptState
from muteki.platform.store import PlatformStore


class SessionClosedProjectionTest(unittest.TestCase):
    def test_session_closed_keeps_running_turn_and_approvals(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / "platform.db"
            store = PlatformStore(db_path=db)
            conv = ConversationStore(store)
            projection = ConversationProjection(store, conv)
            thread_id = "thr-122"
            turn_id = "turn-122"
            conv.save_turn(TurnRecord(
                turn_id=turn_id,
                thread_id=thread_id,
                status=TURN_RUNNING,
                agent_session_id="asess-live",
                seq=1,
            ))
            conv.claim_active_turn(thread_id, turn_id)
            conv.save_state(ThreadState(
                thread_id=thread_id,
                running_turn_id=turn_id,
                agent_session_id="asess-live",
                session_runtime_key="codex:default|cred|access_mode=supervised|permission_mode=|sandbox_mode=",
                pending_approvals={
                    "appr-1": {
                        "approval_id": "appr-1",
                        "status": "pending",
                        "command": "pytest",
                    }
                },
                pending_approval={
                    "approval_id": "appr-1",
                    "status": "pending",
                    "command": "pytest",
                },
            ))

            closed = ev.thread_event(thread_id, ev.EV_SESSION_CLOSED, {
                "agent_session_id": "asess-live",
                "reason": "capability_probe",
            })
            closed = closed.model_copy(update={"stream_seq": 1})
            projection.apply(closed)

            state = conv.get_state(thread_id)
            self.assertEqual(state.running_turn_id, turn_id)
            self.assertIsNone(state.agent_session_id)
            self.assertEqual(
                state.session_runtime_key,
                "codex:default|cred|access_mode=supervised|permission_mode=|sandbox_mode=",
            )
            self.assertIn("appr-1", state.pending_approvals)
            self.assertEqual(
                state.pending_approvals["appr-1"]["status"], "pending"
            )

    def test_session_closed_clears_when_turn_idle(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / "platform.db"
            store = PlatformStore(db_path=db)
            conv = ConversationStore(store)
            projection = ConversationProjection(store, conv)
            thread_id = "thr-122-idle"
            conv.save_state(ThreadState(
                thread_id=thread_id,
                running_turn_id=None,
                agent_session_id="asess-old",
                session_runtime_key="codex:default",
                pending_approvals={
                    "appr-x": {
                        "approval_id": "appr-x",
                        "status": "pending",
                        "command": "ls",
                    }
                },
                pending_approval={
                    "approval_id": "appr-x",
                    "status": "pending",
                    "command": "ls",
                },
            ))
            closed = ev.thread_event(thread_id, ev.EV_SESSION_CLOSED, {
                "agent_session_id": "asess-old",
                "reason": "thread_archived",
            })
            closed = closed.model_copy(update={"stream_seq": 1})
            projection.apply(closed)
            state = conv.get_state(thread_id)
            self.assertIsNone(state.running_turn_id)
            self.assertIsNone(state.agent_session_id)
            self.assertEqual(state.session_runtime_key, "")
            self.assertEqual(
                state.pending_approvals["appr-x"]["status"], "expired"
            )


class ApprovalResolveDeliveryOrderTest(unittest.IsolatedAsyncioTestCase):
    async def test_resolve_keeps_pending_when_delivery_fails(self) -> None:
        handler = ConversationCommandHandler(
            SimpleNamespace(conv=SimpleNamespace(get_state=lambda _tid: ThreadState(
                thread_id="thr-a",
                pending_approvals={
                    "appr-1": {
                        "approval_id": "appr-1",
                        "status": "pending",
                        "command": "echo",
                    }
                },
                pending_approval={
                    "approval_id": "appr-1",
                    "status": "pending",
                    "command": "echo",
                },
            ))),
            executor=SimpleNamespace(
                resolve_approval=AsyncMock(
                    side_effect=LookupError("thread thr-a 没有活跃 AgentSession")
                )
            ),
        )
        # Bypass _require_thread by stubbing plan internals via direct call after
        # patching _require_thread.
        handler._require_thread = lambda command: SimpleNamespace(  # type: ignore[method-assign]
            thread_id="thr-a"
        )
        command = SimpleNamespace(
            command_id="cmd-1",
            actor=SimpleNamespace(id="user"),
            idempotency_key="idem-1",
            payload={
                "approval_id": "appr-1",
                "decision": "allow",
            },
            expected_version=None,
        )
        plan = handler._plan_approval_resolve(command)
        self.assertEqual(
            plan.events[0].event_type, "core.approval.resolve_requested"
        )
        self.assertTrue(callable(plan.side_effect))
        result = await plan.side_effect()
        assert isinstance(result, SideEffectResult)
        self.assertEqual(result.state, ReceiptState.FAILED)
        self.assertEqual(
            result.error.code if result.error else None,
            "conversation.approval.delivery_failed",
        )
        self.assertEqual(result.events, [])

    async def test_resolve_emits_resolved_after_delivery(self) -> None:
        handler = ConversationCommandHandler(
            SimpleNamespace(conv=SimpleNamespace(get_state=lambda _tid: ThreadState(
                thread_id="thr-b",
                pending_approvals={
                    "appr-2": {
                        "approval_id": "appr-2",
                        "status": "pending",
                        "command": "echo",
                    }
                },
                pending_approval={
                    "approval_id": "appr-2",
                    "status": "pending",
                    "command": "echo",
                },
            ))),
            executor=SimpleNamespace(
                resolve_approval=AsyncMock(return_value=None)
            ),
        )
        handler._require_thread = lambda command: SimpleNamespace(  # type: ignore[method-assign]
            thread_id="thr-b"
        )
        command = SimpleNamespace(
            command_id="cmd-2",
            actor=SimpleNamespace(id="user"),
            idempotency_key="idem-2",
            payload={
                "approval_id": "appr-2",
                "decision": "allow",
            },
            expected_version=None,
        )
        plan = handler._plan_approval_resolve(command)
        result = await plan.side_effect()
        assert isinstance(result, SideEffectResult)
        self.assertEqual(result.state, ReceiptState.COMPLETED)
        self.assertEqual(len(result.events), 1)
        self.assertEqual(result.events[0].event_type, ev.EV_APPROVAL_RESOLVED)


class SessionRecordRecoveryTest(unittest.TestCase):
    def test_session_record_rebinds_open_session(self) -> None:
        from muteki.conversation.executor import ExternalAgentSessionExecutor

        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / "platform.db"
            store = PlatformStore(db_path=db)
            conv = ConversationStore(store)
            thread_id = "thr-recover"
            session = AgentSession(
                agent_session_id="asess-open",
                adapter_id="codex",
                runtime_instance_id="default",
                thread_id=thread_id,
            )
            store.save(session)
            conv.save_state(ThreadState(
                thread_id=thread_id,
                running_turn_id="turn-r",
                agent_session_id=None,
                session_runtime_key="codex:default",
            ))
            manager = SimpleNamespace(
                runtime_selection=lambda _tid: SimpleNamespace(
                    adapter_id="codex",
                    instance_id="default",
                    runtime_key="codex:default",
                    session_key="codex:default|",
                    credential_id="",
                    model="",
                    effort="",
                    access_mode="",
                    permission_mode="",
                    sandbox_mode="",
                ),
                bindings=None,
            )
            executor = ExternalAgentSessionExecutor(
                store=store,
                conv=conv,
                manager=manager,
                registry=SimpleNamespace(get=lambda *_a, **_k: None),
                sessions_root=Path(tmp),
            )
            record = executor._session_record(thread_id)
            self.assertIsNotNone(record)
            assert record is not None
            self.assertEqual(record.agent_session_id, "asess-open")
            state = conv.get_state(thread_id)
            self.assertEqual(state.agent_session_id, "asess-open")


if __name__ == "__main__":
    unittest.main()
