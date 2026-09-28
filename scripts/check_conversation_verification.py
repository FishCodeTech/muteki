"""Check automatic model verification with fixture events, without model calls.

Run from the repository root: python scripts/check_conversation_verification.py
"""
from __future__ import annotations

import asyncio
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from apps.web.worker_models import CredentialModelCatalogStore
from muteki.conversation import events as ev
from muteki.conversation.executor import ExternalAgentSessionExecutor
from muteki.conversation.models import ThreadRuntimeSelection, TurnRecord
from muteki.platform.contracts.external_agents import AgentEvent, AgentEventType as Event


def event(kind: Event, **payload) -> AgentEvent:
    return AgentEvent(event_type=kind, payload=payload, agent_session_id="test-session")


async def check() -> None:
    with tempfile.TemporaryDirectory(prefix="muteki-model-verification-") as temp:
        catalog = CredentialModelCatalogStore(temp)
        selection = ThreadRuntimeSelection(
            adapter_id="grok.acp", instance_id="other", credential_id="system:grok",
            model="grok-4.7", effort="high",
        )
        cases = [
            ("success", [event(Event.MESSAGE_COMPLETED, text="Hello"), event(Event.TURN_COMPLETED)], True),
            ("empty", [event(Event.TURN_COMPLETED)], False),
            ("whitespace", [event(Event.MESSAGE_COMPLETED, text="  "), event(Event.TURN_COMPLETED)], False),
            ("user-only", [event(Event.MESSAGE_COMPLETED, role="user", text="Hello"), event(Event.TURN_COMPLETED)], False),
            ("failed", [event(Event.MESSAGE_COMPLETED, text="Partial"), event(Event.TURN_FAILED)], False),
            ("interrupted", [event(Event.MESSAGE_COMPLETED, text="Partial"), event(Event.TURN_FAILED, reason="interrupted")], False),
            ("stream-ended", [event(Event.MESSAGE_COMPLETED, text="Partial")], False),
            ("runtime-error", [event(Event.RUNTIME_ERROR, code="test"), event(Event.MESSAGE_COMPLETED, text="Partial"), event(Event.TURN_COMPLETED)], False),
            ("failed-then-completed", [event(Event.MESSAGE_COMPLETED, text="Partial"), event(Event.TURN_FAILED), event(Event.TURN_COMPLETED)], False),
        ]
        for name, events, expected in cases:
            turn = TurnRecord(thread_id="test", status="running")
            state = SimpleNamespace(pending_approvals={}, pending_approval=None, pending_user_input=None)
            emitted = []
            def append_events(item):
                emitted.append(item)
                if item.event_type in {ev.EV_TURN_COMPLETED, ev.EV_TURN_FAILED, ev.EV_TURN_INTERRUPTED}:
                    turn.status = item.event_type.rsplit(".", 1)[-1]
                if item.event_type == ev.EV_TURN_COMPLETED and expected:
                    # The catalog must be readable by the time completion reaches SSE.
                    assert "grok-4.7" in catalog.get("system:grok", "grok", "local", "grok.acp:other")["verified_models"]
            executor = ExternalAgentSessionExecutor(
                SimpleNamespace(append_events=append_events),
                SimpleNamespace(get_turn=lambda _: turn, get_state=lambda _: state),
                SimpleNamespace(runtime_selection=lambda _: selection),
                None, sessions_root=temp,
            )
            executor._stop_detached_runtime = AsyncMock()
            recorder = Mock(wraps=catalog.record_conversation_success)
            executor.bind_model_success_recorder(recorder)
            async def stream():
                for item in events:
                    yield item
            await executor._consume("test", turn, stream())
            assert recorder.call_count == int(expected), name
            if name == "empty":
                assert emitted[-1].event_type == ev.EV_TURN_FAILED

        # A later picker change must never verify the newly selected model/account.
        current_selection = selection
        executor._manager.runtime_selection = lambda _: current_selection
        recorder.reset_mock()
        async def changed_stream():
            nonlocal current_selection
            current_selection = selection.model_copy(update={"model": "other-model", "credential_id": "account:other"})
            yield event(Event.MESSAGE_COMPLETED, text="Done")
            yield event(Event.TURN_COMPLETED)
        await executor._consume("test", TurnRecord(thread_id="test"), changed_stream())
        assert recorder.call_args.args[0]["model"] == "grok-4.7"
        assert recorder.call_args.args[0]["credential_id"] == "system:grok"
        assert catalog.get("account:other", "grok", "local", "grok.acp:other") is None
        assert catalog.get("system:grok", "grok", "container", "grok.acp:other") is None
        assert catalog.get("system:grok", "grok", "local", "grok.acp:default") is None

        # Status/help/runtime commands and control responses do not prove a model call.
        for command_turn in [None, TurnRecord(thread_id="test", runtime_invocation={"kind": "command", "name": "status"})]:
            recorder.reset_mock()
            async def local_stream():
                yield event(Event.MESSAGE_COMPLETED, text="Status OK")
                yield event(Event.TURN_COMPLETED)
            await executor._consume("test", command_turn, local_stream())
            recorder.assert_not_called()

        # Verification is auxiliary: disk failure cannot turn a good reply into an error.
        executor.bind_model_success_recorder(Mock(side_effect=OSError("fixture disk error")))
        with patch("muteki.conversation.executor.LOG.warning") as warning:
            await executor._consume("test", TurnRecord(thread_id="test"), local_stream())
            warning.assert_called_once()
        assert emitted[-1].event_type == ev.EV_TURN_COMPLETED

        reopened = CredentialModelCatalogStore(temp)
        assert reopened.get("system:grok", "grok", "local", "grok.acp:other")["verified_models"] == ["grok-4.7"]
        before = reopened.path.read_bytes()
        for model in ["", "default"]:
            reopened.record_conversation_success({**selection.model_dump(), "model": model})
        assert reopened.path.read_bytes() == before
        print("Automatic chat verification: success, failure, interruption, isolation, persistence, and callback errors passed")


if __name__ == "__main__":
    asyncio.run(check())
