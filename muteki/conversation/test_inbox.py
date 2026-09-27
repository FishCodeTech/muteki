"""Unit tests for Conversation inbox attention summaries (C29)."""

from __future__ import annotations

from muteki.conversation.inbox import (
    KIND_CLEARED,
    KIND_UPDATED,
    InboxBroker,
    attention_fingerprint,
    build_attention_summary,
    is_attention_clear,
)


class _Thread:
    def __init__(self, thread_id: str, title: str = "T", summary: str = "") -> None:
        self.thread_id = thread_id
        self.title = title
        self.summary = summary
        self.updated_at = "2026-09-14T00:00:00Z"
        self.mode = "conversation"
        self.project_id = None
        self.workspace_id = None

    def model_dump(self, mode: str = "json") -> dict:
        return {
            "thread_id": self.thread_id,
            "title": self.title,
            "summary": self.summary,
            "updated_at": self.updated_at,
            "mode": self.mode,
            "project_id": self.project_id,
            "workspace_id": self.workspace_id,
        }


class _State:
    def __init__(self, **kwargs) -> None:
        self.status = kwargs.get("status", "active")
        self.running_turn_id = kwargs.get("running_turn_id")
        self.pending_approval = kwargs.get("pending_approval")
        self.pending_user_input = kwargs.get("pending_user_input")
        self.last_error = kwargs.get("last_error")
        self.head_stream_seq = kwargs.get("head_stream_seq", 0)
        self.read_stream_seq = kwargs.get("read_stream_seq", 0)

    @property
    def unread(self) -> bool:
        return self.head_stream_seq > self.read_stream_seq

    def model_dump(self, mode: str = "json") -> dict:
        return {
            "status": self.status,
            "running_turn_id": self.running_turn_id,
            "pending_approval": self.pending_approval,
            "pending_user_input": self.pending_user_input,
            "last_error": self.last_error,
            "head_stream_seq": self.head_stream_seq,
            "read_stream_seq": self.read_stream_seq,
        }


def test_build_summary_pending_user_input() -> None:
    thread = _Thread("t1", title="BG-B")
    state = _State(
        pending_user_input={"request_id": "req-1"},
        head_stream_seq=3,
        read_stream_seq=1,
        running_turn_id="turn-1",
    )
    row = build_attention_summary(thread, state)
    assert row["summary"]["pending_kind"] == "user_input"
    assert row["summary"]["pending_id"] == "req-1"
    assert row["summary"]["unread"] is True
    assert row["thread"]["state"]["unread"] is True
    assert not is_attention_clear(row["summary"])


def test_broker_seed_then_diff_and_clear() -> None:
    broker = InboxBroker()
    thread = _Thread("t1", title="BG-B")
    idle = build_attention_summary(thread, _State(head_stream_seq=1, read_stream_seq=1))
    broker.seed_rows([idle])
    assert broker.sync_rows([idle]) == []

    pending = build_attention_summary(
        thread,
        _State(
            pending_user_input={"request_id": "req-1"},
            head_stream_seq=2,
            read_stream_seq=1,
        ),
    )
    emitted = broker.sync_rows([pending])
    assert len(emitted) == 1
    assert emitted[0].kind == KIND_UPDATED
    assert emitted[0].seq == 1

    # Reconnect-style replay of same fingerprint must not emit again.
    assert broker.sync_rows([pending]) == []

    cleared = build_attention_summary(
        thread,
        _State(head_stream_seq=2, read_stream_seq=2),
    )
    cleared_events = broker.sync_rows([cleared])
    assert len(cleared_events) == 1
    assert cleared_events[0].kind == KIND_CLEARED
    assert cleared_events[0].seq == 2
    assert [event.seq for event in broker.events_after(0)] == [1, 2]
    assert [event.seq for event in broker.events_after(1)] == [2]


def test_fingerprint_stable() -> None:
    left = attention_fingerprint(
        thread_id="t1",
        revision=2,
        status="active",
        running=False,
        unread=True,
        pending_kind="user_input",
        pending_id="req-1",
        title="BG-B",
    )
    right = attention_fingerprint(
        thread_id="t1",
        revision=2,
        status="active",
        running=False,
        unread=True,
        pending_kind="user_input",
        pending_id="req-1",
        title="BG-B",
    )
    assert left == right
