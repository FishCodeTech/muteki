from __future__ import annotations

import time
from pathlib import Path

import pytest

from muteki.models.solve_graph import Challenge
from muteki.swarm.shared_graph import SQLiteSharedGraph


LANE = "destructive:tcp:445@10.0.0.5"


def _graph(tmp_path: Path) -> SQLiteSharedGraph:
    return SQLiteSharedGraph(
        tmp_path / "lease-control.db",
        Challenge(id="run-lease", name="lease", category="pwn"),
    )


def _seed_all_lease_kinds(g: SQLiteSharedGraph, *, worker: str = "cli-claude#3",
                          intent: str = "I-freeze") -> None:
    g.propose_intent(
        actor="reason", intent_id=intent, goal="exercise every lease kind",
        payload={"lane_key": LANE},
    )
    assert g.claim_intent(worker=worker, intent_id=intent, lease_s=900)
    g.save_poc(
        actor=worker, poc_id="P-freeze", path="poc.py",
        entry_command="python poc.py", intent_id=intent,
    )
    assert g.claim_poc(worker=worker, poc_id="P-freeze", lease_s=900)
    assert g.try_claim_activity(worker=worker, key="nmap:10.0.0.5", lease_s=900)
    assert g.lock_lane(
        actor=worker, lane_key=LANE, risk_class="destructive",
        owner_worker=worker, owner_intent=intent, lease_s=900,
    )["acquired"]
    assert g.request_resource_lock(
        actor=worker, resource_key=LANE, scope="lane",
        owner_worker=worker, owner_intent=intent, lease_s=900,
    )["acquired"]


def _deadlines(g: SQLiteSharedGraph) -> dict[str, float]:
    queries = {
        "intent": "SELECT lease_until FROM intents WHERE intent_id='I-freeze'",
        "poc": "SELECT lease_until FROM pocs WHERE poc_id='P-freeze'",
        "activity": (
            "SELECT lease_until FROM activity_locks "
            "WHERE activity_key='nmap:10.0.0.5'"
        ),
        "lane": "SELECT lease_until FROM lane_locks WHERE lane_key=?",
        "resource": "SELECT lease_until FROM resource_locks WHERE lock_id=?",
    }
    out: dict[str, float] = {}
    with g._lock:
        for kind, sql in queries.items():
            params: tuple[str, ...] = ()
            if kind == "lane":
                params = (LANE,)
            elif kind == "resource":
                params = (f"rl-{LANE}",)
            row = g._conn.execute(sql, params).fetchone()
            assert row and row[0] is not None
            out[kind] = float(row[0])
    return out


def test_freeze_guard_then_thaw_uses_actual_duration_for_all_leases(tmp_path: Path):
    g = _graph(tmp_path)
    _seed_all_lease_kinds(g)
    before = _deadlines(g)
    frozen_at = time.time()

    suspended = g.suspend_active_leases(
        actor="control", suspension_id="cmd-freeze-1",
        scope_kind="run", scope_id="web-run-1",
        guard_s=600, observed_at=frozen_at,
    )
    assert suspended["affected"] == 5
    guarded = _deadlines(g)
    for kind in before:
        assert guarded[kind] == pytest.approx(before[kind] + 600)

    resumed = g.resume_suspended_leases(
        actor="control", suspension_id="cmd-freeze-1",
        resumed_at=frozen_at + 37,
    )
    assert resumed["duration_s"] == 37
    assert resumed["affected"] == 5
    assert resumed["skipped"] == []
    after = _deadlines(g)
    for kind in before:
        # The unused 563-second guard is removed; only real frozen time remains.
        assert after[kind] == pytest.approx(before[kind] + 37)

    lease_events = [e for e in g.events() if e["kind"].startswith("leases_")]
    assert [e["kind"] for e in lease_events] == [
        "leases_suspended", "leases_resumed",
    ]
    assert lease_events[0]["payload"]["affected"] == 5
    assert lease_events[1]["payload"]["suspended_seq"] == lease_events[0]["seq"]

    # Command retries are idempotent and do not move deadlines or append again.
    again = g.resume_suspended_leases(
        actor="control", suspension_id="cmd-freeze-1", resumed_at=frozen_at + 90,
    )
    assert again["idempotent"] is True
    assert _deadlines(g) == after
    assert len([e for e in g.events() if e["kind"] == "leases_resumed"]) == 1


def test_scope_and_owner_fences_prevent_unrelated_or_reacquired_lease_shift(tmp_path: Path):
    g = _graph(tmp_path)
    _seed_all_lease_kinds(g, worker="cli-claude#3")
    before = _deadlines(g)
    frozen_at = time.time()

    # Engine scope uses the cli-<engine> boundary and reaches every owned kind.
    out = g.suspend_active_leases(
        actor="control", suspension_id="cmd-engine-freeze",
        scope_kind="engine", scope_id="claude", guard_s=300,
        observed_at=frozen_at,
    )
    assert out["affected"] == 5

    # One guarded resource is explicitly released and re-acquired by a new owner.
    assert g.release_resource_lock(
        actor="cli-claude#3", resource_key=LANE, by_worker="cli-claude#3",
    )["released"]
    assert g.request_resource_lock(
        actor="cli-codex-2", resource_key=LANE, scope="lane",
        owner_worker="cli-codex-2", owner_intent="I-other", lease_s=700,
    )["acquired"]
    with g._lock:
        new_owner_deadline = float(g._conn.execute(
            "SELECT lease_until FROM resource_locks WHERE lock_id=?",
            (f"rl-{LANE}",),
        ).fetchone()[0])

    resumed = g.resume_suspended_leases(
        actor="control", suspension_id="cmd-engine-freeze",
        resumed_at=frozen_at + 20,
    )
    assert resumed["affected"] == 4
    assert resumed["skipped"] == [
        {"kind": "resource", "key": f"rl-{LANE}",
         "reason": "owner_or_lease_changed"},
    ]
    after = _deadlines(g)
    for kind in {"intent", "poc", "activity", "lane"}:
        assert after[kind] == pytest.approx(before[kind] + 20)
    assert after["resource"] == new_owner_deadline


def test_single_phase_shift_is_atomic_scoped_and_idempotent(tmp_path: Path):
    g = _graph(tmp_path)
    _seed_all_lease_kinds(g)
    before = _deadlines(g)

    shifted = g.shift_active_leases(
        actor="control", operation_id="cmd-thaw-9", delta_s=12.5,
        scope_kind="intent", scope_id="I-freeze", observed_at=time.time(),
    )
    # Intent scope maps to intent + PoC + lane + resource; activity has no intent.
    assert shifted["affected"] == 4
    after = _deadlines(g)
    for kind in {"intent", "poc", "lane", "resource"}:
        assert after[kind] == pytest.approx(before[kind] + 12.5)
    assert after["activity"] == before["activity"]
    event = [e for e in g.events() if e["kind"] == "leases_shifted"]
    assert len(event) == 1
    assert event[0]["payload"]["scope"] == {"kind": "intent", "id": "I-freeze"}

    retry = g.shift_active_leases(
        actor="control", operation_id="cmd-thaw-9", delta_s=999,
        scope_kind="global", observed_at=time.time(),
    )
    assert retry["idempotent"] is True
    assert _deadlines(g) == after
    with pytest.raises(ValueError, match="non-negative"):
        g.shift_active_leases(actor="control", delta_s=-1)


def test_lease_materialization_rolls_back_when_audit_append_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
):
    g = _graph(tmp_path)
    _seed_all_lease_kinds(g)
    before = _deadlines(g)

    def fail_append(**_kwargs):
        raise RuntimeError("audit unavailable")

    monkeypatch.setattr(g, "_append_lease_event_in_transaction", fail_append)
    with pytest.raises(RuntimeError, match="audit unavailable"):
        g.shift_active_leases(
            actor="control", operation_id="cmd-no-audit", delta_s=45,
            scope_kind="global", observed_at=time.time(),
        )

    assert _deadlines(g) == before
    assert not [e for e in g.events() if e["kind"] == "leases_shifted"]
