"""RunManager — the web/TUI-facing handle to live solve runs."""

from __future__ import annotations

from apps.web import run_bindings as _run_bindings
from apps.web import run_control as _run_control
from apps.web import run_lifecycle as _run_lifecycle
from apps.web import run_recovery as _run_recovery
from apps.web import run_retention as _run_retention
from apps.web import run_standby as _run_standby
from apps.web.run_state import (  # noqa: F401
    LOG, Run, BoundRunConflictError, BoundRunStore, Driver,
    BOUND_RUN_ID_PREFIX, _safe_exception_detail, _runtime_error_id,
    _apply_blackboard_meta, _apply_operator_meta,
)

import asyncio
import os
import stat
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from apps.web.run_meta import FolderStore, RunMetaStore, RunSummaryStore
from apps.web.storage_layout import StorageLayout
from apps.web.worker_config import WorkerConfigStore
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.core.path_ids import encode_run_id
from muteki.core.session_store import SessionStore

class RunManager:
    _STANDBY_ACTIONS = {"ask", "hint", "mark_false", "writeup", "redirect", "focus"}
    _OFFLINE_CONTROL_ACTIONS = {"clear_standing", "reset_guidance"}
    def __init__(self, *, sessions_root: "str | Path | None" = None,
                 state_root: "str | Path | None" = None,
                 control_root: "str | Path | None" = None) -> None:
        self.storage = StorageLayout.resolve(
            sessions_root=sessions_root,
            state_root=state_root,
            control_root=control_root,
        )
        self.storage.prepare()
        # sessions contains only per-Run operator-visible files. Durable service
        # state, event logs, credentials, and coordinator authority live outside
        # this worker-visible tree.
        self.sessions_root = self.storage.sessions_root
        self.state_root = self.storage.state_root
        self.event_root = self.storage.events_root
        self.runtime_state_root = self.storage.runtime_root
        from muteki.core.usage import UsageStore
        self.usage = UsageStore(self.event_root)
        self.control_root = self.storage.control_root
        self.control_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        root_info = self.control_root.lstat()
        if not stat.S_ISDIR(root_info.st_mode) or stat.S_ISLNK(root_info.st_mode):
            raise ValueError("coordinator control root must be a real directory")
        os.chmod(self.control_root, 0o700)
        # Durable graph state is coordinator-owned too. Keep it with the private
        # control state unless a dedicated graph root is configured.
        self.graph_root = self.storage.graph_root
        self.graph_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(self.graph_root, 0o700)
        self.account_projection_root = (
            self.runtime_state_root / "account-projections"
        )
        self.container_bootstrap_root = self.runtime_state_root / "rcp"
        self.account_projection_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.container_bootstrap_root.mkdir(mode=0o700, parents=True, exist_ok=True)
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
        self.meta = RunMetaStore(root=self.state_root)
        self.run_summaries = RunSummaryStore(root=self.state_root)
        # CORE-04: binding key → run_id 的幂等索引，重启后可恢复。
        self.bound_runs = BoundRunStore(root=self.state_root)
        # ensure_bound_run 的查-建必须整体原子，否则并发重试会各建一份 Run。
        self._bound_run_lock = asyncio.Lock()
        # operator-created rail folders (id → name); runs reference one via meta.
        self.folders = FolderStore(root=self.state_root)
        # default worker-roster config (which engines launch per challenge); the
        # dispatch path falls back to this when a request doesn't say otherwise.
        self.worker_config = WorkerConfigStore(root=self.state_root)
        # Product modules may subscribe to the same admitted, durable Run event
        # stream.  The list is kept by RunManager so re-opened buses retain these
        # projections; modules never need to patch an individual Swarm instance.
        self._product_event_sinks: list[
            Callable[[Event], Awaitable[None]]
        ] = []
        self._rehydrate()
        self._recover_interrupted_followups(
            SessionStore(root=self.event_root),
            summaries=self._startup_summaries,
        )
        del self._startup_summaries


    def add_product_event_sink(
        self, sink: Callable[[Event], Awaitable[None]]
    ) -> None:
        """Attach one product projection to existing and future Run buses."""
        if sink in self._product_event_sinks:
            return
        self._product_event_sinks.append(sink)
        for run in self.runs.values():
            run.bus.add_sink(sink)


    def remove_product_event_sink(
        self, sink: Callable[[Event], Awaitable[None]]
    ) -> bool:
        """Detach a product projection from every current Run bus."""
        try:
            self._product_event_sinks.remove(sink)
        except ValueError:
            return False
        for run in self.runs.values():
            run.bus.remove_sink(sink)
        return True


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
                EventType.PROGRESS_BRIEF,
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


    def _apply_meta(self, run: "Run") -> None:
        """Overlay persisted operator metadata (pin/archive/rename) onto a run."""
        m = self.meta.get(run.run_id)
        run.pinned = m["pinned"]
        run.pinned_at = m["pinned_at"]
        run.archived = m["archived"]
        run.custom_name = m["custom_name"]
        run.folder_id = m["folder_id"]
        run.sort_order = m["order"]


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
        store = store or SessionStore(root=self.event_root)
        self._bump_bus_seq(bus, store.last_stream_seq(run_id))

RunManager._bound_run_id_taken = _run_bindings._bound_run_id_taken
RunManager._delete_artifacts = _run_bindings._delete_artifacts
RunManager._delete_owned_run = _run_bindings._delete_owned_run
RunManager._mint_bound_run_id = _run_bindings._mint_bound_run_id
RunManager.create = _run_bindings.create
RunManager.create_folder = _run_bindings.create_folder
RunManager.create_new = _run_bindings.create_new
RunManager.delete = _run_bindings.delete
RunManager.delete_folder = _run_bindings.delete_folder
RunManager.ensure_bound_run = _run_bindings.ensure_bound_run
RunManager.get = _run_bindings.get
RunManager.list_folders = _run_bindings.list_folders
RunManager.list_runs = _run_bindings.list_runs
RunManager.open_workspace = _run_bindings.open_workspace
RunManager.rename = _run_bindings.rename
RunManager.set_archived = _run_bindings.set_archived
RunManager.set_folder = _run_bindings.set_folder
RunManager.set_order = _run_bindings.set_order
RunManager.set_pinned = _run_bindings.set_pinned
RunManager.update_folder = _run_bindings.update_folder
RunManager.uploads_dir = _run_bindings.uploads_dir
RunManager.workspace_dir = _run_bindings.workspace_dir
RunManager.prepare_shared_workspace = _run_bindings.prepare_shared_workspace
RunManager.graph_dir = _run_bindings.graph_dir
RunManager._load_profile_readiness = _run_recovery._load_profile_readiness
RunManager._profile_readiness_path = _run_recovery._profile_readiness_path
RunManager._recover_interrupted_followups = _run_recovery._recover_interrupted_followups
RunManager._rehydrate = _run_recovery._rehydrate
RunManager._winner_continuation_path = _run_recovery._winner_continuation_path
RunManager.coordinator_control_dir = _run_recovery.coordinator_control_dir
RunManager.load_winner_continuation = _run_recovery.load_winner_continuation
RunManager.persist_run_launch_limits = _run_recovery.persist_run_launch_limits
RunManager.load_run_launch_limits = _run_recovery.load_run_launch_limits
RunManager.load_run_pentest_goal = _run_recovery.load_run_pentest_goal
RunManager.persist_profile_readiness = _run_recovery.persist_profile_readiness
RunManager.persist_winner_continuation = _run_recovery.persist_winner_continuation
RunManager.update_winner_continuation_flags = _run_recovery.update_winner_continuation_flags
RunManager._last_activity = _run_retention._last_activity
RunManager.retention_loop = _run_retention.retention_loop
RunManager.retention_sweep = _run_retention.retention_sweep
RunManager._control_epoch_drain_timeout = _run_lifecycle._control_epoch_drain_timeout
RunManager._decision_request_from_event = _run_control._decision_request_from_event
RunManager._drain_control_before_launch = _run_lifecycle._drain_control_before_launch
RunManager._ensure_control = _run_control._ensure_control
RunManager._launch_generation = _run_lifecycle._launch_generation
RunManager._post_control_serialized = _run_control._post_control_serialized
RunManager._reconcile_decision_requests = _run_control._reconcile_decision_requests
RunManager._record_decision_request = _run_control._record_decision_request
RunManager._resolve_launching = _run_lifecycle._resolve_launching
RunManager._retire_hitl_epoch = _run_lifecycle._retire_hitl_epoch
RunManager._retire_worker_command_epoch = _run_lifecycle._retire_worker_command_epoch
RunManager.control_receipt = _run_control.control_receipt
RunManager.has_control_command = _run_control.has_control_command
RunManager.post_control = _run_control.post_control
RunManager.post_hitl = _run_control.post_hitl
RunManager.post_worker_cmd = _run_control.post_worker_cmd
RunManager.resolve = _run_lifecycle.resolve
RunManager.start = _run_lifecycle.start
RunManager._cancel_standby = _run_standby._cancel_standby
RunManager._ensure_standby = _run_standby._ensure_standby
RunManager._ensure_standby_context_cleanup = _run_standby._ensure_standby_context_cleanup
RunManager._fresh_bus = _run_standby._fresh_bus
RunManager._main_runtime_cancel_timeout = _run_standby._main_runtime_cancel_timeout
RunManager._meta_sink_for = _run_standby._meta_sink_for
RunManager._register_standby_winner = _run_standby._register_standby_winner
RunManager._settle_incomplete_runtime = _run_standby._settle_incomplete_runtime
RunManager._settle_standby_runtime = _run_standby._settle_standby_runtime
RunManager._standby_busy = _run_standby._standby_busy
RunManager._standby_cancel_timeout = _run_standby._standby_cancel_timeout
RunManager._standby_runtime_status = _run_standby._standby_runtime_status
RunManager._standby_scope_matches_winner = _run_standby._standby_scope_matches_winner
RunManager.shutdown = _run_standby.shutdown
