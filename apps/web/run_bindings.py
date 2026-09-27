"""Run listing, folders, create, and bound-run ids. Moved from run_manager.py."""

from __future__ import annotations

import asyncio
import os
import shutil
import time
import uuid
from pathlib import Path
from typing import Any, Optional

from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType
from muteki.core.session_store import SessionStore
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.runs import BoundRunRequest

from apps.web.run_state import (  # noqa: F401
    LOG, Run, BoundRunConflictError, BoundRunStore, Driver,
    BOUND_RUN_ID_PREFIX, _safe_exception_detail, _runtime_error_id,
    _apply_blackboard_meta, _apply_operator_meta,
)
from apps.web.run_progress import RunProgressPublisher

def get(self, run_id: str) -> Optional[Run]:
    return self.runs.get(run_id)


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
    pending = [t for t in (
        run.task, run.standby_task, run.recovery_task, run.title_task,
    )
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
    if run.progress_publisher is not None:
        run.progress_publisher.close()
    await run.bus.close()
    self._delete_artifacts(run_id)
    return True


def _delete_artifacts(self, run_id: str) -> None:
    self.meta.forget(run_id)
    self.run_summaries.forget(run_id)
    safe = self._safe_run_id(run_id)
    shared = self.storage.shared_workspace(run_id)
    logical = self.storage.workspace(run_id)
    if logical.is_symlink() and logical.resolve() == shared.resolve():
        logical.unlink()
        if shared.is_dir() and not shared.is_symlink():
            shutil.rmtree(shared, ignore_errors=True)
    jsonl = self.event_root / f"{safe}.jsonl"
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
    shutil.rmtree(self.graph_root / safe, ignore_errors=True)


def workspace_dir(self, run_id: str) -> Path:
    """Return the persistent ``sessions/{id}/workspace`` directory."""
    d = self.storage.workspace(run_id)
    d.mkdir(parents=True, exist_ok=True)
    return d


def prepare_shared_workspace(self, run_id: str) -> Path:
    """Place a fresh Run in the dedicated trusted shared pool."""
    logical = self.storage.workspace(run_id)
    target = self.storage.shared_workspace(run_id)
    pool = self.storage.shared_worker_mount()
    if pool.is_symlink() or target.is_symlink():
        raise RuntimeError("shared worker workspace cannot use an existing symlink")
    if logical.is_symlink():
        if logical.resolve() != target.resolve() or not target.is_dir():
            raise RuntimeError("Run workspace points outside its shared pool slot")
        return logical
    if logical.exists():
        if not logical.is_dir() or any(logical.iterdir()):
            raise RuntimeError(
                "existing Run workspace is not empty; start a new Run to choose shared scope")
        logical.rmdir()
    logical.parent.mkdir(parents=True, exist_ok=True)
    pool.mkdir(mode=0o700, parents=True, exist_ok=True)
    if target.exists() and (not target.is_dir() or any(target.iterdir())):
        raise RuntimeError("shared pool slot already contains data for this Run")
    target.mkdir(mode=0o700, exist_ok=True)
    logical.symlink_to(target, target_is_directory=True)
    return logical


def graph_dir(self, run_id: str) -> Path:
    """Coordinator-private graph directory for a run.

    The graph root stays outside every worker container mount and is reachable
    only through the role-scoped host proxy.
    """
    private = self.storage.run_graph(run_id)
    private.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(private, 0o700)
    return private


def open_workspace(self, run_id: str) -> bool:
    """Open the run's workspace dir in the host file manager (operator-local —
    the deck runs in a browser, so a backend opener is the only way to truly
    reveal Finder/Explorer). Best-effort; False if it can't open."""
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
    as _delete_artifacts so a hostile run_id can't escape sessions/.
    """
    d = self.storage.uploads(run_id)
    d.mkdir(parents=True, exist_ok=True)
    return d


def create(self, run_id: str, *, stream_seq: int | None = None) -> Run:
    if run_id in self.runs:
        return self.runs[run_id]
    bus = EventBus()
    store = SessionStore(root=self.event_root)
    if stream_seq is None:
        self._sync_bus_seq(bus, store=store, run_id=run_id)
    else:
        self._bump_bus_seq(bus, stream_seq)
    self._seq += 1
    run = Run(
        run_id=run_id, bus=bus, cost=CostController(bus=bus), store=store,
        created_seq=self._seq,
    )
    def record_usage(raw, **scope):
        generation = scope.pop("generation", None)
        return self.usage.record(raw, run_id=run_id, generation=generation if generation is not None else run.execution_generation,
                                 workspace_kind="single-security-task", **scope)
    run.cost.usage_sink = record_usage
    run.profile_readiness = self._load_profile_readiness(run_id)
    run.progress_publisher = RunProgressPublisher(run)
    bus.add_filter(self._generation_filter_for(run))
    bus.add_sink(store.sink, required=True)
    for sink in self._product_event_sinks:
        bus.add_sink(sink)
    bus.add_sink(run.progress_publisher.observe)
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
            self.run_summaries.update(run, ev)
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
            run.merge_flags(ch.get("initial_flags") or [])
            if "multi_flag" in ch:
                run.multi_flag = bool(ch["multi_flag"])
        elif ev.event_type is EventType.RUN_TITLED:
            # Display-only rail labels. Do not clobber an operator-supplied name
            # or category; Swarm never reads these fields from this event.
            title = ev.payload.get("title") or ""
            if title and not run.name:
                run.name = title
            category = ev.payload.get("category") or ""
            if category and not run.category:
                run.category = category
        elif ev.event_type is EventType.RUN_REOPENED:
            # The run is solving again. Resolve/continue keeps all prior flags
            # visible; false-positive payloads carry the one invalid flag to
            # drop. Legacy false-positive payloads with no flag still clear all.
            run.finished = False
            run.solved = False
            run.paused = False
            if ev.payload.get("reason") == "resolve":
                self.run_summaries.update(run, ev)
                return
            run.invalidate_flag(ev.payload.get("flag"))
        elif ev.event_type is EventType.FLAG_ACCEPTED:
            # Historical replay tolerance: old session logs may carry this event.
            # Never synthesize solved/finished/progress here.
            run.merge_flags(ev.payload.get("flag"))
        elif ev.event_type is EventType.RUN_FINISHED:
            run.finished = True
            run.paused = False  # a finished run is never "paused"
            run.awaiting_help = False  # finished → no outstanding ask
            run.help_text = ""
            run.pending_help.clear()
            incoming_flags = (
                ev.payload.get("flags")
                if "flags" in ev.payload else ev.payload.get("flag")
            )
            incoming_values = (
                incoming_flags if isinstance(incoming_flags, list)
                else [incoming_flags]
            )
            had_flag_payload = any(value is not None for value in incoming_values)
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

        self.run_summaries.update(run, ev)

    bus.add_sink(_meta_sink)
    self.runs[run_id] = run
    self._apply_meta(run)
    return run


def create_new(self) -> Run:
    """Mint a run under a fresh, never-reused id (for '+ New solve')."""
    self._seq += 1
    run_id = f"run-{self._seq:04d}"
    while run_id in self.runs:
        self._seq += 1
        run_id = f"run-{self._seq:04d}"
    return self.create(run_id)


def _bound_run_id_taken(self, run_id: str) -> bool:
    """run_id 是否已被 RunManager 侧占用（内存 handle、JSONL 历史或目录）。"""
    if run_id in self.runs:
        return True
    if run_id in SessionStore(root=self.event_root).list_runs():
        return True
    safe = self._safe_run_id(run_id)
    return (self.sessions_root / safe).exists()


def _mint_bound_run_id(self) -> str:
    """在 crun_ 命名空间内生成不与任何现有 Run/binding 冲突的 id。"""
    while True:
        run_id = f"{BOUND_RUN_ID_PREFIX}{uuid.uuid4().hex[:16]}"
        if (self.bound_runs.run_id_owner(run_id) is None
                and not self._bound_run_id_taken(run_id)):
            return run_id


async def ensure_bound_run(self, request: BoundRunRequest) -> Run:
    """幂等绑定入口：相同 binding key 永远返回同一 Run。

    - 相同 binding key + 相同 task_revision：返回既有 Run；``create`` 对
      同一 run_id 幂等，重启后重新打开同一 JSONL/workspace，重试不会
      生成第二份 workspace / SessionStore / SharedGraph；
    - 相同 binding key + 不同 task_revision：抛出携带 CONFLICT envelope
      的 BoundRunConflictError；
    - 调用方预分配的 run_id 必须落在 ``crun_`` 命名空间，且不得与
      RunManager 自分配 id、已有 Run 历史或其他 binding 占用的 id 冲突。
    """
    key = str(request.binding_key or "").strip()
    if not key:
        raise BoundRunConflictError(ErrorEnvelope(
            code="run.binding.invalid_key",
            message="binding_key cannot be empty",
            category=ErrorCategory.VALIDATION,
            recovery_hint="provide a stable non-empty binding_key"))
    async with self._bound_run_lock:
        record = self.bound_runs.get(key)
        if record is not None:
            if int(record.get("task_revision") or 0) != int(
                    request.task_revision):
                raise BoundRunConflictError(ErrorEnvelope(
                    code="run.binding.revision_conflict",
                    message=(
                        f"binding {key!r} is bound to task revision "
                        f"{record.get('task_revision')}, got "
                        f"{request.task_revision}"),
                    category=ErrorCategory.CONFLICT,
                    recovery_hint="use a new binding_key for the revised task",
                    detail={
                        "run_id": record.get("run_id"),
                        "bound_revision": record.get("task_revision"),
                        "requested_revision": request.task_revision,
                    }))
            return self.create(str(record["run_id"]))
        run_id = str(request.run_id or "").strip()
        if run_id:
            if not run_id.startswith(BOUND_RUN_ID_PREFIX):
                raise BoundRunConflictError(ErrorEnvelope(
                    code="run.binding.id_namespace",
                    message=(
                        f"caller-assigned run_id must start with "
                        f"{BOUND_RUN_ID_PREFIX!r}, got {run_id!r}"),
                    category=ErrorCategory.VALIDATION,
                    recovery_hint=(
                        "omit run_id or use the crun_ namespace; run-NNNN "
                        "is reserved for RunManager self-assigned ids")))
            owner = self.bound_runs.run_id_owner(run_id)
            if owner is not None and owner != key:
                raise BoundRunConflictError(ErrorEnvelope(
                    code="run.binding.id_conflict",
                    message=(
                        f"run_id {run_id!r} is already bound to "
                        f"binding {owner!r}"),
                    category=ErrorCategory.CONFLICT,
                    recovery_hint="choose a fresh crun_ run_id"))
            if owner is None and self._bound_run_id_taken(run_id):
                raise BoundRunConflictError(ErrorEnvelope(
                    code="run.binding.id_conflict",
                    message=f"run_id {run_id!r} already exists",
                    category=ErrorCategory.CONFLICT,
                    recovery_hint="choose a fresh crun_ run_id"))
        else:
            run_id = self._mint_bound_run_id()
        # 先持久化绑定再打开 Run handle：崩溃后重启仍能从索引恢复映射。
        self.bound_runs.bind(
            key, run_id=run_id, task_id=request.task_id,
            task_kind=request.task_kind,
            task_revision=request.task_revision,
            executor_id=request.executor_id)
        return self.create(run_id)
