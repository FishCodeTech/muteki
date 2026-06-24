"""run-11189 regression: FOUND_FLAG markers that only ever appear in STREAMED text
(an intermediate assistant block, the worker's private reasoning, or a Bash echo's
tool output) must still be accepted — they used to be dropped because the only flag
path read the driver's terminal `result` text, which those channels never reach.

The fix funnels FOUND_FLAG through `_stream_markers` (called live on every stream
step's text, and as an end-of-run backstop), through the SAME provenance gate as the
terminal path. These tests exercise that funnel directly.
"""

from __future__ import annotations

import asyncio

import pytest

from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.models.solve_graph import Challenge
from muteki.solver.cli_solver import CliSolver


def _token_challenge() -> Challenge:
    # token flag_format + multi_flag, exactly like the run-11189 OSINT chain.
    return Challenge(
        id="run-test-stream",
        name="stream-flag",
        category="reverse",
        description="chained SSH levels; per-level token flags",
        flag_format="token",
        multi_flag=True,
        expected_flags=15,
    )


def _solver(bus: EventBus) -> CliSolver:
    return CliSolver(
        spec=None,
        challenge=_token_challenge(),
        engine="claude",
        bus=bus,
        run_id="run-test-stream",
        solver_label="cli-claude-test",
    )


async def _flag_events(bus: EventBus) -> list[str]:
    seen: list[str] = []

    async def _sink(ev: Event) -> None:
        if (ev.event_type == EventType.BLACKBOARD_DELTA
                and (ev.payload or {}).get("kind") == "flag_found"):
            seen.append((ev.payload or {}).get("flag"))
    bus.add_sink(_sink)
    return seen


@pytest.mark.asyncio
async def test_found_flag_in_streamed_reasoning_is_accepted():
    """cursor L1 case: a clean FOUND_FLAG= in an intermediate assistant/reasoning
    chunk (never in the terminal result) must register."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)

    chunk = (
        "[cursor] L1 解出。\n"
        "VERIFIED_FACT=admin panel = vault-admin.atlas-fintech.local\n"
        "FOUND_FLAG=bl_62c1be2414c0143a2da6b5b0982e12e7\n"
        "## L1 解题摘要 ...\n"
    )
    await sv._stream_markers(chunk)

    assert "bl_62c1be2414c0143a2da6b5b0982e12e7" in sv._already_found
    assert "bl_62c1be2414c0143a2da6b5b0982e12e7" in sv._stream_accepted
    assert "bl_62c1be2414c0143a2da6b5b0982e12e7" in list(sv.graph.flags)
    assert seen == ["bl_62c1be2414c0143a2da6b5b0982e12e7"]


@pytest.mark.asyncio
async def test_found_flag_in_bash_echo_tool_output_is_accepted():
    """claude L0 case: the worker emitted `echo \"FOUND_FLAG=...\"`; the flag lands in
    the tool RESULT text, which streams through _stream_markers too."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)

    tool_result_text = (
        "FOUND_FLAG=bl_18c5039503f973296aa31bbd5ac4fef4\n"
        'VERIFIED_FACT=L0 Paper Trail SOLVED\n'
    )
    await sv._stream_markers(tool_result_text)

    assert "bl_18c5039503f973296aa31bbd5ac4fef4" in sv._already_found
    assert seen == ["bl_18c5039503f973296aa31bbd5ac4fef4"]


@pytest.mark.asyncio
async def test_streamed_flag_deduped_across_chunks():
    """The same flag seen in several streamed chunks is accepted exactly once."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)

    chunk = "FOUND_FLAG=bl_aa11bb22cc33dd44ee55ff6600778899\n"
    await sv._stream_markers(chunk)
    await sv._stream_markers(chunk)          # same flag again — should be a no-op
    await sv._stream_markers("FOUND_FLAG=bl_aa11bb22cc33dd44ee55ff6600778899 trailing prose")

    assert seen == ["bl_aa11bb22cc33dd44ee55ff6600778899"]
    assert sv._stream_accepted == ["bl_aa11bb22cc33dd44ee55ff6600778899"]


@pytest.mark.asyncio
async def test_streamed_flag_provenance_gate_still_holds():
    """A FOUND_FLAG token that is a placeholder (no real content) must be rejected by
    the gate even on the stream path — the moat is unchanged."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)

    # placeholder body → gate rejects; a too-short / wordy token → strength floor rejects.
    await sv._stream_markers("FOUND_FLAG=flag{...}\n")
    await sv._stream_markers("FOUND_FLAG=todo\n")
    await sv._stream_markers("FOUND_FLAG=<the flag>\n")

    assert seen == []
    assert sv._stream_accepted == []


@pytest.mark.asyncio
async def test_multiple_distinct_flags_in_one_chunk_all_accepted():
    """A single streamed chunk can legitimately carry several level flags."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)

    chunk = (
        "FOUND_FLAG=bl_0011223344556677889900aabbccddee\n"
        "FOUND_FLAG=bl_ffeeddccbbaa00998877665544332211\n"
    )
    await sv._stream_markers(chunk)

    assert set(seen) == {
        "bl_0011223344556677889900aabbccddee",
        "bl_ffeeddccbbaa00998877665544332211",
    }
    assert len(sv._stream_accepted) == 2


@pytest.mark.asyncio
async def test_tool_command_text_does_not_register_a_flag():
    """run-11550 + run-75379 provenance: a flag may ONLY come from REAL command
    output (a tool_result), never from the worker's prose. Two non-output sources are
    rejected at the _emit_step seam:
      - a 'tool' step (the COMMAND about to run) — the worker's INTENT, not output.
        run-11550: codex ran `grep -E 'FOUND_FLAG=bl_|VERIFIED_FACT=.*L4|...'` and the
        grep PATTERN registered as a flag.
      - a 'reasoning' step (the worker's own thought) — restating `FOUND_FLAG=...` in
        reasoning is a CLAIM; gating it against the reasoning chunk itself is
        self-referential (the claim IS its own provenance) → trivially passes. That is
        exactly how the hallucinated flag02 got laundered through prose (run-75379), so
        reasoning is now allow_flags=False.
    Only a tool_result (real output) sources a flag."""
    from muteki.solver.cli_driver import StreamStep

    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)

    # the exact run-11550 vector: a grep command containing FOUND_FLAG=bl_<pattern>
    cmd = ("ps -axo pid,command | grep -E "
           "'turn-ended|FOUND_FLAG=bl_|VERIFIED_FACT=.*L4|last-assistant-message'")
    await sv._emit_step(StreamStep("tool", tool="shell", text=cmd))
    assert seen == [], "a grep PATTERN in a command must not register as a flag"
    assert sv._stream_accepted == []

    # a flag asserted only in REASONING (the worker's prose) must NOT register — it is
    # a claim, not real output (run-75379 anti-launder). reasoning still yields
    # facts/dead-ends, just never a flag.
    await sv._emit_step(StreamStep(
        "reasoning", text="solved it. FOUND_FLAG=bl_62c1be2414c0143a2da6b5b0982e12e7"))
    assert "bl_62c1be2414c0143a2da6b5b0982e12e7" not in seen, \
        "a flag claimed only in reasoning prose must not register (self-referential)"
    assert sv._stream_accepted == []

    # the SAME flag in a tool_RESULT (real output) IS accepted — the echo case
    await sv._emit_step(StreamStep(
        "tool_result", text="FOUND_FLAG=bl_18c5039503f973296aa31bbd5ac4fef4\n"))
    assert seen == ["bl_18c5039503f973296aa31bbd5ac4fef4"]


@pytest.mark.asyncio
async def test_laundered_flag_from_local_storage_rejected():
    """run-11551 anti-launder: a worker that GREPS another run's log / the engine's
    own history / a sibling process title for a flag token and restates it must NOT
    have it accepted — that token is from a DIFFERENT level, not a recovery from the
    target. Detected by the local-storage-scrape provenance signature."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)

    leaked = "bl_a2a9f8e886a1ee05c9e6892dfd692526"  # the actual run-11551 L2-flag leak
    # rg output over the engine's own session history
    await sv._stream_markers(
        f"/Users/x/.codex/sessions/rollout-abc.jsonl:12:FOUND_FLAG={leaked}\n")
    # grep over another run's persisted log
    await sv._stream_markers(
        f"sessions/run-11191.jsonl:99: FOUND_FLAG={leaked}\n")
    # the "harvest from a process title" phrasing
    await sv._stream_markers(
        f"extracting just that marker from live process output: FOUND_FLAG={leaked}\n")
    assert seen == [], "a flag scraped from local storage must not register"
    assert sv._stream_accepted == []

    # a genuine verifier recovery (no scrape signature) still registers
    await sv._stream_markers("[*] FLAG: FOUND_FLAG=bl_0011223344556677889900aabbccddee\n")
    assert "bl_0011223344556677889900aabbccddee" in seen


@pytest.mark.asyncio
async def test_anti_launder_does_not_false_reject_own_cwd():
    """The anti-launder guard must NOT trip on a worker reading its OWN working dir.
    Every worker legitimately operates under
    sessions/<run>/workspace/workers/<id>/ and reads its board/attachments there; a
    crypto/forensics solve may write its result to a local file in cwd. A flag that
    appears in such cwd output is a REAL recovery and must register — only reads of
    muteki/agent INTERNAL storage (other runs' .jsonl logs, the engine history dirs,
    process-title harvesting) are rejected."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)

    real = "bl_77aa88bb99cc00dd11ee22ff33445566"
    # worker wrote its decrypt/extract result into its own cwd and prints it
    await sv._stream_markers(
        "/Users/x/ccb/sessions/run-test-stream/workspace/workers/cli-claude-test/"
        f"out.txt:\nFOUND_FLAG={real}\n")
    assert real in seen, "a flag in the worker's own cwd output must register"


@pytest.mark.asyncio
async def test_static_flag_value_reused_across_runs_is_accepted():
    """Rivulet regression: a STATIC flag (same value every run) recovered from REAL
    target output must be ACCEPTED, even though an earlier run registered the same
    value. The old cross-run value-dedup rejected it as a "launder" — but most
    CTF/range flags are static, so re-solving the same challenge had its real flag
    refused (operator stuck at a false 1/4 for hours). Cross-run value-dedup was
    REMOVED; only the _LAUNDER_RE path signature (reading internal storage) defends.

    This test would FAIL on the old `_flag_belongs_to_another_run` code (the value
    was registered by another run → rejected), and PASSES now (value-dedup gone)."""
    # a brace-format challenge (Rivulet shape: flagN{uuid}), multi-flag.
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = CliSolver(
        spec=None,
        challenge=Challenge(
            id="run-test-static", name="rivulet", category="pwn",
            flag_format=r"flag\d?\{[^}]+\}", multi_flag=True, expected_flags=4),
        engine="claude", bus=bus, run_id="run-test-static",
        solver_label="cli-claude-static")

    # a value some OTHER run also used (static flag) — recovered here from clean,
    # real target output with NO local-storage scrape signature → must register.
    static = "flag2{9f23aa16-33e7-11f1-9508-7e92a294591d}"
    await sv._stream_markers(
        "$ wmiexec MICHA/Administrator@192.168.1.112\n"
        f"# type C:\\flag2.txt\nFOUND_FLAG={static}\n")
    assert static in seen, "a static flag recovered from real output must register"

    # the genuine launder (grepped from another run's log, with path signature) is
    # STILL rejected by _LAUNDER_RE — the real defense is intact.
    leaked = "flag3{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}"
    await sv._stream_markers(
        f"sessions/run-11191.jsonl:99: FOUND_FLAG={leaked}\n")
    assert leaked not in seen, "a flag grepped from another run's log must NOT register"


@pytest.mark.asyncio
async def test_flag_with_spaces_not_truncated():
    """NYU-eval BUG-2 regression: a brace flag whose body contains SPACES (e.g.
    `flag{H1570rY 12'N7 k1ND ...}`) must register in full — the old `\\S+` capture
    truncated it at the first space (`flag{H1570rY`) so it never closed → never
    registered, despite the worker actually solving the challenge."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)
    sv.challenge.flag_format = r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}"

    flag = "flag{H1570rY 12 N7 k1ND 70 7h053 wh0 Pl4Y g0d}"
    await sv._stream_markers(f"Solved! FOUND_FLAG={flag}\noutput was: {flag}\n")
    assert flag in sv._already_found, "a space-containing brace flag must register in full"


@pytest.mark.asyncio
async def test_flag_strips_trailing_markdown_and_prose():
    """NYU-eval BUG-1 regression: a worker that writes `FOUND_FLAG=flag{x}**` or
    appends prose on the same line (`flag{x}两个任务...`) must register the CLEAN
    flag — the `\\S+`/no-clean path kept the trailing `**`/prose in the token."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv = _solver(bus)
    sv.challenge.flag_format = r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}"

    real = "flag{S4f3_l1nk1nG_GL0B4L5}"
    await sv._stream_markers(f"FOUND_FLAG={real}** then more notes\n[*] {real}\n")
    assert real in sv._already_found
    assert real + "**" not in sv._already_found


# ── run-75379 BUG③: durable reject survives worker RESPAWN ────────────────────
# The load-bearing assertion Codex flagged as MISSING: after mark_false(X), a FRESH
# worker (empty _already_found, as a reopen respawn always is) must have _accept_flag(X)
# return False AND emit no flag_found broadcast — the reject memory is durable, read
# from the shared graph, not from worker-local state.

def _graph_solver(bus: EventBus, tmp_path, *, shared_graph=None, label="cli-fresh"):
    """A CliSolver wired to a real SQLiteSharedGraph, brace flag_format so plain
    flag{...} values are acceptable (the token strength floor would reject them)."""
    from muteki.swarm.shared_graph import SQLiteSharedGraph
    ch = Challenge(
        id="run-75379", name="reject-respawn", category="web",
        flag_format=r"flag\{[^}]+\}", multi_flag=True, expected_flags=4)
    sg = shared_graph or SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=ch)
    sv = CliSolver(
        spec=None, challenge=ch, engine="claude", bus=bus,
        run_id="run-75379", solver_label=label, shared_graph=sg)
    return sv, sg


@pytest.mark.asyncio
async def test_invalidated_flag_refused_on_fresh_worker(tmp_path):
    """After the operator marks flag{a} false, a brand-new worker that re-derives it
    (its _already_found is empty) must NOT re-accept it and must NOT broadcast it."""
    bus = EventBus()
    seen = await _flag_events(bus)

    # worker 1 accepts flag{a} from real output...
    sv1, sg = _graph_solver(bus, tmp_path, label="cli-w1")
    await sv1._stream_markers("output: FOUND_FLAG=flag{a}\n", allow_flags=True)
    assert "flag{a}" in seen

    # ...operator marks it a false positive (writes EV_FLAG_INVALIDATED).
    sg.reopen_after_false_positive(actor="operator", flag="flag{a}")

    # a FRESH worker (separate instance, empty _already_found) re-derives flag{a}.
    seen.clear()
    sv2, _ = _graph_solver(bus, tmp_path, shared_graph=sg, label="cli-fresh")
    assert sv2._already_found == set(), "a respawned worker starts with no local flags"
    assert "flag{a}" in sv2._rejected_flags()

    accepted = await sv2._accept_flag("flag{a}")
    assert accepted is False, "an invalidated flag must not be re-accepted"
    assert "flag{a}" not in seen, "no flag_found broadcast for a rejected flag"

    # and via the full stream path (the live re-occupation vector): still nothing.
    await sv2._stream_markers("re-derived: FOUND_FLAG=flag{a}\n", allow_flags=True)
    assert "flag{a}" not in seen
    assert "flag{a}" not in sv2._stream_accepted


@pytest.mark.asyncio
async def test_non_rejected_flag_still_accepted_on_fresh_worker(tmp_path):
    """The reject gate is value-specific: a DIFFERENT flag from a fresh worker still
    registers normally (we didn't break ordinary acceptance)."""
    bus = EventBus()
    seen = await _flag_events(bus)
    sv1, sg = _graph_solver(bus, tmp_path, label="cli-w1")
    await sv1._stream_markers("output: FOUND_FLAG=flag{a}\n", allow_flags=True)
    sg.reopen_after_false_positive(actor="operator", flag="flag{a}")

    seen.clear()
    sv2, _ = _graph_solver(bus, tmp_path, shared_graph=sg, label="cli-fresh")
    assert await sv2._accept_flag("flag{b}") is True
    assert seen == ["flag{b}"]
    # the rejected sibling is still refused on the same worker
    assert await sv2._accept_flag("flag{a}") is False


@pytest.mark.asyncio
async def test_rejected_flag_block_in_worker_prompt(tmp_path):
    """The known-bad value is surfaced in the worker prompt so the LLM stops
    re-deriving it (FIX #4)."""
    bus = EventBus()
    _ = await _flag_events(bus)
    sv1, sg = _graph_solver(bus, tmp_path, label="cli-w1")
    sg.flag_found(actor="cli-w1", flag="flag{a}")
    sg.reopen_after_false_positive(actor="operator", flag="flag{a}")

    sv2, _ = _graph_solver(bus, tmp_path, shared_graph=sg, label="cli-fresh")
    block = sv2._rejected_flags_block()
    assert "flag{a}" in block
    assert "FALSE POSITIVE" in block.upper()
    # empty when nothing is rejected (byte-identical prompt on the common path)
    bus3 = EventBus()
    sv3, _ = _graph_solver(bus3, tmp_path / "clean", label="cli-clean")
    assert sv3._rejected_flags_block() == ""
