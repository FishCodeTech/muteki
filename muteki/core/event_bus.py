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


class EventPersistenceError(RuntimeError):
    """The authoritative event sink did not confirm publication."""

    code = "event_persistence_failed"


class EventBus:
    def __init__(self, *, ring_size: int = 4096) -> None:
        self._seq = 0
        self._lock = asyncio.Lock()
        self._subscribers: set[asyncio.Queue[Event]] = set()
        self._ring: deque[Event] = deque(maxlen=ring_size)
        self._sinks: list[Callable[[Event], Awaitable[None]]] = []
        self._required_sink: Optional[Callable[[Event], Awaitable[None]]] = None
        self._filters: list[Callable[[Event], Awaitable[bool]]] = []
        self._closed = False
        self._persistence_failure: Optional[EventPersistenceError] = None

    # -- producer side -----------------------------------------------------
    async def _publish(self, event: Event) -> Event:
        async with self._lock:
            if self._persistence_failure is not None:
                raise self._persistence_failure
            for event_filter in list(self._filters):
                try:
                    if not await event_filter(event):
                        return event
                except Exception:
                    # A lifecycle filter is an ownership boundary. An exception
                    # cannot grant publication that the filter failed to approve.
                    return event
            next_seq = self._seq + 1
            object.__setattr__(event, "seq", next_seq)
            subs = list(self._subscribers)
            sinks = list(self._sinks)
            required = self._required_sink
            if required is not None:
                try:
                    await required(event)
                except Exception as exc:
                    # The event remains unpublished: no ring entry, subscriber
                    # delivery, or optional projection may claim it succeeded.
                    failure = EventPersistenceError(
                        f"required event sink failed: {type(exc).__name__}: {exc}"
                    )
                    self._persistence_failure = failure
                    raise failure from exc
            self._seq = next_seq
            self._ring.append(event)
            # Observers are projections, not authorities. Keep their failures
            # isolated after the required JSONL append has succeeded.
            for sink in sinks:
                if sink == required:
                    continue
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

    async def replay_window(
        self, last_event_id: int
    ) -> Optional[tuple[list[Event], int]]:
        async with self._lock:
            if last_event_id < 0 or last_event_id > self._seq:
                return None
            if last_event_id == self._seq:
                return [], self._seq
            if not self._ring or last_event_id < self._ring[0].seq - 1:
                return None
            return [event for event in self._ring if event.seq > last_event_id], self._seq

    # -- durable sinks (SessionStore plugs in here) ------------------------
    def add_sink(
        self, sink: Callable[[Event], Awaitable[None]], *, required: bool = False,
    ) -> None:
        if required:
            if self._required_sink is not None and self._required_sink != sink:
                raise ValueError("EventBus supports one authoritative event sink")
            self._required_sink = sink
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
            if self._required_sink == sink:
                self._required_sink = None
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
