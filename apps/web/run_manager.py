"""RunManager — the web/TUI-facing handle to live solve runs.

The frontends are dumb subscribers (§3): they never call the solver core
directly. They ask the RunManager to start a run, then subscribe to that run's
EventBus and POST HITL commands which land in the run's HITL queue. This keeps
the event schema as the only contract between core and UI.

A "run" here is one challenge being solved (solo or by a swarm). Each gets its
own EventBus + SessionStore (durable replay) + an asyncio.Queue for inbound
human commands.
"""

from __future__ import annotations

import asyncio
import logging
import os
import re
import shutil
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from apps.web.run_meta import FolderStore, RunMetaStore
from apps.web.worker_config import WorkerConfigStore
from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType, hitl_response_payload
from muteki.core.session_store import SessionStore

LOG = logging.getLogger(__name__)


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
    finished: bool = False
    flag: Optional[str] = None
    # multi-flag: every distinct flag the run collected (dedup, discovery order).
    # `flag` stays the first for back-compat. expected_flags drives the rail/UI
    # "collected N/total" + the solved-vs-collecting distinction.
    flags: list[str] = field(default_factory=list)
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

    def merge_flags(self, flags: Any) -> None:
        """Accumulate flags from an event payload (dedup, keep order); keep the
        flag/flags[0] invariant. Accepts a list or a single string."""
        if isinstance(flags, str):
            flags = [flags]
        for f in (flags or []):
            if f and f not in self.flags:
                self.flags.append(f)
        if self.flags and not self.flag:
            self.flag = self.flags[0]

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
            "started": self.started,
            "finished": self.finished,
            "solved": self.solved,
            "paused": self.paused,
            "awaiting_help": self.awaiting_help,
            "help_text": self.help_text,
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
    elif kind in ("awaiting_operator", "collect_idle"):
        run.paused = True
    elif kind in ("operator_resumed", "operator_stopped"):
        run.paused = False


class RunManager:
    def __init__(self, *, sessions_root: "str | Path | None" = None) -> None:
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
        self.runs: dict[str, Run] = {}
        self._seq = 0
        self.meta = RunMetaStore(root=self.sessions_root)
        # operator-created rail folders (id → name); runs reference one via meta.
        self.folders = FolderStore(root=self.sessions_root)
        # default worker-roster config (which engines launch per challenge); the
        # dispatch path falls back to this when a request doesn't say otherwise.
        self.worker_config = WorkerConfigStore(root=self.sessions_root)
        self._rehydrate()

    def _apply_meta(self, run: "Run") -> None:
        """Overlay persisted operator metadata (pin/archive/rename) onto a run."""
        m = self.meta.get(run.run_id)
        run.pinned = m["pinned"]
        run.pinned_at = m["pinned_at"]
        run.archived = m["archived"]
        run.custom_name = m["custom_name"]
        run.folder_id = m["folder_id"]
        run.sort_order = m["order"]

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
            # `summary()` falls back name→run_id; treat that as "no real title" so
            # the rail renders its placeholder instead of leaking the bare id.
            run.name = "" if s.get("name") in (None, "", rid) else s["name"]
            run.category = s.get("category", "") or ""
            run.started = bool(s.get("started"))
            # a rehydrated run has NO live task (the swarm coroutine died with the
            # previous server). So a started run is necessarily finished — even if
            # its on-disk summary says finished=False because it was killed mid-run
            # before emitting RUN_FINISHED (a "ghost run": the rail would otherwise
            # spin forever with no terminal event to settle it). Force-settle here.
            run.finished = bool(s.get("finished")) or run.started
            run.solved = bool(s.get("solved"))
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
        """Hard-delete a run: cancel its task(s), drop the handle + JSONL + meta."""
        run = self.runs.pop(run_id, None)
        if run is None:
            # still scrub any orphaned on-disk artifacts / meta
            self._delete_artifacts(run_id)
            return False
        # Cancel BOTH the swarm task and any live standby worker, then AWAIT them to
        # actually unwind before we close the bus / delete artifacts. Cancelling
        # without awaiting was a use-after-free race: the cancelled coroutine could
        # still be writing to the bus or reading an upload while we closed/removed
        # them. A cancelled task re-raises CancelledError on await — return_exceptions
        # swallows it (and any other shutdown error) so delete never self-destructs.
        pending = [t for t in (run.task, run.standby_task)
                   if t is not None and not t.done()]
        for t in pending:
            t.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        await run.bus.close()
        self._delete_artifacts(run_id)
        return True

    def _delete_artifacts(self, run_id: str) -> None:
        self.meta.forget(run_id)
        safe = run_id.replace("/", "_").replace("..", "_")
        jsonl = self.sessions_root / f"{safe}.jsonl"
        try:
            jsonl.unlink(missing_ok=True)
        except OSError:
            pass
        # also drop the per-run upload dir (sessions/{safe}/) so deleting a
        # conversation doesn't orphan its uploaded challenge files on disk.
        shutil.rmtree(self.sessions_root / safe, ignore_errors=True)

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
            if meta["archived"]:
                if idle > delete_after_s:
                    await self.delete(run.run_id)
                    deleted.append(run.run_id)
                    LOG.info("retention: deleted stale archived run %s (idle %.0fs)",
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
        safe = run_id.replace("/", "_").replace("..", "_")
        d = self.sessions_root / safe / "workspace"
        d.mkdir(parents=True, exist_ok=True)
        return d

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
        as _delete_artifacts so a hostile run_id can't escape sessions/. The dir
        is a sibling of the run's {id}.jsonl log — SessionStore only globs
        *.jsonl, so a directory of the same stem never collides with rehydration.
        """
        safe = run_id.replace("/", "_").replace("..", "_")
        d = self.sessions_root / safe / "uploads"
        d.mkdir(parents=True, exist_ok=True)
        return d

    def create(self, run_id: str) -> Run:
        if run_id in self.runs:
            return self.runs[run_id]
        bus = EventBus()
        store = SessionStore(root=self.sessions_root)
        self._sync_bus_seq(bus, store=store, run_id=run_id)
        bus.add_sink(store.sink)
        self._seq += 1
        run = Run(
            run_id=run_id, bus=bus, cost=CostController(bus=bus), store=store,
            created_seq=self._seq,
        )
        # sniff run.started / run.finished off the bus to keep rail metadata fresh
        # without making the run anything but a dumb event source.
        async def _meta_sink(ev: Event) -> None:
            # any event = activity. Keep this as metadata only: the rail itself is
            # creation-ordered, otherwise concurrent live runs visually hop around.
            self._seq += 1
            run.updated_seq = self._seq
            run.updated_at = ev.ts
            if ev.event_type is EventType.RUN_STARTED:
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
            elif ev.event_type is EventType.HITL_RESPONSE:
                # reflect pause/resume into the rail status icon. The driver still
                # owns the real halt; this is just the displayed state.
                action = ev.payload.get("action")
                if action == "pause":
                    run.paused = True
                elif action == "resume":
                    run.paused = False
                # ANY operator response lowers a raised hand. This MUST live in the
                # same branch: an if/elif chain only matches ONE HITL_RESPONSE arm,
                # so a separate `elif ev.event_type is HITL_RESPONSE` below was dead
                # code and the rail showed "需要输入" forever after a hint/answer.
                run.awaiting_help = False
                run.help_text = ""
            elif ev.event_type is EventType.RUN_REOPENED:
                # The run is solving again. Resolve/continue keeps all prior flags
                # visible; false-positive payloads carry the one invalid flag to
                # drop. Legacy false-positive payloads with no flag still clear all.
                run.finished = False
                run.solved = False
                run.paused = False
                if ev.payload.get("reason") == "resolve":
                    return
                bad = ev.payload.get("flag")
                if bad and run.flags:
                    run.flags = [f for f in run.flags if f != bad]
                    run.flag = run.flags[0] if run.flags else None
                else:
                    run.flag = None
                    run.flags = []
            elif ev.event_type is EventType.HITL_REQUEST:
                # a worker raised its hand (NEED_INPUT / env_down: target crashed,
                # instance expired, missing credential…). Surface it on the summary so
                # an operator (or a 1-min poll of /api/runs) sees it WITHOUT scanning
                # JSONL — this does NOT require the run to be "paused" (the swarm may
                # still be hurling workers at the wall while one hand is up).
                run.awaiting_help = True
                run.help_text = str((ev.payload or {}).get("need")
                                     or (ev.payload or {}).get("text") or "")[:300]
            elif ev.event_type is EventType.RUN_FINISHED:
                run.finished = True
                run.paused = False  # a finished run is never "paused"
                run.awaiting_help = False  # finished → no outstanding ask
                run.help_text = ""
                run.solved = bool(ev.payload.get("solved")) or run.solved
                run.merge_flags(ev.payload.get("flags") or ev.payload.get("flag"))
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

    async def start(self, run_id: str, driver: Driver) -> Run:
        """Create the run and launch `driver` as a background task on its bus."""
        run = self.create(run_id)

        async def _go() -> None:
            failure_detail = ""
            try:
                await driver(run)
            except Exception as exc:
                failure_detail = str(exc)[:500]
            finally:
                # If the driver exited WITHOUT emitting RUN_FINISHED (cancelled
                # mid-run, or it crashed before its own terminal event), the deck
                # never gets a terminal signal and the rail spins forever (a "ghost
                # run"). _meta_sink flips run.finished=True on a real RUN_FINISHED,
                # so a still-False flag here means none was emitted — synthesize one
                # before closing the bus so every run reaches a settled state.
                if not run.finished:
                    try:
                        await run.bus.emit(Event(
                            event_type=EventType.RUN_FINISHED, run_id=run_id,
                            payload={"flag": run.flag, "flags": list(run.flags),
                                     "expected_flags": run.expected_flags,
                                     "multi_flag": run.multi_flag,
                                     "solved": run.solved,
                                     "reason": "runtime_failure",
                                     "detail": failure_detail}))
                    except Exception:
                        pass
                run.finished = True
                await run.bus.close()

        run.task = asyncio.create_task(_go())
        return run

    # actions a standby (post-solve) worker can serve. pause/resume/submit only
    # make sense against a LIVE run, so they never trigger a standby.
    _STANDBY_ACTIONS = {"ask", "hint", "mark_false", "writeup", "redirect", "focus"}

    async def post_hitl(self, run_id: str, target: str, action: str, **fields: Any) -> bool:
        """Route a human command into the run + echo it on the event stream.

        While the run is LIVE, the command flows to the running swarm via run.hitl
        (pause/resume act on the subprocess; hints reach workers). Once the run has
        FINISHED — or the server restarted and there's no live task — a follow-up
        would otherwise vanish: nothing drains run.hitl. So we COLD-START a standby
        worker (resume the winner's session) to actually respond."""
        run = self.runs.get(run_id)
        if run is None:
            return False
        # stop: gracefully END a run. A LIVE run: cancel run.task → the swarm's
        # finally-block _cancel_solver + killpg every worker, the driver's finally
        # closes the bus → the run reaches `finished` (RUN_FINISHED), JSONL + board
        # PRESERVED (unlike DELETE). A GHOST run (no live task but the deck still
        # shows "running" because its event stream ended mid-flight without a
        # terminating RUN_FINISHED — e.g. a relaunch killed when the server died,
        # run-4305): we FORCE it finished here + broadcast RUN_FINISHED so the deck
        # settles and shows the finished controls. Stop must never leave a run stuck.
        if action == "stop":
            await run.bus.emit(Event(
                event_type=EventType.HITL_RESPONSE, run_id=run_id,
                payload=hitl_response_payload(target, action, **fields)))
            if run.task is not None and not run.task.done():
                # ⑤ Route stop THROUGH the hitl queue first so the swarm's _drain_hitl
                # sets _operator_stop=True and the coordinator finalizes as
                # "operator_stop" — NOT "runtime_failure". A bare task.cancel() (the old
                # path) skipped that flag, so finalize mislabeled an operator stop as a
                # crash and parked every in-flight intent as resume noise (run-75377: 53
                # stranded intents). Give the coordinator a brief window to drain + exit
                # cleanly on its own; cancel only as a backstop if it doesn't.
                try:
                    run.hitl.put_nowait({"action": "stop", "target": target})
                except Exception:
                    pass
                for _ in range(40):  # ~4s: _drain_hitl runs each coordinator tick
                    await asyncio.sleep(0.1)
                    if run.task.done():
                        break
                if not run.task.done():
                    run.task.cancel()
            else:
                # ghost / already-dead task → settle the state ourselves.
                run.finished = True
                run.paused = False
                await run.bus.emit(Event(
                    event_type=EventType.RUN_FINISHED, run_id=run_id,
                    payload={"flag": run.flag, "flags": list(run.flags),
                             "expected_flags": run.expected_flags,
                             "multi_flag": run.multi_flag,
                             "solved": run.solved,
                             "reason": "operator_stop"}))
            return True
        # M2: drop an identical back-to-back resend (same target/action/text/url).
        # The UI has no client throttle, and an operator hammering the SAME hint at a
        # busy single-shot worker (run-0011: 11×) otherwise queues 11 items + 11
        # events + 11 downstream _drain_hitl sweeps. A genuinely new command (changed
        # text, or a different action) still goes through.
        sig = (target, action, str(fields.get("text") or fields.get("hint") or ""),
               str(fields.get("url") or fields.get("target_url") or ""),
               str(fields.get("flag") or ""))
        # `writeup` is an idempotent-looking no-arg command from the UI, but each
        # click is a real request to run a fresh post-solve standby turn. If we
        # dedupe it here, the second "生成复盘" click only echoes a duplicate
        # HITL_RESPONSE and never starts a worker, which reads as a stuck button.
        if action != "writeup" and getattr(run, "_last_hitl_sig", None) == sig:
            await run.bus.emit(Event(
                event_type=EventType.HITL_RESPONSE, run_id=run_id,
                payload=hitl_response_payload(target, action, delivery="duplicate",
                                              **fields)))
            return True
        run._last_hitl_sig = sig

        live = run.task is not None and not run.task.done()
        # M4: tell the operator WHERE the command went, so they stop re-sending a hint
        # that already landed. A non-standing hint can't steer a live single-shot
        # worker mid-turn (it's folded into the NEXT spawn), and a finished run routes
        # to a cold-start standby — both look identical without this status.
        if live:
            delivery = "queued_for_next_worker" if action in ("hint", "focus") else "applied_live"
        elif action in self._STANDBY_ACTIONS:
            delivery = "standby"
        else:
            delivery = "no_live_workers"

        await run.hitl.put({"target": target, "action": action, **fields})
        await run.bus.emit(
            Event(
                event_type=EventType.HITL_RESPONSE,
                run_id=run_id,
                payload=hitl_response_payload(target, action, delivery=delivery, **fields),
            )
        )
        if not live and action in self._STANDBY_ACTIONS:
            self._ensure_standby(run_id, {"target": target, "action": action, **fields})
        return True

    async def post_worker_cmd(self, run_id: str, action: str, *,
                              engine: Optional[str] = None,
                              solver_id: Optional[str] = None) -> bool:
        """Queue an operator worker command (spawn/kill) for the LIVE coordinator
        to drain. Only meaningful while the run is running; a finished/ghost run
        has no coordinator loop to act on it, so we reject it."""
        run = self.runs.get(run_id)
        if run is None:
            return False
        live = run.task is not None and not run.task.done()
        if not live:
            return False
        cmd: dict[str, Any] = {"action": action}
        if engine:
            cmd["engine"] = engine
        if solver_id:
            cmd["solver_id"] = solver_id
        await run.worker_cmds.put(cmd)
        return True

    async def resolve(self, run_id: str, body: dict[str, Any] | None = None) -> bool:
        """"继续做题" — relaunch the FULL coordinator swarm on a finished run.

        Unlike a standby (one cold-started worker resuming the winner's session to
        answer a follow-up), this reopens the run and re-runs the real Swarm:
        bootstrap workers + reason/explore scaling, reusing the SAME workspace_dir
        so the persisted shared_graph (verified facts / dead-ends) carries straight
        over — the swarm builds ON the prior evidence instead of from scratch.

        The challenge is reconstructed from winner.json (the durable run snapshot),
        falling back to the run's rail metadata. Caller-supplied `body` fields win
        (e.g. an operator hint folded into the description, a new target)."""
        run = self.runs.get(run_id)
        if run is None:
            return False
        if run.task is not None and not run.task.done():
            return False  # already live — nothing to relaunch (use HITL instead)

        # rebuild the challenge body from the durable winner.json snapshot.
        ch: dict[str, Any] = {}
        try:
            import json
            wp = self.workspace_dir(run_id) / "winner.json"
            if wp.exists():
                ch = (json.loads(wp.read_text()) or {}).get("challenge") or {}
        except Exception:
            ch = {}
        if not ch:
            try:
                async for ev in run.store.replay(run_id):
                    if ev.event_type is EventType.RUN_STARTED:
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

        # revive the closed bus so the relaunched swarm's events reach SSE, and
        # reopen the run state (rail flips back to running).
        self._fresh_bus(run)
        run.finished = False
        run.solved = False
        run.paused = False
        await run.bus.emit(Event(
            event_type=EventType.RUN_REOPENED, run_id=run_id,
            payload={"reason": "resolve"}))

        from apps.web.drivers import build_driver
        driver = build_driver(merged, mgr=self)

        async def _go() -> None:
            try:
                await driver(run)
            finally:
                run.finished = True
                await run.bus.close()

        run.task = asyncio.create_task(_go())
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
        new_bus.add_sink(run.store.sink)
        new_bus.add_sink(self._meta_sink_for(run))
        # carry the seq forward so SSE Last-Event-ID continuity holds across runs
        self._bump_bus_seq(new_bus, max(getattr(run.bus, "_seq", 0), durable_seq))
        run.bus = new_bus
        run.cost.bus = new_bus  # cost updates emit onto the live bus too

    def _ensure_standby(self, run_id: str, cmd: dict[str, Any]) -> None:
        """Spin up a standby worker to serve `cmd`, unless one is already running
        (serialized — one standby per run). Fire-and-forget; events stream live."""
        run = self.runs.get(run_id)
        if run is None:
            return
        if run.standby_task is not None and not run.standby_task.done():
            return  # a standby is already serving this run — don't pile on
        self._fresh_bus(run)
        from apps.web.drivers import build_standby_driver
        driver = build_standby_driver(cmd, mgr=self)

        async def _go() -> None:
            try:
                LOG.info("standby worker starting for %s action=%s",
                         run_id, cmd.get("action"))
                await driver(run)
                LOG.info("standby worker finished for %s action=%s",
                         run_id, cmd.get("action"))
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                detail = str(exc)[:500]
                LOG.exception("standby worker failed for %s action=%s",
                              run_id, cmd.get("action"))
                try:
                    await run.bus.emit(Event(
                        event_type=EventType.HITL_REQUEST,
                        run_id=run_id,
                        payload={
                            "target": cmd.get("target") or "global",
                            "source": "standby",
                            "action": cmd.get("action"),
                            "need": f"standby worker failed: {detail}",
                            "text": f"standby worker failed: {detail}",
                        },
                    ))
                except Exception:
                    pass
            finally:
                # do NOT close the bus — keep the run reachable for more follow-ups.
                run.standby_task = None

        run.standby_task = asyncio.create_task(_go())

    def _meta_sink_for(self, run: Run):
        """The rail-metadata sink bound to a specific Run (used when rebuilding a
        fresh bus). Mirrors the inline _meta_sink in create()."""
        async def _meta_sink(ev: Event) -> None:
            self._seq += 1
            run.updated_seq = self._seq
            run.updated_at = ev.ts
            if ev.event_type is EventType.RUN_REOPENED:
                run.finished = False
                run.solved = False
                run.paused = False
                if ev.payload.get("reason") == "resolve":
                    return
                bad = ev.payload.get("flag")
                if bad and run.flags:
                    run.flags = [f for f in run.flags if f != bad]
                    run.flag = run.flags[0] if run.flags else None
                else:
                    run.flag = None
                    run.flags = []
            elif ev.event_type is EventType.HITL_REQUEST:
                # a (standby) worker raised its hand — surface it on the summary, same
                # as the inline _meta_sink in create().
                run.awaiting_help = True
                run.help_text = str((ev.payload or {}).get("need")
                                     or (ev.payload or {}).get("text") or "")[:300]
            elif ev.event_type is EventType.HITL_RESPONSE:
                # mirror the primary sink: reflect pause/resume AND lower the hand.
                action = ev.payload.get("action")
                if action == "pause":
                    run.paused = True
                elif action == "resume":
                    run.paused = False
                run.awaiting_help = False
                run.help_text = ""
            elif ev.event_type is EventType.RUN_FINISHED:
                run.finished = True
                run.paused = False
                run.awaiting_help = False
                run.help_text = ""
                run.solved = bool(ev.payload.get("solved")) or run.solved
                run.merge_flags(ev.payload.get("flags") or ev.payload.get("flag"))
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
        pending: list[asyncio.Task] = []
        for run in list(self.runs.values()):
            for t in (run.task, run.standby_task):
                if t is not None and not t.done():
                    t.cancel()
                    pending.append(t)
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
