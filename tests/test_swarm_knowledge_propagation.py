"""Integration tests pinning the fixes for the operator's THREE core complaints
(see docs/DESIGN_swarm_knowledge_propagation.md). Each test drives the real
CliSolver / SQLiteSharedGraph / InsightBus and asserts the root-cause link is now
blocked — these are the "mock test ensures the 3 problems are fixed" acceptance.

  ① 新 worker 重走老路       → P1 (board shows attempted directions, incl. bootstrap)
  ② 知识总结 40 字限制       → P5 (gist no longer cuts anchors; raw stays whole)
  ③ fact/hint 传不到 + flag1
     做好几次                → P0 (static flag accepted) + P2 (multi-flag keeps
                                workers alive) + P3 (fold)
"""

from __future__ import annotations

import asyncio

import pytest

from muteki.core.event_bus import EventBus
from muteki.models.solve_graph import Challenge
from muteki.solver.cli_solver import CliSolver
from muteki.swarm.insight_bus import Insight, InsightBus, InsightKind
from muteki.swarm.shared_graph import SQLiteSharedGraph


def _graph(tmp_path) -> SQLiteSharedGraph:
    ch = Challenge(id="run-kp", name="rivulet", category="pwn",
                   flag_format=r"flag\d?\{[^}]+\}", multi_flag=True, expected_flags=4)
    return SQLiteSharedGraph(str(tmp_path / "g.db"), ch)


def _worker(bus, *, insight=None, shared_graph=None, workdir=None, sid="cli-claude-1"):
    return CliSolver(
        spec=None,
        challenge=Challenge(id="run-kp", name="rivulet", category="pwn",
                            flag_format=r"flag\d?\{[^}]+\}", multi_flag=True,
                            expected_flags=4),
        engine="claude", bus=bus, run_id="run-kp", solver_label=sid,
        insight=insight, shared_graph=shared_graph, workdir=workdir)


# ─────────────────────────────────────────────────────────────────────────────
# 问题 ①:新 worker 重走老路 —— P1 三块拼图
# ─────────────────────────────────────────────────────────────────────────────

def test_p1a_board_shows_attempted_directions_to_workers(tmp_path):
    """P1-A: a WORKER's board (to_board_markdown) must now show concluded/attempted
    directions, not just the Reason planner's view. Before, a new worker re-walked
    them ("重走老路")."""
    g = _graph(tmp_path)
    g.propose_intent(actor="reason", intent_id="intent:i1",
                     goal="brute-force the Shiro AES key with a public key list")
    g.claim_intent(worker="cli-codex-9", intent_id="intent:i1")
    g.conclude_intent(actor="cli-codex-9", intent_id="intent:i1",
                      result="dead_end: random Shiro key, 1107-key list all miss")

    board = g.to_board_markdown()
    assert "Already attempted" in board, "board must list attempted directions"
    assert "Shiro AES key" in board, "the concluded direction's goal must be visible"
    assert "1107-key list all miss" in board, "and its result, so nobody re-runs it"


def test_p1b_bootstrap_intent_lands_in_db_so_board_sees_it(tmp_path):
    """P1-B: a bootstrap/whole-challenge worker's intent must be recorded in the DB
    intents table (was only an SSE _emit_bb), so _attempted_intents_block sees it.
    This is the MAJORITY of "重走老路" workers — each new bootstrap re-running recon."""
    g = _graph(tmp_path)
    bus = EventBus()
    sv = _worker(bus, shared_graph=g, sid="cli-claude-boot1")
    sv._intent_id = "intent:cli-claude-boot1"

    # the new helpers the bootstrap path calls
    sv._record_intent_db("Solve rivulet [pwn]")        # propose+claim, like turn start
    sv._conclude_intent_db(result="explored but found no verified flag")  # barren end

    board = g.to_board_markdown()
    assert "Solve rivulet" in board, \
        "a concluded bootstrap attempt must show on the board (was invisible before)"
    # and it counts as a barren-concluded direction for the planner's dedup
    assert "Solve rivulet [pwn]" in g.barren_concluded_goal_texts()


def test_p1c_attempted_directions_inlined_into_prompt(tmp_path):
    """P1-C: the already-attempted directions must be INLINED into the prompt, not
    only in the .muteki_board.md file a headless worker may skip. Verifies the
    ruled-out digest renders in the board-context prompt block."""
    g = _graph(tmp_path)
    g.propose_intent(actor="reason", intent_id="intent:x",
                     goal="enumerate /admin via login brute")
    g.claim_intent(worker="w", intent_id="intent:x")
    g.conclude_intent(actor="w", intent_id="intent:x", result="dead_end: 401 always")

    bus = EventBus()
    sv = _worker(bus, shared_graph=g)
    sv._board_file_written = True   # simulate the file was written (normal path)
    block = sv._board_context()     # the actual prompt block a worker sees
    assert "Already attempted" in block, \
        "ruled-out directions must be inlined in the prompt, not only in the file"
    assert "/admin via login brute" in block


def test_p1_escape_valve_productive_direction_not_deduped(tmp_path):
    """P1 escape-valve: a concluded direction that PRODUCED a fact (to_fact_seq set)
    is NOT in the barren-dedup set — so it can be re-proposed under new evidence
    (avoids run-7349 starvation). Only barren (no-fact) directions are suppressed."""
    g = _graph(tmp_path)
    # a barren one (no fact) → suppressed
    g.propose_intent(actor="reason", intent_id="intent:barren", goal="barren probe")
    g.claim_intent(worker="w", intent_id="intent:barren")
    g.conclude_intent(actor="w", intent_id="intent:barren", result="explored")
    # a productive one (attached a fact) → NOT suppressed
    fs = g.add_evidence(actor="w", source="w", fact="found creds", verified=True)
    g.propose_intent(actor="reason", intent_id="intent:prod", goal="productive probe")
    g.claim_intent(worker="w", intent_id="intent:prod")
    g.conclude_intent(actor="w", intent_id="intent:prod", result="explored",
                      to_fact_seq=fs)   # add_evidence returns the int fact seq

    barren = g.barren_concluded_goal_texts()
    assert "barren probe" in barren, "a barren direction is deduped"
    assert "productive probe" not in barren, \
        "a fact-producing direction stays re-proposable under new evidence"


# ─────────────────────────────────────────────────────────────────────────────
# 问题 ②:知识总结 40 字限制 —— P5
# ─────────────────────────────────────────────────────────────────────────────

def test_p5_gist_fallback_never_cuts_a_flag_in_half():
    """P5: fallback_summary must keep a flag/credential WHOLE even past the char cap
    — operator saw 'flag2{9f23aa1' (cut). A half-flag label is worse than a long one."""
    from muteki.solver.summarizer import fallback_summary
    raw = ("[claude] the secret table holds "
           "flag2{9f23aa16-33e7-11f1-9508-7e92a294591d} on host 192.168.1.123")
    out = fallback_summary(raw, max_chars=40)
    assert "flag2{9f23aa16-33e7-11f1-9508-7e92a294591d}" in out, \
        "the full flag must survive truncation, not be cut mid-token"
    assert "[claude]" not in out  # worker tag still stripped


def test_p5_gist_is_label_only_worker_reads_raw_fact(tmp_path):
    """P5: confirm the gist is a pure front-end label — the worker board renders the
    RAW fact verbatim, never the (possibly truncated) gist. So the 40-char limit
    never affected the model; only the operator's deck view."""
    g = _graph(tmp_path)
    long_fact = ("DCSync recovered Administrator NTLM hash "
                 ":015f5d04d14d053508d14ac11d6496bb on DC01 192.168.1.83")
    g.add_evidence(actor="cli-codex", source="codex", fact=long_fact, verified=True)
    g.record_fact_summary(fact_seq=1, summary="拿到域管hash")   # a short gist stored

    board = g.to_board_markdown()
    assert "015f5d04d14d053508d14ac11d6496bb" in board, \
        "the worker board must carry the FULL raw fact (hash), not the short gist"
    assert "拿到域管hash" not in board, "the gist is front-end only, never on the board"


# ─────────────────────────────────────────────────────────────────────────────
# 问题 ③:fact/hint 传不到 + flag1 做好几次 —— P0 + P2 + P3
# ─────────────────────────────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_p0_static_flag_reused_across_runs_accepted():
    """P0: a static flag (same value across runs) recovered from REAL output must be
    accepted — the cross-run value-dedup误杀 that stuck the operator at a false 1/4
    is gone. (Also covered in test_stream_flag_extraction; kept here as the ③ link.)"""
    bus = EventBus()
    seen = []

    async def _sink(ev):
        from muteki.core.events import EventType
        if ev.event_type == EventType.BLACKBOARD_DELTA and (ev.payload or {}).get("kind") == "flag_found":
            seen.append((ev.payload or {}).get("flag"))
    bus.add_sink(_sink)
    sv = _worker(bus)
    static = "flag2{9f23aa16-33e7-11f1-9508-7e92a294591d}"
    await sv._stream_markers(f"# type C:\\flag2.txt\nFOUND_FLAG={static}\n")
    assert static in seen, "a static flag from real output must register"


def test_p2_multi_flag_sibling_flag_does_not_interrupt_current_turn():
    """P2: multi-flag collection must keep sibling workers alive after one teammate
    lands a flag. The worker records the flag so it won't re-hunt it, but only the
    explicit ALL_FLAGS_FOUND signal may stop it."""
    bus = EventBus()
    insight = InsightBus("run-kp")
    sv = _worker(bus, insight=insight)
    sv._insight_inbox = insight.subscribe(sv.solver_id)
    sv._turn_active = True   # a subprocess turn is running (steer is only valid then)
    assert not sv._steer_event.is_set()

    # a sibling's NEW flag arrives mid-turn
    sv._insight_inbox.put_nowait(
        Insight(InsightKind.FLAG, "cli-codex-2", "flag1{aaaa-bbbb}"))
    sv._drain_control()
    assert "flag1{aaaa-bbbb}" in sv._already_found, "the flag is noted"
    assert not sv._steer_event.is_set(), \
        "a partial multi-flag FlagFound must not steer-kill a live worker"


def test_p2_single_flag_sibling_flag_still_interrupts_current_turn():
    """Single-flag mode keeps the old first-valid-flag-wins behavior: a sibling flag
    can still end this worker's current pass."""
    bus = EventBus()
    insight = InsightBus("run-single")
    sv = CliSolver(
        spec=None,
        challenge=Challenge(id="run-single", name="single", category="web",
                            flag_format=r"flag\{[^}]+\}"),
        engine="claude", bus=bus, run_id="run-single", solver_label="cli-claude-1",
        insight=insight,
    )
    sv._insight_inbox = insight.subscribe(sv.solver_id)
    sv._turn_active = True

    sv._insight_inbox.put_nowait(
        Insight(InsightKind.FLAG, "cli-codex-2", "flag{one-and-done}"))
    sv._drain_control()

    assert "flag{one-and-done}" in sv._already_found
    assert sv._steer_event.is_set()


def test_p2_backlog_replay_does_not_steerkill_fresh_worker():
    """run-40726 regression: a FRESHLY SPAWNED worker drains the InsightBus HISTORY
    backlog (every prior FLAG + standing hint replayed to new subscribers) BEFORE its
    first subprocess turn starts (_turn_active=False). Those must be CONSUMED (folded
    into _already_found / _standing_guidance) but must NOT set _steer_event — else the
    not-yet-started subprocess is killed instantly → 0 tokens → conclude fallback.
    This is exactly why 60+ claude workers quick-exited after flag1 landed."""
    bus = EventBus()
    insight = InsightBus("run-kp")
    # a prior FLAG + standing hint already in history (the backlog a new worker gets)
    asyncio.run(insight.flag_found("cli-codex-1", "flag1{already-found}"))
    asyncio.run(insight.guidance("use the VPS 1.2.3.4", action="hint",
                                 target="global", standing=True))
    sv = _worker(bus, insight=insight)          # fresh worker, _turn_active=False
    sv._insight_inbox = insight.subscribe(sv.solver_id)   # replays the backlog
    sv._drain_control()

    # the knowledge IS consumed...
    assert "flag1{already-found}" in sv._already_found
    assert "use the VPS 1.2.3.4" in sv._standing_guidance
    # ...but the not-yet-started turn is NOT steer-killed (the regression)
    assert not sv._steer_event.is_set(), \
        "history-backlog replay must NOT steer-kill a worker before its turn starts"


def test_p2_duplicate_flag_does_not_thrash_steer():
    """P2 guard: a flag this worker ALREADY knows (InsightBus replays history) must
    NOT re-trigger a steer — only genuinely new flags interrupt."""
    bus = EventBus()
    insight = InsightBus("run-kp")
    sv = _worker(bus, insight=insight)
    sv._insight_inbox = insight.subscribe(sv.solver_id)
    sv._already_found.add("flag1{known}")
    sv._insight_inbox.put_nowait(Insight(InsightKind.FLAG, "cli-x", "flag1{known}"))
    sv._drain_control()
    assert not sv._steer_event.is_set(), "a known flag must not thrash the steer"


def test_resume_falls_back_to_execute_when_session_not_established():
    """run-42598 regression: a `build_resume` (claude `-r <sid>`) against a session
    the engine never actually seated returns "No conversation found" → 0 tokens →
    "(no output)" → instant dead_end, and never-give-up re-spawns into the same trap
    (claude-5..33 each lived ~1.7s). Until a turn really produces output, a resume
    turn must fall back to a FRESH execute, not `-r` a ghost session."""
    bus = EventBus()
    sv = _worker(bus)
    assert not sv._session_established, "a fresh worker has no established session"

    # session NOT established → must NOT emit a resume (-r) argv; falls back to execute
    argv = sv._resume_or_execute_argv("CONTINUE", "ghost-uuid-never-seated")
    assert "-r" not in argv, "resume of an unseated session must fall back to execute"
    assert "ghost-uuid-never-seated" not in argv, "the ghost session id is not reused"
    assert "-p" in argv, "the fallback is a fresh execute"


def test_resume_used_once_session_is_established():
    """The flip side: once a turn really produced output, _mark_session_if_live flips
    the guard and a later resume turn legitimately uses `-r <sid>` (keeps context)."""
    from muteki.solver.cli_driver import CliResult
    bus = EventBus()
    sv = _worker(bus)
    # a turn that produced real output seats the session
    sv._mark_session_if_live(CliResult(text="found something", output_tokens=42,
                                       session="real-sess-123"))
    assert sv._session_established

    argv = sv._resume_or_execute_argv("CONTINUE", "real-sess-123")
    assert "-r" in argv and "real-sess-123" in argv, \
        "an established session resumes with -r to keep its memory"


def test_zero_token_turn_does_not_establish_session():
    """A turn that 0-token-died (the exact failure mode) must NOT mark the session
    established — otherwise the next resume would walk straight back into the trap."""
    from muteki.solver.cli_driver import CliResult
    bus = EventBus()
    sv = _worker(bus)
    sv._mark_session_if_live(CliResult(text="", output_tokens=0, session=None))
    assert not sv._session_established, "an empty 0-token turn never seats a session"


def test_p2_standing_guidance_does_not_interrupt_current_turn():
    """Standing guidance is persistent context for current/future prompts, not a
    live-turn kill switch. A VPS/SSH hint sent after race start must not empty-kill
    every running worker."""
    bus = EventBus()
    insight = InsightBus("run-kp")
    sv = _worker(bus, insight=insight)
    sv._insight_inbox = insight.subscribe(sv.solver_id)
    sv._turn_active = True   # a subprocess turn is running (steer is only valid then)
    asyncio.run(
        insight.guidance("stop Shiro, use Fastjson", action="hint",
                         target="global", standing=True))
    sv._drain_control()
    assert "stop Shiro, use Fastjson" in sv._standing_guidance
    assert not sv._steer_event.is_set(), \
        "standing guidance must not interrupt an already-running worker"


def test_p3_peer_fact_propagates_via_shared_board(tmp_path):
    """P3: a teammate's VERIFIED fact reaches every other worker through the SHARED
    GRAPH board (the real channel), not a per-worker prompt buffer. _record_fact
    writes add_evidence → to_board_markdown carries it into the next-turn prompt.
    (The old InsightBus FACT/DEAD_END prompt-buffer was redundant dead code and is
    gone; this pins the channel that actually works.)"""
    g = _graph(tmp_path)
    g.add_evidence(actor="cli-codex-3", source="codex",
                   fact="admin/admin works on /doLogin", verified=True,
                   artifact_id="a1")
    board = g.to_board_markdown()
    assert "admin/admin works on /doLogin" in board, \
        "a teammate's verified fact must appear on every worker's board"


def test_p3_drain_control_does_not_crash_on_peer_fact_events():
    """The FACT/DEAD_END branch of _drain_control is now a no-op (board is the
    channel) — draining such events must not raise and must not steer the turn."""
    bus = EventBus()
    insight = InsightBus("run-kp")
    sv = _worker(bus, insight=insight)
    sv._insight_inbox = insight.subscribe(sv.solver_id)
    sv._insight_inbox.put_nowait(
        Insight(InsightKind.FACT, "cli-codex-3", "admin/admin works"))
    sv._insight_inbox.put_nowait(
        Insight(InsightKind.DEAD_END, "cli-codex-3", "Shiro key brute is futile"))
    sv._drain_control()  # must not raise
    assert not sv._steer_event.is_set(), "a peer fact must not interrupt the turn"


# ─────────────────────────────────────────────────────────────────────────────
# P4:动作级去重(并发打同一目标)
# ─────────────────────────────────────────────────────────────────────────────

def test_p4_activity_lock_prevents_concurrent_same_target(tmp_path):
    """P4: two workers must not both nmap the same target. The first claims the
    activity; the second gets LOST and avoids it. Lease-expiry self-heals."""
    g = _graph(tmp_path)
    assert g.try_claim_activity(worker="A", key="nmap 8.130.96.176") is True
    assert g.try_claim_activity(worker="B", key="NMAP:8.130.96.176") is False, \
        "a second worker must not redo the same activity"
    # a DIFFERENT target is fine
    assert g.try_claim_activity(worker="B", key="nmap 8.130.96.177") is True
    # the board shows in-progress activities so a worker's prompt avoids them
    board = g.to_board_markdown()
    assert "In progress" in board and "nmap:8.130.96.176" in board
    # release re-opens it
    g.release_activity(worker="A", key="nmap 8.130.96.176")
    assert g.try_claim_activity(worker="B", key="nmap:8.130.96.176") is True
