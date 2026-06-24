"""Sprint 0.2 acceptance: bus fan-out ordering, Last-Event-ID resume, JSONL replay."""

import asyncio
from pathlib import Path

import pytest

from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.core.session_store import SessionStore


def _ev(i: int) -> Event:
    return Event(event_type=EventType.TEXT_MESSAGE_DELTA, run_id="run-1", payload={"i": i})


async def test_two_concurrent_subscribers_receive_100_in_order() -> None:
    bus = EventBus()
    received_a: list[int] = []
    received_b: list[int] = []
    ready = asyncio.Event()

    async def consume(sink: list[int]) -> None:
        n = 0
        async for e in bus.subscribe():
            sink.append(e.seq)
            n += 1
            if n == 1:
                ready.set()  # signal first event seen
            if n == 100:
                return

    ta = asyncio.create_task(consume(received_a))
    tb = asyncio.create_task(consume(received_b))
    # give subscribers a tick to register
    await asyncio.sleep(0.02)

    for i in range(100):
        await bus.emit(_ev(i))

    await asyncio.wait_for(asyncio.gather(ta, tb), timeout=5)

    assert received_a == list(range(1, 101))  # strictly increasing seq
    assert received_b == list(range(1, 101))
    assert received_a == received_b  # both saw identical ordering


async def test_seq_is_monotonic_under_concurrent_emit() -> None:
    bus = EventBus()
    seen: list[int] = []

    async def consume() -> None:
        async for e in bus.subscribe():
            seen.append(e.seq)
            if len(seen) == 50:
                return

    t = asyncio.create_task(consume())
    await asyncio.sleep(0.02)
    # fire emits concurrently
    await asyncio.gather(*[bus.emit(_ev(i)) for i in range(50)])
    await asyncio.wait_for(t, timeout=5)
    assert seen == sorted(seen)
    assert seen == list(range(1, 51))


async def test_slow_sink_does_not_reorder_realtime_fanout() -> None:
    """Finding A regression: a slow sink must NOT let a later emit's fan-out overtake
    an earlier one. The old emit() ran sink + fan-out OUTSIDE the seq lock, so emit-1
    yielding at `await sink` let emit-2's `q.put` land first → a live subscriber saw
    seq=2 before seq=1 (probe form [(2,2),(1,1)]). With the whole publish serialized
    the subscriber must see strictly [1,2,...]."""
    bus = EventBus()
    order: list[int] = []

    # The sink's delay DECREASES with seq, so a later-seq event's sink finishes
    # FIRST. Under the old out-of-lock emit() this guaranteed the later event's
    # fan-out (`q.put`) overtook the earlier one's → reorder. With the publish
    # serialized under the lock, emit-2 can't even start its sink until emit-1 has
    # fully fanned out, so the subscriber still sees strict seq order.
    async def slow_sink(ev: Event) -> None:
        await asyncio.sleep(max(0.0, 0.05 - ev.seq * 0.008))
        order.append(ev.seq)
    bus.add_sink(slow_sink)

    seen: list[int] = []

    async def consume() -> None:
        async for e in bus.subscribe():
            seen.append(e.seq)
            if len(seen) == 5:
                return

    t = asyncio.create_task(consume())
    await asyncio.sleep(0.02)  # let the subscriber register
    # fire 5 emits concurrently — the inverted-delay sink maximizes the reorder window
    await asyncio.gather(*[bus.emit(_ev(i)) for i in range(5)])
    await asyncio.wait_for(t, timeout=5)

    assert seen == [1, 2, 3, 4, 5], f"subscriber saw out-of-order seq: {seen}"
    assert order == [1, 2, 3, 4, 5], f"sink ran out of seq order: {order}"


async def test_sink_exception_does_not_block_fanout() -> None:
    """Finding A: a sink that raises must be isolated — the fan-out (and the durable
    sink registered before/after it) must still run, never wedging the stream."""
    bus = EventBus()

    async def boom(_ev: Event) -> None:
        raise RuntimeError("sink blew up")

    delivered: list[int] = []

    async def ok(ev: Event) -> None:
        delivered.append(ev.seq)

    bus.add_sink(boom)
    bus.add_sink(ok)

    seen: list[int] = []

    async def consume() -> None:
        async for e in bus.subscribe():
            seen.append(e.seq)
            if len(seen) == 3:
                return

    t = asyncio.create_task(consume())
    await asyncio.sleep(0.02)
    for i in range(3):
        await bus.emit(_ev(i))
    await asyncio.wait_for(t, timeout=5)

    assert seen == [1, 2, 3], "fan-out must survive a raising sink"
    assert delivered == [1, 2, 3], "a healthy sink after a raising one must still run"


async def test_last_event_id_resume_replays_backlog() -> None:
    bus = EventBus()
    # emit 10 with no subscriber; they go into the ring
    for i in range(10):
        await bus.emit(_ev(i))

    got: list[int] = []

    async def consume_from(seq: int) -> None:
        async for e in bus.subscribe(last_event_id=seq):
            got.append(e.seq)
            if e.seq == 10:
                return

    t = asyncio.create_task(consume_from(5))
    await asyncio.wait_for(t, timeout=5)
    # should replay 6..10 (everything after seq 5)
    assert got == [6, 7, 8, 9, 10]


async def test_session_store_replay_reproduces(tmp_path: Path) -> None:
    bus = EventBus()
    store = SessionStore(root=tmp_path)
    bus.add_sink(store.sink)

    for i in range(100):
        await bus.emit(_ev(i))

    replayed = [e async for e in store.replay("run-1")]
    assert len(replayed) == 100
    assert [e.seq for e in replayed] == list(range(1, 101))
    assert [e.payload["i"] for e in replayed] == list(range(100))


async def test_session_store_replay_yields_to_loop(tmp_path: Path) -> None:
    bus = EventBus()
    store = SessionStore(root=tmp_path)
    bus.add_sink(store.sink)

    for i in range(250):
        await bus.emit(_ev(i))

    ticks = 0
    done = False

    async def ticker() -> None:
        nonlocal ticks
        while not done:
            await asyncio.sleep(0)
            ticks += 1

    task = asyncio.create_task(ticker())
    try:
        replayed = [e async for e in store.replay("run-1")]
        done = True
        await task
    finally:
        done = True
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)

    assert len(replayed) == 250
    assert ticks > 0


async def test_session_store_partitions_by_run(tmp_path: Path) -> None:
    bus = EventBus()
    store = SessionStore(root=tmp_path)
    bus.add_sink(store.sink)
    await bus.emit(Event(event_type=EventType.RUN_STARTED, run_id="A"))
    await bus.emit(Event(event_type=EventType.RUN_STARTED, run_id="B"))
    await bus.emit(Event(event_type=EventType.RUN_FINISHED, run_id="A"))
    assert set(store.list_runs()) == {"A", "B"}
    assert len(store.load_all("A")) == 2
    assert len(store.load_all("B")) == 1


async def test_close_ends_subscribers() -> None:
    bus = EventBus()
    done = asyncio.Event()

    async def consume() -> None:
        async for _ in bus.subscribe():
            pass
        done.set()  # iteration ended cleanly

    t = asyncio.create_task(consume())
    await asyncio.sleep(0.02)
    await bus.close()
    await asyncio.wait_for(done.wait(), timeout=5)
    await t


async def test_subscribe_after_close_returns_without_blocking() -> None:
    """Regression: a stream that attaches to an ALREADY-CLOSED bus must replay its
    backlog then end — NOT block forever on the next event. This is the "继续做题"
    bug: the deck opens an SSE on a finished run (bus already closed); without this,
    the live tail hangs, the HTTP stream never closes, the browser never reconnects,
    and the operator must hard-refresh to see the relaunched swarm."""
    bus = EventBus()
    await bus.emit(_ev(1))
    await bus.emit(_ev(2))
    await bus.close()  # close BEFORE anyone subscribes

    got: list[int] = []

    async def consume() -> None:
        # last_event_id=0 → replay the 2 buffered events, then it must RETURN
        # (not await a sentinel that already fired before we subscribed).
        async for ev in bus.subscribe(last_event_id=0):
            got.append(ev.payload["i"])

    # If subscribe() blocked, wait_for would TimeoutError.
    await asyncio.wait_for(consume(), timeout=5)
    assert got == [1, 2]


async def test_remove_sink_detaches(tmp_path: Path) -> None:
    """L3: a sink can be detached so a reused bus doesn't accumulate stale sinks
    (the coordinator's _help_sink / _submit_gate_sink on a standby/resolve restart)."""
    bus = EventBus()
    seen: list[int] = []

    async def sink(ev: Event) -> None:
        seen.append(ev.payload["i"])

    bus.add_sink(sink)
    await bus.emit(_ev(1))
    assert bus.remove_sink(sink) is True       # detached
    await bus.emit(_ev(2))
    assert seen == [1], "a removed sink must not receive later events"
    assert bus.remove_sink(sink) is False      # idempotent: already gone
