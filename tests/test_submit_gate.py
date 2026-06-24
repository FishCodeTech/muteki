"""Submission gate (rate-limited verifier coordination).

When a challenge opts in via Challenge.verifier_rate_limited, the swarm serializes
submissions and backs off globally on a cooldown/burn-lockout, instead of N workers
each independently burning the shared per-player attempt budget (run-11552 L4: 7
lock-hits vs 2 real submissions). These tests pin:

  - lockout-duration parsing (the binding backoff is the LONGEST duration seen)
  - a worker broadcasts VERIFIER_LOCKED + records a deadline when it sees a cooldown
  - SUBMIT_LOCKED / VERIFIER_LOCKED / SUBMIT_UNLOCKED toggle the worker's hold state
  - READY_TO_SUBMIT surfaces a board delta + a SUBMIT_LOCKED broadcast (serialize)
  - the gate prompt block + all detection are NO-OPs when the flag is off
    (every ordinary CTF path stays byte-identical)
"""

from __future__ import annotations

import asyncio

import pytest

from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.models.solve_graph import Challenge
from muteki.solver.cli_solver import (
    CliSolver, _parse_lockout_seconds, _looks_like_verifier_output)
from muteki.swarm.insight_bus import Insight, InsightBus, InsightKind


def _challenge(*, rate_limited: bool) -> Challenge:
    return Challenge(
        id="run-test-gate",
        name="gate",
        category="reverse",
        description="chained SSH levels; rate-limited per-player verifier",
        flag_format="token",
        multi_flag=True,
        expected_flags=15,
        verifier_rate_limited=rate_limited,
    )


def _solver(bus: EventBus, *, rate_limited: bool, insight: InsightBus | None = None) -> CliSolver:
    return CliSolver(
        spec=None,
        challenge=_challenge(rate_limited=rate_limited),
        engine="claude",
        bus=bus,
        run_id="run-test-gate",
        solver_label="cli-claude-gate",
        insight=insight,
    )


async def _bb_deltas(bus: EventBus, kind: str) -> list[dict]:
    seen: list[dict] = []

    async def _sink(ev: Event) -> None:
        if (ev.event_type == EventType.BLACKBOARD_DELTA
                and (ev.payload or {}).get("kind") == kind):
            seen.append(ev.payload or {})
    bus.add_sink(_sink)
    return seen


# ── lockout duration parsing ────────────────────────────────────────────────

@pytest.mark.parametrize("text,expected", [
    ("wait 8s between attempts", 8),
    ("cooldown: 45 seconds", 45),
    ("rate limited - retry in 5 min", 300),
    ("locked for 30 minutes", 1800),
    ("burn-lockout: locked for 30 minutes", 1800),
    ("Too many attempts. Try again in 2 hours.", 7200),
    # the LONGEST duration binds (an 8s per-attempt + a 30-min burn in one chunk)
    ("8s cooldown, also locked out for 30 min", 1800),
    # no lock word / no duration → nothing
    ("flag accepted, level unlocked", 0),
    ("the password is 30 chars long", 0),
])
def test_parse_lockout_seconds(text, expected):
    assert _parse_lockout_seconds(text) == expected


# ── worker detects a cooldown → broadcasts VERIFIER_LOCKED + sets deadline ──

@pytest.mark.asyncio
async def test_worker_broadcasts_verifier_locked_on_cooldown():
    bus = EventBus()
    insight = InsightBus("run-test-gate")
    other = insight.subscribe("cli-codex-sibling")  # a sibling inbox to receive it
    sv = _solver(bus, rate_limited=True, insight=insight)

    await sv._stream_markers(
        "specter-verify: too many wrong attempts — locked for 30 minutes")

    # the worker recorded a ~30-min local deadline and is now blocked from submitting
    assert sv._verifier_locked_now()
    # and broadcast it so siblings back off
    ins = other.get_nowait()
    assert ins.kind is InsightKind.VERIFIER_LOCKED
    assert int(ins.text) == 1800


# ── run-11553 regression: reading a doc that MENTIONS the lockout ≠ a real lock ──

@pytest.mark.parametrize("text,is_verdict", [
    # the verifier's OWN verdict output (phrasing + invocation footprint) → real
    ("$ /opt/verify-people-recon.sh\nburn-lockout: 3 burns in last 30 min — wait for the cooldown", True),
    ("specter-verify: too many wrong attempts — locked for 30 minutes", True),
    ("/opt/verify-people-recon.sh → 2 attempts remaining before lockout", True),
    # phrasing but NO invocation footprint → narrated prose, not a produced verdict
    ("burn-lockout: wait for the cooldown to lift", False),
    ("the challenge verifier is locked for 30 minutes after too many tries", False),
    # a worker READING our own doc that DESCRIBES the lockout (has BOTH phrasing AND
    # mentions /opt/verify-…sh) → rejected by the doc-read guard
    ("read: docs/PROBLEM_verifier_rate_limit_burn_lockout.md\n"
     "worker 对 /opt/verify-people-recon.sh 7 次撞 burn-lockout: 3 burns/30min → 30min lock", False),
    ("read: L4_known_intel.md\n跑 /opt/verify-people-recon.sh,burn-lockout: 3 错/30min → 锁 30min", False),
])
def test_only_real_verifier_verdict_counts_as_lockout(text, is_verdict):
    assert _looks_like_verifier_output(text) is is_verdict


@pytest.mark.asyncio
async def test_reading_problem_doc_does_not_broadcast_phantom_lockout():
    """run-11553: cursor read docs/PROBLEM_verifier_rate_limit_burn_lockout.md; the
    lock prose in the doc matched the duration regex and broadcast a fake 30-min
    VERIFIER_LOCKED that the whole swarm honored — though nobody had run the
    verifier. Reading a doc that DESCRIBES the lockout must NOT trigger a backoff."""
    bus = EventBus()
    insight = InsightBus("run-test-gate")
    other = insight.subscribe("cli-codex-sibling")
    sv = _solver(bus, rate_limited=True, insight=insight)

    # the exact false-trigger surface: the CLI's file-read step over our problem doc
    await sv._stream_markers(
        "read: /Users/x/ccb/docs/PROBLEM_verifier_rate_limit_burn_lockout.md\n"
        "现象: worker 对 verifier 7 次撞 burn-lockout ... 3 burns/30min → wait 30 minutes")

    assert not sv._verifier_locked_now(), "reading a doc must not lock the verifier"
    assert other.qsize() == 0, "no phantom VERIFIER_LOCKED should be broadcast"


@pytest.mark.asyncio
async def test_real_verifier_verdict_still_broadcasts():
    """The fix must NOT muzzle a genuine lockout: the verifier's real verdict output
    (with its characteristic phrasing) still records the deadline + broadcasts."""
    bus = EventBus()
    insight = InsightBus("run-test-gate")
    other = insight.subscribe("cli-codex-sibling")
    sv = _solver(bus, rate_limited=True, insight=insight)

    await sv._stream_markers(
        "$ /opt/verify-people-recon.sh\n"
        "specter-verify: burn-lockout: 3 burns in last 30 min — wait 30 minutes for cooldown")

    assert sv._verifier_locked_now()
    ins = other.get_nowait()
    assert ins.kind is InsightKind.VERIFIER_LOCKED
    assert int(ins.text) == 1800


@pytest.mark.asyncio
async def test_worker_does_not_rebroadcast_same_lockout():
    bus = EventBus()
    insight = InsightBus("run-test-gate")
    other = insight.subscribe("cli-codex-sibling")
    sv = _solver(bus, rate_limited=True, insight=insight)

    v = "$ /opt/verify-people-recon.sh\nburn-lockout: locked for 30 minutes"
    await sv._stream_markers(v)
    await sv._stream_markers(v)  # same verdict again → same deadline → no re-broadcast

    assert other.qsize() == 1  # only the first one went out


# ── inbound signals toggle the worker's hold state (via _drain_control) ──────

def _put(sv: CliSolver, ins: Insight) -> None:
    sv._insight_inbox.put_nowait(ins)


@pytest.mark.asyncio
async def test_submit_locked_signal_holds_then_self_clears():
    bus = EventBus()
    insight = InsightBus("run-test-gate")
    sv = _solver(bus, rate_limited=True, insight=insight)
    sv._insight_inbox = insight.subscribe(sv.solver_id)

    _put(sv, Insight(InsightKind.SUBMIT_LOCKED, "cli-codex-other", ""))
    sv._drain_control()
    assert sv._submit_blocked_now()

    # an explicit UNLOCK re-opens immediately
    _put(sv, Insight(InsightKind.SUBMIT_UNLOCKED, "cli-codex-other", "rejected: over-claim"))
    sv._drain_control()
    assert not sv._submit_blocked_now()


@pytest.mark.asyncio
async def test_verifier_locked_signal_sets_hard_deadline():
    bus = EventBus()
    insight = InsightBus("run-test-gate")
    sv = _solver(bus, rate_limited=True, insight=insight)
    sv._insight_inbox = insight.subscribe(sv.solver_id)

    _put(sv, Insight(InsightKind.VERIFIER_LOCKED, "cli-codex-other", "1800"))
    sv._drain_control()
    assert sv._verifier_locked_now()


# ── READY_TO_SUBMIT surfaces a board delta + a SUBMIT_LOCKED broadcast ──────

@pytest.mark.asyncio
async def test_ready_to_submit_surfaces_delta_and_broadcast():
    bus = EventBus()
    insight = InsightBus("run-test-gate")
    other = insight.subscribe("cli-codex-sibling")
    seen = await _bb_deltas(bus, "ready_to_submit")
    sv = _solver(bus, rate_limited=True, insight=insight)

    await sv._stream_markers(
        "READY_TO_SUBMIT=local validator OK, 18/18 people, admiralty conservative")

    assert len(seen) == 1
    assert "validator OK" in seen[0].get("note", "")
    ins = other.get_nowait()
    assert ins.kind is InsightKind.SUBMIT_LOCKED


# ── default-OFF: every detection is a no-op for an ordinary challenge ────────

@pytest.mark.asyncio
async def test_gate_is_noop_when_flag_off():
    bus = EventBus()
    insight = InsightBus("run-test-gate")
    other = insight.subscribe("cli-codex-sibling")
    ready = await _bb_deltas(bus, "ready_to_submit")
    sv = _solver(bus, rate_limited=False, insight=insight)  # NOT rate-limited

    # text that WOULD trip the gate on an opted-in challenge
    await sv._stream_markers("locked for 30 minutes")
    await sv._stream_markers("READY_TO_SUBMIT=looks good")

    assert not sv._verifier_locked_now()
    assert other.qsize() == 0          # nothing broadcast
    assert ready == []                 # no ready_to_submit delta
    assert sv._submit_gate_block() == ""  # no prompt block


def test_gate_prompt_block_only_when_flag_on():
    bus = EventBus()
    on = _solver(bus, rate_limited=True)
    off = _solver(bus, rate_limited=False)
    block = on._submit_gate_block()
    assert "submission discipline" in block.lower()
    assert "READY_TO_SUBMIT" in block
    assert off._submit_gate_block() == ""
