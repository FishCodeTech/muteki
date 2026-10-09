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


_RUN_LAUNCH_LIMITS = (
    "wall_clock_budget", "max_total_workers", "cost_budget_usd",
    "token_budget", "tool_call_budget", "max_workers", "start_workers",
)


class WorkerRuntimePolicyUnavailable(RuntimeError):
    """A prior Run's Worker isolation cannot be proven for continuation."""

    code = "worker_runtime_policy_unavailable"
    phase = "worker_runtime_recovery"


def _shared_workspace_link(self, run_id: str) -> bool:
    workspace = self.storage.workspace(run_id)
    if not workspace.is_symlink():
        return False
    try:
        target = workspace.resolve(strict=True)
    except (OSError, RuntimeError) as exc:
        raise WorkerRuntimePolicyUnavailable(
            f"Run shared-container workspace is unavailable: {run_id}") from exc
    if target != self.storage.shared_workspace(run_id).resolve():
        raise WorkerRuntimePolicyUnavailable(
            f"Run workspace points outside its shared pool slot: {run_id}")
    return True


def _trusted_worker_runtime_history(self, run_id: str) -> dict[str, str]:
    """Read backend receipts from coordinator-owned records, never workspace files."""
    run = self.runs.get(run_id)
    store = run.store if run is not None else SessionStore(root=self.event_root)
    backends: set[str] = set()
    container_scopes: set[str] = set()
    try:
        for event in store.iter_matching_events(
            run_id, event_types=(EventType.RUN_FINISHED.value,),
        ):
            payload = event.payload or {}
            backend = payload.get("backend")
            if backend not in {"local", "container"}:
                continue  # Web-synthesized terminals have no Worker backend receipt.
            degraded = payload.get("runtime_degraded")
            if (backend == "local" and isinstance(degraded, list)
                    and any(isinstance(item, dict)
                            and item.get("requested_backend") == "container"
                            for item in degraded)):
                backend = "container"
            backends.add(backend)
            if backend == "container":
                scope = payload.get("container_scope")
                if scope in {"run", "shared"}:
                    container_scopes.add(scope)
    except OSError as exc:
        raise WorkerRuntimePolicyUnavailable(
            f"Run Worker runtime receipts are unreadable: {run_id}") from exc
    winner = self.load_winner_continuation(run_id)
    winner_backend = winner.get("backend")
    if winner_backend in {"local", "container"}:
        backends.add(winner_backend)
    if not backends:
        return {}
    is_shared = _shared_workspace_link(self, run_id)
    if "shared" in container_scopes and not is_shared:
        raise WorkerRuntimePolicyUnavailable(
            f"Run shared-container workspace is missing: {run_id}")
    if is_shared and "run" in container_scopes:
        raise WorkerRuntimePolicyUnavailable(
            f"Run Worker runtime receipts conflict with its workspace: {run_id}")
    # A Run may have several generations. Once one used a container, a later
    # follow-up must never execute against the same Worker data on the host.
    backend = "container" if "container" in backends or is_shared else "local"
    return {
        "worker_backend": backend,
        "worker_container_scope": "shared" if is_shared else "run",
    }


def persist_run_launch_limits(self, run_id: str, body: dict[str, Any]) -> None:
    """Keep launch bounds and the selected Worker isolation across generations."""
    is_shared = _shared_workspace_link(self, run_id)
    directory = self.coordinator_control_dir(run_id)
    path = directory / "run-launch-limits.json"
    temporary = directory / f".run-launch-limits-{os.getpid()}-{time.time_ns()}.tmp"
    values = {key: body[key] for key in _RUN_LAUNCH_LIMITS if key in body}
    challenge = body.get("challenge") or {}
    from apps.web.worker_config import resolve_worker_backend
    from muteki.core.runtime_env import is_web_container
    from apps.web.dispatch_parse import explicit_category
    category = explicit_category(challenge.get("category"))
    config = self.worker_config.resolve(category or None)
    contract = body.get("task_contract") or challenge.get("task_contract") or {}
    mode = (contract.get("mode") if isinstance(contract, dict) else None)
    mode = mode or challenge.get("mode") or body.get("mode") or "ctf"
    runtime_backend = resolve_worker_backend(
        request_backend=body.get("worker_backend"),
        config_backend=config.get("worker_backend"),
        env_backend=os.environ.get("MUTEKI_WORKER_BACKEND"),
        in_web_container=is_web_container(),
    )
    runtime_scope = str(body.get("worker_container_scope")
                        or config.get("worker_container_scope") or "run")
    if runtime_backend == "local":
        runtime_scope = "run"
    if runtime_scope not in {"run", "shared"}:
        raise RuntimeError("invalid Worker container scope")
    prior_payload = _load_run_launch_policy(self, run_id)
    prior_history = _trusted_worker_runtime_history(self, run_id)
    if prior_payload or prior_history or is_shared:
        previous = load_run_launch_limits(self, run_id)
        if (runtime_backend != previous["worker_backend"]
                or runtime_scope != previous["worker_container_scope"]):
            raise WorkerRuntimePolicyUnavailable(
                f"Run Worker isolation change is forbidden: {run_id}")
    pentest_contract = challenge.get("pentest_contract") or {}
    goal_mode = challenge.get("report_goal_mode", pentest_contract.get("report_goal_mode"))
    count = challenge.get("expected_findings", pentest_contract.get("expected_findings"))
    goal = ({"report_goal_mode": goal_mode, "expected_findings": count}
            if mode == "pentest" and goal_mode in {"automatic", "count"}
            else {})
    try:
        temporary.write_text(json.dumps({"version": 1, "limits": values,
                                         "worker_runtime": {
                                             "worker_backend": runtime_backend,
                                             "worker_container_scope": runtime_scope,
                                         },
                                         "pentest_goal": goal},
                                        ensure_ascii=False, sort_keys=True), encoding="utf-8")
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def load_run_launch_limits(self, run_id: str) -> dict[str, Any]:
    is_shared = _shared_workspace_link(self, run_id)
    payload = _load_run_launch_policy(self, run_id)
    values = payload.get("limits", {})
    if any(key not in _RUN_LAUNCH_LIMITS or not isinstance(value, (int, float))
           or isinstance(value, bool) for key, value in values.items()):
        raise RuntimeError(f"Run launch limits contain invalid values: {run_id}")
    result = dict(values)
    runtime = payload.get("worker_runtime")
    history = _trusted_worker_runtime_history(self, run_id)
    if runtime is not None:
        if (not isinstance(runtime, dict)
                or runtime.get("worker_backend") not in {"local", "container"}
                or runtime.get("worker_container_scope") not in {"run", "shared"}):
            raise WorkerRuntimePolicyUnavailable(
                f"Run Worker runtime policy is invalid: {run_id}")
        runtime = dict(runtime)
        if runtime["worker_backend"] == "local":
            # Older policy snapshots preserved an unused shared-scope setting
            # even though no shared workspace was mounted for a local Worker.
            runtime["worker_container_scope"] = "run"
        if (history.get("worker_backend") == "container"
                and runtime["worker_backend"] != "container"):
            raise WorkerRuntimePolicyUnavailable(
                f"Run Worker runtime policy conflicts with container history: {run_id}")
        if (is_shared and (runtime["worker_backend"] != "container"
                           or runtime["worker_container_scope"] != "shared")):
            raise WorkerRuntimePolicyUnavailable(
                f"Run Worker runtime policy conflicts with shared workspace: {run_id}")
        if (history.get("worker_backend") == "container"
                and runtime["worker_container_scope"]
                != history["worker_container_scope"]):
            raise WorkerRuntimePolicyUnavailable(
                f"Run Worker container scope conflicts with history: {run_id}")
        result.update(runtime)
    elif is_shared:
        # Older shared-container Runs predate the persisted runtime policy.
        # Their workspace link is the concrete isolation choice; never resume
        # one as a host-local Worker because the global default changed.
        result.update({"worker_backend": "container", "worker_container_scope": "shared"})
    elif history:
        result.update(history)
    else:
        raise WorkerRuntimePolicyUnavailable(
            f"Run Worker runtime policy has no trusted receipt: {run_id}")
    return result


def load_run_pentest_goal(self, run_id: str) -> dict[str, Any]:
    payload = _load_run_launch_policy(self, run_id)
    goal = payload.get("pentest_goal") or {}
    if not isinstance(goal, dict):
        raise RuntimeError(f"Run pentest goal is invalid: {run_id}")
    mode = goal.get("report_goal_mode")
    count = goal.get("expected_findings")
    if mode not in {None, "automatic", "count"} or (mode == "count" and
            (not isinstance(count, int) or isinstance(count, bool) or count <= 0)):
        raise RuntimeError(f"Run pentest goal is invalid: {run_id}")
    return dict(goal)


def _load_run_launch_policy(self, run_id: str) -> dict[str, Any]:
    path = self.coordinator_control_dir(run_id) / "run-launch-limits.json"
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {}
    if (not isinstance(payload, dict) or payload.get("version") != 1
            or not isinstance(payload.get("limits"), dict)):
        raise RuntimeError(f"Run launch limits are invalid: {run_id}")
    return payload


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
        run.mode = "pentest" if s.get("mode") == "pentest" else "ctf"
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
