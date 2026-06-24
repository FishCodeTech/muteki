"""BE-auto-archive: the retention sweep (auto-archive idle runs, delete stale
archived ones, never touch pinned). Drives the logic with a faked idle clock so
it doesn't depend on real elapsed time. Pure/unit."""

from __future__ import annotations

import asyncio

from apps.web.run_manager import RunManager

DAY = 86400.0
NOW = 1_000_000.0


def _started(mgr: RunManager, rid: str):
    run = mgr.create(rid)
    run.started = True
    return run


def _sweep(mgr: RunManager, now: float = NOW):
    return asyncio.run(mgr.retention_sweep(
        now=now, archive_after_s=3 * DAY, delete_after_s=7 * DAY))


def test_archives_idle_run(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path)
    _started(mgr, "run-1")
    monkeypatch.setattr(mgr, "_last_activity", lambda run: NOW - 4 * DAY)  # idle 4d
    res = _sweep(mgr)
    assert res["archived"] == ["run-1"] and res["deleted"] == []
    m = mgr.meta.get("run-1")
    assert m["archived"] is True
    assert m["archived_at"] == NOW  # stamped for the delete step


def test_skips_fresh_run(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path)
    _started(mgr, "run-1")
    monkeypatch.setattr(mgr, "_last_activity", lambda run: NOW - 1 * DAY)  # idle 1d
    assert _sweep(mgr) == {"archived": [], "deleted": []}
    assert mgr.meta.get("run-1")["archived"] is False


def test_never_archives_pinned(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path)
    _started(mgr, "run-1")
    mgr.set_pinned("run-1", True, now=0.0)
    monkeypatch.setattr(mgr, "_last_activity", lambda run: NOW - 30 * DAY)  # very idle
    assert _sweep(mgr) == {"archived": [], "deleted": []}
    assert mgr.meta.get("run-1")["archived"] is False


def test_deletes_stale_archived_run(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path)
    _started(mgr, "run-1")
    mgr.set_archived("run-1", True, now=NOW - 10 * DAY)
    monkeypatch.setattr(mgr, "_last_activity", lambda run: NOW - 10 * DAY)  # idle 10d
    res = _sweep(mgr)
    assert res["deleted"] == ["run-1"] and res["archived"] == []
    assert mgr.get("run-1") is None  # hard-deleted


def test_keeps_recently_archived_run(tmp_path, monkeypatch):
    # archived but idle only 4 days (< 7) → stays; not deleted yet.
    mgr = RunManager(sessions_root=tmp_path)
    _started(mgr, "run-1")
    mgr.set_archived("run-1", True, now=NOW - 4 * DAY)
    monkeypatch.setattr(mgr, "_last_activity", lambda run: NOW - 4 * DAY)
    assert _sweep(mgr) == {"archived": [], "deleted": []}
    assert mgr.get("run-1") is not None


def test_skips_unknown_idle_clock(tmp_path, monkeypatch):
    # ts == 0 (no persisted events) → can't date it → never auto-touched.
    mgr = RunManager(sessions_root=tmp_path)
    _started(mgr, "run-1")
    monkeypatch.setattr(mgr, "_last_activity", lambda run: 0.0)
    assert _sweep(mgr) == {"archived": [], "deleted": []}


def test_skips_undispatched_draft(tmp_path, monkeypatch):
    # a run that never started (an empty draft stub) is not a conversation → skip.
    mgr = RunManager(sessions_root=tmp_path)
    mgr.create("run-draft")  # started stays False
    monkeypatch.setattr(mgr, "_last_activity", lambda run: NOW - 30 * DAY)
    assert _sweep(mgr) == {"archived": [], "deleted": []}


def test_set_archived_false_clears_archived_at(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    _started(mgr, "run-1")
    mgr.set_archived("run-1", True, now=NOW)
    assert mgr.meta.get("run-1")["archived_at"] == NOW
    mgr.set_archived("run-1", False)
    assert mgr.meta.get("run-1")["archived"] is False
    assert mgr.meta.get("run-1")["archived_at"] is None
