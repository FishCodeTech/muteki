"""Idle run archive/delete retention. Moved from run_manager.py."""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import time
from pathlib import Path
from typing import Any


from apps.web.run_state import (  # noqa: F401
    LOG, Run, BoundRunConflictError, BoundRunStore, Driver,
    BOUND_RUN_ID_PREFIX, _safe_exception_detail, _runtime_error_id,
    _apply_blackboard_meta, _apply_operator_meta,
)


DEFAULT_POLICY = {
    "archive_enabled": True,
    "archive_after_days": 15,
    "delete_enabled": True,
    "delete_after_days": 30,
}


class RunRetentionPolicyStore:
    """Small persistent per-mode retention settings store."""

    MODES = ("ctf", "pentest")

    def __init__(self, root: str | Path) -> None:
        self.path = Path(root) / "_run_retention_policies.json"
        self._policies = {mode: dict(DEFAULT_POLICY) for mode in self.MODES}
        self._load()

    @staticmethod
    def _valid_policy(value: Any) -> bool:
        return (isinstance(value, dict)
                and set(value) == set(DEFAULT_POLICY)
                and type(value.get("archive_enabled")) is bool
                and type(value.get("archive_after_days")) is int
                and type(value.get("delete_enabled")) is bool
                and type(value.get("delete_after_days")) is int
                and 1 <= value["archive_after_days"] <= 3650
                and 1 <= value["delete_after_days"] <= 3650
                and not (value["archive_enabled"] and value["delete_enabled"]
                         and value["delete_after_days"] <= value["archive_after_days"]))

    def _load(self) -> None:
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"Cannot read run retention policies: {self.path}") from exc
        policies = raw.get("policies") if isinstance(raw, dict) else None
        if (not isinstance(raw, dict) or raw.get("version") != 1
                or not isinstance(policies, dict)
                or set(policies) != set(self.MODES)
                or any(not self._valid_policy(policies[mode]) for mode in self.MODES)):
            raise ValueError(f"Invalid run retention policies: {self.path}")
        self._policies = {mode: dict(policies[mode]) for mode in self.MODES}

    def all(self) -> dict[str, dict[str, Any]]:
        return {mode: dict(policy) for mode, policy in self._policies.items()}

    def get(self, mode: str) -> dict[str, Any]:
        return dict(self._policies[mode])

    def set(self, mode: str, policy: dict[str, Any]) -> dict[str, Any]:
        if mode not in self.MODES or not self._valid_policy(policy):
            raise ValueError("Invalid run retention policy")
        candidate = {**self._policies, mode: dict(policy)}
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary: str | None = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w", encoding="utf-8", dir=self.path.parent,
                prefix=".run-retention-", suffix=".tmp", delete=False,
            ) as stream:
                temporary = stream.name
                json.dump({"version": 1, "policies": candidate}, stream,
                          ensure_ascii=False, separators=(",", ":"))
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
        finally:
            if temporary and os.path.exists(temporary):
                os.unlink(temporary)
        self._policies = candidate
        return self.get(mode)

def _last_activity(self, run: "Run") -> float:
    """Epoch seconds of a run's most recent event (its idle clock). 0.0 when
    unknown (no persisted events) → such a run is never auto-touched."""
    try:
        return float(run.store.summary(run.run_id).get("ts", 0.0) or 0.0)
    except Exception:
        return 0.0


async def retention_sweep(self, *, now: float,
                          archive_after_s: float | None = None,
                          delete_after_s: float | None = None) -> dict[str, list[str]]:
    """One pass using idle time since the run's last persisted event for both
    thresholds. Archive eligible started runs, then DELETE already-archived
    runs. PINNED runs are never auto-touched; unknown idle clocks are skipped. Returns
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
        policy = self.retention_policies.get(run.mode)
        if meta["archived"]:
            delete_s = (policy["delete_after_days"] * 86400
                        if delete_after_s is None else delete_after_s)
            if policy["delete_enabled"] and idle > delete_s:
                if await self.delete(run.run_id):
                    deleted.append(run.run_id)
                    LOG.info(
                        "retention: deleted stale archived run %s (idle %.0fs)",
                        run.run_id, idle)
        else:
            archive_s = (policy["archive_after_days"] * 86400
                         if archive_after_s is None else archive_after_s)
            if not policy["archive_enabled"] or idle <= archive_s:
                continue
            self.set_archived(run.run_id, True, now=now)
            archived.append(run.run_id)
            LOG.info("retention: archived idle run %s (idle %.0fs)", run.run_id, idle)
    return {"archived": archived, "deleted": deleted}


async def retention_loop(self, *, interval_s: float) -> None:
    """Background task: run retention_sweep every interval_s until cancelled.
    Sleeps FIRST so startup isn't blocked and a short-lived test process never
    triggers a sweep. A sweep failure is logged and the loop continues."""
    while True:
        try:
            await asyncio.sleep(interval_s)
            await self.retention_sweep(now=time.time())
        except asyncio.CancelledError:
            break
        except Exception:
            LOG.exception("retention sweep failed; continuing")
