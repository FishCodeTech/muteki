"""Flag candidate parsing and submission-boundary tests.

Stream text, reasoning, tool output and historical ``FOUND_FLAG`` markers may
provide evidence or candidates. They never change accepted Flag state on their
own. Protocol 1 acceptance requires a validated Blackboard ``submit-flag``
request; the end-to-end SQLite submission tests live in ``test_cli_executor``.
"""

from __future__ import annotations

import pytest

from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.models.solve_graph import Challenge
from muteki.solver.cli_driver import StreamStep
from muteki.solver.cli_solver import CliSolver


def _token_challenge() -> Challenge:
    return Challenge(
        id="run-test-stream",
        name="stream-flag",
        category="reverse",
        description="chained levels with token flags",
        flag_format="token",
        multi_flag=True,
        expected_flags=15,
    )


def _solver(bus: EventBus) -> CliSolver:
    return CliSolver(
        spec=None,
        challenge=_token_challenge(),
        engine="claude",
        bus=bus,
        run_id="run-test-stream",
        solver_label="cli-claude-test",
    )


async def _flag_events(bus: EventBus) -> list[str]:
    seen: list[str] = []

    async def _sink(ev: Event) -> None:
        if (
            ev.event_type == EventType.BLACKBOARD_DELTA
            and (ev.payload or {}).get("kind") == "flag_found"
        ):
            seen.append(str((ev.payload or {}).get("flag") or ""))

    bus.add_sink(_sink)
    return seen


@pytest.mark.asyncio
async def test_stream_text_never_accepts_without_submit_flag() -> None:
    bus = EventBus()
    seen = await _flag_events(bus)
    solver = _solver(bus)
    flag = "bl_62c1be2414c0143a2da6b5b0982e12e7"

    await solver._stream_markers(f"FOUND_FLAG={flag}\n")
    await solver._emit_step(
        StreamStep("reasoning", text=f"solved: FOUND_FLAG={flag}")
    )
    await solver._emit_step(
        StreamStep("tool_result", text=f"FOUND_FLAG={flag}\n")
    )

    assert seen == []
    assert solver._already_found == set()
    assert solver._stream_accepted == []
    assert list(solver.graph.flags) == []


def test_candidate_parser_keeps_multiple_and_space_bearing_flags() -> None:
    solver = _solver(EventBus())
    solver.challenge.flag_format = r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}"
    first = "flag{H1570rY 12 N7 k1ND 70 7h053 wh0 Pl4Y g0d}"
    second = "flag{S4f3_l1nk1nG_GL0B4L5}"

    candidates = solver._extract_flags(
        f"FOUND_FLAG={first}\nFOUND_FLAG={second}** trailing prose\n"
    )

    assert candidates == [first, second]


def test_candidate_provenance_rejects_local_state_scrapes() -> None:
    solver = _solver(EventBus())
    flag = "bl_a2a9f8e886a1ee05c9e6892dfd692526"

    assert solver._flag_ok(flag, f"target verifier printed {flag}\n") is True
    assert (
        solver._flag_ok(
            flag,
            f"/Users/x/.codex/sessions/rollout.jsonl:12:FOUND_FLAG={flag}\n",
        )
        is False
    )
    assert (
        solver._flag_ok(
            flag,
            f"sessions/run-11191.jsonl:99: FOUND_FLAG={flag}\n",
        )
        is False
    )


@pytest.mark.asyncio
async def test_bare_tool_output_is_candidate_only_until_submission() -> None:
    bus = EventBus()
    seen = await _flag_events(bus)
    solver = _solver(bus)
    solver.challenge.flag_format = r"flag\{[^}]+\}"
    flag = "flag{c86d9044ae62}"
    tool_output = (
        "====flag via LFI====\n"
        f'{{"content":"{flag}","filename":"/challenge/flag.txt"}}\n'
    )

    assert flag in solver._poc_flag_literals(tool_output)
    await solver._stream_markers(
        tool_output, allow_flags=True, flag_provenance=tool_output
    )

    assert seen == []
    assert solver._already_found == set()


@pytest.mark.asyncio
async def test_validated_submission_accepts_once() -> None:
    bus = EventBus()
    seen = await _flag_events(bus)
    solver = _solver(bus)
    flag = "bl_aa11bb22cc33dd44ee55ff6600778899"

    solver._validated_flag_submissions.add(flag)
    assert await solver._accept_flag(flag) is True
    assert await solver._accept_flag(flag) is False

    assert seen == [flag]
    assert solver._already_found == {flag}


def _graph_solver(bus: EventBus, tmp_path, *, shared_graph=None, label="cli-fresh"):
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    challenge = Challenge(
        id="run-75379",
        name="reject-respawn",
        category="web",
        flag_format=r"flag\{[^}]+\}",
        multi_flag=True,
        expected_flags=4,
    )
    graph = shared_graph or SQLiteSharedGraph.open(
        db_path=tmp_path / "g.db", challenge=challenge
    )
    solver = CliSolver(
        spec=None,
        challenge=challenge,
        engine="claude",
        bus=bus,
        run_id="run-75379",
        solver_label=label,
        shared_graph=graph,
    )
    return solver, graph


@pytest.mark.asyncio
async def test_invalidated_flag_is_refused_after_worker_respawn(tmp_path) -> None:
    bus = EventBus()
    seen = await _flag_events(bus)
    first, graph = _graph_solver(bus, tmp_path, label="cli-w1")
    first._validated_flag_submissions.add("flag{a}")
    assert await first._accept_flag("flag{a}") is True

    graph.reopen_after_false_positive(actor="operator", flag="flag{a}")
    seen.clear()
    fresh, _ = _graph_solver(
        bus, tmp_path, shared_graph=graph, label="cli-fresh"
    )
    fresh._validated_flag_submissions.update({"flag{a}", "flag{b}"})

    assert await fresh._accept_flag("flag{a}") is False
    assert await fresh._accept_flag("flag{b}") is True
    assert seen == ["flag{b}"]


def test_rejected_flag_is_rendered_in_worker_prompt(tmp_path) -> None:
    bus = EventBus()
    first, graph = _graph_solver(bus, tmp_path, label="cli-w1")
    graph.flag_found(actor=first.solver_id, flag="flag{a}")
    graph.reopen_after_false_positive(actor="operator", flag="flag{a}")

    fresh, _ = _graph_solver(
        bus, tmp_path, shared_graph=graph, label="cli-fresh"
    )
    block = fresh._rejected_flags_block()

    assert "flag{a}" in block
    assert "FALSE POSITIVE" in block.upper()
