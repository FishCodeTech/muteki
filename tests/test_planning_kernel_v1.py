"""Unit tests for planning-layer kernel (no network, no Swarm I/O)."""

from __future__ import annotations

from muteki.solver.planning_kernel_v1 import (
    Hypothesis,
    HypothesisStatus,
    Phase,
    Subgoal,
    UnitStatus,
    WorkingMemory,
    apply_executor_receipt,
    apply_hypothesis_result,
    default_mission_seed,
    merge_hypotheses,
    merge_subgoals,
    next_phase,
    parse_hypothesis_plan,
    parse_mission_plan,
    phase_parallel_budget,
    ready_subgoals,
    select_dispatch,
    select_hypothesis_tests,
)


def test_parse_mission_plan_extracts_deps_and_abandon():
    raw = """
    Here is the plan:
    ```json
    {
      "subgoals": [
        {"id": "S1", "goal": "run file on bin", "depends_on": []},
        {"id": "S2", "goal": "xor decode", "depends_on": ["S1"], "rationale": "chain"}
      ],
      "abandon": ["S0"],
      "parallel_max": 3,
      "notes": "ok"
    }
    ```
    """
    plan = parse_mission_plan(raw)
    assert len(plan.subgoals) == 2
    assert plan.subgoals[1].depends_on == ["S1"]
    assert plan.abandon_ids == ["S0"]
    assert plan.parallel_max == 3


def test_ready_subgoals_respects_dependencies():
    subs = [
        Subgoal(id="S1", goal="a", status=UnitStatus.DONE),
        Subgoal(id="S2", goal="b", depends_on=["S1"]),
        Subgoal(id="S3", goal="c", depends_on=["S2"]),
    ]
    ready = ready_subgoals(subs)
    assert [s.id for s in ready] == ["S2"]
    assert next(s for s in subs if s.id == "S3").status == UnitStatus.BLOCKED


def test_select_dispatch_prefers_unseen_low_attempts():
    ready = [
        Subgoal(id="S1", goal="repeat me", attempts=1),
        Subgoal(id="S2", goal="fresh", attempts=0),
    ]
    chosen = select_dispatch(ready, parallel_max=1, avoid_goals={"repeat me"})
    assert chosen[0].id == "S2"


def test_merge_subgoals_abandons_and_dedups():
    existing = [Subgoal(id="S1", goal="old path")]
    incoming = [
        Subgoal(id="S2", goal="new path"),
        Subgoal(id="S3", goal="Old Path"),  # dup normalized
    ]
    merged = merge_subgoals(existing, incoming, abandon_ids=["S1"])
    by_id = {s.id: s for s in merged}
    assert by_id["S1"].status == UnitStatus.ABANDONED
    assert "S2" in by_id
    assert "S3" not in by_id


def test_apply_executor_receipt_folds_memory():
    mem = WorkingMemory()
    sub = Subgoal(id="S1", goal="x")
    apply_executor_receipt(
        mem,
        sub,
        new_facts=["magic=ELF"],
        success=True,
        summary="found elf",
        found_flag="",
    )
    assert sub.status == UnitStatus.DONE
    assert "magic=ELF" in mem.facts
    assert mem.receipts


def test_hypothesis_falsify_and_select():
    hyps = [
        Hypothesis(id="H1", claim="otp reuse", test="crib drag"),
        Hypothesis(id="H2", claim="rsa wiener", test="check d small"),
    ]
    mem = WorkingMemory()
    apply_hypothesis_result(
        mem,
        hyps[0],
        verdict="falsified",
        evidence="no repeating xor",
        new_facts=[],
    )
    assert hyps[0].status == HypothesisStatus.FALSIFIED
    assert any("FALSIFIED" in d for d in mem.dead_ends)
    chosen = select_hypothesis_tests(hyps, parallel_max=2)
    assert [h.id for h in chosen] == ["H2"]


def test_merge_hypotheses_keeps_supported():
    existing = [
        Hypothesis(
            id="H1",
            claim="a",
            test="t",
            status=HypothesisStatus.SUPPORTED,
        )
    ]
    incoming = [Hypothesis(id="H1", claim="a2", test="t2")]
    merged = merge_hypotheses(existing, incoming, abandon_ids=[])
    assert merged[0].claim == "a"
    assert merged[0].status == HypothesisStatus.SUPPORTED


def test_phase_transitions_and_parallel_budget():
    assert next_phase(
        Phase.RECON,
        fact_count=2,
        fruitless_rounds=0,
        has_candidate_flag=False,
        phase_budget_exhausted=False,
    ) == Phase.ANALYZE
    assert next_phase(
        Phase.EXPLOIT,
        fact_count=5,
        fruitless_rounds=0,
        has_candidate_flag=True,
        phase_budget_exhausted=False,
    ) == Phase.VERIFY
    assert phase_parallel_budget(Phase.VERIFY, max_workers=4) == 1
    assert phase_parallel_budget(Phase.EXPLOIT, max_workers=4) == 3


def test_parse_hypothesis_plan_requires_claim_and_test():
    plan = parse_hypothesis_plan(
        '{"hypotheses":[{"id":"H1","claim":"c","test":"t"},{"id":"H2","claim":"only"}]}'
    )
    assert len(plan.hypotheses) == 1
    assert plan.hypotheses[0].id == "H1"


def test_default_mission_seed_is_chained():
    plan = default_mission_seed("crypto", "otp challenge")
    assert len(plan.subgoals) >= 3
    assert plan.subgoals[1].depends_on == ["S1"]
    assert plan.subgoals[2].depends_on == ["S2"]


def test_working_memory_caps():
    mem = WorkingMemory(max_facts=3, max_receipts=2)
    for i in range(5):
        mem.add_fact(f"f{i}")
        mem.add_receipt(f"r{i}")
    assert len(mem.facts) == 3
    assert mem.facts[0] == "f2"
    assert len(mem.receipts) == 2
    text = mem.render()
    assert "[working-memory]" in text
    assert "f4" in text
