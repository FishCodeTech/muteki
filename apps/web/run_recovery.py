"""Run rehydrate and winner continuation. Moved from run_manager.py."""

from __future__ import annotations

import json
import os
import re
import stat
import time
from pathlib import Path
from typing import Any, Optional

from muteki.core.events import Event, EventType
from muteki.core.session_store import SessionStore

from apps.web.run_state import (  # noqa: F401
    LOG, Run, BoundRunConflictError, BoundRunStore, Driver,
    BOUND_RUN_ID_PREFIX, _safe_exception_detail, _runtime_error_id,
    _apply_blackboard_meta, _apply_operator_meta,
)

def coordinator_control_dir(self, run_id: str) -> Path:
    """Return the run's coordinator-only control directory.

    Worker containers receive the exact path returned by :meth:`workspace_dir`
    as a read-write bind mount.  This directory is rooted separately and the
    resolved-path check fails closed if configuration or a symlink would place
    it below that worker-visible tree.
    """
    safe = self._safe_run_id(run_id)
    directory = self.control_root / safe
    workspace = self.workspace_dir(run_id)
    resolved_directory = directory.resolve()
    worker_workspaces = {workspace, *self.sessions_root.glob("*/workspace")}
    for worker_workspace in worker_workspaces:
        if self._is_within(resolved_directory, worker_workspace.resolve()):
            raise RuntimeError(
                "coordinator control directory cannot be inside worker workspace")
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode):
        raise RuntimeError("run control directory must be a real directory")
    os.chmod(directory, 0o700)
    return directory


def _profile_readiness_path(self, run_id: str) -> Path:
    return self.control_root / self._safe_run_id(run_id) / "profile-readiness.json"


def _load_profile_readiness(
    self, run_id: str,
) -> dict[str, tuple[bool, Optional[dict[str, Any]]]]:
    """Load the task-owned probe results used by continuation generations."""
    path = self._profile_readiness_path(run_id)
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, ValueError, TypeError):
        return {}
    rows = payload.get("profiles") if isinstance(payload, dict) else None
    if not isinstance(rows, dict):
        return {}
    loaded: dict[str, tuple[bool, Optional[dict[str, Any]]]] = {}
    for key, value in rows.items():
        if not isinstance(key, str) or not isinstance(value, dict):
            continue
        ok = value.get("ok")
        failure = value.get("failure")
        if not isinstance(ok, bool):
            continue
        if failure is not None and not isinstance(failure, dict):
            continue
        loaded[key] = (ok, failure)
    return loaded


def persist_profile_readiness(self, run: Run) -> None:
    """Persist real profile probes outside every Worker-visible workspace."""
    directory = self.coordinator_control_dir(run.run_id)
    path = directory / "profile-readiness.json"
    temporary = directory / f".profile-readiness-{os.getpid()}-{time.time_ns()}.tmp"
    payload = {
        "version": 1,
        "profiles": {
            key: {"ok": ok, "failure": failure}
            for key, (ok, failure) in run.profile_readiness.items()
        },
    }
    temporary.write_text(
        json.dumps(payload, ensure_ascii=False, sort_keys=True),
        encoding="utf-8",
    )
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)


def _winner_continuation_path(self, run_id: str) -> Path:
    return (
        self.control_root / self._safe_run_id(run_id)
        / "winner-continuation.json"
    )


def persist_winner_continuation(
    self, run_id: str, payload: dict[str, Any],
) -> None:
    """Persist the resumable winner outside the Worker-writable workspace.

    Only stable identifiers and continuation data are retained. The current
    Worker configuration remains authoritative for credentials, endpoints,
    models and backend selection when a follow-up starts.
    """
    directory = self.coordinator_control_dir(run_id)
    path = directory / "winner-continuation.json"
    temporary = directory / (
        f".winner-continuation-{os.getpid()}-{time.time_ns()}.tmp"
    )
    workspace = self.workspace_dir(run_id).resolve()
    worker_root = (workspace / "workers").resolve()
    agent_state_root = (workspace / ".muteki-agent-state").resolve()
    workdir_rel = ""
    raw_workdir = str(payload.get("workdir") or "").strip()
    if raw_workdir:
        try:
            resolved_workdir = Path(raw_workdir).resolve()
            if self._is_within(resolved_workdir, worker_root):
                workdir_rel = str(resolved_workdir.relative_to(workspace))
        except (OSError, ValueError):
            workdir_rel = ""

    agent_state_rel = ""
    raw_agent_state = str(payload.get("agent_state_dir") or "").strip()
    if raw_agent_state:
        try:
            resolved_agent_state = Path(raw_agent_state).resolve()
            if self._is_within(resolved_agent_state, agent_state_root):
                agent_state_rel = str(
                    resolved_agent_state.relative_to(workspace))
        except (OSError, ValueError):
            agent_state_rel = ""

    raw_profile = payload.get("profile")
    profile_id = str(payload.get("profile_id") or "").strip()
    if not profile_id and isinstance(raw_profile, dict):
        profile_id = str(
            raw_profile.get("id") or raw_profile.get("name") or ""
        ).strip()
    backend = str(payload.get("backend") or "").strip()
    if backend not in {"local", "container"}:
        backend = ""
    flags = [
        str(value) for value in list(payload.get("flags") or [])
        if value is not None
    ]
    raw_first_flag = payload.get("flag")
    first_flag = str(raw_first_flag) if raw_first_flag is not None else None
    if first_flag is not None and first_flag not in flags:
        flags.insert(0, first_flag)
    challenge = payload.get("challenge")
    stored = {
        "version": 1,
        "worker_id": str(payload.get("worker_id") or "").strip(),
        "profile_id": profile_id,
        "engine": str(payload.get("engine") or "").strip(),
        "session": str(payload.get("session") or "").strip(),
        "workdir_rel": workdir_rel,
        "agent_state_rel": agent_state_rel,
        "backend": backend,
        "flag": flags[0] if flags else "",
        "flags": list(dict.fromkeys(flags)),
        "challenge": challenge if isinstance(challenge, dict) else {},
    }
    temporary.write_text(
        json.dumps(stored, ensure_ascii=False, sort_keys=True),
        encoding="utf-8",
    )
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)


def load_winner_continuation(self, run_id: str) -> dict[str, Any]:
    """Load coordinator-owned continuation metadata, failing closed."""
    path = self._winner_continuation_path(run_id)
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, ValueError, TypeError):
        return {}
    if not isinstance(payload, dict) or payload.get("version") != 1:
        return {}
    return dict(payload)


def update_winner_continuation_flags(
    self, run_id: str, flags: list[str],
) -> None:
    continuation = self.load_winner_continuation(run_id)
    if not continuation:
        return
    continuation["flags"] = list(dict.fromkeys(
        str(value) for value in flags if value is not None
    ))
    continuation["flag"] = (
        continuation["flags"][0] if continuation["flags"] else ""
    )
    rel = str(continuation.pop("workdir_rel", "") or "")
    agent_state_rel = str(
        continuation.pop("agent_state_rel", "") or "")
    continuation["workdir"] = (
        str((self.workspace_dir(run_id) / rel).resolve()) if rel else ""
    )
    continuation["agent_state_dir"] = (
        str((self.workspace_dir(run_id) / agent_state_rel).resolve())
        if agent_state_rel else ""
    )
    self.persist_winner_continuation(run_id, continuation)

def _recover_interrupted_followups(
    self, store: SessionStore, *, summaries: list[dict[str, Any]]
) -> None:
    """为进程退出时中断的 Ask/writeup 持久化失败终态。"""
    for summary in summaries:
        run_id = str(summary.get("run_id") or "")
        pending = dict(summary.get("pending_followups") or {})
        if not run_id or not pending:
            continue
        try:
            next_seq = int(summary.get("stream_seq") or 0) + 1
            for interrupted in pending.values():
                payload = {
                    "followup_id": interrupted["followup_id"],
                    "kind": interrupted["kind"],
                    "detail": "服务已重启，后续操作已中断",
                    "recovery_id": interrupted["recovery_id"],
                }
                generation = interrupted.get("execution_generation")
                if generation is not None:
                    payload["execution_generation"] = generation
                recovery = Event(
                    event_type=EventType.FOLLOWUP_FAILED,
                    run_id=run_id,
                    solver_id="web-runtime-recovery",
                    seq=next_seq,
                    payload=payload,
                )
                store.append_if_absent_sync(
                    recovery,
                    identity_field="recovery_id",
                    identity=interrupted["recovery_id"],
                )
                run = self.runs.get(run_id)
                if run is not None:
                    self.run_summaries.update(run, recovery)
                next_seq += 1
        except Exception as exc:
            LOG.error(
                "Interrupted follow-up recovery failed for %s error_type=%s",
                run_id, type(exc).__name__,
            )


def _rehydrate(self) -> None:
    """Re-populate the rail from durable JSONL on startup.

    Without this, a server restart drops every past conversation: self.runs
    starts empty so the rail shows nothing, AND _seq resets to 0 so the next
    "+ New solve" mints `run-0001` — colliding with a STALE run-0001.jsonl and
    replaying its old events under a "new" conversation. Hydrating both fixes
    history loss and the new-solve-shows-old-chat bug at once.

    We build lightweight Run handles (own bus + store) seeded with the
    persisted summary. The full event history is NOT loaded into memory here
    — the events SSE replays it from JSONL on demand. We only need the rail
    metadata + a correctly advanced _seq.
    """
    store = SessionStore(root=self.event_root)
    max_seq = 0
    # summaries() is newest-first; create() stamps created_seq in CALL order,
    # and the rail sorts by created_seq DESC — so feed oldest-first to keep the
    # newest conversation on top of the rail.
    summaries = self.run_summaries.summaries(store)
    self._startup_summaries = summaries
    for s in reversed(summaries):
        rid = s["run_id"]
        # Skip never-dispatched drafts: a run that opened an SSE stream but was
        # never /start-ed has a JSONL with no run.started — it's an empty stub,
        # not a conversation. Don't let those clutter the rail on restart.
        if not s.get("started"):
            m0 = re.match(r"run-(\d+)$", rid)
            if m0:
                max_seq = max(max_seq, int(m0.group(1)))
            continue
        run = self.create(rid, stream_seq=s["stream_seq"])
        run.execution_generation = int(
            s.get("execution_generation") or 0)
        run.terminal_generations = {
            int(generation)
            for generation in (s.get("terminal_generations") or [])
            if int(generation) > 0
        }
        # `summary()` falls back name→run_id; treat that as "no real title" so
        # the rail renders its placeholder instead of leaking the bare id.
        run.name = "" if s.get("name") in (None, "", rid) else s["name"]
        run.category = s.get("category", "") or ""
        run.started = bool(s.get("started"))
        # Historical ghost-run compatibility contract: a dead started run is
        # force-settled on rehydrate so the rail never shows a zombie live run.
        run.finished = bool(s.get("finished")) or run.started
        run.solved = bool(s.get("solved"))
        run.flag = s.get("flag")
        run.flags = list(s.get("flags") or (
            [run.flag] if run.flag is not None else []))
        run.expected_flags = int(s.get("expected_flags") or 1)
        run.multi_flag = bool(s.get("multi_flag", False))
        # a rehydrated run is never live → it can't be paused or mid-run.
        run.paused = False
        # order persisted runs by recency of activity (newest gets the highest
        # created_seq, so the rail's reverse sort puts it on top). created_seq
        # is assigned by create() in call order; mirror it into updated_seq so
        # the "recent" section's recency sort matches on startup.
        run.updated_seq = run.created_seq
        run.updated_at = float(s.get("ts", 0.0) or 0.0)
        # overlay operator metadata (pin/archive/rename) from the side table.
        self._apply_meta(run)
        m = re.match(r"run-(\d+)$", rid)
        if m:
            max_seq = max(max_seq, int(m.group(1)))
    # advance the id counter past every persisted run-NNNN so create_new()
    # never re-mints an id that already has history on disk.
    self._seq = max(self._seq, max_seq)
