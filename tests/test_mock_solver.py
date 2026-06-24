"""Sprint 0.4 acceptance: mock solver emits a full stream, persists, replays."""

from pathlib import Path

import pytest

from examples.mock_solver import run_mock_solve
from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.events import EventType
from muteki.core.session_store import SessionStore


async def test_mock_solve_emits_full_stream_and_replays(tmp_path: Path) -> None:
    store = SessionStore(root=tmp_path)
    bus = EventBus()
    bus.add_sink(store.sink)
    cost = CostController(bus=bus)

    g = await run_mock_solve(bus, cost, run_id="t-c1")
    assert g.flag == "flag{mock_encoding_solved}"

    replayed = [e async for e in store.replay("t-c1")]
    types = {e.event_type for e in replayed}
    # the full pipeline must be represented
    for required in (
        EventType.RUN_STARTED,
        EventType.REASONING_DELTA,
        EventType.TOOL_CALL_START,
        EventType.TOOL_CALL_RESULT,
        EventType.TERMINAL_OUTPUT,
        EventType.SOLVE_GRAPH_DELTA,
        EventType.CONTEXT_STATE,
        EventType.COST_UPDATE,
        EventType.INSIGHT_BUS_EVENT,
        EventType.RUN_FINISHED,
    ):
        assert required in types, f"missing {required}"

    # ordering: run_started first, run_finished last, seq strictly increasing
    assert replayed[0].event_type is EventType.RUN_STARTED
    assert replayed[-1].event_type is EventType.RUN_FINISHED
    seqs = [e.seq for e in replayed]
    assert seqs == sorted(seqs)

    # flag found event carries the flag (the mock also emits a DeadEndMarked
    # insight, so select the FlagFound one specifically rather than the first).
    ff = [
        e for e in replayed
        if e.event_type is EventType.INSIGHT_BUS_EVENT and e.payload.get("kind") == "FlagFound"
    ][0]
    assert ff.payload["flag"] == "flag{mock_encoding_solved}"

    # cost recorded + points awarded -> north star positive
    assert cost.global_usd() > 0
    assert cost.snapshot()["points"] == 50


async def test_mock_solve_multiflag_emits_n_flags(tmp_path: Path) -> None:
    store = SessionStore(root=tmp_path)
    bus = EventBus()
    bus.add_sink(store.sink)
    cost = CostController(bus=bus)

    g = await run_mock_solve(bus, cost, run_id="t-mf", expected_flags=2)
    assert g.flags == ["flag{mock_encoding_solved}", "flag{mock_part_2}"]
    assert g.challenge.expected_flags == 2

    replayed = [e async for e in store.replay("t-mf")]
    ffs = [e for e in replayed
           if e.event_type is EventType.INSIGHT_BUS_EVENT
           and e.payload.get("kind") == "FlagFound"]
    assert {e.payload["flag"] for e in ffs} == {
        "flag{mock_encoding_solved}", "flag{mock_part_2}"}
    # RUN_FINISHED carries the full set + expected_flags
    rf = [e for e in replayed if e.event_type is EventType.RUN_FINISHED][-1]
    assert rf.payload["flags"] == ["flag{mock_encoding_solved}", "flag{mock_part_2}"]
    assert rf.payload["expected_flags"] == 2 and rf.payload["solved"] is True
