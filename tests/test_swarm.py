"""Swarm + Insight Bus mechanics.

The execution layer is now CLI-only (the code-driven scripted-LLM path was
retired), so these tests exercise the swarm's coordination machinery directly:
the InsightBus fan-out, the CLI race lineup + degrade logic, and the coordinator's
plan / dispatch loop — all with the CLI subprocess stubbed out (no real engine).
"""

import asyncio
import time
from pathlib import Path

import pytest

from muteki.core.llm import ModelSpec
from muteki.models.solve_graph import Challenge
from muteki.sandbox.manager import SandboxManager
from muteki.solver.result import ArtifactStore
from muteki.solver.types import SolverConfig
from muteki.swarm.insight_bus import InsightBus, InsightKind
from muteki.swarm.swarm import Swarm


@pytest.fixture
def challenge() -> Challenge:
    return Challenge(id="c-swarm", name="swarm-test", category="web", points=50,
                     description="solve me", flag_format=r"flag\{[^}]+\}")


@pytest.fixture(autouse=True)
def _reset_health_probe_cache():
    """The health-probe cache is process-wide (so sibling runs share verdicts in
    production). In tests that means a stubbed roster could leak across cases — clear
    it before AND after each test so verdicts never bleed."""
    from muteki.swarm import swarm as _swarm_mod
    _swarm_mod._health_cache_clear()
    yield
    _swarm_mod._health_cache_clear()


# ── InsightBus: cross-solver fact/dead-end sharing ───────────────────────────

async def test_insight_bus_fan_out_excludes_producer() -> None:
    bus = InsightBus("c1")
    qa = bus.subscribe("A")
    qb = bus.subscribe("B")
    await bus.fact("A", "service is nginx 1.18")
    # B receives it, A does not get its own
    assert qb.get_nowait().text == "service is nginx 1.18"
    assert qa.empty()


async def test_insight_bus_backlog_for_late_subscriber() -> None:
    bus = InsightBus("c1")
    bus.subscribe("A")
    await bus.fact("A", "leaked cred admin:hunter2")
    await bus.dead_end("A", "SQLi on /login is patched")
    # C joins late -> gets the backlog (both, since neither was produced by C)
    qc = bus.subscribe("C")
    got = [qc.get_nowait() for _ in range(2)]
    kinds = {g.kind for g in got}
    assert InsightKind.FACT in kinds and InsightKind.DEAD_END in kinds


# ── CLI executor: race lineup + degrade (no real subprocess) ─────────────────

def _cli_swarm(challenge, tmp_path, *, cli_race, healthy, **kw):
    """Build a Swarm in CLI-executor mode with healthchecks stubbed, then build
    its solver lineup. `healthy` is the set of engine names whose probe passes."""
    import muteki.solver.cli_driver as cd
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    swarm = Swarm(
        challenge, [ModelSpec(solver_id="seat", model="mock")],
        llm=None, sandbox=sandbox, artifacts=arts,
        executor="cli", cli_race=cli_race, **kw,
    )
    # _build_solvers routes its liveness check through the cached probe path
    # (_probe_engine_health → driver.health_detail). Stub BOTH the bool healthcheck
    # and the (ok, detail) health_detail so either entrypoint honours `healthy`, and
    # disable the probe cache so a stubbed roster can't leak a stale verdict.
    swarm._health_probe_ttl = 0
    orig_hc = {n: cd.DRIVERS[n].healthcheck for n in cd.DRIVERS}
    orig_hd = {n: cd.DRIVERS[n].health_detail for n in cd.DRIVERS}
    for n, drv in cd.DRIVERS.items():
        drv.healthcheck = (  # type: ignore[method-assign]
            lambda *a, n=n, **k: n in healthy)
        drv.health_detail = (  # type: ignore[method-assign]
            lambda *a, n=n, **k: (True, "") if n in healthy else (False, f"down ({n})"))
    try:
        return swarm._build_solvers()
    finally:
        for n, drv in cd.DRIVERS.items():
            drv.healthcheck = orig_hc[n]  # type: ignore[method-assign]
            drv.health_detail = orig_hd[n]  # type: ignore[method-assign]


def test_cli_race_builds_both_engines(challenge, tmp_path: Path) -> None:
    solvers = _cli_swarm(challenge, tmp_path, cli_race=True,
                         healthy={"claude", "codex"})
    engines = sorted(s.driver.name for s in solvers)
    assert engines == ["claude", "codex"]
    # distinct ids so the swarm's winner bookkeeping + deck can tell them apart
    assert len({s.solver_id for s in solvers}) == 2


def test_cli_race_degrades_when_codex_unhealthy(challenge, tmp_path: Path) -> None:
    # codex usage-limited → race collapses to claude alone, swarm still runs.
    solvers = _cli_swarm(challenge, tmp_path, cli_race=True, healthy={"claude"})
    assert [s.driver.name for s in solvers] == ["claude"]


def test_cli_single_engine_degrades_to_claude(challenge, tmp_path: Path) -> None:
    solvers = _cli_swarm(challenge, tmp_path, cli_race=False, healthy={"claude"},
                         cli_engine="codex")  # asked for codex, but it's down
    assert [s.driver.name for s in solvers] == ["claude"]


def test_cli_offline_and_no_kb_propagate(challenge, tmp_path: Path) -> None:
    solvers = _cli_swarm(challenge, tmp_path, cli_race=False, healthy={"claude"},
                         web_access=False, kb=False)
    s = solvers[0]
    assert s.web_access is False and s.kb is False


def test_cli_race_three_engines(challenge, tmp_path: Path) -> None:
    # the full roster races: cursor + claude + codex, one worker each.
    solvers = _cli_swarm(challenge, tmp_path, cli_race=True,
                         healthy={"cursor", "claude", "codex"},
                         engines=["cursor", "claude", "codex"])
    engines = sorted(s.driver.name for s in solvers)
    assert engines == ["claude", "codex", "cursor"]
    assert len({s.solver_id for s in solvers}) == 3


def test_cli_race_drops_unhealthy_cursor(challenge, tmp_path: Path) -> None:
    # cursor down (e.g. not logged in) → race runs claude + codex only.
    solvers = _cli_swarm(challenge, tmp_path, cli_race=True,
                         healthy={"claude", "codex"},
                         engines=["cursor", "claude", "codex"])
    assert sorted(s.driver.name for s in solvers) == ["claude", "codex"]


def test_engines_default_preserves_claude_codex(challenge, tmp_path: Path) -> None:
    # no engines passed → historical claude+codex roster (back-compat).
    solvers = _cli_swarm(challenge, tmp_path, cli_race=True,
                         healthy={"cursor", "claude", "codex"})
    assert sorted(s.driver.name for s in solvers) == ["claude", "codex"]


def test_engines_roster_deduped(challenge, tmp_path: Path) -> None:
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    sw = Swarm(challenge, [ModelSpec(solver_id="seat", model="mock")],
               llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
               engines=["cursor", "claude", "cursor", "codex", "claude"])
    assert sw.engines == ["cursor", "claude", "codex"]


# ── single-engine multi-instance: N same-engine profiles, distinct models ─────
# (P1 §10: "drag out 3 codex, each pinned to a different model; race them
# concurrently; dispatch later phases by priority".)

def _codex_trio_profiles():
    """Three codex profiles (same base engine), each a distinct model, with
    priorities deliberately OUT of declaration order so an order-preserving
    roster would NOT be priority-sorted. id == name (normalize copies one to the
    other)."""
    mk = lambda pid, model, prio: {
        "id": pid, "name": pid, "engine": "codex", "runtime": "local",
        "credential_account": "codex-main", "auth": "subscription",
        "roles": ["race", "bootstrap", "explore", "review"],
        "race": True, "max_running": 1, "priority": prio, "model": model,
        "enabled": True,
    }
    # declaration order a,b,c but priorities 30,10,20 → priority order is b,c,a
    return [mk("codex-a", "gpt-5.3", 30),
            mk("codex-b", "gpt-5.5", 10),
            mk("codex-c", "gpt-5.4", 20)]


def _build_profile_race(challenge, tmp_path, profiles, *, healthy_base):
    """Build a cli_race Swarm over worker_profiles and return _build_solvers().

    NOTE: with profiles, _build_solvers' liveness check goes through
    _probe_engine_health → driver_for(profile).health_detail. driver_for(profile)
    returns a ProfileDriver that INHERITS health_detail from CliDriver (it doesn't
    delegate to the DRIVERS-dict instance), so stubbing DRIVERS[...].health_detail
    (as _cli_swarm does for bare engine names) would be BYPASSED and a real CLI
    hello would run. So we stub at the swarm method level instead: the healthcheck
    just consults the base-engine name against `healthy_base`."""
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    swarm = Swarm(
        challenge, [ModelSpec(solver_id="seat", model="mock")],
        llm=None, sandbox=sandbox, artifacts=arts,
        executor="cli", cli_race=True, worker_profiles=profiles,
    )
    from muteki.solver.worker_profiles import base_engine_for_profile
    def _stub_probe(name, role):
        prof = swarm._profile_for_engine(name, role=role, advance=False)
        base = base_engine_for_profile(prof or name)
        ok = base in healthy_base
        return (ok, "" if ok else f"down ({base})")
    swarm._health_probe_ttl = 0
    swarm._probe_engine_health = _stub_probe  # type: ignore[assignment]
    return swarm._build_solvers()


def test_cli_race_same_engine_trio_distinct_solver_ids(challenge, tmp_path: Path) -> None:
    # THE BUG FIX (§10.8 Step 2): the classic cli_race path (_build_solvers,
    # which bypasses _make_cli_worker) used to give every same-base-engine racer
    # the same solver_id "cli-codex" → event lanes / account maps overwrite each
    # other. Now each gets a unique id via the _label_seq scheme.
    solvers = _build_profile_race(challenge, tmp_path,
                                  _codex_trio_profiles(), healthy_base={"codex"})
    # all three actually spawned, one worker per profile
    assert len(solvers) == 3
    assert all(s.driver.name == "codex" for s in solvers)
    # distinct solver_ids — no collapse
    ids = [s.solver_id for s in solvers]
    assert len(set(ids)) == 3, ids
    # first keeps the bare id (winner bookkeeping / back-compat), rest get -2/-3
    assert set(ids) == {"cli-codex", "cli-codex-2", "cli-codex-3"}, ids
    # each worker carries its profile's distinct model down to the driver
    models = sorted(s.driver._model() for s in solvers)
    assert models == ["gpt-5.3", "gpt-5.4", "gpt-5.5"], models


def test_engines_roster_sorted_by_priority(challenge, tmp_path: Path) -> None:
    # THE BUG FIX (§10.8 Step 3): self.engines drives dispatch order
    # (_pick_engine walks it and prefers the first not-running candidate), so it
    # must be priority-sorted. Declaration order is a,b,c but priorities are
    # 30,10,20 → expect b(10), c(20), a(30).
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    sw = Swarm(challenge, [ModelSpec(solver_id="seat", model="mock")],
               llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
               worker_profiles=_codex_trio_profiles())
    assert sw.engines == ["codex-b", "codex-c", "codex-a"], sw.engines
    # race_engines (derived from self.engines when not given explicitly) inherits
    # the priority order too.
    assert sw.race_engines == ["codex-b", "codex-c", "codex-a"], sw.race_engines


def test_pick_engine_honors_priority_order(challenge, tmp_path: Path) -> None:
    # The priority sort only matters if the DISPATCHER actually honors it. This
    # proves _pick_engine returns the priority-TOP instance when all are idle.
    # NOTE the real semantics (§10.8 "语义澄清"): _running_count_for_candidate
    # aggregates by BASE ENGINE, so once ANY codex instance runs, the remaining
    # same-engine instances are NOT priority-distinguished (heterogeneity-first
    # treats them as "the codex slot, already covered"). Priority is authoritative
    # exactly when the contenders are idle — which is the all-idle pick below and
    # the classic-race lineup order (test_engines_roster_sorted_by_priority).
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    sw = Swarm(challenge, [ModelSpec(solver_id="seat", model="mock")],
               llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
               worker_profiles=_codex_trio_profiles())
    healthy = ["codex-a", "codex-b", "codex-c"]
    # _pick_engine(running_engines, healthy, *, role)
    # all idle → top priority (codex-b, prio 10) picked first
    assert sw._pick_engine([], healthy, role="bootstrap") == "codex-b"
    # all-idle pick is deterministic regardless of declaration order: priority wins
    assert sw._pick_engine([], list(reversed(healthy)), role="bootstrap") == "codex-b"


def test_priority_zero_is_highest_not_demoted(challenge, tmp_path: Path) -> None:
    # REGRESSION (GPT-5.5 §10 impl review): `int(priority or 100)` turned a legal
    # priority 0 (highest precedence, reachable via hand-edited JSON / API import —
    # coerce_nonneg_int keeps 0) into 100, sinking the top-priority profile. The
    # fix uses coerce_nonneg_int so 0 stays 0 and sorts FIRST.
    mk = lambda pid, prio: {
        "id": pid, "name": pid, "engine": "codex", "runtime": "local",
        "credential_account": "codex-main", "auth": "subscription",
        "roles": ["race", "bootstrap"], "race": True, "max_running": 1,
        "priority": prio, "model": "gpt-5.5", "enabled": True,
    }
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    # zero-prio declared LAST; must still sort first (not demoted to 100).
    sw = Swarm(challenge, [ModelSpec(solver_id="seat", model="mock")],
               llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
               worker_profiles=[mk("codex-ten", 10), mk("codex-zero", 0)])
    assert sw.engines == ["codex-zero", "codex-ten"], sw.engines


def test_container_unavailable_falls_back_local_on_host(challenge, tmp_path: Path) -> None:
    # P2-v3 BLOCKER-d: on a BARE HOST (not in the web container), an unavailable
    # container backend keeps the historical silent fallback to a local worker.
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    sw = Swarm(challenge, [ModelSpec(solver_id="seat", model="mock")],
               llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
               worker_backend="container")
    sw._container_unavailable = True
    import muteki.swarm.swarm as swmod
    # force host semantics regardless of where the test actually runs
    monkey = pytest.MonkeyPatch()
    monkey.setattr(swmod, "is_web_container", lambda: False)
    try:
        assert sw._backend_for_engine("codex") == "local"  # degrades, no raise
    finally:
        monkey.undo()


def test_container_unavailable_hard_fails_in_web_container(challenge, tmp_path: Path) -> None:
    # P2-v3 BLOCKER-d: INSIDE the web container, an unavailable container backend
    # must HARD-FAIL — never silently launch a host-native CLI in the web container.
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    sw = Swarm(challenge, [ModelSpec(solver_id="seat", model="mock")],
               llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
               worker_backend="container")
    sw._container_unavailable = True
    import muteki.swarm.swarm as swmod
    monkey = pytest.MonkeyPatch()
    monkey.setattr(swmod, "is_web_container", lambda: True)
    try:
        with pytest.raises(RuntimeError, match="refusing to fall back"):
            sw._backend_for_engine("codex")
    finally:
        monkey.undo()


def test_engines_priority_sort_only_when_profiles(challenge, tmp_path: Path) -> None:
    # GUARD: the priority sort is scoped to worker_profiles. A bare engine-name
    # roster (no profiles) must keep its given order untouched — this is what
    # test_engines_roster_deduped relies on and what the historical race lineup
    # expects.
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    sw = Swarm(challenge, [ModelSpec(solver_id="seat", model="mock")],
               llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
               engines=["cursor", "claude", "codex"])
    assert sw.engines == ["cursor", "claude", "codex"]


def test_cli_single_mode_preserves_lineup_label(challenge, tmp_path: Path) -> None:
    # GUARD: the §10.8 Step 2 fix is race-mode-only. Single mode must still take
    # its solver_id from the lineup spec (solver_label stays None), not get
    # overridden to "cli-<engine>".
    solvers = _cli_swarm(challenge, tmp_path, cli_race=False, healthy={"claude"},
                         cli_engine="claude")
    assert len(solvers) == 1
    # the lineup ModelSpec passed by _cli_swarm has solver_id="seat"
    assert solvers[0].solver_id == "seat", solvers[0].solver_id


# ── _healthy_engines: silent-degrade NO LONGER silent ────────────────────────
# An engine dropped from the roster by a dispatch-time health-check failure now
# emits an `engine_degraded` blackboard delta (with the failure REASON) so the
# operator sees WHY it never showed up — instead of it vanishing from the panel.

def _bus_health_swarm(challenge, tmp_path, *, healthy: dict[str, bool]):
    """Coordinator swarm wired to a real EventBus, with each engine's
    health_detail() stubbed from `healthy` (name -> ok). Returns (swarm, events)
    where events is a live-appended list of every emitted Event."""
    import muteki.solver.cli_driver as cd
    from muteki.core.event_bus import EventBus
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    bus = EventBus()
    sw = Swarm(
        challenge, [ModelSpec(solver_id="seat", model="mock")],
        llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
        coordinator=True, race_scout=False, bus=bus,
        engines=["cursor", "claude", "codex"],
    )
    # these tests exercise probe → state-change → RE-PROBE semantics (degrade then
    # recover when a stubbed driver flips health). Disable the health-probe cache so
    # each _healthy_engines() call genuinely re-probes the (swapped) drivers.
    sw._health_probe_ttl = 0
    events: list = []
    bus.add_sink(lambda ev: events.append(ev) or _noop())  # sink must be awaitable
    orig = {n: cd.DRIVERS[n].health_detail for n in cd.DRIVERS}
    for n, drv in cd.DRIVERS.items():
        ok = healthy.get(n, True)
        # accept *a/**k — the probe path now calls health_detail(env=...)
        drv.health_detail = (  # type: ignore[method-assign]
            lambda *a, ok=ok, n=n, **k: (True, "") if ok
            else (False, f"Authentication required ({n})"))
    sw._restore_health = lambda: [setattr(cd.DRIVERS[n], "health_detail", orig[n]) for n in orig]
    return sw, events


async def _noop() -> None:
    return None


def _degrade_events(events):
    from muteki.core.events import EventType
    return [e for e in events
            if e.event_type is EventType.BLACKBOARD_DELTA
            and (e.payload or {}).get("kind") == "engine_degraded"]


async def test_healthy_engines_emits_degrade_with_reason(challenge, tmp_path: Path) -> None:
    sw, events = _bus_health_swarm(challenge, tmp_path,
                                   healthy={"cursor": False, "claude": True, "codex": True})
    try:
        roster = sw._healthy_engines()
        await asyncio.sleep(0)  # let the fire-and-forget emit tasks run
    finally:
        sw._restore_health()
    # cursor dropped from the roster; claude + codex remain
    assert sorted(roster) == ["claude", "codex"]
    degr = _degrade_events(events)
    assert len(degr) == 1
    p = degr[0].payload
    assert p["engine"] == "cursor" and p["status"] == "degraded"
    assert "Authentication required" in p["reason"]


async def test_healthy_engines_degrade_deduped(challenge, tmp_path: Path) -> None:
    sw, events = _bus_health_swarm(challenge, tmp_path,
                                   healthy={"cursor": False, "claude": True, "codex": True})
    try:
        sw._healthy_engines()
        sw._healthy_engines()  # same failure twice → still ONE event (no spam)
        await asyncio.sleep(0)
    finally:
        sw._restore_health()
    assert len(_degrade_events(events)) == 1


async def test_healthy_engines_recovery_event(challenge, tmp_path: Path) -> None:
    sw, events = _bus_health_swarm(challenge, tmp_path,
                                   healthy={"cursor": False, "claude": True, "codex": True})
    try:
        sw._healthy_engines()              # cursor down → degraded
        await asyncio.sleep(0)
        # cursor logs back in
        import muteki.solver.cli_driver as cd
        cd.DRIVERS["cursor"].health_detail = lambda *a, **k: (True, "")  # type: ignore[method-assign]
        roster = sw._healthy_engines()     # cursor back → recovered event
        await asyncio.sleep(0)
    finally:
        sw._restore_health()
    assert "cursor" in roster
    degr = _degrade_events(events)
    statuses = [e.payload["status"] for e in degr]
    assert statuses == ["degraded", "recovered"]


async def test_healthy_engines_async_does_not_block_event_loop(
        challenge, tmp_path: Path, monkeypatch) -> None:
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    sw = Swarm(
        challenge, [ModelSpec(solver_id="seat", model="mock")],
        llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
        coordinator=True, race_scout=False, engines=["claude"],
    )

    def slow_probe():
        time.sleep(0.15)
        return ["claude"]

    monkeypatch.setattr(sw, "_healthy_engines", slow_probe)
    ticks = 0
    done = False

    async def ticker():
        nonlocal ticks
        while not done:
            await asyncio.sleep(0)
            ticks += 1

    task = asyncio.create_task(ticker())
    try:
        assert await sw._healthy_engines_async() == ["claude"]
    finally:
        done = True
        await task

    assert ticks > 0


# ── health-probe latency fix: parallel probes + short-TTL cache ──────────────
# The "dispatch freezes for ~a minute" symptom: _healthy_engines shelled a real
# one-turn CLI hello per engine SERIALLY (60–150s timeout each) on the critical
# path before the first worker spawned. These cover the two fixes.

def _probe_swarm(challenge, tmp_path, engines):
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    return Swarm(
        challenge, [ModelSpec(solver_id="seat", model="mock")],
        llm=None, sandbox=sandbox, artifacts=arts, executor="cli",
        coordinator=True, race_scout=False, engines=list(engines),
    )


def test_healthy_engines_probes_run_in_parallel(challenge, tmp_path: Path,
                                                 monkeypatch) -> None:
    # three engines, each probe sleeps 0.3s. SERIAL → ~0.9s; PARALLEL → ~0.3s.
    sw = _probe_swarm(challenge, tmp_path, ["cursor", "claude", "codex"])

    def slow_probe(name, role):
        time.sleep(0.3)
        return True, ""

    monkeypatch.setattr(sw, "_probe_engine_health", slow_probe)
    t0 = time.monotonic()
    roster = sw._healthy_engines()
    elapsed = time.monotonic() - t0
    assert sorted(roster) == ["claude", "codex", "cursor"]
    # generous bound: parallel must finish well under the 0.9s serial cost.
    assert elapsed < 0.7, f"probes look serial: {elapsed:.2f}s for 3×0.3s"


def test_healthy_engines_caches_verdicts_within_ttl(challenge, tmp_path: Path,
                                                    monkeypatch) -> None:
    # a SECOND dispatch within the TTL must reuse verdicts, not re-probe.
    sw = _probe_swarm(challenge, tmp_path, ["claude", "codex"])
    calls: list[str] = []

    def counting_probe(name, role):
        calls.append(name)
        return True, ""

    monkeypatch.setattr(sw, "_probe_engine_health", counting_probe)
    assert sorted(sw._healthy_engines()) == ["claude", "codex"]
    assert sorted(calls) == ["claude", "codex"]  # first sweep probes both
    calls.clear()
    assert sorted(sw._healthy_engines()) == ["claude", "codex"]
    assert calls == []  # second sweep served entirely from cache


def test_healthy_engines_ttl_zero_disables_cache(challenge, tmp_path: Path,
                                                 monkeypatch) -> None:
    sw = _probe_swarm(challenge, tmp_path, ["claude"])
    sw._health_probe_ttl = 0
    n = {"count": 0}

    def counting_probe(name, role):
        n["count"] += 1
        return True, ""

    monkeypatch.setattr(sw, "_probe_engine_health", counting_probe)
    sw._healthy_engines()
    sw._healthy_engines()
    assert n["count"] == 2  # ttl=0 → every sweep re-probes


def test_healthy_engines_failure_cached_shorter(challenge, tmp_path: Path,
                                                monkeypatch) -> None:
    # a FAILED verdict expires at a fraction of the TTL so a recovered engine
    # rejoins quickly. With a tiny TTL the failure window lapses between sweeps.
    sw = _probe_swarm(challenge, tmp_path, ["claude"])
    sw._health_probe_ttl = 0.4  # failure horizon = 0.4 * 0.25 = 0.1s
    state = {"ok": False, "calls": 0}

    def flip_probe(name, role):
        state["calls"] += 1
        return state["ok"], "" if state["ok"] else "down"

    monkeypatch.setattr(sw, "_probe_engine_health", flip_probe)
    sw._healthy_engines()              # claude down → cached fail (short horizon)
    assert state["calls"] == 1
    state["ok"] = True
    time.sleep(0.15)                   # past the failure horizon, within the TTL
    roster = sw._healthy_engines()     # must RE-probe and see recovery
    assert state["calls"] == 2
    assert roster == ["claude"]


# ── launch-time deployed-skill reconciliation (run-75378 drift gap) ──────────

async def test_reconcile_blackboard_skill_resyncs_and_emits(challenge, tmp_path,
                                                             monkeypatch):
    """A non-container run reconciles deployed skill copies at launch: when something
    was stale it re-syncs AND emits a board delta so the drift is visible."""
    from muteki.core.event_bus import EventBus
    from muteki.solver import cli_solver

    captured = []
    bus = EventBus()

    async def _sink(ev):
        captured.append(ev)
    bus.add_sink(_sink)
    sw = _probe_swarm(challenge, tmp_path, ["claude"])
    sw.bus = bus
    sw.worker_backend = "local"

    monkeypatch.setattr(
        cli_solver, "sync_deployed_blackboard_skills",
        lambda: [{"path": "/home/u/.claude/skills/muteki-blackboard/blackboard.py",
                  "status": "synced", "was": "stale(deadbeef0000)", "now": "cafebabe1111"}])

    await sw._reconcile_blackboard_skill()
    deltas = [e for e in captured if e.payload.get("kind") == "skill_resynced"]
    assert len(deltas) == 1
    assert "stale" in deltas[0].payload.get("summary", "")


async def test_reconcile_blackboard_skill_skips_container_backend(challenge, tmp_path,
                                                                  monkeypatch):
    """Container workers use the image-baked skill — the host reconcile must be skipped
    entirely (and never even call the sync)."""
    from muteki.solver import cli_solver

    sw = _probe_swarm(challenge, tmp_path, ["claude"])
    sw.worker_backend = "container"
    called = {"n": 0}
    monkeypatch.setattr(cli_solver, "sync_deployed_blackboard_skills",
                        lambda: called.__setitem__("n", called["n"] + 1) or [])

    await sw._reconcile_blackboard_skill()
    assert called["n"] == 0


# ── Coordinator: evidence-driven plan / dispatch loop ────────────────────────

def _coordinator_swarm(challenge, tmp_path, **kw):
    sandbox = SandboxManager(root=tmp_path / "sbx")
    arts = ArtifactStore(root=tmp_path / "arts")
    # default the race-scout layer OFF for coordinator-LOOP tests (they exercise the
    # main coordinator loop, not the front race phase); race-scout tests pass race_scout=True.
    kw.setdefault("race_scout", False)
    return Swarm(
        challenge, [ModelSpec(solver_id="seat", model="mock")],
        llm=None, sandbox=sandbox, artifacts=arts,
        executor="cli", coordinator=True, **kw,
    )


def test_pick_engine_prefers_unrunning():
    # heterogeneity-aware: prefer an engine not currently running
    sw = Swarm.__new__(Swarm)
    healthy = ["claude", "codex"]
    assert sw._pick_engine([], healthy) == "claude"          # none running
    assert sw._pick_engine(["claude"], healthy) == "codex"    # claude busy
    assert sw._pick_engine(["claude", "codex"], healthy) == "claude"  # both → least-loaded


def test_reason_backpressure_trips_on_large_ordinary_queue(challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path, max_workers=2)
    for i in range(4):
        sw.shared_graph.propose_intent(
            actor="reason", intent_id=f"I-{i}", goal=f"ordinary task {i}",
            payload={"worker_class": "code"})
    sw.shared_graph.propose_intent(
        actor="reason", intent_id="I-review", goal="review task",
        payload={"worker_class": "review"})

    open_intents = sw._open_intents()

    assert sw._ordinary_open_queue_depth(open_intents) == 4
    assert sw._reason_backpressure_active(open_intents) is True


# ── race-scout cold-start invariant (run-75379 BUG④) ─────────────────────────
# race-scout is a cold-start warmup for an EMPTY graph. On a reopen/resume of a
# populated graph it re-races a challenge that already has facts (BUG④: 3 fresh
# bootstrap workers re-racing 33+ verified facts). The guard must be an INVARIANT
# of the coordinator, not something each caller remembers to pass.

def test_is_cold_start_true_on_empty_fresh_graph(challenge, tmp_path: Path):
    # fresh run, empty graph, no flags, cold_start hint default-True → cold.
    sw = _coordinator_swarm(challenge, tmp_path)
    assert sw.cold_start is True
    assert sw._prior_intent_count() == 0
    assert sw._is_cold_start() is True


def test_is_cold_start_true_with_operator_preseeded_facts_only(challenge, tmp_path: Path):
    # Codex edge: an operator MAY pre-seed *facts* into a genuine cold run. Facts
    # alone must NOT be read as a resume — the backstop keys on intents/flags, which
    # only a prior run produces. So this is still a cold start (race should run).
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.shared_graph.add_evidence(
        actor="operator", source="brief", fact="target is 10.0.0.5:8080")
    assert sw._total_fact_count() == 1
    assert sw._prior_intent_count() == 0
    assert sw._is_cold_start() is True


def test_is_cold_start_false_when_prior_intents_present(challenge, tmp_path: Path):
    # graph-state backstop: a prior run left an intent → resume, even if the caller
    # forgot to flip cold_start (this is the bug the invariant closes).
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.shared_graph.propose_intent(
        actor="reason", intent_id="I-prior", goal="probe ssh",
        payload={"worker_class": "code"})
    assert sw.cold_start is True            # caller did NOT flip it
    assert sw._prior_intent_count() == 1
    assert sw._is_cold_start() is False     # ...but the backstop catches it


def test_is_cold_start_false_when_flag_already_found(challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path)
    sw._found_flags = ["flag{already}"]
    assert sw._is_cold_start() is False


def test_is_cold_start_false_on_explicit_resume_hint(challenge, tmp_path: Path):
    # explicit signal is authoritative for "resume": even an empty graph is treated
    # as a resume when cold_start=False (the web resolve() path).
    sw = _coordinator_swarm(challenge, tmp_path, cold_start=False)
    assert sw._prior_intent_count() == 0
    assert sw._is_cold_start() is False


def test_cold_start_hint_from_stage_policy(challenge, tmp_path: Path):
    # config-driven relaunch can flip cold_start via stage_policy.race without a kwarg.
    sw = _coordinator_swarm(
        challenge, tmp_path,
        stage_policy={"race": {"enabled": True, "cold_start": False}})
    assert sw.cold_start is False
    assert sw._is_cold_start() is False


def _async_return(value):
    async def _f(*a, **k):
        return value
    return _f


async def _run_coordinator_briefly(sw, monkeypatch):
    """Enter _run_coordinator with no-op workers + dry Reason + a tiny wall clock so
    it returns fast. Records whether _run_race_scout fired and the phase_transition
    'from' tags emitted. Returns (race_called: bool, transitions: list[str])."""
    from muteki.solver.types import SolveOutcome

    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_healthy_engines_async",
                        _async_return(["claude"]))

    race_called = {"n": 0}

    async def fake_race(healthy):
        race_called["n"] += 1
        return (None, None, {})  # slow path: no flag, fall through
    monkeypatch.setattr(sw, "_run_race_scout", fake_race)

    async def fake_reason():
        return 0  # dry — proposes nothing
    monkeypatch.setattr(sw, "_run_reason", fake_reason)

    class FakeWorker:
        solver_id = "cli-claude"
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "miss")
    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: FakeWorker())

    transitions: list[str] = []
    real_emit = sw._emit_coord_bb

    async def spy_emit(kind, **fields):
        if kind == "phase_transition":
            transitions.append(str(fields.get("from")))
        return await real_emit(kind, **fields)
    monkeypatch.setattr(sw, "_emit_coord_bb", spy_emit)

    await asyncio.wait_for(sw._run_coordinator(), timeout=5)
    return race_called["n"] > 0, transitions


async def test_coordinator_skips_race_scout_on_populated_graph(
        challenge, tmp_path: Path, monkeypatch):
    """INVARIANT: race_scout=True but a graph with prior intents → NO race-scout, go
    straight to the warm Reason/Explore loop. Caller did NOT pass cold_start=False —
    the coordinator's own guard must still protect it."""
    sw = _coordinator_swarm(
        challenge, tmp_path, race_scout=True, start_workers=1, wall_clock_budget=0.3)
    sw.shared_graph.add_evidence(
        actor="cli-claude", source="recon", fact="admin panel at /admin")
    sw.shared_graph.propose_intent(
        actor="reason", intent_id="I-prior", goal="probe /admin",
        payload={"worker_class": "code"})
    assert sw.cold_start is True  # the bug scenario: hint not flipped

    race_called, transitions = await _run_coordinator_briefly(sw, monkeypatch)

    assert race_called is False            # warm start: race-scout skipped
    assert "resume" in transitions        # announced the warm entry
    assert "race" not in transitions      # no after-race transition fired


async def test_coordinator_runs_race_scout_on_cold_empty_graph(
        challenge, tmp_path: Path, monkeypatch):
    """An empty cold graph must STILL race-scout (no regression to the warmup)."""
    sw = _coordinator_swarm(
        challenge, tmp_path, race_scout=True, start_workers=1, wall_clock_budget=0.3)
    assert sw._is_cold_start() is True

    race_called, transitions = await _run_coordinator_briefly(sw, monkeypatch)

    assert race_called is True             # cold start: race-scout ran
    assert "race" in transitions          # slow-path race→coordinator transition
    assert "resume" not in transitions


async def test_coordinator_skips_race_scout_on_explicit_resume(
        challenge, tmp_path: Path, monkeypatch):
    """Explicit cold_start=False (web resolve path) skips race-scout even on an
    empty graph — the explicit signal is authoritative."""
    sw = _coordinator_swarm(
        challenge, tmp_path, race_scout=True, cold_start=False,
        start_workers=1, wall_clock_budget=0.3)

    race_called, transitions = await _run_coordinator_briefly(sw, monkeypatch)

    assert race_called is False
    assert "resume" in transitions


def test_prior_intent_count_survives_reopen_on_same_graph_dir(challenge, tmp_path: Path):
    """The backstop reads the durable intents table, so a FRESH Swarm constructed on
    the SAME graph_dir (the reopen shape) sees the prior run's intents even though its
    in-memory state is empty."""
    graph_dir = tmp_path / "graph"
    sw1 = _coordinator_swarm(challenge, tmp_path, graph_dir=graph_dir)
    sw1.shared_graph.propose_intent(
        actor="reason", intent_id="I-1", goal="enumerate", payload={})
    sw1.shared_graph.close()

    # a brand-new Swarm on the same dir (no cold_start flip) — the bug scenario.
    sw2 = _coordinator_swarm(challenge, tmp_path, graph_dir=graph_dir, race_scout=True)
    assert sw2._found_flags == []          # fresh in-memory state
    assert sw2._prior_intent_count() == 1  # ...but the DB carries history
    assert sw2._is_cold_start() is False


async def test_graph_tail_bridge_drains_direct_db_writes_once_in_seq_order(
        challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path, max_workers=1)
    assert sw.shared_graph is not None
    g = sw.shared_graph
    fact_seq = g.add_evidence(actor="skill", source="blackboard",
                              fact="admin password is hunter2", verified=True)
    dead_seq = g.add_dead_end(actor="skill", reason="ftp anonymous is disabled")
    g.propose_intent(actor="reason", intent_id="I-bridge", goal="try ssh")
    g.claim_intent(worker="cli-1", intent_id="I-bridge")
    g.conclude_intent(
        actor="cli-1",
        intent_id="I-bridge",
        result="explored",
        result_detail="Tried ssh with hunter2 and got permission denied.",
    )

    emitted: list[tuple[str, dict]] = []

    async def emit(kind: str, **fields):
        emitted.append((kind, fields))

    await sw._drain_graph_to_bus(emit_bb=emit)

    assert [kind for kind, _ in emitted] == [
        "fact_added",
        "dead_end",
        "intent_proposed",
        "intent_claimed",
        "intent_concluded",
    ]
    assert emitted[0][1]["fact_seq"] == fact_seq
    assert emitted[1][1]["dead_end_seq"] == dead_seq
    assert emitted[-1][1]["result"] == "explored"
    assert "permission denied" in emitted[-1][1]["result_detail"]

    await sw._drain_graph_to_bus(emit_bb=emit)
    assert [kind for kind, _ in emitted] == [
        "fact_added",
        "dead_end",
        "intent_proposed",
        "intent_claimed",
        "intent_concluded",
    ]


async def test_graph_tail_bridge_advances_watermark_only_after_emit_success(
        challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path, max_workers=1)
    assert sw.shared_graph is not None
    fact_seq = sw.shared_graph.add_evidence(
        actor="skill", source="blackboard", fact="service is nginx", verified=True)

    async def failing_emit(kind: str, **fields):
        raise RuntimeError("sink down")

    await sw._drain_graph_to_bus(emit_bb=failing_emit)
    assert sw._last_graph_event_seq == 0

    emitted: list[tuple[str, dict]] = []

    async def emit(kind: str, **fields):
        emitted.append((kind, fields))

    await sw._drain_graph_to_bus(emit_bb=emit)
    assert sw._last_graph_event_seq == fact_seq
    assert len(emitted) == 1
    assert emitted[0][0] == "fact_added"


async def test_graph_tail_bridge_skips_poison_event_after_bounded_retries(
        challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path, max_workers=1)
    assert sw.shared_graph is not None
    fact_seq = sw.shared_graph.add_evidence(
        actor="skill", source="blackboard", fact="poison bridge event",
        verified=True)

    async def failing_emit(kind: str, **fields):
        raise RuntimeError("sink still down")

    await sw._drain_graph_to_bus(emit_bb=failing_emit)
    await sw._drain_graph_to_bus(emit_bb=failing_emit)
    assert sw._last_graph_event_seq == 0

    await sw._drain_graph_to_bus(emit_bb=failing_emit)
    assert sw._last_graph_event_seq == fact_seq

    emitted: list[tuple[str, dict]] = []

    async def emit(kind: str, **fields):
        emitted.append((kind, fields))

    await sw._drain_graph_to_bus(emit_bb=emit)
    assert emitted == []


def test_operator_hint_intent_orders_before_existing_queue(challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path, max_workers=2)
    sw.shared_graph.propose_intent(
        actor="reason", intent_id="I-old", goal="old queued task",
        payload={"worker_class": "code"})
    sw.shared_graph.propose_intent(
        actor="operator", intent_id="I-operator-test", goal="manual hint task",
        payload={"source": "operator_hint", "action": "hint"})

    open_intents = sw._open_intents()

    assert [i["intent_id"] for i in open_intents[:2]] == [
        "I-operator-test", "I-old",
    ]
    assert open_intents[0]["priority"] == 100


async def test_review_intents_wait_when_review_concurrency_full(challenge, tmp_path: Path):
    sw = _coordinator_swarm(
        challenge, tmp_path,
        stage_policy={"coordinator": {"review": {
            "enabled": True, "max_concurrent": 1,
        }}},
    )

    async def sleeper():
        await asyncio.sleep(0.05)

    task = asyncio.create_task(sleeper())
    sw._active_review_tasks.add(task)
    intents = [
        {"intent_id": "I-review", "goal": "audit", "worker_class": "review"},
        {"intent_id": "I-code", "goal": "exploit", "worker_class": "code"},
    ]

    filtered = sw._dispatchable_open_intents(intents)

    assert [i["intent_id"] for i in filtered] == ["I-code"]
    await task


async def test_review_worker_uses_reserved_capacity_when_ordinary_slots_full(
    challenge, tmp_path: Path, monkeypatch,
):
    sw = _coordinator_swarm(
        challenge, tmp_path, max_workers=1,
        stage_policy={"coordinator": {"review": {
            "enabled": True, "engine": "claude", "max_concurrent": 1,
            "cooldown_events": 0, "max_review_workers": 3,
        }}},
    )

    async def long_running():
        await asyncio.sleep(3600)

    ordinary = asyncio.create_task(long_running(), name="ordinary")
    tasks = {ordinary: "codex"}
    task_solvers = {ordinary: object()}
    emitted: list[tuple[str, dict]] = []

    class FakeReviewWorker:
        solver_id = "cli-claude-review"

        async def run(self):
            await asyncio.sleep(3600)

    monkeypatch.setattr(sw, "_select_review_engine", lambda healthy: "claude")
    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: FakeReviewWorker())

    async def emit_bb(kind, **fields):
        emitted.append((kind, fields))

    try:
        started = await sw._maybe_start_review(
            trigger="operator_hint",
            directive="audit duplicated candidates",
            healthy=["claude", "codex"],
            tasks=tasks,
            task_solvers=task_solvers,
            emit_bb=emit_bb,
        )

        assert started is True
        assert len(tasks) == 2
        assert sum(
            1 for task in tasks
            if task.get_name().startswith("review-")
        ) == 1
        assert any(k == "review_started" for k, _ in emitted)
    finally:
        for task in list(tasks):
            task.cancel()


async def test_review_intent_remains_dispatchable_when_ordinary_slots_full(
    challenge, tmp_path: Path,
):
    sw = _coordinator_swarm(
        challenge, tmp_path, max_workers=1,
        stage_policy={"coordinator": {"review": {
            "enabled": True, "max_concurrent": 1,
        }}},
    )

    async def long_running():
        await asyncio.sleep(3600)

    ordinary = asyncio.create_task(long_running(), name="ordinary")
    tasks = {ordinary: "codex"}
    intents = [
        {"intent_id": "I-code", "goal": "exploit", "worker_class": "code"},
        {"intent_id": "I-review", "goal": "audit", "worker_class": "review"},
    ]

    try:
        filtered = sw._capacity_dispatchable_open_intents(
            sw._dispatchable_open_intents(intents), tasks)

        assert [i["intent_id"] for i in filtered] == ["I-review"]
    finally:
        ordinary.cancel()


async def test_run_reason_passes_standing_guidance(challenge, tmp_path: Path, monkeypatch):
    from muteki.solver.reason import ReasonResult

    sw = _coordinator_swarm(challenge, tmp_path)
    sw.llm = object()
    sw._standing_guidance = ["Use the VPS tunnel; do not test internal hosts from the Mac."]
    seen: dict[str, list[str]] = {}
    original = sw.shared_graph.to_reason_summary

    def fake_summary(*, standing_guidance=None):
        seen["standing"] = list(standing_guidance or [])
        return original(standing_guidance=standing_guidance)

    async def fake_run_reason(**_kwargs):
        return ReasonResult(goal_met=False, intents=[], audit_notes=[])

    monkeypatch.setattr(sw.shared_graph, "to_reason_summary", fake_summary)
    monkeypatch.setattr("muteki.solver.reason.run_reason", fake_run_reason)

    proposed = await sw._run_reason()

    assert proposed == 0
    assert seen["standing"] == sw._standing_guidance


async def test_run_reason_persists_model_selected_fact_pins(
        challenge, tmp_path: Path, monkeypatch):
    from muteki.solver.reason import ReasonResult

    sw = _coordinator_swarm(challenge, tmp_path)
    sw.llm = object()
    fact_seq = sw.shared_graph.add_evidence(
        actor="cli-a", source="cmd", fact="后台口令是 admin / 猎人二号",
        verified=True)
    for i in range(10):
        sw.shared_graph.add_evidence(
            actor=f"noise-{i}", source="scan", fact=f"old noise {i}",
            verified=True)
    seen: dict[str, str] = {}

    async def fake_run_reason(**kwargs):
        seen["fact_index"] = kwargs.get("fact_index", "")
        return ReasonResult(goal_met=False, intents=[], audit_notes=[],
                            pinned_facts=[fact_seq])

    monkeypatch.setattr("muteki.solver.reason.run_reason", fake_run_reason)

    proposed = await sw._run_reason()

    assert proposed == 0
    assert f"[#{fact_seq}]" in seen["fact_index"]
    assert fact_seq in sw.shared_graph.pinned_fact_seqs()
    assert "后台口令是 admin" in sw.shared_graph.to_reason_summary()


async def test_coordinator_applies_tier1_review_proposal(challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path)
    fact_seq = sw.shared_graph.add_evidence(
        actor="cli-a", source="claude", fact="JWT alg is HS256",
        verified=True, artifact_id="a1")
    sw.shared_graph.add_review_proposal(
        actor="cli-review", marker="FACT_CHALLENGE",
        payload={
            "fact_seq": fact_seq,
            "reason": "no raw header proof",
            "verification_goal": "Decode a real JWT header.",
        },
    )
    emitted: list[tuple[str, dict]] = []

    async def emit_bb(kind, **fields):
        emitted.append((kind, fields))

    applied = await sw._drain_review_proposals(emit_bb=emit_bb)

    assert applied == 1
    assert sw.shared_graph.challenged_facts()[0]["fact_seq"] == fact_seq
    assert any(k == "review_proposal_decision" and f["decision"] == "accepted"
               for k, f in emitted)


async def test_route_suppress_proposal_requires_three_real_failures(
    challenge, tmp_path: Path,
):
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.shared_graph.add_review_proposal(
        actor="cli-review", marker="ROUTE_SUPPRESS",
        payload={"route_hash": "web:login:sqli", "reason": "loop", "confidence": 0.95},
        tier="tier2",
    )
    emitted: list[tuple[str, dict]] = []

    async def emit_bb(kind, **fields):
        emitted.append((kind, fields))

    assert await sw._drain_review_proposals(emit_bb=emit_bb) == 0
    assert not sw.shared_graph.is_route_suppressed("web:login:sqli")
    assert emitted[-1][1]["decision"] == "deferred"

    for i in range(3):
        iid = f"I-fail-{i}"
        worker = f"cli-worker-{i}"
        sw.shared_graph.propose_intent(
            actor="reason", intent_id=iid, goal=f"try login SQLi {i}",
            payload={"worker_class": "code", "route_hash": "web:login:sqli"})
        sw.shared_graph.claim_intent(worker=worker, intent_id=iid)
        sw.shared_graph.conclude_intent(
            actor=worker, intent_id=iid, result="dead: failed with no flag")
    sw.shared_graph.add_review_proposal(
        actor="cli-review", marker="ROUTE_SUPPRESS",
        payload={"route_hash": "web:login:sqli", "reason": "loop now proven",
                 "confidence": 0.95},
        tier="tier2",
    )

    assert await sw._drain_review_proposals(emit_bb=emit_bb) == 1
    assert sw.shared_graph.is_route_suppressed("web:login:sqli")


def test_open_intents_dedupes_same_route_but_keeps_review(challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.shared_graph.propose_intent(
        actor="reason", intent_id="I-a", goal="try login SQLi variant A",
        payload={"worker_class": "code", "route_hash": "web:login:sqli"})
    sw.shared_graph.propose_intent(
        actor="reason", intent_id="I-b", goal="try login SQLi variant B",
        payload={"worker_class": "code", "route_hash": "web:login:sqli"})
    sw.shared_graph.propose_intent(
        actor="reason", intent_id="I-review", goal="review login SQLi loop",
        payload={"worker_class": "review", "route_hash": "web:login:sqli"})

    ids = [i["intent_id"] for i in sw._open_intents()]

    assert ids == ["I-a", "I-review"]


def test_lane_lock_proposal_locks_and_next_intent_keeps_lane(
    challenge, tmp_path: Path,
):
    async def _run():
        sw = _coordinator_swarm(challenge, tmp_path)
        lane = "destructive:tcp:445@172.22.11.45"
        sw.shared_graph.add_review_proposal(
            actor="cli-review", marker="LANE_LOCK",
            payload={"lane_key": lane, "risk_class": "destructive",
                     "owner_worker": "cli-review", "reason": "serialize exploit"},
            tier="tier2",
        )
        emitted: list[tuple[str, dict]] = []

        async def emit_bb(kind, **fields):
            emitted.append((kind, fields))

        assert await sw._drain_review_proposals(emit_bb=emit_bb) == 1
        assert sw.shared_graph.active_lanes()[0]["lane_key"] == lane
        assert any(k == "lane_locked" for k, _ in emitted)

        sw.shared_graph.add_review_proposal(
            actor="cli-review", marker="NEXT_INTENT",
            payload={"id": "I-next", "goal": "retry SMB after lock",
                     "lane_key": lane, "risk_class": "destructive"},
        )
        assert await sw._drain_review_proposals(emit_bb=emit_bb) == 1
        with sw.shared_graph._lock:
            row = sw.shared_graph._conn.execute(
                "SELECT lane_key, risk_class FROM intents WHERE intent_id='I-next'"
            ).fetchone()
        assert row == (lane, "destructive")

    asyncio.run(_run())


def test_next_intent_infers_lane_from_goal_text(challenge, tmp_path: Path):
    async def _run():
        sw = _coordinator_swarm(challenge, tmp_path)
        emitted: list[tuple[str, dict]] = []

        async def emit_bb(kind, **fields):
            emitted.append((kind, fields))

        sw.shared_graph.add_review_proposal(
            actor="cli-review", marker="NEXT_INTENT",
            payload={
                "id": "I-review-lane-text",
                "goal": (
                    "CONSOLIDATED FINAL DIAGNOSTIC under "
                    "destructive:tcp:5000@107.170.15.231 lane: perform one "
                    "serialized exploit verification and do not fan out."
                ),
                "worker_class": "code",
            },
        )

        assert await sw._drain_review_proposals(emit_bb=emit_bb) == 1
        with sw.shared_graph._lock:
            row = sw.shared_graph._conn.execute(
                "SELECT lane_key, risk_class FROM intents "
                "WHERE intent_id='I-review-lane-text'"
            ).fetchone()
        assert row == ("destructive:tcp:5000@107.170.15.231", "destructive")
        assert any(
            fields.get("lane_key") == "destructive:tcp:5000@107.170.15.231"
            for kind, fields in emitted
            if kind == "intent_proposed"
        )

    asyncio.run(_run())


def test_open_intents_stays_pure_when_lane_is_locked(challenge, tmp_path: Path):
    sw = _coordinator_swarm(challenge, tmp_path)
    lane = "destructive:tcp:445@172.22.11.45"
    sw.shared_graph.propose_intent(
        actor="reason", intent_id="I-lane", goal="exploit smb",
        payload={"lane_key": lane, "risk_class": "destructive"})
    sw.shared_graph.lock_lane(
        actor="coord", lane_key=lane, risk_class="destructive",
        owner_worker="other", owner_intent="I-other")
    before = len(sw.shared_graph.events())
    first = sw._open_intents()
    second = sw._open_intents()
    after = len(sw.shared_graph.events())
    assert first == second
    assert first[0]["lane_key"] == lane
    assert before == after
    assert not any(e["kind"] == "intent_lane_deferred" for e in sw.shared_graph.events())


def test_open_intents_backfills_structured_lane_from_existing_goal_text(
    challenge, tmp_path: Path,
):
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.shared_graph.propose_intent(
        actor="coordinator",
        intent_id="I-old-lane",
        goal=(
            "CONSOLIDATED FINAL DIAGNOSTIC under "
            "destructive:tcp:5000@107.170.15.231 lane; run exactly one probe."
        ),
        payload={"worker_class": "verifier"},
    )
    sw.shared_graph.propose_intent(
        actor="coordinator",
        intent_id="I-vague-lane",
        goal="Under the lane, mutate one field and compare timing.",
        payload={"worker_class": "verifier"},
    )

    rows = {it["intent_id"]: it for it in sw._open_intents()}
    assert rows["I-old-lane"]["lane_key"] == "destructive:tcp:5000@107.170.15.231"
    assert rows["I-old-lane"]["risk_class"] == "destructive"
    assert rows["I-vague-lane"]["lane_key"] == ""
    with sw.shared_graph._lock:
        db_rows = dict(sw.shared_graph._conn.execute(
            "SELECT intent_id, COALESCE(lane_key, '') FROM intents "
            "WHERE intent_id IN ('I-old-lane', 'I-vague-lane')"
        ).fetchall())
    assert db_rows["I-old-lane"] == "destructive:tcp:5000@107.170.15.231"
    assert db_rows["I-vague-lane"] == ""


def test_pick_engine_least_loaded_when_all_running():
    sw = Swarm.__new__(Swarm)
    healthy = ["claude", "codex"]
    # claude running twice, codex once → pick codex
    assert sw._pick_engine(["claude", "claude", "codex"], healthy) == "codex"


def test_pick_engine_three_engines_heterogeneity():
    # with cursor in the roster, _pick_engine still prefers an unrunning engine,
    # walking the roster order, then falls back to least-loaded.
    sw = Swarm.__new__(Swarm)
    healthy = ["cursor", "claude", "codex"]
    assert sw._pick_engine([], healthy) == "cursor"            # none running
    assert sw._pick_engine(["cursor"], healthy) == "claude"     # cursor busy
    assert sw._pick_engine(["cursor", "claude"], healthy) == "codex"
    # all three running → least-loaded (codex once vs cursor/claude twice)
    assert sw._pick_engine(
        ["cursor", "cursor", "claude", "claude", "codex"], healthy) == "codex"


# ── operator runtime worker control (spawn/kill a specific engine) ───────────

class _FakeWorker:
    def __init__(self, engine: str):
        self.solver_id = f"cli-{engine}-op"
        self.cancelled = False

    async def run(self):
        await asyncio.sleep(3600)  # long-lived until cancelled

    def cancel(self):
        self.cancelled = True


@pytest.mark.asyncio
async def test_apply_worker_cmds_spawn_then_kill(challenge, tmp_path, monkeypatch):
    sw = _coordinator_swarm(challenge, tmp_path, engines=["claude", "codex"])
    sw.worker_cmds = asyncio.Queue()
    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: _FakeWorker(engine))

    tasks: dict = {}
    task_solvers: dict = {}
    emitted: list = []

    async def emit_bb(kind, **f):
        emitted.append((kind, f))

    # spawn a claude worker on demand
    sw.worker_cmds.put_nowait({"action": "spawn", "engine": "claude"})
    await sw._apply_worker_cmds(
        tasks=tasks, task_solvers=task_solvers, healthy=["claude", "codex"],
        running_engines_fn=lambda: list(tasks.values()), emit_bb=emit_bb)
    assert len(tasks) == 1
    w = next(iter(task_solvers.values()))
    assert w.solver_id == "cli-claude-op"
    assert any(k == "worker_spawned" for k, _ in emitted)

    # kill it by solver_id → solver cancelled + worker_killed emitted
    sw.worker_cmds.put_nowait({"action": "kill", "solver_id": "cli-claude-op"})
    await sw._apply_worker_cmds(
        tasks=tasks, task_solvers=task_solvers, healthy=["claude", "codex"],
        running_engines_fn=lambda: list(tasks.values()), emit_bb=emit_bb)
    assert w.cancelled is True
    assert any(k == "worker_killed" for k, _ in emitted)

    for t in list(tasks):
        t.cancel()


@pytest.mark.asyncio
async def test_apply_worker_cmds_rejects_unknown_engine_and_max(challenge, tmp_path, monkeypatch):
    sw = _coordinator_swarm(challenge, tmp_path, engines=["claude", "codex"])
    sw.worker_cmds = asyncio.Queue()
    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: _FakeWorker(engine))
    tasks: dict = {}
    task_solvers: dict = {}
    emitted: list = []

    async def emit_bb(kind, **f):
        emitted.append((kind, f))

    # cursor is NOT in this swarm's roster (e.g. offline) → spawn rejected
    sw.worker_cmds.put_nowait({"action": "spawn", "engine": "cursor"})
    await sw._apply_worker_cmds(
        tasks=tasks, task_solvers=task_solvers, healthy=["claude", "codex"],
        running_engines_fn=lambda: list(tasks.values()), emit_bb=emit_bb)
    assert len(tasks) == 0
    assert any(k == "worker_spawn_rejected" and f.get("reason") == "unknown_engine"
               for k, f in emitted)

    # at max_workers → spawn rejected
    emitted.clear()
    sw.max_workers = 0
    sw.worker_cmds.put_nowait({"action": "spawn", "engine": "claude"})
    await sw._apply_worker_cmds(
        tasks=tasks, task_solvers=task_solvers, healthy=["claude", "codex"],
        running_engines_fn=lambda: list(tasks.values()), emit_bb=emit_bb)
    assert len(tasks) == 0
    assert any(k == "worker_spawn_rejected" and f.get("reason") == "max_workers"
               for k, f in emitted)


async def test_coordinator_bootstrap_then_flag(challenge, tmp_path: Path, monkeypatch):
    """Bootstrap worker finds a flag → coordinator returns solved, no Reason needed."""
    from muteki.solver.types import SolveOutcome

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=2)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])

    class FakeWorker:
        def __init__(self, engine, solved):
            self.solver_id = f"cli-{engine}"
            self._solved = solved
        async def run(self):
            await asyncio.sleep(0)
            if self._solved:
                return SolveOutcome(True, "flag{win}", 1, None, "solved")
            return SolveOutcome(False, None, 1, None, "miss")

    # claude solves, codex misses
    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        return FakeWorker(engine, solved=(engine == "claude"))
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    out = await sw.run()
    assert out.solved is True
    assert out.flag == "flag{win}"
    assert out.winner == "cli-claude"


async def test_coordinator_multiflag_waits_for_all(challenge, tmp_path: Path, monkeypatch):
    """expected_flags=2: one worker returning ONE flag must NOT end the run; the
    coordinator keeps going until two distinct flags are collected."""
    from muteki.solver.types import SolveOutcome

    challenge.expected_flags = 2
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=2)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])

    # each engine returns a DIFFERENT single flag; neither alone completes the run.
    flags = {"claude": "flag{a}", "codex": "flag{b}"}

    class FakeWorker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
            self._f = flags[engine]
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(True, self._f, 1, None, "solved", flags=[self._f])

    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: FakeWorker(engine))

    out = await sw.run()
    assert out.solved is True
    # the run collected BOTH distinct flags (not just the first worker's)
    assert set(sw._found_flags) == {"flag{a}", "flag{b}"}
    assert sw._flags_complete() is True


async def test_coordinator_singleflag_stops_on_first(challenge, tmp_path: Path, monkeypatch):
    """expected_flags=1 (default): the first flag completes the run immediately —
    byte-identical to the legacy 'first flag wins'."""
    from muteki.solver.types import SolveOutcome

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=2)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])

    class FakeWorker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(True, "flag{x}", 1, None, "solved", flags=["flag{x}"])

    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: FakeWorker(engine))
    out = await sw.run()
    assert out.solved is True and out.flag == "flag{x}"
    assert sw._found_flags == ["flag{x}"]  # stopped at one


async def test_coordinator_ctf_complete_reason_stops_dispatching(
    challenge, tmp_path: Path, monkeypatch
):
    """run-61718 regression: after all real flags are already collected, Reason may
    return verdict=complete while old/irrelevant intents still exist. CTF must
    settle immediately instead of spawning another explore worker."""
    from muteki.solver.reason import ReasonResult
    from muteki.solver.types import SolveOutcome

    challenge.expected_flags = 4
    challenge.multi_flag = True
    sw = _coordinator_swarm(
        challenge, tmp_path, start_workers=1, max_workers=2, wall_clock_budget=0.3)
    sw._found_flags = ["flag{a}", "flag{b}", "flag{c}", "flag{d}"]
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])
    monkeypatch.setattr(sw, "_verified_fact_count", lambda: 1)

    makes: list[tuple[str, str]] = []

    class FakeWorker:
        solver_id = "cli-claude"
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "miss")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        makes.append((engine, mode))
        return FakeWorker()

    async def fake_reason():
        sw._last_reason = ReasonResult(
            goal_met=True, intents=[], audit_notes=[],
            verdict="complete", complete_why="all four flags are verified")
        return 0

    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)
    monkeypatch.setattr(sw, "_run_reason", fake_reason)

    out = await asyncio.wait_for(sw.run(), timeout=5)

    assert out.solved is True
    assert makes == [("claude", "bootstrap")]


async def test_coordinator_reason_then_explore(challenge, tmp_path: Path, monkeypatch):
    """Bootstrap finds nothing → Reason proposes an intent → Explore claims it."""
    from muteki.solver.types import SolveOutcome

    # finite budget so the (now never-give-up) loop terminates for the assertion —
    # the swarm no longer stops on its own when Reason runs dry (CTF has a unique
    # solution → it re-bootstraps forever; only solve/stop/budget ends it).
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1, stall_seconds=0.01,
                            wall_clock_budget=0.3)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])

    spawned_modes = []

    class FakeWorker:
        def __init__(self, engine, mode):
            self.solver_id = f"cli-{engine}"
            self.mode = mode
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "miss")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        spawned_modes.append(mode)
        return FakeWorker(engine, mode)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    # Reason proposes exactly one intent the first time, then nothing
    reason_calls = {"n": 0}
    async def fake_reason():
        reason_calls["n"] += 1
        if reason_calls["n"] == 1:
            try:
                sw.shared_graph.propose_intent(
                    actor="reason", intent_id="I1", goal="try SQLi")
            except Exception:
                pass
            return 1
        return 0
    monkeypatch.setattr(sw, "_run_reason", fake_reason)

    out = await sw.run()
    # bootstrap ran, then at least one explore was spawned for the proposed intent
    assert "bootstrap" in spawned_modes
    assert "explore" in spawned_modes
    assert out.solved is False  # nobody found a flag in this scenario


async def test_coordinator_rebootstrap_on_course_correct(challenge, tmp_path: Path, monkeypatch):
    """Reason verdict course_correct → coordinator spawns a re-bootstrap worker
    seeded with the drift direction (phase 7: adaptive mode switch)."""
    from muteki.solver.types import SolveOutcome
    from muteki.solver.reason import ReasonResult

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1, stall_seconds=0.01,
                            wall_clock_budget=0.3)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])

    spawned = []  # (mode, intent_goal)

    class FakeWorker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "miss")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        spawned.append((mode, intent_goal))
        return FakeWorker(engine)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    calls = {"n": 0}
    async def fake_reason():
        calls["n"] += 1
        if calls["n"] == 1:
            sw._last_reason = ReasonResult(
                goal_met=False, intents=[], audit_notes=[],
                verdict="course_correct", drift="stop brute-forcing, decode the cookie")
            return 0
        sw._last_reason = ReasonResult(
            goal_met=False, intents=[], audit_notes=[], verdict="explore")
        return 0
    monkeypatch.setattr(sw, "_run_reason", fake_reason)

    out = await asyncio.wait_for(sw.run(), timeout=5)
    # a re-bootstrap worker was spawned with the drift as its steer
    rebootstraps = [g for (m, g) in spawned if m == "bootstrap" and "decode the cookie" in g]
    assert rebootstraps, f"no re-bootstrap seeded with drift; spawned={spawned}"


async def test_course_correct_runs_review_before_rebootstrap(challenge, tmp_path: Path, monkeypatch):
    """Final flow: Reason course_correct must route through one review worker whose
    directive controls the rebootstrap, instead of immediately spawning another
    ordinary bootstrap on raw drift text."""
    from muteki.solver.types import SolveOutcome
    from muteki.solver.reason import ReasonResult

    sw = _coordinator_swarm(
        challenge, tmp_path, start_workers=1, stall_seconds=0.01,
        wall_clock_budget=0.4,
        stage_policy={"coordinator": {"review": {
            "enabled": True, "engine": "codex", "timeout": 120,
            "on_course_correct": True, "cooldown_events": 0,
        }}},
    )
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])

    spawned: list[tuple[str, str, str]] = []

    class FakeWorker:
        def __init__(self, engine, mode, goal):
            self.solver_id = f"cli-{engine}-{mode}"
            self.mode = mode
            self.goal = goal

        async def run(self):
            if self.mode == "review":
                sw.shared_graph.add_coordinator_directive(
                    actor=self.solver_id,
                    action="rebootstrap",
                    directive="Stop brute force; decode the signed cookie.",
                    priority="high",
                )
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, self.mode)

    def fake_make(engine, *, mode, intent_goal="", intent_id="", **_kw):
        spawned.append((engine, mode, intent_goal))
        return FakeWorker(engine, mode, intent_goal)

    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    calls = {"n": 0}

    async def fake_reason():
        calls["n"] += 1
        if calls["n"] == 1:
            sw._last_reason = ReasonResult(
                goal_met=False, intents=[], audit_notes=[],
                verdict="course_correct", drift="raw drift: stop brute force")
            return 0
        sw._last_reason = ReasonResult(
            goal_met=False, intents=[], audit_notes=[], verdict="explore")
        return 0

    monkeypatch.setattr(sw, "_run_reason", fake_reason)

    await asyncio.wait_for(sw.run(), timeout=5)

    assert ("codex", "review", "raw drift: stop brute force") in spawned
    assert any(mode == "bootstrap" and "decode the signed cookie" in goal
               for _engine, mode, goal in spawned), spawned


async def test_operator_hint_queues_review_request(challenge, tmp_path):
    sw = _coordinator_swarm(
        challenge, tmp_path,
        stage_policy={"coordinator": {"review": {
            "enabled": True, "on_operator_hint": True,
        }}},
    )

    await _drain_one(sw, {
        "target": "global", "action": "hint",
        "text": "pivot through the SOCKS tunnel instead of repeated SSH hops",
    })

    assert sw._queued_review_requests
    assert sw._queued_review_requests[0]["trigger"] == "operator_hint"
    assert "SOCKS tunnel" in sw._queued_review_requests[0]["directive"]


async def test_completed_worker_threshold_starts_review(challenge, tmp_path: Path, monkeypatch):
    from muteki.solver.types import SolveOutcome

    sw = _coordinator_swarm(
        challenge, tmp_path, start_workers=1, max_workers=2,
        wall_clock_budget=0.2,
        stage_policy={"coordinator": {"review": {
            "enabled": True, "engine": "codex",
            "every_completed_workers": 1,
            "on_candidate_spike": False,
            "on_reason_dry": False,
            "cooldown_events": 0,
        }}},
    )
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())

    spawned: list[tuple[str, str]] = []

    class FakeWorker:
        def __init__(self, engine, mode):
            self.solver_id = f"cli-{engine}-{mode}-{len(spawned)}"
            self.mode = mode

        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, self.mode)

    def fake_make(engine, *, mode, **_kw):
        spawned.append((engine, mode))
        return FakeWorker(engine, mode)

    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    out = await asyncio.wait_for(sw.run(), timeout=5)
    assert out.solved is False
    assert ("codex", "review") in spawned


async def test_candidate_spike_starts_review(challenge, tmp_path: Path, monkeypatch):
    from muteki.solver.types import SolveOutcome

    sw = _coordinator_swarm(
        challenge, tmp_path, start_workers=1, max_workers=2,
        wall_clock_budget=0.2,
        stage_policy={"coordinator": {"review": {
            "enabled": True, "engine": "codex",
            "every_completed_workers": 0,
            "on_candidate_spike": True,
            "candidate_spike_threshold": 2,
            "on_reason_dry": False,
            "cooldown_events": 0,
        }}},
    )
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())
    candidates = {"n": 0}
    monkeypatch.setattr(sw, "_candidate_fact_count", lambda: candidates["n"])

    spawned: list[tuple[str, str]] = []

    class FakeWorker:
        def __init__(self, engine, mode):
            self.solver_id = f"cli-{engine}-{mode}-{len(spawned)}"
            self.mode = mode

        async def run(self):
            if self.mode != "review":
                candidates["n"] = 3
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, self.mode)

    def fake_make(engine, *, mode, **_kw):
        spawned.append((engine, mode))
        return FakeWorker(engine, mode)

    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    out = await asyncio.wait_for(sw.run(), timeout=5)
    assert out.solved is False
    assert ("codex", "review") in spawned


async def test_coordinator_respects_wall_clock_budget(challenge, tmp_path: Path, monkeypatch):
    """A worker that never finishes is cancelled when the budget is exhausted."""
    from muteki.solver.types import SolveOutcome

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1,
                            wall_clock_budget=0.01, stall_seconds=999)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())

    class HangWorker:
        solver_id = "cli-claude"
        async def run(self):
            await asyncio.sleep(100)  # never finishes
            return SolveOutcome(False, None, 1, None, "x")

    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda *a, **k: HangWorker())
    out = await asyncio.wait_for(sw.run(), timeout=5)
    assert out.solved is False


async def _async_zero():
    return 0


# ── winner KILLS the loser's subprocess, not just its task (bug #2) ───────────

async def test_coordinator_winner_cancels_loser_solver(challenge, tmp_path: Path, monkeypatch):
    """When one worker finds the flag, the coordinator must call cancel() on the
    OTHER worker (which kills its CLI subprocess) — not merely cancel the task."""
    from muteki.solver.types import SolveOutcome

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=2)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])

    cancelled = []

    class FakeWorker:
        def __init__(self, engine, solved):
            self.solver_id = f"cli-{engine}"
            self._solved = solved
        def cancel(self):
            cancelled.append(self.solver_id)
        async def run(self):
            if self._solved:
                await asyncio.sleep(0)
                return SolveOutcome(True, "flag{win}", 1, None, "solved")
            await asyncio.sleep(100)  # loser hangs until cancelled
            return SolveOutcome(False, None, 1, None, "miss")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        return FakeWorker(engine, solved=(engine == "claude"))
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    out = await asyncio.wait_for(sw.run(), timeout=5)
    assert out.solved is True and out.winner == "cli-claude"
    # the LOSER's solver.cancel() was invoked (so its subprocess dies)
    assert "cli-codex" in cancelled


async def test_cancel_solver_is_noop_without_cancel_method():
    # a solver without a cancel() method — _cancel_solver must not raise.
    Swarm._cancel_solver(None)                       # None is safe
    Swarm._cancel_solver(object())                   # no cancel attr is safe

    class Boom:
        def cancel(self): raise RuntimeError("nope")
    Swarm._cancel_solver(Boom())                     # a throwing cancel is swallowed


async def test_coordinator_never_gives_up_rebootstraps_when_reason_dry(challenge, tmp_path: Path, monkeypatch):
    """CTF has a unique solution → when Reason produces NO new intents and nothing
    is running, the coordinator must RE-BOOTSTRAP a fresh attempt, not declare
    'exhausted' and stop. We bound the test with a finite budget; within it, at
    least one retry-bootstrap must have been spawned."""
    from muteki.solver.types import SolveOutcome

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1, stall_seconds=0.01,
                            wall_clock_budget=0.4)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])

    spawned_modes = []

    class FakeWorker:
        def __init__(self, engine, mode):
            self.solver_id = f"cli-{engine}"
            self.mode = mode
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "miss")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        spawned_modes.append(mode)
        return FakeWorker(engine, mode)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)
    # Reason ALWAYS dry — never proposes an intent. Old code would stop immediately.
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())

    out = await asyncio.wait_for(sw.run(), timeout=5)
    # the run never "solved", but it kept re-bootstrapping fresh attempts instead of
    # giving up after the first dry Reason: >1 bootstrap spawned.
    assert spawned_modes.count("bootstrap") >= 2, spawned_modes
    assert out.solved is False


def test_retry_goal_lists_dead_ends(challenge, tmp_path):
    """_retry_goal surfaces the board's ruled-out paths (so a re-bootstrap doesn't
    retry them) AND pushes the worker to DRIVE a lead to a working exploit — not the
    old 're-examine / try a different angle' wording that made retry workers conclude
    after a few probes (run-7349)."""
    sw = _coordinator_swarm(challenge, tmp_path)
    try:
        sw.shared_graph.add_dead_end(actor="cli-claude", reason="SQLi on /login is sanitized")
    except Exception:
        pass
    goal = sw._retry_goal()
    assert "HAS a solution" in goal
    # it must push depth-to-exploit, not shallow reconsideration
    assert "exploit" in goal.lower()
    assert "do not stop at recon" in goal.lower() or "do not conclude after a few" in goal.lower()
    # and it lists the ruled-out dead-end so the worker doesn't retry it
    assert "SQLi on /login" in goal


def test_make_cli_worker_assigns_unique_labels(challenge, tmp_path):
    """Each spawned worker gets a UNIQUE solver_id (so the deck draws one lane per
    worker), keeping the cli-<engine> prefix (so the engine badge still resolves).
    The first worker of an engine keeps the bare cli-<engine> for back-compat."""
    sw = _coordinator_swarm(challenge, tmp_path)
    a = sw._make_cli_worker("claude", mode="bootstrap")
    b = sw._make_cli_worker("claude", mode="explore")
    c = sw._make_cli_worker("codex", mode="bootstrap")
    d = sw._make_cli_worker("codex", mode="explore")
    assert a.solver_id == "cli-claude"      # 1st claude → bare prefix
    assert b.solver_id == "cli-claude-2"    # 2nd claude → distinct
    assert c.solver_id == "cli-codex"       # 1st codex → bare prefix
    assert d.solver_id == "cli-codex-2"
    # all distinct → distinct lanes on the deck
    assert len({a.solver_id, b.solver_id, c.solver_id, d.solver_id}) == 4
    # prefix preserved so workerEngine() detects the engine from the id alone
    assert all("claude" in s for s in (a.solver_id, b.solver_id))
    assert all("codex" in s for s in (c.solver_id, d.solver_id))
    # swarm sub-workers are worker-scoped: their end is WORKER_FINISHED, not the run.
    assert all(w.lifecycle_scope == "worker" for w in (a, b, c, d))


async def test_coordinator_emits_single_run_level_finished(challenge, tmp_path, monkeypatch):
    """The run-7345 bug: a worker ending must NOT mark the whole run finished. Each
    sub-worker emits WORKER_FINISHED; the coordinator emits exactly ONE run-level
    RUN_FINISHED when it actually settles (here: a worker solves)."""
    from muteki.solver.types import SolveOutcome
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event, EventType

    captured = []
    bus = EventBus()

    async def _sink(ev):
        captured.append(ev)
    bus.add_sink(_sink)

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=2, bus=bus)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])

    class FakeWorker:
        """Mimics a real CliSolver's lifecycle emits: each worker fires its OWN
        WORKER_FINISHED (worker-level), never a run-level RUN_FINISHED."""
        def __init__(self, engine, solved):
            self.solver_id = f"cli-{engine}"
            self._solved = solved
        async def run(self):
            await asyncio.sleep(0)
            flag = "flag{win}" if self._solved else None
            await bus.emit(Event(
                event_type=EventType.WORKER_FINISHED, run_id=sw.run_id,
                challenge_id=challenge.id, solver_id=self.solver_id,
                payload={"flag": flag, "solved": self._solved}))
            return SolveOutcome(self._solved, flag, 1, None,
                                "solved" if self._solved else "miss")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        return FakeWorker(engine, solved=(engine == "claude"))
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    out = await sw.run()
    assert out.solved is True

    run_finished = [e for e in captured if e.event_type is EventType.RUN_FINISHED]
    worker_finished = [e for e in captured if e.event_type is EventType.WORKER_FINISHED]
    # the WHOLE run finishes exactly once, regardless of how many workers ended
    assert len(run_finished) == 1, f"expected 1 run-level RUN_FINISHED, got {len(run_finished)}"
    assert run_finished[0].payload.get("solved") is True
    assert run_finished[0].payload.get("flag") == "flag{win}"
    # the run-level RUN_FINISHED has no solver_id (it belongs to the run, not a worker)
    assert not run_finished[0].solver_id
    # both workers reported their own worker-level end
    assert len(worker_finished) >= 1


async def test_m11_cancelled_coordinator_finalizes_and_closes_graph(
        challenge, tmp_path, monkeypatch):
    """M11: when the coordinator is CANCELLED mid-run, it must still finalize — close
    the shared_graph (release the SQLite WAL/-shm handles) and emit the run-level
    RUN_FINISHED — instead of leaking them (the cleanup used to sit after the finally,
    on the normal-return path only)."""
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event, EventType

    captured = []
    bus = EventBus()
    async def _sink(ev):
        captured.append(ev)
    bus.add_sink(_sink)

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1, bus=bus)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])

    closed = {"n": 0}
    if sw.shared_graph is not None:
        orig_close = sw.shared_graph.close
        def _spy_close():
            closed["n"] += 1
            return orig_close()
        monkeypatch.setattr(sw.shared_graph, "close", _spy_close)

    class HangWorker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            await asyncio.sleep(3600)  # hang until cancelled

    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, *, mode, intent_goal="", intent_id="": HangWorker(engine))

    task = asyncio.create_task(sw.run())
    await asyncio.sleep(0.1)        # let it spawn + start hanging
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass

    assert sw._run_finalized is True, "a cancelled coordinator must finalize"
    if sw.shared_graph is not None:
        assert closed["n"] >= 1, "shared_graph must be closed on a cancelled run"
    run_finished = [e for e in captured if e.event_type is EventType.RUN_FINISHED]
    assert len(run_finished) == 1, "a cancelled run still emits exactly one RUN_FINISHED"
    # L3: the coordinator's bus sinks are detached on finalize (no leak on a reused bus)
    assert sw._coord_sinks == [], "coordinator bus sinks must be detached on finalize"


async def test_finalize_merges_shared_graph_flags_before_finished(tmp_path):
    """Coordinator finalization is the last line of defense: if flags were already
    persisted in the shared graph, RUN_FINISHED must carry them even when there is
    no winner.json-producing worker outcome in hand."""
    from muteki.core.event_bus import EventBus
    from muteki.core.events import EventType

    ch = Challenge(id="c-multi", name="multi", category="web", points=0,
                   description="collect two", flag_format=r"flag\{[^}]+\}",
                   expected_flags=2, multi_flag=True)
    events = []
    bus = EventBus()

    async def _sink(ev):
        events.append(ev)
    bus.add_sink(_sink)

    sw = _coordinator_swarm(ch, tmp_path, bus=bus)
    assert sw.shared_graph is not None
    sw.shared_graph.flag_found(actor="cli-a", flag="flag{one}")
    sw.shared_graph.flag_found(actor="cli-b", flag="flag{two}")

    await sw._finalize_coordinator_run(
        winner=None, flag=None, goal_complete=False, per_solver={})

    fin = [e for e in events if e.event_type is EventType.RUN_FINISHED][-1]
    assert fin.payload["solved"] is True
    assert fin.payload["flag"] == "flag{one}"
    assert fin.payload["flags"] == ["flag{one}", "flag{two}"]


# ── lease + OODA refactor: stall-kill removed, lease closes the loop ──────────

async def test_coordinator_does_not_steer_kill_on_global_fact_stall(
        challenge, tmp_path: Path, monkeypatch):
    """The run-7352 fix: a worker that emits no GLOBAL verified fact must NOT be
    steer-killed. The old design called request_steer() after stall_seconds of no
    global fact, which murdered freshly-spawned workers mid-exploit. There is no
    stall-kill anymore — a worker runs until it finishes on its own."""
    from muteki.solver.types import SolveOutcome

    # stall_seconds tiny: under the OLD code this would steer-kill the worker fast.
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1, stall_seconds=0.01,
                            wall_clock_budget=2.0)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())

    steered = {"n": 0}

    class SlowNoFactWorker:
        solver_id = "cli-claude"
        def request_steer(self):           # coordinator must NEVER call this for reclaim
            steered["n"] += 1
        async def run(self):
            await asyncio.sleep(0.5)        # works a while, emits no fact
            return SolveOutcome(False, None, 1, None, "explored, no flag")

    monkeypatch.setattr(sw, "_make_cli_worker", lambda *a, **k: SlowNoFactWorker())
    await asyncio.wait_for(sw.run(), timeout=5)
    assert steered["n"] == 0, "coordinator must not steer-kill a worker on global fact stall"


# ── MIGRATION RED LINE (single-shot migration, DESIGN_single_shot_migration.md) ──
# This is the load-bearing invariant for reverting to the single-shot model.
# The run-7352 death spiral happened because a GLOBAL signal (global fact stall)
# decided a SINGLE worker's fate. The migration's red line: a worker's life is
# governed ONLY by its own clock (its own timeout / its own lease) — NEVER by a
# global signal (global fact count, global stall time, another worker's progress).
# If a future change re-introduces a global-progress reclaim path, this fails.
async def test_REDLINE_no_global_signal_kills_a_progressless_worker(
        challenge, tmp_path: Path, monkeypatch):
    """A worker that produces NO global fact for a long time, while the run is
    otherwise idle (no other progress anywhere), must run to its OWN natural
    completion — the coordinator must neither steer it nor cancel its task. This
    guards the single-shot migration against re-growing the run-7352 stall-kill leg
    in any form (steer OR cancel)."""
    from muteki.solver.types import SolveOutcome

    # tiny stall_seconds: the OLD stall-kill would have fired almost immediately.
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1, stall_seconds=0.01,
                            wall_clock_budget=2.0)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())

    events = {"steered": 0, "cancelled": 0, "ran_to_completion": False}

    class SlowNoFactWorker:
        solver_id = "cli-claude"
        def request_steer(self):       # global logic must NEVER steer for reclaim
            events["steered"] += 1
        def cancel(self):              # global logic must NEVER cancel for reclaim
            events["cancelled"] += 1
        async def run(self):
            try:
                # emits no global fact for a meaningful stretch (deep-exploit setup)
                await asyncio.sleep(0.6)
            except asyncio.CancelledError:
                # if the coordinator cancelled the task on a global signal, that's
                # the death-spiral leg — record it and re-raise.
                events["cancelled"] += 1
                raise
            events["ran_to_completion"] = True
            return SolveOutcome(False, None, 1, None, "explored, no flag")

    monkeypatch.setattr(sw, "_make_cli_worker", lambda *a, **k: SlowNoFactWorker())
    await asyncio.wait_for(sw.run(), timeout=5)

    assert events["steered"] == 0, \
        "RED LINE: a global signal must not steer a progressless worker (run-7352)"
    assert events["cancelled"] == 0, \
        "RED LINE: a global signal must not cancel a progressless worker (run-7352)"
    assert events["ran_to_completion"], \
        "worker must reach its own natural completion, governed by its own clock"


def test_make_cli_worker_explore_gets_short_timeout(challenge, tmp_path):
    """Dual to the above: the ONLY backstop that frees a slot held by a stuck explore
    is its SHORT per-turn timeout. explore must get explore_timeout; bootstrap keeps
    the long default (whole-challenge rush)."""
    sw = _coordinator_swarm(challenge, tmp_path, explore_timeout=720)
    boot = sw._make_cli_worker("claude", mode="bootstrap")
    expl = sw._make_cli_worker("claude", mode="explore",
                               intent_goal="probe", intent_id="I1-abc")
    assert expl.timeout == 720, "explore worker must get the short explore_timeout"
    assert boot.timeout == 2400, "bootstrap worker keeps the long default timeout"
    assert expl.timeout < boot.timeout


def test_open_intents_includes_expired_lease(challenge, tmp_path):
    """lease closure: an intent whose claim LEASE EXPIRED (worker died holding it)
    must be re-offered by _open_intents, else a stuck worker orphans its intent
    forever. A still-live claim must NOT be re-offered."""
    import time
    sw = _coordinator_swarm(challenge, tmp_path)
    g = sw.shared_graph
    g.propose_intent(actor="reason", intent_id="I-open", goal="never claimed")
    g.propose_intent(actor="reason", intent_id="I-live", goal="claimed, live lease")
    g.propose_intent(actor="reason", intent_id="I-dead", goal="claimed, expired lease")
    # live claim (long lease) — must stay hidden
    assert g.claim_intent(worker="w1", intent_id="I-live", lease_s=1000.0)
    # expired claim: claim with a lease already in the past
    assert g.claim_intent(worker="w2", intent_id="I-dead", lease_s=-1.0)

    open_ids = {i["intent_id"] for i in sw._open_intents()}
    assert "I-open" in open_ids          # never claimed → available
    assert "I-dead" in open_ids          # lease expired → re-offered (the fix)
    assert "I-live" not in open_ids      # live claim → not re-offered


def test_conclude_intent_lease_fencing(challenge, tmp_path):
    """owner/lease fencing: a LATE worker concluding an intent whose lease already
    lapsed (and which a new worker may now own) must NOT clobber the fresh claim.
    The conclusion event is still recorded, but the table state is not flipped."""
    import time
    sw = _coordinator_swarm(challenge, tmp_path)
    g = sw.shared_graph
    g.propose_intent(actor="reason", intent_id="I1-x", goal="g")
    # worker A claims with an already-expired lease (simulates a hung/slow worker)
    g.claim_intent(worker="A", intent_id="I1-x", lease_s=-1.0)
    # worker B re-claims it (lease expired → allowed) with a live lease
    assert g.claim_intent(worker="B", intent_id="I1-x", lease_s=1000.0)
    # late worker A tries to conclude as dead_end — must be FENCED (B owns it now)
    g.conclude_intent(actor="A", intent_id="I1-x", result="dead_end")
    with g._lock:
        row = g._conn.execute(
            "SELECT status, worker FROM intents WHERE intent_id='I1-x'").fetchone()
    assert row[0] == "claimed", "late conclude must not flip a re-claimed intent to done"
    assert row[1] == "B", "the fresh owner must remain B"


def test_conclude_intent_solved_always_wins(challenge, tmp_path):
    """The fence exempts a SOLVED conclusion: a real flag ends the run regardless of
    lease state, so it must always flip the intent to done."""
    sw = _coordinator_swarm(challenge, tmp_path)
    g = sw.shared_graph
    g.propose_intent(actor="reason", intent_id="I2-x", goal="g")
    g.claim_intent(worker="A", intent_id="I2-x", lease_s=-1.0)  # expired
    g.conclude_intent(actor="A", intent_id="I2-x", result="solved")
    with g._lock:
        row = g._conn.execute(
            "SELECT status FROM intents WHERE intent_id='I2-x'").fetchone()
    assert row[0] == "done", "a solved conclusion must always win, even on an expired lease"


def test_supersede_open_intents_retires_obsolete_asks(challenge, tmp_path):
    """run-11190 convergence fix #1: once the operator supplies a resource, the open
    'ask the operator for X' intents are obsolete and must be retired so fresh
    workers stop re-claiming them. supersede matches by goal substring and only
    touches open / expired-lease rows — a LIVE claim is left alone, and unrelated
    intents are untouched."""
    sw = _coordinator_swarm(challenge, tmp_path)
    g = sw.shared_graph
    g.propose_intent(actor="reason", intent_id="I-ask1",
                     goal="Request the operator for the L2 SSH password")
    g.propose_intent(actor="reason", intent_id="I-ask2",
                     goal="Submit L1 flag on the BreachLab dashboard to unlock L2")
    g.propose_intent(actor="reason", intent_id="I-solve",
                     goal="Recover scattered secrets via GitHub dorking")
    g.propose_intent(actor="reason", intent_id="I-live",
                     goal="ask the operator something — but actively worked")
    g.claim_intent(worker="w-live", intent_id="I-live", lease_s=1000.0)  # live

    killed = g.supersede_open_intents(actor="coordinator", match="operator",
                                      reason="operator supplied L2 password")
    assert "I-ask1" in killed
    assert "I-live" not in killed, "a live claim must NOT be superseded"
    # the matched open intent is now done → no longer re-offered
    open_ids = {i["intent_id"] for i in sw._open_intents()}
    assert "I-ask1" not in open_ids
    assert "I-solve" in open_ids, "an unrelated solve intent must remain open"
    assert "I-live" not in open_ids  # live claim was already hidden

    # a second needle ('dashboard') retires the other obsolete ask
    killed2 = g.supersede_open_intents(actor="coordinator", match="dashboard")
    assert "I-ask2" in killed2
    assert "I-ask2" not in {i["intent_id"] for i in sw._open_intents()}


def test_reopen_false_positive_does_not_revive_superseded_intent(challenge, tmp_path):
    """#11 regression: marking a flag a false positive must reopen the SOLVED intents
    only — NOT the 'ask the operator for X' intents that were SUPERSEDED when the
    operator supplied the resource. The old reopen flipped EVERY status='done' row back
    to open, resurrecting the retired asks (run-11190 238-worker 'request the password'
    loop came back on a mark-false)."""
    sw = _coordinator_swarm(challenge, tmp_path)
    g = sw.shared_graph
    # a real solve
    fs = g.add_evidence(actor="cli-claude", source="claude", fact="real", verified=True)
    g.propose_intent(actor="reason", intent_id="I-solve", goal="exploit /login")
    g.conclude_intent(actor="cli-claude", intent_id="I-solve",
                      result="solved", to_fact_seq=fs)
    # an ask-operator intent that the operator obsoleted → superseded (status='done')
    g.propose_intent(actor="reason", intent_id="I-ask",
                     goal="Request the operator for the L2 SSH password")
    superseded = g.supersede_open_intents(actor="coordinator", match="operator")
    assert "I-ask" in superseded
    # a barren explored intent (also 'done', result not solved)
    g.propose_intent(actor="reason", intent_id="I-barren", goal="brute /admin")
    g.conclude_intent(actor="cli-claude", intent_id="I-barren", result="explored")

    info = g.reopen_after_false_positive(actor="operator", flag="flag{fake}")
    assert info["reopened"] == ["I-solve"], \
        "only the solved intent reopens; superseded/barren stay retired"
    # confirm the superseded ask did NOT come back as open
    open_ids = {i["intent_id"] for i in sw._open_intents()}
    assert "I-ask" not in open_ids
    assert "I-barren" not in open_ids
    assert "I-solve" in open_ids


async def test_drain_hitl_does_not_supersede_submit_intent(challenge, tmp_path):
    """#12 regression: an unrelated operator hint must NOT retire a legitimate
    in-flight 'submit candidate flag to verifier' intent. The bare needle "submit"
    was removed from the supersede list — on a rate-limited chained-flag challenge
    (Specter), an irrelevant hint used to kill the active submission intent and stall
    the chain."""
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.hitl_inbox = asyncio.Queue()
    g = sw.shared_graph
    g.propose_intent(actor="reason", intent_id="I-submit",
                     goal="Submit candidate flag to the verifier endpoint")
    g.propose_intent(actor="reason", intent_id="I-ask",
                     goal="Request the operator for the dashboard token")

    # an unrelated operator hint flows through _drain_hitl → triggers supersede
    await _drain_one(sw, {"action": "hint", "text": "try port 8080 next"})

    open_ids = {i["intent_id"] for i in sw._open_intents()}
    assert "I-submit" in open_ids, \
        "a 'submit candidate flag' intent must survive an unrelated operator hint"
    # the genuine ask-operator intent is still correctly retired by 'dashboard'
    assert "I-ask" not in open_ids


async def test_drain_hitl_hint_records_operator_directive(challenge, tmp_path):
    """B: an operator hint is now a FIRST-CLASS OperatorDirective (not a fake
    low-confidence candidate fact). It still binds a claimable directive-tagged
    intent the next worker batch picks up — but it is NOT injected as evidence."""
    sw = _coordinator_swarm(challenge, tmp_path)
    hint = "try /robots.txt and then /data/note.txt"

    await _drain_one(sw, {"target": "global", "action": "hint", "text": hint})

    assert hint in sw._next_worker_guidance

    # the directive is recorded on the operator_directives table, active + bound
    directives = sw.shared_graph.operator_directives()
    assert any(d["text"] == hint and d["action"] == "hint" for d in directives)
    bound = [d for d in directives if d["text"] == hint][0]
    assert bound["status"] == "bound"
    assert hint in sw.shared_graph.active_operator_directive_texts()

    # it must NOT have become a fake candidate fact (design §12)
    fact_events = [
        e for e in sw.shared_graph.events()
        if e.get("kind") == "fact_added"
        and (e.get("payload") or {}).get("fact") == f"Operator hint: {hint}"
    ]
    assert not fact_events, "operator hint must NOT be a candidate fact anymore"

    # it binds a claimable directive-tagged intent
    assert any(goal == hint for goal in sw.shared_graph.open_goal_texts())


async def test_drain_hitl_directive_classification_recorded(challenge, tmp_path):
    """F: a worker hand-raise (external_blocker) is persisted as a classified
    hitl_request so the deck can distinguish it from an auto-resolving kind."""
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event, EventType, hitl_request_payload

    bus = EventBus()
    sw = _coordinator_swarm(challenge, tmp_path, bus=bus)

    async def _help_sink(ev):
        if ev.event_type is EventType.HITL_REQUEST:
            payload = dict(ev.payload or {})
            need_kind = str(payload.get("need_kind") or "external_blocker")
            need_text = str(payload.get("need", "")).strip()
            worker = str(payload.get("worker", ""))
            if need_text:
                sw.shared_graph.add_hitl_request(
                    worker=worker or "worker", need=need_text, need_kind=need_kind,
                    status=("awaiting_operator" if need_kind == "external_blocker"
                            else "auto_resolved"))

    bus.add_sink(_help_sink)
    await bus.emit(Event(
        event_type=EventType.HITL_REQUEST, run_id=sw.run_id,
        challenge_id=challenge.id, solver_id="cli-claude",
        payload=hitl_request_payload("cli-claude", "need a VPS with 4444 open",
                                     kind="need_input")))
    # the classification persisted with awaiting_operator status
    rows = sw.shared_graph.events()
    classified = [e for e in rows if e.get("kind") == "hitl_classified"]
    assert classified, "hand-raise must be persisted as a classified hitl_request"
    assert classified[0]["payload"]["need_kind"] == "external_blocker"


def test_coordinator_rechecks_external_blocker_before_pausing(challenge, tmp_path):
    sw = _coordinator_swarm(challenge, tmp_path)

    assert sw._rechecked_need_kind(
        "I am unsure whether to try JWT first or upload first",
        "external_blocker",
    ) == "worker_uncertainty"
    assert sw._rechecked_need_kind(
        "target returns connection refused and the instance may be expired",
        "external_blocker",
    ) == "external_blocker"


async def test_drain_hitl_mark_false_invalidates_only_target_flag_live(challenge, tmp_path):
    """A live run receiving mark_false must immediately remove only the selected
    flag and reopen only the intent linked to that flag."""
    from muteki.core.event_bus import EventBus
    from muteki.core.events import EventType

    bus = EventBus()
    events = []
    bus.add_sink(lambda ev: events.append(ev) or _noop())
    sw = _coordinator_swarm(challenge, tmp_path, bus=bus)
    sw._found_flags = ["flag{a}", "flag{b}", "flag{c}"]
    g = sw.shared_graph
    g.propose_intent(actor="reason", intent_id="I-a", goal="get flag a")
    g.propose_intent(actor="reason", intent_id="I-b", goal="get flag b")
    g.conclude_intent(actor="cli-a", intent_id="I-a", result="solved")
    g.conclude_intent(actor="cli-b", intent_id="I-b", result="solved")
    g.flag_found(actor="cli-a", flag="flag{a}", intent_id="I-a")
    g.flag_found(actor="cli-b", flag="flag{b}", intent_id="I-b")

    await _drain_one(sw, {"target": "global", "action": "mark_false",
                          "flag": "flag{b}", "text": "flag{b}"})

    assert sw._found_flags == ["flag{a}", "flag{c}"]
    assert g.snapshot().flags == ["flag{a}"]
    open_ids = {i["intent_id"] for i in sw._open_intents()}
    assert "I-b" in open_ids
    assert "I-a" not in open_ids
    assert any(
        e.event_type is EventType.BLACKBOARD_DELTA
        and (e.payload or {}).get("kind") == "flag_invalidated"
        and (e.payload or {}).get("flag") == "flag{b}"
        for e in events
    )


# ── 缺陷4: standing guidance LRU + clear (single-shot migration follow-up) ────
async def _drain_one(sw, cmd):
    """Push one HITL cmd, run the drain briefly, stop it."""
    sw.hitl_inbox = sw.hitl_inbox or asyncio.Queue()
    await sw.hitl_inbox.put(cmd)
    t = asyncio.create_task(sw._drain_hitl())
    await asyncio.sleep(0.03)
    t.cancel()
    try:
        await t
    except asyncio.CancelledError:
        pass


async def test_m3_bare_hint_without_resource_does_not_supersede(challenge, tmp_path):
    """M3: a contentless operator command (no text/url/standing) must NOT run the
    ask-operator supersede sweep — its broad needles (operator/unlock/dashboard)
    could wrongly retire a legitimate in-flight intent on an unrelated hint."""
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.hitl_inbox = asyncio.Queue()
    g = sw.shared_graph
    g.propose_intent(actor="reason", intent_id="I-ask",
                     goal="Request the operator for the dashboard token")
    # a resume action carries no resource → supersede must NOT fire
    await _drain_one(sw, {"action": "resume", "text": ""})
    open_ids = {i["intent_id"] for i in sw._open_intents()}
    assert "I-ask" in open_ids, "a contentless command must not retire ask-operator intents"


async def test_m5_solver_scoped_command_only_clears_that_workers_help(challenge, tmp_path):
    """M5: a hint scoped solver:<id> must only clear THAT worker's pending help — a
    hint addressed to worker B must not wipe worker A's still-unmet blocker."""
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.hitl_inbox = asyncio.Queue()
    sw._pending_help = [
        {"worker": "cli-codex-1", "need": "need a VPS"},
        {"worker": "cli-codex-2", "need": "need the dashboard token"},
    ]
    await _drain_one(sw, {"action": "hint", "text": "use http on 8080",
                          "target": "solver:cli-codex-2"})
    workers = {h["worker"] for h in sw._pending_help}
    assert "cli-codex-1" in workers, "worker A's unmet blocker must survive a B-scoped hint"
    assert "cli-codex-2" not in workers, "the targeted worker's ask is cleared"


async def test_dismiss_clears_help_unfreezes_and_deadends(challenge, tmp_path):
    """Operator DISMISSES a hand-raise without supplying the resource: the pending
    ask is cleared, the swarm unpauses (operator_event set, _operator_paused False),
    and a dead-end is recorded so a re-spawned worker doesn't immediately re-raise."""
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.hitl_inbox = asyncio.Queue()
    sw._operator_event = asyncio.Event()
    sw._operator_paused = True
    sw._pending_help = [
        {"worker": "cli-codex-1", "need": "need a public VPS for reverse shell"},
        {"worker": "cli-claude-2", "need": "target seems expired"},
    ]
    deadends = []
    if sw.insight is not None:
        orig = sw.insight.dead_end
        async def _spy(by, reason): deadends.append(reason); await orig(by, reason)
        sw.insight.dead_end = _spy  # type: ignore[assignment]

    await _drain_one(sw, {"action": "dismiss", "target": "global"})

    assert sw._pending_help == [], "global dismiss clears all pending help"
    assert sw._operator_paused is False, "dismiss must unfreeze the swarm"
    assert sw._operator_event.is_set(), "dismiss must wake the coordinator"
    if sw.insight is not None:
        assert any("dismissed" in d for d in deadends), "a dead-end is recorded so it won't re-raise"


async def test_dismiss_scoped_only_clears_that_worker(challenge, tmp_path):
    """A solver-scoped dismiss clears only that worker's ask, leaving others pending."""
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.hitl_inbox = asyncio.Queue()
    sw._operator_event = asyncio.Event()
    sw._pending_help = [
        {"worker": "cli-codex-1", "need": "need a VPS"},
        {"worker": "cli-claude-2", "need": "need a token"},
    ]
    await _drain_one(sw, {"action": "dismiss", "target": "solver:cli-codex-1"})
    workers = {h["worker"] for h in sw._pending_help}
    assert workers == {"cli-claude-2"}, "only the scoped worker's ask is dismissed"


def test_m6_help_sink_dedups_same_blocker_and_caps(challenge, tmp_path):
    """M6: the same blocker raised by many workers is deduped on (worker, need), and
    the pending list is bounded so a never-give-up run can't grow it unbounded."""
    from muteki.swarm.swarm import _PENDING_HELP_MAX
    from muteki.core.events import EventType, hitl_request_payload
    from muteki.core.event_bus import Event

    async def _run():
        sw = _coordinator_swarm(challenge, tmp_path)
        # build the help sink exactly as _run_coordinator does
        async def _help_sink(ev):
            if ev.event_type is EventType.HITL_REQUEST:
                payload = dict(ev.payload or {})
                key = (str(payload.get("worker", "")), str(payload.get("need", "")).strip())
                for h in sw._pending_help:
                    if (str(h.get("worker", "")), str(h.get("need", "")).strip()) == key:
                        break
                else:
                    sw._pending_help.append(payload)
                    if len(sw._pending_help) > _PENDING_HELP_MAX:
                        del sw._pending_help[: len(sw._pending_help) - _PENDING_HELP_MAX]
        # same blocker from 5 distinct workers but identical (worker,need) repeats
        for _ in range(5):
            await _help_sink(Event(event_type=EventType.HITL_REQUEST, run_id="r",
                payload=hitl_request_payload("cli-codex-1", "need a VPS")))
        assert len(sw._pending_help) == 1, "an identical (worker,need) ask dedups to one"
        # many DISTINCT blockers → capped
        for i in range(_PENDING_HELP_MAX + 10):
            await _help_sink(Event(event_type=EventType.HITL_REQUEST, run_id="r",
                payload=hitl_request_payload(f"w{i}", f"blocker {i}")))
        assert len(sw._pending_help) <= _PENDING_HELP_MAX, "pending help must be bounded"

    asyncio.run(_run())


def test_m1_insight_bus_dedups_repeated_guidance():
    """M1: an operator hint storm (identical guidance re-sent N times) must not flood
    the bounded history — each copy would evict a real VERIFIED_FACT / DEAD_END and
    replay to every cold subscriber. An identical trailing guidance is dropped; a
    fact in between, or a changed hint, still publishes."""
    from muteki.swarm.insight_bus import InsightBus, InsightKind

    async def _run():
        bus = InsightBus("run-kp")
        for _ in range(11):
            await bus.guidance("try /admin", action="hint", target="global")
        guidance = [i for i in bus.history if i.kind is InsightKind.GUIDANCE]
        assert len(guidance) == 1, "11 identical hints must collapse to one in history"
        # a different hint still publishes
        await bus.guidance("try /api instead", action="hint", target="global")
        assert len([i for i in bus.history if i.kind is InsightKind.GUIDANCE]) == 2
        # a hint repeated AFTER other activity re-broadcasts (not the most-recent one)
        await bus.fact("cli-1", "creds are admin/admin", "a1")
        await bus.guidance("try /admin", action="hint", target="global")
        assert len([i for i in bus.history if i.kind is InsightKind.GUIDANCE]) == 3

    asyncio.run(_run())


async def test_defect4_standing_lru_caps_count(challenge, tmp_path):
    """defect-4: standing guidance is LRU-capped so the cumulative text can't bloat
    every new worker's prompt unbounded (the 36k-token claude empty-exit)."""
    from muteki.swarm.swarm import _STANDING_MAX
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.hitl_inbox = asyncio.Queue()
    for i in range(_STANDING_MAX + 5):
        await _drain_one(sw, {"action": "hint", "text": f"hint-{i}", "standing": True})
    assert len(sw._standing_guidance) == _STANDING_MAX           # capped
    assert sw._standing_guidance[-1] == f"hint-{_STANDING_MAX + 4}"  # most-recent kept
    assert "hint-0" not in sw._standing_guidance                  # oldest evicted


async def test_defect4_clear_standing(challenge, tmp_path):
    """defect-4: clear_standing wipes all (or one by exact text) so an operator can
    retract a stale correction — the list was only-grew before."""
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.hitl_inbox = asyncio.Queue()
    await _drain_one(sw, {"action": "hint", "text": "a", "standing": True})
    await _drain_one(sw, {"action": "hint", "text": "b", "standing": True})
    assert sw._standing_guidance == ["a", "b"]
    # clear one by text
    await _drain_one(sw, {"action": "clear_standing", "text": "a"})
    assert sw._standing_guidance == ["b"]
    # clear all
    await _drain_one(sw, {"action": "clear_standing"})
    assert sw._standing_guidance == []


def test_defect4_standing_block_char_budget(challenge, tmp_path):
    """defect-4: even within the count cap, the per-worker injected block is bounded
    by a char budget (most-recent hints win)."""
    from muteki.solver.cli_solver import CliSolver
    s = CliSolver(None, challenge, driver=None, engine="claude", kb=False)
    s._standing_guidance = ["x" * 3000, "y" * 3000, "z-newest"]  # 6KB+ > 4KB budget
    block = s._standing_block()
    assert "z-newest" in block                       # newest always kept
    assert len(block) < s._STANDING_CHAR_BUDGET + 200  # bounded (+ header slack)
    assert block.count("x" * 3000) == 0              # oldest dropped over budget


# ── M-3: intent-level HITL (single-shot migration) ───────────────────────────
async def test_m3_redirect_reaches_next_spawned_worker(challenge, tmp_path, monkeypatch):
    """Single-shot migration M-3: a non-standing redirect/hint can't steer a live
    worker anymore, so it must reach the NEXT spawned worker — the redirect url as
    the new target override, the text as one-shot guidance — and the guidance is
    consumed (one-shot), not re-applied to every future worker."""
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.hitl_inbox = asyncio.Queue()

    # operator drops a non-standing redirect.
    await sw.hitl_inbox.put({"action": "redirect", "text": "decode the cookie, not brute force",
                             "url": "http://new-target:9000", "target": "global"})
    drain = asyncio.create_task(sw._drain_hitl())
    await asyncio.sleep(0.05)            # let the drain consume it
    drain.cancel()
    try:
        await drain
    except asyncio.CancelledError:
        pass

    assert sw._target_redirect == "http://new-target:9000"
    assert "decode the cookie, not brute force" in sw._next_worker_guidance

    # the next spawned worker gets BOTH: the redirect url as its target override and
    # the text folded into its guidance.
    w = sw._make_cli_worker("claude", mode="bootstrap")
    assert w._target_override == "http://new-target:9000"
    assert "decode the cookie, not brute force" in w._standing_guidance
    # one-shot: the coordinator's pending guidance is consumed after the spawn.
    assert sw._next_worker_guidance == []


# ── race-scout layer (DESIGN_race_scout_layer.md) ────────────────────────────
async def test_race_scout_fast_path_flag_wins(challenge, tmp_path, monkeypatch):
    """FAST PATH: a race worker captures the flag → the coordinator finishes via the
    race phase and NEVER enters the main coordinator loop (no _run_reason call)."""
    from muteki.solver.types import SolveOutcome
    sw = _coordinator_swarm(challenge, tmp_path, race_scout=True,
                            wall_clock_budget=2.0)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex", "cursor"])
    reason_calls = {"n": 0}
    async def fake_reason():
        reason_calls["n"] += 1
        return 0
    monkeypatch.setattr(sw, "_run_reason", fake_reason)

    class FakeWorker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
            self.timeout = 720
        async def run(self):
            await asyncio.sleep(0)
            solved = self.solver_id == "cli-codex"
            return SolveOutcome(solved, "flag{won}" if solved else None, 1, None,
                                "race", flags=["flag{won}"] if solved else None)
    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: FakeWorker(engine))

    out = await asyncio.wait_for(sw.run(), timeout=5)
    assert out.solved is True and out.flag == "flag{won}"
    assert out.winner == "cli-codex"
    assert reason_calls["n"] == 0, "fast path must skip the coordinator loop entirely"


async def test_race_scout_slow_path_hands_off_to_coordinator(challenge, tmp_path, monkeypatch):
    """SLOW PATH: no race worker captures the flag → their facts are on the graph and
    the coordinator falls through to the main solve loop (_run_reason IS called)."""
    from muteki.solver.types import SolveOutcome
    sw = _coordinator_swarm(challenge, tmp_path, race_scout=True,
                            wall_clock_budget=0.5)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex", "cursor"])
    reason_calls = {"n": 0}
    async def fake_reason():
        reason_calls["n"] += 1
        return 0
    monkeypatch.setattr(sw, "_run_reason", fake_reason)

    class FakeWorker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
            self.timeout = 720
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "no flag")  # nobody solves
    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: FakeWorker(engine))

    out = await asyncio.wait_for(sw.run(), timeout=5)
    assert out.solved is False
    assert reason_calls["n"] >= 1, "slow path must fall through to the main coordinator loop"


async def test_race_scout_disabled_does_not_race(challenge, tmp_path, monkeypatch):
    """race_scout=False → the race phase is skipped entirely; the first workers come
    from the main coordinator loop (bootstrap), not a parallel race round."""
    from muteki.solver.types import SolveOutcome
    sw = _coordinator_swarm(challenge, tmp_path, race_scout=False,
                            wall_clock_budget=0.3)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex", "cursor"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())
    phases = []
    class FakeWorker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
            self.timeout = 720
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "x")
    def fake_make(engine, **kw):
        phases.append(kw.get("timeout_override"))
        return FakeWorker(engine)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)
    await asyncio.wait_for(sw.run(), timeout=5)
    # no spawn carried a race timeout_override (the race phase never ran).
    assert all(t is None for t in phases)


def test_race_engines_subset_and_timeout(challenge, tmp_path):
    """config: race_engines restricts which engines race; race_timeout is the short
    per-worker timeout the race spawns use."""
    sw = _coordinator_swarm(challenge, tmp_path, race_scout=True,
                            race_engines=["claude"], race_timeout=300,
                            engines=["claude", "codex", "cursor"])
    assert sw.race_engines == ["claude"]          # cursor/codex dropped
    assert sw.race_timeout == 300
    w = sw._make_cli_worker("claude", mode="bootstrap", timeout_override=sw.race_timeout)
    assert w.timeout == 300                       # short race timeout applied


def test_stage_policy_overrides_race_budget_and_planner(challenge, tmp_path):
    sw = _coordinator_swarm(
        challenge, tmp_path,
        stage_policy={
            "race": {"enabled": True, "timeout": 123, "engines": ["claude"]},
            "coordinator": {"wall_clock_budget": 9},
            "budgets": {"max_total_workers": 7, "cost_budget_usd": 0.5},
        },
        llm_profiles={"planner": {"provider": "deepseek", "model": "planner-x"}},
        engines=["claude", "codex"],
    )
    assert sw.race_scout is True
    assert sw.race_timeout == 123
    assert sw.race_engines == ["claude"]
    assert sw.wall_clock_budget == 9
    assert sw.max_total_workers == 7
    assert sw.cost_budget_usd == 0.5
    assert sw.reason_model == "planner-x"


def test_stage_policy_reads_coordinator_key():
    """The coordinator stage policy is keyed "coordinator"; from_config reads it and
    round-trips it through model_dump. Unknown keys are ignored."""
    from muteki.swarm.stage_policy import StagePolicy
    sp = StagePolicy.from_config({"coordinator": {"wall_clock_budget": 7},
                                  "unknown_key": {"wall_clock_budget": 42}})
    assert sp.coordinator == {"wall_clock_budget": 7}
    assert sp.model_dump()["coordinator"] == {"wall_clock_budget": 7}


async def test_race_scout_empty_healthy_subset_returns_three_tuple(challenge, tmp_path):
    sw = _coordinator_swarm(challenge, tmp_path, race_scout=True,
                            race_engines=["codex"], engines=["claude", "codex"])
    got = await sw._run_race_scout(["claude"])
    assert got == (None, None, {})


async def test_race_scout_slow_path_emits_phase_transition(challenge, tmp_path, monkeypatch):
    from muteki.solver.types import SolveOutcome
    events = []

    class Bus:
        async def emit(self, ev):
            events.append(ev)
        def add_sink(self, sink):
            pass

    sw = _coordinator_swarm(challenge, tmp_path, race_scout=True,
                            wall_clock_budget=0.2, bus=Bus())
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())

    class FakeWorker:
        solver_id = "cli-claude"
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "no flag")

    monkeypatch.setattr(sw, "_make_cli_worker", lambda *a, **k: FakeWorker())
    await asyncio.wait_for(sw.run(), timeout=5)
    assert any(e.payload.get("kind") == "phase_transition" for e in events)


async def test_worker_budget_hard_gate_finishes_budget_exhausted(challenge, tmp_path, monkeypatch):
    from muteki.solver.types import SolveOutcome
    events = []

    class Bus:
        async def emit(self, ev):
            events.append(ev)
        def add_sink(self, sink):
            pass

    sw = _coordinator_swarm(challenge, tmp_path, start_workers=2,
                            max_total_workers=1, wall_clock_budget=0.2,
                            bus=Bus())
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])

    class FakeWorker:
        solver_id = "cli-claude"
        async def run(self):
            await asyncio.sleep(0.01)
            return SolveOutcome(False, None, 1, None, "done")

    def fake_make(*args, **kwargs):
        sw._reserve_worker_spawn()
        return FakeWorker()

    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)
    out = await asyncio.wait_for(sw.run(), timeout=5)
    assert out.reason == "budget_exhausted"
    assert any(e.payload.get("kind") == "worker_budget_exhausted" for e in events)


async def test_cost_budget_hard_gate_finishes_budget_exhausted(challenge, tmp_path, monkeypatch):
    from muteki.core.cost import CostController
    events = []

    class Bus:
        async def emit(self, ev):
            events.append(ev)
        def add_sink(self, sink):
            pass

    cost = CostController()
    cost._global.usd = 2.0
    sw = _coordinator_swarm(challenge, tmp_path, cost=cost,
                            cost_budget_usd=1.0, bus=Bus())
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    out = await asyncio.wait_for(sw.run(), timeout=5)
    assert out.reason == "budget_exhausted"
    assert any(e.payload.get("kind") == "cost_budget_exhausted" for e in events)


async def test_operator_stop_interrupts_race_scout_wait(challenge, tmp_path, monkeypatch):
    sw = _coordinator_swarm(challenge, tmp_path, race_scout=True,
                            race_engines=["claude"], engines=["claude"])
    sw._operator_event = asyncio.Event()
    cancelled = {"n": 0}

    class FakeWorker:
        solver_id = "cli-claude"
        async def run(self):
            await asyncio.sleep(10)
        def cancel(self):
            cancelled["n"] += 1

    monkeypatch.setattr(sw, "_make_cli_worker", lambda *a, **k: FakeWorker())

    async def stop_soon():
        await asyncio.sleep(0.02)
        sw._operator_stop = True
        sw._operator_event.set()

    stopper = asyncio.create_task(stop_soon())
    got = await asyncio.wait_for(sw._run_race_scout(["claude"]), timeout=1)
    await stopper
    assert got[0] is None
    assert cancelled["n"] >= 1


async def test_race_scout_no_global_signal_kills_worker(challenge, tmp_path, monkeypatch):
    """RED LINE: the race phase produces no flag and no facts, yet no race worker is
    cancelled by a global signal — each runs to its own natural exit (run-7352)."""
    from muteki.solver.types import SolveOutcome
    sw = _coordinator_swarm(challenge, tmp_path, race_scout=True,
                            stall_seconds=0.01, wall_clock_budget=0.5)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex", "cursor"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())
    cancelled = {"n": 0}
    class FakeWorker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
            self.timeout = 720
        def cancel(self):
            cancelled["n"] += 1
        async def run(self):
            await asyncio.sleep(0.05)             # works a bit, emits nothing
            return SolveOutcome(False, None, 1, None, "no fact")
    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: FakeWorker(engine))
    await asyncio.wait_for(sw.run(), timeout=5)
    assert cancelled["n"] == 0, "RED LINE: a global signal must not cancel a race worker"


def test_nofact_deadend_conclude_is_not_redispatched(challenge, tmp_path):
    """run-11190 convergence fix #2 (DB contract the caller fix relies on): a no-fact
    dead_end conclude (to_fact_seq=None) must flip the intent to 'done' so its expired
    lease does NOT resurrect the same stale direction. The caller bug was in
    cli_solver._run_explore, which SKIPPED this conclude when `lfs is None` (no fact
    recorded) — leaving the row 'claimed' → lease expiry re-offered it forever
    (238-worker loop). The fix drops that gate; this asserts the DB side does the
    right thing for a None fact pointer so the unconditional conclude is safe."""
    sw = _coordinator_swarm(challenge, tmp_path)
    g = sw.shared_graph
    g.propose_intent(actor="reason", intent_id="I-nf", goal="a doomed direction")
    # worker claims with an already-expired lease (so without the fix the row would
    # be re-offered), then concludes dead_end with NO fact pointer.
    g.claim_intent(worker="wx", intent_id="I-nf", lease_s=-1.0)
    g.conclude_intent(actor="wx", intent_id="I-nf", result="dead_end",
                      to_fact_seq=None)  # the no-fact path
    with g._lock:
        row = g._conn.execute(
            "SELECT status FROM intents WHERE intent_id='I-nf'").fetchone()
    assert row[0] == "done", "a no-fact dead_end conclude must still flip status to done"
    assert "I-nf" not in {i["intent_id"] for i in sw._open_intents()}, \
        "a concluded no-fact dead_end must NOT be re-dispatched on lease expiry"


# ── standing guidance reaches workers spawned AFTER the operator gave it ──────

def test_standing_guidance_injected_into_new_worker_turn1(challenge, tmp_path):
    """The VPS/SSH-hint bug: a worker spawned AFTER the operator's standing hint
    must carry it in its turn-1 prompt. Before, the coordinator didn't persist
    standing, so _make_cli_worker built workers with an empty _standing_block and
    late workers never saw the VPS info."""
    sw = _coordinator_swarm(challenge, tmp_path)
    # operator gave standing guidance earlier in the run (coordinator persisted it)
    sw._standing_guidance.append("reverse shell: ssh root@38.247.145.244 (VPS relay)")
    # a worker spawned NOW must inherit it
    w = sw._make_cli_worker("claude", mode="explore",
                            intent_goal="probe", intent_id="I1-x")
    block = w._standing_block()
    assert "38.247.145.244" in block, "new worker's turn-1 standing block must carry the VPS hint"
    prompt = w._build_explore_prompt()
    assert "38.247.145.244" in prompt, "the VPS hint must be in the worker's actual turn-1 prompt"


def test_drain_hitl_persists_standing_on_coordinator(challenge, tmp_path):
    """_drain_hitl must store a standing hint on the coordinator's canonical list
    (so future _make_cli_worker calls inherit it), not just broadcast it live."""
    import asyncio as _aio
    sw = _coordinator_swarm(challenge, tmp_path)
    inbox: _aio.Queue = _aio.Queue()
    sw.hitl_inbox = inbox

    async def drive():
        drain = _aio.create_task(sw._drain_hitl())
        await inbox.put({"target": "global", "action": "hint",
                         "text": "ssh root@1.2.3.4 for reverse shell", "standing": True})
        await _aio.sleep(0.05)
        drain.cancel()
        try:
            await drain
        except _aio.CancelledError:
            pass

    _aio.run(drive())
    assert any("1.2.3.4" in s for s in sw._standing_guidance), \
        "standing hint must be persisted on the coordinator for late-spawned workers"


def test_drain_hitl_stop_sets_operator_stop_and_wakes(challenge, tmp_path):
    """An operator `stop` (or `complete`) command flips _operator_stop and wakes the
    coordinator — the graceful-terminate lever for a run that never gates a flag
    (run-10070). It is distinct from a steer, which only guides workers."""
    import asyncio as _aio
    sw = _coordinator_swarm(challenge, tmp_path)
    inbox: _aio.Queue = _aio.Queue()
    sw.hitl_inbox = inbox
    sw._operator_event = _aio.Event()

    async def drive():
        drain = _aio.create_task(sw._drain_hitl())
        await inbox.put({"target": "global", "action": "stop"})
        await _aio.sleep(0.05)
        drain.cancel()
        try:
            await drain
        except _aio.CancelledError:
            pass

    _aio.run(drive())
    assert sw._operator_stop is True
    assert sw._operator_event.is_set()


def test_drain_hitl_steer_does_not_stop(challenge, tmp_path):
    """A normal steer/hint must NOT set _operator_stop — only stop/complete do."""
    import asyncio as _aio
    sw = _coordinator_swarm(challenge, tmp_path)
    inbox: _aio.Queue = _aio.Queue()
    sw.hitl_inbox = inbox
    sw._operator_event = _aio.Event()

    async def drive():
        drain = _aio.create_task(sw._drain_hitl())
        await inbox.put({"target": "global", "action": "hint", "text": "try /admin"})
        await _aio.sleep(0.05)
        drain.cancel()
        try:
            await drain
        except _aio.CancelledError:
            pass

    _aio.run(drive())
    assert sw._operator_stop is False


# ── worker raises its hand → coordinator pauses for the operator (not re-spawn) ──

async def test_coordinator_pauses_on_need_input_until_operator(challenge, tmp_path, monkeypatch):
    """A worker emitting HITL_REQUEST (NEED_INPUT) while the swarm goes idle must
    make the coordinator WAIT for the operator instead of re-bootstrapping into the
    same unsolvable-without-help wall. An operator command unblocks it."""
    from muteki.solver.types import SolveOutcome
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event, EventType, hitl_request_payload

    bus = EventBus()
    inbox: asyncio.Queue = asyncio.Queue()
    # inf budget: a worker explicitly asked, so the pause must wait for the operator
    # indefinitely (no budget-timeout fallback). The finite-budget timeout path is
    # covered separately by test_coordinator_need_input_pause_respects_finite_budget.
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1,
                            bus=bus, hitl_inbox=inbox,
                            wall_clock_budget=float("inf"))
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())  # never proposes

    spawns = {"n": 0}
    helped = {"v": False}  # flips True once the operator supplies the VPS

    class Worker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            await asyncio.sleep(0)
            if not helped["v"]:
                # blocked: raise a hand (the coordinator's sink records HITL_REQUEST)
                await bus.emit(Event(
                    event_type=EventType.HITL_REQUEST, run_id=sw.run_id,
                    challenge_id=challenge.id, solver_id=self.solver_id,
                    payload=hitl_request_payload(self.solver_id,
                                                 "need a VPS for reverse shell", kind="need_input")))
            return SolveOutcome(False, None, 1, None,
                                "blocked" if not helped["v"] else "tried")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        spawns["n"] += 1
        return Worker(engine)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    run_task = asyncio.create_task(sw.run())
    # wait until the coordinator has paused (pending help recorded + event cleared)
    paused = False
    for _ in range(300):
        await asyncio.sleep(0.02)
        if sw._pending_help and sw._operator_event is not None \
                and not sw._operator_event.is_set():
            paused = True
            break
    assert paused, "coordinator should have paused awaiting operator after a NEED_INPUT"
    paused_spawns = spawns["n"]
    # while paused it must NOT keep spawning retry-bootstraps into the wall
    await asyncio.sleep(0.2)
    assert spawns["n"] == paused_spawns, "coordinator must not re-spawn while awaiting operator"
    # operator supplies the VPS → unblocks; resumed workers no longer raise. Give a
    # finite budget so the (now never-give-up) loop terminates the test.
    helped["v"] = True
    sw.wall_clock_budget = 0.3
    await inbox.put({"target": "global", "action": "hint",
                     "text": "standing: ssh root@9.9.9.9", "standing": True})
    out = await asyncio.wait_for(run_task, timeout=15)
    assert out.solved is False
    assert spawns["n"] > paused_spawns, "after operator input the coordinator resumes spawning"


async def test_coordinator_pauses_on_need_input_while_BUSY(challenge, tmp_path, monkeypatch):
    """run-11189 regression: the pause must fire even when the swarm is NOT idle.

    The original pause was nested inside `if not tasks and not open_intents:`, so it
    only fired when the swarm had fully drained. But the never-give-up Reason engine
    keeps proposing intents, so `open_intents` is never empty — the swarm stays busy
    and the pause never fired (3 NEED_INPUTs, 0 awaiting_operator, 30 min of workers
    hurled at the same no-token wall). Here we FORCE the busy state: _run_reason keeps
    'proposing' and _open_intents always returns a non-empty list. The coordinator
    must STILL pause on a pending NEED_INPUT and stop spawning new workers."""
    from muteki.solver.types import SolveOutcome
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event, EventType, hitl_request_payload

    bus = EventBus()
    inbox: asyncio.Queue = asyncio.Queue()
    # inf budget — same reasoning as the idle-path test above: the explicit ask must
    # block until the operator acts.
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1,
                            bus=bus, hitl_inbox=inbox,
                            wall_clock_budget=float("inf"))
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    # BUSY path: Reason "proposes" something every round AND there is always an open
    # intent — so `not tasks and not open_intents` is NEVER true. The old idle-only
    # pause could never run here; the new top-of-loop pause must.
    async def _reason_proposes():
        return 1
    monkeypatch.setattr(sw, "_run_reason", lambda: _reason_proposes())
    monkeypatch.setattr(sw, "_open_intents",
                        lambda: [{"intent_id": "I1", "goal": "keep going"}])

    spawns = {"n": 0}
    helped = {"v": False}
    awaiting = {"seen": False}

    async def _bb_spy(ev: Event) -> None:
        if (ev.event_type == EventType.BLACKBOARD_DELTA
                and (ev.payload or {}).get("kind") == "awaiting_operator"):
            awaiting["seen"] = True
    bus.add_sink(_bb_spy)

    class Worker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            await asyncio.sleep(0)
            if not helped["v"]:
                await bus.emit(Event(
                    event_type=EventType.HITL_REQUEST, run_id=sw.run_id,
                    challenge_id=challenge.id, solver_id=self.solver_id,
                    payload=hitl_request_payload(
                        self.solver_id, "need the dashboard token", kind="need_input")))
            return SolveOutcome(False, None, 1, None, "blocked")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        spawns["n"] += 1
        return Worker(engine)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)
    # the explore-spawn path also claims intents on the shared graph; stub it so the
    # fake _open_intents doesn't need a live claim to round-trip.
    if sw.shared_graph is not None:
        monkeypatch.setattr(sw.shared_graph, "claim_intent",
                            lambda **kw: True)

    run_task = asyncio.create_task(sw.run())
    # wait until the coordinator pauses despite being "busy"
    paused = False
    for _ in range(400):
        await asyncio.sleep(0.02)
        if sw._pending_help and sw._operator_event is not None \
                and not sw._operator_event.is_set():
            paused = True
            break
    assert paused, "coordinator must pause on NEED_INPUT even while busy (intents open)"
    assert awaiting["seen"], "an awaiting_operator event must be emitted on the busy path"
    paused_spawns = spawns["n"]
    await asyncio.sleep(0.25)
    assert spawns["n"] == paused_spawns, \
        "while paused the coordinator must NOT spawn new workers into the wall"
    # operator responds → unblock; give a finite budget so the loop ends the test.
    helped["v"] = True
    sw.wall_clock_budget = 0.3
    await inbox.put({"target": "global", "action": "hint", "text": "token abc123"})
    out = await asyncio.wait_for(run_task, timeout=15)
    assert out.solved is False


async def test_coordinator_need_input_pause_respects_finite_budget(
        challenge, tmp_path, monkeypatch):
    """#8 regression: a NEED_INPUT pause under a FINITE wall_clock_budget (offline
    eval, no operator present) must still exhaust the budget and terminate rather than
    block forever on `_operator_event.wait()`. The old code waited unconditionally; the
    fix mirrors the barren pause's budget-aware wait_for + budget_exhausted break."""
    from muteki.solver.types import SolveOutcome
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event, EventType, hitl_request_payload

    bus = EventBus()
    inbox: asyncio.Queue = asyncio.Queue()
    # finite budget, and NO operator will ever respond — the pause must time out.
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1,
                            bus=bus, hitl_inbox=inbox, wall_clock_budget=0.4)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())  # never proposes

    budget_exhausted = {"seen": False}

    async def _bb_spy(ev: Event) -> None:
        if (ev.event_type == EventType.BLACKBOARD_DELTA
                and (ev.payload or {}).get("kind") == "budget_exhausted"):
            budget_exhausted["seen"] = True
    bus.add_sink(_bb_spy)

    class Worker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            await asyncio.sleep(0)
            await bus.emit(Event(
                event_type=EventType.HITL_REQUEST, run_id=sw.run_id,
                challenge_id=challenge.id, solver_id=self.solver_id,
                payload=hitl_request_payload(
                    self.solver_id, "need a VPS", kind="need_input")))
            return SolveOutcome(False, None, 1, None, "blocked")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        return Worker(engine)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    # No operator command is ever enqueued; the only way out is the budget timeout.
    out = await asyncio.wait_for(sw.run(), timeout=15)
    assert out.solved is False
    assert budget_exhausted["seen"], \
        "a finite-budget NEED_INPUT pause must emit budget_exhausted and terminate"


async def test_operator_pause_stops_spawning_resume_continues(challenge, tmp_path, monkeypatch):
    """#5 regression: an operator `pause` HITL command must SOFT-pause the coordinator
    — stop spawning NEW workers (without killing running ones or ending the run) —
    and a `resume` must continue. This is the meaningful pause for a single-shot swarm
    (the old behavior had NO swarm-level pause wired to the operator's pause command,
    so 'pause' looked like a no-op button)."""
    from muteki.solver.types import SolveOutcome
    from muteki.core.event_bus import EventBus
    # EventType must be imported INTO this function's scope: the _bb_spy sink below
    # references it, and EventBus.emit() runs every sink inside `try/except
    # Exception: pass` (a slow/failing sink must never wedge the publish). Without
    # the import the sink raised NameError on EVERY event — silently swallowed by
    # emit — so paused_seen never flipped and the assert failed regardless of the
    # (correct) production pause logic. This was the whole "flake".
    from muteki.core.events import EventType

    bus = EventBus()
    inbox: asyncio.Queue = asyncio.Queue()
    # barren_limit=0 DISABLES the no-progress backpressure pause (swarm.py guards it
    # behind `self.barren_limit > 0`), so this test isolates the OPERATOR pause: the
    # only thing that can pause the loop here is the operator's `pause` command, not
    # the autonomous barren-worker backpressure. Without this the fruitless 0.01s
    # mock workers would race the loop into a barren `collect_idle` park before the
    # operator command even arrived, making WHERE the loop sits nondeterministic.
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1,
                            bus=bus, hitl_inbox=inbox, barren_limit=0,
                            wall_clock_budget=float("inf"))
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())

    spawns = {"n": 0}
    paused_seen = {"v": False}

    async def _bb_spy(ev):
        if (ev.event_type == EventType.BLACKBOARD_DELTA
                and (ev.payload or {}).get("kind") == "operator_paused"):
            paused_seen["v"] = True
    bus.add_sink(_bb_spy)

    class Worker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            # sleep long enough (>> the test's 0.15s settle + poll cadence) that a
            # worker is reliably LIVE in asyncio.wait when the pause arrives, so the
            # pause is observed mid-flight (not only at an idle boundary).
            await asyncio.sleep(0.05)
            return SolveOutcome(False, None, 1, None, "tried")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        spawns["n"] += 1
        return Worker(engine)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    run_task = asyncio.create_task(sw.run())
    # let a few workers spawn, then PAUSE
    await asyncio.sleep(0.15)
    await inbox.put({"target": "global", "action": "pause"})
    # wait until the pause is observed (operator_paused delta on the bus) AND the
    # coordinator flag is set. _drain_hitl handles `pause` on a separate task, so
    # this is robust to whether the loop is mid-wait or between iterations.
    for _ in range(200):
        await asyncio.sleep(0.02)
        if paused_seen["v"] and sw._operator_paused:
            break
    assert paused_seen["v"], "an operator_paused blackboard delta must be emitted"
    assert sw._operator_paused is True
    paused_spawns = spawns["n"]
    # while paused, NO new workers spawn — even though the (single-shot) workers keep
    # finishing fruitlessly, the loop must park instead of re-bootstrapping.
    await asyncio.sleep(0.3)
    assert spawns["n"] == paused_spawns, "no new workers may spawn while paused"
    # RESUME → spawning continues; give a finite budget so the loop ends the test
    sw.wall_clock_budget = 0.3
    await inbox.put({"target": "global", "action": "resume"})
    out = await asyncio.wait_for(run_task, timeout=15)
    assert out.solved is False
    assert sw._operator_paused is False
    assert spawns["n"] > paused_spawns, "after resume the coordinator spawns again"


async def test_coordinator_barren_backpressure_pauses_single_flag(
        challenge, tmp_path, monkeypatch):
    """run-11190 regression: a SINGLE-flag run whose workers keep finishing with no
    new fact/flag must hit the no-progress backpressure and PAUSE for the operator —
    the old guardrail was collect-mode-only (multi_flag + unknown count), so a
    single-flag chained run had NO spend cap and could spike workers without bound.
    Here every worker is fruitless and the board never grows; after barren_limit
    completions the coordinator must emit the pause and stop spawning."""
    from muteki.solver.types import SolveOutcome
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event, EventType

    bus = EventBus()
    inbox: asyncio.Queue = asyncio.Queue()
    # single-flag (default multi_flag=False); small limit so the test is quick.
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1, bus=bus,
                            hitl_inbox=inbox, barren_limit=3,
                            wall_clock_budget=10.0)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    # Reason keeps the swarm "busy" (open intent always present) so the pause can
    # only come from the top-of-loop backpressure, never the fully-idle branch.
    async def _reason_one():
        return 1
    monkeypatch.setattr(sw, "_run_reason", lambda: _reason_one())
    monkeypatch.setattr(sw, "_open_intents",
                        lambda: [{"intent_id": "I1", "goal": "keep going"}])
    if sw.shared_graph is not None:
        monkeypatch.setattr(sw.shared_graph, "claim_intent", lambda **kw: True)
    # board never grows — every worker is fruitless.
    monkeypatch.setattr(sw, "_total_fact_count", lambda: 0)
    monkeypatch.setattr(sw, "_verified_fact_count", lambda: 0)

    spawns = {"n": 0}
    paused_seen = {"v": False}

    async def _bb_spy(ev: Event) -> None:
        if (ev.event_type == EventType.BLACKBOARD_DELTA
                and (ev.payload or {}).get("kind") == "collect_idle"):
            paused_seen["v"] = True
    bus.add_sink(_bb_spy)

    class Worker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "fruitless")

    def fake_make(engine, *, mode, intent_goal="", intent_id=""):
        spawns["n"] += 1
        return Worker(engine)
    monkeypatch.setattr(sw, "_make_cli_worker", fake_make)

    run_task = asyncio.create_task(sw.run())
    paused = False
    for _ in range(400):
        await asyncio.sleep(0.02)
        if paused_seen["v"] and sw._operator_event is not None \
                and not sw._operator_event.is_set():
            paused = True
            break
    assert paused, "single-flag run must pause on barren backpressure"
    paused_spawns = spawns["n"]
    await asyncio.sleep(0.25)
    assert spawns["n"] == paused_spawns, \
        "while paused on backpressure the coordinator must NOT spawn more workers"
    # operator stops → clean end
    await inbox.put({"action": "stop"})
    out = await asyncio.wait_for(run_task, timeout=15)
    assert out.solved is False


async def test_coordinator_barren_resets_on_new_fact(challenge, tmp_path, monkeypatch):
    """A worker that DOES grow the board (even a candidate fact) must reset the
    fruitless counter — the backpressure is for genuinely-stuck spend, and a
    deep-exploit worker mid-chain that just produced a candidate is making
    progress and must not be paused. With the board growing each round, the run
    must NEVER hit the barren pause before its finite budget ends it."""
    from muteki.solver.types import SolveOutcome
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event, EventType

    bus = EventBus()
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=1, bus=bus,
                            barren_limit=3, wall_clock_budget=0.5)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude"])
    monkeypatch.setattr(sw, "_run_reason", lambda: _async_zero())

    grew = {"n": 0}
    # board grows by one fact on every call → counter resets every round.
    monkeypatch.setattr(sw, "_total_fact_count",
                        lambda: grew.__setitem__("n", grew["n"] + 1) or grew["n"])
    monkeypatch.setattr(sw, "_verified_fact_count", lambda: 0)

    paused_seen = {"v": False}

    async def _bb_spy(ev: Event) -> None:
        if (ev.event_type == EventType.BLACKBOARD_DELTA
                and (ev.payload or {}).get("kind") == "collect_idle"):
            paused_seen["v"] = True
    bus.add_sink(_bb_spy)

    class Worker:
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
        async def run(self):
            await asyncio.sleep(0)
            return SolveOutcome(False, None, 1, None, "made a candidate")

    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: Worker(engine))

    out = await asyncio.wait_for(sw.run(), timeout=10)
    assert out.solved is False
    assert not paused_seen["v"], \
        "a run that keeps growing the board must never hit the barren pause"


# ── code-review fixes (origin/main..HEAD review) ─────────────────────────────

def test_missing_profile_does_not_leak_budget(challenge, tmp_path):
    """#3: when a worker_profile is missing for the requested engine/role,
    _make_cli_worker must raise WorkerSpawnRejected WITHOUT charging _spawned_total
    (the old code did _reserve_worker_spawn() BEFORE resolving the profile, then
    bailed — leaking a phantom spawn toward max_total_workers and crashing the
    coordinator with a bare RuntimeError)."""
    from muteki.swarm.swarm import WorkerSpawnRejected
    sw = _coordinator_swarm(
        challenge, tmp_path,
        worker_profiles=[{"id": "claude-sub", "engine": "claude",
                          "roles": ["bootstrap"], "runtime": "local"}],
    )
    before = sw._spawned_total
    # request an engine that has NO profile → rejected, not budget-charged
    with pytest.raises(WorkerSpawnRejected):
        sw._make_cli_worker("codex", mode="bootstrap")
    assert sw._spawned_total == before, "a rejected spawn must not charge the budget"
    # and it must NOT be a plain RuntimeError that spawn sites don't catch
    assert issubclass(WorkerSpawnRejected, RuntimeError)


def test_codex_subscription_uses_profile_capacity_not_account_mutex(challenge, tmp_path):
    """Codex subscription profiles are not account-mutexed. They obey the profile's
    ordinary worker capacity just like claude/cursor profiles."""
    sw = _coordinator_swarm(
        challenge, tmp_path,
        worker_profiles=[{
            "id": "codex-sub",
            "engine": "codex",
            "credential_mode": "subscription",
            "credential_account": "codex-main",
            "roles": ["bootstrap", "explore"],
            "runtime": "local",
            "max_running": 2,
            "enabled": True,
        }],
        engines=["codex-sub"],
    )
    first = sw._profile_for_engine("codex", role="bootstrap")
    assert first is not None
    sw._claim_worker_account("cli-codex-1", "codex", first, role="bootstrap")

    second = sw._profile_for_engine("codex", role="explore", advance=False)
    assert second is not None
    sw._claim_worker_account("cli-codex-2", "codex", second, role="explore")
    assert sw._profile_for_engine("codex", role="bootstrap", advance=False) is None

    class _S:
        def __init__(self, sid): self.solver_id = sid

    sw._release_worker_account(_S("cli-codex-1"))
    assert sw._profile_for_engine("codex", role="bootstrap", advance=False) is not None


def test_container_runtime_mismatch_records_degraded(challenge, tmp_path, monkeypatch):
    """#11: one-container-per-run. A second engine whose profile asks for a DIFFERENT
    runtime than the container was built with must be flagged runtime_degraded, not
    silently inherit the first profile's isolation settings."""
    import muteki.solver.container_exec as ce

    class _FakeHandle:
        container = "muteki-run-x"
    # _container_for_engine does `from muteki.solver.container_exec import
    # ensure_container` at call time, so patch it on the SOURCE module.
    monkeypatch.setattr(ce, "ensure_container",
                        lambda *a, **k: _FakeHandle(), raising=False)
    wroot = tmp_path / "wroot"
    wroot.mkdir(parents=True, exist_ok=True)
    sw = _coordinator_swarm(
        challenge, tmp_path, worker_backend="container", worker_root=wroot,
        worker_profiles=[
            {"id": "web", "engine": "claude", "roles": ["bootstrap"], "runtime": "docker-web"},
            {"id": "off", "engine": "codex", "roles": ["bootstrap"], "runtime": "docker-offline"},
        ],
        runtime_profiles=[
            {"id": "docker-web", "backend": "container", "network": "bridge"},
            {"id": "docker-offline", "backend": "container", "network": "none"},
        ],
    )
    web_profile = sw._profile_for_engine("claude", role="bootstrap")
    sw._container_for_engine("claude", web_profile)
    assert sw._container_runtime_id == "docker-web"
    degraded_before = list(sw._runtime_degraded)
    off_profile = sw._profile_for_engine("codex", role="bootstrap")
    sw._container_for_engine("codex", off_profile)
    assert len(sw._runtime_degraded) > len(degraded_before), \
        "#11: a different requested runtime on the cached container must record degraded"


def test_container_runtime_links_blackboard_skill_into_isolated_home(challenge, tmp_path, monkeypatch):
    import muteki.solver.container_exec as container_exec

    wroot = tmp_path / "workspace" / "workers"
    wroot.mkdir(parents=True, exist_ok=True)
    sw = _coordinator_swarm(
        challenge, tmp_path, worker_backend="container", worker_root=wroot,
    )
    chowned = []
    monkeypatch.setattr(
        container_exec, "_chown_tree_to_worker",
        lambda path: chowned.append(Path(path)),
    )

    class _FakeContainer:
        def to_container_path(self, path: str) -> str:
            return path.replace(str(tmp_path / "workspace"), "/home/kali/workspace")

    env = sw._runtime_env_for("codex", "cli-codex", container=_FakeContainer())

    assert env["HOME"] == "/home/kali/workspace/homes/cli-codex"
    home = tmp_path / "workspace" / "homes" / "cli-codex"
    assert chowned == [home]
    for rel in (
        ".claude/skills/muteki-blackboard",
        ".agents/skills/muteki-blackboard",
        ".codex/skills/muteki-blackboard",
        ".cursor/skills-cursor/muteki-blackboard",
        ".cursor/skills/muteki-blackboard",
    ):
        link = home / rel
        assert link.is_symlink()
        assert str(link.readlink()) == "/opt/muteki/muteki-blackboard"


# ── review-policy sanitization ───────────────────────────────────────────────

def test_clean_review_policy_preserves_max_challenges_per_cycle():
    """run-75377 knob: an operator-set max_challenges_per_cycle must survive
    _clean_review_policy. It used to be dropped (not in the int whitelist), so the
    read site always fell back to the hard-coded 8 — the knob was inert."""
    cleaned = Swarm._clean_review_policy({"enabled": True, "max_challenges_per_cycle": 3})
    assert cleaned["max_challenges_per_cycle"] == 3
    # absent → documented default of 8 (consistent with siblings like max_review_workers)
    assert Swarm._clean_review_policy({})["max_challenges_per_cycle"] == 8
    # a configured 0 is preserved as a genuine "disable challenge fan-out this cycle",
    # NOT silently rewritten to 8 (the read site honors the same floor).
    assert Swarm._clean_review_policy({"max_challenges_per_cycle": 0})["max_challenges_per_cycle"] == 0
    # garbage / negative is coerced: non-int falls back to default, negative clamps to 0.
    assert Swarm._clean_review_policy({"max_challenges_per_cycle": "nope"})["max_challenges_per_cycle"] == 8
    assert Swarm._clean_review_policy({"max_challenges_per_cycle": -5})["max_challenges_per_cycle"] == 0


# ── End-to-end lifecycle integration (TODO_IMPLEMENTATION_DISCUSSION Phases 1-7) ──

def test_lifecycle_integration_directive_review_resource_compact(challenge, tmp_path: Path):
    """Drive the full new lifecycle through the real Swarm + SharedGraph on a local
    mock challenge: operator directive (B) → claimable directive intent; fact review
    reject/merge (A) → snapshot filtering; resource lock (E) → dispatch preflight;
    finalize-by-stop-reason (J); compaction (H). No API key / scripted worker."""
    sw = _coordinator_swarm(challenge, tmp_path, max_workers=2)
    g = sw.shared_graph

    # --- A: facts + review lifecycle ---
    f_good = g.add_evidence(actor="cli-a", source="x", fact="SMB open on .45", verified=True)
    f_bad = g.add_evidence(actor="cli-b", source="x", fact="bogus RCE on .45", verified=True)
    f_dup = g.add_evidence(actor="cli-c", source="x", fact="port 445 listening", verified=True)
    g.reject_fact(actor="review", fact_seq=f_bad, reason="not reproducible")
    g.merge_fact(actor="review", from_fact_seq=f_dup, to_fact_seq=f_good, reason="same finding")
    facts = [e.fact for e in g.snapshot().evidence]
    assert facts == ["SMB open on .45"]  # rejected + merged dropped

    # --- B: operator directive → first-class steering + claimable directive intent ---
    info = g.add_operator_directive(action="redirect", text="pivot to internal 10.0.0.5",
                                    preempt_policy="soft_rebind")
    g.propose_intent(actor="operator", intent_id=f"I-{info['directive_id']}",
                     goal="pivot to internal 10.0.0.5",
                     payload={"directive_id": info["directive_id"], "priority": "operator"})
    summary = g.to_reason_summary()
    assert "Operator directives" in summary and "pivot to internal 10.0.0.5" in summary

    # --- E: resource lock + dispatch preflight (an intent on a locked resource is held) ---
    g.request_resource_lock(actor="cli-x", resource_key="destructive:tcp:445@10.0.0.5",
                            owner_worker="cli-x")
    g.propose_intent(actor="reason", intent_id="I-collide",
                     goal="exploit 10.0.0.5 smb",
                     payload={"resource_key": "destructive:tcp:445@10.0.0.5"})
    g.propose_intent(actor="reason", intent_id="I-free", goal="enumerate users")
    dispatchable = {i["intent_id"] for i in sw._open_intents()}
    assert "I-collide" not in dispatchable  # locked resource → preflight skip
    assert "I-free" in dispatchable
    assert f"I-{info['directive_id']}" in dispatchable  # operator intent dispatchable

    # --- H: compaction retires a barren closed intent, keeps facts ---
    g.propose_intent(actor="reason", intent_id="I-barren", goal="dead direction")
    g.claim_intent(worker="cli-z", intent_id="I-barren")
    g.conclude_intent(actor="cli-z", intent_id="I-barren", result="no flag")
    cinfo = g.compact_graph(trigger="no_progress_time")
    assert "I-barren" in cinfo["retired_intent_ids"]
    assert any(e.fact == "SMB open on .45" for e in g.snapshot().evidence)

    # --- J: a non-solved crash terminal (runtime_failure) holds active intents back
    # as resume; revive flips them active again. (operator_stop now CLOSES instead —
    # see test_shared_graph.test_finalize_operator_stop_closes_active_intents.)
    out = g.release_claims_for_finalize(reason="runtime_failure")
    assert "I-free" in out["resumed_intents"]
    assert g.open_goal_texts() == []  # all resume/retired, nothing active

    # revive restores them
    revived = g.revive_resume_intents()
    assert "I-free" in revived


# ── split-brain flag-completion source of truth (run-75379 BUG②) ─────────────
# A flag reaches the AUTHORITATIVE shared graph the moment a worker accepts it
# (_accept_flag → shared_graph.flag_found), but _found_flags (the in-memory list
# _flags_complete() reads) is fed ONLY from a reaped `outcome.flags`. A worker
# cancelled/errored right after it accepted a flag — or the live DB→bus bridge —
# can put a flag in the graph/UI without it ever reaching _found_flags. In
# run-75379 the graph held 4 valid flags while _found_flags was stuck at 2, so
# the run never finalized and spawned ~55 post-solve waves. _sync_flags_from_graph
# reconciles against the snapshot so completion reads the real flag count.

def test_sync_flags_from_graph_absorbs_graph_only_flags(challenge, tmp_path: Path):
    """Flags recorded ONLY via the shared-graph path (never via outcome.flags) must
    make _flags_complete() true after reconcile — no split-brain."""
    challenge.expected_flags = 2
    challenge.multi_flag = True
    sw = _coordinator_swarm(challenge, tmp_path)
    assert sw.shared_graph is not None
    # two flags land on the graph (as _accept_flag would) but _found_flags is empty:
    # the worker outcomes never delivered them (cancelled-after-accept / DB bridge).
    sw.shared_graph.flag_found(actor="cli-x", flag="flag{a}", intent_id=None)
    sw.shared_graph.flag_found(actor="cli-y", flag="flag{b}", intent_id=None)
    assert sw._found_flags == []
    assert sw._flags_complete() is False  # in-memory set still empty → split-brain

    fresh = sw._sync_flags_from_graph()

    assert set(fresh) == {"flag{a}", "flag{b}"}          # both newly absorbed
    assert set(sw._found_flags) == {"flag{a}", "flag{b}"}
    assert sw._flags_complete() is True                  # completion now fires


def test_sync_flags_drops_operator_invalidated_flag(challenge, tmp_path: Path):
    """An operator-invalidated (blacklisted) flag must NOT count toward
    expected_flags after reconcile (BUG③ cross-check): syncing FROM the snapshot,
    which already excludes invalidated flags, removes it from the in-memory set."""
    challenge.expected_flags = 2
    challenge.multi_flag = True
    sw = _coordinator_swarm(challenge, tmp_path)
    assert sw.shared_graph is not None
    sw.shared_graph.flag_found(actor="cli-x", flag="flag{real}", intent_id=None)
    sw.shared_graph.flag_found(actor="cli-y", flag="flag{bogus}", intent_id=None)
    sw._sync_flags_from_graph()
    assert sw._flags_complete() is True                  # 2/2 before invalidation

    # operator marks the bogus one false → it must leave the count.
    sw.shared_graph.reopen_after_false_positive(actor="operator", flag="flag{bogus}")
    sw._sync_flags_from_graph()

    assert sw._found_flags == ["flag{real}"]             # bogus dropped
    assert sw._flags_complete() is False                 # 1/2 again, not falsely complete


def test_sync_flags_noop_without_graph(challenge, tmp_path: Path):
    """No shared graph → reconcile is a no-op and never wipes the in-memory set
    (a transient/absent graph must not erase genuinely-held flags)."""
    challenge.expected_flags = 1
    sw = _coordinator_swarm(challenge, tmp_path)
    sw.shared_graph = None
    sw._found_flags = ["flag{kept}"]
    assert sw._sync_flags_from_graph() == []
    assert sw._found_flags == ["flag{kept}"]             # untouched


async def test_coordinator_finalizes_when_flag_only_on_graph(challenge, tmp_path: Path,
                                                             monkeypatch):
    """End-to-end (run-75379 BUG②): expected_flags=2 where each worker writes its
    flag to the shared graph (as _accept_flag does) but returns a SolveOutcome with
    EMPTY flags — the leak. The per-reap tally stays at 0; only the graph reconcile
    drives completion. The run must finalize solved instead of spawning forever."""
    from muteki.solver.types import SolveOutcome

    challenge.expected_flags = 2
    challenge.multi_flag = True
    sw = _coordinator_swarm(challenge, tmp_path, start_workers=2)
    monkeypatch.setattr(sw, "_healthy_engines", lambda: ["claude", "codex"])

    flags = {"claude": "flag{a}", "codex": "flag{b}"}

    class GraphOnlyWorker:
        """Accepts a flag onto the shared graph but never returns it in the
        outcome — models a worker cancelled/errored after _accept_flag, or the
        live DB→bus bridge path."""
        def __init__(self, engine):
            self.solver_id = f"cli-{engine}"
            self._f = flags[engine]
        async def run(self):
            await asyncio.sleep(0)
            sw.shared_graph.flag_found(actor=self.solver_id, flag=self._f,
                                       intent_id=None)
            # outcome carries NO flags → _record_flags(*outcome.flags) sees nothing.
            return SolveOutcome(False, None, 1, None, "done", flags=[])

    monkeypatch.setattr(sw, "_make_cli_worker",
                        lambda engine, **kw: GraphOnlyWorker(engine))

    out = await asyncio.wait_for(sw.run(), timeout=10)

    assert out.solved is True                            # finalized, did not hang
    assert set(sw._found_flags) == {"flag{a}", "flag{b}"}  # reconciled from graph
    assert sw._flags_complete() is True
