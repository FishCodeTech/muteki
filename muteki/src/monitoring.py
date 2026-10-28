from __future__ import annotations

import asyncio
import difflib
import hashlib
import json
import math
import re
import uuid
from datetime import datetime, timedelta, timezone
from typing import Any, Awaitable, Callable, Optional
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from muteki.platform.contracts.base import new_id, utcnow
from muteki.platform.contracts.objects import ResourceLease
from muteki.platform.store import StateConflictError


class SrcMonitorError(RuntimeError):
    def __init__(self, code: str, message: str, *, detail: Optional[dict[str, Any]] = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.detail = detail or {}


class ExternalOutcomeUnknown(SrcMonitorError):
    def __init__(self, message: str, *, receipt: Optional[dict[str, Any]] = None) -> None:
        super().__init__("external_outcome_unknown", message, detail={"receipt": receipt or {}})
        self.receipt = receipt or {}


class CycleTimeout(SrcMonitorError):
    def __init__(self, message: str = "monitor cycle timed out") -> None:
        super().__init__("cycle_timeout", message)


Clock = Callable[[], datetime]
CycleRunner = Callable[[dict[str, Any]], Awaitable[dict[str, Any]]]
NotifyHook = Callable[[dict[str, Any]], Awaitable[None]]
PreflightHook = Callable[[dict[str, Any], dict[str, Any]], Awaitable[dict[str, Any]]]


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), default=str)


def _digest(value: Any) -> str:
    return hashlib.sha256(_json(value).encode("utf-8")).hexdigest()


def _dt(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat()


def _parse_dt(value: Any) -> Optional[datetime]:
    if not value:
        return None
    if isinstance(value, datetime):
        return value.astimezone(timezone.utc)
    return datetime.fromisoformat(str(value).replace("Z", "+00:00")).astimezone(timezone.utc)


def _paths(value: Any, prefix: str = "") -> list[str]:
    if isinstance(value, dict):
        return [item for key, item in value.items() for item in _paths(item, f"{prefix}.{key}" if prefix else str(key))]
    return [prefix]


def _drop_path(value: Any, path: str) -> None:
    parts = path.split(".")
    if not parts:
        return
    current = value
    for part in parts[:-1]:
        if not isinstance(current, dict) or part not in current:
            return
        current = current[part]
    if isinstance(current, dict):
        current.pop(parts[-1], None)


def _denoise(value: Any, rules: dict[str, Any]) -> tuple[Any, dict[str, Any]]:
    import copy

    result = copy.deepcopy(value)
    ignored: list[str] = []
    for path in rules.get("ignore_fields") or []:
        if path in _paths(result):
            _drop_path(result, path)
            ignored.append(path)

    def strip_keys(item: Any) -> None:
        if isinstance(item, dict):
            for key in list(item):
                if key in set(rules.get("ignore_keys") or []):
                    item.pop(key, None)
                    ignored.append(key)
                else:
                    strip_keys(item[key])
        elif isinstance(item, list):
            for child in item:
                strip_keys(child)

    strip_keys(result)
    replacements = rules.get("ignore_patterns") or []
    for item in replacements:
        path = str(item.get("path") or "")
        pattern = str(item.get("pattern") or "")
        replacement = str(item.get("replacement") or "<ignored>")
        if not path or not pattern:
            continue
        parent = result
        parts = path.split(".")
        for part in parts[:-1]:
            if not isinstance(parent, dict) or part not in parent:
                parent = None
                break
            parent = parent[part]
        if isinstance(parent, dict) and isinstance(parent.get(parts[-1]), str):
            updated = re.sub(pattern, replacement, parent[parts[-1]])
            if updated != parent[parts[-1]]:
                parent[parts[-1]] = updated
                ignored.append(path)
    return result, {
        "version": str(rules.get("version") or "v1"),
        "ignored": sorted(set(ignored)),
        "rules": rules,
    }


def _denoise_text(value: str, rules: dict[str, Any]) -> tuple[str, dict[str, Any]]:
    patterns = [str(item) for item in rules.get("ignore_line_patterns") or []]
    kept: list[str] = []
    ignored: list[str] = []
    for line in value.splitlines(keepends=True):
        if any(re.search(pattern, line) for pattern in patterns):
            ignored.append(line)
        else:
            kept.append(line)
    return "".join(kept), {
        "version": str(rules.get("version") or "v1"),
        "ignored": ignored,
        "rules": rules,
    }


def _recursive_diff(before: Any, after: Any, path: str = "") -> list[dict[str, Any]]:
    if type(before) is not type(after):
        return [{"path": path, "before": before, "after": after}]
    if isinstance(before, dict):
        rows: list[dict[str, Any]] = []
        for key in sorted(set(before) | set(after)):
            child = f"{path}.{key}" if path else str(key)
            if key not in before:
                rows.append({"path": child, "before": None, "after": after[key], "operation": "added"})
            elif key not in after:
                rows.append({"path": child, "before": before[key], "after": None, "operation": "removed"})
            else:
                rows.extend(_recursive_diff(before[key], after[key], child))
        return rows
    if isinstance(before, list):
        if before == after:
            return []
        return [{"path": path, "before": before, "after": after}]
    return [] if before == after else [{"path": path, "before": before, "after": after}]


class SrcMonitoringService:
    """Persistent monitoring definitions, cycles, baselines, changes and notices.

    ``src`` only needs ``store``, ``artifacts`` and ``platform`` attributes. The
    scheduler is an adapter for the product lifecycle, not another Agent loop.
    """

    def __init__(
        self,
        src: Any,
        *,
        clock: Clock = utcnow,
        runner: Optional[CycleRunner] = None,
        preflight: Optional[PreflightHook] = None,
        notify: Optional[NotifyHook] = None,
        instance_id: str = "",
    ) -> None:
        self.src = src
        self.store = src.store
        self.artifacts = src.artifacts
        self.platform = src.platform
        self.clock = clock
        self.runner = runner
        self.preflight = preflight
        self.notify = notify
        self.instance_id = instance_id or f"monitor-{uuid.uuid4().hex}"
        self._schedulers: dict[str, asyncio.Task[Any]] = {}
        self._stop_events: dict[str, asyncio.Event] = {}
        self._scheduler_health: dict[str, dict[str, Any]] = {}
        self._running: set[tuple[str, str]] = set()

    # ---- definitions -----------------------------------------------------

    def create_monitor(self, task_id: str, payload: dict[str, Any], *, actor_id: str) -> dict[str, Any]:
        if payload.get("continuous") is not True:
            raise SrcMonitorError("monitor_confirmation_required", "持续监控必须由用户明确创建；保存查询不会自动创建监控")
        monitor_id = new_id("smon")
        now = self.clock()
        definition = self._validate_definition(task_id, monitor_id, payload, version=1, actor_id=actor_id, now=now)
        current = {
            "monitor_id": monitor_id, "task_id": task_id, "current_version": 1,
            "status": "active", "current_cycle_id": "", "next_wake_at": _dt(now),
            "blocked_reason": "", "cost_used": 0.0, "cost_unknown": False,
            "last_success_at": None, "last_cycle_id": "",
        }
        with self.platform.transaction():
            self.store.put(task_id, "monitor_definition", definition, record_id=f"{monitor_id}:v1", parent_id=monitor_id, natural_key=f"{monitor_id}:v1")
            self.store.put(task_id, "monitor_current", current, record_id=monitor_id, natural_key=monitor_id)
        return self.get_monitor(task_id, monitor_id)

    def _validate_definition(
        self, task_id: str, monitor_id: str, payload: dict[str, Any], *,
        version: int, actor_id: str, now: datetime,
    ) -> dict[str, Any]:
        chain_type = str(payload.get("chain_type") or "")
        if chain_type not in {"asset", "rule"}:
            raise SrcMonitorError("monitor_chain_invalid", "监控链必须是 asset 或 rule")
        source = dict(payload.get("source") or {})
        if not source.get("connector_id") or not source.get("capability_manifest_ref"):
            raise SrcMonitorError("capability_unavailable", "监控必须固定真实来源与 capability manifest")
        if not str(payload.get("authorization_ref") or "").strip():
            raise SrcMonitorError("authorization_unresolved", "持续调用必须绑定明确授权版本")
        schedule = dict(payload.get("schedule") or {})
        interval = int(schedule.get("interval_seconds") or 0)
        if interval < 1:
            raise SrcMonitorError("monitor_schedule_invalid", "监控周期必须大于零秒")
        timezone_name = str(schedule.get("timezone") or "UTC")
        try:
            ZoneInfo(timezone_name)
        except ZoneInfoNotFoundError as exc:
            raise SrcMonitorError("monitor_schedule_invalid", f"未知时区 {timezone_name}") from exc
        budget = dict(payload.get("budget") or {})
        total = float(budget.get("total") or 0)
        per_cycle = float(budget.get("per_cycle") or total)
        call_limit = int(budget.get("call_limit") or 0)
        if not math.isfinite(total) or not math.isfinite(per_cycle) or total < 0 or per_cycle <= 0 or call_limit < 0:
            raise SrcMonitorError("monitor_budget_invalid", "预算必须为非负总额、正单轮上限和非负调用上限")
        notification = dict(payload.get("notification") or {})
        if int(notification.get("merge_window_seconds") or 0) < 0:
            raise SrcMonitorError("monitor_notification_invalid", "通知合并窗口不能为负数")
        baseline_policy = dict(payload.get("baseline_policy") or {})
        denoise = dict(baseline_policy.get("denoise") or {})
        denoise.setdefault("version", "v1")
        return {
            "monitor_id": monitor_id, "task_id": task_id, "version": version,
            "name": str(payload.get("name") or "").strip(),
            "chain_type": chain_type,
            "query_ref": payload.get("query_ref"),
            "target_selection": dict(payload.get("target_selection") or {}),
            "source": source,
            "plan_ref": str(payload.get("plan_ref") or ""),
            "authorization_ref": str(payload["authorization_ref"]),
            "credential_ref": str(payload.get("credential_ref") or ""),
            "tool_config_ref": str(payload.get("tool_config_ref") or ""),
            "schedule": {**schedule, "interval_seconds": interval, "timezone": timezone_name},
            "budget": {**budget, "total": total, "per_cycle": per_cycle, "call_limit": call_limit},
            "notification": notification,
            "baseline_policy": {**baseline_policy, "denoise": denoise},
            "created_by": actor_id,
            "source_message_id": str(payload.get("source_message_id") or ""),
            "created_at": _dt(now),
        }

    def revise_monitor(self, task_id: str, monitor_id: str, payload: dict[str, Any], *, actor_id: str, expected_revision: int) -> dict[str, Any]:
        current_record = self.store.require(task_id, "monitor_current", monitor_id)
        if current_record.revision != expected_revision:
            raise SrcMonitorError("version_conflict", "monitor revision is stale")
        previous = self._definition(task_id, monitor_id)
        merged = {**previous, **payload, "continuous": True}
        version = int(previous["version"]) + 1
        definition = self._validate_definition(task_id, monitor_id, merged, version=version, actor_id=actor_id, now=self.clock())
        unresolved = str(current_record.payload.get("blocked_reason") or "") in {"cost_unknown", "external_outcome_unknown"}
        current = {
            **current_record.payload, "current_version": version,
            "status": "active", "next_wake_at": None if unresolved else _dt(self.clock()),
            "blocked_reason": current_record.payload["blocked_reason"] if unresolved else "",
        }
        with self.platform.transaction():
            self.store.put(task_id, "monitor_definition", definition, record_id=f"{monitor_id}:v{version}", parent_id=monitor_id, natural_key=f"{monitor_id}:v{version}")
            self.store.put(task_id, "monitor_current", current, record_id=monitor_id, expected_revision=expected_revision, natural_key=monitor_id)
        return self.get_monitor(task_id, monitor_id)

    def _definition(self, task_id: str, monitor_id: str, version: Optional[int] = None) -> dict[str, Any]:
        selected = int(version if version is not None else self.store.require(task_id, "monitor_current", monitor_id).payload["current_version"])
        record = self.store.require(task_id, "monitor_definition", f"{monitor_id}:v{selected}")
        return record.payload

    def _cycle_definition(self, task_id: str, cycle: dict[str, Any]) -> dict[str, Any]:
        return self._definition(task_id, str(cycle["monitor_id"]), int(cycle["definition_version"]))

    def get_monitor(self, task_id: str, monitor_id: str) -> dict[str, Any]:
        current = self.store.require(task_id, "monitor_current", monitor_id)
        definition = self._definition(task_id, monitor_id)
        cycles = self.store.list(task_id, "monitor_cycle", parent_id=monitor_id, reverse=True, limit=20)
        baseline = self.store.list(task_id, "monitor_baseline", parent_id=monitor_id, reverse=True, limit=1)
        return {
            **current.payload,
            "revision": current.revision,
            "definition": definition,
            "cycles": [row.payload for row in cycles],
            "baseline": baseline[0].payload if baseline else None,
        }

    def list_monitors(self, task_id: str) -> list[dict[str, Any]]:
        return [self.get_monitor(task_id, row.record_id) for row in self.store.all(task_id, "monitor_current")]

    def set_state(self, task_id: str, monitor_id: str, state: str, *, expected_revision: int) -> dict[str, Any]:
        if state not in {"active", "paused", "cancelled"}:
            raise SrcMonitorError("monitor_state_invalid", "监控状态无效")
        record = self.store.require(task_id, "monitor_current", monitor_id)
        if record.revision != expected_revision:
            raise SrcMonitorError("version_conflict", "monitor revision is stale")
        payload = dict(record.payload)
        payload["status"] = state
        payload["next_wake_at"] = _dt(self.clock()) if state == "active" else None
        if state == "cancelled":
            payload["blocked_reason"] = "cancelled_by_user"
            cycle_id = str(payload.get("current_cycle_id") or "")
            if cycle_id:
                cycle = self.store.require(task_id, "monitor_cycle", cycle_id)
                if cycle.payload.get("status") in {"scheduled", "running"}:
                    self.store.put(task_id, "monitor_cycle", {**cycle.payload, "cancel_requested": True}, record_id=cycle_id, expected_revision=cycle.revision, parent_id=monitor_id)
        self.store.put(task_id, "monitor_current", payload, record_id=monitor_id, expected_revision=expected_revision, natural_key=monitor_id)
        return self.get_monitor(task_id, monitor_id)

    # ---- durable scheduling and leases -----------------------------------

    def due_monitors(self, task_id: str, *, now: Optional[datetime] = None) -> list[dict[str, Any]]:
        current_time = now or self.clock()
        due: list[dict[str, Any]] = []
        for record in self.store.all(task_id, "monitor_current"):
            row = record.payload
            wake = _parse_dt(row.get("next_wake_at"))
            if (
                row.get("status") == "active" and wake is not None and wake <= current_time
                and not row.get("current_cycle_id") and not row.get("blocked_reason")
            ):
                due.append(row)
        return due

    def _acquire(self, task_id: str, monitor_id: str, cycle_id: str, ttl: int) -> ResourceLease:
        key = f"{task_id}:{monitor_id}"
        existing = self.platform.get(ResourceLease, "src.monitor.cycle", key)
        token = (existing.fencing_token if existing else 0) + 1
        owner = f"{self.instance_id}:{cycle_id}"
        now = self.clock()
        lease = ResourceLease(
            resource_kind="src.monitor.cycle", resource_key=key, owner=owner,
            scope=f"src:{task_id}", ttl_seconds=max(1, ttl), fencing_token=token,
            acquired_at=now, expires_at=now + timedelta(seconds=max(1, ttl)),
        )
        try:
            return self.platform.acquire_lease(lease)
        except StateConflictError as exc:
            raise SrcMonitorError("resource_busy", "上一轮监控仍在运行或租约尚未过期") from exc

    def _assert_lease(self, lease: ResourceLease) -> None:
        current = self.platform.get(ResourceLease, "src.monitor.cycle", lease.resource_key)
        now = self.clock()
        expires = _parse_dt(current.expires_at) if current is not None and current.expires_at is not None else None
        if (
            current is None or current.owner != lease.owner
            or current.fencing_token != lease.fencing_token
            or current.released_at is not None
            or (expires is not None and expires <= now)
        ):
            raise SrcMonitorError("resource_busy", "监控轮次租约已失效")

    def _release(self, lease: ResourceLease) -> None:
        current = self.platform.get(ResourceLease, "src.monitor.cycle", lease.resource_key)
        if current is not None and current.owner == lease.owner and current.fencing_token == lease.fencing_token:
            self.platform.release_lease(lease.resource_kind, lease.resource_key)

    async def wake(self, task_id: str) -> list[dict[str, Any]]:
        results: list[dict[str, Any]] = []
        for row in self.due_monitors(task_id):
            try:
                cycle = self._new_cycle(task_id, row["monitor_id"])
                results.append(await self.run_cycle(task_id, row["monitor_id"], cycle["cycle_id"]))
            except SrcMonitorError as exc:
                results.append({"monitor_id": row["monitor_id"], "status": exc.code, "error": exc.message})
        return results

    async def run_once(self, task_id: str, monitor_id: str) -> dict[str, Any]:
        """Run one selected active monitor without waking its neighbors."""
        current = self.store.require(task_id, "monitor_current", monitor_id).payload
        if current.get("status") != "active":
            raise SrcMonitorError("monitor_state_invalid", "只有活跃监控可以执行一轮")
        if current.get("blocked_reason"):
            raise SrcMonitorError(str(current["blocked_reason"]), "监控已阻塞；先处理阻塞原因")
        if current.get("current_cycle_id"):
            raise SrcMonitorError("resource_busy", "上一轮监控仍在运行")
        cycle = self._new_cycle(task_id, monitor_id)
        return await self.run_cycle(task_id, monitor_id, cycle["cycle_id"])

    def scheduler_health(self, task_id: Optional[str] = None) -> Any:
        def view(task: str) -> dict[str, Any]:
            scheduler = self._schedulers.get(task)
            running = scheduler is not None and not scheduler.done()
            health = self._scheduler_health.get(task, {
                "task_id": task, "state": "stopped", "consecutive_failures": 0,
                "restart_count": 0, "last_error": None, "last_result": None,
            })
            return {
                **health, "task_id": task, "scheduler_running": running,
                "state": "running" if running and health.get("state") != "degraded" else health.get("state", "stopped"),
            }

        if task_id is not None:
            return view(task_id)
        return {task: view(task) for task in sorted(set(self._scheduler_health) | set(self._schedulers))}

    def _health_update(self, task_id: str, **changes: Any) -> None:
        current = self._scheduler_health.setdefault(task_id, {
            "task_id": task_id, "state": "stopped", "consecutive_failures": 0,
            "restart_count": 0, "last_error": None, "last_result": None,
        })
        current.update(changes)
        current["updated_at"] = _dt(self.clock())

    @staticmethod
    def _exception_view(exc: Exception) -> dict[str, str]:
        return {"type": type(exc).__name__, "code": str(getattr(exc, "code", "")), "message": str(exc)}

    async def start(self, *, task_id: str, poll_seconds: float = 1.0) -> None:
        existing = self._schedulers.get(task_id)
        if existing is not None and not existing.done():
            return
        stop_event = asyncio.Event()
        self._stop_events[task_id] = stop_event
        restart_count = int(self._scheduler_health.get(task_id, {}).get("restart_count") or 0) + 1
        self._health_update(task_id, state="running", restart_count=restart_count)

        async def loop() -> None:
            try:
                while not stop_event.is_set():
                    try:
                        results = await self.wake(task_id)
                        failures = [row for row in results if row.get("error")]
                        previous = self._scheduler_health.get(task_id, {}).get("last_error")
                        if not results:
                            self._health_update(task_id, last_result=results, last_tick_at=_dt(self.clock()))
                        else:
                            self._health_update(
                                task_id, last_result=results, last_tick_at=_dt(self.clock()),
                                state="degraded" if failures else "running",
                                consecutive_failures=(int(self._scheduler_health.get(task_id, {}).get("consecutive_failures") or 0) + 1) if failures else 0,
                                last_error=failures if failures else None,
                                previous_error=previous if not failures and previous else self._scheduler_health.get(task_id, {}).get("previous_error"),
                                last_success_at=None if failures else _dt(self.clock()),
                            )
                    except asyncio.CancelledError:
                        raise
                    except Exception as exc:
                        failures = int(self._scheduler_health.get(task_id, {}).get("consecutive_failures") or 0) + 1
                        self._health_update(
                            task_id, state="degraded", consecutive_failures=failures,
                            last_failure_at=_dt(self.clock()), last_error=self._exception_view(exc),
                        )
                    try:
                        await asyncio.wait_for(stop_event.wait(), timeout=poll_seconds)
                    except asyncio.TimeoutError:
                        pass
            except asyncio.CancelledError:
                pass
            finally:
                self._health_update(task_id, state="stopped", stopped_at=_dt(self.clock()))

        self._schedulers[task_id] = asyncio.create_task(loop(), name=f"src-monitoring-{task_id}")

    async def stop(self, task_id: Optional[str] = None) -> Any:
        task_ids = [task_id] if task_id is not None else list(self._schedulers)
        running: list[tuple[str, asyncio.Task[Any]]] = []
        for task in task_ids:
            if task is None:
                continue
            event = self._stop_events.get(task)
            if event is not None:
                event.set()
            scheduler = self._schedulers.get(task)
            if scheduler is not None:
                scheduler.cancel()
                running.append((task, scheduler))
        results = await asyncio.gather(*(scheduler for _task, scheduler in running), return_exceptions=True)
        for (task, _scheduler), result in zip(running, results):
            if isinstance(result, BaseException) and not isinstance(result, asyncio.CancelledError):
                self._health_update(task, state="degraded", last_error=self._exception_view(result), last_failure_at=_dt(self.clock()))
            self._schedulers.pop(task, None)
            self._stop_events.pop(task, None)
            self._health_update(task, state="stopped", stopped_at=_dt(self.clock()))
        return self.scheduler_health(task_id)

    def _new_cycle(self, task_id: str, monitor_id: str) -> dict[str, Any]:
        with self.platform.transaction():
            current = self.store.require(task_id, "monitor_current", monitor_id)
            active_cycle_id = str(current.payload.get("current_cycle_id") or "")
            if active_cycle_id:
                return self.store.require(task_id, "monitor_cycle", active_cycle_id).payload
            definition = self._definition(task_id, monitor_id)
            now = self.clock()
            scheduled = _dt(now)
            cycle_id = f"{monitor_id}:v{definition['version']}:{hashlib.sha256(scheduled.encode()).hexdigest()[:20]}"
            existing = self.store.get(task_id, "monitor_cycle", cycle_id)
            if existing is not None:
                return existing.payload
            budget = definition["budget"]
            used = float(current.payload.get("cost_used") or 0)
            total = float(budget["total"])
            if total and used >= total:
                cycle = self._cycle_payload(task_id, definition, cycle_id, scheduled, "budget_exhausted", now)
                cycle["end_reason"] = "budget_exhausted"
                cycle["finished_at"] = _dt(now)
                self.store.put(task_id, "monitor_cycle", cycle, record_id=cycle_id, parent_id=monitor_id, natural_key=cycle_id)
                self.store.put(task_id, "monitor_current", {**current.payload, "last_cycle_id": cycle_id, "next_wake_at": None, "blocked_reason": "budget_exhausted"}, record_id=monitor_id, expected_revision=current.revision, natural_key=monitor_id)
                self._finish_notification(task_id, definition, cycle)
                return cycle
            reserved = min(float(budget["per_cycle"]), max(0.0, total - used) if total else float(budget["per_cycle"]))
            cycle = self._cycle_payload(task_id, definition, cycle_id, scheduled, "scheduled", now)
            cycle["cost"] = {"reserved": reserved, "actual": 0.0, "unknown": False, "currency": budget.get("currency", "USD")}
            self.store.put(task_id, "monitor_cycle", cycle, record_id=cycle_id, parent_id=monitor_id, natural_key=cycle_id)
            self.store.put(task_id, "monitor_current", {**current.payload, "current_cycle_id": cycle_id}, record_id=monitor_id, expected_revision=current.revision, natural_key=monitor_id)
            self._cost_ledger(task_id, cycle_id, "reserved", reserved, False, {"budget_version": definition["version"]})
            return cycle

    def _cycle_payload(self, task_id: str, definition: dict[str, Any], cycle_id: str, scheduled: str, status: str, now: datetime) -> dict[str, Any]:
        return {
            "cycle_id": cycle_id, "task_id": task_id, "monitor_id": definition["monitor_id"],
            "definition_version": definition["version"], "target_snapshot": dict(definition["target_selection"]),
            "scheduled_at": scheduled, "started_at": None, "finished_at": None,
            "status": status, "completeness": "unavailable", "source_watermarks": {},
            "cost": {"reserved": 0.0, "actual": 0.0, "unknown": False},
            "baseline_id": "", "compared_baseline_id": "", "end_reason": "",
            "dispatch_state": "created", "cancel_requested": False,
            "source_receipts": [], "observation_count": 0, "error": "",
        }

    def _cost_ledger(self, task_id: str, cycle_id: str, kind: str, amount: float, unknown: bool, detail: dict[str, Any]) -> None:
        key = f"{cycle_id}:{kind}:{_digest(detail)}"
        if self.store.idempotent(task_id, "monitor.cost", key, detail) is not None:
            return
        record = {"ledger_id": new_id("scost"), "cycle_id": cycle_id, "kind": kind, "amount": amount, "unknown": unknown, "detail": detail, "created_at": _dt(self.clock())}
        self.store.put(task_id, "monitor_cost", record, record_id=record["ledger_id"], parent_id=cycle_id)
        self.store.remember(task_id, "monitor.cost", key, detail, record)

    # ---- cycle execution -------------------------------------------------

    async def run_cycle(self, task_id: str, monitor_id: str, cycle_id: str, *, runner: Optional[CycleRunner] = None) -> dict[str, Any]:
        running_key = (task_id, cycle_id)
        if running_key in self._running:
            raise SrcMonitorError("resource_busy", "此轮监控已在当前进程运行")
        record = self.store.require(task_id, "monitor_cycle", cycle_id)
        if record.payload.get("monitor_id") != monitor_id or record.payload.get("task_id") != task_id:
            raise SrcMonitorError("task_binding_invalid", "监控轮次与选定 Task/监控不一致")
        if record.payload.get("status") == "running":
            raise SrcMonitorError("resource_busy", "轮次仍在运行；重启后先执行 recover，不能直接重放来源调用")
        if record.payload.get("status") != "scheduled":
            return record.payload
        self._running.add(running_key)
        try:
            lease = self._acquire(task_id, monitor_id, cycle_id, int(self._cycle_definition(task_id, record.payload)["schedule"].get("cycle_timeout_seconds") or 300))
            try:
                return await self._run_cycle_lease(task_id, monitor_id, cycle_id, lease, runner or self.runner)
            finally:
                self._release(lease)
        finally:
            self._running.discard(running_key)

    async def _run_cycle_lease(self, task_id: str, monitor_id: str, cycle_id: str, lease: ResourceLease, runner: Optional[CycleRunner]) -> dict[str, Any]:
        cycle_record = self.store.require(task_id, "monitor_cycle", cycle_id)
        cycle = cycle_record.payload
        definition = self._cycle_definition(task_id, cycle)
        current_record = self.store.require(task_id, "monitor_current", monitor_id)
        if current_record.payload.get("status") != "active" or cycle.get("cancel_requested"):
            return self._finish(task_id, cycle_id, "cancelled", "cancelled_by_user", lease=lease)
        if runner is None:
            return self._finish(task_id, cycle_id, "failed", "capability_unavailable", error="no real monitor source runner is configured", lease=lease)
        started = self.clock()
        cycle = {**cycle, "status": "running", "started_at": _dt(started), "dispatch_state": "preflight"}
        self.store.put(task_id, "monitor_cycle", cycle, record_id=cycle_id, expected_revision=cycle_record.revision, parent_id=monitor_id)
        try:
            self._assert_lease(lease)
            context = {
                "task_id": task_id, "monitor_id": monitor_id, "cycle_id": cycle_id,
                "definition": definition, "cycle": cycle,
                "existing_receipts": list(cycle.get("source_receipts") or []),
            }
            if self.preflight is not None:
                preflight = await self.preflight(definition, cycle)
                context["preflight"] = preflight
                if preflight.get("allowed") is False:
                    return self._finish(task_id, cycle_id, "failed", str(preflight.get("code") or "authorization_stale"), error=str(preflight.get("message") or "authorization is stale"), lease=lease)
            cycle["dispatch_state"] = "dispatched"
            cycle_record = self.store.require(task_id, "monitor_cycle", cycle_id)
            self.store.put(task_id, "monitor_cycle", cycle, record_id=cycle_id, expected_revision=cycle_record.revision, parent_id=monitor_id)
            timeout = float(definition["schedule"].get("cycle_timeout_seconds") or 300)
            try:
                result = await asyncio.wait_for(runner(context), timeout=timeout)
            except asyncio.TimeoutError as exc:
                raise CycleTimeout() from exc
            self._assert_lease(lease)
            latest_cycle = self.store.require(task_id, "monitor_cycle", cycle_id).payload
            latest_current = self.store.require(task_id, "monitor_current", monitor_id).payload
            if latest_current.get("status") != "active" or latest_cycle.get("cancel_requested"):
                return self._finish(task_id, cycle_id, "cancelled", "cancelled_by_user", lease=lease)
            return self._commit_result(task_id, monitor_id, cycle_id, result, lease=lease)
        except ExternalOutcomeUnknown as exc:
            return self._finish(
                task_id, cycle_id, "failed", "external_outcome_unknown",
                error=exc.message, external_unknown=True, receipt=exc.receipt, lease=lease,
            )
        except CycleTimeout as exc:
            return self._finish(task_id, cycle_id, "failed", "cycle_timeout", error=exc.message, external_unknown=True, lease=lease)
        except SrcMonitorError as exc:
            return self._finish(task_id, cycle_id, "failed", exc.code, error=exc.message, lease=lease)
        except asyncio.CancelledError:
            self._finish(task_id, cycle_id, "cancelled", "cancelled_by_user", external_unknown=True, lease=lease)
            raise
        except Exception as exc:
            return self._finish(task_id, cycle_id, "failed", type(exc).__name__, error=f"{type(exc).__name__}: {exc}", external_unknown=True, lease=lease)

    def _commit_result(self, task_id: str, monitor_id: str, cycle_id: str, result: dict[str, Any], *, lease: ResourceLease) -> dict[str, Any]:
        cycle = self.store.require(task_id, "monitor_cycle", cycle_id).payload
        definition = self._cycle_definition(task_id, cycle)
        completeness = str(result.get("completeness") or ("complete" if result.get("complete") else "partial"))
        if completeness not in {"complete", "partial", "unavailable"}:
            raise SrcMonitorError("source_result_invalid", "来源完整性必须是 complete/partial/unavailable")
        calls = int(result.get("calls") or 0)
        limit = int(definition["budget"].get("call_limit") or 0)
        cost = dict(result.get("cost") or {})
        actual_cost = float(cost.get("actual") or 0)
        unknown_cost = bool(cost.get("unknown"))
        if calls < 0 or not math.isfinite(actual_cost) or actual_cost < 0:
            raise SrcMonitorError("source_result_invalid", "来源调用次数和实际费用不能为负数")
        reserved = float((cycle.get("cost") or {}).get("reserved") or 0)
        over_calls = bool(limit and calls > limit)
        over_cost = not unknown_cost and actual_cost > reserved
        over_budget = over_calls or over_cost
        errors = list(result.get("errors") or [])
        if over_calls:
            errors.append({"code": "budget_exhausted", "message": f"calls {calls} exceeds {limit}"})
        if over_cost:
            errors.append({"code": "budget_exhausted", "message": f"actual cost {actual_cost} exceeds reserved {reserved}"})
        revalidation = result.get("revalidation")
        if not isinstance(revalidation, dict):
            errors.append({"code": "authorization_unresolved", "message": "source did not return this cycle's source/permission/plan/authorization verification"})
            revalidation = {}
        for field in ("source_available", "permission_valid", "plan_valid", "authorization_valid"):
            if revalidation.get(field) is not True and not any(item.get("code") == "authorization_unresolved" for item in errors):
                errors.append({"code": "authorization_unresolved", "message": f"cycle revalidation failed: {field}"})
        raw_snapshot = result.get("raw_snapshot")
        raw_bytes = raw_snapshot if isinstance(raw_snapshot, bytes) else _json(raw_snapshot).encode("utf-8")
        raw_ref = self.artifacts.put(task_id, raw_bytes, media_type="application/octet-stream" if isinstance(raw_snapshot, bytes) else "application/json", name=f"{cycle_id}-source")
        receipt = {
            "receipt_id": new_id("sreceipt"), "cycle_id": cycle_id,
            "source": definition["source"], "raw_artifact": raw_ref.model_dump(mode="json"),
            "source_receipt": result.get("receipt"), "source_watermarks": result.get("source_watermarks") or {},
            "complete": completeness == "complete", "created_at": _dt(self.clock()),
        }
        with self.platform.transaction():
            self.store.put(task_id, "monitor_source_receipt", receipt, record_id=receipt["receipt_id"], parent_id=cycle_id)
            committed = self._commit_observations(task_id, cycle_id, list(result.get("observations") or []), raw_ref)
            status = "budget_exhausted" if over_budget else "completed" if completeness == "complete" and not errors else "partial" if completeness == "partial" else "failed"
            authorization_failed = any(item.get("code") == "authorization_unresolved" for item in errors)
            end_reason = "budget_exhausted" if over_budget else "authorization_unresolved" if authorization_failed else "cost_unknown" if unknown_cost and calls else "" if status == "completed" else "source_incomplete"
            cycle = {
                **cycle, "source_receipts": [*list(cycle.get("source_receipts") or []), receipt["receipt_id"]],
                "source_watermarks": result.get("source_watermarks") or {},
                "observation_count": committed,
                "cost": {**cycle.get("cost", {}), "actual": actual_cost, "unknown": unknown_cost, "calls": calls},
                "completeness": completeness, "revalidation": revalidation,
            }
            self.store.put(task_id, "monitor_cycle", cycle, record_id=cycle_id, parent_id=monitor_id)
            self._cost_ledger(task_id, cycle_id, "actual", actual_cost, unknown_cost, {"source_receipt": receipt["receipt_id"], "calls": calls})
            current_record = self.store.require(task_id, "monitor_current", monitor_id)
            current = {
                **current_record.payload,
                "cost_used": float(current_record.payload.get("cost_used") or 0) + actual_cost,
                "cost_unknown": bool(current_record.payload.get("cost_unknown")) or unknown_cost,
            }
            self.store.put(task_id, "monitor_current", current, record_id=monitor_id, expected_revision=current_record.revision, natural_key=monitor_id)
        if status in {"failed", "budget_exhausted"}:
            return self._finish(task_id, cycle_id, status, end_reason, error=_json(errors), lease=lease, completeness=completeness)
        baseline_result = self._compare_and_baseline(task_id, monitor_id, cycle_id, result, raw_ref, complete=completeness == "complete" and not errors)
        return self._finish(
            task_id, cycle_id, status, end_reason,
            error=_json(errors) if errors else "", lease=lease, completeness=completeness,
            compared_baseline_id=baseline_result.get("compared_baseline_id", ""),
            baseline_id=baseline_result.get("baseline_id", ""), comparison=baseline_result,
        )

    def _commit_observations(self, task_id: str, cycle_id: str, observations: list[dict[str, Any]], source_ref: Any) -> int:
        count = 0
        for index, observation in enumerate(observations):
            stable = str(observation.get("observation_id") or observation.get("key") or index)
            request = {"cycle_id": cycle_id, "stable": stable, "observation": observation}
            key = f"monitor.observation:{_digest(request)}"
            if self.store.idempotent(task_id, "monitor.observation", key, request) is not None:
                continue
            record = {
                "observation_id": str(observation.get("observation_id") or new_id("smob")),
                "task_id": task_id, "cycle_id": cycle_id,
                "key": str(observation.get("key") or stable),
                "observed_at": observation.get("observed_at"),
                "ingested_at": _dt(self.clock()),
                "attributes": observation.get("attributes") or {},
                "raw_values": observation.get("raw_values", observation.get("raw")),
                "relations": observation.get("relations") or [],
                "source_artifact": source_ref.model_dump(mode="json"),
                "completeness": observation.get("completeness", "complete"),
                "success": observation.get("success"),
            }
            self.store.put(task_id, "monitor_observation", record, record_id=record["observation_id"], parent_id=cycle_id, natural_key=record["observation_id"])
            self.store.remember(task_id, "monitor.observation", key, request, record)
            count += 1
        return count

    # ---- baselines and changes ------------------------------------------

    def _latest_baseline(self, task_id: str, monitor_id: str) -> Optional[dict[str, Any]]:
        rows = self.store.list(task_id, "monitor_baseline", parent_id=monitor_id, reverse=True, limit=1)
        return rows[0].payload if rows else None

    def _definition_signature(self, definition: dict[str, Any]) -> str:
        return _digest({
            "chain_type": definition["chain_type"], "query_ref": definition.get("query_ref"),
            "source": definition["source"], "authorization_ref": definition["authorization_ref"],
            "tool_config_ref": definition.get("tool_config_ref"),
            "target_selection": definition["target_selection"],
            "denoise": definition["baseline_policy"].get("denoise"),
        })

    def _coverage_signature(self, chain_type: str, snapshot: Any, *, definition: Optional[dict[str, Any]] = None) -> str:
        if definition is not None:
            explicit_scope = definition.get("target_selection") or {}
            if explicit_scope:
                return _digest({"chain_type": chain_type, "target_selection": explicit_scope})
        if chain_type == "asset":
            return "asset-selection"
        if chain_type == "rule":
            return "program-rules"
        return _digest(snapshot)

    def _compare_and_baseline(self, task_id: str, monitor_id: str, cycle_id: str, result: dict[str, Any], raw_ref: Any, *, complete: bool) -> dict[str, Any]:
        if not complete:
            return {"comparable": False, "reason": "source_incomplete", "baseline_id": "", "compared_baseline_id": ""}
        definition = self._cycle_definition(task_id, self.store.require(task_id, "monitor_cycle", cycle_id).payload)
        snapshot = result.get("raw_snapshot")
        if snapshot is None:
            return {"comparable": False, "reason": "snapshot_missing", "baseline_id": "", "compared_baseline_id": ""}
        previous = self._latest_baseline(task_id, monitor_id)
        denoise_rules = dict(definition["baseline_policy"].get("denoise") or {})
        signature = self._definition_signature(definition)
        coverage = self._coverage_signature(definition["chain_type"], snapshot, definition=definition)
        if previous is not None and previous.get("definition_signature") != signature:
            if definition["baseline_policy"].get("rebase_on_definition_change", True):
                baseline = self._new_baseline(task_id, definition, cycle_id, snapshot, raw_ref, denoise_rules, signature, coverage, "definition_changed_new_baseline")
                return {"comparable": False, "reason": "definition_changed_new_baseline", "baseline_id": baseline["baseline_id"], "compared_baseline_id": previous["baseline_id"]}
            self._record_change(task_id, definition, cycle_id, {
                "root_object_key": "monitor-definition", "kind": "incomparable",
                "before": previous, "after": {"definition_version": definition["version"]},
                "comparable": False, "meaningful": True,
                "gaps": ["查询、来源、权限、采样或提取规则已变化，不能与旧基线直接比较"],
            })
            return {"comparable": False, "reason": "definition_changed", "baseline_id": "", "compared_baseline_id": previous["baseline_id"]}
        if previous is not None and previous.get("coverage_signature") != coverage:
            self._record_change(task_id, definition, cycle_id, {
                "root_object_key": "coverage", "kind": "coverage_changed",
                "before": previous.get("coverage_signature"), "after": coverage,
                "comparable": False, "meaningful": True,
                "gaps": ["覆盖对象集合变化；新增覆盖对象不能称为刚上线，也不能据旧集合报告删除"],
            })
            return {"comparable": False, "reason": "coverage_changed", "baseline_id": "", "compared_baseline_id": previous["baseline_id"]}
        if previous is None:
            baseline = self._new_baseline(task_id, definition, cycle_id, snapshot, raw_ref, denoise_rules, signature, coverage, "first_complete_cycle")
            return {"comparable": True, "reason": "first_complete_cycle", "baseline_id": baseline["baseline_id"], "compared_baseline_id": ""}
        changes = self._diff_snapshot(task_id, definition, cycle_id, previous, snapshot, raw_ref)
        baseline = self._new_baseline(task_id, definition, cycle_id, snapshot, raw_ref, denoise_rules, signature, coverage, "next_complete_cycle")
        gap_cycles = self._cycles_between(task_id, monitor_id, str(previous.get("cycle_id")), cycle_id)
        return {
            "comparable": True, "reason": "compared_last_valid_baseline",
            "baseline_id": baseline["baseline_id"], "compared_baseline_id": previous["baseline_id"],
            "changes": changes, "missing_cycles": gap_cycles,
        }

    def establish_baseline(self, task_id: str, monitor_id: str, cycle_id: str, *, expected_revision: int) -> dict[str, Any]:
        """Explicitly accept a complete cycle's new comparison coverage."""
        current = self.store.require(task_id, "monitor_current", monitor_id)
        if current.revision != expected_revision:
            raise SrcMonitorError("version_conflict", "monitor revision is stale")
        cycle = self.store.require(task_id, "monitor_cycle", cycle_id).payload
        if cycle.get("status") != "completed" or cycle.get("completeness") != "complete":
            raise SrcMonitorError("source_incomplete", "只有完整成功轮次可以建立新基线")
        receipt_ids = list(cycle.get("source_receipts") or [])
        if not receipt_ids:
            raise SrcMonitorError("artifact_missing", "本轮没有可固定的来源原件")
        receipt = self.store.require(task_id, "monitor_source_receipt", receipt_ids[-1]).payload
        from .contracts import ArtifactRef

        ref = ArtifactRef.model_validate(receipt["raw_artifact"])
        raw = self.artifacts.read(ref, expected_task_id=task_id)
        definition = self._cycle_definition(task_id, cycle)
        snapshot = raw if definition["chain_type"] == "rule" else json.loads(raw.decode("utf-8"))
        return self._new_baseline(
            task_id, definition, cycle_id, snapshot, ref,
            dict(definition["baseline_policy"].get("denoise") or {}),
            self._definition_signature(definition),
            self._coverage_signature(definition["chain_type"], snapshot, definition=definition),
            "operator_accepted_coverage",
        )

    def _new_baseline(self, task_id: str, definition: dict[str, Any], cycle_id: str, snapshot: Any, raw_ref: Any, denoise_rules: dict[str, Any], signature: str, coverage: str, reason: str) -> dict[str, Any]:
        monitor_id = definition["monitor_id"]
        key = f"monitor.baseline:{cycle_id}"
        existing = self.store.idempotent(task_id, "monitor.baseline", key, {"cycle_id": cycle_id})
        if existing is not None:
            return existing
        if definition["chain_type"] == "rule":
            text = snapshot.decode("utf-8") if isinstance(snapshot, bytes) else str(snapshot)
            denoised, denoise_meta = _denoise_text(text, denoise_rules)
        else:
            denoised, denoise_meta = _denoise(snapshot, denoise_rules)
        denoised_ref = self.artifacts.put(task_id, _json(denoised).encode("utf-8"), media_type="application/json", name=f"{cycle_id}-denoised")
        baseline = {
            "baseline_id": new_id("sbaseline"), "task_id": task_id, "monitor_id": monitor_id,
            "cycle_id": cycle_id, "definition_version": definition["version"],
            "created_at": _dt(self.clock()), "reason": reason,
            "raw_artifact": raw_ref.model_dump(mode="json"),
            "denoised_artifact": denoised_ref.model_dump(mode="json"),
            "denoise": denoise_meta, "definition_signature": signature,
            "coverage_signature": coverage, "completeness": "complete",
        }
        self.store.put(task_id, "monitor_baseline", baseline, record_id=baseline["baseline_id"], parent_id=monitor_id, natural_key=baseline["baseline_id"])
        self.store.remember(task_id, "monitor.baseline", key, {"cycle_id": cycle_id}, baseline)
        return baseline

    def _cycles_between(self, task_id: str, monitor_id: str, previous_cycle_id: str, current_cycle_id: str) -> list[dict[str, Any]]:
        rows = self.store.all(task_id, "monitor_cycle")
        selected: list[dict[str, Any]] = []
        seen_previous = False
        for row in rows:
            if row.parent_id != monitor_id:
                continue
            if row.record_id == previous_cycle_id:
                seen_previous = True
                continue
            if row.record_id == current_cycle_id:
                break
            if seen_previous:
                selected.append({"cycle_id": row.record_id, "status": row.payload.get("status"), "completeness": row.payload.get("completeness")})
        return selected

    def _diff_snapshot(self, task_id: str, definition: dict[str, Any], cycle_id: str, previous: dict[str, Any], snapshot: Any, raw_ref: Any) -> list[str]:
        old_ref = previous["raw_artifact"]
        from .contracts import ArtifactRef

        before_bytes = self.artifacts.read(ArtifactRef.model_validate(old_ref), expected_task_id=task_id)
        before = before_bytes if definition["chain_type"] == "rule" else json.loads(before_bytes.decode("utf-8"))
        after = snapshot
        changes: list[dict[str, Any]] = []
        if definition["chain_type"] == "rule":
            before_text = before.decode("utf-8") if isinstance(before, bytes) else str(before)
            after_text = after.decode("utf-8") if isinstance(after, bytes) else str(after)
            raw_diff = list(difflib.unified_diff(before_text.splitlines(), after_text.splitlines(), lineterm=""))
            before_denoised, before_meta = _denoise_text(before_text, definition["baseline_policy"].get("denoise") or {})
            after_denoised, after_meta = _denoise_text(after_text, definition["baseline_policy"].get("denoise") or {})
            denoised_diff = list(difflib.unified_diff(before_denoised.splitlines(), after_denoised.splitlines(), lineterm=""))
            if raw_diff:
                changes.append({
                    "root_object_key": "program-rules", "kind": "rule_text_changed",
                    "before_artifact": old_ref, "after_artifact": raw_ref.model_dump(mode="json"),
                    "raw_diff": raw_diff, "denoised_diff": denoised_diff,
                    "denoise": {"before": before_meta, "after": after_meta},
                    "meaningful": bool(denoised_diff), "comparable": True,
                })
        else:
            before_map = self._asset_map(before)
            after_map = self._asset_map(after)
            rules = definition["baseline_policy"].get("denoise") or {}
            for key in sorted(set(before_map) | set(after_map)):
                if key not in before_map:
                    changes.append({"root_object_key": key, "kind": "added", "before": None, "after": after_map[key], "meaningful": True, "comparable": True})
                    continue
                if key not in after_map:
                    changes.append({"root_object_key": key, "kind": "removed", "before": before_map[key], "after": None, "meaningful": True, "comparable": True})
                    continue
                raw_diff = _recursive_diff(before_map[key], after_map[key])
                if not raw_diff:
                    continue
                before_denoised, before_meta = _denoise(before_map[key], rules)
                after_denoised, after_meta = _denoise(after_map[key], rules)
                denoised_diff = _recursive_diff(before_denoised, after_denoised)
                changes.append({
                    "root_object_key": key, "kind": "changed",
                    "before": before_map[key], "after": after_map[key],
                    "before_artifact": old_ref, "after_artifact": raw_ref.model_dump(mode="json"),
                    "raw_diff": raw_diff, "denoised_diff": denoised_diff,
                    "denoise": {"before": before_meta, "after": after_meta},
                    "meaningful": bool(denoised_diff), "comparable": True,
                })
        return [self._record_change(task_id, definition, cycle_id, change) for change in changes]

    @staticmethod
    def _asset_map(snapshot: Any) -> dict[str, Any]:
        if isinstance(snapshot, dict):
            return {str(key): value for key, value in snapshot.items()}
        if isinstance(snapshot, list):
            rows: dict[str, Any] = {}
            for index, item in enumerate(snapshot):
                key = str(item.get("key") or item.get("asset_id") or index) if isinstance(item, dict) else str(item)
                rows[key] = item
            return rows
        raise SrcMonitorError("snapshot_invalid", "资产快照必须是对象或数组")

    def _record_change(self, task_id: str, definition: dict[str, Any], cycle_id: str, change: dict[str, Any]) -> str:
        monitor_id = definition["monitor_id"]
        request = {"cycle_id": cycle_id, "change": change}
        key = f"monitor.change:{_digest(request)}"
        existing = self.store.idempotent(task_id, "monitor.change", key, request)
        if existing is not None:
            return str(existing["change_id"])
        change_id = new_id("schange")
        payload = {
            **change, "change_id": change_id, "task_id": task_id,
            "monitor_id": monitor_id, "cycle_id": cycle_id,
            "chain_type": definition["chain_type"], "detected_at": _dt(self.clock()),
        }
        candidate_id = ""
        if change.get("meaningful"):
            candidate = {
                "candidate_id": new_id("scandidate"), "task_id": task_id,
                "monitor_id": monitor_id, "cycle_id": cycle_id, "change_id": change_id,
                "projection_kind": "observation",
                "question": f"核对 {change.get('root_object_key')} 的变化是否改变当前研究前提",
                "related_objects": [change.get("root_object_key")],
                "evidence_refs": [change_id],
                "missing_conditions": list(change.get("gaps") or []),
                "suggested_action": "review_evidence",
                "status": "pending", "created_at": _dt(self.clock()),
            }
            candidate_id = candidate["candidate_id"]
            self.store.put(task_id, "monitor_candidate", candidate, record_id=candidate_id, parent_id=cycle_id)
            payload["candidate_id"] = candidate_id
        self.store.put(task_id, "monitor_change", payload, record_id=change_id, parent_id=cycle_id)
        self.store.remember(task_id, "monitor.change", key, request, payload)
        if change.get("meaningful"):
            self._queue_notification(task_id, definition, payload)
        return change_id

    # ---- notifications and recovery -------------------------------------

    def _queue_notification(self, task_id: str, definition: dict[str, Any], change: dict[str, Any]) -> None:
        notification = definition.get("notification") or {}
        if notification.get("enabled") is False:
            return
        now = self.clock()
        root = str(change.get("root_object_key") or change["change_id"])
        window = int(notification.get("merge_window_seconds") or 0)
        for row in reversed(self.store.all(task_id, "monitor_notification")):
            data = row.payload
            if data.get("status") != "pending" or data.get("root_object_key") != root or data.get("monitor_id") != definition["monitor_id"]:
                continue
            started = _parse_dt(data.get("window_start"))
            if started is not None and now - started <= timedelta(seconds=window):
                if change["change_id"] not in data["change_ids"]:
                    data["change_ids"].append(change["change_id"])
                    if change.get("candidate_id"):
                        data.setdefault("candidate_ids", []).append(change["candidate_id"])
                    data["window_end"] = _dt(now + timedelta(seconds=window))
                    self.store.put(task_id, "monitor_notification", data, record_id=row.record_id, expected_revision=row.revision, parent_id=row.parent_id)
                return
        key = f"monitor.notification:{change['change_id']}"
        if self.store.idempotent(task_id, "monitor.notification", key, {"change_id": change["change_id"]}) is not None:
            return
        payload = {
            "notification_id": new_id("snotice"), "task_id": task_id,
            "monitor_id": definition["monitor_id"], "root_object_key": root,
            "kind": "meaningful_change", "change_ids": [change["change_id"]],
            "candidate_ids": [change["candidate_id"]] if change.get("candidate_id") else [],
            "window_start": _dt(now), "window_end": _dt(now + timedelta(seconds=window)),
            "status": "pending", "created_at": _dt(now), "delivered_at": None,
            "channel": "external" if notification.get("external_enabled") else "task",
            "external_channel": notification.get("external_channel") if notification.get("external_enabled") else "",
        }
        self.store.put(task_id, "monitor_notification", payload, record_id=payload["notification_id"], parent_id=definition["monitor_id"])
        self.store.remember(task_id, "monitor.notification", key, {"change_id": change["change_id"]}, payload)

    def _finish(
        self, task_id: str, cycle_id: str, status: str, reason: str, *,
        error: str = "", external_unknown: bool = False,
        receipt: Optional[dict[str, Any]] = None, lease: ResourceLease,
        completeness: Optional[str] = None, compared_baseline_id: str = "",
        baseline_id: str = "", comparison: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        self._assert_lease(lease)
        record = self.store.require(task_id, "monitor_cycle", cycle_id)
        cycle = record.payload
        monitor_id = cycle["monitor_id"]
        definition = self._cycle_definition(task_id, cycle)
        now = self.clock()
        updated = {
            **cycle, "status": status, "finished_at": _dt(now),
            "end_reason": reason, "error": error,
            "dispatch_state": "outcome_unknown" if external_unknown else "finished",
            "baseline_id": baseline_id or cycle.get("baseline_id", ""),
            "compared_baseline_id": compared_baseline_id or cycle.get("compared_baseline_id", ""),
            "comparison": comparison or cycle.get("comparison") or {},
        }
        if completeness is not None:
            updated["completeness"] = completeness
        if receipt is not None:
            updated["external_receipt"] = receipt
        current_record = self.store.require(task_id, "monitor_current", monitor_id)
        total = float(definition["budget"].get("total") or 0)
        exhausted = bool(total and float(current_record.payload.get("cost_used") or 0) >= total)
        blocked = "external_outcome_unknown" if external_unknown else "cost_unknown" if reason == "cost_unknown" else "budget_exhausted" if status == "budget_exhausted" or exhausted else reason if reason in {"authorization_unresolved", "capability_unavailable"} else ""
        current = {
            **current_record.payload, "current_cycle_id": "", "last_cycle_id": cycle_id,
            "next_wake_at": None if blocked else _dt(now + timedelta(seconds=definition["schedule"]["interval_seconds"])),
            "blocked_reason": blocked,
        }
        if status == "completed" and updated.get("completeness") == "complete" and not error:
            current["last_success_at"] = _dt(now)
        with self.platform.transaction():
            self.store.put(task_id, "monitor_cycle", updated, record_id=cycle_id, expected_revision=record.revision, parent_id=monitor_id)
            self.store.put(task_id, "monitor_current", current, record_id=monitor_id, expected_revision=current_record.revision, natural_key=monitor_id)
        self._finish_notification(task_id, definition, updated)
        return updated

    def _finish_notification(self, task_id: str, definition: dict[str, Any], cycle: dict[str, Any]) -> None:
        preferences = definition.get("notification") or {}
        failure = cycle["status"] in {"failed", "partial", "budget_exhausted"} or cycle.get("end_reason") not in {"", "source_incomplete"}
        completion = cycle["status"] == "completed" and preferences.get("notify_on_completion") is True
        if not failure and not completion:
            return
        key = f"monitor.cycle_notice:{cycle['cycle_id']}:{cycle['status']}:{cycle['end_reason']}"
        if self.store.idempotent(task_id, "monitor.cycle_notice", key, cycle) is not None:
            return
        payload = {
            "notification_id": new_id("snotice"), "task_id": task_id,
            "monitor_id": definition["monitor_id"], "root_object_key": definition["monitor_id"],
            "kind": "cycle_failed" if failure else "cycle_completed",
            "change_ids": [], "candidate_ids": [], "cycle_id": cycle["cycle_id"],
            "status": "pending", "created_at": _dt(self.clock()), "delivered_at": None,
            "window_start": _dt(self.clock()), "window_end": _dt(self.clock()),
            "channel": "external" if preferences.get("external_enabled") else "task",
            "external_channel": preferences.get("external_channel") if preferences.get("external_enabled") else "",
            "detail": {"end_reason": cycle["end_reason"], "error": cycle.get("error", "")},
        }
        self.store.put(task_id, "monitor_notification", payload, record_id=payload["notification_id"], parent_id=definition["monitor_id"])
        self.store.remember(task_id, "monitor.cycle_notice", key, cycle, payload)

    async def deliver_notifications(self, task_id: str) -> list[dict[str, Any]]:
        delivered: list[dict[str, Any]] = []
        for row in self.store.all(task_id, "monitor_notification"):
            if row.payload.get("status") != "pending":
                continue
            if self.notify is None:
                continue
            try:
                await self.notify(row.payload)
            except Exception as exc:
                self.store.put(task_id, "monitor_notification", {**row.payload, "delivery_error": f"{type(exc).__name__}: {exc}"}, record_id=row.record_id, expected_revision=row.revision, parent_id=row.parent_id)
                continue
            updated = {**row.payload, "status": "sent", "delivered_at": _dt(self.clock()), "delivery_error": ""}
            self.store.put(task_id, "monitor_notification", updated, record_id=row.record_id, expected_revision=row.revision, parent_id=row.parent_id)
            delivered.append(updated)
        return delivered

    def handle_candidate(self, task_id: str, candidate_id: str, *, expected_revision: int, outcome: str) -> dict[str, Any]:
        record = self.store.require(task_id, "monitor_candidate", candidate_id)
        if record.revision != expected_revision:
            raise SrcMonitorError("version_conflict", "candidate revision is stale")
        payload = {**record.payload, "status": "handled", "handled_at": _dt(self.clock()), "outcome": outcome}
        self.store.put(task_id, "monitor_candidate", payload, record_id=candidate_id, expected_revision=expected_revision, parent_id=record.parent_id)
        return payload

    def recover(self, task_id: str) -> dict[str, Any]:
        try:
            result = self._recover_task(task_id)
        except Exception as exc:
            self._health_update(
                task_id, state="degraded", last_error=self._exception_view(exc),
                last_failure_at=_dt(self.clock()),
            )
            raise
        self._health_update(
            task_id, state="running" if task_id in self._schedulers else "stopped",
            last_recovery=result, last_error=None,
        )
        return result

    def recover_all(self, task_ids: Optional[list[str]] = None) -> dict[str, Any]:
        if task_ids is None:
            with self.platform.lock:
                rows = list(self.platform.conn.execute("SELECT DISTINCT task_id FROM src_records WHERE kind='monitor_current'").fetchall())
            task_ids = sorted({str(row[0]) for row in rows} | set(self._schedulers) | set(self._scheduler_health))
        results: dict[str, Any] = {}
        for task_id in task_ids:
            try:
                results[task_id] = self.recover(task_id)
            except Exception as exc:
                results[task_id] = {"error": self._exception_view(exc)}
        return results

    def _recover_task(self, task_id: str) -> dict[str, Any]:
        recovered: list[str] = []
        blocked: list[str] = []
        for row in self.store.all(task_id, "monitor_cycle"):
            if row.payload.get("status") not in {"scheduled", "running"}:
                continue
            cycle_id = row.record_id
            monitor_id = row.payload["monitor_id"]
            current = self.store.require(task_id, "monitor_current", monitor_id)
            external_unknown = row.payload.get("dispatch_state") == "dispatched"
            updated = {
                **row.payload, "status": "failed", "finished_at": _dt(self.clock()),
                "end_reason": "external_outcome_unknown" if external_unknown else "restart_interrupted",
                "dispatch_state": "outcome_unknown" if external_unknown else "finished",
            }
            self.store.put(task_id, "monitor_cycle", updated, record_id=cycle_id, expected_revision=row.revision, parent_id=monitor_id)
            self.store.put(task_id, "monitor_current", {
                **current.payload, "current_cycle_id": "", "last_cycle_id": cycle_id,
                "blocked_reason": "external_outcome_unknown" if external_unknown else "",
                "next_wake_at": None if external_unknown else _dt(self.clock()),
            }, record_id=monitor_id, expected_revision=current.revision, natural_key=monitor_id)
            self._finish_notification(task_id, self._cycle_definition(task_id, updated), updated)
            if self.platform.get(ResourceLease, "src.monitor.cycle", f"{task_id}:{monitor_id}") is not None:
                self.platform.release_lease("src.monitor.cycle", f"{task_id}:{monitor_id}")
            recovered.append(cycle_id)
            if external_unknown:
                blocked.append(cycle_id)
        return {"recovered": recovered, "blocked_external_unknown": blocked}

    def reconcile_external(self, task_id: str, cycle_id: str, receipt: dict[str, Any]) -> dict[str, Any]:
        record = self.store.require(task_id, "monitor_cycle", cycle_id)
        dispatch_state = record.payload.get("dispatch_state")
        unknown_cost = bool((record.payload.get("cost") or {}).get("unknown"))
        if dispatch_state not in {"outcome_unknown", "reconciled"} and not (dispatch_state == "finished" and unknown_cost):
            return record.payload
        if dispatch_state == "reconciled" and not unknown_cost:
            return record.payload
        cost = dict(receipt.get("cost") or {})
        known_cost = cost.get("actual") is not None and cost.get("unknown") is not True
        try:
            actual = float(cost["actual"]) if known_cost else 0.0
        except (TypeError, ValueError) as exc:
            raise SrcMonitorError("source_result_invalid", "来源费用回执不是有效数字") from exc
        if not math.isfinite(actual) or actual < 0:
            raise SrcMonitorError("source_result_invalid", "来源实际费用不能为负数")
        definition = self._cycle_definition(task_id, record.payload)
        current_record = self.store.require(task_id, "monitor_current", record.payload["monitor_id"])
        if current_record.payload.get("last_cycle_id") != cycle_id or current_record.payload.get("current_cycle_id"):
            raise SrcMonitorError("resource_busy", "只能结算当前待核对的监控轮次")
        previous_actual = float((record.payload.get("cost") or {}).get("actual") or 0)
        cost_used = float(current_record.payload.get("cost_used") or 0) + (actual - previous_actual if known_cost else 0)
        total = float(definition["budget"].get("total") or 0)
        reserved = float((record.payload.get("cost") or {}).get("reserved") or 0)
        over_budget = known_cost and (actual > reserved or bool(total and cost_used >= total))
        blocked = "budget_exhausted" if over_budget else "" if known_cost else "cost_unknown" if receipt else "external_outcome_unknown"
        updated = {
            **record.payload, "external_receipt": receipt,
            "dispatch_state": "reconciled" if receipt else dispatch_state,
            "reconciled_at": _dt(self.clock()) if receipt else record.payload.get("reconciled_at"),
            "end_reason": blocked if blocked else "reconciled",
            "status": "budget_exhausted" if over_budget else record.payload["status"],
        }
        if known_cost:
            updated["cost"] = {**record.payload.get("cost", {}), "actual": actual, "unknown": False}
        current_updates: dict[str, Any] = {
            **current_record.payload,
            "cost_used": cost_used, "cost_unknown": not known_cost,
            "blocked_reason": blocked,
            "next_wake_at": _dt(self.clock()) if not blocked and current_record.payload.get("status") == "active" else None,
        }
        with self.platform.transaction():
            if known_cost:
                self._cost_ledger(task_id, cycle_id, "reconciled", actual - previous_actual, False, {"receipt": receipt})
            self.store.put(
                task_id, "monitor_current", current_updates,
                record_id=record.payload["monitor_id"], expected_revision=current_record.revision,
                natural_key=record.payload["monitor_id"],
            )
            self.store.put(task_id, "monitor_cycle", updated, record_id=cycle_id, expected_revision=record.revision, parent_id=record.parent_id)
        return updated

    def list_changes(self, task_id: str, monitor_id: str) -> list[dict[str, Any]]:
        return [row.payload for row in self.store.all(task_id, "monitor_change") if row.payload.get("monitor_id") == monitor_id]

    def list_notifications(self, task_id: str, monitor_id: str) -> list[dict[str, Any]]:
        return [row.payload for row in self.store.all(task_id, "monitor_notification") if row.payload.get("monitor_id") == monitor_id]


__all__ = [
    "CycleTimeout", "ExternalOutcomeUnknown", "SrcMonitorError", "SrcMonitoringService",
]
