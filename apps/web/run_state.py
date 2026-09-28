"""Run record, bound-run store, and meta helpers. Moved from run_manager.py."""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from muteki.control import (
    ControlActor,
    InMemoryWorkerRegistry,
    SQLiteControlJournal,
)
from muteki.control.secrets import SecretStore
from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.core.session_store import SessionStore
from muteki.platform.contracts.errors import ErrorEnvelope

LOG = logging.getLogger("apps.web.run_manager")


def _safe_exception_detail(prefix: str, exc: BaseException) -> str:
    """Keep the concrete boundary error available for operator diagnosis."""
    message = str(exc).replace("\x00", "").strip()
    message = "".join(ch for ch in message if ch in "\n\t" or ord(ch) >= 32)
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
    # A freshly launched driver is live before the Coordinator has installed its
    # queue consumers.  Control commands submitted in that interval must wait for
    # the corresponding consumer instead of starting their ACK timeout early.
    control_ready: asyncio.Event = field(default_factory=asyncio.Event)
    worker_control_ready: asyncio.Event = field(default_factory=asyncio.Event)
    task: Optional[asyncio.Task] = None
    # post-solve standby: a short-lived worker spun up to serve a HITL command when
    # the main run is no longer live (finished, or the server restarted). Serialized
    # — one at a time per run.
    standby_task: Optional[asyncio.Task] = None
    # False-positive invalidation relaunches the full Coordinator asynchronously
    # after the control actor has committed its receipt. Keep that bridge task
    # owned by the Run so shutdown can cancel and await it deterministically.
    recovery_task: Optional[asyncio.Task] = None
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
    mode: str = "ctf"
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
    terminal_reason: str = ""
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
    # Deterministic, event-derived progress summaries for the main conversation.
    progress_publisher: Optional[Any] = None

    def merge_flags(self, flags: Any) -> None:
        """Accumulate flags from an event payload (dedup, keep order); keep the
        flag/flags[0] invariant. Accepts a list or a single string."""
        if isinstance(flags, str):
            flags = [flags]
        for f in (flags or []):
            if f in self.invalidated_flags:
                continue
            if f is not None and f not in self.flags:
                self.flags.append(f)
        if self.flags and self.flag is None:
            self.flag = self.flags[0]

    def valid_incoming_flags(self, flags: Any) -> list[str]:
        if isinstance(flags, str):
            flags = [flags]
        return [
            f for f in (flags or [])
            if f is not None and f not in self.invalidated_flags
        ]

    def invalidate_flag(self, flag: Any = None) -> None:
        """Drop a false-positive flag and remember it across graph replay."""
        if flag is not None:
            bad = str(flag)
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
            # custom_name (operator rename) wins; else the auto/challenge name.
            # Empty when neither is set — the rail renders its own placeholder, we
            # do NOT leak the bare run id as a display name.
            "name": self.custom_name or self.name,
            "category": self.category or "",
            "mode": self.mode,
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


#: ensure_bound_run 调用方预分配 run_id 的独立命名空间前缀（比赛设计 16.3）。
#: RunManager 自分配 id 使用 ``run-NNNN``，两个命名空间互不相交。
BOUND_RUN_ID_PREFIX = "crun_"


class BoundRunConflictError(ValueError):
    """ensure_bound_run 的 typed 冲突错误，携带统一错误 envelope（任务书 6.1）。"""

    def __init__(self, error: ErrorEnvelope) -> None:
        super().__init__(error.message)
        self.error = error


class BoundRunStore:
    """binding key → run_id 的持久幂等索引（任务书 CORE-04）。

    与 RunMetaStore 同一风格：小型 JSON 索引放在私有 state 根目录，每次变更
    原子重写。重启后新 RunManager 据此把同一 binding key 继续解析到同一
    Run，调用方重试不会生成第二份 workspace / SessionStore / SharedGraph。
    """

    def __init__(self, root: "str | Path") -> None:
        self.path = Path(root) / "_bound_runs.json"
        self._data: dict[str, dict[str, Any]] = {}
        self._load()

    def _load(self) -> None:
        if not self.path.exists():
            return
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
            if isinstance(raw, dict):
                self._data = {k: v for k, v in raw.items() if isinstance(v, dict)}
        except (json.JSONDecodeError, OSError):
            # 索引损坏不得拖垮启动；绑定缺失时按未绑定处理（保守失败）
            self._data = {}

    def _flush(self) -> None:
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(self._data, ensure_ascii=False, indent=0),
                       encoding="utf-8")
        tmp.replace(self.path)  # POSIX 上原子替换

    def get(self, binding_key: str) -> Optional[dict[str, Any]]:
        record = self._data.get(binding_key)
        return dict(record) if isinstance(record, dict) else None

    def run_id_owner(self, run_id: str) -> Optional[str]:
        """返回已占用该 run_id 的 binding key；未占用返回 None。"""
        for key, record in self._data.items():
            if record.get("run_id") == run_id:
                return key
        return None

    def bind(self, binding_key: str, *, run_id: str, task_id: Optional[str],
             task_kind: str, task_revision: int,
             executor_id: Optional[str]) -> dict[str, Any]:
        record = {
            "run_id": run_id,
            "task_id": task_id,
            "task_kind": task_kind,
            "task_revision": int(task_revision),
            "executor_id": executor_id,
            "created_at": time.time(),
        }
        self._data[binding_key] = record
        self._flush()
        return dict(record)



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
            # A manual resume clears a manual hold.  Outstanding HITL cards remain
            # visible through ``awaiting_help``; they only pause a run when the
            # coordinator emits its own awaiting_operator / collect_idle lifecycle
            # event.  Treating every pending card as a hold made a non-blocking
            # runtime notice immediately turn a successfully resumed Run back into
            # ``paused``.
            run.paused = False
        return True
    return False
