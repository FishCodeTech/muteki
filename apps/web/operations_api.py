"""产品运维观测、诊断包与显式维护入口。"""

from __future__ import annotations

import hashlib
import io
import json
import os
import shutil
import time
import zipfile
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from fastapi import APIRouter
from fastapi.responses import Response
from pydantic import BaseModel, Field

from muteki.competition.models import (
    PlatformConnection,
    PlatformKind,
    PlatformSubmission,
    SyncCursor,
)
from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.objects import AgentSession
from muteki.platform.reconciler import unfinished_receipts_by_command_type

def _age_seconds(value: str | None) -> float:
    if not value:
        return 0.0
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return max(0.0, (datetime.now(timezone.utc) - parsed).total_seconds())
    except (TypeError, ValueError):
        return 0.0


class MaintenanceApplyBody(BaseModel):
    confirm: bool
    candidate_ids: list[str] = Field(default_factory=list, max_length=1000)


class ProductObservability:
    """从真实存储和服务状态导出安全的产品观测数据。"""

    def __init__(self, stack: Any) -> None:
        self.stack = stack
        self.started_at = time.time()
        self._sse = Counter()

    def open_sse(self, stream: str, *, resumed: bool) -> None:
        self._sse[f"{stream}.opened_total"] += 1
        self._sse[f"{stream}.active"] += 1
        if resumed:
            self._sse[f"{stream}.resumed_total"] += 1

    def close_sse(self, stream: str) -> None:
        self._sse[f"{stream}.closed_total"] += 1
        self._sse[f"{stream}.active"] = max(
            0, self._sse[f"{stream}.active"] - 1)

    @staticmethod
    def _outbox_summary(conn: Any, table: str) -> dict[str, Any]:
        rows = conn.execute(
            f"SELECT status, COUNT(*) AS count, MIN(created_at) AS oldest "  # noqa: S608
            f"FROM {table} GROUP BY status"  # noqa: S608
        ).fetchall()
        by_state = {str(row["status"]): int(row["count"]) for row in rows}
        oldest = max((_age_seconds(row["oldest"]) for row in rows
                      if row["status"] != "delivered"), default=0.0)
        return {"by_state": by_state, "oldest_pending_seconds": round(oldest, 3)}

    def metrics(self) -> dict[str, Any]:
        platform = self.stack.store.snapshot()
        competition_conn = self.stack.competition_store.conn
        platform_conn = self.stack.store.conn
        submissions = self.stack.competition_store.list(PlatformSubmission)
        submission_states = Counter(item.state for item in submissions)
        sessions = self.stack.store.list(AgentSession)
        closed_durations = [
            (item.closed_at - item.created_at).total_seconds()
            for item in sessions if item.closed_at is not None
        ]
        event_rows = platform_conn.execute(
            "SELECT event_type, COUNT(*) AS count FROM domain_events "
            "GROUP BY event_type"
        ).fetchall()
        runtime_events = {
            str(row["event_type"]): int(row["count"])
            for row in event_rows
            if str(row["event_type"]).startswith("core.")
            and any(word in str(row["event_type"])
                    for word in ("session", "turn", "approval", "tool"))
        }
        sync_cursors = self.stack.competition_store.list(SyncCursor)
        watermark_lags = [
            max(0, platform.event_watermark - int(value))
            for value in platform.projection_watermarks.values()
        ]
        effects = self.stack.store.list_effect_receipts(limit=5000)
        effect_latencies: dict[str, list[float]] = {}
        orphan_effects = 0
        for effect in effects:
            if effect.completed_at is not None:
                effect_latencies.setdefault(effect.destination, []).append(
                    (effect.completed_at - effect.created_at).total_seconds())
            domain = self.stack.store.command_domain(effect.command_id)
            found = (
                self.stack.competition_store.get_receipt(effect.command_id)
                if domain == "competition"
                else self.stack.store.get_receipt(effect.command_id)
            )
            if found is None:
                orphan_effects += 1
        start_latencies: list[float] = []
        pending_turn: dict[str, datetime] = {}
        for row in platform_conn.execute(
            "SELECT aggregate_id, event_type, occurred_at FROM domain_events "
            "WHERE event_type IN ('core.turn.requested','core.session.started') "
            "ORDER BY seq"
        ).fetchall():
            try:
                occurred = datetime.fromisoformat(
                    str(row["occurred_at"]).replace("Z", "+00:00"))
            except ValueError:
                continue
            aggregate_id = str(row["aggregate_id"])
            if row["event_type"] == "core.turn.requested":
                pending_turn[aggregate_id] = occurred
            elif aggregate_id in pending_turn:
                start_latencies.append(
                    max(0.0, (occurred - pending_turn.pop(aggregate_id)).total_seconds()))
        return {
            "uptime_seconds": round(time.time() - self.started_at, 3),
            "platform": {
                "event_watermark": platform.event_watermark,
                "pending_receipts": platform.pending_receipts,
                "pending_receipts_by_type": unfinished_receipts_by_command_type(
                    self.stack.store),
                "projection_lag_max": max(watermark_lags, default=0),
                "outbox": self._outbox_summary(platform_conn, "outbox"),
                "effects": dict(Counter(item.state.value for item in effects)),
                "effect_duration_avg_seconds": {
                    destination: round(sum(values) / len(values), 3)
                    for destination, values in effect_latencies.items()
                },
                "orphan_effects": orphan_effects,
            },
            "competition": {
                "pending_receipts": len(self.stack.competition_store.pending_receipts()),
                "outbox": self._outbox_summary(
                    competition_conn, "competition_outbox"),
                "submissions": dict(submission_states),
                "sync_cursors": len(sync_cursors),
                "sync_errors": 0,
            },
            "runtime": {
                "registered_instances": len(self.stack.registry.instances()),
                "sessions_total": len(sessions),
                "sessions_active": sum(1 for item in sessions if item.closed_at is None),
                "closed_duration_avg_seconds": (
                    round(sum(closed_durations) / len(closed_durations), 3)
                    if closed_durations else 0.0
                ),
                "start_duration_avg_seconds": (
                    round(sum(start_latencies) / len(start_latencies), 3)
                    if start_latencies else 0.0
                ),
                "events": runtime_events,
            },
            "sse": dict(self._sse),
            "extensions": {
                "installed": len(self.stack.extension.list_records()),
                "enabled": sum(
                    1 for item in self.stack.extension.list_records() if item.enabled),
                "start_count": sum(
                    item.start_count for item in self.stack.extension.list_records()),
                "degraded": sum(
                    1 for item in self.stack.extension.list_records()
                    if item.state.value in {"degraded", "unavailable"}),
            },
        }

    def alerts(self, metrics: dict[str, Any] | None = None) -> list[dict[str, Any]]:
        data = metrics or self.metrics()
        alerts: list[dict[str, Any]] = []
        for domain in ("platform", "competition"):
            outbox = data[domain]["outbox"]
            if outbox["oldest_pending_seconds"] >= 60:
                alerts.append({
                    "code": f"{domain}.outbox.delayed",
                    "severity": "warning",
                    "detail": f"最早未完成 outbox 已等待 {int(outbox['oldest_pending_seconds'])} 秒",
                })
            if data[domain]["pending_receipts"]:
                by_type = data[domain].get("pending_receipts_by_type") or {}
                breakdown = "，".join(f"{name} {count}" for name, count in by_type.items())
                alerts.append({
                    "code": f"{domain}.receipt.pending",
                    "severity": "warning",
                    "detail": f"存在 {data[domain]['pending_receipts']} 个未完成回执"
                              + (f"（{breakdown}）" if breakdown else ""),
                    **({"by_type": dict(by_type)} if by_type else {}),
                })
        if data["platform"]["projection_lag_max"]:
            alerts.append({
                "code": "platform.projection.lag",
                "severity": "warning",
                "detail": f"最大投影延迟 {data['platform']['projection_lag_max']} 个事件",
            })
        if data["extensions"]["degraded"]:
            alerts.append({
                "code": "extension.health.degraded",
                "severity": "warning",
                "detail": f"{data['extensions']['degraded']} 个扩展处于降级状态",
            })
        frequent = [
            item.extension_id for item in self.stack.extension.list_records()
            if item.enabled and item.start_count >= 5
        ]
        if frequent:
            alerts.append({
                "code": "extension.restart.frequent",
                "severity": "warning",
                "detail": f"频繁重启扩展：{', '.join(frequent)}",
            })
        if data["platform"]["orphan_effects"]:
            alerts.append({
                "code": "platform.effect.orphan",
                "severity": "critical",
                "detail": f"存在 {data['platform']['orphan_effects']} 个孤立效果回执",
            })
        dead = data["platform"]["outbox"]["by_state"].get("dead_letter", 0)
        dead += data["competition"]["outbox"]["by_state"].get("dead_letter", 0)
        if dead:
            alerts.append({
                "code": "outbox.dead_letter",
                "severity": "critical",
                "detail": f"存在 {dead} 个 dead-letter 外部效果",
            })
        return alerts

    def overview(self) -> dict[str, Any]:
        metrics = self.metrics()
        root_hash = hashlib.sha256(str(self.stack.root).encode()).hexdigest()[:12]
        return {
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "instance": {
                "id": root_hash,
                "pid": os.getpid(),
                "backend_url": os.environ.get(
                    "MUTEKI_BACKEND_URL",
                    f"http://127.0.0.1:{os.environ.get('MUTEKI_WEB_PORT', '8000')}",
                ),
                "control_port": self.stack.control_receiver_status.get("port"),
            },
            "health": self.stack.health(),
            "metrics": metrics,
            "alerts": self.alerts(metrics),
        }

    def outbox_rows(self, *, limit: int = 200) -> list[dict[str, Any]]:
        rows: list[dict[str, Any]] = []
        for domain, conn, table in (
            ("platform", self.stack.store.conn, "outbox"),
            ("competition", self.stack.competition_store.conn,
             "competition_outbox"),
        ):
            for row in conn.execute(
                f"SELECT outbox_id, command_id, destination, status, attempts, "  # noqa: S608
                f"created_at, updated_at, last_error FROM {table} "  # noqa: S608
                "ORDER BY created_at DESC LIMIT ?", (int(limit),),
            ).fetchall():
                rows.append({
                    "domain": domain,
                    "outbox_id": row["outbox_id"],
                    "command_id": row["command_id"],
                    "destination": row["destination"],
                    "status": row["status"],
                    "attempts": int(row["attempts"]),
                    "created_at": row["created_at"],
                    "updated_at": row["updated_at"],
                    "last_error": row["last_error"],
                })
        return sorted(rows, key=lambda item: str(item["created_at"]), reverse=True)[:limit]

    async def receipt_trace(self, command_id: str) -> dict[str, Any]:
        receipt = await self.stack.routed_command_api.get_receipt(command_id)
        domain = self.stack.store.command_domain(command_id)
        effects = self.stack.store.effects_for_command(command_id)
        outbox = (
            self.stack.competition_store.outbox.for_command(command_id)
            if domain == "competition"
            else []
        )
        if domain == "platform":
            rows = self.stack.store.conn.execute(
                "SELECT payload FROM outbox WHERE command_id=? ORDER BY created_at",
                (command_id,),
            ).fetchall()
            from muteki.platform.contracts.receipts import OutboxRecord
            outbox = [OutboxRecord.model_validate_json(row["payload"]) for row in rows]
        return {
            "domain": domain,
            "receipt": receipt.model_dump(mode="json"),
            "effects": [item.model_dump(mode="json") for item in effects],
            "outbox": [{
                "outbox_id": item.outbox_id,
                "destination": item.destination,
                "status": item.status.value,
                "attempts": item.attempts,
                "created_at": item.created_at.isoformat(),
                "delivered_at": item.delivered_at.isoformat()
                if item.delivered_at else None,
                "last_error": item.last_error,
            } for item in outbox],
        }

    def recovery_preview(self) -> dict[str, Any]:
        platform = self.stack.store.snapshot()
        competition_pending = self.stack.competition_store.pending_receipts()
        event_gaps = self.stack.store.conn.execute(
            "SELECT COUNT(*) FROM ("
            "SELECT aggregate_type, aggregate_id, MAX(stream_seq)-COUNT(*) AS gap "
            "FROM domain_events GROUP BY aggregate_type, aggregate_id HAVING gap != 0)"
        ).fetchone()[0]
        return {
            "dry_run": True,
            "writes_performed": False,
            "platform": {
                "event_watermark": platform.event_watermark,
                "stream_gap_count": int(event_gaps),
                "pending_receipts": platform.pending_receipts,
                "pending_receipts_by_type": unfinished_receipts_by_command_type(
                    self.stack.store),
                "pending_outbox": platform.pending_outbox,
                "projection_watermarks": platform.projection_watermarks,
            },
            "competition": {
                "event_watermark": self.stack.competition_store.event_watermark(),
                "pending_receipts": len(competition_pending),
                "pending_outbox": len(self.stack.competition_store.outbox.pending(
                    limit=1000)),
            },
            "last_startup_recovery": self.stack.recovery,
        }

    def maintenance_preview(self) -> dict[str, Any]:
        now = time.time()
        candidates: list[dict[str, Any]] = []
        root = self.stack.root.resolve()
        for path in root.rglob("*.tmp"):
            try:
                if path.is_file() and now - path.stat().st_mtime >= 3600:
                    relative = path.resolve().relative_to(root)
                    candidates.append({
                        "candidate_id": f"temp:{relative.as_posix()}",
                        "kind": "temporary_file",
                        "target": relative.as_posix(),
                        "age_seconds": int(now - path.stat().st_mtime),
                    })
            except (OSError, ValueError):
                continue
        referenced = {
            Path(value).resolve()
            for record in self.stack.extension.list_records()
            for value in record.install_dirs.values()
        }
        install_root = self.stack.extension.install_root.resolve()
        if install_root.is_dir():
            for path in install_root.glob("*/*"):
                if path.is_dir() and path.resolve() not in referenced:
                    relative = path.resolve().relative_to(root)
                    candidates.append({
                        "candidate_id": f"extension:{relative.as_posix()}",
                        "kind": "orphan_extension_package",
                        "target": relative.as_posix(),
                    })
        for connection in self.stack.competition_store.list(PlatformConnection):
            if connection.platform_kind != PlatformKind.GENERIC_BROWSER.value:
                continue
            try:
                adapter = self.stack.competition_services.adapters.for_connection(
                    connection)
                status = adapter.storage_state_status(
                    self.stack.competition_services.adapters.connection_ref(connection))
            except Exception:
                continue
            if status.get("present") and status.get("expired"):
                candidates.append({
                    "candidate_id": f"browser:{connection.connection_id}",
                    "kind": "expired_browser_session",
                    "target": connection.connection_id,
                    "expires_at": status.get("expires_at"),
                })
        return {
            "dry_run": True,
            "writes_performed": False,
            "candidates": candidates,
            "count": len(candidates),
        }

    def apply_maintenance(self, candidate_ids: list[str]) -> dict[str, Any]:
        preview = self.maintenance_preview()
        available = {item["candidate_id"]: item for item in preview["candidates"]}
        selected = candidate_ids or list(available)
        unknown = [item for item in selected if item not in available]
        if unknown:
            raise ValueError(f"unknown or stale maintenance candidates: {unknown}")
        root = self.stack.root.resolve()
        results = []
        for candidate_id in selected:
            item = available[candidate_id]
            if candidate_id.startswith("browser:"):
                connection = self.stack.competition_store.get(
                    PlatformConnection, item["target"])
                if connection is None:
                    raise ValueError(f"connection disappeared: {item['target']}")
                adapter = self.stack.competition_services.adapters.for_connection(
                    connection)
                changed = adapter.revoke_storage_state(
                    self.stack.competition_services.adapters.connection_ref(connection))
            else:
                path = (root / item["target"]).resolve()
                path.relative_to(root)
                changed = path.exists()
                if path.is_dir():
                    shutil.rmtree(path)
                elif path.exists():
                    path.unlink()
            results.append({**item, "removed": bool(changed)})
        audit = root / "operations" / "maintenance.jsonl"
        audit.parent.mkdir(parents=True, exist_ok=True)
        entry = {
            "at": datetime.now(timezone.utc).isoformat(),
            "actor": "local-user",
            "candidate_ids": selected,
            "results": results,
        }
        with audit.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry, ensure_ascii=False) + "\n")
        return {"applied": len(results), "results": results, "audit": "operations/maintenance.jsonl"}

    def diagnostic_bundle(self) -> bytes:
        documents = {
            "overview.json": self.overview(),
            "recovery-preview.json": self.recovery_preview(),
            "outbox.json": {"items": self.outbox_rows()},
            "maintenance-preview.json": self.maintenance_preview(),
        }
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, payload in documents.items():
                archive.writestr(
                    name,
                    json.dumps(payload, ensure_ascii=False, indent=2,
                               sort_keys=True) + "\n",
                )
        return buffer.getvalue()


def create_operations_router(observability: ProductObservability) -> APIRouter:
    router = APIRouter(prefix="/api/operations", tags=["operations"])

    @router.get("/overview")
    async def overview() -> Any:
        return observability.overview()

    @router.get("/outbox")
    async def outbox(limit: int = 200) -> Any:
        return {"items": observability.outbox_rows(limit=max(1, min(limit, 1000)))}

    @router.get("/receipts/{command_id}")
    async def receipt(command_id: str) -> Any:
        try:
            return await observability.receipt_trace(command_id)
        except CommandAPIError as exc:
            # Only a missing receipt is a 404; storage or decoding failures
            # must surface as real errors rather than "not found".
            if exc.error.category is not ErrorCategory.NOT_FOUND:
                raise
            return Response(
                content=json.dumps({"error": {
                    "code": exc.error.code,
                    "message": exc.error.message,
                }}, ensure_ascii=False),
                status_code=404,
                media_type="application/json",
            )

    @router.get("/recovery-preview")
    async def recovery_preview() -> Any:
        return observability.recovery_preview()

    @router.get("/maintenance-preview")
    async def maintenance_preview() -> Any:
        return observability.maintenance_preview()

    @router.post("/maintenance")
    async def maintenance(body: MaintenanceApplyBody) -> Any:
        if not body.confirm:
            return Response(
                content=json.dumps({"error": {
                    "code": "operations.maintenance.confirm_required",
                    "message": "维护清理需要 confirm=true",
                }}, ensure_ascii=False),
                status_code=400,
                media_type="application/json",
            )
        try:
            return observability.apply_maintenance(body.candidate_ids)
        except ValueError as exc:
            return Response(
                content=json.dumps({"error": {
                    "code": "operations.maintenance.stale_preview",
                    "message": str(exc),
                }}, ensure_ascii=False),
                status_code=409,
                media_type="application/json",
            )

    @router.get("/diagnostic-bundle")
    async def diagnostic_bundle() -> Response:
        return Response(
            observability.diagnostic_bundle(),
            media_type="application/zip",
            headers={
                "Content-Disposition": "attachment; filename=muteki-diagnostic.zip",
                "X-Content-Type-Options": "nosniff",
            },
        )

    return router


__all__ = [
    "MaintenanceApplyBody",
    "ProductObservability",
    "create_operations_router",
]
