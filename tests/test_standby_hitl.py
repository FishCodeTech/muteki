"""Post-solve standby HITL: cold-start a worker to serve a follow-up after a run
finished (or the server restarted). Covers winner.json persistence, the
false-positive state machine, and the post_hitl → standby routing."""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
from pathlib import Path

import pytest

from muteki.models.solve_graph import Challenge
from muteki.solver.types import SolveOutcome
from muteki.swarm.shared_graph import SQLiteSharedGraph


def _challenge() -> Challenge:
    return Challenge(id="run-x", name="t", category="web", points=0, description="")


# ── A: winner.json persistence (via Swarm._persist_winner) ──────────────────
def test_persist_winner_writes_session_handle(tmp_path):
    from muteki.swarm.swarm import Swarm
    from muteki.sandbox.manager import SandboxManager

    graph_dir = tmp_path / "graph"
    sw = Swarm(
        _challenge(), [], llm=None,
        sandbox=SandboxManager(root=tmp_path / "sbx"),
        graph_dir=graph_dir, run_id="run-x",
    )
    out = SolveOutcome(True, "csawctf{x}", 1, None, "solved",
                       session="sess-abc", engine="claude", workdir="/tmp/w")
    sw._persist_winner(out, "csawctf{x}")
    winner = json.loads((graph_dir.parent / "winner.json").read_text())
    assert winner["session"] == "sess-abc"
    assert winner["engine"] == "claude"
    assert winner["backend"] == "local"
    assert "runtime_degraded" in winner
    assert winner["flag"] == "csawctf{x}"
    assert winner["challenge"]["id"] == "run-x"
    # multi-flag: winner.json also carries the full flags list (here just the one)
    assert winner["flags"] == ["csawctf{x}"]


def test_persist_winner_carries_all_flags(tmp_path):
    from muteki.swarm.swarm import Swarm
    from muteki.sandbox.manager import SandboxManager
    import json

    graph_dir = tmp_path / "graph"
    sw = Swarm(
        _challenge(), [], llm=None,
        sandbox=SandboxManager(root=tmp_path / "sbx"),
        graph_dir=graph_dir, run_id="run-multi",
    )
    sw._found_flags = ["flag{a}", "flag{b}", "flag{c}"]  # run collected three
    out = SolveOutcome(True, "flag{a}", 1, None, "solved",
                       session="sess-1", engine="claude", workdir="/tmp/w",
                       flags=["flag{c}"])  # this worker only found the last one
    sw._persist_winner(out, "flag{a}")
    winner = json.loads((graph_dir.parent / "winner.json").read_text())
    # winner.json carries the RUN's full set (authoritative), not one worker's
    assert winner["flags"] == ["flag{a}", "flag{b}", "flag{c}"]
    assert winner["flag"] == "flag{a}"  # first, back-compat


def test_persist_winner_skips_without_session(tmp_path):
    from muteki.swarm.swarm import Swarm
    from muteki.sandbox.manager import SandboxManager

    graph_dir = tmp_path / "graph"
    sw = Swarm(_challenge(), [], llm=None,
               sandbox=SandboxManager(root=tmp_path / "sbx"),
               graph_dir=graph_dir, run_id="run-x")
    # no session → nothing to resume → no file
    sw._persist_winner(SolveOutcome(True, "f{x}", 1, None, "", session=None),
                       "f{x}")
    assert not (graph_dir.parent / "winner.json").exists()


# ── D2: false-positive state machine ────────────────────────────────────────
def test_reopen_after_false_positive(tmp_path):
    g = SQLiteSharedGraph(str(tmp_path / "g.db"), _challenge())
    fs = g.add_evidence(actor="cli-claude", source="claude", fact="real", verified=True)
    g.propose_intent(actor="reason", intent_id="intent:cli-claude", goal="solve")
    g.conclude_intent(actor="cli-claude", intent_id="intent:cli-claude",
                      result="solved", to_fact_seq=fs)
    info = g.reopen_after_false_positive(actor="operator", flag="csawctf{fake}")
    assert info["reopened"] == ["intent:cli-claude"]
    assert "false positive" in info["dead_end_reason"]
    # intent flipped back to open, fact link cleared
    row = g._conn.execute(
        "SELECT status, to_fact_seq FROM intents WHERE intent_id=?",
        ("intent:cli-claude",)).fetchone()
    assert row == ("open", None)
    # the false flag is now a dead-end (so nobody retries it)
    assert any(e["kind"] == "dead_end" and "csawctf{fake}" in e["payload"].get("reason", "")
               for e in g.events())
    g.close()


# ── B: post_hitl routing — finished run cold-starts a standby ───────────────
def test_post_hitl_finished_run_triggers_standby(tmp_path, monkeypatch):
    from apps.web import run_manager as rm

    spawned = {}

    def _fake_build_standby(cmd, mgr=None):
        spawned["cmd"] = cmd

        async def _drive(run):
            spawned["ran"] = True
        return _drive

    monkeypatch.setattr("apps.web.drivers.build_standby_driver", _fake_build_standby)

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        mgr.worker_config.resolve = lambda category: {"worker_backend": "local"}
        run = mgr.create("run-x")
        run.finished = True
        run.solved = True
        run.task = None  # no live task → finished
        # close the bus to mimic a finished run; _fresh_bus must revive it
        await run.bus.close()
        ok = await mgr.post_hitl("run-x", "global", "ask", text="how?")
        assert ok
        # standby driver was built + scheduled
        assert run.standby_task is not None
        await asyncio.gather(run.standby_task, return_exceptions=True)
        return spawned

    out = asyncio.run(_run())
    assert out.get("ran") is True
    assert out["cmd"]["action"] == "ask"


def test_post_hitl_repeated_writeup_triggers_new_standby(tmp_path, monkeypatch):
    from apps.web import run_manager as rm

    spawned = []

    def _fake_build_standby(cmd, mgr=None):
        spawned.append(dict(cmd))

        async def _drive(run):
            await asyncio.sleep(0.01)
        return _drive

    monkeypatch.setattr("apps.web.drivers.build_standby_driver", _fake_build_standby)

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        mgr.worker_config.resolve = lambda category: {"worker_backend": "local"}
        run = mgr.create("run-x")
        run.finished = True
        run.solved = True
        run.task = None
        await run.bus.close()

        assert await mgr.post_hitl("run-x", "global", "writeup", text="")
        first = run.standby_task
        assert first is not None
        await first

        assert await mgr.post_hitl("run-x", "global", "writeup", text="")
        second = run.standby_task
        assert second is not None
        await second

    asyncio.run(_run())
    assert [cmd["action"] for cmd in spawned] == ["writeup", "writeup"]


def test_post_hitl_live_run_does_not_standby(tmp_path, monkeypatch):
    from apps.web import run_manager as rm

    called = {"n": 0}
    monkeypatch.setattr(rm.RunManager, "_ensure_standby",
                        lambda self, rid, cmd: called.__setitem__("n", called["n"] + 1))

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        # a live task → pause/hint go through the queue, NOT a standby
        run.task = asyncio.create_task(asyncio.sleep(5))
        await mgr.post_hitl("run-x", "global", "pause")
        run.task.cancel()
        return called

    out = asyncio.run(_run())
    assert out["n"] == 0


def test_pause_resume_never_standby(tmp_path, monkeypatch):
    """pause/resume only act on a live subprocess — never cold-start a worker."""
    from apps.web import run_manager as rm

    called = {"n": 0}
    monkeypatch.setattr(rm.RunManager, "_ensure_standby",
                        lambda self, rid, cmd: called.__setitem__("n", called["n"] + 1))

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.finished = True
        run.task = None
        await mgr.post_hitl("run-x", "global", "pause")
        await mgr.post_hitl("run-x", "global", "resume")
        return called

    assert asyncio.run(_run())["n"] == 0


# ── _fresh_bus revives a closed bus (so standby events reach a new SSE) ──────
def test_fresh_bus_revives_closed_bus(tmp_path):
    from apps.web import run_manager as rm

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        old = run.bus
        await run.bus.close()
        assert run.bus._closed is True
        mgr._fresh_bus(run)
        assert run.bus is not old
        assert run.bus._closed is False
        # the new bus still persists to the SessionStore + carries seq forward
        from muteki.core.events import Event, EventType
        await run.bus.emit(Event(event_type=EventType.REASONING_DELTA,
                                 run_id="run-x", payload={"text": "hi"}))

    asyncio.run(_run())


def test_rehydrated_run_bus_continues_after_persisted_stream_seq(tmp_path):
    from apps.web import run_manager as rm
    from muteki.core.events import Event, EventType

    async def _run():
        sessions = tmp_path / "sessions"
        sessions.mkdir()
        run_id = "run-x"
        # Simulate an old corrupted log: raw seq reset after reopen. The next
        # in-memory bus event must continue after the normalized stream seq (4),
        # not raw max(2) or a fresh 1.
        with (sessions / f"{run_id}.jsonl").open("w", encoding="utf-8") as f:
            for ev in [
                {"event_type": "run.started", "seq": 1, "ts": 1.0, "run_id": run_id, "payload": {}},
                {"event_type": "run.finished", "seq": 2, "ts": 2.0, "run_id": run_id, "payload": {}},
                {"event_type": "run.reopened", "seq": 1, "ts": 3.0, "run_id": run_id, "payload": {"reason": "resolve"}},
                {"event_type": "reasoning.delta", "seq": 2, "ts": 4.0, "run_id": run_id, "payload": {"text": "after"}},
            ]:
                import json
                f.write(json.dumps(ev) + "\n")
        mgr = rm.RunManager(sessions_root=str(sessions))
        run = mgr.get(run_id)
        assert run is not None
        emitted = await run.bus.emit(Event(
            event_type=EventType.REASONING_DELTA, run_id=run_id,
            payload={"text": "new"}))
        assert emitted.seq == 5

    asyncio.run(_run())

# ── stop action: soft-stop a live run (cancel task, keep history) ────────────
def test_post_hitl_stop_cancels_live_task(tmp_path, monkeypatch):
    from apps.web import run_manager as rm
    called = {"n": 0}
    monkeypatch.setattr(rm.RunManager, "_ensure_standby",
                        lambda self, rid, cmd: called.__setitem__("n", called["n"] + 1))

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.task = asyncio.create_task(asyncio.sleep(30))
        ok = await mgr.post_hitl("run-x", "global", "stop")
        # give the event loop a tick to process the cancellation
        await asyncio.sleep(0)
        return ok, run, called

    ok, run, called = asyncio.run(_run())
    assert ok is True
    assert run.task.cancelled() or run.task.done()   # the live task was cancelled
    assert called["n"] == 0                            # stop never spawns a standby


def test_post_hitl_stop_on_finished_run_is_noop(tmp_path, monkeypatch):
    from apps.web import run_manager as rm
    called = {"n": 0}
    monkeypatch.setattr(rm.RunManager, "_ensure_standby",
                        lambda self, rid, cmd: called.__setitem__("n", called["n"] + 1))

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.finished = True
        run.task = None                               # nothing live to stop
        ok = await mgr.post_hitl("run-x", "global", "stop")
        return ok, called

    ok, called = asyncio.run(_run())
    assert ok is True
    assert called["n"] == 0                            # no standby for a stop


def test_post_hitl_stop_echoes_hitl_response(tmp_path):
    from apps.web import run_manager as rm
    from muteki.core.events import EventType

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        seen = []
        orig_emit = run.bus.emit
        async def _spy(ev):
            seen.append(ev); await orig_emit(ev)
        run.bus.emit = _spy
        run.task = asyncio.create_task(asyncio.sleep(30))
        await mgr.post_hitl("run-x", "global", "stop")
        await asyncio.sleep(0)
        return seen

    seen = asyncio.run(_run())
    hitl = [e for e in seen if e.event_type is EventType.HITL_RESPONSE]
    assert hitl and hitl[-1].payload.get("action") == "stop"


def test_m2_post_hitl_drops_identical_back_to_back_hint(tmp_path):
    """M2: an identical hint resent back-to-back is dropped — not re-queued — so an
    operator hammering the same hint can't pile up 11 queue items + 11 events."""
    from apps.web import run_manager as rm

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.task = asyncio.create_task(asyncio.sleep(30))
        # first send queues; the 10 identical resends are dropped
        for _ in range(11):
            await mgr.post_hitl("run-x", "global", "hint", text="try /admin")
        depth = run.hitl.qsize()
        # a genuinely new hint goes through
        await mgr.post_hitl("run-x", "global", "hint", text="now try /api")
        depth2 = run.hitl.qsize()
        run.task.cancel()
        return depth, depth2

    depth, depth2 = asyncio.run(_run())
    assert depth == 1, f"11 identical hints must queue once, got {depth}"
    assert depth2 == 2, "a changed hint must still queue"


def test_m4_post_hitl_reports_delivery_status(tmp_path):
    """M4: HITL_RESPONSE carries a `delivery` status so the operator knows where a
    command went — queued_for_next_worker (live hint) vs no_live_workers / standby."""
    from apps.web import run_manager as rm
    from muteki.core.events import EventType

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        seen = []
        orig_emit = run.bus.emit
        async def _spy(ev):
            seen.append(ev); await orig_emit(ev)
        run.bus.emit = _spy
        # live run → a hint is queued for the next worker
        run.task = asyncio.create_task(asyncio.sleep(30))
        await mgr.post_hitl("run-x", "global", "hint", text="try /admin")
        run.task.cancel()
        await asyncio.sleep(0)
        return seen

    seen = asyncio.run(_run())
    hitl = [e for e in seen if e.event_type is EventType.HITL_RESPONSE]
    assert hitl, "a hint must echo a HITL_RESPONSE"
    assert hitl[-1].payload.get("delivery") == "queued_for_next_worker"


# ── resolve action: "继续做题" relaunches the FULL swarm (not a single standby) ──
def test_resolve_relaunches_full_swarm_not_standby(tmp_path, monkeypatch):
    """On a finished run, /resolve must rebuild the real swarm driver (multi-worker
    coordinator), NOT cold-start a single standby worker."""
    from apps.web import run_manager as rm
    import apps.web.drivers as drivers

    built = {"driver": 0, "standby": 0}

    # stub build_driver to a no-op driver and count calls (proves the FULL swarm
    # path was taken, not _ensure_standby's single-worker build_standby_driver).
    async def _noop_driver(run):
        return None
    monkeypatch.setattr(drivers, "build_driver", lambda body, mgr=None: (built.__setitem__("driver", built["driver"] + 1) or _noop_driver))
    monkeypatch.setattr(rm.RunManager, "_ensure_standby",
                        lambda self, rid, cmd: built.__setitem__("standby", built["standby"] + 1))

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.started = True
        run.finished = True
        run.solved = True
        run.flag = "flag{a}"
        run.flags = ["flag{a}", "flag{b}"]
        run.task = None  # finished, no live task
        ok = await mgr.resolve("run-x", {})
        assert run.solved is False        # run reopened
        assert run.flag == "flag{a}"      # prior results remain visible while re-solving
        assert run.flags == ["flag{a}", "flag{b}"]
        # let the relaunched task run + settle
        if run.task:
            await asyncio.gather(run.task, return_exceptions=True)
        return ok, run, built

    ok, run, built = asyncio.run(_run())
    assert ok is True
    assert built["driver"] == 1       # the FULL swarm driver was built + launched
    assert built["standby"] == 0      # NOT the single-worker standby path
    assert run.solved is False        # run reopened
    assert run.flags == ["flag{a}", "flag{b}"]
    assert run.flag == "flag{a}"


def test_mark_false_standby_uses_operator_selected_flag(tmp_path, monkeypatch):
    """Multi-flag false-positive feedback must target the operator-selected flag,
    not blindly invalidate winner.flag/run.flag (usually the first flag)."""
    from apps.web import run_manager as rm
    import muteki.solver.cli_solver as cli_solver
    import apps.web.drivers as drivers

    captured = {}

    class FakeCliSolver:
        def __init__(self, *args, **kwargs):
            captured["hitl_cmd"] = kwargs.get("hitl_cmd")
            captured["found_flags"] = kwargs.get("found_flags")
            captured["challenge"] = kwargs.get("challenge") or (args[1] if len(args) > 1 else None)

        async def run(self):
            return SolveOutcome(False, None, 1, None, "still searching",
                                flags=captured["found_flags"])

    monkeypatch.setattr(cli_solver, "CliSolver", FakeCliSolver)

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        mgr.worker_config.resolve = lambda category: {"worker_backend": "local"}
        run = mgr.create("run-x")
        run.started = True
        run.finished = True
        run.solved = True
        run.flag = "flag{a}"
        run.flags = ["flag{a}", "flag{b}", "flag{c}"]
        wp = mgr.workspace_dir("run-x") / "winner.json"
        wp.write_text(json.dumps({
            "engine": "claude",
            "session": "sess-1",
            "workdir": str(tmp_path),
            "flag": "flag{a}",
            "flags": ["flag{a}", "flag{b}", "flag{c}"],
            "challenge": {
                "id": "run-x",
                "name": "multi",
                "category": "web",
                "expected_flags": 3,
                "multi_flag": True,
            },
        }))
        driver = drivers.build_standby_driver(
            {"target": "global", "action": "mark_false", "flag": "flag{b}"},
            mgr=mgr,
        )
        await driver(run)
        return run

    run = asyncio.run(_run())
    assert captured["hitl_cmd"]["flag"] == "flag{b}"
    assert captured["found_flags"] == ["flag{a}", "flag{c}"]
    assert run.flags == ["flag{a}", "flag{c}"]
    assert run.flag == "flag{a}"


def test_finished_hitl_in_web_container_uses_worker_container(tmp_path, monkeypatch):
    """A finished-run follow-up in compose must cold-start a worker container.

    This covers writeup/ask/mark_false's shared standby path: the web container
    must not shell a host-native CLI with the wrong toolchain/credentials.
    """
    from apps.web import run_manager as rm
    import apps.web.drivers as drivers
    import muteki.solver.cli_solver as cli_solver
    import muteki.solver.container_exec as container_exec

    monkeypatch.setattr(drivers, "is_web_container", lambda: True)
    captured = {}

    class FakeHandle:
        container = "muteki-run-run-x"

        def to_container_path(self, host_path):
            return "/home/kali/workspace/" + Path(host_path).name

        def to_container_cwd(self, host_cwd):
            return self.to_container_path(host_cwd)

    def fake_ensure_container(run_id, host_workspace, **kwargs):
        captured["ensure"] = {
            "run_id": run_id,
            "host_workspace": host_workspace,
            **kwargs,
        }
        return FakeHandle()

    def fake_teardown(run_id, *, remove=True):
        captured["teardown"] = {"run_id": run_id, "remove": remove}

    def fake_chown(path):
        captured.setdefault("chown", []).append(Path(path).name)

    class FakeCliSolver:
        def __init__(self, *args, **kwargs):
            captured["solver_kwargs"] = kwargs

        async def run(self):
            return SolveOutcome(False, None, 1, None, "writeup",
                                engine="codex", reply="# Writeup")

    monkeypatch.setattr(container_exec, "ensure_container", fake_ensure_container)
    monkeypatch.setattr(container_exec, "teardown_container", fake_teardown)
    monkeypatch.setattr(container_exec, "_chown_tree_to_worker", fake_chown)
    monkeypatch.setattr(cli_solver, "CliSolver", FakeCliSolver)

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        mgr.worker_config.resolve = lambda category: {
            "worker_backend": "container",
            "runtime_profiles": [
                {"id": "docker-web", "backend": "container", "network": "bridge"},
            ],
            "worker_profiles": [
                {
                    "id": "seat-codex",
                    "name": "seat-codex",
                    "engine": "codex",
                    "runtime": "docker-web",
                    "credential_account": "codex-main",
                    "model": "gpt-5.4",
                    "enabled": True,
                    "roles": ["bootstrap", "explore", "worker"],
                },
            ],
        }
        run = mgr.create("run-x")
        run.started = True
        run.finished = True
        run.solved = True
        home = mgr.workspace_dir("run-x") / "homes" / "cli-codex"
        home.mkdir(parents=True, exist_ok=True)
        (home / "session.txt").write_text("thread-1")
        wp = mgr.workspace_dir("run-x") / "winner.json"
        wp.write_text(json.dumps({
            "engine": "codex",
            "session": "thread-1",
            "workdir": str(mgr.workspace_dir("run-x") / "workers" / "cli-codex-1"),
            "flag": "flag{ok}",
            "flags": ["flag{ok}"],
            "challenge": {
                "id": "run-x",
                "name": "t",
                "category": "web",
                "description": "",
            },
        }))
        await run.bus.close()
        ok = await mgr.post_hitl("run-x", "global", "writeup", text="")
        assert run.standby_task is not None
        await run.standby_task
        return ok

    ok = asyncio.run(_run())
    assert ok is True
    assert captured["ensure"]["run_id"] == "run-x"
    assert captured["ensure"]["network"] == "bridge"
    assert captured["solver_kwargs"]["container"].container == "muteki-run-run-x"
    assert captured["solver_kwargs"]["engine"] == "codex"
    assert captured["solver_kwargs"]["resume_session"] == "thread-1"
    assert captured["solver_kwargs"]["worker_env"]["MUTEKI_WORKER_MODEL"] == "gpt-5.4"
    assert captured["solver_kwargs"]["worker_env"]["HOME"].endswith("/cli-codex")
    assert captured["chown"] == ["standby-codex", "cli-codex"]
    assert captured["teardown"] == {"run_id": "run-x", "remove": True}


def test_standby_reuses_challenge_from_session_jsonl_without_winner(tmp_path, monkeypatch):
    """Old runs may have no winner.json; standby must still recover the original
    challenge payload instead of launching a context-free worker."""
    from apps.web import run_manager as rm
    import muteki.solver.cli_solver as cli_solver
    from muteki.core.events import Event, EventType
    from apps.web.drivers import build_standby_driver

    captured = {}

    class FakeCliSolver:
        def __init__(self, _llm, challenge, **kwargs):
            captured["challenge"] = challenge
            captured["kwargs"] = kwargs

        async def run(self):
            return SolveOutcome(False, None, 1, None, "reply",
                                engine="claude", reply="ok")

    monkeypatch.setattr(cli_solver, "CliSolver", FakeCliSolver)

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        mgr.worker_config.resolve = lambda category: {"worker_backend": "local"}
        run = mgr.create("run-x")
        await run.bus.emit(Event(
            event_type=EventType.RUN_STARTED,
            run_id="run-x",
            payload={"challenge": {
                "name": "threadweaver",
                "category": "misc",
                "description": "live challenge",
                "target": "94.237.52.90:12345",
                "expected_flags": 1,
            }},
        ))
        run.finished = True
        run.solved = True
        run.flag = "flag{bad}"
        driver = build_standby_driver({"target": "global", "action": "ask",
                                       "text": "continue"}, mgr=mgr)
        await driver(run)

    asyncio.run(_run())
    ch = captured["challenge"]
    assert ch.name == "threadweaver"
    assert ch.category == "misc"
    assert ch.description == "live challenge"
    assert ch.target == "94.237.52.90:12345"


def test_standby_failure_is_logged_and_emitted(tmp_path, monkeypatch, caplog):
    from apps.web import run_manager as rm
    from muteki.core.events import EventType
    import apps.web.drivers as drivers

    async def _boom(run):
        raise RuntimeError("container did not start")

    monkeypatch.setattr(drivers, "build_standby_driver",
                        lambda cmd, mgr=None: _boom)

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.started = True
        run.finished = True
        run.solved = True
        run.task = None
        await run.bus.close()
        ok = await mgr.post_hitl("run-x", "global", "ask", text="continue")
        assert ok is True
        assert run.standby_task is not None
        await asyncio.gather(run.standby_task, return_exceptions=True)
        seen = [ev async for ev in run.store.replay("run-x")]
        return seen

    caplog.set_level("INFO")
    seen = asyncio.run(_run())
    assert any("standby worker failed" in r.message for r in caplog.records)
    reqs = [e for e in seen if e.event_type is EventType.HITL_REQUEST]
    assert reqs
    assert "container did not start" in reqs[-1].payload["need"]


def test_resolve_reuses_challenge_from_winner_json(tmp_path, monkeypatch):
    """resolve rebuilds the challenge from the durable winner.json snapshot so the
    re-solve targets the same host."""
    from apps.web import run_manager as rm
    import apps.web.drivers as drivers

    seen = {}

    async def _noop(run):
        return None

    def _capture(body, mgr=None):
        seen.update(body or {})
        return _noop
    monkeypatch.setattr(drivers, "build_driver", _capture)

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.started = True; run.finished = True; run.task = None
        # drop a winner.json with the original challenge
        wp = mgr.workspace_dir("run-x") / "winner.json"
        wp.write_text(json.dumps({"challenge": {
            "name": "expensey-eats", "category": "web",
            "target": "https://target.example/"}}))
        await mgr.resolve("run-x", {})
        if run.task:
            await asyncio.gather(run.task, return_exceptions=True)
        return seen

    seen = asyncio.run(_run())
    assert seen.get("challenge", {}).get("target") == "https://target.example/"
    assert seen["challenge"]["name"] == "expensey-eats"


def test_resolve_reuses_challenge_from_session_jsonl_without_winner(tmp_path, monkeypatch):
    """An unsolved run has no winner.json; resolve must recover the original
    challenge from the durable run.started JSONL instead of collapsing back to
    name/category only."""
    from apps.web import run_manager as rm
    from muteki.core.events import Event, EventType
    import apps.web.drivers as drivers

    seen = {}

    async def _noop(run):
        return None

    def _capture(body, mgr=None):
        seen.update(body or {})
        return _noop
    monkeypatch.setattr(drivers, "build_driver", _capture)

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        await run.bus.emit(Event(
            event_type=EventType.RUN_STARTED,
            run_id="run-x",
            payload={"challenge": {
                "name": "multi-target",
                "category": "web",
                "description": "solve the three flag service",
                "target": "https://live.example/",
                "expected_flags": 3,
                "multi_flag": True,
            }},
        ))
        run.finished = True; run.solved = False; run.task = None
        await mgr.resolve("run-x", {})
        if run.task:
            await asyncio.gather(run.task, return_exceptions=True)
        return seen

    seen = asyncio.run(_run())
    ch = seen["challenge"]
    assert ch["target"] == "https://live.example/"
    assert ch["description"] == "solve the three flag service"
    assert ch["expected_flags"] == 3
    assert ch["multi_flag"] is True


def test_resolve_noop_on_live_run(tmp_path):
    """resolve refuses to relaunch a run that's already live (use HITL instead)."""
    from apps.web import run_manager as rm

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.task = asyncio.create_task(asyncio.sleep(30))
        ok = await mgr.resolve("run-x", {})
        run.task.cancel()
        return ok

    assert asyncio.run(_run()) is False

# ── stop must settle a GHOST run (no live task but deck thinks it's running) ──
def test_post_hitl_stop_settles_ghost_run(tmp_path):
    """run-4305 class: a run whose event stream ended mid-flight (no terminating
    RUN_FINISHED) and whose task is dead. Stop must FORCE it finished + broadcast
    RUN_FINISHED so the deck unsticks — not silently no-op."""
    from apps.web import run_manager as rm
    from muteki.core.events import EventType

    async def _run():
        mgr = rm.RunManager(sessions_root=str(tmp_path / "sessions"))
        run = mgr.create("run-x")
        run.started = True
        run.finished = False     # deck thinks it's running...
        run.task = None          # ...but the task is dead (ghost)
        seen = []
        orig = run.bus.emit
        async def _spy(ev):
            seen.append(ev); await orig(ev)
        run.bus.emit = _spy
        ok = await mgr.post_hitl("run-x", "global", "stop")
        return ok, run, seen

    ok, run, seen = asyncio.run(_run())
    assert ok is True
    assert run.finished is True   # forced finished
    fin = [e for e in seen if e.event_type is EventType.RUN_FINISHED]
    assert fin, "stop on a ghost run must broadcast RUN_FINISHED so the deck settles"
