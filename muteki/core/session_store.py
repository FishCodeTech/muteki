"""Durable event log — append every event to JSONL (one file per run), replay later.

This is what makes "replay any challenge's full solve after the match" work.
It registers as a sink on the EventBus so persistence is automatic.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any, AsyncIterator, Iterator, Mapping

from muteki.core.events import Event
from muteki.core.path_ids import decode_run_id, encode_run_id


_REPLAY_YIELD_EVERY = 100
_TRANSPORT_FIELDS = frozenset({"seq", "ts"})
_SUMMARY_EVENT_NEEDLES = tuple(
    f'"{value}"'.encode("ascii")
    for value in (
        "run.preparing", "run.started", "run.titled", "run.finished",
        "run.reopened", "insight.event", "flag.accepted",
        "blackboard.delta", "followup.started", "followup.completed",
        "followup.failed",
    )
)


def _json_number_field(raw: bytes, field: bytes) -> float:
    """Read a top-level JSON number without decoding a potentially huge payload."""
    marker = b'"' + field + b'"'
    start = raw.find(marker)
    if start < 0:
        return 0.0
    start = raw.find(b":", start + len(marker))
    if start < 0:
        return 0.0
    start += 1
    while start < len(raw) and raw[start] in b" \t":
        start += 1
    end = start
    while end < len(raw) and raw[end] in b"-+0123456789.eE":
        end += 1
    try:
        return float(raw[start:end])
    except (TypeError, ValueError):
        return 0.0


class ProjectionIdentityConflict(ValueError):
    """One local projection identity was reused for different logical content."""


def _object_row(value: object) -> dict[str, Any] | None:
    return value if isinstance(value, dict) else None


def _payload_object(row: Mapping[str, Any]) -> dict[str, Any]:
    payload = row.get("payload")
    return payload if isinstance(payload, dict) else {}


def _logical_event(event: Event | Mapping[str, Any]) -> dict[str, Any]:
    if isinstance(event, Event):
        row = event.model_dump(mode="json")
    else:
        row = dict(event)
    return {
        key: value
        for key, value in row.items()
        if key not in _TRANSPORT_FIELDS
    }


class SessionStore:
    def __init__(self, root: str | Path = "state") -> None:
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        self._locks: dict[str, asyncio.Lock] = {}

    def _path(self, run_id: str) -> Path:
        safe = encode_run_id(run_id)
        return self.root / f"{safe}.jsonl"

    def _lock_for(self, run_id: str) -> asyncio.Lock:
        if run_id not in self._locks:
            self._locks[run_id] = asyncio.Lock()
        return self._locks[run_id]

    async def append(self, event: Event) -> None:
        path = self._path(event.run_id)
        line = event.model_dump_json() + "\n"
        async with self._lock_for(event.run_id):
            # Synchronous append under an async lock; writes are small and the
            # OS buffers them. Keeps ordering per run without a thread pool.
            with path.open("a", encoding="utf-8") as f:
                f.write(line)

    def _append_if_absent_unlocked(
        self, event: Event, *, identity_field: str, identity: str
    ) -> bool:
        if type(identity_field) is not str or not identity_field:
            raise ValueError("identity_field must be exact non-empty text")
        if type(identity) is not str or not identity:
            raise ValueError("identity must be exact non-empty text")
        payload = event.payload
        if payload.get(identity_field) != identity:
            raise ValueError("event payload does not match projection identity")
        expected = _logical_event(event)
        path = self._path(event.run_id)
        if path.exists():
            with path.open("r", encoding="utf-8") as f:
                for raw in f:
                    raw = raw.strip()
                    if not raw:
                        continue
                    try:
                        existing = _object_row(json.loads(raw))
                    except json.JSONDecodeError:
                        continue
                    if existing is None:
                        continue
                    try:
                        existing_event = Event.model_validate(existing)
                    except (TypeError, ValueError):
                        # Identity authority belongs only to rows accepted by the same
                        # typed replay contract. A schema-invalid look-alike must not
                        # suppress the valid projection that replay/load_all can see.
                        continue
                    existing_payload = existing_event.payload
                    if existing_payload.get(identity_field) != identity:
                        continue
                    if _logical_event(existing_event) == expected:
                        return False
                    raise ProjectionIdentityConflict(
                        f"projection identity collision for {identity_field}"
                    )
        needs_separator = path.exists() and path.stat().st_size > 0
        if needs_separator:
            with path.open("rb") as raw_file:
                raw_file.seek(-1, 2)
                needs_separator = raw_file.read(1) not in {b"\n", b"\r"}
        with path.open("a", encoding="utf-8") as f:
            if needs_separator:
                # Preserve a torn tail as invalid history while ensuring this event
                # starts on its own parseable JSONL row.
                f.write("\n")
            f.write(event.model_dump_json() + "\n")
        return True

    def append_if_absent_sync(
        self, event: Event, *, identity_field: str, identity: str
    ) -> bool:
        """Single-owner startup form of :meth:`append_if_absent`."""

        return self._append_if_absent_unlocked(
            event, identity_field=identity_field, identity=identity
        )

    async def append_if_absent(
        self, event: Event, *, identity_field: str, identity: str
    ) -> bool:
        """Append one durable logical projection at most once per run log.

        This is a local JSONL idempotency boundary, not a subscriber-delivery
        acknowledgement. A live fan-out may still be observed more than once.
        """

        async with self._lock_for(event.run_id):
            return self._append_if_absent_unlocked(
                event, identity_field=identity_field, identity=identity
            )

    # EventBus sink signature
    async def sink(self, event: Event) -> None:
        await self.append(event)

    async def replay(self, run_id: str) -> AsyncIterator[Event]:
        path = self._path(run_id)
        if not path.exists():
            return
        with path.open("r", encoding="utf-8") as f:
            n = 0
            for raw in f:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    parsed = _object_row(json.loads(raw))
                    if parsed is None:
                        continue
                    event = Event.model_validate(parsed)
                except (json.JSONDecodeError, TypeError, ValueError):
                    # JSONL is append-only operational history: one torn, scalar, or
                    # schema-invalid row must not hide later valid events.
                    continue
                n += 1
                yield event
                if n % _REPLAY_YIELD_EVERY == 0:
                    # Historical SSE subscribers can replay tens of thousands
                    # of JSONL events. Cooperate with the uvicorn loop so
                    # unrelated API calls do not look globally frozen.
                    await asyncio.sleep(0)

    def last_stream_seq(self, run_id: str) -> int:
        """Return the monotonic SSE sequence after normalizing persisted history.

        Old runs can contain a sequence reset after a backend restart/reopen
        (for example 1808, then 1). Raw max(seq) is not enough in that case:
        the browser's Last-Event-ID is a stream cursor, so future buses must
        continue after the normalized cursor, not after the raw max.
        """
        path = self._path(run_id)
        if not path.exists():
            return 0
        seq = 0
        with path.open("r", encoding="utf-8") as f:
            for raw in f:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    ev = _object_row(json.loads(raw))
                except json.JSONDecodeError:
                    continue
                if ev is None:
                    continue
                try:
                    raw_seq = int(ev.get("seq") or 0)
                except (TypeError, ValueError):
                    raw_seq = 0
                seq = max(seq + 1, raw_seq)
        return seq

    async def replay_monotonic(
        self, run_id: str, *, after_seq: int = 0
    ) -> AsyncIterator[Event]:
        """Replay durable history with a strictly increasing stream seq.

        The event payload remains unchanged, but `event.seq` is rewritten for
        transport/reducer identity if a persisted segment reset its raw seq.
        This repairs existing corrupted JSONL without rewriting the file.
        """
        stream_seq = 0
        n = 0
        async for ev in self.replay(run_id):
            raw_seq = int(ev.seq or 0)
            stream_seq = max(stream_seq + 1, raw_seq)
            if stream_seq <= after_seq:
                continue
            n += 1
            if stream_seq != ev.seq:
                ev = ev.model_copy(update={"seq": stream_seq})
            yield ev
            if n % _REPLAY_YIELD_EVERY == 0:
                await asyncio.sleep(0)

    def list_runs(self) -> list[str]:
        return sorted(decode_run_id(p.stem) for p in self.root.glob("*.jsonl"))

    def load_all(self, run_id: str) -> list[dict]:
        """Sync convenience for tests / frontends: full event dicts for a run."""
        path = self._path(run_id)
        if not path.exists():
            return []
        out = []
        with path.open("r", encoding="utf-8") as f:
            for raw in f:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    parsed = _object_row(json.loads(raw))
                    if parsed is None:
                        continue
                    Event.model_validate(parsed)
                except (json.JSONDecodeError, TypeError, ValueError):
                    continue
                out.append(parsed)
        return out

    def iter_matching_events(
        self,
        run_id: str,
        *,
        event_types: tuple[str, ...] = (),
        payload_kinds: tuple[str, ...] = (),
    ) -> Iterator[Event]:
        """流式读取少量目标事件，避免为投影恢复反序列化完整 Run 历史。"""
        path = self._path(run_id)
        if not path.exists():
            return
        type_set = frozenset(event_types)
        kind_set = frozenset(payload_kinds)
        needles = tuple(
            json.dumps(value, ensure_ascii=False)
            for value in (*event_types, *payload_kinds)
        )
        with path.open("r", encoding="utf-8") as f:
            for raw in f:
                if needles and not any(needle in raw for needle in needles):
                    continue
                try:
                    parsed = _object_row(json.loads(raw))
                    if parsed is None:
                        continue
                    event = Event.model_validate(parsed)
                except (json.JSONDecodeError, TypeError, ValueError):
                    continue
                payload = event.payload or {}
                if (
                    event.event_type.value in type_set
                    or str(payload.get("kind") or "") in kind_set
                ):
                    yield event

    def summary(self, run_id: str, *, bounded: bool = False) -> dict:
        """Cheap one-run digest for the deck's thread rail (name/category/won/flag).

        Scans the persisted JSONL without reconstructing deck state — pulls the
        challenge identity from run.started and the verdict from run.finished /
        the FlagFound insight. Returns zeros for a run with no events yet.

        Multi-flag aware: carries flags(list)/expected_flags/multi_flag through so a
        rehydrated multi-flag run isn't flattened to a single-flag look-alike. `solved`
        is computed by MODE:
          - single-flag (or mode unknown): a FlagFound is enough to mark solved — this
            keeps the "ghost run" fallback (FlagFound but no RUN_FINISHED → still
            shows solved after restart);
          - multi-flag PARTIAL (collected < expected): a FlagFound does NOT mark solved
            (one of three flags is not a win).
        run.finished's explicit `solved` always wins (it knows the real verdict).
        """
        path = self._path(run_id)
        summary = {
            "run_id": run_id, "name": run_id, "category": "", "mode": "ctf",
            "started": False, "finished": False, "solved": False, "flag": None,
            "flags": [], "expected_flags": 1, "multi_flag": False,
            "events": 0, "ts": 0.0, "execution_generation": 0,
            "stream_seq": 0,
            "terminal_generations": [],
        }
        if not path.exists():
            return summary

        flags: list[str] = []  # de-duped, order-preserved collected flags
        invalidated: set[str] = set()

        def _add_flag(val) -> None:
            for f in (val if isinstance(val, list) else [val]):
                if f is not None and f not in invalidated and f not in flags:
                    flags.append(f)

        def _valid_flags(val) -> list[str]:
            return [
                f for f in (val if isinstance(val, list) else [val])
                if f is not None and f not in invalidated
            ]

        def _invalidate_flag(val) -> None:
            if val is not None:
                bad = str(val)
                invalidated.add(bad)
                flags[:] = [f for f in flags if f != bad]
            else:
                invalidated.update(flags)
                flags.clear()

        finished_solved: bool | None = None  # explicit verdict from run.finished
        flag_implies_solved = False
        terminal_generations: set[int] = set()
        pending_followups: dict[str, dict[str, Any]] = {}

        def _rows() -> Iterator[bytes]:
            if not bounded:
                with path.open("rb") as source:
                    yield from source
                return
            # A legacy install has no summary index yet. Startup only needs the
            # initial challenge identity and the latest terminal state, which are
            # written at the beginning and end of the append-only log. Bound this
            # one-time migration independently of total history size.
            size = path.stat().st_size
            head_size = min(size, 128 * 1024)
            tail_start = max(head_size, size - 512 * 1024)
            with path.open("rb") as source:
                head = source.read(head_size)
                for row in head.splitlines(
                        keepends=True)[:-1 if head_size < size else None]:
                    yield row
                if tail_start < size:
                    source.seek(tail_start)
                    tail = source.read()
                    if tail_start > head_size:
                        split = tail.find(b"\n")
                        tail = tail[split + 1:] if split >= 0 else b""
                    yield from tail.splitlines(keepends=True)

        for raw in _rows():
            raw = raw.strip()
            if not raw:
                continue
            raw_seq = int(_json_number_field(raw, b"seq"))
            summary["stream_seq"] = max(summary["stream_seq"] + 1, raw_seq)
            summary["events"] += 1
            summary["ts"] = _json_number_field(raw, b"ts") or summary["ts"]
            # Tool output and token deltas dominate large logs. None of those
            # rows can change the rail or startup recovery projection, so avoid
            # feeding their often multi-megabyte payloads through json.loads.
            if not any(needle in raw for needle in _SUMMARY_EVENT_NEEDLES):
                continue
            try:
                ev = _object_row(json.loads(raw))
            except json.JSONDecodeError:
                continue
            if ev is None:
                continue
            et = ev.get("event_type")
            p = _payload_object(ev)
            try:
                generation = int(p.get("execution_generation") or 0)
            except (TypeError, ValueError):
                generation = 0
            summary["execution_generation"] = max(
                summary["execution_generation"], generation)
            if et in {"run.preparing", "run.started"}:
                summary["started"] = True
                ch = p.get("challenge") or {}
                summary["name"] = ch.get("name") or summary["name"]
                summary["category"] = ch.get("category") or summary["category"]
                if ch.get("mode") == "pentest":
                    summary["mode"] = "pentest"
                if ch.get("expected_flags"):
                    summary["expected_flags"] = int(ch["expected_flags"])
                if "multi_flag" in ch:
                    summary["multi_flag"] = bool(ch["multi_flag"])
            elif et == "run.titled":
                # ChatGPT-style auto-title persisted on the run — survives restart
                summary["name"] = p.get("title") or summary["name"]
            elif et == "run.finished":
                summary["finished"] = True
                if generation > 0:
                    terminal_generations.add(generation)
                incoming_flags = (
                    p.get("flags") if "flags" in p else p.get("flag")
                )
                incoming_values = (
                    incoming_flags if isinstance(incoming_flags, list)
                    else [incoming_flags]
                )
                had_flag_payload = any(
                    value is not None for value in incoming_values)
                valid_incoming = _valid_flags(incoming_flags)
                if p.get("solved"):
                    finished_solved = (
                        bool(valid_incoming) if had_flag_payload else True)
                else:
                    finished_solved = False
                _add_flag(incoming_flags)
                # run.finished may carry the authoritative mode (the single-solver
                # _emit_finished does not — default fallbacks above cover that).
                if p.get("expected_flags"):
                    summary["expected_flags"] = int(p["expected_flags"])
                if "multi_flag" in p:
                    summary["multi_flag"] = bool(p["multi_flag"])
            elif et == "run.reopened":
                summary["finished"] = False
                finished_solved = False
                if p.get("reason") == "resolve":
                    continue
                _invalidate_flag(p.get("flag"))
            elif et == "insight.event" and p.get("kind") == "FlagFound":
                _add_flag(p.get("flag"))
                flag_implies_solved = True
            elif et == "flag.accepted":
                # A verified accepted handoff is public evidence of that flag,
                # but not proof of solved, finished, progress, or clean closure.
                _add_flag(p.get("flag"))
            elif et == "blackboard.delta":
                kind = p.get("kind")
                if kind == "flag_invalidated":
                    _invalidate_flag(p.get("flag"))
                    finished_solved = False
                elif kind == "flag_found":
                    _add_flag(p.get("flag"))
                    if not p.get("authority_receipt_digest"):
                        flag_implies_solved = True
            elif et == "followup.started":
                followup_id = str(p.get("followup_id") or "")
                key = followup_id or f"legacy:{summary['events']}"
                pending_followups[key] = {
                    "followup_id": followup_id,
                    "kind": str(p.get("kind") or "ask"),
                    "execution_generation": p.get("execution_generation"),
                    "recovery_id": (
                        f"interrupted-followup:{summary['events']}:{followup_id}"
                    ),
                }
            elif et in {"followup.completed", "followup.failed"}:
                followup_id = str(p.get("followup_id") or "")
                if followup_id:
                    pending_followups.pop(followup_id, None)
                else:
                    legacy_key = next(
                        (key for key in reversed(pending_followups)
                         if not pending_followups[key]["followup_id"]),
                        None,
                    )
                    if legacy_key is not None:
                        pending_followups.pop(legacy_key, None)

        if bounded:
            # A file byte offset is a stable upper bound for all historic event
            # sequence values. New events may safely continue above it; replay of
            # the immutable JSONL remains independent of this startup cursor.
            stat_result = path.stat()
            summary["stream_seq"] = max(
                int(summary["stream_seq"]), int(stat_result.st_size))
            summary["ts"] = max(
                float(summary["ts"]), float(stat_result.st_mtime))

        summary["flags"] = flags
        summary["flag"] = flags[0] if flags else None
        summary["terminal_generations"] = sorted(terminal_generations)
        summary["pending_followups"] = pending_followups

        # ── verdict, by mode ────────────────────────────────────────────────
        if finished_solved is not None:
            summary["solved"] = finished_solved  # explicit verdict wins
        elif flags and flag_implies_solved:
            # no RUN_FINISHED on disk (ghost run) but legacy flag publications were
            # found. Authority-recovery projections deliberately do not infer solved.
            if summary["multi_flag"]:
                summary["solved"] = len(flags) >= summary["expected_flags"]
            else:
                summary["solved"] = True
        return summary

    def summaries(self) -> list[dict]:
        """All persisted runs, newest-activity first — feeds the rail's Recent."""
        out = [self.summary(rid) for rid in self.list_runs()]
        out.sort(key=lambda s: s.get("ts", 0.0), reverse=True)
        return out
