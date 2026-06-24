"""P-A: SQLiteSharedGraph — append / materialize / dedupe / replay / concurrency."""

from __future__ import annotations

import threading

import pytest

from muteki.models.solve_graph import Challenge
from muteki.swarm.shared_graph import SQLiteSharedGraph, SharedGraph, canonicalize_lane


def _chal() -> Challenge:
    return Challenge(id="t1", name="t", category="crypto")


def test_append_then_materialize(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.add_evidence(actor="s1", source="run_python", fact="key is 0x1337",
                   artifact_id="aid1", verified=True)
    snap = g.snapshot()
    assert len(snap.evidence) == 1
    assert snap.evidence[0].fact == "key is 0x1337"
    assert snap.evidence[0].verified is True
    assert snap.evidence[0].source_solver == "s1"
    g.close()


def test_dedupe_same_fact(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.add_evidence(actor="s1", source="x", fact="same", artifact_id="a")
    g.add_evidence(actor="s1", source="x", fact="same", artifact_id="a")  # dup
    assert len(g.snapshot().evidence) == 1
    # different actor → not a dup
    g.add_evidence(actor="s2", source="x", fact="same", artifact_id="a")
    assert len(g.snapshot().evidence) == 2
    g.close()


def test_replay_consistency_reopen(tmp_path):
    p = tmp_path / "g.db"
    g = SQLiteSharedGraph.open(db_path=p, challenge=_chal())
    g.add_evidence(actor="s1", source="x", fact="f1")
    g.add_dead_end(actor="s1", reason="brute force too slow")
    g.flag_found(actor="s2", flag="flag{x}")
    g.close()
    # reopen → same materialized state from the event log
    g2 = SQLiteSharedGraph.open(db_path=p, challenge=_chal())
    snap = g2.snapshot()
    assert any(ev.fact == "f1" for ev in snap.evidence)
    assert "brute force too slow" in snap.dead_ends
    assert snap.flag == "flag{x}"
    assert len(g2.events()) == 3
    g2.close()


def test_events_since_returns_incremental_filtered_rows(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    f1 = g.add_evidence(actor="s1", source="x", fact="f1", verified=True)
    d1 = g.add_dead_end(actor="s1", reason="dead route")
    g.propose_intent(actor="reason", intent_id="I1", goal="try next")

    all_after_fact = g.events_since(f1)
    assert [e["seq"] for e in all_after_fact] == [d1, d1 + 1]

    only_dead = g.events_since(0, kinds=["dead_end"])
    assert [e["kind"] for e in only_dead] == ["dead_end"]
    assert only_dead[0]["seq"] == d1
    g.close()


def test_add_evidence_records_intent_products(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-prod", goal="check admin panel")
    g.claim_intent(worker="cli-a", intent_id="I-prod")

    f1 = g.add_evidence(
        actor="cli-a", source="curl", fact="admin panel exists",
        verified=True, intent_id="I-prod")
    f2 = g.add_evidence(
        actor="cli-a", source="curl", fact="admin panel leaks version",
        verified=True, intent_id="I-prod")

    assert g.intent_products("I-prod") == [f1, f2]
    g.close()


def test_deduped_evidence_still_links_product_to_later_intent(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-a", goal="first route")
    g.propose_intent(actor="reason", intent_id="I-b", goal="second route")

    f1 = g.add_evidence(
        actor="cli-a", source="cmd", fact="same observed service",
        verified=True, intent_id="I-a")
    f2 = g.add_evidence(
        actor="cli-a", source="cmd", fact="same observed service",
        verified=True, intent_id="I-b")

    assert f1 > 0
    assert f2 == -1, "event dedupe should still avoid appending duplicate facts"
    assert g.intent_products("I-a") == [f1]
    assert g.intent_products("I-b") == [f1]
    g.close()


def test_retired_source_fact_is_not_active_lineage_or_worker_neighborhood(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    fact = g.add_evidence(actor="cli-a", source="cmd", fact="retired token value",
                          verified=True)
    g.propose_intent(actor="reason", intent_id="I-bad", goal="use reviewed credential",
                     from_fact_seqs=[fact])
    g.reject_fact(actor="review", fact_seq=fact, reason="credential disproved")

    summary = g.to_reason_summary()
    neighborhood = g.intent_neighborhood_block("I-bad")

    assert "Retired facts" in summary
    assert "retired token value" in summary
    lineage = summary.split("## Active intent lineage", 1)[-1]
    assert "retired token value" not in lineage
    assert "retired token value" not in neighborhood
    g.close()


def test_reason_summary_keeps_orphan_candidate_and_model_pinned_old_facts(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    old_verified = g.add_evidence(
        actor="cli-old", source="cmd", fact="ADMIN CREDENTIALS admin:hunter2",
        verified=True)
    host_port = g.add_evidence(
        actor="cli-old", source="cmd", fact="service reachable at 10.0.0.5:8080",
        verified=True)
    candidate = g.add_evidence(
        actor="cli-candidate", source="cmd", fact="login form may be injectable",
        verified=False)
    for idx in range(10):
        g.add_evidence(actor=f"cli-{idx}", source="cmd",
                       fact=f"new verified fact {idx}", verified=True)

    pre_pin = g.to_reason_summary()
    assert "ADMIN CREDENTIALS admin:hunter2" not in pre_pin
    assert "service reachable at 10.0.0.5:8080" not in pre_pin

    g.pin_facts(actor="reason", fact_seqs=[old_verified],
                reason="model selected reusable credential")
    summary = g.to_reason_summary()

    assert old_verified > 0 and host_port > 0 and candidate > 0
    assert "ADMIN CREDENTIALS admin:hunter2" in summary
    assert "service reachable at 10.0.0.5:8080" not in summary
    assert "login form may be injectable" in summary
    assert "Candidates / needs verification" in summary
    g.close()


def test_fact_pin_context_exposes_non_english_facts_for_model_judgment(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    seq = g.add_evidence(actor="cli-cn", source="cmd",
                         fact="后台口令是 admin / 猎人二号", verified=True)

    ctx = g.fact_pin_context()

    assert f"[#{seq}]" in ctx
    assert "后台口令" in ctx
    g.close()


def test_dead_end_producer_does_not_revive_sibling_products_through_bridge(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-dead", goal="dead route")
    assert g.claim_intent(worker="cli-dead", intent_id="I-dead") is True
    bridge = g.add_evidence(actor="cli-dead", source="cmd", fact="bridge fact",
                            verified=False, intent_id="I-dead")
    g.add_evidence(actor="cli-dead", source="cmd", fact="dead sibling lead",
                   verified=False, intent_id="I-dead")
    g.conclude_intent(actor="cli-dead", intent_id="I-dead", result="dead_end")
    g.propose_intent(actor="reason", intent_id="I-now", goal="current route",
                     from_fact_seqs=[bridge])

    summary = g.to_reason_summary()
    assert "bridge fact" in summary
    assert "dead sibling lead" not in summary

    g.reject_fact(actor="review", fact_seq=bridge, reason="bridge disproved")
    rejected_summary = g.to_reason_summary()
    assert "dead sibling lead" not in rejected_summary
    lineage = rejected_summary.split("## Active intent lineage", 1)[-1]
    assert "bridge fact" not in lineage
    g.close()


def test_intents_schema_has_separate_result_columns(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    with g._lock:
        cols = {
            str(row[1]): str(row[2])
            for row in g._conn.execute("PRAGMA table_info(intents)").fetchall()
        }
    assert cols["result_seq"].upper() == "INTEGER"
    assert cols["result_detail"].upper() == "TEXT"
    g.close()


def test_concurrent_writers_no_loss(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    N = 20

    def writer(i):
        g.add_evidence(actor=f"s{i}", source="x", fact=f"fact{i}")

    threads = [threading.Thread(target=writer, args=(i,)) for i in range(N)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    # every distinct (actor,fact) event present — no lost writes (busy_timeout)
    assert len(g.snapshot().evidence) == N
    g.close()


def test_atomic_intent_claim_single_winner(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I1", goal="crack xor")
    wins = []

    def claimer(w):
        wins.append(g.claim_intent(worker=w, intent_id="I1"))

    threads = [threading.Thread(target=claimer, args=(f"w{i}",)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert sum(1 for x in wins if x) == 1  # exactly one winner, zero TOCTOU
    g.close()


def test_canonicalize_lane_resource_only_and_failclosed():
    key1, conf1, _ = canonicalize_lane(
        host="http://172.22.11.45/ms17-010", port=445,
        service="smb", risk_class="destructive")
    key2, conf2, _ = canonicalize_lane(
        host="172.22.11.45", port=445,
        service="eternalblue", risk_class="destructive")
    assert key1 == key2 == "destructive:tcp:445@172.22.11.45"
    assert conf1 > 0 and conf2 > 0

    unknown, conf, reason = canonicalize_lane(
        host="??target alias??", port=None, service="",
        risk_class="destructive")
    assert unknown.startswith("destructive:tcp:*@unknown-host:")
    assert conf < 0.7
    assert reason


def test_lane_lock_blocks_second_owner_and_cooldown(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    lane = "destructive:tcp:445@172.22.11.45"
    first = g.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="w1", owner_intent="I1")
    second = g.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="w2", owner_intent="I2")

    assert first["acquired"] is True
    assert second["acquired"] is False
    assert second["held_by"] == "w1"
    assert g.is_lane_held_by_other(lane, "w2") is True

    rel = g.release_lane(actor="coord", lane_key=lane, by_worker="w1")
    assert rel["released"] is True
    assert g.in_lane_cooldown(lane, "w1") is True
    assert g.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="w1", owner_intent="I1b")["acquired"] is False
    assert g.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="w2", owner_intent="I2")["acquired"] is True
    g.close()


def test_coordinator_lane_reservation_is_claimed_by_first_worker(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    lane = "destructive:tcp:5000@107.170.15.231"
    reservation = g.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="coordinator", owner_intent="")
    first_worker = g.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="cli-codex-1", owner_intent="I-next")
    second_worker = g.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="cli-claude-1", owner_intent="I-other")

    assert reservation["acquired"] is True
    assert first_worker["acquired"] is True
    assert second_worker["acquired"] is False
    assert second_worker["held_by"] == "cli-codex-1"
    assert g.active_lanes()[0]["owner_worker"] == "cli-codex-1"
    g.close()


def test_lane_deferred_intent_revives_on_release_and_epoch_dedups(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    lane = "destructive:tcp:445@172.22.11.45"
    g.propose_intent(
        actor="reason", intent_id="I1", goal="exploit smb",
        payload={"lane_key": lane, "risk_class": "destructive"})
    assert g.claim_intent(worker="w-loser", intent_id="I1")
    lock = g.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="w-owner", owner_intent="I-owner")

    g.defer_intent_for_lane(
        actor="coord", intent_id="I1", lane_key=lane,
        against_locked_seq=lock["seq"])
    g.defer_intent_for_lane(
        actor="coord", intent_id="I1", lane_key=lane,
        against_locked_seq=lock["seq"])
    with g._lock:
        row = g._conn.execute(
            "SELECT status, worker, lane_deferrals FROM intents WHERE intent_id='I1'"
        ).fetchone()
    assert row == ("done", None, 1)

    rel = g.release_lane(actor="coord", lane_key=lane, by_worker="w-owner")
    assert rel["revived"] == ["I1"]
    with g._lock:
        row = g._conn.execute(
            "SELECT status, worker, result_seq, deferred_against_locked_seq "
            "FROM intents WHERE intent_id='I1'"
        ).fetchone()
    assert row == ("open", None, None, None)
    g.close()


def test_finalize_releases_claims_and_only_solved_closes_open(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-open", goal="open")
    g.propose_intent(actor="reason", intent_id="I-claimed", goal="claimed")
    g.claim_intent(worker="w1", intent_id="I-claimed")
    # runtime_failure (a crash that may be retried) is the non-solved terminal that
    # PARKS intents as resume rather than closing them; status stays 'open'.
    # operator_stop now closes like solved (see test_finalize_operator_stop_closes).
    out = g.release_claims_for_finalize(reason="runtime_failure")
    assert out["released_claims"] == ["I-claimed"]
    with g._lock:
        rows = dict(g._conn.execute(
            "SELECT intent_id, status FROM intents ORDER BY intent_id").fetchall())
    assert rows == {"I-claimed": "open", "I-open": "open"}
    g.close()

    g2 = SQLiteSharedGraph.open(db_path=tmp_path / "g2.db", challenge=_chal())
    g2.propose_intent(actor="reason", intent_id="I-open", goal="open")
    g2.propose_intent(actor="reason", intent_id="I-claimed", goal="claimed")
    g2.claim_intent(worker="w1", intent_id="I-claimed")
    out2 = g2.release_claims_for_finalize(reason="solved")
    assert sorted(out2["closed_intents"]) == ["I-claimed", "I-open"]
    with g2._lock:
        rows = dict(g2._conn.execute(
            "SELECT intent_id, status FROM intents ORDER BY intent_id").fetchall())
    assert rows == {"I-claimed": "done", "I-open": "done"}
    g2.close()


def test_finalize_operator_stop_closes_active_intents(tmp_path):
    # ⑤ operator_stop = the user deliberately ending the run → close active intents
    # like a solved run (done/closed/stop_reason=operator_stop), NOT park as resume.
    # Parking them stranded a pile of verify/review intents no running coordinator
    # ever revives (run-75377: 53 stranded). budget/runtime still resume.
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-open", goal="open")
    g.propose_intent(actor="reason", intent_id="I-claimed", goal="claimed")
    g.claim_intent(worker="w1", intent_id="I-claimed")
    out = g.release_claims_for_finalize(reason="operator_stop")
    assert out["released_claims"] == ["I-claimed"]
    assert sorted(out["closed_intents"]) == ["I-claimed", "I-open"]
    assert out["resumed_intents"] == []
    with g._lock:
        rows = dict(g._conn.execute(
            "SELECT intent_id, status || ':' || dispatch_state || ':' || "
            "COALESCE(stop_reason,'') FROM intents ORDER BY intent_id").fetchall())
    assert rows == {"I-claimed": "done:closed:operator_stop",
                    "I-open": "done:closed:operator_stop"}
    assert g.open_goal_texts() == []  # nothing left to dispatch
    g.close()


def test_conclude_intent(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I1", goal="g")
    assert g.claim_intent(worker="w1", intent_id="I1") is True
    g.conclude_intent(actor="w1", intent_id="I1", result="done")
    # a second claim on a done intent fails
    assert g.claim_intent(worker="w2", intent_id="I1") is False
    g.close()


def test_result_code_predicates_are_shared():
    from muteki.solver.result_codes import (
        RESULT_DEAD_END,
        RESULT_EXPLORED,
        RESULT_TIMED_OUT,
        is_genuine_giveup,
        is_neutral,
        is_transient,
    )

    assert is_genuine_giveup(RESULT_DEAD_END) is True
    assert is_genuine_giveup(RESULT_TIMED_OUT) is False
    assert is_transient(RESULT_TIMED_OUT) is True
    assert is_neutral(RESULT_EXPLORED) is True


def test_conclude_intent_stores_and_renders_result_detail(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-detail", goal="check admin panel")
    assert g.claim_intent(worker="w1", intent_id="I-detail") is True

    g.conclude_intent(
        actor="w1",
        intent_id="I-detail",
        result="timed_out",
        result_detail="Ran curl against /admin and timed out before a verified flag.",
    )

    with g._lock:
        row = g._conn.execute(
            "SELECT close_reason, result_detail FROM intents WHERE intent_id='I-detail'"
        ).fetchone()
    assert row == ("timed_out", "Ran curl against /admin and timed out before a verified flag.")
    attempted = g._attempted_intents_block()
    assert "timed_out" in attempted
    assert "Ran curl against /admin" in attempted
    g.close()


def test_poc_save_claim_conclude_owner_fence_and_nullable_intent(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.save_poc(
        actor="cli-a",
        poc_id="poc-null",
        path="shared/objects/aa/bb/hash",
        artifact_id="hash",
        entry_command="python poc.py",
        status="available",
        note="race bootstrap artifact",
        intent_id=None,
        name="poc.py",
    )

    row = g.pocs()[0]
    assert row["intent_id"] is None
    assert row["status"] == "available"
    assert g.claim_poc(worker="cli-b", poc_id="poc-null", lease_s=1000.0) is True
    assert g.claim_poc(worker="cli-c", poc_id="poc-null", lease_s=1000.0) is False

    g.conclude_poc(actor="cli-c", poc_id="poc-null", status="spent", note="late")
    assert g.pocs()[0]["status"] == "wip", "late non-owner conclusion must be fenced"
    g.conclude_poc(actor="cli-b", poc_id="poc-null", status="spent", note="done")
    assert g.pocs()[0]["status"] == "spent"
    assert {e["kind"] for e in g.events()} >= {"poc_saved", "poc_claimed", "poc_concluded"}
    g.close()


def test_poc_metadata_on_board_reason_and_barren_dedup_exclusion(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-poc", goal="exploit upload parser")
    g.claim_intent(worker="cli-a", intent_id="I-poc")
    g.save_poc(
        actor="cli-a",
        poc_id="poc-dir",
        path="shared/objects/aa/bb/hash",
        artifact_id="hash",
        entry_command="python poc.py http://target",
        status="directional",
        note="needs final cookie",
        intent_id="I-poc",
        name="poc.py",
    )
    g.conclude_intent(actor="cli-a", intent_id="I-poc", result="explored")

    assert "exploit upload parser" not in g.barren_concluded_goal_texts()
    board = g.to_board_markdown()
    reason = g.to_reason_summary()
    # a 'directional' PoC is inheritable → listed under the Inheritable header that
    # promises the ./inherited/<poc_id>/ mount (#10); only inheritable PoCs get that
    # path, in-use/spent ones go under "Historical" with no path.
    assert "Inheritable PoCs" in board and "python poc.py http://target" in board
    assert "./inherited/<poc_id>/" in board
    assert "poc-dir" in reason
    g.close()


def test_dead_end_conclusion_marks_related_poc_spent(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I-dead", goal="try old CVE")
    g.claim_intent(worker="cli-a", intent_id="I-dead")
    g.save_poc(
        actor="cli-a",
        poc_id="poc-old-cve",
        path="shared/objects/aa/bb/hash",
        artifact_id="hash",
        entry_command="python old.py",
        status="available",
        intent_id="I-dead",
        name="old.py",
    )

    g.conclude_intent(actor="cli-a", intent_id="I-dead", result="dead_end: patched")

    assert g.pocs()[0]["status"] == "spent"
    assert "try old CVE" in g.barren_concluded_goal_texts()
    # #10: a spent PoC is NOT inheritable and must NOT be advertised with a
    # ./inherited/ mount path — it goes under "Historical PoCs", metadata only.
    assert g.pocs(inheritable_only=True) == []
    board = g.to_board_markdown()
    assert "Historical PoCs" in board
    assert "poc-old-cve" in board  # still listed for context
    # the inherited-path header only appears when there IS an inheritable PoC
    assert "Inheritable PoCs" not in board
    g.close()


def test_claimed_poc_reappears_after_lease_expiry(tmp_path):
    """#9: claim_poc flips a PoC to 'wip' to mark it in-use, but that must NOT make
    it vanish from the inheritable pool forever. While the claiming worker's lease
    is live the PoC is hidden; once the lease EXPIRES (worker died/finished) it must
    be offered again, mirroring how _open_intents re-offers an expired-lease intent."""
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.save_poc(
        actor="cli-a", poc_id="poc-x", path="shared/objects/aa/bb/h",
        artifact_id="h", entry_command="python x.py", status="available",
        intent_id=None, name="x.py",
    )
    assert {p["poc_id"] for p in g.pocs(inheritable_only=True)} == {"poc-x"}
    # claim with a tiny lease → immediately in-use, hidden from the pool
    assert g.claim_poc(worker="cli-b", poc_id="poc-x", lease_s=0.05) is True
    assert g.pocs(inheritable_only=True) == [], "a live-leased wip PoC is hidden"
    # lease expires → the PoC must reappear as inheritable (not lost forever)
    import time as _t
    _t.sleep(0.08)
    assert {p["poc_id"] for p in g.pocs(inheritable_only=True)} == {"poc-x"}, \
        "an expired-lease wip PoC must be re-offered, not single-use"
    g.close()


def test_protocol_conformance(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    assert isinstance(g, SharedGraph)  # runtime_checkable Protocol
    g.close()


# ── planner view: uncapped dead-ends + in-flight/attempted intent sections ───

def test_to_reason_summary_uncaps_dead_ends(tmp_path):
    """Reason's view must show EVERY dead-end, not just the last 8 — a long run
    that buries an early dead-end under newer ones would otherwise re-propose it.
    (P1.5 lifted the evidence cap but left dead-ends tied to max_hypotheses=8.)"""
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    for i in range(20):
        g.add_dead_end(actor="w", reason=f"ruled out path number {i}")
    s = g.to_reason_summary()
    assert "path number 0" in s, "oldest dead-end must survive into the planner view"
    assert "path number 19" in s
    # the compaction summary still clips (legacy behavior preserved)
    short = g.to_summary()
    assert "path number 0" not in short


def test_to_reason_summary_shows_open_and_attempted_intents(tmp_path):
    """The planner view must carry the two intent sections the SolveGraph snapshot
    can't: open/claimed (in flight) and concluded (attempted, with results)."""
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="open1", goal="probe the login form")
    g.propose_intent(actor="reason", intent_id="done1", goal="crack the weak XOR key")
    g.claim_intent(worker="w1", intent_id="done1")
    g.conclude_intent(actor="w1", intent_id="done1",
                      result="XOR key was not weak; brute force exhausted")
    s = g.to_reason_summary()
    assert "Open intents" in s and "probe the login form" in s
    assert "Already attempted" in s and "crack the weak XOR key" in s
    assert "brute force exhausted" in s, "the conclusion result must be shown"
    g.close()


def test_to_reason_summary_uses_active_intent_lineage_not_old_noise(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    for i in range(25):
        g.add_evidence(actor="noise", source="scan", fact=f"old unrelated noise {i}",
                       verified=True)
    root = g.add_evidence(actor="cli-a", source="curl",
                          fact="root credential admin:hunter2 works", verified=True)
    g.propose_intent(
        actor="reason", intent_id="I-lineage", goal="use admin credential on /admin",
        from_fact_seqs=[root])

    summary = g.to_reason_summary()

    assert "Active intent lineage" in summary
    assert f"#{root}" in summary and "root credential admin:hunter2 works" in summary
    assert "old unrelated noise 0" not in summary
    assert g.claim_intent(worker="w1", intent_id="I-lineage") is True
    g.close()


def test_open_intents_prompt_cap_does_not_change_claimability(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    for i in range(30):
        g.propose_intent(actor="reason", intent_id=f"I{i}", goal=f"queued task {i}")

    block = g._open_intents_block(limit=5)

    assert "queued task 29" in block
    assert "queued task 0" not in block
    assert g.claim_intent(worker="w1", intent_id="I0") is True
    g.close()


def test_open_goal_texts_returns_open_and_claimed_only(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="o1", goal="open direction")
    g.propose_intent(actor="reason", intent_id="c1", goal="claimed direction")
    g.claim_intent(worker="w1", intent_id="c1")
    g.propose_intent(actor="reason", intent_id="d1", goal="done direction")
    g.claim_intent(worker="w2", intent_id="d1")
    g.conclude_intent(actor="w2", intent_id="d1", result="finished")
    goals = g.open_goal_texts()
    assert "open direction" in goals and "claimed direction" in goals
    assert "done direction" not in goals, "concluded intents are not dedup references"
    g.close()


def test_defect9_captured_flags_surface_in_reason_summary(tmp_path):
    """defect-9: the planner's reason summary lists already-captured flags so it won't
    re-propose intents to re-recover them (the ezrop-ROP re-do). Empty before any
    flag — byte-identical for single/zero-flag runs."""
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    assert "Flags already captured" not in g.to_reason_summary()   # none yet
    g.flag_found(actor="cli-claude", flag="flag{first}")
    g.flag_found(actor="cli-codex", flag="flag{second}")
    summary = g.to_reason_summary()
    assert "Flags already captured" in summary
    assert "flag{first}" in summary and "flag{second}" in summary
    assert "DONE" in summary                                       # the no-re-propose cue
    g.close()


# ── review-arbiter state: challenged facts, routes, branches ────────────────

def test_review_challenges_fact_and_creates_verifier_intent(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    seq = g.add_evidence(actor="cli-a", source="a", fact="JWT likely uses HS256",
                         verified=True, artifact_id="a1")

    info = g.challenge_fact(
        actor="cli-review", fact_seq=seq,
        reason="Only a model inference; no decoded header was shown.",
        verification_goal="Decode a real JWT header and verify the alg from output.",
    )

    assert info["verification_intent_id"].startswith("I-verify-")
    assert g.challenged_facts()[0]["fact_seq"] == seq
    snap = g.snapshot()
    assert snap.evidence[0].verified is False, \
        "challenged facts must not remain usable as verified planner evidence"
    summary = g.to_reason_summary()
    assert "Challenged facts" in summary
    assert "JWT likely uses HS256" in summary
    assert "Decode a real JWT header" in summary
    with g._lock:
        row = g._conn.execute(
            "SELECT worker_class FROM intents WHERE intent_id=?",
            (info["verification_intent_id"],),
        ).fetchone()
    assert row[0] == "verifier"
    g.close()


def test_review_revalidates_fact_restores_verified_eligibility(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    seq = g.add_evidence(actor="cli-a", source="a", fact="JWT alg is HS256",
                         verified=True, artifact_id="a1")
    g.challenge_fact(actor="cli-review", fact_seq=seq, reason="needs proof",
                     verification_goal="verify JWT alg")
    assert g.snapshot().evidence[0].verified is False

    g.revalidate_fact(actor="cli-review", fact_seq=seq,
                      reason="Verifier decoded header from real token.")

    assert g.challenged_facts() == []
    assert g.snapshot().evidence[0].verified is True
    assert "Revalidated facts" in g.to_reason_summary()
    g.close()


def test_review_suppressed_route_retires_matching_open_intents(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(
        actor="reason", intent_id="I-sqli", goal="try login SQL injection",
        payload={"worker_class": "code", "route_hash": "web:login:sqli"},
    )
    g.propose_intent(
        actor="reason", intent_id="I-upload", goal="inspect upload SVG parser",
        payload={"worker_class": "code", "route_hash": "web:upload:svg"},
    )

    res = g.suppress_route(
        actor="cli-review", route_hash="WEB LOGIN SQL injection",
        label="login SQL injection", reason="Repeated by three workers",
        matching_intents=["I-sqli"],
    )

    assert res["route_hash"] == "web:login:sqli"
    assert res["superseded"] == ["I-sqli"]
    assert g.suppressed_routes()[0]["route_hash"] == "web:login:sqli"
    with g._lock:
        rows = dict(g._conn.execute(
            "SELECT intent_id, status FROM intents ORDER BY intent_id").fetchall())
    assert rows["I-sqli"] == "done"
    assert rows["I-upload"] == "open"
    summary = g.to_board_markdown()
    assert "Suppressed routes" in summary and "login SQL injection" in summary
    g.close()


def test_review_suppress_route_does_not_kill_live_claim(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    for iid in ("I-live", "I-open"):
        g.propose_intent(
            actor="reason", intent_id=iid, goal=f"{iid} login SQL injection",
            payload={"worker_class": "code", "route_hash": "web:login:sqli"},
        )
    assert g.claim_intent(worker="cli-claude-1", intent_id="I-live")

    res = g.suppress_route(
        actor="cli-review", route_hash="web:login:sqli",
        label="login SQLi", reason="review found repeated failed route",
    )

    assert res["superseded"] == ["I-open"]
    with g._lock:
        rows = {
            r[0]: (r[1], r[2])
            for r in g._conn.execute(
                "SELECT intent_id, status, worker FROM intents ORDER BY intent_id"
            ).fetchall()
        }
    assert rows["I-live"] == ("claimed", "cli-claude-1")
    assert rows["I-open"] == ("done", None)
    g.close()


def test_review_reopen_route_makes_route_schedulable_again(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.suppress_route(actor="cli-review", route_hash="web:login:sqli",
                     label="login SQLi", reason="loop")
    assert g.is_route_suppressed("web:login:sqli") is True

    g.reopen_route(actor="cli-review", route_hash="web:login:sqli",
                   reason="new WAF bypass evidence", intent_goal="Retest login SQLi with WAF bypass")

    assert g.is_route_suppressed("web:login:sqli") is False
    assert any("Retest login SQLi" in goal for goal in g.open_goal_texts())
    g.close()


def test_review_reopen_route_revives_suppressed_open_intents(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(
        actor="reason", intent_id="I-sqli", goal="try login SQL injection",
        payload={"worker_class": "code", "route_hash": "web:login:sqli"},
    )
    g.suppress_route(actor="cli-review", route_hash="web:login:sqli",
                     label="login SQLi", reason="loop")
    with g._lock:
        assert g._conn.execute(
            "SELECT status FROM intents WHERE intent_id='I-sqli'"
        ).fetchone()[0] == "done"

    info = g.reopen_route(actor="cli-review", route_hash="web:login:sqli",
                          reason="operator hint supplied a real bypass")

    assert info["reopened"] == ["I-sqli"]
    with g._lock:
        row = g._conn.execute(
            "SELECT status, worker, result_seq FROM intents WHERE intent_id='I-sqli'"
        ).fetchone()
    assert row == ("open", None, None)
    g.close()


def test_review_branch_split_records_branch_intents(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    branch = g.split_branch(
        actor="cli-review",
        title="Patched service vs malformed payload",
        branches=[
            {"id": "branch-patched", "assumption": "service is patched",
             "prove_or_disprove": "collect version proof"},
            {"id": "branch-payload", "assumption": "payload is malformed",
             "prove_or_disprove": "build minimal trigger"},
        ],
    )

    assert branch["branch_id"].startswith("branch-")
    branches = g.branches()
    assert {b["branch_id"] for b in branches} == {"branch-patched", "branch-payload"}
    summary = g.to_review_summary()
    assert "Open branches" in summary
    assert "service is patched" in summary and "payload is malformed" in summary
    g.close()


def test_review_branch_resolve_records_materialized_state(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.split_branch(
        actor="cli-review", title="Two hypotheses",
        branches=[
            {"id": "branch-a", "assumption": "A is true",
             "prove_or_disprove": "prove A"},
        ],
    )

    info = g.resolve_branch(actor="cli-review", branch_id="branch-a",
                            reason="A disproven by verifier")

    assert info["branch_id"] == "branch-a"
    assert info["status"] == "resolved"
    assert any(e["kind"] == "branch_resolved" for e in g.events())
    assert g.branches()[0]["status"] == "resolved"
    g.close()


def test_reason_summary_includes_standing_guidance_without_fact_event(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    text = "All internal lateral movement must run from the VPS tunnel."

    summary = g.to_reason_summary(standing_guidance=[text])

    assert "Operator standing guidance" in summary
    assert text in summary
    assert not any(
        e["kind"] == "fact_added" and text in str(e.get("payload", {}))
        for e in g.events()
    )
    g.close()


def test_dead_end_near_dedupes_semantic_rewording(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    first = g.add_dead_end(
        actor="cli-a", reason="web login sqli route failed after three probes")
    second = g.add_dead_end(
        actor="cli-b", reason="web login sqli route failed after 3 probes")

    assert first > 0
    assert second == -1
    assert len([e for e in g.events() if e["kind"] == "dead_end"]) == 1
    g.close()


def test_candidate_fact_cap_per_source_route(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    for i in range(20):
        assert g.add_evidence(
            actor="cli-a", source="claude", fact=f"candidate {i}",
            verified=False, route_hash="web:login:sqli",
        ) > 0

    dropped = g.add_evidence(
        actor="cli-a", source="claude", fact="candidate overflow",
        verified=False, route_hash="web:login:sqli",
    )
    other_route = g.add_evidence(
        actor="cli-a", source="claude", fact="candidate other route",
        verified=False, route_hash="web:upload:svg",
    )

    assert dropped == -1
    assert other_route > 0
    assert len([e for e in g.events() if e["kind"] == "fact_added"]) == 21
    g.close()


# ── A+J: fact lifecycle (reject/merge/supersede) + intent dispatch_state ──────

def test_reject_fact_leaves_snapshot(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    f1 = g.add_evidence(actor="s1", source="x", fact="good", verified=True)
    f2 = g.add_evidence(actor="s2", source="x", fact="bad", verified=True)
    g.reject_fact(actor="rev", fact_seq=f2, reason="proven false")
    facts = [e.fact for e in g.snapshot().evidence]
    assert "good" in facts and "bad" not in facts
    assert len(g.verified_evidence()) == 1
    retired = g.retired_facts()
    assert any(r["fact_seq"] == f2 and r["state"] == "rejected" for r in retired)
    g.close()


def test_merge_fact_folds_and_retires(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    f1 = g.add_evidence(actor="s1", source="x", fact="canonical", verified=True)
    f2 = g.add_evidence(actor="s2", source="x", fact="dup of canonical", verified=True)
    g.merge_fact(actor="rev", from_fact_seq=f2, to_fact_seq=f1, reason="same finding")
    facts = [e.fact for e in g.snapshot().evidence]
    assert facts == ["canonical"]
    retired = {r["fact_seq"]: r for r in g.retired_facts()}
    assert retired[f2]["state"] == "merged" and retired[f2]["merged_into"] == f1
    g.close()


def test_supersede_fact(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    f1 = g.add_evidence(actor="s1", source="x", fact="old reading", verified=True)
    g.supersede_fact(actor="rev", fact_seq=f1, reason="newer reading replaces it")
    assert g.snapshot().evidence == []
    assert g.active_candidates() == []
    g.close()


def test_challenge_then_revalidate_restores_verified(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    f1 = g.add_evidence(actor="s1", source="x", fact="fact A", verified=True)
    g.challenge_fact(actor="rev", fact_seq=f1, reason="unsure",
                     verification_goal="verify A")
    e = [x for x in g.snapshot().evidence if x.fact == "fact A"][0]
    assert e.verified is False  # challenge de-verifies
    # challenged fact is still an active candidate (under review, not retired)
    assert any(c["fact_seq"] == f1 for c in g.active_candidates())
    g.revalidate_fact(actor="rev", fact_seq=f1, reason="confirmed")
    e = [x for x in g.snapshot().evidence if x.fact == "fact A"][0]
    assert e.verified is True  # revalidate restores the original verdict
    g.close()


def test_review_fact_dispatch(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    f1 = g.add_evidence(actor="s1", source="x", fact="claim", verified=True)
    res = g.review_fact(actor="rev", fact_seq=f1, action="reject", reason="nope")
    assert res["action"] == "reject" and res["seq"] > 0
    assert g.snapshot().evidence == []
    g.close()


def test_resume_intent_not_claimable(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="I1", goal="active dir")
    g.propose_intent(actor="reason", intent_id="I2", goal="held dir")
    g.claim_intent(worker="w1", intent_id="I1")
    # runtime_failure parks active intents as resume (operator_stop now closes them);
    # this test exercises the resume→not-claimable→revive→claimable round-trip.
    out = g.release_claims_for_finalize(reason="runtime_failure")
    # both became resume (I1 released-then-resume, I2 active-then-resume)
    assert "I2" in out["resumed_intents"]
    assert g.open_goal_texts() == []  # nothing active to dispatch
    assert g.claim_intent(worker="w2", intent_id="I2") is False
    revived = g.revive_resume_intents()
    assert "I2" in revived
    assert g.claim_intent(worker="w2", intent_id="I2") is True
    g.close()


def test_finalize_solved_closes_active_intents(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="J1", goal="aaa")
    g.propose_intent(actor="reason", intent_id="J2", goal="bbb")
    out = g.release_claims_for_finalize(reason="solved")
    assert set(out["closed_intents"]) == {"J1", "J2"}
    assert out["resumed_intents"] == []
    assert g.open_goal_texts() == []
    g.close()


def test_conclude_intent_sets_closed_dispatch_state(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.propose_intent(actor="reason", intent_id="K1", goal="ccc")
    g.claim_intent(worker="w1", intent_id="K1")
    g.conclude_intent(actor="w1", intent_id="K1", result="dead end")
    # a closed intent is gone from open_goal_texts and not re-claimable
    assert g.open_goal_texts() == []
    g.close()


def test_lifecycle_survives_reopen_cold_start(tmp_path):
    """Standby HITL cold-start: a re-opened DB still has fact_states/dispatch cols."""
    p = tmp_path / "g.db"
    g = SQLiteSharedGraph.open(db_path=p, challenge=_chal())
    f1 = g.add_evidence(actor="s1", source="x", fact="keep", verified=True)
    f2 = g.add_evidence(actor="s2", source="x", fact="drop", verified=True)
    g.reject_fact(actor="rev", fact_seq=f2, reason="bad")
    g.close()
    g2 = SQLiteSharedGraph.open(db_path=p, challenge=_chal())
    facts = [e.fact for e in g2.snapshot().evidence]
    assert facts == ["keep"]
    g2.close()


# ── E: unified resource locks ────────────────────────────────────────────────

def test_resource_lock_exclusive(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    r1 = g.request_resource_lock(actor="w1", resource_key="destructive:tcp:445@1.2.3.4",
                                 scope="lane", risk_class="destructive", owner_worker="w1")
    assert r1["acquired"]
    r2 = g.request_resource_lock(actor="w2", resource_key="destructive:tcp:445@1.2.3.4",
                                 owner_worker="w2")
    assert r2["acquired"] is False and r2["held_by"] == "w1"
    conflict = g.check_resource_conflicts(
        resource_key="destructive:tcp:445@1.2.3.4", by_worker="w2")
    assert conflict["conflict"] and conflict["blockers"][0]["owner"] == "w1"
    rel = g.release_resource_lock(actor="w1", resource_key="destructive:tcp:445@1.2.3.4",
                                  by_worker="w1")
    assert rel["released"]
    assert g.active_resource_locks() == []
    # released → another worker can now take it
    r3 = g.request_resource_lock(actor="w2", resource_key="destructive:tcp:445@1.2.3.4",
                                 owner_worker="w2")
    assert r3["acquired"]
    g.close()


def test_resource_lock_owner_fenced_release(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    g.request_resource_lock(actor="w1", resource_key="account:svc@dc01", owner_worker="w1")
    # a non-owner cannot release a live lock
    rel = g.release_resource_lock(actor="w2", resource_key="account:svc@dc01", by_worker="w2")
    assert rel["released"] is False and rel.get("held_by") == "w1"
    assert len(g.active_resource_locks()) == 1
    g.close()


# ── H: long-run compaction ───────────────────────────────────────────────────

def test_compact_retires_barren_intents_keeps_facts(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=_chal())
    f1 = g.add_evidence(actor="w", source="x", fact="keep verified", verified=True)
    g.propose_intent(actor="reason", intent_id="I1", goal="barren one")
    g.claim_intent(worker="w1", intent_id="I1")
    g.conclude_intent(actor="w1", intent_id="I1", result="dead end")
    g.propose_intent(actor="reason", intent_id="I2", goal="productive")
    g.claim_intent(worker="w2", intent_id="I2")
    g.conclude_intent(actor="w2", intent_id="I2", result="found x", to_fact_seq=f1)
    info = g.compact_graph(trigger="no_progress_time")
    assert info["retired_intent_ids"] == ["I1"]  # productive (to_fact_seq) survives
    assert any(e.fact == "keep verified" for e in g.snapshot().evidence)
    assert len(g.compact_epochs()) == 1
    g.close()
