"""Multi-flag support — Phase 1: data model + completion predicate.

A challenge can require collecting N distinct flags (Challenge.expected_flags)
before the run is solved. expected_flags=1 (the default) must stay byte-identical
to the old "first flag wins" behaviour. These tests pin the building blocks:
the InsightBus flag set and the swarm's _flags_complete() predicate.
"""

from __future__ import annotations

import asyncio

import pytest

from muteki.models.solve_graph import Challenge
from muteki.swarm.insight_bus import InsightBus, InsightKind
from muteki.swarm.swarm import Swarm


# ── InsightBus: flag set, not single-flag lock ───────────────────────────────

def test_insightbus_collects_distinct_flags_in_order() -> None:
    bus = InsightBus("c1")

    async def go() -> None:
        await bus.flag_found("w1", "flag{a}")
        await bus.flag_found("w2", "flag{b}")
        await bus.flag_found("w1", "flag{a}")  # dup — must not re-add

    asyncio.run(go())
    assert bus.flags == ["flag{a}", "flag{b}"]
    assert bus.flag == "flag{a}"  # back-compat property = first


def test_insightbus_flag_property_none_when_empty() -> None:
    assert InsightBus("c1").flag is None and InsightBus("c1").flags == []


def test_insightbus_flag_insight_reaches_siblings() -> None:
    bus = InsightBus("c1")
    a = bus.subscribe("a")
    b = bus.subscribe("b")

    async def go() -> None:
        await bus.flag_found("a", "flag{x}")  # produced by a

    asyncio.run(go())
    # the producer 'a' does NOT get its own insight; sibling 'b' does
    assert a.empty()
    ins = b.get_nowait()
    assert ins.kind is InsightKind.FLAG and ins.text == "flag{x}"


def test_insightbus_all_flags_found_is_a_distinct_kind() -> None:
    bus = InsightBus("c1")
    b = bus.subscribe("b")

    async def go() -> None:
        await bus.all_flags_found("coordinator", count=3)

    asyncio.run(go())
    ins = b.get_nowait()
    assert ins.kind is InsightKind.ALL_FLAGS_FOUND
    # ALL_FLAGS_FOUND must NOT populate the flag set (it's a stop signal)
    assert bus.flags == []


# ── swarm completion predicate ───────────────────────────────────────────────

class _PredHost:
    """Minimal host exposing just the swarm's flag helpers (no full swarm init)."""
    _expected_flags = Swarm._expected_flags
    _multi_flag = Swarm._multi_flag
    _flags_complete = Swarm._flags_complete
    _record_flags = Swarm._record_flags

    def __init__(self, challenge: Challenge) -> None:
        self.challenge = challenge
        self._found_flags: list[str] = []


def test_flags_complete_single_flag_stops_immediately() -> None:
    # expected_flags=1 (default): the first flag completes the run — byte-identical
    # to the legacy "first flag wins".
    h = _PredHost(Challenge(id="c", name="n", category="web"))
    assert h._flags_complete() is False
    assert h._record_flags("flag{a}") == ["flag{a}"]
    assert h._flags_complete() is True


def test_flags_complete_waits_for_all_n() -> None:
    h = _PredHost(Challenge(id="c", name="n", category="web", expected_flags=3))
    assert h._record_flags("flag{a}") == ["flag{a}"]
    assert h._flags_complete() is False  # 1/3
    assert h._record_flags("flag{a}", "flag{b}") == ["flag{b}"]  # dup skipped
    assert h._flags_complete() is False  # 2/3
    assert h._record_flags("flag{c}") == ["flag{c}"]
    assert h._flags_complete() is True   # 3/3


def test_record_flags_dedups_and_skips_none() -> None:
    h = _PredHost(Challenge(id="c", name="n", category="web", expected_flags=2))
    assert h._record_flags("flag{a}", None, "flag{a}", "") == ["flag{a}"]
    assert h._found_flags == ["flag{a}"]


def test_expected_flags_clamps_below_one() -> None:
    # a bogus expected_flags <= 0 must not make _flags_complete() never true.
    h = _PredHost(Challenge(id="c", name="n", category="web", expected_flags=0))
    assert h._expected_flags() == 1
    h._record_flags("flag{a}")
    assert h._flags_complete() is True


# ── multi_flag mode: decouple SAVE from FINISH (run-10070) ───────────────────

def test_collect_mode_unknown_count_saves_but_never_finishes() -> None:
    # multi_flag + no count (expected_flags<=1): a saved flag does NOT finish the
    # run (the literal "save != finish"). Flags still accumulate; only operator STOP
    # / no-progress pause ends it.
    h = _PredHost(Challenge(id="c", name="n", category="web", multi_flag=True))
    assert h._flags_complete() is False
    h._record_flags("W3lc0m3T0Gh0st")
    assert h._flags_complete() is False          # saved, NOT finished
    h._record_flags("D4shIsN0tAFl4g", "H1dd3nInSh4dow")
    assert len(h._found_flags) == 3
    assert h._flags_complete() is False          # still collecting


def test_collect_mode_with_count_finishes_at_n() -> None:
    # multi_flag + explicit N: finishes once N distinct flags collected.
    h = _PredHost(Challenge(id="c", name="n", category="web",
                            multi_flag=True, expected_flags=3))
    h._record_flags("a1b2c3d4")
    assert h._flags_complete() is False           # 1/3
    h._record_flags("e5f6g7h8", "i9j0k1l2")
    assert h._flags_complete() is True            # 3/3


def test_single_flag_mode_unchanged_by_multi_flag_field() -> None:
    # multi_flag=False (default): byte-identical first-flag-wins, regardless of the
    # new field existing.
    h = _PredHost(Challenge(id="c", name="n", category="web"))   # multi_flag default False
    assert h.challenge.multi_flag is False
    h._record_flags("flag{a}")
    assert h._flags_complete() is True            # first flag still finishes


# ── web Run model (Phase 4) ──────────────────────────────────────────────────

def _bare_run():
    """A Run with stubbed deps — we only exercise the in-memory flag bookkeeping."""
    from apps.web.run_manager import Run
    return Run(run_id="r1", bus=None, cost=None, store=None)  # type: ignore[arg-type]


def test_run_merge_flags_accumulates_and_dedups() -> None:
    r = _bare_run()
    r.merge_flags("flag{a}")
    r.merge_flags(["flag{b}", "flag{a}"])  # dup skipped
    assert r.flags == ["flag{a}", "flag{b}"]
    assert r.flag == "flag{a}"  # first, back-compat


def test_run_summary_carries_flags() -> None:
    r = _bare_run()
    r.expected_flags = 3
    r.merge_flags(["flag{a}", "flag{b}"])
    s = r.summary()
    assert s["flags"] == ["flag{a}", "flag{b}"]
    assert s["expected_flags"] == 3
    assert s["flag"] == "flag{a}"  # back-compat field still present


# ── shared-graph snapshot (Phase 5) ──────────────────────────────────────────

def _shared_graph(tmp_path, expected_flags=2):
    from muteki.swarm.shared_graph import SQLiteSharedGraph
    from muteki.solver.result import ArtifactStore
    ch = Challenge(id="c", name="n", category="web", expected_flags=expected_flags)
    return SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=ch,
                                  artifacts=ArtifactStore(root=tmp_path / "arts"))


def test_snapshot_accumulates_flags_not_last_wins(tmp_path) -> None:
    g = _shared_graph(tmp_path)
    g.flag_found(actor="w1", flag="flag{a}")
    g.flag_found(actor="w2", flag="flag{b}")
    snap = g.snapshot()
    # was a last-wins overwrite (would be just flag{b}); now accumulates both.
    assert snap.flags == ["flag{a}", "flag{b}"]
    assert snap.flag == "flag{a}"


def test_reopen_after_false_positive_drops_only_that_flag(tmp_path) -> None:
    g = _shared_graph(tmp_path)
    g.flag_found(actor="w1", flag="flag{a}")
    g.flag_found(actor="w2", flag="flag{b}")
    g.reopen_after_false_positive(actor="operator", flag="flag{a}")
    snap = g.snapshot()
    assert snap.flags == ["flag{b}"]  # only the false one removed; flag{b} kept
    assert snap.flag == "flag{b}"


def test_reopen_after_false_positive_reopens_only_linked_solved_intent(tmp_path) -> None:
    """When flag_found records the producing intent, a per-flag false-positive
    reopens only that intent instead of every solved direction."""
    g = _shared_graph(tmp_path, expected_flags=2)
    g.propose_intent(actor="reason", intent_id="I-a", goal="get flag a")
    g.propose_intent(actor="reason", intent_id="I-b", goal="get flag b")
    g.conclude_intent(actor="cli-a", intent_id="I-a", result="solved")
    g.conclude_intent(actor="cli-b", intent_id="I-b", result="solved")
    g.flag_found(actor="cli-a", flag="flag{a}", intent_id="I-a")
    g.flag_found(actor="cli-b", flag="flag{b}", intent_id="I-b")

    info = g.reopen_after_false_positive(actor="operator", flag="flag{b}")

    assert info["reopened"] == ["I-b"]
    with g._lock:
        rows = dict(g._conn.execute(
            "SELECT intent_id, status FROM intents WHERE intent_id IN ('I-a','I-b')"
        ).fetchall())
    assert rows == {"I-a": "done", "I-b": "open"}


# ── run-75379 BUG③: a false-positive flag stays PERMANENTLY rejected ──────────
# Codex's localization: the DB flag row already dedups on the permanent
# `flag::<value>` key, so a second DB row is impossible — but a reopened worker can
# re-emit the LIVE broadcast + mutate in-memory state. So the durable reject memory
# must (1) materialize into the snapshot, (2) be readable by the acceptance gate.

def test_snapshot_materializes_rejected_flags(tmp_path) -> None:
    """An invalidated flag lands in snapshot.rejected_flags (durable reject memory),
    not just dropped from .flags — so a fresh reader can refuse to re-accept it."""
    g = _shared_graph(tmp_path)
    g.flag_found(actor="w1", flag="flag{a}")
    g.flag_found(actor="w2", flag="flag{b}")
    g.reopen_after_false_positive(actor="operator", flag="flag{a}")
    snap = g.snapshot()
    assert snap.flags == ["flag{b}"]              # bad one gone from the live set
    assert snap.rejected_flags == ["flag{a}"]     # but remembered permanently
    assert g.invalidated_flags() == {"flag{a}"}   # gate reads the same source


def test_rejected_flag_stays_rejected_when_refound_after_reopen(tmp_path) -> None:
    """The invalidate→reopen→re-find loop: a worker re-emits the bad flag AFTER it was
    invalidated (the reopened intent re-runs and re-derives it). snapshot must NOT let
    it back into .flags — add_flag refuses anything in rejected_flags, regardless of
    EV_FLAG_FOUND replay order."""
    g = _shared_graph(tmp_path)
    g.flag_found(actor="w1", flag="flag{a}")
    g.reopen_after_false_positive(actor="operator", flag="flag{a}")
    # a reopened worker re-finds the SAME value and writes flag_found again. The DB
    # dedups the row (same flag::value key), but even a brand-new flag_found event for
    # this value must not resurrect it in the snapshot.
    g.flag_found(actor="w1-respawn", flag="flag{a}")
    snap = g.snapshot()
    assert "flag{a}" not in snap.flags
    assert snap.flags == []
    assert "flag{a}" in snap.rejected_flags


def test_solvegraph_add_flag_refuses_rejected_value() -> None:
    """SolveGraph.add_flag is the in-memory gate: once a value is rejected it can't be
    re-added, even on a fresh graph that replays flag_found after the rejection."""
    from muteki.models.solve_graph import SolveGraph
    g = SolveGraph(challenge=Challenge(id="c", name="n", category="web"))
    assert g.add_flag("flag{a}") is True
    g.reject_flag("flag{a}")
    assert g.flags == [] and g.flag is None
    assert g.rejected_flags == ["flag{a}"]
    # re-adding the rejected value is refused (the re-derivation case)
    assert g.add_flag("flag{a}") is False
    assert g.flags == []
    # reject is idempotent and order-independent: rejecting BEFORE any add also sticks
    g2 = SolveGraph(challenge=Challenge(id="c", name="n", category="web"))
    g2.reject_flag("flag{x}")
    assert g2.add_flag("flag{x}") is False and g2.flags == []


# ── distill (Phase 5) ────────────────────────────────────────────────────────

def test_distill_sanitizes_all_flags() -> None:
    from muteki.learning.distill import distill
    from muteki.models.solve_graph import SolveGraph
    g = SolveGraph(challenge=Challenge(id="c", name="n", category="web",
                                       expected_flags=2))
    g.add_flag("flag{secret1}")
    g.add_flag("flag{secret2}")
    g.add_evidence("w", "got flag{secret1} from /a and flag{secret2} from /b")
    tpl = distill(g)
    blob = " ".join(tpl.steps) + " ".join(tpl.evidence_chain)
    # neither flag may leak into the reusable template (both sanitized to <FLAG>)
    assert "flag{secret1}" not in blob and "flag{secret2}" not in blob
    assert "<FLAG>" in blob


def test_run_reopened_per_flag_drops_only_the_bad_one() -> None:
    # mark_false emits RUN_REOPENED{flag: bad}; the sink must drop ONLY that flag
    # from run.flags, keeping the survivors (per-flag, not wipe-all).
    from apps.web.run_manager import RunManager
    from muteki.core.events import Event, EventType
    import asyncio

    async def go():
        mgr = RunManager.__new__(RunManager)  # we only need the sink wiring on a run
        mgr._seq = 0
        run = _bare_run()
        run.flags = ["flag{a}", "flag{b}", "flag{c}"]
        run.flag = "flag{a}"
        run.solved = True
        run.finished = True
        # replicate the meta-sink's RUN_REOPENED handling
        sink = mgr._meta_sink_for(run)
        await sink(Event(event_type=EventType.RUN_REOPENED, run_id="r1",
                         payload={"flag": "flag{b}"}))
        return run

    run = asyncio.run(go())
    assert run.flags == ["flag{a}", "flag{c}"]   # only flag{b} dropped
    assert run.flag == "flag{a}" and run.solved is False and run.finished is False
