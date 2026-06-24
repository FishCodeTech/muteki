"""刀1-7 regression: candidate/intent/fact lifecycle wiring — prove that the
retirement machinery is actually reachable end-to-end and that the dangerous
edges (never compact an active intent) hold. See docs/AUDIT_intent_fact_accumulation_v3.md.
"""

from __future__ import annotations

from muteki.models.solve_graph import Challenge
from muteki.swarm.shared_graph import (
    EV_FACT_REJECTED,
    EV_INTENT_STATE_CHANGED,
    INTENT_DISPATCH_ACTIVE,
    INTENT_DISPATCH_CLOSED,
    INTENT_DISPATCH_RESUME,
    INTENT_DISPATCH_RETIRED,
    SQLiteSharedGraph,
)


def _chal() -> Challenge:
    return Challenge(id="t1", name="t", category="pwn")


def _dispatch_states(g) -> dict[str, str]:
    with g._lock:
        return dict(g._conn.execute(
            "SELECT intent_id, dispatch_state FROM intents ORDER BY intent_id"
        ).fetchall())


# ── 刀1: fact terminal states retire a candidate out of the active view ──────
def test_reject_fact_retires_candidate(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    seq = g.add_evidence(actor="s1", source="x", fact="maybe-cred ABC", verified=False)
    assert any(c["fact_seq"] == seq for c in g.active_candidates())
    g.reject_fact(actor="coordinator", fact_seq=seq, reason="proved false")
    # gone from the live candidate set; event row stays for audit.
    assert not any(c["fact_seq"] == seq for c in g.active_candidates())
    assert any(e.get("kind") == EV_FACT_REJECTED for e in g.events())
    g.close()


def test_merge_and_supersede_retire(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    a = g.add_evidence(actor="s1", source="x", fact="dup A", verified=False)
    b = g.add_evidence(actor="s1", source="x", fact="canonical B", verified=False)
    c = g.add_evidence(actor="s1", source="x", fact="old C", verified=False)
    g.merge_fact(actor="coordinator", from_fact_seq=a, to_fact_seq=b, reason="same")
    g.supersede_fact(actor="coordinator", fact_seq=c, reason="newer exists")
    live = {x["fact_seq"] for x in g.active_candidates()}
    assert a not in live and c not in live and b in live
    # merge into self is rejected (-1)
    assert g.merge_fact(actor="coordinator", from_fact_seq=b, to_fact_seq=b) == -1
    g.close()


# ── 刀2/J: non-solved finalize parks active opens as resume + emits delta ─────
def test_finalize_resume_emits_state_change(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-open", goal="open dir")
    out = g.release_claims_for_finalize(reason="runtime_failure")
    assert out["resumed_intents"] == ["I-open"]
    assert _dispatch_states(g)["I-open"] == INTENT_DISPATCH_RESUME
    # the transition is recorded as an event row (the bus mirror is emitted by the
    # coordinator from this same return value).
    sc = [e for e in g.events() if e.get("kind") == EV_INTENT_STATE_CHANGED]
    assert sc and "I-open" in str(sc[-1]["payload"].get("intent_id"))
    g.close()


# ── 刀4: revive flips resume → active so a continued run can dispatch them ────
def test_revive_resume_intents_roundtrip(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-a", goal="a")
    g.propose_intent(actor="reason", intent_id="I-b", goal="b")
    # runtime_failure parks active intents as resume (operator_stop now closes them),
    # giving us resume rows to exercise the revive round-trip below.
    g.release_claims_for_finalize(reason="runtime_failure")
    assert set(_dispatch_states(g).values()) == {INTENT_DISPATCH_RESUME}
    # resume rows are NOT claimable (held back)…
    assert g.claim_intent(worker="w1", intent_id="I-a") is False
    revived = g.revive_resume_intents(actor="coordinator")
    assert sorted(revived) == ["I-a", "I-b"]
    assert set(_dispatch_states(g).values()) == {INTENT_DISPATCH_ACTIVE}
    # …and claimable again after revive.
    assert g.claim_intent(worker="w1", intent_id="I-a") is True
    g.close()


# ── 刀5: route suppression closes intents into dispatch_state=closed ──────────
def test_suppress_route_closes_dispatch_state(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    rh = SQLiteSharedGraph.normalize_route_hash("sqli on /login")
    g.propose_intent(actor="reason", intent_id="I-r", goal="hit /login",
                     payload={"route_hash": rh})
    res = g.suppress_route(actor="coordinator", route_hash=rh, label="sqli on /login",
                           reason="dead route")
    assert res["superseded"] == ["I-r"]
    # 刀5: not just status='done' — dispatch_state must be 'closed' so the
    # compactor (done/closed) and the deck agree it's terminal.
    assert _dispatch_states(g)["I-r"] == INTENT_DISPATCH_CLOSED
    with g._lock:
        st, cr = g._conn.execute(
            "SELECT status, close_reason FROM intents WHERE intent_id='I-r'"
        ).fetchone()
    assert st == "done" and cr == "route_suppressed"
    # reopen_route restores it to open/active (symmetry).
    g.reopen_route(actor="coordinator", route_hash=rh)
    assert _dispatch_states(g)["I-r"] == INTENT_DISPATCH_ACTIVE
    g.close()


# ── 刀6: compact retires resume + closed-barren, but NEVER an active intent ───
def test_compact_retires_resume_never_active(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    # an active, queued intent — the dispatch queue; must survive compaction.
    g.propose_intent(actor="reason", intent_id="I-active", goal="live work")
    # a resume-parked intent (stranded by a prior finalize).
    g.propose_intent(actor="reason", intent_id="I-resume", goal="stranded")
    # a concluded barren intent (done/closed, no fact).
    g.propose_intent(actor="reason", intent_id="I-done", goal="barren")
    g.claim_intent(worker="w1", intent_id="I-done")
    g.conclude_intent(actor="w1", intent_id="I-done", result="dead_end")
    # park I-resume by finalizing then re-proposing the active one fresh.
    with g._lock:
        g._conn.execute(
            "UPDATE intents SET dispatch_state='resume' WHERE intent_id='I-resume'")
        g._conn.commit()

    info = g.compact_graph(actor="coordinator", trigger="test")
    retired = set(info["retired_intent_ids"])
    assert "I-resume" in retired
    assert "I-done" in retired
    # THE GUARD: an active intent is never compacted.
    assert "I-active" not in retired
    states = _dispatch_states(g)
    assert states["I-active"] == INTENT_DISPATCH_ACTIVE
    assert states["I-resume"] == INTENT_DISPATCH_RETIRED
    assert states["I-done"] == INTENT_DISPATCH_RETIRED
    # a claim on the still-active intent works; on a retired one it does not.
    assert g.claim_intent(worker="w2", intent_id="I-active") is True
    assert g.claim_intent(worker="w3", intent_id="I-resume") is False
    g.close()


def test_compact_keeps_intent_with_fact(tmp_path):
    """A concluded intent that PRODUCED a fact (to_fact_seq set) is evidence-bearing
    and must not be compacted away."""
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-fact", goal="produced a fact")
    g.claim_intent(worker="w1", intent_id="I-fact")
    with g._lock:
        g._conn.execute(
            "UPDATE intents SET status='done', dispatch_state='closed', to_fact_seq=99 "
            "WHERE intent_id='I-fact'")
        g._conn.commit()
    info = g.compact_graph(actor="coordinator", trigger="test")
    assert "I-fact" not in set(info["retired_intent_ids"])
    g.close()


# ── 刀7: route-less candidates are now capped (per-actor catch-all bucket) ────
def test_routeless_candidate_cap(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    cap = g.CANDIDATE_CAP_PER_SOURCE_NOROUTE
    accepted = 0
    for i in range(cap + 25):
        seq = g.add_evidence(actor="cli-codex", source="x",
                             fact=f"routeless finding {i}", verified=False)
        if seq != -1:
            accepted += 1
    assert accepted == cap  # bounded, no longer an unbounded bypass
    # a DIFFERENT actor has its own independent bucket (not globally starved).
    seq2 = g.add_evidence(actor="cli-claude", source="x",
                          fact="other actor finding", verified=False)
    assert seq2 != -1
    # a routed candidate uses the per-route cap, independent of the route-less one.
    rh = SQLiteSharedGraph.normalize_route_hash("some specific route")
    seq3 = g.add_evidence(actor="cli-codex", source="x", fact="routed",
                          verified=False, route_hash=rh)
    assert seq3 != -1
    g.close()


def test_routeless_cap_does_not_block_verified(tmp_path):
    """The cap is only for unverified candidates — verified facts always land."""
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    cap = g.CANDIDATE_CAP_PER_SOURCE_NOROUTE
    for i in range(cap + 5):
        g.add_evidence(actor="s1", source="x", fact=f"cand {i}", verified=False)
    # verified still accepted past the candidate cap.
    seq = g.add_evidence(actor="s1", source="x", fact="VERIFIED truth", verified=True)
    assert seq != -1
    g.close()
