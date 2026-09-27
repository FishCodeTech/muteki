"""Cross-thread Conversation inbox summaries (C29).

Builds attention fingerprints from Thread list rows and maintains a small
in-process log so ``GET /api/threads/inbox/events`` can resume with ``after``.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass
from typing import Any, Iterable, Optional
from uuid import uuid4


PENDING_NONE = "none"
PENDING_APPROVAL = "approval"
PENDING_USER_INPUT = "user_input"
PENDING_FAILED = "failed"

KIND_UPDATED = "attention.updated"
KIND_CLEARED = "attention.cleared"


def _pending_fields(state: Any) -> tuple[str, Optional[str]]:
    approvals = getattr(state, "pending_approvals", None) or {}
    if isinstance(approvals, dict):
        for row in approvals.values():
            if not isinstance(row, dict):
                continue
            if str(row.get("status") or "pending") != "pending":
                continue
            pending_id = str(
                row.get("approval_id")
                or row.get("id")
                or row.get("request_id")
                or ""
            ).strip() or None
            return PENDING_APPROVAL, pending_id

    approval = getattr(state, "pending_approval", None) or None
    if isinstance(approval, dict) and approval and str(approval.get("status") or "pending") == "pending":
        pending_id = str(
            approval.get("approval_id")
            or approval.get("id")
            or approval.get("request_id")
            or ""
        ).strip() or None
        return PENDING_APPROVAL, pending_id

    user_input = getattr(state, "pending_user_input", None) or None
    if isinstance(user_input, dict) and user_input:
        pending_id = str(
            user_input.get("request_id")
            or user_input.get("id")
            or user_input.get("approval_id")
            or ""
        ).strip() or None
        return PENDING_USER_INPUT, pending_id

    last_error = getattr(state, "last_error", None) or None
    # Keep the failure in ThreadState for inspection, but stop treating it as
    # pending attention once the user has viewed the thread.
    if (isinstance(last_error, dict) and last_error
            and bool(getattr(state, "unread", False))):
        return PENDING_FAILED, None
    return PENDING_NONE, None


def attention_fingerprint(
    *,
    thread_id: str,
    revision: int,
    status: str,
    running: bool,
    unread: bool,
    pending_kind: str,
    pending_id: Optional[str],
    title: str,
) -> str:
    return "|".join(
        (
            thread_id,
            str(int(revision or 0)),
            str(status or ""),
            "1" if running else "0",
            "1" if unread else "0",
            str(pending_kind or PENDING_NONE),
            str(pending_id or ""),
            str(title or ""),
        )
    )


def build_attention_summary(thread: Any, state: Any) -> dict[str, Any]:
    """Build one ThreadAttentionSummary (+ list row) from Thread + ThreadState."""
    pending_kind, pending_id = _pending_fields(state)
    unread = bool(getattr(state, "unread", False))
    running = bool(getattr(state, "running_turn_id", None))
    revision = int(getattr(state, "head_stream_seq", 0) or 0)
    status = str(getattr(state, "status", "") or "active")
    title = str(getattr(thread, "title", "") or "")
    fingerprint = attention_fingerprint(
        thread_id=str(thread.thread_id),
        revision=revision,
        status=status,
        running=running,
        unread=unread,
        pending_kind=pending_kind,
        pending_id=pending_id,
        title=title,
    )
    thread_row = {
        **thread.model_dump(mode="json"),
        "state": {
            **state.model_dump(mode="json"),
            "unread": unread,
        },
    }
    summary = {
        "thread_id": str(thread.thread_id),
        "revision": revision,
        "status": status,
        "running": running,
        "unread": unread,
        "pending_kind": pending_kind,
        "pending_id": pending_id,
        "title": title,
        "preview": str(getattr(thread, "summary", "") or ""),
        "updated_at": str(getattr(thread, "updated_at", "") or ""),
        "fingerprint": fingerprint,
    }
    return {"summary": summary, "thread": thread_row, "fingerprint": fingerprint}


def is_attention_clear(summary: dict[str, Any]) -> bool:
    return (
        str(summary.get("pending_kind") or PENDING_NONE) == PENDING_NONE
        and not bool(summary.get("unread"))
        and not bool(summary.get("running"))
        and str(summary.get("status") or "") != "archived"
    )


@dataclass(frozen=True)
class InboxEvent:
    seq: int
    event_id: str
    kind: str
    summary: dict[str, Any]
    thread: dict[str, Any]

    def as_payload(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "seq": self.seq,
            "kind": self.kind,
            "summary": self.summary,
            "thread": self.thread,
        }


class InboxBroker:
    """In-process inbox cursor + ring buffer for SSE resume."""

    def __init__(self, *, maxlen: int = 2000) -> None:
        self._seq = 0
        self._events: deque[InboxEvent] = deque(maxlen=maxlen)
        self._fingerprints: dict[str, str] = {}

    @property
    def seq(self) -> int:
        return self._seq

    def events_after(self, after_seq: int, *, limit: int = 200) -> list[InboxEvent]:
        out: list[InboxEvent] = []
        for event in self._events:
            if event.seq <= after_seq:
                continue
            out.append(event)
            if len(out) >= limit:
                break
        return out

    def seed_rows(self, rows: Iterable[dict[str, Any]]) -> None:
        """Set fingerprints without emitting (used for SSE snapshot bootstrap)."""
        seen: set[str] = set()
        for row in rows:
            thread_id = str(row["summary"]["thread_id"])
            seen.add(thread_id)
            self._fingerprints[thread_id] = str(row["fingerprint"])
        for thread_id in list(self._fingerprints):
            if thread_id not in seen:
                self._fingerprints.pop(thread_id, None)

    def sync_rows(self, rows: Iterable[dict[str, Any]]) -> list[InboxEvent]:
        """Diff attention rows against last fingerprints; append new inbox events."""
        seen: set[str] = set()
        emitted: list[InboxEvent] = []
        for row in rows:
            summary = row["summary"]
            thread = row["thread"]
            thread_id = str(summary["thread_id"])
            fingerprint = str(row["fingerprint"])
            seen.add(thread_id)
            previous = self._fingerprints.get(thread_id)
            if previous == fingerprint:
                continue
            self._fingerprints[thread_id] = fingerprint
            kind = KIND_CLEARED if is_attention_clear(summary) else KIND_UPDATED
            event = self._append(kind=kind, summary=summary, thread=thread)
            emitted.append(event)

        for thread_id in list(self._fingerprints):
            if thread_id in seen:
                continue
            # Thread disappeared from the list (rare); clear local fingerprint.
            previous = self._fingerprints.pop(thread_id, None)
            if previous is None:
                continue
            summary = {
                "thread_id": thread_id,
                "revision": 0,
                "status": "archived",
                "running": False,
                "unread": False,
                "pending_kind": PENDING_NONE,
                "pending_id": None,
                "title": "",
                "preview": "",
                "updated_at": "",
                "fingerprint": f"{thread_id}|gone",
            }
            event = self._append(
                kind=KIND_CLEARED,
                summary=summary,
                thread={
                    "thread_id": thread_id,
                    "title": "",
                    "mode": "conversation",
                    "state": {
                        "status": "archived",
                        "unread": False,
                        "running_turn_id": None,
                        "pending_approval": None,
                        "pending_user_input": None,
                    },
                },
            )
            emitted.append(event)
        return emitted

    def _append(
        self,
        *,
        kind: str,
        summary: dict[str, Any],
        thread: dict[str, Any],
    ) -> InboxEvent:
        self._seq += 1
        pending_kind = str(summary.get("pending_kind") or PENDING_NONE)
        pending_id = str(summary.get("pending_id") or "")
        event_id = (
            f"inbox:{summary.get('thread_id')}:{pending_kind}:"
            f"{pending_id or summary.get('revision')}:{self._seq}:{uuid4().hex[:8]}"
        )
        event = InboxEvent(
            seq=self._seq,
            event_id=event_id,
            kind=kind,
            summary=summary,
            thread=thread,
        )
        self._events.append(event)
        return event


def collect_attention_rows(manager: Any, project_id: str = "") -> list[dict[str, Any]]:
    threads = manager.list_threads(project_id)
    rows: list[dict[str, Any]] = []
    for thread in threads:
        state = manager.conv.get_state(thread.thread_id)
        rows.append(build_attention_summary(thread, state))
    return rows
