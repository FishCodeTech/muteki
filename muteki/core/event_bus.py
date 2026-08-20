"""Async event bus — the in-process spine the whole system emits onto.

Design constraints (from §3):
- One ordered, typed event stream. Producers `emit`; the bus assigns a
  monotonic `seq` so ordering is total even across concurrent producers.
- Multiple subscribers, each receives every event in order (fan-out).
- `Last-Event-ID` resume: a (re)connecting subscriber can ask for everything
  after a given seq. We keep a bounded in-memory ring for recent replay; the
  durable full history lives in SessionStore (JSONL).

The bus is transport-agnostic. SSE / WS / TUI / SessionStore all hang off it.
"""

from __future__ import annotations

import asyncio
from collections import deque
from typing import AsyncIterator, Awaitable, Callable, Optional

from muteki.core.events import Event


class EventBus:
    def __init__(self, *, ring_size: int = 4096) -> None:
        self._seq = 0
        self._lock = asyncio.Lock()
        self._subscribers: set[asyncio.Queue[Event]] = set()
        self._ring: deque[Event] = deque(maxlen=ring_size)
        self._sinks: list[Callable[[Event], Awaitable[None]]] = []
        self._filters: list[Callable[[Event], Awaitable[bool]]] = []
        self._closed = False

    # -- producer side -----------------------------------------------------
    async def _publish(self, event: Event) -> Event:
        async with self._lock:
            for event_filter in list(self._filters):
                try:
                    if not await event_filter(event):
                        return event
                except Exception:
                    # A lifecycle filter is an ownership boundary. An exception
                    # cannot grant publication that the filter failed to approve.
                    return event
            self._seq += 1
            object.__setattr__(event, "seq", self._seq)
            self._ring.append(event)
            subs = list(self._subscribers)
            sinks = list(self._sinks)
            # sinks (e.g. SessionStore JSONL) run before fan-out so a replay can
            # never observe an event that wasn't durably recorded. A sink raising
            # must NOT abort the publish (it would drop the fan-out and leave the
            # stream stuck), so each sink is isolated.
            for sink in sinks:
                try:
                    await sink(event)
                except Exception:
                    pass
            for q in subs:
                await q.put(event)
        return event

    async def emit(self, event: Event) -> Event:
        """Assign seq + ts, persist to sinks, fan out to all subscribers.

        Returns the same event with its seq filled in (handy for callers).

        Ordering: the WHOLE publish — assign seq → ring → sinks → fan-out — runs
        under one lock, so concurrent emits are serialized and a subscriber always
        receives events in strict seq order. The previous version assigned seq under
        the lock but ran the sink/fan-out loops OUTSIDE it: emit-A could yield at
        `await sink(event)` and let emit-B's `q.put` land first, so an online
        subscriber observed seq=2 before seq=1 (the real-time reorder bug). Holding
        the lock across the awaits costs a little producer concurrency but is the
        only way to keep one ordered stream without a background drain task (the bus
        is constructed in sync contexts with no running loop, so we can't start one).

        Publication is local cancellation-complete: after this caller is admitted to
        the bus lock, cancellation is deferred until every snapshotted local sink and
        subscriber queue has been processed, then re-raised. This prevents one local
        publication from splitting durable JSONL and metadata; it is not an
        exactly-once or distributed-delivery guarantee.
        """
        publish = asyncio.create_task(self._publish(event))
        try:
            return await asyncio.shield(publish)
        except asyncio.CancelledError:
            # ``shield`` keeps the publication owner alive. Wait through repeated
            # cancellation requests, then preserve caller cancellation only after
            # the local publication boundary has completed.
            while not publish.done():
                try:
                    await asyncio.shield(publish)
                except asyncio.CancelledError:
                    continue
            publish.result()
            raise

    @property
    def current_seq(self) -> int:
        return self._seq

    # -- durable sinks (SessionStore plugs in here) ------------------------
    def add_sink(self, sink: Callable[[Event], Awaitable[None]]) -> None:
        self._sinks.append(sink)

    def add_filter(self, event_filter: Callable[[Event], Awaitable[bool]]) -> None:
        """Register an admission filter that runs before sequence assignment.

        Filters may add local metadata to an event and return ``False`` to drop a
        stale or duplicate publication before it reaches the ring, durable sinks,
        or subscribers.
        """
        self._filters.append(event_filter)

    def remove_sink(self, sink: Callable[[Event], Awaitable[None]]) -> bool:
        """Detach a previously-added sink (L3). Returns True if it was present. The
        coordinator's _help_sink / _submit_gate_sink close over the whole Swarm; on a
        standby/resolve restart that re-enters the coordinator, re-adding them on a
        reused bus would leak a Swarm-closing sink per cycle with no way to detach it.
        Idempotent — removing an absent sink is a no-op."""
        try:
            self._sinks.remove(sink)
            return True
        except ValueError:
            return False

    # -- subscriber side ---------------------------------------------------
    async def subscribe(
        self, *, last_event_id: Optional[int] = None
    ) -> AsyncIterator[Event]:
        """Yield events as they arrive.

        If `last_event_id` is given, first replay any buffered events with a
        higher seq (reconnect continuity), then stream live ones. Replayed and
        live events never duplicate or reorder because we snapshot the ring and
        register the live queue under the same lock.
        """
        q: asyncio.Queue[Event] = asyncio.Queue()
        async with self._lock:
            backlog: list[Event] = []
            if last_event_id is not None:
                backlog = [e for e in self._ring if e.seq > last_event_id]
            # Subscribing to an ALREADY-CLOSED bus: close() only fans the sentinel
            # to subscribers present AT close time, so a stream that attaches LATER
            # (a deck opening the SSE on a finished run, then "继续做题" swaps in a
            # fresh bus via _fresh_bus) would block forever on `await q.get()` —
            # the live tail never ends, the HTTP stream never closes, the browser
            # EventSource never reconnects, so the operator must hard-refresh to
            # see the relaunched swarm. Mark closed-at-subscribe so we replay the
            # backlog then return cleanly, ending the stream → browser reconnects →
            # gen() re-runs and binds run.bus (now the NEW live bus).
            already_closed = self._closed
            if not already_closed:
                self._subscribers.add(q)
        try:
            for e in backlog:
                yield e
            if already_closed:
                return
            while True:
                e = await q.get()
                # None is the close sentinel
                if e is _CLOSE:  # type: ignore[comparison-overlap]
                    return
                yield e
        finally:
            async with self._lock:
                self._subscribers.discard(q)

    async def close(self) -> None:
        """Signal all live subscribers to stop iterating."""
        async with self._lock:
            self._closed = True
            subs = list(self._subscribers)
        for q in subs:
            await q.put(_CLOSE)  # type: ignore[arg-type]


# Sentinel pushed onto subscriber queues to end iteration cleanly.
_CLOSE: Event = object()  # type: ignore[assignment]
