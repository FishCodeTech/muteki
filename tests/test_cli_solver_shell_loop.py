"""Tests for the f10 edge-cognition worker shell loop in CliSolver.

The CLI runtime (_run_invocation) is mocked — no real CLI, no LLM, no API.
"""

from __future__ import annotations

import asyncio

from muteki.models.solve_graph import Challenge
from muteki.solver.cli_driver import CliResult
from muteki.solver.cli_solver import CliSolver
from muteki.frameworks.f10_edge_cognition.state import (
    load_state,
    new_state,
    render_envelope_guidance,
    save_state,
)


def _challenge() -> Challenge:
    return Challenge(
        id="shell-ch",
        name="shell-ch",
        category="web",
        description="a web challenge",
        flag_format="flag{...}",
    )


def _envelope(**over) -> dict:
    env = {
        "intent_id": "I-shell-1",
        "shell_id": "shell-test01",
        "goal": "fuzz the upload parser for SSTI",
        "goal_id": "sha256:test",
        "predicted_effects": [
            {"selector": "upload.accepted_types", "op": "contains",
             "value": "svg"}
        ],
        "success_criteria": ["verified fact: upload returns 200"],
        "budget": {"token_limit": 120000, "turn_limit": 10},
        "profile": {"plan_every": 3, "stuck_limit": 3},
    }
    env.update(over)
    return env


class _Graph:
    """Append-only event-log stub for worker-side checkpoint emission."""

    def __init__(self):
        self._conn = None
        self._lock = None
        self.events = []

    def _append(self, kind, actor, payload, dedupe_key=None):
        self.events.append(
            {"seq": len(self.events) + 1, "kind": kind, "actor": actor,
             "payload": payload})
        return len(self.events)

    def __getattr__(self, name):
        # anything beyond the append-only event log is absent; callers guard
        # with getattr(..., None) or try/except, so AttributeError is correct.
        raise AttributeError(name)


def _make_solver(tmp_path, *, envelope=None, mode="explore", graph=None,
                 standing=None, workdir=None) -> CliSolver:
    return CliSolver(
        None,
        _challenge(),
        engine="claude",
        mode=mode,
        intent_goal="fuzz the upload parser for SSTI",
        intent_id="I-shell-1",
        workdir=workdir or str(tmp_path / "ws"),
        shell_envelope=envelope,
        standing_guidance=standing,
        shared_graph=graph,
        timeout=60,
    )


def _script(solver: CliSolver, results: list[CliResult]) -> list[str]:
    """Mock the CLI runtime: pop scripted CliResults, capture every prompt."""
    prompts: list[str] = []
    queue = list(results)

    async def fake(argv, *, cwd, timeout, stdin_text=None):
        prompts.append(str(argv[-1] if argv else ""))
        if queue:
            result = queue.pop(0)
            # These tests exercise shell checkpoint and turn sequencing. Model a
            # Blackboard submission decision for canned flags so they do not rely
            # on the removed text-to-acceptance path.
            solver._validated_flag_submissions.update(
                solver._extract_flags(result.text)
            )
            return result
        return CliResult(text="REFLECT=unchanged: nothing new")

    solver._run_invocation = fake  # type: ignore[method-assign]
    return prompts


def _run(solver: CliSolver):
    return asyncio.run(solver.run())


# ── activation (开关隔离) ─────────────────────────────────────────────────────


def test_shell_loop_off_by_default(tmp_path):
    solver = CliSolver(None, _challenge(), engine="claude", mode="explore",
                       workdir=str(tmp_path / "ws"))
    assert solver._shell_loop is False
    assert solver._shell_envelope is None


def test_shell_loop_on_via_kwarg(tmp_path):
    solver = _make_solver(tmp_path, envelope=_envelope())
    assert solver._shell_loop is True
    assert solver._shell_envelope["intent_id"] == "I-shell-1"


def test_shell_loop_on_via_guidance_marker(tmp_path):
    marker = render_envelope_guidance(_envelope())
    solver = _make_solver(tmp_path, envelope=None, standing=[marker])
    assert solver._shell_loop is True
    assert solver._shell_envelope["shell_id"] == "shell-test01"


def test_shell_loop_never_captures_review_or_respond(tmp_path):
    for mode in ("review", "respond"):
        solver = _make_solver(tmp_path, envelope=_envelope(), mode=mode)
        assert solver._shell_loop is False


# ── the cognitive loop ────────────────────────────────────────────────────────


def test_shell_loop_multi_turn_then_solved(tmp_path):
    graph = _Graph()
    solver = _make_solver(tmp_path, envelope=_envelope(), graph=graph)
    solver._flag_ok = lambda flag, prov: True  # the gate itself is tested elsewhere
    prompts = _script(solver, [
        # turn 1 (planning): declares a 2-step plan + a real fact
        CliResult(
            text="curl shows the upload form at /upload returning HTTP 200\n"
                 "PLAN_STEP=1|probe svg upload|server returns 200\n"
                 "PLAN_STEP=2|send jinja payload|output contains 49\n"
                 "VERIFIED_FACT=upload form at /upload returns 200\n"
                 "REFLECT=changed: found upload endpoint",
            session="sess-1", input_tokens=100, output_tokens=50),
        # turn 2 (execute): barren
        CliResult(text="REFLECT=unchanged: probe inconclusive",
                  session="sess-1", input_tokens=100, output_tokens=50),
        # turn 3 (execute): new direction declared + flag recovered
        CliResult(text="response body shows 49 for the jinja probe\n"
                       "VERIFIED_FACT=SSTI confirmed: jinja probe rendered 49\n"
                       "SUB_INTENT=escalate SSTI to RCE via template globals\n"
                       "FOUND_FLAG=flag{ssti_edge_win}\n"
                       "REFLECT=changed: ssti confirmed",
                  session="sess-1", input_tokens=100, output_tokens=50),
    ])
    outcome = _run(solver)
    assert outcome.solved is True
    assert outcome.flag == "flag{ssti_edge_win}"
    # 3 loop turns, no conclude fallback (solved inside the loop)
    assert len(prompts) == 3
    assert outcome.steps == 3
    # local WorkerShellState persisted with REAL turn/token counts
    st = load_state(tmp_path / "ws")
    assert st is not None
    assert st["budget"]["turns_used"] == 3
    assert st["budget"]["tokens_used"] == 450  # 3 × (100+50), local sum
    assert st["schema"] == "muteki.edge.shell.v1"
    assert st["shell_id"] == "shell-test01"
    # the plan declared on turn 1 was consumed across turns 2-3
    assert st["plan_queue"] == []
    # confirmed facts accumulated into working memory
    wm = st["working_memory"]
    assert any("upload" in f for f in wm["confirmed_subfacts"])
    # per-turn tool-output digest recorded (design §2.1)
    assert wm["last_tool_output_digest"].startswith("sha256:")
    # checkpoints + sub-intent were appended to the event bus for the arbiter
    kinds = [e["kind"] for e in graph.events]
    assert kinds.count("edge_shell_checkpoint") == 3
    assert "edge_sub_intent" in kinds
    sub = next(e for e in graph.events if e["kind"] == "edge_sub_intent")
    assert sub["payload"]["goal"].startswith("escalate SSTI")
    assert sub["payload"]["parent_intent_id"] == "I-shell-1"
    # checkpoint budget_signal carries real cumulative counts
    cps = [e for e in graph.events if e["kind"] == "edge_shell_checkpoint"]
    assert cps[-1]["payload"]["budget_signal"]["turns_used"] == 3
    assert cps[-1]["payload"]["checkpoint"]["turn"] == 3
    # turn-2 prompt was an EXECUTE turn carrying the planned step + verifier
    assert "EXECUTE turn" in prompts[1]
    assert "send jinja payload" in prompts[1]


def test_shell_loop_stuck_self_kill(tmp_path):
    graph = _Graph()
    solver = _make_solver(tmp_path, envelope=_envelope(), graph=graph)
    prompts = _script(solver, [])  # every scripted turn is barren
    outcome = _run(solver)
    assert outcome.solved is False
    # 3 barren loop turns (stuck_limit=3) + 1 conclude fallback
    assert len(prompts) == 4
    assert solver._worker_stop_reason == "stuck"
    st = load_state(tmp_path / "ws")
    assert st["budget"]["turns_used"] == 3
    assert st["stuck_counter"] >= 3
    kinds = [e["kind"] for e in graph.events]
    assert "edge_shell_stuck" in kinds


def test_shell_loop_turn_limit_hard_cap(tmp_path):
    solver = _make_solver(tmp_path, envelope=_envelope(
        budget={"token_limit": 120000, "turn_limit": 4}))
    prompts = _script(solver, [
        # every turn produces a fact → never stuck; must stop at turn_limit
        CliResult(text=f"real output line {i}\nVERIFIED_FACT=fact number {i}\n"
                       f"REFLECT=changed: f{i}",
                  session="s", input_tokens=10, output_tokens=5)
        for i in range(6)
    ])
    outcome = _run(solver)
    assert outcome.solved is False
    # 4 loop turns + 1 conclude fallback; never a 6th loop turn
    assert len(prompts) == 5
    st = load_state(tmp_path / "ws")
    assert st["budget"]["turns_used"] == 4


def test_shell_loop_token_budget_kill(tmp_path):
    solver = _make_solver(tmp_path, envelope=_envelope(
        budget={"token_limit": 1000, "turn_limit": 10}))
    prompts = _script(solver, [
        CliResult(text=f"output {i}\nVERIFIED_FACT=real finding {i}\n"
                       f"REFLECT=changed: f{i}",
                  session="s", input_tokens=400, output_tokens=100)
        for i in range(10)
    ])
    outcome = _run(solver)
    assert outcome.solved is False
    # turn1 → 500 tokens; turn2 → 1000 ≥ 95% of 1000 → hard stop + conclude
    assert len(prompts) == 3
    assert solver._worker_stop_reason == "budget"
    st = load_state(tmp_path / "ws")
    assert st["budget"]["turns_used"] == 2


def test_shell_loop_crash_resume_from_checkpoint(tmp_path):
    ws = tmp_path / "ws"
    # a previous incarnation got through 2 turns before dying
    st = new_state(shell_id="shell-test01", intent_id="I-shell-1",
                   goal="fuzz the upload parser for SSTI",
                   token_limit=120000, turn_limit=10)
    st["budget"]["turns_used"] = 2
    st["budget"]["tokens_used"] = 300
    st["plan_queue"] = [{"step": 2, "action": "send jinja payload",
                         "verifier": "output contains 49"}]
    st["working_memory"]["confirmed_subfacts"] = ["upload form at /upload"]
    save_state(ws, st)

    solver = _make_solver(tmp_path, envelope=_envelope(), workdir=str(ws))
    solver._flag_ok = lambda flag, prov: True
    prompts = _script(solver, [
        CliResult(text="rendered 49\n"
                       "VERIFIED_FACT=SSTI confirmed via jinja probe 49\n"
                       "FOUND_FLAG=flag{resumed_win}\nREFLECT=changed: done",
                  session="s", input_tokens=10, output_tokens=5),
    ])
    outcome = _run(solver)
    assert outcome.solved is True
    # resume happened at turn 3: exactly one invocation, which carried the
    # RESUMED FROM CHECKPOINT block and the confirmed subfacts
    assert len(prompts) == 1
    assert "RESUMED FROM CHECKPOINT" in prompts[0]
    assert "upload form at /upload" in prompts[0]
    st2 = load_state(ws)
    assert st2["budget"]["turns_used"] == 3


def test_shell_loop_sub_intent_and_plan_markers_parsed(tmp_path):
    assert CliSolver._shell_parse_plan_steps(
        "PLAN_STEP=1|probe /admin|HTTP 200\nPLAN_STEP=2|x|y\njunk") == [
        {"step": 1, "action": "probe /admin", "verifier": "HTTP 200"},
        {"step": 2, "action": "x", "verifier": "y"},
    ]
    assert CliSolver._shell_parse_sub_intents(
        "SUB_INTENT=try ldap bounce\nSUB_INTENT=try ldap bounce") == [
        "try ldap bounce"]
    assert CliSolver._shell_parse_reflect("REFLECT=changed: learned x") == \
        "changed: learned x"


# ── backwards compatibility: default paths unchanged ─────────────────────────


def test_default_explore_path_untouched_by_shell_machinery(tmp_path):
    solver = _make_solver(tmp_path, envelope=None)
    assert solver._shell_loop is False
    prompts = _script(solver, [
        CliResult(text="VERIFIED_FACT=something real from output\n",
                  session="s"),
    ])
    outcome = _run(solver)
    assert outcome.solved is False
    # single-shot explore: exactly one invocation, no shell state file
    assert len(prompts) == 1
    assert load_state(tmp_path / "ws") is None
