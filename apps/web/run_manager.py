"""RunManager — the web/TUI-facing handle to live solve runs.

The frontends are dumb subscribers (§3): they never call the solver core
directly. They ask the RunManager to start a run, subscribe to that run's
EventBus, and submit durable control commands. A narrow queue adapter preserves
the existing coordinator inbox without treating queueing as proof of effect.

A "run" here is one challenge being solved (solo or by a swarm). Each gets its
own EventBus + SessionStore (durable replay) + an asyncio.Queue for inbound
human commands.
"""

from __future__ import annotations

import asyncio
import hashlib
import inspect
import json
import logging
import os
import re
import shutil
import stat
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from apps.web.control_adapter import (
    ControlPayloadError,
    QueueControlPort,
    compile_control_command,
    control_paths,
    effect_event_payload,
    safe_receipt_detail,
    safe_hitl_echo,
)
from apps.web.run_meta import FolderStore, RunMetaStore
from apps.web.worker_config import WorkerConfigStore
from muteki.control import (
    ApplyResult,
    ControlAction,
    ControlActor,
    ControlAdmission,
    ControlScope,
    DecisionKind,
    DecisionRequest,
    DecisionStatus,
    EffectState,
    InMemoryWorkerRegistry,
    RunControlMode,
    SQLiteControlJournal,
    StateConflict,
    WorkerRef,
)
from muteki.control.secrets import SecretStore
from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType, hitl_response_payload
from muteki.core.path_ids import encode_run_id
from muteki.core.session_store import SessionStore

LOG = logging.getLogger(__name__)


def _safe_exception_detail(prefix: str, exc: BaseException) -> str:
    """Keep the concrete boundary error available for operator diagnosis."""
    message = str(exc).replace("\x00", "").strip()
    message = "".join(ch for ch in message if ch in "\n\t" or ord(ch) >= 32)
    message = re.sub(
        r"(?i)\b(password|secret|token|api[_-]?key)\s*[:=]\s*"
        r"(?:\"[^\"]*\"|'[^']*'|\S+)",
        lambda match: f"{match.group(1)}=<redacted>",
        message,
    )
    suffix = f": {message[:2000]}" if message else ""
    return f"{prefix} ({type(exc).__name__}){suffix}"


def _runtime_error_id(run_id: str, generation: int, detail: str) -> str:
    material = f"{run_id}\x1f{generation}\x1f{detail}"
    digest = hashlib.sha256(material.encode("utf-8", "replace")).hexdigest()
    return f"RT-{digest[:10].upper()}"


@dataclass
class Run:
    run_id: str
    bus: EventBus
    cost: CostController
    store: SessionStore
    hitl: "asyncio.Queue[dict[str, Any]]" = field(default_factory=asyncio.Queue)
    # operator worker commands (spawn/kill a specific engine) the coordinator drains
    worker_cmds: "asyncio.Queue[dict[str, Any]]" = field(default_factory=asyncio.Queue)
    task: Optional[asyncio.Task] = None
    # post-solve standby: a short-lived worker spun up to serve a HITL command when
    # the main run is no longer live (finished, or the server restarted). Serialized
    # — one at a time per run.
    standby_task: Optional[asyncio.Task] = None
    # The asyncio task is only the Python wrapper.  The actual standby worker owns
    # a shelled CLI process tree, so STOP must cross that runtime boundary before
    # cancelling/awaiting the wrapper task.  The driver installs this callback as
    # soon as CliSolver exists and removes it only after its final cleanup.
    standby_cancel: Optional[Callable[[], Any]] = None
    standby_runtime_exited: Optional[Callable[[], bool]] = None
    standby_wait_runtime_exit: Optional[Callable[[Optional[float]], Awaitable[bool]]] = None
    # If the wrapper exits before its runner thread/process, the driver keeps an
    # autonomous kill/reap watcher here.  This prevents a PARTIAL receipt from
    # orphaning the runtime merely because later STOP admission is unavailable.
    standby_runtime_cleanup_task: Optional[asyncio.Task] = None
    # Pre-start standby context release is a separate durable owner. A journal
    # failure must not strand a one-shot reservation after the wrapper exits.
    standby_context_cleanup_task: Optional[asyncio.Task] = None
    standby_context_cleanup_owner: str = ""
    standby_context_cleanup_reservations: list[tuple[str, str]] = field(
        default_factory=list)
    # Container/runtime acquisition can itself be a non-cancellable to_thread call.
    # Track it independently so cancel/delete/resolve never mistake "callbacks not
    # registered yet" for proof that no runtime resource exists.
    standby_setup_task: Optional[asyncio.Task] = None
    # A control callback that ignored shutdown cancellation still owns an in-flight
    # mutation even after the main wrapper task returned. Keep that owner and its
    # autonomous settle task first-class so resolve/delete/shutdown cannot tear down
    # or replace the runtime underneath it.
    runtime_incomplete: bool = False
    runtime_owner: Optional[Any] = None
    runtime_cleanup_task: Optional[asyncio.Task] = None
    runtime_settle: Optional[Callable[[], Awaitable[None]]] = None
    runtime_error: str = ""
    finished: bool = False
    flag: Optional[str] = None
    # multi-flag: every distinct flag the run collected (dedup, discovery order).
    # `flag` stays the first for back-compat. expected_flags drives the rail/UI
    # "collected N/total" + the solved-vs-collecting distinction.
    flags: list[str] = field(default_factory=list)
    # flags the operator explicitly marked false. A reopened run replays the
    # shared graph, so old flag_found events can appear again; never let those
    # values re-enter the rail summary once invalidated.
    invalidated_flags: set[str] = field(default_factory=set)
    expected_flags: int = 1
    # multi-flag MODE bit (collect vs single). Relayed on the synthetic RUN_FINISHED
    # so a reconnecting deck knows a collect run shouldn't read "solved" on flag #1.
    multi_flag: bool = False
    # ---- lightweight metadata for the thread rail (conversation-first deck) ----
    # The deck lists runs in a ChatGPT-style sidebar; it needs a name/category/
    # outcome per run without replaying the whole event stream. We sniff these off
    # the bus as a sink (the run stays a dumb event source — no extra contract).
    name: str = ""
    category: str = ""
    started: bool = False
    solved: bool = False
    paused: bool = False
    # a worker raised its hand (HITL_REQUEST: NEED_INPUT / target crashed / instance
    # expired / missing credential). True until the operator answers (HITL_RESPONSE)
    # or the run finishes. Surfaced on the summary so a poll of /api/runs catches it
    # — independent of `paused` (the swarm may keep running with one hand up).
    awaiting_help: bool = False
    help_text: str = ""
    pending_help: dict[str, str] = field(default_factory=dict)
    created_seq: int = 0
    updated_seq: int = 0  # bumped on every event — exposed as activity metadata
    updated_at: float = 0.0  # epoch seconds of the latest event, for rail "x ago"
    # operator-set rail metadata (persisted in RunMetaStore, injected by manager)
    pinned: bool = False
    pinned_at: Optional[float] = None
    archived: bool = False
    custom_name: Optional[str] = None
    # rail folder (None = top-level) + operator drag-order within its section
    folder_id: Optional[str] = None
    sort_order: Optional[int] = None
    # M2: signature of the last HITL command (target, action, text, url) — an
    # identical back-to-back resend is dropped instead of re-queued/re-emitted.
    _last_hitl_sig: Optional[tuple] = None
    # Lazily-created per-run control plane. Its journal and SecretStore live under
    # RunManager.control_root, which is coordinator-private and deliberately outside
    # every bind-mounted worker workspace. The actor is the sole async writer/router.
    control_actor: Optional[ControlActor] = None
    control_journal: Optional[SQLiteControlJournal] = None
    control_secrets: Optional[SecretStore] = None
    worker_registry: InMemoryWorkerRegistry = field(
        default_factory=InMemoryWorkerRegistry)
    # Monotonic in-memory ownership token for the main execution wrapper. A stale
    # generation may finish late, but it may never synthesize terminal state or
    # close the bus owned by a newer generation.
    execution_generation: int = 0
    control_generation: int = 0
    # Lifecycle admission state. Old-generation events are dropped before they
    # reach the durable log, and each generation may publish RUN_FINISHED once.
    terminal_generations: set[int] = field(default_factory=set)
    # Ask/Writeup run after RUN_FINISHED but still publish durable, typed output.
    # IDs stay active until their terminal follow-up event has been emitted.
    active_followups: set[str] = field(default_factory=set)
    termination_reasons: dict[int, str] = field(default_factory=dict)
    # One task owns one readiness result for each exact participating profile
    # configuration.  A continuation generation reuses the result; changing the
    # profile/model/account/runtime produces a different key and therefore a new
    # real probe.  This cache deliberately lives on Run rather than Swarm because
    # every continuation constructs a fresh Swarm instance.
    profile_readiness: dict[str, tuple[bool, Optional[dict[str, Any]]]] = field(
        default_factory=dict)
    # The title request belongs to the generation that started it and is cancelled
    # before a replacement generation or shutdown.
    title_task: Optional[asyncio.Task] = None
    # Protocol is fixed at fresh-start admission. Protocol 2 intentionally disables
    # legacy standby/resolve/control/delete paths until their canonical adapters
    # exist; callers can inspect this field before any side effect.
    protocol_version: int = 1
    protocol_ownership_confirmed: bool = True

    def merge_flags(self, flags: Any) -> None:
        """Accumulate flags from an event payload (dedup, keep order); keep the
        flag/flags[0] invariant. Accepts a list or a single string."""
        if isinstance(flags, str):
            flags = [flags]
        for f in (flags or []):
            if f in self.invalidated_flags:
                continue
            if f and f not in self.flags:
                self.flags.append(f)
        if self.flags and not self.flag:
            self.flag = self.flags[0]

    def valid_incoming_flags(self, flags: Any) -> list[str]:
        if isinstance(flags, str):
            flags = [flags]
        return [
            f for f in (flags or [])
            if f and f not in self.invalidated_flags
        ]

    def invalidate_flag(self, flag: Any = None) -> None:
        """Drop a false-positive flag and remember it across graph replay."""
        bad = str(flag or "").strip()
        if bad:
            self.invalidated_flags.add(bad)
            self.flags = [f for f in self.flags if f != bad]
        else:
            self.invalidated_flags.update(self.flags)
            self.flags = []
        self.flag = self.flags[0] if self.flags else None
        if not self.flags:
            self.solved = False

    def status(self) -> str:
        """Single derived lifecycle status the rail renders an icon for.

        draft → never started. running → started, not finished, not paused.
        paused → operator paused a live run. solved/finished/failed are terminal.
        """
        if not self.started:
            return "draft"
        if not self.finished:
            return "paused" if self.paused else "running"
        if self.solved:
            return "solved"
        return "finished"  # ended, no flag (we don't distinguish "failed" yet)

    def summary(self) -> dict[str, Any]:
        """The shape the deck's thread rail consumes (one row per run)."""
        return {
            "run_id": self.run_id,
            "protocol_version": self.protocol_version,
            # custom_name (operator rename) wins; else the auto/challenge name.
            # Empty when neither is set — the rail renders its own placeholder, we
            # do NOT leak the bare run id as a display name.
            "name": self.custom_name or self.name,
            "category": self.category or "",
            "started": self.started,
            "finished": self.finished,
            "solved": self.solved,
            "paused": self.paused,
            "awaiting_help": self.awaiting_help,
            "help_text": self.help_text,
            "runtime_incomplete": self.runtime_incomplete,
            "runtime_error": self.runtime_error,
            "status": self.status(),
            "flag": self.flag,
            "flags": list(self.flags),
            "expected_flags": self.expected_flags,
            "multi_flag": self.multi_flag,
            "pinned": self.pinned,
            "pinned_at": self.pinned_at,
            "archived": self.archived,
            "folder_id": self.folder_id,
            # operator drag-order if set, else creation order (rail sorts by this)
            "order": self.sort_order if self.sort_order is not None else self.created_seq,
            "updated": self.updated_seq,
            "updated_at": self.updated_at,
        }


# A driver is any coroutine fn(run) that emits onto run.bus and returns.
Driver = Callable[[Run], Awaitable[None]]


def _apply_blackboard_meta(run: "Run", ev: Event) -> None:
    """Reflect coordinator BLACKBOARD_DELTA lifecycle into the rail/summary state so
    the deck shows mid-run progress, not just the terminal RUN_FINISHED. Two things
    the operator complained were invisible (run-11189):
      • flag_found — a flag landed mid-run (collect mode keeps going); merge it into
        run.flags NOW so the N/total counter ticks up instead of staying 0 until the
        run ends.
      • awaiting_operator / collect_idle — the swarm auto-paused waiting for the
        operator (NEED_INPUT). Flip run.paused so the rail shows "paused", not a
        spinner that looks like it's still churning. operator_resumed / a STOP clears
        it (RUN_FINISHED already clears paused on its own)."""
    if ev.event_type is not EventType.BLACKBOARD_DELTA:
        return
    kind = (ev.payload or {}).get("kind")
    if kind == "flag_found":
        run.merge_flags((ev.payload or {}).get("flag"))
    elif kind == "flag_invalidated":
        run.invalidate_flag((ev.payload or {}).get("flag"))
    elif kind in ("awaiting_operator", "collect_idle"):
        run.paused = True
    elif kind in ("operator_resumed", "operator_stopped"):
        run.paused = False


def _apply_operator_meta(run: "Run", ev: Event) -> bool:
    """Fold HITL/control events into rail metadata without guessing effects.

    Returns True when the event was fully handled. A submitted command is merely
    an echo; only a ``control.command/effect_observed`` event can change pause.
    """
    payload = ev.payload or {}
    if ev.event_type is EventType.HITL_REQUEST:
        need = str(payload.get("need") or payload.get("text") or "")[:300]
        request_id = str(payload.get("request_id") or payload.get("id") or
                         f"legacy:{payload.get('worker', '')}:{need}")
        run.pending_help[request_id] = need
        run.awaiting_help = True
        run.help_text = next(iter(run.pending_help.values()), "")
        return True
    if ev.event_type is EventType.HITL_RESPONSE:
        # This is only the immutable operator echo (normally PERSISTED). The
        # durable DecisionAnswer companion closes the card via CONTROL_COMMAND.
        return True
    if ev.event_type is EventType.CONTROL_COMMAND:
        if (bool(payload.get("decision_closed"))
                and payload.get("status") == "effect_observed"):
            request_id = str(payload.get("request_id") or "").strip()
            if request_id:
                run.pending_help.pop(request_id, None)
            run.awaiting_help = bool(run.pending_help)
            run.help_text = next(iter(run.pending_help.values()), "")
        if payload.get("status") != "effect_observed":
            return True
        effect = payload.get("effect") if isinstance(payload.get("effect"), dict) else {}
        effect_kind = str(effect.get("kind") or payload.get("effect_kind") or "").lower()
        if effect_kind in {"run_quiesced", "run_frozen"}:
            run.paused = True
        elif effect_kind in {"run_resumed", "run_thawed"}:
            run.paused = bool(run.pending_help)
        return True
    return False


class RunManager:
    def __init__(self, *, sessions_root: "str | Path | None" = None,
                 control_root: "str | Path | None" = None) -> None:
        # P2-v3: in the compose layout the sessions/ tree must live UNDER the
        # mirrored data root (MUTEKI_HOST_DATA_ROOT bind-mounted into the web
        # container), so worker sibling containers — launched by the host daemon —
        # can bind-mount the same physical dir. MUTEKI_SESSIONS_ROOT names it
        # (compose points it at <data root>/sessions). Default "sessions" (CWD-
        # relative) preserves the bare-host behaviour.
        if sessions_root is None:
            sessions_root = os.environ.get("MUTEKI_SESSIONS_ROOT") or "sessions"
        self.sessions_root = Path(sessions_root)
        self.sessions_root.mkdir(parents=True, exist_ok=True)
        # Operator control state is more privileged than worker evidence. Never put
        # control.db or materialised secret files below sessions/{run}/workspace:
        # that whole workspace is recursively chowned to the worker uid and mounted
        # read-write into its container. The default sibling root is visible only to
        # the coordinator process. Deployments may place it on a dedicated volume.
        if control_root is None:
            control_root = (
                os.environ.get("MUTEKI_COORDINATOR_CONTROL_ROOT")
                or self.sessions_root / ".coordinator-control"
            )
        self.control_root = Path(control_root)
        try:
            rel_to_sessions = self.control_root.resolve().relative_to(
                self.sessions_root.resolve())
        except ValueError:
            rel_to_sessions = None
        if (rel_to_sessions is not None and len(rel_to_sessions.parts) >= 2
                and rel_to_sessions.parts[1] == "workspace"):
            raise ValueError(
                "coordinator control root cannot be inside a worker workspace")
        self.control_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        root_info = self.control_root.lstat()
        if not stat.S_ISDIR(root_info.st_mode) or stat.S_ISLNK(root_info.st_mode):
            raise ValueError("coordinator control root must be a real directory")
        os.chmod(self.control_root, 0o700)
        # Protocol 2 has an independent host-only catalog/composition root.  It is
        # wired into the real Web process now, but production dispatch stays
        # fail-closed until the canary/black-box receipts enable it explicitly.
        self.protocol2 = None
        self.protocol2_error = ""
        try:
            from apps.web.protocol2_adapter import Protocol2WebAdapter
            self.protocol2 = Protocol2WebAdapter(control_root=self.control_root)
        except Exception as exc:
            self.protocol2_error = type(exc).__name__
        self.runs: dict[str, Run] = {}
        # Main-task, standby, resolve, delete, and shutdown admission share one
        # lifecycle boundary. Long cleanup waits publish an explicit per-run fence
        # rather than holding this lock, so conflicting operations fail closed
        # instead of deadlocking behind an adversarial cancellation handler.
        self._lifecycle_lock = asyncio.Lock()
        self._closing_runs: set[str] = set()
        self._launching_runs: set[str] = set()
        self._shutting_down = False
        # Admission is single-writer all the way through secret extraction.  The
        # actor serialises journal mutation, but compilation happens before the
        # actor and may create opaque SecretStore refs.  Without this boundary,
        # two concurrent retries carrying the same command_id can each mint a
        # different ref before either command is visible in SQLite.
        self._control_submit_locks: dict[str, asyncio.Lock] = {}
        self._seq = 0
        self.meta = RunMetaStore(root=self.sessions_root)
        # operator-created rail folders (id → name); runs reference one via meta.
        self.folders = FolderStore(root=self.sessions_root)
        # default worker-roster config (which engines launch per challenge); the
        # dispatch path falls back to this when a request doesn't say otherwise.
        self.worker_config = WorkerConfigStore(root=self.sessions_root)
        protocol2_run_ids = self._reconcile_protocol2_flags()
        self._recover_interrupted_followups(
            SessionStore(root=self.sessions_root))
        self._rehydrate(protocol2_run_ids=protocol2_run_ids)

    def _execution_owned(
        self, run: Run, generation: int,
        task: "Optional[asyncio.Task[Any]]" = None,
    ) -> bool:
        return bool(
            self.runs.get(run.run_id) is run
            and run.execution_generation == generation
            and (task is None or run.task is task)
        )

    def _generation_filter_for(self, run: Run):
        async def _generation_filter(ev: Event) -> bool:
            payload = ev.payload
            supplied = payload.get("execution_generation")
            if supplied is None:
                generation = run.execution_generation
                payload["execution_generation"] = generation
            else:
                try:
                    generation = int(supplied)
                except (TypeError, ValueError):
                    return False
            if generation < run.execution_generation:
                return False
            payload.setdefault("control_generation", run.control_generation)
            followup_types = {
                EventType.FOLLOWUP_STARTED,
                EventType.FOLLOWUP_COMPLETED,
                EventType.FOLLOWUP_FAILED,
            }
            # These event types are exclusively produced by the post-run driver.
            # Their own lifecycle ID provides UI correlation; admission must not
            # depend on a transient in-memory set because a very short worker can
            # complete while its control receipt is still settling.
            allowed_followup = ev.event_type in followup_types
            if (generation in run.terminal_generations
                    and ev.event_type is not EventType.CONTROL_COMMAND
                    and not allowed_followup):
                # The terminal event closes this execution generation.  A worker
                # subprocess may still flush a buffered frame while cancellation is
                # propagating, but that frame belongs to a closed runtime and must not
                # mutate the durable/UI projection.  The terminal control receipt is
                # still admitted so operators can audit the completed STOP/COMPLETE.
                return False
            if ev.event_type is EventType.RUN_FINISHED:
                run.terminal_generations.add(generation)
            return True
        return _generation_filter

    @staticmethod
    def _safe_run_id(run_id: str) -> str:
        return encode_run_id(run_id)

    @staticmethod
    def _is_within(path: Path, root: Path) -> bool:
        try:
            path.relative_to(root)
            return True
        except ValueError:
            return False

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
        workdir_rel = ""
        raw_workdir = str(payload.get("workdir") or "").strip()
        if raw_workdir:
            try:
                resolved_workdir = Path(raw_workdir).resolve()
                if self._is_within(resolved_workdir, worker_root):
                    workdir_rel = str(resolved_workdir.relative_to(workspace))
            except (OSError, ValueError):
                workdir_rel = ""

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
            str(value).strip() for value in list(payload.get("flags") or [])
            if str(value).strip()
        ]
        first_flag = str(payload.get("flag") or "").strip()
        if first_flag and first_flag not in flags:
            flags.insert(0, first_flag)
        challenge = payload.get("challenge")
        stored = {
            "version": 1,
            "worker_id": str(payload.get("worker_id") or "").strip(),
            "profile_id": profile_id,
            "engine": str(payload.get("engine") or "").strip(),
            "session": str(payload.get("session") or "").strip(),
            "workdir_rel": workdir_rel,
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
            str(value).strip() for value in flags if str(value).strip()
        ))
        continuation["flag"] = (
            continuation["flags"][0] if continuation["flags"] else ""
        )
        rel = str(continuation.pop("workdir_rel", "") or "")
        continuation["workdir"] = (
            str((self.workspace_dir(run_id) / rel).resolve()) if rel else ""
        )
        self.persist_winner_continuation(run_id, continuation)

    def _apply_meta(self, run: "Run") -> None:
        """Overlay persisted operator metadata (pin/archive/rename) onto a run."""
        m = self.meta.get(run.run_id)
        run.pinned = m["pinned"]
        run.pinned_at = m["pinned_at"]
        run.archived = m["archived"]
        run.custom_name = m["custom_name"]
        run.folder_id = m["folder_id"]
        run.sort_order = m["order"]

    def _reconcile_protocol2_flags(self) -> Optional[frozenset[str]]:
        """Project accepted handoffs and return one authoritative owner snapshot.

        ``None`` means the catalog inventory could not be established. Callers must
        not reinterpret that uncertainty as proof that a persisted run is Protocol 1.
        """

        if self.protocol2 is None:
            return None
        store = SessionStore(root=self.sessions_root)
        try:
            catalog_run_ids = set(self.protocol2.list_run_ids())
        except Exception as exc:
            # No run id can be attributed safely when the canonical inventory itself
            # is unavailable, so retain a process diagnostic without persisting any
            # exception-controlled material.
            LOG.error(
                "Protocol 2 projection inventory is incomplete (%s)",
                type(exc).__name__,
            )
            return None
        # The union prevents display-only/session history from disappearing from the
        # startup scan, while catalog membership remains the sole Protocol 2 owner bit.
        for run_id in sorted(catalog_run_ids | set(store.list_runs())):
            if run_id not in catalog_run_ids:
                continue
            try:
                publications = self.protocol2.recover_flag_publications(run_id)
                for publication in publications:
                    event = Event(
                        event_type=EventType.FLAG_ACCEPTED,
                        seq=store.last_stream_seq(run_id) + 1,
                        run_id=run_id,
                        solver_id="protocol2-authority-projector",
                        payload={
                            "schema_id": "muteki.flag-accepted-projection.v1",
                            "publication_id": publication.publication_id,
                            "evaluation_id": publication.evaluation_id,
                            "flag": publication.flag,
                            "flag_digest": publication.flag_digest,
                            "gate_receipt_digest": publication.gate_receipt_digest,
                        },
                    )
                    store.append_if_absent_sync(
                        event,
                        identity_field="publication_id",
                        identity=publication.publication_id,
                    )
            except Exception as exc:
                # One corrupt canonical run must not suppress recovery for the rest.
                # This typed event is deliberately non-terminal and carries only a
                # local diagnostic identity plus a bounded exception class name.
                diagnostic_id = "protocol2-projection-incomplete:" + encode_run_id(run_id)
                diagnostic = Event(
                    event_type=EventType.PROJECTION_INCOMPLETE,
                    seq=store.last_stream_seq(run_id) + 1,
                    run_id=run_id,
                    solver_id="protocol2-authority-projector",
                    payload={
                        "schema_id": "muteki.projection-incomplete.v1",
                        "diagnostic_id": diagnostic_id,
                        "projection": "flag.accepted",
                        "error_class": type(exc).__name__[:100],
                    },
                )
                try:
                    store.append_if_absent_sync(
                        diagnostic,
                        identity_field="diagnostic_id",
                        identity=diagnostic_id,
                    )
                except Exception:
                    LOG.error(
                        "Protocol 2 projection diagnostic could not persist for %s",
                        run_id,
                    )
        return frozenset(catalog_run_ids)

    def _recover_interrupted_followups(self, store: SessionStore) -> None:
        """Persist a terminal event for follow-ups abandoned by a process exit.

        Ask and writeup executions belong to the web process that started them.
        During manager construction there cannot be a surviving standby task from
        the previous process, so every durable ``followup.started`` without a
        matching terminal event is interrupted.  Recording the recovery in the
        same JSONL log keeps replay deterministic and unlocks every fresh client,
        rather than synthesizing a different result per SSE connection.
        """
        started_type = EventType.FOLLOWUP_STARTED.value
        terminal_types = {
            EventType.FOLLOWUP_COMPLETED.value,
            EventType.FOLLOWUP_FAILED.value,
        }
        for run_id in store.list_runs():
            pending: dict[str, dict[str, Any]] = {}
            try:
                for index, row in enumerate(store.load_all(run_id)):
                    event_type = str(row.get("event_type") or "")
                    payload = row.get("payload")
                    payload = payload if isinstance(payload, dict) else {}
                    followup_id = str(payload.get("followup_id") or "")
                    if event_type == started_type:
                        key = followup_id or f"legacy:{index}"
                        pending[key] = {
                            "followup_id": followup_id,
                            "kind": str(payload.get("kind") or "ask"),
                            "execution_generation": payload.get(
                                "execution_generation"),
                            "recovery_id": (
                                f"interrupted-followup:{index}:{followup_id}"
                            ),
                        }
                    elif event_type in terminal_types:
                        if followup_id:
                            pending.pop(followup_id, None)
                        else:
                            legacy_key = next(
                                (key for key in reversed(pending)
                                 if not pending[key]["followup_id"]),
                                None,
                            )
                            if legacy_key is not None:
                                pending.pop(legacy_key, None)

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
                        seq=store.last_stream_seq(run_id) + 1,
                        payload=payload,
                    )
                    store.append_if_absent_sync(
                        recovery,
                        identity_field="recovery_id",
                        identity=interrupted["recovery_id"],
                    )
            except Exception as exc:
                LOG.error(
                    "Interrupted follow-up recovery failed for %s error_type=%s",
                    run_id, type(exc).__name__,
                )

    def _rehydrate(
        self, *, protocol2_run_ids: Optional[frozenset[str]] = None
    ) -> None:
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
        store = SessionStore(root=self.sessions_root)
        max_seq = 0
        # summaries() is newest-first; create() stamps created_seq in CALL order,
        # and the rail sorts by created_seq DESC — so feed oldest-first to keep the
        # newest conversation on top of the rail.
        for s in reversed(store.summaries()):
            rid = s["run_id"]
            # Skip never-dispatched drafts: a run that opened an SSE stream but was
            # never /start-ed has a JSONL with no run.started — it's an empty stub,
            # not a conversation. Don't let those clutter the rail on restart.
            if not s.get("started"):
                m0 = re.match(r"run-(\d+)$", rid)
                if m0:
                    max_seq = max(max_seq, int(m0.group(1)))
                continue
            run = self.create(rid)
            run.execution_generation = int(
                s.get("execution_generation") or 0)
            run.terminal_generations = {
                int(generation)
                for generation in (s.get("terminal_generations") or [])
                if int(generation) > 0
            }
            # Reuse the inventory already verified by startup reconciliation. If that
            # inventory failed, ownership is unknown rather than Protocol 1.
            protocol2_owned: Optional[bool] = (
                rid in protocol2_run_ids
                if protocol2_run_ids is not None
                else self.protocol2_ownership(rid, run=run)
            )
            if protocol2_owned is True:
                run.protocol_version = 2
            elif protocol2_owned is None:
                run.protocol_ownership_confirmed = False
            # `summary()` falls back name→run_id; treat that as "no real title" so
            # the rail renders its placeholder instead of leaking the bare id.
            run.name = "" if s.get("name") in (None, "", rid) else s["name"]
            run.category = s.get("category", "") or ""
            run.started = bool(s.get("started"))
            # Protocol 1 keeps the historical ghost-run compatibility contract: a
            # dead started run is force-settled. Protocol 2 lifecycle is canonical
            # elsewhere, so missing public closure is unresolved, not permission to
            # fabricate terminal state. Accepted-only history likewise stays open.
            protocol1_owned = protocol2_owned is False
            run.finished = (
                bool(s.get("finished")) or run.started
                if protocol1_owned
                else bool(s.get("finished"))
            )
            run.solved = (
                bool(s.get("solved"))
                if protocol1_owned
                else bool(s.get("solved")) and run.finished
            )
            run.flag = s.get("flag")
            run.flags = list(s.get("flags") or ([run.flag] if run.flag else []))
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

    def get(self, run_id: str) -> Optional[Run]:
        return self.runs.get(run_id)

    def protocol2_ownership(
        self, run_id: str, *, run: Optional[Run] = None
    ) -> Optional[bool]:
        """Return catalog ownership as ``True`` / ``False`` / unknown ``None``.

        An in-memory Protocol 2 bit is a positive durable hint. Absence is not a
        Protocol 1 proof: if the adapter or catalog lookup is unavailable, legacy
        synthesis and mutation must remain disabled rather than claiming ownership.
        """
        candidate = run if run is not None else self.runs.get(run_id)
        if candidate is not None and candidate.protocol_version == 2:
            return True
        if (candidate is not None
                and candidate.protocol_version == 1
                and candidate.protocol_ownership_confirmed):
            return False
        adapter = self.protocol2
        if adapter is None:
            return None
        try:
            return bool(adapter.has_run(run_id))
        except Exception as exc:
            LOG.error(
                "Protocol 2 ownership lookup is incomplete for %s (%s)",
                run_id,
                type(exc).__name__,
            )
            return None

    def is_protocol2_run(self, run_id: str, *, run: Optional[Run] = None) -> bool:
        """Compatibility facade: true only for positively owned Protocol 2 runs."""
        return self.protocol2_ownership(run_id, run=run) is True

    def is_protocol1_run(self, run_id: str, *, run: Optional[Run] = None) -> bool:
        """Return true only when the catalog positively excludes Protocol 2."""
        return self.protocol2_ownership(run_id, run=run) is False

    def list_runs(self, *, include_archived: bool = False) -> list[dict[str, Any]]:
        """Run summaries for the thread rail, newest first.

        Only STARTED runs are real conversations. A run handle also gets created
        lazily when a deck merely OPENS an SSE stream (so the stream is live the
        instant a run starts) — including for local draft ids that are never
        dispatched. Those empty stubs must not appear in the rail; the active
        draft is shown from the deck's own local state, not this list.

        Archived runs are hidden by default (the rail's "+ archived" view passes
        include_archived=True). Ordering: a RUNNING run always floats to the top
        (so the题 currently being solved is the first thing the operator sees —
        previously we sorted purely by created_seq, which buried a live run under
        already-finished ones when the manager rehydrated from disk in a different
        order than the eval ran). Within the running / non-running groups we sort
        by latest activity (updated_at), newest first, then created_seq as a tiebreak.
        """
        def _key(r: "Run"):
            running = r.status() == "running"
            return (1 if running else 0, r.updated_at or 0.0, r.created_seq)
        return [
            r.summary()
            for r in sorted(self.runs.values(), key=_key, reverse=True)
            if r.started and (include_archived or not r.archived)
        ]

    # ---- operator rail mutations (persisted in the meta side-table) ----------

    def set_pinned(self, run_id: str, pinned: bool, *, now: float) -> bool:
        run = self.runs.get(run_id)
        if run is None:
            return False
        m = self.meta.set_pinned(run_id, pinned, now=now)
        run.pinned, run.pinned_at = m["pinned"], m["pinned_at"]
        return True

    def set_archived(self, run_id: str, archived: bool, *,
                     now: Optional[float] = None) -> bool:
        run = self.runs.get(run_id)
        if run is None:
            return False
        if not self.is_protocol1_run(run_id, run=run):
            # V2 or unknown ownership is never permission for legacy rail archive.
            return False
        m = self.meta.set_archived(run_id, archived,
                                   now=now if now is not None else time.time())
        run.archived, run.pinned, run.pinned_at = m["archived"], m["pinned"], m["pinned_at"]
        return True

    def rename(self, run_id: str, name: Optional[str]) -> bool:
        run = self.runs.get(run_id)
        if run is None:
            return False
        run.custom_name = self.meta.set_name(run_id, name)["custom_name"]
        return True

    def set_folder(self, run_id: str, folder_id: Optional[str]) -> bool:
        run = self.runs.get(run_id)
        if run is None:
            return False
        run.folder_id = self.meta.set_folder(run_id, folder_id)["folder_id"]
        return True

    def set_order(self, run_id: str, order: Optional[int]) -> bool:
        run = self.runs.get(run_id)
        if run is None:
            return False
        run.sort_order = self.meta.set_order(run_id, order)["order"]
        return True

    # ---- rail folders (operator-created groupings) ---------------------------

    def list_folders(self) -> list[dict[str, Any]]:
        return self.folders.list()

    def create_folder(self, name: str) -> dict[str, Any]:
        return self.folders.create(name)

    def update_folder(self, fid: str, *, name: Optional[str] = None,
                      order: Optional[int] = None) -> bool:
        return self.folders.update(fid, name=name, order=order)

    def delete_folder(self, fid: str) -> bool:
        # unfile every run that was in this folder, then drop the folder itself.
        self.meta.clear_folder_for_all(fid)
        for run in self.runs.values():
            if run.folder_id == fid:
                run.folder_id = None
        return self.folders.delete(fid)

    async def delete(self, run_id: str) -> bool:
        """Hard-delete under a published per-run lifecycle fence."""
        async with self._lifecycle_lock:
            if run_id in self._closing_runs or run_id in self._launching_runs:
                return False
            run = self.runs.get(run_id)
            if not self.is_protocol1_run(run_id, run=run):
                return False
            # No authority means no mutation. In particular, an unknown id must
            # never become an orphan-artifact scrub that bypasses the V2 catalog.
            if run is None:
                return False
            self._closing_runs.add(run_id)
        try:
            return await self._delete_owned_run(run_id, run)
        finally:
            async with self._lifecycle_lock:
                self._closing_runs.discard(run_id)

    async def _delete_owned_run(self, run_id: str, run: Run) -> bool:
        """Settle and remove the exact Run captured by :meth:`delete`."""
        if run.runtime_incomplete and not await self._settle_incomplete_runtime(
                run, timeout=self._standby_cancel_timeout()):
            LOG.error(
                "refusing to delete %s: main runtime owner is still unsettled",
                run_id)
            return False
        # Cancel BOTH the swarm task and any live standby worker, then AWAIT them to
        # actually unwind before we close the bus / delete artifacts. Cancelling
        # without awaiting was a use-after-free race: the cancelled coroutine could
        # still be writing to the bus or reading an upload while we closed/removed
        # them. A cancelled task re-raises CancelledError on await — return_exceptions
        # swallows it (and any other shutdown error) so delete never self-destructs.
        if not await self._settle_standby_runtime(
                run, timeout=self._standby_cancel_timeout()):
            # Keep the run, callbacks, cleanup watcher and artifacts intact. A
            # caller can retry STOP/FORCE_CANCEL/delete; dropping ownership here
            # would turn a PARTIAL kill into an orphan.
            LOG.error(
                "refusing to delete %s: standby runtime exit is unconfirmed", run_id)
            return False
        pending = [t for t in (run.task, run.standby_task, run.title_task)
                   if t is not None and not t.done()]
        for t in pending:
            t.cancel()
        if pending:
            done, still_live = await asyncio.wait(
                tuple(pending), timeout=self._standby_cancel_timeout())
            if done:
                await asyncio.gather(*done, return_exceptions=True)
            if still_live:
                # A wrapper that suppresses CancelledError still owns the Run and
                # its files. Publish the same retained-owner fence used by driver
                # runtime cleanup, then return boundedly; start/resolve/control will
                # fail closed until the autonomous waiter proves task exit.
                owned = tuple(still_live)
                owner_token = object()
                run.runtime_incomplete = True
                run.runtime_owner = owner_token

                async def _settle_wrapper_owner() -> None:
                    await asyncio.gather(
                        *(asyncio.shield(task) for task in owned),
                        return_exceptions=True,
                    )
                    # The late wrapper unwind may discover and publish a stronger
                    # subprocess/container owner. Clear only our exact token/task;
                    # never erase ownership transferred after the delete timeout.
                    current = asyncio.current_task()
                    if (all(task.done() for task in owned)
                            and run.runtime_owner is owner_token
                            and run.runtime_cleanup_task is current):
                        run.runtime_incomplete = False
                        run.runtime_owner = None
                        run.runtime_error = ""
                        run.runtime_settle = None

                run.runtime_settle = _settle_wrapper_owner
                run.runtime_error = "task cancellation exit unconfirmed"
                run.runtime_cleanup_task = asyncio.create_task(
                    _settle_wrapper_owner(),
                    name=f"delete-runtime-owner-settle-{run_id}",
                )
                LOG.error(
                    "refusing to delete %s: task cancellation exit is unconfirmed",
                    run_id,
                )
                return False
        # Cancelling the asyncio wrapper can be the operation that discovers an
        # independently-live worker runtime.  The driver then transfers ownership
        # to ``runtime_cleanup_task`` and marks ``runtime_incomplete`` while the
        # gather above unwinds.  Re-check after cancellation before dropping the
        # Run, journal, callbacks, or artifacts.
        if run.runtime_incomplete and not await self._settle_incomplete_runtime(
                run, timeout=self._standby_cancel_timeout()):
            LOG.error(
                "refusing to delete %s: cancellation exposed an unsettled "
                "main runtime owner", run_id)
            return False
        # Remove the exact handle only after every execution boundary is settled.
        # start/resolve/standby all reject the published closing fence, but retain
        # the identity check as the final destructive commit guard.
        async with self._lifecycle_lock:
            if self.runs.get(run_id) is not run:
                return False
            self.runs.pop(run_id, None)
        if run.control_actor is not None:
            try:
                await run.control_actor.close()
            except Exception:
                LOG.exception("failed to close control actor for %s", run_id)
        if run.control_journal is not None:
            try:
                run.control_journal.close()
            except Exception:
                LOG.exception("failed to close control journal for %s", run_id)
        await run.bus.close()
        self._delete_artifacts(run_id)
        return True

    def _delete_artifacts(self, run_id: str) -> None:
        self.meta.forget(run_id)
        safe = self._safe_run_id(run_id)
        jsonl = self.sessions_root / f"{safe}.jsonl"
        try:
            jsonl.unlink(missing_ok=True)
        except OSError:
            pass
        # also drop the per-run upload dir (sessions/{safe}/) so deleting a
        # conversation doesn't orphan its uploaded challenge files on disk.
        shutil.rmtree(self.sessions_root / safe, ignore_errors=True)
        # Control state may live on a dedicated coordinator volume outside the
        # sessions tree, so deleting the worker workspace is intentionally not
        # relied on to scrub it.
        shutil.rmtree(self.control_root / safe, ignore_errors=True)

    def _protocol2_owner_settled(self, run: Optional[Run]) -> bool:
        if run is None:
            return True
        return bool(
            not run.runtime_incomplete
            and (run.task is None or run.task.done())
            and not self._standby_busy(run)
            and (run.runtime_cleanup_task is None or run.runtime_cleanup_task.done())
            and (run.standby_runtime_cleanup_task is None
                 or run.standby_runtime_cleanup_task.done())
        )

    async def archive_protocol2(self, run_id: str) -> dict[str, Any]:
        """Run the catalog-owned archive saga; legacy metadata is display-only."""
        adapter = self.protocol2
        if adapter is None or not self.is_protocol2_run(run_id):
            raise StateConflict("Protocol 2 run is unavailable")
        async with self._lifecycle_lock:
            if (self._shutting_down or run_id in self._closing_runs
                    or run_id in self._launching_runs):
                raise StateConflict("run lifecycle transition is in progress")
            run = self.runs.get(run_id)
            if not self._protocol2_owner_settled(run):
                raise StateConflict("Protocol 2 runtime owner is not settled")
            self._closing_runs.add(run_id)
        try:
            status = adapter.archive(
                run_id=run_id, operation_id=f"archive:{run_id}",
                occurred_at_ns=time.time_ns())
            if status.get("state") == "archived" and run is not None:
                # This is a non-authoritative rail projection applied only after
                # the catalog and per-run archive receipts are durable.
                meta = self.meta.set_archived(run_id, True, now=time.time())
                run.archived = bool(meta["archived"])
                run.pinned = bool(meta["pinned"])
                run.pinned_at = meta["pinned_at"]
            return status
        finally:
            async with self._lifecycle_lock:
                self._closing_runs.discard(run_id)

    def _protocol2_purge_plan(self, run_id: str) -> tuple[dict[str, str], ...]:
        # Logical locators are sealed in the external plan. Host paths remain an
        # adapter detail and never enter catalog events or API responses.
        return (
            {"locator": "session-jsonl", "adapter": "file"},
            {"locator": "session-tree", "adapter": "tree"},
            {"locator": "legacy-control-tree", "adapter": "tree"},
            {"locator": "rail-meta", "adapter": "metadata"},
            # The canonical run DB/CAS is deleted last. The catalog/tombstone is
            # outside this tree and can therefore resume after any crash window.
            {"locator": "protocol2-run-tree", "adapter": "tree"},
        )

    def _protocol2_purge_target(self, run_id: str, locator: str) -> Path | None:
        safe = self._safe_run_id(run_id)
        if locator == "session-jsonl":
            return self.sessions_root / f"{safe}.jsonl"
        if locator == "session-tree":
            return self.sessions_root / safe
        if locator == "legacy-control-tree":
            return self.control_root / safe
        if locator == "rail-meta":
            return None
        if locator == "protocol2-run-tree":
            view = self.protocol2.run_view(run_id)
            target = Path(view["target_root"])
            allowed = (Path(self.protocol2.root) / "runs").resolve()
            if not self._is_within(target.resolve(), allowed):
                raise RuntimeError("Protocol 2 target escaped the catalog run root")
            return target
        raise RuntimeError("unknown sealed purge locator")

    @staticmethod
    def _path_present(path: Path) -> bool:
        return path.exists() or path.is_symlink()

    def _execute_protocol2_purge_item(
        self, *, run_id: str, locator: str, adapter: str,
    ) -> bool:
        """Delete one sealed logical item and return whether it was pre-absent."""
        if adapter == "metadata":
            if locator != "rail-meta":
                raise RuntimeError("metadata adapter locator mismatch")
            already_absent = not self.meta.contains(run_id)
            self.meta.forget(run_id)
            if self.meta.contains(run_id):
                raise OSError("metadata absence readback failed")
            return already_absent
        path = self._protocol2_purge_target(run_id, locator)
        if path is None:
            raise RuntimeError("filesystem adapter has no target")
        already_absent = not self._path_present(path)
        if not already_absent:
            if adapter == "file":
                path.unlink()
            elif adapter == "tree":
                if path.is_symlink():
                    path.unlink()
                else:
                    shutil.rmtree(path)
            else:
                raise RuntimeError("unsupported purge adapter")
        if self._path_present(path):
            raise OSError("filesystem absence readback failed")
        return already_absent

    async def purge_protocol2(self, run_id: str) -> dict[str, Any]:
        """Execute a sealed, itemized, idempotent purge and retain tombstone."""
        adapter = self.protocol2
        if adapter is None or not self.is_protocol2_run(run_id):
            raise StateConflict("Protocol 2 run is unavailable")
        async with self._lifecycle_lock:
            if (self._shutting_down or run_id in self._closing_runs
                    or run_id in self._launching_runs):
                raise StateConflict("run lifecycle transition is in progress")
            run = self.runs.get(run_id)
            if not self._protocol2_owner_settled(run):
                raise StateConflict("Protocol 2 runtime owner is not settled")
            self._closing_runs.add(run_id)
        operation_id = f"purge:{run_id}"
        try:
            status = adapter.begin_purge(
                run_id=run_id, operation_id=operation_id,
                items=self._protocol2_purge_plan(run_id),
                occurred_at_ns=time.time_ns())
            for item in status["items"]:
                if item["state"] == "absent":
                    continue
                try:
                    already_absent = self._execute_protocol2_purge_item(
                        run_id=run_id, locator=item["locator"],
                        adapter=item["adapter"])
                except Exception as exc:
                    if item["state"] == "pending":
                        status = adapter.purge_item_unknown(
                            operation_id=operation_id, ordinal=item["ordinal"],
                            locator=item["locator"], adapter=item["adapter"],
                            error_class=type(exc).__name__,
                            occurred_at_ns=time.time_ns())
                    return status
                status = adapter.purge_item_absent(
                    operation_id=operation_id, ordinal=item["ordinal"],
                    locator=item["locator"], adapter=item["adapter"],
                    already_absent=already_absent,
                    occurred_at_ns=time.time_ns())
            status = adapter.complete_purge(
                operation_id=operation_id, occurred_at_ns=time.time_ns())
            if status.get("state") == "purged" and run is not None:
                async with self._lifecycle_lock:
                    if self.runs.get(run_id) is run:
                        self.runs.pop(run_id, None)
                await run.bus.close()
            return status
        finally:
            async with self._lifecycle_lock:
                self._closing_runs.discard(run_id)

    # ---- retention sweep: auto-archive idle runs, then delete stale ones -----

    def _last_activity(self, run: "Run") -> float:
        """Epoch seconds of a run's most recent event (its idle clock). 0.0 when
        unknown (no persisted events) → such a run is never auto-touched."""
        try:
            return float(run.store.summary(run.run_id).get("ts", 0.0) or 0.0)
        except Exception:
            return 0.0

    async def retention_sweep(self, *, now: float, archive_after_s: float,
                              delete_after_s: float) -> dict[str, list[str]]:
        """One retention pass: archive started runs idle > archive_after_s, and
        DELETE already-archived runs idle > delete_after_s. PINNED runs are never
        auto-touched; runs with an unknown idle clock (ts==0) are skipped. Returns
        {"archived": [...], "deleted": [...]} for logging/tests."""
        archived: list[str] = []
        deleted: list[str] = []
        for run in list(self.runs.values()):
            if not run.started or run.pinned:
                continue
            ts = self._last_activity(run)
            if ts <= 0:
                continue  # can't date it → leave it alone
            idle = now - ts
            meta = self.meta.get(run.run_id)
            ownership = self.protocol2_ownership(run.run_id, run=run)
            if ownership is None:
                continue
            if ownership:
                try:
                    view = self.protocol2.run_view(run.run_id)
                    if view["state"] == "archived" and idle > delete_after_s:
                        status = await self.purge_protocol2(run.run_id)
                        if status.get("state") == "purged":
                            deleted.append(run.run_id)
                    elif view["state"] == "sealed" and idle > archive_after_s:
                        status = await self.archive_protocol2(run.run_id)
                        if status.get("state") == "archived":
                            archived.append(run.run_id)
                except Exception:
                    LOG.exception(
                        "retention: Protocol 2 lifecycle failed closed for %s",
                        run.run_id)
                continue
            if meta["archived"]:
                if idle > delete_after_s:
                    if await self.delete(run.run_id):
                        deleted.append(run.run_id)
                        LOG.info(
                            "retention: deleted stale archived run %s (idle %.0fs)",
                            run.run_id, idle)
            elif idle > archive_after_s:
                self.set_archived(run.run_id, True, now=now)
                archived.append(run.run_id)
                LOG.info("retention: archived idle run %s (idle %.0fs)", run.run_id, idle)
        return {"archived": archived, "deleted": deleted}

    async def retention_loop(self, *, interval_s: float, archive_after_s: float,
                             delete_after_s: float) -> None:
        """Background task: run retention_sweep every interval_s until cancelled.
        Sleeps FIRST so startup isn't blocked and a short-lived test process never
        triggers a sweep. A sweep failure is logged and the loop continues."""
        while True:
            try:
                await asyncio.sleep(interval_s)
                await self.retention_sweep(
                    now=time.time(), archive_after_s=archive_after_s,
                    delete_after_s=delete_after_s)
            except asyncio.CancelledError:
                break
            except Exception:
                LOG.exception("retention sweep failed; continuing")

    def workspace_dir(self, run_id: str) -> Path:
        """Per-run persistent workspace: sessions/{id}/workspace/.

        Replaces the old tempfile.mkdtemp root so sandbox, artifacts, and
        shared_graph.db survive process restarts. Same id-sanitization as
        uploads_dir / _delete_artifacts."""
        safe = self._safe_run_id(run_id)
        d = self.sessions_root / safe / "workspace"
        d.mkdir(parents=True, exist_ok=True)
        return d

    def open_workspace(self, run_id: str) -> bool:
        """Open the run's workspace dir in the host file manager (operator-local —
        the deck runs in a browser, so a backend opener is the only way to truly
        reveal Finder/Explorer). Best-effort; False if it can't open."""
        if not self.is_protocol1_run(run_id):
            return False
        import subprocess
        import sys

        d = self.workspace_dir(run_id)  # created if missing; opening empty is fine
        try:
            if sys.platform.startswith("win"):
                os.startfile(str(d))  # type: ignore[attr-defined]
                return True
            opener = "open" if sys.platform == "darwin" else "xdg-open"
            if shutil.which(opener) is None:
                return False
            subprocess.Popen([opener, str(d)],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return True
        except Exception:
            return False

    def uploads_dir(self, run_id: str) -> Path:
        """Per-run folder where uploaded challenge files land: sessions/{id}/uploads/.

        Each conversation gets its own directory so a file-based challenge's
        handouts stay scoped to that run (the worker later stages them into its
        cwd via CliSolver._stage_attachments). Sanitize the id with the same rule
        as _delete_artifacts so a hostile run_id can't escape sessions/. The dir
        is a sibling of the run's {id}.jsonl log — SessionStore only globs
        *.jsonl, so a directory of the same stem never collides with rehydration.
        """
        safe = self._safe_run_id(run_id)
        d = self.sessions_root / safe / "uploads"
        d.mkdir(parents=True, exist_ok=True)
        return d

    def create(self, run_id: str) -> Run:
        if run_id in self.runs:
            return self.runs[run_id]
        bus = EventBus()
        store = SessionStore(root=self.sessions_root)
        self._sync_bus_seq(bus, store=store, run_id=run_id)
        self._seq += 1
        run = Run(
            run_id=run_id, bus=bus, cost=CostController(bus=bus), store=store,
            created_seq=self._seq,
        )
        run.profile_readiness = self._load_profile_readiness(run_id)
        # A run may already have a canonical Protocol 2 owner before its Web
        # projection is opened. Resolve that once at handle creation. A confirmed
        # Protocol 1 handle remains stable afterwards even if a later catalog query
        # is temporarily unavailable.
        if self.protocol2 is not None:
            try:
                if self.protocol2.has_run(run_id):
                    run.protocol_version = 2
            except Exception:
                run.protocol_ownership_confirmed = False
        bus.add_filter(self._generation_filter_for(run))
        bus.add_sink(store.sink)
        # sniff run.started / run.finished off the bus to keep rail metadata fresh
        # without making the run anything but a dumb event source.
        async def _meta_sink(ev: Event) -> None:
            # any event = activity. Keep this as metadata only: the rail itself is
            # creation-ordered, otherwise concurrent live runs visually hop around.
            self._seq += 1
            run.updated_seq = self._seq
            run.updated_at = ev.ts
            if ev.event_type is EventType.HITL_REQUEST:
                self._record_decision_request(run, ev)
            if _apply_operator_meta(run, ev):
                return
            if ev.event_type in {EventType.RUN_PREPARING, EventType.RUN_STARTED}:
                ch = ev.payload.get("challenge", {}) or {}
                run.started = True
                # Keep name EMPTY when the operator gave none — the rail renders a
                # "new conversation" placeholder, and the background summarizer fills
                # in a ChatGPT-style title via RUN_TITLED. Don't pin it to the run_id.
                if ch.get("name"):
                    run.name = ch["name"]
                run.category = ch.get("category", run.category) or run.category
                if ch.get("expected_flags"):
                    run.expected_flags = int(ch["expected_flags"])
                if "multi_flag" in ch:
                    run.multi_flag = bool(ch["multi_flag"])
            elif ev.event_type is EventType.RUN_TITLED:
                # auto-title landed from the background summarizer; only adopt it
                # if the operator hasn't supplied a real name (don't clobber).
                title = ev.payload.get("title") or ""
                if title and not run.name:
                    run.name = title
            elif ev.event_type is EventType.RUN_REOPENED:
                # The run is solving again. Resolve/continue keeps all prior flags
                # visible; false-positive payloads carry the one invalid flag to
                # drop. Legacy false-positive payloads with no flag still clear all.
                run.finished = False
                run.solved = False
                run.paused = False
                if ev.payload.get("reason") == "resolve":
                    return
                run.invalidate_flag(ev.payload.get("flag"))
            elif ev.event_type is EventType.FLAG_ACCEPTED:
                # Protocol 2 public visibility is intentionally independent from
                # lifecycle closure. Never synthesize solved/finished/progress here.
                run.merge_flags(ev.payload.get("flag"))
            elif ev.event_type is EventType.RUN_FINISHED:
                run.finished = True
                run.paused = False  # a finished run is never "paused"
                run.awaiting_help = False  # finished → no outstanding ask
                run.help_text = ""
                run.pending_help.clear()
                incoming_flags = ev.payload.get("flags") or ev.payload.get("flag")
                had_flag_payload = bool(incoming_flags)
                valid_incoming = run.valid_incoming_flags(incoming_flags)
                run.merge_flags(incoming_flags)
                if bool(ev.payload.get("solved")):
                    run.solved = bool(valid_incoming) if had_flag_payload else True
                if ev.payload.get("expected_flags"):
                    run.expected_flags = int(ev.payload["expected_flags"])
                if "multi_flag" in ev.payload:
                    run.multi_flag = bool(ev.payload["multi_flag"])
            else:
                _apply_blackboard_meta(run, ev)

        bus.add_sink(_meta_sink)
        self.runs[run_id] = run
        self._apply_meta(run)
        return run

    @staticmethod
    def _bump_bus_seq(bus: EventBus, seq: int) -> None:
        try:
            bus._seq = max(int(getattr(bus, "_seq", 0) or 0), int(seq or 0))
        except Exception:
            pass

    def _sync_bus_seq(
        self, bus: EventBus, *, store: Optional[SessionStore] = None,
        run_id: str
    ) -> None:
        store = store or SessionStore(root=self.sessions_root)
        self._bump_bus_seq(bus, store.last_stream_seq(run_id))

    def create_new(self) -> Run:
        """Mint a run under a fresh, never-reused id (for '+ New solve')."""
        self._seq += 1
        run_id = f"run-{self._seq:04d}"
        while run_id in self.runs:
            self._seq += 1
            run_id = f"run-{self._seq:04d}"
        return self.create(run_id)

    def _retire_worker_command_epoch(self, run: Run) -> None:
        """Clear execution-local selectors and close queued command receipts."""
        run.worker_registry.clear()
        while True:
            try:
                stale_worker_cmd = run.worker_cmds.get_nowait()
            except asyncio.QueueEmpty:
                break
            if isinstance(stale_worker_cmd, dict):
                stale_ack = stale_worker_cmd.get("_control_ack")
                if isinstance(stale_ack, asyncio.Future) and not stale_ack.done():
                    stale_ack.set_result({
                        "state": "unknown",
                        "detail": "stale worker command retired at execution epoch",
                        "target_ids": [],
                        "metadata": {"code": "stale_execution_epoch"},
                    })
            run.worker_cmds.task_done()

    def _retire_hitl_epoch(self, run: Run, *, terminal: bool) -> None:
        """Remove commands that no longer belong to a live execution generation."""
        while True:
            try:
                stale = run.hitl.get_nowait()
            except asyncio.QueueEmpty:
                break
            if isinstance(stale, dict):
                acknowledgement = stale.get("_control_ack")
                action = str(stale.get("action") or "")
                if (isinstance(acknowledgement, asyncio.Future)
                        and not acknowledgement.done()):
                    observed_stop = terminal and action in {
                        "stop", "complete", "force_cancel"}
                    acknowledgement.set_result({
                        "state": (
                            "effect_observed" if observed_stop else "unknown"),
                        "detail": (
                            "run generation terminated" if observed_stop
                            else "stale command retired at execution generation"),
                        "target_ids": [],
                        "metadata": {
                            "effect": (
                                "run_terminated" if observed_stop
                                else "stale_execution_generation"),
                        },
                    })
            run.hitl.task_done()

    def _launch_generation(self, run: Run, driver: Driver) -> asyncio.Task[Any]:
        """Create one owner-token-fenced execution wrapper.

        Caller holds ``_lifecycle_lock`` and has completed admission. The wrapper
        is shared by fresh start and resolve so both synthesize a terminal event on
        crash/cancel and neither stale generation can close a replacement bus.
        """
        previous_generation = run.execution_generation
        generation = previous_generation + 1
        if run.title_task is not None and not run.title_task.done():
            run.title_task.cancel()
        run.execution_generation = generation
        run.finished = False

        async def _go() -> None:
            failure_detail = ""
            try:
                await driver(run)
            except Exception as exc:
                LOG.exception(
                    "run driver failed before terminal receipt: run_id=%s generation=%s",
                    run.run_id, generation,
                )
                failure_detail = _safe_exception_detail("driver failed", exc)
            finally:
                current = asyncio.current_task()
                # A driver may transfer cleanup to a separately fenced runtime
                # owner. A stale wrapper or transferred owner performs no generation
                # finalization here. Avoid returning from ``finally`` so cancellation
                # and unexpected base exceptions retain their normal semantics.
                if (self._execution_owned(run, generation, current)
                        and not run.runtime_incomplete):
                    # If the driver exited without a terminal receipt, synthesize the
                    # generation's sole terminal event before closing its bus.
                    if not run.finished:
                        try:
                            reason = run.termination_reasons.pop(
                                generation, "runtime_failure")
                            detail = failure_detail
                            if reason == "operator_stop" and not detail:
                                detail = "Operator requested run termination"
                            error_id = _runtime_error_id(
                                run.run_id, generation, detail or reason)
                            if run.protocol_version == 2:
                                payload = {
                                    "expected_flags": run.expected_flags,
                                    "multi_flag": run.multi_flag,
                                    "solved": run.solved,
                                    "reason": reason,
                                    "failure_code": (
                                        "operator_stop"
                                        if reason == "operator_stop"
                                        else "runtime_driver_failed"),
                                    "failure_phase": "runtime",
                                    "error_id": error_id,
                                    "detail": detail,
                                }
                            else:
                                payload = {
                                    "flag": run.flag,
                                    "flags": list(run.flags),
                                    "expected_flags": run.expected_flags,
                                    "multi_flag": run.multi_flag,
                                    "solved": run.solved,
                                    "reason": reason,
                                    "failure_code": (
                                        "operator_stop"
                                        if reason == "operator_stop"
                                        else "runtime_driver_failed"),
                                    "failure_phase": "runtime",
                                    "error_id": error_id,
                                    "detail": detail,
                                }
                            await run.bus.emit(Event(
                                event_type=EventType.RUN_FINISHED,
                                run_id=run.run_id,
                                payload=payload,
                            ))
                        except Exception:
                            pass
                    # Event sinks may await. Re-check ownership before final close.
                    if self._execution_owned(run, generation, current):
                        self._retire_hitl_epoch(run, terminal=True)
                        self._retire_worker_command_epoch(run)
                        title_task = run.title_task
                        if (title_task is not None and title_task is not current
                                and not title_task.done()):
                            title_task.cancel()
                            await asyncio.gather(
                                title_task, return_exceptions=True)
                        run.finished = True
                        await run.bus.close()

        coroutine = _go()
        try:
            task = asyncio.create_task(
                coroutine, name=f"run-{run.run_id}-generation-{generation}")
        except BaseException:
            coroutine.close()
            run.execution_generation = previous_generation
            raise
        run.task = task
        return task

    @staticmethod
    def _control_epoch_drain_timeout() -> float:
        try:
            return max(0.05, float(os.environ.get(
                "MUTEKI_CONTROL_EPOCH_DRAIN_TIMEOUT", "35")))
        except (TypeError, ValueError):
            return 35.0

    async def _drain_control_before_launch(self, run_id: str, run: Run) -> bool:
        """Fence and drain every pre-launch control command to a terminal receipt.

        ``_launching_runs`` is published before this method is called, so new
        submissions fail closed. Acquiring the compile/submit lock waits for a
        request that crossed admission but has not yet reached the actor; actor
        ``join`` then drains commands already queued or executing. The wait is
        bounded—failure leaves the old epoch intact and launches nothing.
        """
        submit_lock = self._control_submit_locks.setdefault(
            run_id, asyncio.Lock())
        async with submit_lock:
            actor = run.control_actor
            if actor is None:
                return True
            try:
                await asyncio.wait_for(
                    actor.join(), timeout=self._control_epoch_drain_timeout())
            except asyncio.TimeoutError:
                LOG.error(
                    "refusing to launch %s: prior control epoch did not drain",
                    run_id,
                )
                return False
            return True

    async def start(self, run_id: str, driver: Driver) -> Run:
        """Admit and launch one fresh execution generation.

        Duplicate live starts are conflicts; they never overwrite the only task
        handle. Finished generations may be explicitly restarted on the same run
        id, with a fresh bus/control epoch and cleared terminal projection.
        """
        async with self._lifecycle_lock:
            if self._shutting_down:
                raise StateConflict("run manager is shutting down")
            if run_id in self._closing_runs or run_id in self._launching_runs:
                raise StateConflict(f"run {run_id} lifecycle transition is in progress")
            run = self.create(run_id)
            if not self.is_protocol1_run(run_id, run=run):
                # A catalog-owned V2 run is immutable and never re-enters the
                # generic start path after restart/archive/purge. Continuation
                # needs a future explicit execution-generation command.
                raise StateConflict(
                    f"run {run_id} is Protocol 2; legacy/restart start is unavailable")
            if run.runtime_incomplete:
                raise StateConflict(
                    f"run {run_id} still has an unsettled runtime owner")
            if run.task is not None and not run.task.done():
                raise StateConflict(f"run {run_id} is already running")
            if self._standby_busy(run):
                raise StateConflict(f"run {run_id} standby runtime is still active")
            self._launching_runs.add(run_id)
        try:
            if not await self._drain_control_before_launch(run_id, run):
                raise StateConflict(
                    f"run {run_id} prior control epoch is still draining")
            async with self._lifecycle_lock:
                if (self._shutting_down or self.runs.get(run_id) is not run
                        or run_id in self._closing_runs
                        or run.runtime_incomplete
                        or (run.task is not None and not run.task.done())
                        or self._standby_busy(run)):
                    raise StateConflict(
                        f"run {run_id} lifecycle changed before launch")
                self._fresh_bus(run)
                self._retire_hitl_epoch(run, terminal=False)
                self._retire_worker_command_epoch(run)
                run.protocol_version = int(
                    getattr(driver, "protocol_version", 1) or 1)
                if run.started:
                    _actor, journal, _secrets = self._ensure_control(run)
                    state = journal.reopen_state(
                        reason="explicit start generation")
                    run.control_generation = state.generation
                self._launch_generation(run, driver)
                run.finished = False
                run.solved = False
                run.flag = None
                run.flags = []
                run.paused = False
                run.started = True
                return run
        finally:
            async with self._lifecycle_lock:
                self._launching_runs.discard(run_id)

    # actions a standby (post-solve) worker can serve. pause/resume/submit only
    # make sense against a LIVE run, so they never trigger a standby.
    _STANDBY_ACTIONS = {"ask", "hint", "mark_false", "writeup", "redirect", "focus"}
    _OFFLINE_CONTROL_ACTIONS = {"clear_standing", "reset_guidance"}

    @staticmethod
    def _standby_cancel_timeout() -> float:
        try:
            return max(0.01, float(os.environ.get(
                "MUTEKI_STANDBY_CANCEL_TIMEOUT", "2")))
        except (TypeError, ValueError):
            return 2.0

    @staticmethod
    def _standby_runtime_status(run: Run) -> Optional[bool]:
        query = run.standby_runtime_exited
        if not callable(query):
            return None
        try:
            return bool(query())
        except Exception:
            # A broken proof boundary is never proof of exit.
            return False

    async def _settle_incomplete_runtime(
        self, run: Run, *, timeout: float,
    ) -> bool:
        """Boundedly wait/retry the retained main-runtime owner cleanup."""
        if not run.runtime_incomplete:
            return True
        task = run.runtime_cleanup_task
        if task is None or task.done():
            settle = run.runtime_settle
            if callable(settle):
                task = asyncio.create_task(
                    settle(), name=f"runtime-owner-settle-{run.run_id}")
                run.runtime_cleanup_task = task
        if task is None:
            return False
        try:
            await asyncio.wait_for(
                asyncio.shield(task), timeout=max(0.01, float(timeout)))
        except asyncio.TimeoutError:
            return False
        except asyncio.CancelledError:
            raise
        except Exception:
            return False
        return not run.runtime_incomplete and run.runtime_owner is None

    def _standby_scope_matches_winner(self, run: Run, target: str) -> bool:
        """Prove a finished-run selector includes the one resumable winner."""
        try:
            scope = ControlScope.parse(target or "global")
        except Exception:
            return False
        if scope.kind.value == "global":
            return True
        if scope.kind.value in {"run", "challenge"}:
            return scope.value == run.run_id
        winner = self.load_winner_continuation(run.run_id)
        if not winner:
            return False
        if scope.kind.value == "worker":
            persisted_worker = str(winner.get("worker_id") or "")
            return bool(persisted_worker and persisted_worker == scope.value)
        if scope.kind.value == "engine":
            return str(winner.get("engine") or "") == scope.value
        # Intent/lane identity is not persisted in continuation state; never widen it to
        # the winner merely because that is the only standby session available.
        return False

    def _register_standby_winner(self, run: Run) -> None:
        """Project the persisted winner as the only valid finished-run mailbox."""
        try:
            winner = self.load_winner_continuation(run.run_id)
            worker_id = str(winner.get("worker_id") or "").strip()
            if not worker_id:
                return
            run.worker_registry.register(WorkerRef(
                worker_id=worker_id,
                engine=str(winner.get("engine") or ""),
                challenge_id=run.run_id,
                status="standby",
                metadata={"persisted_winner": True},
            ))
        except Exception:
            return

    def _ensure_standby_context_cleanup(
        self, run: Run, *, owner: str,
        reservations: list[tuple[str, str]],
    ) -> Optional[asyncio.Task]:
        """Retain/retry standby reservation release until SQLite proves terminal."""
        if not owner or not reservations or run.control_journal is None:
            return None
        run.standby_context_cleanup_owner = str(owner)
        run.standby_context_cleanup_reservations = list(dict.fromkeys([
            *run.standby_context_cleanup_reservations,
            *((str(a), str(b)) for a, b in reservations),
        ]))
        current = run.standby_context_cleanup_task
        if current is not None and not current.done():
            return current

        async def _cleanup() -> None:
            journal = run.control_journal
            assert journal is not None
            pending = list(run.standby_context_cleanup_reservations)
            while pending:
                remaining: list[tuple[str, str]] = []
                for context_id, reservation_id in pending:
                    released = False
                    try:
                        released = bool(journal.release_context_reservation(
                            str(context_id), worker_id=str(owner),
                            reservation_id=str(reservation_id)))
                    except Exception:
                        released = False
                    if not released:
                        try:
                            # bound/unknown/already-active are terminal postconditions;
                            # only an actually reserved row still needs retry.
                            released = (
                                journal.context_delivery_status(str(context_id))
                                != "reserved")
                        except Exception:
                            released = False
                    if not released:
                        remaining.append((str(context_id), str(reservation_id)))
                pending = remaining
                run.standby_context_cleanup_reservations = list(remaining)
                if pending:
                    await asyncio.sleep(0.05)
            run.standby_context_cleanup_owner = ""

        task = asyncio.create_task(
            _cleanup(), name=f"standby-context-release-{run.run_id}")
        run.standby_context_cleanup_task = task

        def _done(done: asyncio.Task) -> None:
            try:
                done.result()
            except BaseException:
                pass
            if run.standby_context_cleanup_task is done:
                run.standby_context_cleanup_task = None

        task.add_done_callback(_done)
        return task

    async def _settle_standby_runtime(self, run: Run, *, timeout: float) -> bool:
        """Boundedly drive standby teardown while retaining its kill owner.

        Returns only proof: no live wrapper and no independently-live runtime.  A
        timeout never cancels the autonomous reaper; callers must keep the Run and
        its callbacks so cleanup remains retryable.
        """
        loop = asyncio.get_running_loop()
        deadline = loop.time() + max(0.01, float(timeout))
        if (run.standby_context_cleanup_reservations
                and (run.standby_context_cleanup_task is None
                     or run.standby_context_cleanup_task.done())):
            self._ensure_standby_context_cleanup(
                run, owner=run.standby_context_cleanup_owner,
                reservations=list(run.standby_context_cleanup_reservations))
        if self._standby_busy(run):
            await self._cancel_standby(
                run, timeout=max(0.01, deadline - loop.time()))
        cleanup = run.standby_runtime_cleanup_task
        if cleanup is not None and not cleanup.done():
            remaining = deadline - loop.time()
            if remaining > 0:
                try:
                    await asyncio.wait_for(
                        asyncio.shield(cleanup), timeout=remaining)
                except asyncio.TimeoutError:
                    return False
                except asyncio.CancelledError:
                    raise
                except Exception:
                    return False
        setup = run.standby_setup_task
        if setup is not None and not setup.done():
            remaining = deadline - loop.time()
            if remaining > 0:
                try:
                    await asyncio.wait_for(
                        asyncio.shield(setup), timeout=remaining)
                except asyncio.TimeoutError:
                    return False
                except asyncio.CancelledError:
                    raise
                except Exception:
                    # A failed acquisition is settled only after its owning wrapper
                    # has run the teardown/finally path below.
                    pass
        context_cleanup = run.standby_context_cleanup_task
        if context_cleanup is not None and not context_cleanup.done():
            remaining = deadline - loop.time()
            if remaining > 0:
                try:
                    await asyncio.wait_for(
                        asyncio.shield(context_cleanup), timeout=remaining)
                except asyncio.TimeoutError:
                    return False
                except asyncio.CancelledError:
                    raise
                except Exception:
                    return False
        task_live = run.standby_task is not None and not run.standby_task.done()
        runtime_live = self._standby_runtime_status(run) is False
        cleanup_live = (
            run.standby_runtime_cleanup_task is not None
            and not run.standby_runtime_cleanup_task.done()
        )
        setup_live = (
            run.standby_setup_task is not None
            and not run.standby_setup_task.done()
        )
        context_cleanup_live = (
            run.standby_context_cleanup_task is not None
            and not run.standby_context_cleanup_task.done()
        ) or bool(run.standby_context_cleanup_reservations)
        return (not task_live and not runtime_live and not cleanup_live
                and not setup_live and not context_cleanup_live)

    @classmethod
    def _standby_busy(cls, run: Run) -> bool:
        task_live = run.standby_task is not None and not run.standby_task.done()
        setup_live = (
            run.standby_setup_task is not None
            and not run.standby_setup_task.done()
        )
        runtime_status = cls._standby_runtime_status(run)
        context_cleanup_live = (
            run.standby_context_cleanup_task is not None
            and not run.standby_context_cleanup_task.done()
        ) or bool(run.standby_context_cleanup_reservations)
        return (task_live or setup_live or context_cleanup_live
                or runtime_status is False)

    async def _cancel_standby(self, run: Run, *, timeout: float) -> dict[str, Any]:
        """Cancel a standby at both the runtime and asyncio boundaries.

        Calling ``Task.cancel`` alone only interrupts the coroutine waiting on
        ``asyncio.to_thread``; it does not stop the thread or the shelled CLI.
        Therefore an observed effect requires ALL THREE fences: successful delivery
        to the live worker cancel callback, wrapper task unwind, and CliSolver proof
        that every runner thread and process handle exited. A deadline can prove only
        a partial/unknown effect, never success.
        """
        task = run.standby_task
        initial_runtime_status = self._standby_runtime_status(run)
        task_live = task is not None and not task.done()
        if not task_live and initial_runtime_status is not False:
            return {
                "state": "unknown",
                "detail": "no live standby worker was available to cancel",
                "target_ids": [],
                "metadata": {
                    "code": "no_live_standby",
                    "worker_cancel_delivered": False,
                    "task_done": bool(task is not None and task.done()),
                    "runtime_exit_confirmed": initial_runtime_status is True,
                },
            }

        loop = asyncio.get_running_loop()
        deadline = loop.time() + max(0.01, float(timeout))
        cancel_callback = run.standby_cancel
        runtime_query = run.standby_runtime_exited
        runtime_wait = run.standby_wait_runtime_exit
        cancel_delivered = False
        cancel_error = ""
        timed_out = False

        # Order is intentional: signal the real process tree first.  Cancelling the
        # wrapper first can run driver.finally and lose the only live process handle.
        if callable(cancel_callback):
            try:
                callback_result = cancel_callback()
                if inspect.isawaitable(callback_result):
                    remaining = max(0.001, deadline - loop.time())
                    callback_result = await asyncio.wait_for(
                        callback_result, timeout=remaining)
                # A callback may explicitly return False when its runtime boundary
                # could not accept the signal.  Legacy ``worker.cancel`` returns
                # None, which means the call itself completed successfully.
                cancel_delivered = callback_result is not False
            except asyncio.TimeoutError:
                timed_out = True
                cancel_error = "worker cancellation callback timed out"
            except Exception as exc:  # noqa: BLE001 - recorded as effect evidence
                cancel_error = _safe_exception_detail(
                    "worker cancellation callback failed", exc)

        if task is not None and not task.done():
            task.cancel()

        if task is not None and not task.done() and not timed_out:
            remaining = max(0.001, deadline - loop.time())
            try:
                await asyncio.wait_for(asyncio.shield(task), timeout=remaining)
            except asyncio.TimeoutError:
                timed_out = True
            except asyncio.CancelledError:
                # Expected when the INNER standby task acknowledged cancellation.
                # If it is not done, this CancelledError belongs to our own actor and
                # must propagate rather than being forged into a terminal receipt.
                if not task.done():
                    raise
            except Exception:
                # A failed-but-done worker is still an observed termination once the
                # real cancel callback was delivered.
                pass

        task_done = bool(task is not None and task.done())
        runtime_exit_confirmed = False
        if callable(runtime_query):
            try:
                runtime_exit_confirmed = bool(runtime_query())
            except Exception:
                runtime_exit_confirmed = False

        # The wrapper may already be done while asyncio.to_thread continues. Spend
        # the remainder of the standby deadline waiting on the independent runtime
        # fence. The waiter itself never cancels the tracked runner tasks.
        if (task_done and not runtime_exit_confirmed and callable(runtime_wait)
                and not timed_out):
            remaining = deadline - loop.time()
            if remaining > 0:
                try:
                    wait_result = runtime_wait(remaining)
                    if inspect.isawaitable(wait_result):
                        wait_result = await asyncio.wait_for(
                            wait_result, timeout=remaining)
                    runtime_exit_confirmed = bool(wait_result)
                except asyncio.TimeoutError:
                    timed_out = True
                except asyncio.CancelledError:
                    raise
                except Exception as exc:  # noqa: BLE001 - effect evidence
                    cancel_error = (cancel_error + "; " if cancel_error else "") + (
                        _safe_exception_detail("runtime exit fence failed", exc))
            else:
                timed_out = True

        # Re-query after the await to close a boundary race where the waiter reached
        # its deadline just as the final process changed poll state.
        if callable(runtime_query):
            try:
                runtime_exit_confirmed = bool(runtime_query())
            except Exception:
                pass
        if not (task_done and runtime_exit_confirmed) and loop.time() >= deadline:
            timed_out = True

        metadata = {
            "worker_cancel_registered": callable(cancel_callback),
            "worker_cancel_delivered": cancel_delivered,
            "task_done": task_done,
            "runtime_exit_registered": (
                callable(runtime_query) and callable(runtime_wait)),
            "runtime_exit_confirmed": runtime_exit_confirmed,
            "timed_out": timed_out,
        }
        if cancel_error:
            metadata["cancel_error"] = cancel_error[:500]

        if cancel_delivered and task_done and runtime_exit_confirmed:
            return {
                "state": "effect_observed",
                "detail": (
                    "standby worker cancellation, task unwind, and runtime exit confirmed"),
                "target_ids": [],
                "metadata": {**metadata, "effect": "standby_cancelled"},
            }
        if cancel_delivered or task_done or runtime_exit_confirmed:
            detail = (cancel_error or
                      "standby cancellation was requested but runtime exit was not fully confirmed")
            return {
                "state": "partial",
                "detail": detail,
                "target_ids": [],
                "metadata": {**metadata, "code": "standby_cancel_unconfirmed"},
            }
        return {
            "state": "unknown",
            "detail": (cancel_error or
                       "standby cancellation could not be confirmed before the deadline"),
            "target_ids": [],
            "metadata": {**metadata, "code": "standby_cancel_unknown"},
        }

    def _ensure_control(self, run: Run) -> tuple[ControlActor, SQLiteControlJournal,
                                                  SecretStore]:
        """Create the one actor/journal/secret boundary owned by this run."""
        if (run.control_actor is not None and run.control_journal is not None
                and run.control_secrets is not None):
            return run.control_actor, run.control_journal, run.control_secrets

        db_path, secrets_path = control_paths(
            self.coordinator_control_dir(run.run_id))
        journal = SQLiteControlJournal.open(db_path=db_path, run_id=run.run_id)
        # SessionStore and the control journal are separate durable sinks. A crash
        # can persist HITL_REQUEST to JSONL before the live metadata sink appends
        # its DecisionRequest. Rebuild that idempotent edge before validating any
        # answer so a replayed card can never become permanently unanswerable.
        self._reconcile_decision_requests(run, journal)
        # This is the sole owner boundary for a fresh web runtime generation. Any
        # pre-Popen reservation left by the prior process has an unknowable delivery
        # outcome and must be terminalised append-only, never silently replayed.
        journal.recover_context_reservations(actor="web-runtime-recovery")
        secrets = SecretStore(secrets_path)

        def _live() -> bool:
            return run.task is not None and not run.task.done()

        try:
            ack_timeout = float(os.environ.get("MUTEKI_CONTROL_ACK_TIMEOUT", "2"))
        except (TypeError, ValueError):
            ack_timeout = 2.0
        try:
            claim_timeout = float(os.environ.get(
                "MUTEKI_CONTROL_CLAIM_TIMEOUT", "30"))
        except (TypeError, ValueError):
            claim_timeout = 30.0
        async def _standby_control(wire: dict[str, Any]) -> Any:
            action = str(wire.get("action") or "").lower()
            busy = self._standby_busy(run)
            target = str(wire.get("target") or "global")
            exact_text = str(wire.get("text") or wire.get("hint") or "").strip()
            if action == "ask" and not exact_text:
                return {
                    "state": "unknown",
                    "detail": "ask requires a question",
                    "target_ids": [],
                    "metadata": {"code": "followup_question_required"},
                }

            if action in self._OFFLINE_CONTROL_ACTIONS:
                # The actor already expired typed ContextResources. Atomically
                # expire the evidence-graph projection too so a restart cannot
                # resurrect guidance that an offline command claimed to clear.
                graph_db = (
                    self.workspace_dir(run.run_id) / "graph" / "shared_graph.db")
                expired_directives: list[str] = []
                companion = (wire.get("_control_companion")
                             if isinstance(wire.get("_control_companion"), dict)
                             else {})
                expired_context_count = int(
                    companion.get("expired_context_count") or 0)
                graph = None
                if graph_db.exists():
                    try:
                        from muteki.models.solve_graph import Challenge
                        from muteki.swarm.shared_graph import SQLiteSharedGraph
                        graph = SQLiteSharedGraph.open(
                            db_path=graph_db,
                            challenge=Challenge(
                                id=run.run_id,
                                name=run.name or run.run_id,
                                category=run.category or "web",
                            ),
                        )
                        source_command_id = str(
                            wire.get("command_id") or "").strip()
                        matched_source_ids = sorted({
                            str(value or "").strip()
                            for value in (
                                companion.get("matched_source_command_ids") or [])
                            if str(value or "").strip()
                        })
                        if source_command_id:
                            clear_result = graph.apply_standing_clear(
                                command_id=source_command_id,
                                actor="operator",
                                text=("" if exact_text.startswith("secret://")
                                      else exact_text),
                                eligible_command_ids=(
                                    matched_source_ids if exact_text else None),
                                match_by_source_ids=exact_text.startswith(
                                    "secret://"),
                            )
                            expired_directives = list(
                                clear_result.get("expired_directives") or [])
                        else:
                            expired_directives = graph.expire_standing_directives(
                                actor="operator", text=exact_text)
                        remaining = [
                            row for row in graph.operator_directives(active_only=True)
                            if row.get("standing")
                            and (not exact_text or row.get("text") == exact_text)
                        ]
                        if remaining:
                            raise RuntimeError("standing directive remained active")
                    except Exception:
                        return {
                            "state": ("partial" if expired_context_count else "failed"),
                            "detail": "offline standing guidance expiration failed",
                            "target_ids": [],
                            "metadata": {
                                "code": "guidance_graph_expire_failed",
                                "expired_context_count": expired_context_count,
                            },
                        }
                    finally:
                        if graph is not None:
                            graph.close()
                remaining_context = [
                    resource for resource in journal.context_resources(active_only=True)
                    if resource.standing
                    and (not exact_text or resource.content == exact_text)
                ]
                if remaining_context:
                    return {
                        "state": ("partial" if (
                            expired_directives or expired_context_count) else "failed"),
                        "detail": "offline standing context expiration was not confirmed",
                        "target_ids": [],
                        "metadata": {
                            "code": "guidance_context_expire_failed",
                            "expired_context_count": expired_context_count,
                        },
                    }
                return {
                    "state": "effect_observed",
                    "detail": "offline standing guidance durably expired",
                    "target_ids": [],
                    "metadata": {
                        "effect": "guidance_cleared",
                        "expired_directives": expired_directives,
                        "expired_context_count": expired_context_count,
                    },
                }

            if not self._standby_scope_matches_winner(run, target):
                return {
                    "state": "unknown",
                    "detail": "standby winner identity does not match control scope",
                    "target_ids": [],
                    "metadata": {"code": "standby_scope_unresolved"},
                }
            if action == "mark_false":
                flag = str(wire.get("flag") or "").strip()
                if not flag and exact_text:
                    match = re.search(
                        r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}", exact_text)
                    flag = match.group(0) if match else exact_text
                if not flag:
                    flag = str(run.flag or "")
                if not flag:
                    return {
                        "state": "unknown",
                        "detail": "no flag was available to invalidate",
                        "target_ids": [],
                        "metadata": {"code": "flag_unavailable"},
                    }
                graph_db = (
                    self.workspace_dir(run.run_id) / "graph" / "shared_graph.db")
                if not graph_db.exists():
                    return {
                        "state": "failed",
                        "detail": "offline flag graph is unavailable",
                        "target_ids": [],
                        "metadata": {"code": "flag_graph_unavailable"},
                    }
                graph = None
                try:
                    from muteki.models.solve_graph import Challenge
                    from muteki.swarm.shared_graph import SQLiteSharedGraph
                    graph = SQLiteSharedGraph.open(
                        db_path=graph_db,
                        challenge=Challenge(
                            id=run.run_id,
                            name=run.name or run.run_id,
                            category=run.category or "web",
                        ),
                    )
                    info = graph.reopen_after_false_positive(
                        actor="operator", flag=flag)
                except Exception:
                    return {
                        "state": "failed",
                        "detail": "offline flag invalidation was not committed",
                        "target_ids": [],
                        "metadata": {"code": "flag_invalidation_failed"},
                    }
                finally:
                    if graph is not None:
                        graph.close()

                run.invalidate_flag(flag)
                try:
                    self.update_winner_continuation_flags(
                        run.run_id, list(run.flags))
                except Exception:
                    pass
                runtime_wire = dict(wire)
                runtime_wire["_control_mark_false_applied"] = True
                try:
                    from muteki.core.events import blackboard_delta_payload
                    # A false-positive invalidation resumes solving and therefore
                    # owns a new execution generation. This lets its normal
                    # RUN_STARTED/RUN_FINISHED stream through while the completed
                    # generation remains sealed against late worker frames.
                    run.execution_generation += 1
                    self._fresh_bus(run)
                    run.finished = False
                    run.solved = False
                    run.paused = False
                    await run.bus.emit(Event(
                        event_type=EventType.BLACKBOARD_DELTA,
                        run_id=run.run_id,
                        payload=blackboard_delta_payload(
                            "flag_invalidated", actor="operator", flag=flag),
                    ))
                    await run.bus.emit(Event(
                        event_type=EventType.RUN_REOPENED,
                        run_id=run.run_id,
                        payload={
                            "flag": flag,
                            "execution_generation": run.execution_generation,
                        },
                    ))
                except Exception:
                    pass
                if busy:
                    return {
                        "state": "partial",
                        "detail": "flag invalidated; standby re-solve is already busy",
                        "target_ids": [],
                        "metadata": {
                            "effect": "flag_invalidated",
                            "code": "standby_busy",
                            "reopened": list(info.get("reopened") or []),
                        },
                    }
                accepted = self._ensure_standby(run.run_id, runtime_wire)
                return {
                    "state": "effect_observed" if accepted else "partial",
                    "detail": (
                        "flag invalidated and standby re-solve scheduled" if accepted
                        else "flag invalidated but standby re-solve could not start"),
                    "target_ids": [],
                    "metadata": {
                        "effect": "flag_invalidated",
                        "resolve_scheduled": bool(accepted),
                        "reopened": list(info.get("reopened") or []),
                    },
                }
            if action in {"stop", "complete", "force_cancel"} and busy:
                return await self._cancel_standby(
                    run, timeout=self._standby_cancel_timeout())
            if action not in self._STANDBY_ACTIONS:
                return None
            if busy:
                return {
                    "state": "unknown",
                    "detail": "standby worker is already serving another command",
                    "target_ids": [],
                    "metadata": {"code": "standby_busy"},
                }
            runtime_wire = dict(wire)  # remains opaque; driver resolves after reserve
            command_id = str(wire.get("command_id") or "")
            reservation: Optional[tuple[str, str]] = None
            reservation_owner = f"standby:{command_id}" if command_id else ""
            context_status = "missing"
            if command_id:
                try:
                    from muteki.control import context_resource_id_for_command
                    context_id = context_resource_id_for_command(command_id)
                    context_status = journal.context_delivery_status(context_id)
                    if context_status == "active":
                        reservation_id = journal.reserve_context(
                            context_id, worker_id=reservation_owner)
                        if not reservation_id:
                            raise RuntimeError("reservation unavailable")
                        reservation = (context_id, str(reservation_id))
                    elif context_status != "missing":
                        raise RuntimeError(
                            f"context state {context_status} is not deliverable")
                except Exception:
                    return {
                        "state": "unknown",
                        "detail": "standby context is not deliverable",
                        "target_ids": [],
                        "metadata": {"code": "standby_context_unavailable"},
                    }

            def _has_secret_ref(value: Any) -> bool:
                if isinstance(value, dict):
                    return any(_has_secret_ref(child) for child in value.values())
                if isinstance(value, (list, tuple)):
                    return any(_has_secret_ref(child) for child in value)
                return isinstance(value, str) and value.startswith("secret://")

            carries_prompt_context = (
                action in {"ask", "hint", "focus", "redirect"}
                and any(wire.get(key) for key in (
                    "text", "hint", "url", "target_url", "context"))
            )
            if reservation is None and (
                    _has_secret_ref(wire) or carries_prompt_context):
                return {
                    "state": "unknown",
                    "detail": "standby prompt context has no active reservation",
                    "target_ids": [],
                    "metadata": {"code": "standby_context_unavailable"},
                }
            loop = asyncio.get_running_loop()
            delivery_ack: "asyncio.Future[bool]" = loop.create_future()
            runtime_wire["_standby_delivery_ack"] = delivery_ack
            runtime_wire["_control_context_reservations"] = (
                [reservation] if reservation is not None else [])
            runtime_wire["_control_context_owner"] = reservation_owner
            accepted = self._ensure_standby(run.run_id, runtime_wire)
            if not accepted:
                if reservation is not None:
                    released = False
                    try:
                        released = bool(journal.release_context_reservation(
                            reservation[0], worker_id=reservation_owner,
                            reservation_id=reservation[1]))
                    except Exception:
                        pass
                    if not released:
                        self._ensure_standby_context_cleanup(
                            run, owner=reservation_owner,
                            reservations=[reservation])
                return {
                    "state": "unknown",
                    "detail": "standby worker could not be started",
                    "target_ids": [],
                    "metadata": {"code": "standby_start_failed"},
                }
            try:
                delivered = await asyncio.wait_for(
                    asyncio.shield(delivery_ack), timeout=max(0.05, claim_timeout))
            except asyncio.TimeoutError:
                await self._cancel_standby(
                    run, timeout=self._standby_cancel_timeout())
                return {
                    "state": "unknown",
                    "detail": "standby prompt delivery was not confirmed",
                    "target_ids": [],
                    "metadata": {"code": "standby_delivery_timeout"},
                }
            return {
                "state": "effect_observed" if delivered else "unknown",
                "detail": ("standby prompt delivery confirmed" if delivered
                           else ("standby prompt delivery was not confirmed and "
                                 "may have crossed the process/stdin boundary")),
                "target_ids": [],
                "metadata": {
                    "effect": ("standby_prompt_started" if delivered
                               else "delivery_unknown"),
                    "context_reserved": reservation is not None,
                    "process_start_unknown": not delivered,
                },
            }

        port = QueueControlPort(
            inbox=run.hitl,
            is_live=_live,
            ack_timeout=ack_timeout,
            claim_timeout=claim_timeout,
            standby_actions=tuple(
                self._STANDBY_ACTIONS | self._OFFLINE_CONTROL_ACTIONS),
            on_standby=_standby_control,
        )

        async def _cancel_main_runtime(command, targets, desired) -> ApplyResult:
            run_wide = command.scope.kind.value in {
                "global", "run", "challenge",
            }
            allows_run_termination_fallback = (
                command.action in {ControlAction.STOP, ControlAction.COMPLETE}
                or (
                    command.action is ControlAction.FORCE_CANCEL
                    and desired.mode is RunControlMode.TERMINATED
                )
            )
            if allows_run_termination_fallback and run_wide:
                run.termination_reasons[run.execution_generation] = "operator_stop"
            result = await port.apply(command, targets, desired)
            if not allows_run_termination_fallback or not run_wide:
                return result
            # QueueControlPort already owns the stronger standby process/runtime
            # fence. Never launder its PARTIAL/UNKNOWN into a clean main-task exit
            # merely because run.task is absent.
            if (run.standby_task is not None
                    or callable(run.standby_runtime_exited)
                    or callable(run.standby_wait_runtime_exit)):
                return result

            task = run.task
            cancel_requested = bool(task is not None and not task.done())
            if cancel_requested:
                run.termination_reasons[run.execution_generation] = "operator_stop"
                task.cancel()
                try:
                    await asyncio.wait_for(
                        asyncio.shield(task),
                        timeout=self._standby_cancel_timeout(),
                    )
                except (asyncio.CancelledError, asyncio.TimeoutError):
                    pass
                except Exception:
                    # The wrapper's failure is reflected by task.done/runtime owner
                    # below; exception text is never needed for the cancellation proof.
                    pass
            task_done = task is None or task.done()
            exit_confirmed = task_done and not run.runtime_incomplete
            metadata = {
                "effect": "run_terminated" if exit_confirmed else "run_cancel_requested",
                "coordinator_effect": result.state.value,
                "cancel_requested": cancel_requested,
                "task_done": task_done,
                "runtime_exit_confirmed": exit_confirmed,
            }
            if exit_confirmed:
                run.paused = False
                if not run.finished:
                    detail = "Operator requested run termination"
                    await run.bus.emit(Event(
                        event_type=EventType.RUN_FINISHED,
                        run_id=run.run_id,
                        payload={
                            "solved": bool(run.solved),
                            "flag": run.flag,
                            "flags": list(run.flags),
                            "reason": "operator_stop",
                            "failure_code": "operator_stop",
                            "failure_phase": "runtime",
                            "error_id": _runtime_error_id(
                                run.run_id, run.execution_generation, detail),
                            "detail": detail,
                        },
                    ))
                return ApplyResult(
                    state=EffectState.EFFECT_OBSERVED,
                    detail="run task cancellation and runtime exit confirmed",
                    target_ids=[], metadata=metadata,
                )
            return ApplyResult(
                state=EffectState.PARTIAL if cancel_requested else EffectState.UNKNOWN,
                detail="run cancellation requested but runtime exit is unconfirmed",
                target_ids=[], metadata=metadata,
            )

        class _RuntimeFencedPort:
            async def apply(self, command, targets, desired):
                return await _cancel_main_runtime(command, targets, desired)

        async def _effect_sink(receipt) -> None:
            command = journal.get_command(receipt.command_id)
            if command is None:
                return
            run.control_generation = journal.current_state().generation
            await run.bus.emit(Event(
                event_type=EventType.CONTROL_COMMAND,
                run_id=run.run_id,
                payload=effect_event_payload(command, receipt),
            ))

        actor = ControlActor(
            run_id=run.run_id,
            journal=journal,
            port=_RuntimeFencedPort(),
            registry=run.worker_registry,
            admission=ControlAdmission(challenge_id=run.run_id),
            effect_sink=_effect_sink,
            secret_resolver=secrets.resolve,
        )
        run.control_actor = actor
        run.control_journal = journal
        run.control_secrets = secrets
        run.control_generation = journal.current_state().generation
        return actor, journal, secrets

    def _decision_request_from_event(
        self, run: Run, ev: Event,
    ) -> Optional[DecisionRequest]:
        payload = ev.payload or {}
        request_id = str(
            payload.get("request_id") or payload.get("id") or "").strip()
        prompt = str(
            payload.get("need") or payload.get("prompt") or "").strip()
        if not request_id or not prompt:
            return None
        worker = str(payload.get("worker") or ev.solver_id or "").strip()
        raw_scope = payload.get("blocking_scope")
        try:
            scope = ControlScope.parse(
                raw_scope if raw_scope is not None
                else f"worker:{worker}" if worker else "global")
        except (TypeError, ValueError):
            scope = ControlScope.parse(
                f"worker:{worker}" if worker else "global")
        worker_ref = next(
            (ref for ref in run.worker_registry.snapshot()
             if ref.worker_id == worker), None)
        intent_id = str(
            payload.get("intent_id")
            or getattr(worker_ref, "intent_id", "") or "")
        lane = str(
            payload.get("lane")
            or getattr(worker_ref, "lane", "") or "")
        delivery_scope = str(payload.get("delivery_scope") or "")
        if not delivery_scope:
            if intent_id:
                delivery_scope = f"intent:{intent_id}"
            elif lane:
                delivery_scope = f"lane:{lane}"
        need_kind = str(payload.get("need_kind") or "external_blocker")
        kind = (DecisionKind.EXTERNAL_INPUT
                if need_kind == "external_blocker"
                else DecisionKind.UNCERTAINTY)
        return DecisionRequest(
            request_id=request_id,
            run_id=run.run_id,
            worker_id=worker,
            prompt=prompt,
            kind=kind,
            blocking_scope=scope,
            choices=[str(v) for v in (payload.get("options") or [])][:32],
            default_action=str(payload.get("default_action") or ""),
            execution_id=str(payload.get("execution_id") or ""),
            execution_occurrence=str(
                payload.get("execution_occurrence") or ""),
            resolve_epoch=str(payload.get("resolve_epoch") or ""),
            deadline_at=(float(payload["deadline_at"])
                         if payload.get("deadline_at") is not None else None),
            created_at=float(ev.ts),
            metadata={
                "delivery_scope": delivery_scope,
                "engine": str(
                    payload.get("engine")
                    or getattr(worker_ref, "engine", "") or ""),
                "intent_id": intent_id,
                "lane": lane,
                "reconciled_from_session": True,
            },
        )

    def _reconcile_decision_requests(
        self, run: Run, journal: SQLiteControlJournal,
    ) -> int:
        appended = 0
        for raw in run.store.load_all(run.run_id):
            if str(raw.get("event_type") or "") != EventType.HITL_REQUEST.value:
                continue
            try:
                request = self._decision_request_from_event(
                    run, Event.model_validate(raw))
                if request is None:
                    continue
                if journal.get_decision_request(request.request_id) is None:
                    journal.append_decision_request(request)
                    appended += 1
            except Exception:
                # Preserve other valid cards if one historical row is malformed or
                # reuses an id inconsistently. That row remains fail-closed/unknown.
                LOG.error(
                    "failed to reconcile durable decision request for run %s",
                    run.run_id,
                )
        return appended

    def _record_decision_request(self, run: Run, ev: Event) -> None:
        request = self._decision_request_from_event(run, ev)
        if request is None:
            return
        try:
            _actor, journal, _secrets = self._ensure_control(run)
            journal.append_decision_request(request)
        except Exception:
            LOG.exception(
                "failed to journal decision request %s", request.request_id)

    async def post_control(self, run_id: str, body: dict[str, Any]) -> dict[str, Any]:
        """Durably accept one command; never confuse HTTP acceptance with effect."""
        run = self.runs.get(run_id)
        if run is None:
            return {"ok": False, "status": "unknown_run"}
        if not self.is_protocol1_run(run_id, run=run):
            return {
                "ok": False,
                "status": "unavailable",
                "detail": "Protocol 2 control/standby is not enabled for live canary",
                "code": "PROTOCOL2_CONTROL_UNAVAILABLE",
            }
        if (self._shutting_down or run_id in self._closing_runs
                or run_id in self._launching_runs):
            return {
                "ok": False,
                "status": "unknown",
                "detail": "run lifecycle transition is in progress",
                "code": "run_lifecycle_unavailable",
            }
        lock = self._control_submit_locks.setdefault(run_id, asyncio.Lock())
        async with lock:
            if (self.runs.get(run_id) is not run or self._shutting_down
                    or run_id in self._closing_runs
                    or run_id in self._launching_runs):
                return {
                    "ok": False,
                    "status": "unknown",
                    "detail": "run lifecycle transition is in progress",
                    "code": "run_lifecycle_unavailable",
                }
            return await self._post_control_serialized(run_id, run, body)

    async def _post_control_serialized(
        self, run_id: str, run: Run, body: dict[str, Any],
    ) -> dict[str, Any]:
        """Compile and submit under the per-run idempotency/secret boundary."""
        if run.runtime_incomplete:
            return {
                "ok": False,
                "status": "unknown",
                "detail": "runtime shutdown is incomplete",
                "code": "runtime_shutdown_incomplete",
            }
        actor, journal, secrets = self._ensure_control(run)
        requested_id = str(body.get("command_id") or "").strip()
        existing = journal.get_command(requested_id) if requested_id else None

        def _decision_status_with_reconcile(
                request_id: str) -> Optional[DecisionStatus]:
            status = journal.decision_status(request_id)
            if status is None and request_id:
                # The JSONL sink may have committed while the live metadata sink
                # transiently failed. Repair in the same process as well as after
                # restart, then re-read before rejecting the operator's answer.
                self._reconcile_decision_requests(run, journal)
                status = journal.decision_status(request_id)
            return status

        # Validate decision correlation before any plaintext is staged into the
        # SecretStore. This closes the larger boundary around compile validation:
        # an unknown/already-answered request must not leave an unreachable file.
        raw_payload = body.get("payload")
        decision_payload = raw_payload if isinstance(raw_payload, dict) else {}
        raw_request_id = str(
            decision_payload.get("request_id")
            or body.get("request_id")
            or ""
        ).strip()
        raw_action = str(body.get("action") or "hint").strip().lower()
        is_decision_answer = (
            raw_action == ControlAction.ANSWER_DECISION.value
            or (raw_action in {"answer", "submit"} and bool(raw_request_id))
        )
        if is_decision_answer:
            decision_status = _decision_status_with_reconcile(raw_request_id)
            if decision_status is None:
                raise ControlPayloadError(
                    f"unknown decision request: {raw_request_id}")
            if decision_status is not DecisionStatus.OPEN and existing is None:
                raise StateConflict(
                    f"decision {raw_request_id!r} is already {decision_status.value}")

        command = compile_control_command(
            run_id, body, secrets=secrets, existing_command=existing)
        if existing is None:
            existing = journal.get_command(command.command_id)

        request_id = str(command.payload.get("request_id") or "").strip()
        if command.action is ControlAction.ANSWER_DECISION:
            decision_status = _decision_status_with_reconcile(request_id)
            if decision_status is None:
                raise ControlPayloadError(f"unknown decision request: {request_id}")
            if decision_status is not DecisionStatus.OPEN and existing is None:
                raise StateConflict(
                    f"decision {request_id!r} is already {decision_status.value}")

        live = run.task is not None and not run.task.done()
        schedule_standby = not live and command.action.value in self._STANDBY_ACTIONS
        schedule_offline = (
            not live
            and command.action.value in self._OFFLINE_CONTROL_ACTIONS)
        if schedule_standby:
            self._register_standby_winner(run)
        if ((schedule_standby or schedule_offline)
                and bool(getattr(run.bus, "_closed", False))):
            # Finished buses are closed; make receipt events visible before the actor
            # emits them and before the standby worker starts.
            self._fresh_bus(run)

        receipt = await actor.submit(command)
        if existing is None:
            await run.bus.emit(Event(
                event_type=EventType.HITL_RESPONSE,
                run_id=run_id,
                payload=safe_hitl_echo(command, status=receipt.state.value),
            ))
        ok = receipt.state not in {EffectState.REJECTED, EffectState.FAILED}
        result = {
            "ok": ok,
            "command_id": command.command_id,
            "status": receipt.state.value,
            "generation": receipt.observed_generation,
            "detail": safe_receipt_detail(command, receipt.detail),
            "code": receipt.metadata.get("code", ""),
        }
        if schedule_standby and existing is None:
            # Give the actor one scheduling turn so legacy callers can immediately
            # observe standby_task without claiming the terminal effect in HTTP.
            await asyncio.sleep(0)
        return result

    def control_receipt(self, run_id: str, command_id: str) -> Optional[dict[str, Any]]:
        """Return a safe durable receipt projection for crash/event reconciliation."""
        run = self.runs.get(run_id)
        if run is None:
            return None
        if not self.is_protocol1_run(run_id, run=run):
            return None
        _actor, journal, _secrets = self._ensure_control(run)
        command = journal.get_command(str(command_id or ""))
        if command is None:
            return None
        receipt = journal.latest_effect(command.command_id)
        if receipt is None:
            return None
        return {
            "command_id": command.command_id,
            "receipt_id": receipt.receipt_id,
            "action": command.action.value,
            "target": command.scope.as_legacy_target(),
            "status": receipt.state.value,
            "generation": receipt.observed_generation,
            "target_ids": list(receipt.target_ids),
            "detail": safe_receipt_detail(command, receipt.detail),
            "code": str(receipt.metadata.get("code") or ""),
            "terminal": receipt.state.terminal,
        }

    async def post_hitl(self, run_id: str, target: str, action: str, **fields: Any) -> bool:
        """Legacy bool facade compiled onto the same durable ControlCommand path."""
        run = self.runs.get(run_id)
        if run is None:
            return False
        if not self.is_protocol1_run(run_id, run=run):
            return False
        fields = dict(fields)
        # Old clients have no decision picker correlation. Preserve compatibility
        # only when there is exactly one unambiguous pending request; with zero or
        # multiple requests we refuse to guess and leave request_id absent.
        if not fields.get("request_id") and len(run.pending_help) == 1:
            fields["request_id"] = next(iter(run.pending_help))
        # M2: drop an identical back-to-back resend (same target/action/text/url).
        # The UI has no client throttle, and an operator hammering the SAME hint at a
        # busy single-shot worker (run-0011: 11×) otherwise queues 11 items + 11
        # events + 11 downstream _drain_hitl sweeps. A genuinely new command (changed
        # text, or a different action) still goes through.
        sig = (target, action, str(fields.get("text") or fields.get("hint") or ""),
               str(fields.get("url") or fields.get("target_url") or ""),
               str(fields.get("flag") or ""),
               str(fields.get("request_id") or ""),
               bool(fields.get("standing", False)),
               str(fields.get("preempt_policy") or fields.get("preemption") or ""))
        # `writeup` is an idempotent-looking no-arg command from the UI, but each
        # click is a real request to run a fresh post-solve standby turn. If we
        # dedupe it here, the second "生成复盘" click only echoes a duplicate
        # HITL_RESPONSE and never starts a worker, which reads as a stuck button.
        if action != "writeup" and getattr(run, "_last_hitl_sig", None) == sig:
            await run.bus.emit(Event(
                event_type=EventType.HITL_RESPONSE, run_id=run_id,
                payload=hitl_response_payload(
                    target, action, status="duplicate", text="[duplicate omitted]")))
            return True
        result = await self.post_control(
            run_id, {"target": target, "action": action, **fields})
        terminal_ok = False
        terminal_observed = False
        if result.get("command_id") and run.control_actor is not None:
            await run.control_actor.join()
            terminal = run.control_journal.latest_effect(result["command_id"])
            terminal_ok = bool(
                terminal is not None
                and terminal.state in {
                    EffectState.EFFECT_OBSERVED,
                    EffectState.PARTIAL,
                }
            )
            terminal_observed = bool(
                terminal is not None
                and terminal.state is EffectState.EFFECT_OBSERVED)
        if terminal_observed:
            run._last_hitl_sig = sig
        return terminal_ok

    async def post_worker_cmd(self, run_id: str, action: str, *,
                              engine: Optional[str] = None,
                              solver_id: Optional[str] = None) -> bool:
        """Compile worker spawn/kill through the durable typed control plane.

        Only a terminal runtime ACK returns true. A finished/ghost run has no
        coordinator capable of proving the effect and is rejected.
        """
        run = self.runs.get(run_id)
        if run is None:
            return False
        if not self.is_protocol1_run(run_id, run=run):
            return False
        live = run.task is not None and not run.task.done()
        if not live:
            return False
        if action == "spawn":
            result = await self.post_control(run_id, {
                "action": "spawn_worker",
                "target": "global",
                "payload": {"engine": str(engine or "")},
            })
        elif action == "kill" and solver_id:
            result = await self.post_control(run_id, {
                "action": "cancel_worker",
                "target": f"worker:{solver_id}",
                "payload": {"worker_id": solver_id},
            })
        else:
            return False
        if run.control_actor is None:
            return False
        await run.control_actor.join()
        receipt = run.control_journal.latest_effect(
            str(result.get("command_id") or ""))
        return bool(receipt is not None
                    and receipt.state is EffectState.EFFECT_OBSERVED)

    async def resolve(self, run_id: str, body: dict[str, Any] | None = None) -> bool:
        """Fence and launch an explicit continuation generation."""
        async with self._lifecycle_lock:
            run = self.runs.get(run_id)
            if (run is None or self._shutting_down
                    or run_id in self._closing_runs
                    or run_id in self._launching_runs):
                return False
            if not self.is_protocol1_run(run_id, run=run):
                return False
            if run.task is not None and not run.task.done():
                return False
            self._launching_runs.add(run_id)
        try:
            return await self._resolve_launching(run_id, run, body)
        finally:
            async with self._lifecycle_lock:
                self._launching_runs.discard(run_id)

    async def _resolve_launching(
        self, run_id: str, run: Run, body: dict[str, Any] | None = None,
    ) -> bool:
        """"继续做题" — relaunch the FULL coordinator swarm on a finished run.

        Unlike a standby (one cold-started worker resuming the winner's session to
        answer a follow-up), this reopens the run and re-runs the real Swarm:
        bootstrap workers + reason/explore scaling, reusing the SAME workspace_dir
        so the persisted shared_graph (verified facts / dead-ends) carries straight
        over — the swarm builds ON the prior evidence instead of from scratch.

        The challenge is reconstructed from coordinator-owned continuation state,
        falling back to durable lifecycle events and rail metadata. Caller-supplied
        `body` fields win
        (e.g. an operator hint folded into the description, a new target)."""
        if run.runtime_incomplete and not await self._settle_incomplete_runtime(
                run, timeout=self._standby_cancel_timeout()):
            LOG.error(
                "refusing to resolve %s: main runtime owner is still unsettled",
                run_id)
            return False
        if run.task is not None and not run.task.done():
            return False  # already live — nothing to relaunch (use HITL instead)
        if self._standby_busy(run):
            # A resumed winner and a fresh coordinator may share session/workspace/
            # container state. Resolve is allowed only after the real standby runtime
            # (not merely its asyncio wrapper) crosses the exit fence.
            if not await self._settle_standby_runtime(
                    run, timeout=self._standby_cancel_timeout()):
                LOG.error(
                    "refusing to resolve %s: standby runtime exit is unconfirmed",
                    run_id)
                return False

        if not await self._drain_control_before_launch(run_id, run):
            return False

        continuation = self.load_winner_continuation(run_id)
        ch = continuation.get("challenge") or {}
        if not ch:
            try:
                async for ev in run.store.replay(run_id):
                    if ev.event_type in {
                        EventType.RUN_PREPARING, EventType.RUN_STARTED,
                    }:
                        ch = (ev.payload or {}).get("challenge") or {}
                        break
            except Exception:
                ch = {}
        if not ch:  # degrade to rail metadata
            ch = {"name": run.name or run_id, "category": run.category or "web",
                  "expected_flags": run.expected_flags,
                  "multi_flag": run.multi_flag}
        merged = {"challenge": ch, **(body or {})}
        if body and body.get("challenge"):
            merged["challenge"] = {**ch, **body["challenge"]}
        # "继续做题" 跳过 race-scout 竞速层：竞速是"从空图并行单发初探"，只在冷启动有意义。
        # resolve 复用同一个 workspace_dir，shared_graph 已满是 verified facts / dead-ends,
        # 应直接进主协调器循环(规划/派发)在已有证据上续做,而不是再竞速一轮
        # 从头探(浪费一轮 + 把已死方向重提)。操作者显式传 race_scout 仍可覆盖。
        # cold_start=False 是 run-75379 BUG④ 的显式信号：协调器内部以此为不变量直接跳过竞速
        # (race_scout=False 现为冗余保险)。即便某条复跑路径忘了传，Swarm 还有图状态兜底。
        merged.setdefault("race_scout", False)
        merged.setdefault("cold_start", False)

        from apps.web.drivers import build_driver
        try:
            driver = build_driver(merged, mgr=self)
        except Exception:
            LOG.exception("failed to build resolve driver for %s", run_id)
            return False

        # Destructive/visible commit: revalidate the exact Run and shutdown fence,
        # then reopen control state, bus, and execution owner as one admission
        # transaction. Holding the lifecycle lock across the replayable bus emit is
        # acceptable; it is a short local sink operation and prevents shutdown from
        # observing a half-reopened generation.
        async with self._lifecycle_lock:
            if (self._shutting_down or self.runs.get(run_id) is not run
                    or run_id in self._closing_runs
                    or (run.task is not None and not run.task.done())):
                return False
            try:
                _actor, control_journal, _secrets = self._ensure_control(run)
                state = control_journal.reopen_state(reason="operator resolve")
                run.control_generation = state.generation
            except Exception:
                LOG.exception("failed to reopen control epoch for %s", run_id)
                return False
            self._fresh_bus(run)
            self._retire_hitl_epoch(run, terminal=False)
            self._retire_worker_command_epoch(run)
            run.finished = False
            run.solved = False
            run.paused = False
            await run.bus.emit(Event(
                event_type=EventType.RUN_REOPENED, run_id=run_id,
                payload={
                    "reason": "resolve",
                    "execution_generation": run.execution_generation + 1,
                    "control_generation": run.control_generation,
                }))
            self._launch_generation(run, driver)
            return True

    def _fresh_bus(self, run: Run) -> None:
        """Replace a run's CLOSED bus with a live one (same sinks) so a standby
        worker's events reach a freshly-opened SSE stream. After the main run
        ended, run.bus was close()d — its subscribers got the end sentinel and the
        browser's EventSource reconnected, but the closed bus won't fan out to new
        subscribers. A new bus, re-wired to the SessionStore + rail meta sinks,
        keeps the durable JSONL append-only and the rail metadata fresh."""
        durable_seq = run.store.last_stream_seq(run.run_id)
        self._bump_bus_seq(run.bus, durable_seq)
        if not getattr(run.bus, "_closed", False):
            return  # still open (live run) — keep it
        new_bus = EventBus()
        new_bus.add_filter(self._generation_filter_for(run))
        new_bus.add_sink(run.store.sink)
        new_bus.add_sink(self._meta_sink_for(run))
        # carry the seq forward so SSE Last-Event-ID continuity holds across runs
        self._bump_bus_seq(new_bus, max(getattr(run.bus, "_seq", 0), durable_seq))
        run.bus = new_bus
        run.cost.bus = new_bus  # cost updates emit onto the live bus too

    def _ensure_standby(self, run_id: str, cmd: dict[str, Any]) -> bool:
        """Spin up a standby worker to serve `cmd`, unless one is already running
        (serialized — one standby per run). Fire-and-forget; events stream live."""
        run = self.runs.get(run_id)
        if run is None:
            return False
        if (self._shutting_down or run_id in self._closing_runs
                or run_id in self._launching_runs):
            return False
        if self._standby_busy(run):
            return False  # a standby is already serving this run — don't pile on
        action = str(cmd.get("action") or "").lower()
        if action in {"ask", "writeup"}:
            followup_id = str(
                cmd.get("followup_id")
                or cmd.get("command_id")
                or uuid.uuid4().hex
            )
            cmd["followup_id"] = followup_id
            run.active_followups.add(followup_id)
        # A prior driver clears these only after the runtime-exit fence. Clear stale
        # registrations defensively before publishing the next worker instance.
        run.standby_cancel = None
        run.standby_runtime_exited = None
        run.standby_wait_runtime_exit = None
        cleanup_task = run.standby_runtime_cleanup_task
        if cleanup_task is not None and not cleanup_task.done():
            cleanup_task.cancel()
        run.standby_runtime_cleanup_task = None
        if bool(getattr(run.bus, "_closed", False)):
            self._fresh_bus(run)
        from apps.web.drivers import build_standby_driver
        driver = build_standby_driver(cmd, mgr=self)

        async def _go() -> None:
            followup_terminal = False

            async def _note_followup_terminal(ev: Event) -> None:
                nonlocal followup_terminal
                if ev.event_type not in {
                    EventType.FOLLOWUP_COMPLETED,
                    EventType.FOLLOWUP_FAILED,
                }:
                    return
                wanted = str(cmd.get("followup_id") or "")
                event_id = str(ev.payload.get("followup_id") or "")
                if not wanted or event_id == wanted:
                    followup_terminal = True

            async def _emit_followup_failed(detail: str) -> None:
                nonlocal followup_terminal
                if action not in {"ask", "writeup"} or followup_terminal:
                    return
                try:
                    if bool(getattr(run.bus, "_closed", False)):
                        self._fresh_bus(run)
                    await run.bus.emit(Event(
                        event_type=EventType.FOLLOWUP_FAILED,
                        run_id=run_id,
                        payload={
                            "followup_id": cmd.get("followup_id"),
                            "kind": action,
                            "detail": detail,
                        },
                    ))
                    followup_terminal = True
                except Exception:
                    pass

            run.bus.add_sink(_note_followup_terminal)
            try:
                LOG.info("standby worker starting for %s action=%s",
                         run_id, cmd.get("action"))
                await driver(run)
                LOG.info("standby worker finished for %s action=%s",
                         run_id, cmd.get("action"))
            except asyncio.CancelledError:
                await _emit_followup_failed("后续操作已取消")
                raise
            except Exception as exc:
                detail = _safe_exception_detail("standby worker failed", exc)
                # Do not log the traceback here: exception messages from worker
                # boundaries may contain materialised operator secrets.
                LOG.error("standby worker failed for %s action=%s error_type=%s",
                          run_id, cmd.get("action"), type(exc).__name__)
                try:
                    if action in {"ask", "writeup"}:
                        await _emit_followup_failed(detail)
                    else:
                        await run.bus.emit(Event(
                            event_type=EventType.HITL_REQUEST,
                            run_id=run_id,
                            payload={
                                "target": cmd.get("target") or "global",
                                "source": "standby",
                                "action": cmd.get("action"),
                                "need": detail,
                                "text": detail,
                            },
                        ))
                except Exception:
                    pass
            finally:
                run.bus.remove_sink(_note_followup_terminal)
                if action in {"ask", "writeup"} and not followup_terminal:
                    await _emit_followup_failed("后续操作已中断")
                # Do not close the bus; retain the completed task as an observable
                # receipt. `_ensure_standby` checks `.done()` and replaces it on the
                # next command, so this does not block subsequent follow-ups.
                cancel_boundary = run.standby_cancel
                if callable(cancel_boundary):
                    try:
                        cancel_result = cancel_boundary()
                        if inspect.isawaitable(cancel_result):
                            await cancel_result
                    except Exception as exc:
                        # Runtime callbacks may embed materialised prompt/credential
                        # values in exception messages. Log only the local type.
                        LOG.error(
                            "standby final cancel boundary failed for %s "
                            "error_type=%s",
                            run_id, type(exc).__name__,
                        )
                delivery_ack = cmd.get("_standby_delivery_ack")
                if isinstance(delivery_ack, asyncio.Future) and not delivery_ack.done():
                    # The CLI process-start hook can run on a worker thread and
                    # publishes its positive ACK with call_soon_threadsafe().  If a
                    # very short-lived worker returns in the same tick, give that
                    # already-queued callback one chance to land before recording a
                    # negative pre-start outcome here.
                    await asyncio.sleep(0)
                if isinstance(delivery_ack, asyncio.Future) and not delivery_ack.done():
                    delivery_ack.set_result(False)
                owner = str(cmd.get("_control_context_owner") or "")
                reservations = [
                    (str(context_id), str(reservation_id))
                    for context_id, reservation_id in list(
                        cmd.get("_control_context_reservations") or [])
                ]
                self._ensure_standby_context_cleanup(
                    run, owner=owner, reservations=reservations)
                followup_id = str(cmd.get("followup_id") or "")
                if followup_id:
                    run.active_followups.discard(followup_id)

        run.standby_task = asyncio.create_task(_go())
        return True

    def _meta_sink_for(self, run: Run):
        """The rail-metadata sink bound to a specific Run (used when rebuilding a
        fresh bus). Mirrors the inline _meta_sink in create()."""
        async def _meta_sink(ev: Event) -> None:
            self._seq += 1
            run.updated_seq = self._seq
            run.updated_at = ev.ts
            if ev.event_type is EventType.HITL_REQUEST:
                self._record_decision_request(run, ev)
            if _apply_operator_meta(run, ev):
                return
            if ev.event_type in {EventType.RUN_PREPARING, EventType.RUN_STARTED}:
                ch = ev.payload.get("challenge", {}) or {}
                run.started = True
                if ch.get("name"):
                    run.name = ch["name"]
                run.category = ch.get("category", run.category) or run.category
                if ch.get("expected_flags"):
                    run.expected_flags = int(ch["expected_flags"])
                if "multi_flag" in ch:
                    run.multi_flag = bool(ch["multi_flag"])
            elif ev.event_type is EventType.RUN_REOPENED:
                run.finished = False
                run.solved = False
                run.paused = False
                if ev.payload.get("reason") == "resolve":
                    return
                run.invalidate_flag(ev.payload.get("flag"))
            elif ev.event_type is EventType.RUN_FINISHED:
                run.finished = True
                run.paused = False
                run.awaiting_help = False
                run.help_text = ""
                run.pending_help.clear()
                incoming_flags = ev.payload.get("flags") or ev.payload.get("flag")
                had_flag_payload = bool(incoming_flags)
                valid_incoming = run.valid_incoming_flags(incoming_flags)
                run.merge_flags(incoming_flags)
                if bool(ev.payload.get("solved")):
                    run.solved = bool(valid_incoming) if had_flag_payload else True
                if ev.payload.get("expected_flags"):
                    run.expected_flags = int(ev.payload["expected_flags"])
                if "multi_flag" in ev.payload:
                    run.multi_flag = bool(ev.payload["multi_flag"])
            else:
                _apply_blackboard_meta(run, ev)
        return _meta_sink

    async def shutdown(self) -> None:
        """Cancel every live task on server shutdown so no swarm/standby coroutine —
        and its shelled CLI subprocess group — survives as a budget-eating zombie.
        Cancels BOTH run.task AND standby_task (the latter was leaking: a standby
        worker spun up to answer a post-solve follow-up kept running). The titler is a
        detached create_task with no stored handle, so it can't be cancelled here; it
        is short-lived and self-terminates."""
        # Publish the admission fence before the first snapshot/await. It remains
        # latched even when bounded cleanup reports incomplete; callers may retry
        # shutdown, but no new main or standby generation can race into the gap.
        async with self._lifecycle_lock:
            self._shutting_down = True
        pending: dict[asyncio.Task, str] = {}
        live_standbys = [
            run for run in list(self.runs.values())
            if self._standby_busy(run)
        ]
        main_unsettled: set[str] = set()
        standby_unsettled: set[str] = set()
        task_unsettled: set[str] = set()
        live_main_owners = [
            run for run in list(self.runs.values()) if run.runtime_incomplete
        ]
        if live_main_owners:
            main_results = await asyncio.gather(*(
                self._settle_incomplete_runtime(
                    run, timeout=self._standby_cancel_timeout())
                for run in live_main_owners
            ), return_exceptions=True)
            main_unsettled.update(
                run.run_id for run, result in zip(
                    live_main_owners, main_results)
                if result is not True
            )
        if live_standbys:
            results = await asyncio.gather(*(
                self._settle_standby_runtime(
                    run, timeout=self._standby_cancel_timeout())
                for run in live_standbys
            ), return_exceptions=True)
            standby_unsettled.update({
                run.run_id for run, result in zip(live_standbys, results)
                if result is not True
            })
        for run in list(self.runs.values()):
            if run.run_id in main_unsettled or run.run_id in standby_unsettled:
                # The bounded runtime settler/reaper already owns cancellation.
                # A second raw cancel+gather can hang forever when the wrapper
                # suppresses CancelledError and would discard the retained owner.
                continue
            for t in (run.task, run.standby_task, run.title_task):
                if t is not None and not t.done():
                    t.cancel()
                    pending[t] = run.run_id
        if pending:
            done, still_live = await asyncio.wait(
                tuple(pending), timeout=self._standby_cancel_timeout())
            if done:
                await asyncio.gather(*done, return_exceptions=True)
            task_unsettled.update(pending[task] for task in still_live)
        # A driver may only learn that its subprocess/container survived while its
        # cancelled wrapper is unwinding.  Such ownership transfer happens after
        # the pre-cancel snapshot above, so settle/rescan before closing control
        # state.  Discard a prior timeout when the autonomous reaper has since
        # proved exit; preserve every still-unsettled owner.
        post_cancel_main = [
            run for run in list(self.runs.values())
            if run.runtime_incomplete or run.run_id in main_unsettled
        ]
        if post_cancel_main:
            post_results = await asyncio.gather(*(
                self._settle_incomplete_runtime(
                    run, timeout=self._standby_cancel_timeout())
                for run in post_cancel_main
            ), return_exceptions=True)
            for run, result in zip(post_cancel_main, post_results):
                if result is True:
                    main_unsettled.discard(run.run_id)
                else:
                    main_unsettled.add(run.run_id)
        unsettled = main_unsettled | standby_unsettled | task_unsettled
        for run in list(self.runs.values()):
            if run.run_id in unsettled:
                # Keep its journal/actor/cleanup callbacks owned and retryable. The
                # method will fail loudly below instead of pretending shutdown was
                # clean while its kill boundary remains live.
                continue
            if run.control_actor is not None:
                try:
                    await run.control_actor.close()
                except Exception:
                    LOG.exception("failed to close control actor for %s", run.run_id)
            if run.control_journal is not None:
                try:
                    run.control_journal.close()
                except Exception:
                    LOG.exception("failed to close control journal for %s", run.run_id)
        if unsettled:
            joined = ", ".join(sorted(unsettled))
            raise RuntimeError(
                f"runtime owner exit unconfirmed; shutdown incomplete for: {joined}")
