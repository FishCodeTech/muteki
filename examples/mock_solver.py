"""Mock solver — no real model. Scripts a full solve event stream to prove the
foundation closes the loop (Sprint 0.4): reasoning -> tool -> terminal ->
solve_graph -> cost -> flag_found, persisted to SessionStore and replayable.

Run:  uv run python -m examples.mock_solver
"""

from __future__ import annotations

import asyncio
from pathlib import Path

from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.events import (
    Event,
    EventType,
    blackboard_delta_payload,
    context_state_payload,
    insight_payload,
    reason_intent_payload,
    shared_graph_delta_payload,
    solve_graph_delta_payload,
    tool_result_payload,
    worker_status_payload,
)
from muteki.core.session_store import SessionStore
from muteki.models.solve_graph import Challenge, HypothesisStatus, SolveGraph

# small inter-event pacing so the evolving graph + chat actually animate in the
# UI (and so HITL commands have a window to land mid-run). 0 in headless tests.
_TICK = 0.0


async def _pace() -> None:
    if _TICK:
        await asyncio.sleep(_TICK)


async def run_mock_solve(
    bus: EventBus, cost: CostController, run_id: str = "mock-c1",
    *, tick: float = 0.0, expected_flags: int = 1,
) -> SolveGraph:
    global _TICK
    _TICK = tick
    expected_flags = max(1, int(expected_flags))
    chal = Challenge(
        id=run_id,
        name="baby-encode",
        category="web",
        points=50,
        description="The flag is hidden behind layers of encoding at /secret",
        target="http://localhost:8000",
        expected_flags=expected_flags,
    )
    g = SolveGraph(challenge=chal)
    # two solvers so the graph + race show a real swarm, not a single lane. These
    # mirror the live naming (cli-<engine>) so the deck's engine badge resolves and
    # the roster exercises the same render path as a real run.
    primary, rival = "cli-claude", "cli-codex"

    async def emit(etype: EventType, solver: str = primary, **payload) -> None:
        await bus.emit(
            Event(
                event_type=etype,
                run_id=run_id,
                challenge_id=chal.id,
                solver_id=solver,
                payload=payload,
            )
        )
        await _pace()

    await emit(EventType.RUN_STARTED, challenge=chal.model_dump())

    # --- roster comes online: each worker reports presence + its CLI session id so
    #     the deck can show a manual-attach (`claude -r <id>`) affordance. ---
    await emit(EventType.WORKER_STATUS, solver=primary,
               **worker_status_payload(True, status="online", reason="started",
                                       engine="claude", session="a1b2c3d4-mock"))
    await emit(EventType.WORKER_STATUS, solver=rival,
               **worker_status_payload(True, status="online", reason="started",
                                       engine="codex", session="z9y8x7-mock"))
    await emit(EventType.WORKER_STATUS, solver="cli-cursor",
               **worker_status_payload(True, status="online", reason="started",
                                       engine="cursor", session="cur-mock"))

    # --- both solvers start reasoning (race) ---
    for chunk in ["I see a web target. ", "Likely an encoding chain. ", "Let me GET /secret."]:
        await emit(EventType.REASONING_DELTA, text=chunk)
    for chunk in ["Looking at this from auth angle. ", "I'll probe /admin and cookies."]:
        await emit(EventType.REASONING_DELTA, solver=rival, text=chunk)

    # --- tool call: http get (primary) ---
    await emit(EventType.TOOL_CALL_START, tool="web.http.get", call_id="t1")
    await emit(EventType.TOOL_CALL_ARGS, call_id="t1", args={"url": "http://localhost:8000/secret"})
    await emit(EventType.TERMINAL_OUTPUT, call_id="t1", text="$ GET /secret -> 200\n")
    await emit(
        EventType.TOOL_CALL_RESULT,
        **tool_result_payload(
            "web.http.get",
            {"status": 200, "body_b64": "ZmxhZ3tu...=="},
            artifact_id="mockaid1",
        ),
        call_id="t1",
    )
    g.add_evidence("web.http.get", "GET /secret returns base64-looking blob")
    await emit(
        EventType.SOLVE_GRAPH_DELTA,
        **solve_graph_delta_payload(
            "evidence_added", source="web.http.get", fact="GET /secret returns base64 blob"
        ),
    )

    # --- shared graph: a CANDIDATE (rival's unverified claim) then a VERIFIED
    # fact (primary's, witness located in the artifact) — shows the gate split. ---
    await emit(
        EventType.SHARED_GRAPH_DELTA, solver=rival,
        **shared_graph_delta_payload(
            "/admin requires a JWT (unconfirmed)", verified=False, confidence=0.4,
            actor=rival,
        ),
    )
    await emit(
        EventType.SHARED_GRAPH_DELTA,
        **shared_graph_delta_payload(
            "GET /secret body decodes as base64", verified=True, confidence=0.95,
            actor=primary, verifier="witness:artifact-grep", artifact_id="mockaid1",
        ),
    )

    # --- reason phase: typed intents + an evidence-audit note ---
    await emit(
        EventType.REASON_INTENT,
        **reason_intent_payload(
            goal_met=False,
            intents=[
                {"id": "i1", "goal": "decode the base64 chain at /secret", "worker_class": "code"},
                {"id": "i2", "goal": "verify /admin JWT claim before relying on it", "worker_class": "verifier"},
            ],
            audit=["/admin JWT claim is unverified — do not build on it yet"],
        ),
    )

    # --- shared knowledge blackboard: the collaboration lifecycle. Both intents
    # are proposed, then the two solvers CLAIM one each (the claim race the
    # blackboard canvas animates), a verified fact lands with full provenance,
    # then the claimed intents conclude. ---
    await emit(EventType.BLACKBOARD_DELTA, solver=primary,
               **blackboard_delta_payload("intent_proposed", actor=primary,
                   intent_id="i1", goal="decode the base64 chain at /secret",
                   worker_class="code"))
    await emit(EventType.BLACKBOARD_DELTA, solver=primary,
               **blackboard_delta_payload("intent_proposed", actor=primary,
                   intent_id="i2", goal="verify /admin JWT claim before relying on it",
                   worker_class="verifier"))
    await emit(EventType.BLACKBOARD_DELTA, solver=primary,
               **blackboard_delta_payload("intent_claimed", actor=primary,
                   intent_id="i1", worker=primary, goal="decode the base64 chain at /secret"))
    await emit(EventType.BLACKBOARD_DELTA, solver=rival,
               **blackboard_delta_payload("intent_claimed", actor=rival,
                   intent_id="i2", worker=rival, goal="verify /admin JWT claim before relying on it"))
    await emit(EventType.BLACKBOARD_DELTA, solver=primary,
               **blackboard_delta_payload("fact_added", actor=primary,
                   fact="GET /secret body decodes as base64", verified=True,
                   confidence=0.95, verifier="witness:artifact-grep",
                   witness="ZmxhZ3tu… (located in artifact)", artifact_id="mockaid1"))

    # --- coordinator spawns a SECOND claude worker mid-run. Same engine as the
    #     first → it gets a distinct id (cli-claude-2) so the deck shows it as its
    #     own lane with its own session id, not collapsed onto cli-claude. ---
    spawned = "cli-claude-2"
    await emit(EventType.WORKER_STATUS, solver=spawned,
               **worker_status_payload(True, status="online", reason="started",
                                       engine="claude", session="m3n4o5-mock"))
    await emit(EventType.REASONING_DELTA, solver=spawned,
               text="Fresh angle: fuzz the /export endpoint for the encoding key.")
    # this spawned worker ends WITHOUT a flag — a WORKER_FINISHED (worker-level), so
    # the deck must keep showing the run as RUNNING (the coordinator carries on). This
    # is exactly the run-7345 case: one worker ending must not finish the whole run.
    await emit(EventType.WORKER_FINISHED, solver=spawned, flag=None, solved=False)
    await emit(EventType.WORKER_STATUS, solver=spawned,
               **worker_status_payload(False, status="offline", reason="finished",
                                       engine="claude", session="m3n4o5-mock"))

    # --- a dead-end the rival hit (prunes a path; shows red node) ---
    await emit(
        EventType.INSIGHT_BUS_EVENT, solver=rival,
        **insight_payload("DeadEndMarked", text="SQLi on /login is patched", by=rival),
    )
    await emit(EventType.BLACKBOARD_DELTA, solver=rival,
               **blackboard_delta_payload("dead_end", actor=rival,
                   reason="SQLi on /login is patched"))
    await emit(EventType.BLACKBOARD_DELTA, solver=rival,
               **blackboard_delta_payload("intent_concluded", actor=rival,
                   intent_id="i2", worker=rival))

    # --- hypothesis ---
    h = g.add_hypothesis("multi-layer base64 encoding", "blob is base64 charset", priority=0.8)
    await emit(
        EventType.SOLVE_GRAPH_DELTA,
        **solve_graph_delta_payload("hypothesis_added", id=h.id, statement=h.statement),
    )

    # --- context fuel gauge ---
    await emit(
        EventType.CONTEXT_STATE,
        **context_state_payload(
            zones=[
                {"label": "system", "tokens": 1200},
                {"label": "history", "tokens": 3400},
                {"label": "tool_out", "tokens": 800},
            ],
            total=5400,
            limit=128000,
        ),
    )

    # --- decode tool (primary closes it out) ---
    await emit(EventType.TOOL_CALL_START, tool="misc.encoding.auto_decode", call_id="t2")
    await emit(EventType.TERMINAL_OUTPUT, call_id="t2", text="$ auto_decode -> base64 x2 -> flag\n")
    flag = "flag{mock_encoding_solved}"
    await emit(
        EventType.TOOL_CALL_RESULT,
        **tool_result_payload("misc.encoding.auto_decode", {"flag": flag}),
        call_id="t2",
    )
    g.set_status(h.id, HypothesisStatus.CONFIRMED)
    # (the flag(s) are recorded via g.add_flag in the flag-found block below)

    # --- cost (simulated multi-agent LLM usage) ---
    # the deepseek coordinator (token-priced) + the two CLI workers (claude reports
    # a dollar cost, codex is token-priced) so the deck's per-agent cost/token hover
    # card has a realistic breakdown to render.
    await cost.record(
        model="deepseek-v4-flash",
        input_tokens=2400, output_tokens=600,
        run_id=run_id, challenge_id=chal.id, solver_id="reason",
    )
    await cost.add_external_usd(
        0.1840, run_id=run_id, challenge_id=chal.id, solver_id=primary,
        input_tokens=128_400, output_tokens=3_200,
    )
    await cost.add_external_usd(
        0.0312, run_id=run_id, challenge_id=chal.id, solver_id=rival,
        input_tokens=41_900, output_tokens=820,
    )
    # a cursor worker too: subscription-backed (no $) but reports tokens, so it
    # appears in the token card at $0 — exercises the cursor row + zero-cost path.
    await cost.add_external_usd(
        0.0, run_id=run_id, challenge_id=chal.id, solver_id="cli-cursor",
        input_tokens=58_300, output_tokens=1_450,
    )

    # --- blackboard: primary's decode intent concludes, then the flag lands ---
    await emit(EventType.BLACKBOARD_DELTA, solver=primary,
               **blackboard_delta_payload("intent_concluded", actor=primary,
                   intent_id="i1", worker=primary))

    # --- flag(s) found (insight bus). multi-flag: emit expected_flags distinct
    # flags so the deck animates the "collecting N/total" → solved progression. ---
    flags = [flag] + [f"flag{{mock_part_{i}}}" for i in range(2, expected_flags + 1)]
    for i, fl in enumerate(flags):
        g.add_flag(fl)
        actor = primary if i == 0 else rival  # spread across workers
        await emit(EventType.INSIGHT_BUS_EVENT, **insight_payload("FlagFound", flag=fl, by=actor))
        await emit(EventType.BLACKBOARD_DELTA, solver=actor,
                   **blackboard_delta_payload("flag_found", actor=actor, flag=fl))
    cost.add_points(chal.points)
    await emit(EventType.RUN_FINISHED, flag=flags[0], flags=flags,
               expected_flags=expected_flags, solved=True, reason="goal_met")
    return g


async def main() -> None:
    store = SessionStore(root="sessions")
    bus = EventBus()
    bus.add_sink(store.sink)
    cost = CostController(bus=bus)

    g = await run_mock_solve(bus, cost)
    print(f"[mock] solved -> {g.flag}")
    print(f"[mock] cost snapshot: {cost.snapshot()}")

    # prove replay
    replayed = [e async for e in store.replay("mock-c1")]
    print(f"[mock] persisted {len(replayed)} events; replaying types:")
    for e in replayed:
        print(f"    seq={e.seq:>3}  {e.event_type.value}")


if __name__ == "__main__":
    asyncio.run(main())
