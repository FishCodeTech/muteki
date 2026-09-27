"""Idle run archive/delete retention. Moved from run_manager.py."""

from __future__ import annotations

import asyncio
import time


from apps.web.run_state import (  # noqa: F401
    LOG, Run, BoundRunConflictError, BoundRunStore, Driver,
    BOUND_RUN_ID_PREFIX, _safe_exception_detail, _runtime_error_id,
    _apply_blackboard_meta, _apply_operator_meta,
)

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
