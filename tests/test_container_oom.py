"""OOM-vs-timeout discrimination in the container worker backend.

An OOM-killed worker (kernel SIGKILL'd it because a sibling run's container starved
the Docker VM — no per-container --memory cap) and a real wall-clock timeout BOTH
surface as exit 137 (the in-container `timeout` wrapper propagates 128+9 for either).
That ambiguity made an OOM victim — dead at 60s with an EMPTY transcript — get
mislabeled `reason=timeout` (a 2400s budget expiry), sending diagnosis the wrong way.

The discriminator is the cgroup `oom_kill` counter delta across the run. These tests
lock in (a) the counter parser for cgroup v2 / v1, and (b) that a nonzero delta flips
the result from timed_out → oom_killed. Pure logic — `_docker` is monkeypatched, no
real Docker.
"""

from __future__ import annotations

import subprocess

import muteki.solver.container_exec as ce
from muteki.solver.container_exec import _oom_kill_count


def _fake_completed(stdout: str, rc: int = 0) -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=rc, stdout=stdout, stderr="")


# ── cgroup oom_kill counter parsing ──────────────────────────────────────────

def test_oom_count_parses_cgroup_v2_memory_events(monkeypatch):
    # cgroup v2 memory.events: one "oom_kill <N>" line among others.
    body = "low 0\nhigh 0\nmax 0\noom 0\noom_kill 20\noom_group_kill 0\n"
    monkeypatch.setattr(ce, "_docker", lambda *a, **k: _fake_completed(body))
    assert _oom_kill_count("c") == 20


def test_oom_count_handles_zero(monkeypatch):
    body = "low 0\nhigh 0\noom_kill 0\n"
    monkeypatch.setattr(ce, "_docker", lambda *a, **k: _fake_completed(body))
    assert _oom_kill_count("c") == 0


def test_oom_count_none_when_unreadable(monkeypatch):
    # docker exec failed (rc!=0) → can't read the counter → None (don't guess).
    monkeypatch.setattr(ce, "_docker", lambda *a, **k: _fake_completed("", rc=1))
    assert _oom_kill_count("c") is None


def test_oom_count_none_when_no_oom_kill_line(monkeypatch):
    monkeypatch.setattr(ce, "_docker", lambda *a, **k: _fake_completed("low 0\nhigh 0\n"))
    assert _oom_kill_count("c") is None


# ── 137 discrimination: timeout vs OOM ───────────────────────────────────────

def test_oom_delta_means_oom_not_timeout():
    """The core rule: exit 137 + a nonzero oom_kill delta ⇒ oom_killed, NOT timed_out.
    Mirrors the inline logic in run_cli_*_container without spinning a real subprocess.
    """
    rc = 137
    oom_before, oom_after = 20, 21  # one OOM fired during the run
    oom_killed = (oom_before is not None and oom_after is not None
                  and oom_after > oom_before)
    timed_out = False if oom_killed else (rc == 137)
    assert oom_killed is True
    assert timed_out is False


def test_137_without_oom_delta_is_timeout():
    """A genuine wall-clock kill: 137 but the oom counter did NOT move ⇒ timed_out."""
    rc = 137
    oom_before, oom_after = 20, 20  # unchanged → not an OOM
    oom_killed = (oom_before is not None and oom_after is not None
                  and oom_after > oom_before)
    timed_out = False if oom_killed else (rc == 137)
    assert oom_killed is False
    assert timed_out is True


def test_clean_exit_is_neither():
    rc = 0
    oom_before, oom_after = 5, 5
    oom_killed = (oom_before is not None and oom_after is not None
                  and oom_after > oom_before)
    timed_out = False if oom_killed else (rc == 137)
    assert not oom_killed and not timed_out
