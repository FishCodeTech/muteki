from __future__ import annotations

from typing import Any, Iterable, Optional

from muteki.platform.contracts.base import new_id

from .artifacts import SrcArtifactStore
from .contracts import ArtifactRef
from .reports import SrcReportService, artifact_ref, utcnow
from .store import SrcStoreError

PLATFORM_STATES = {
    "not_submitted", "submitted", "pending_info", "accepted", "duplicate",
    "rejected", "reward_confirmed", "reward_paid",
}


class SrcFeedbackService:
    def __init__(self, reports: SrcReportService, artifacts: SrcArtifactStore) -> None:
        self.reports = reports
        self.artifacts = artifacts
        self.store = reports.store

    def record_feedback(
        self, task_id: str, report_id: str, *, text: str = "", source: str,
        actor_id: str, attachments: Iterable[ArtifactRef | dict[str, Any]] = (),
        platform_reference: str = "", received_at: str = "",
    ) -> dict[str, Any]:
        self.reports.get(task_id, report_id)
        attachment_rows = []
        for value in attachments:
            ref = artifact_ref(value)
            self.artifacts.read(ref, expected_task_id=task_id)
            attachment_rows.append(ref.model_dump(mode="json"))
        if not text.strip() and not attachment_rows:
            raise SrcStoreError("version_conflict", "platform feedback requires text or attachments")
        artifact = (
            self.artifacts.put(task_id, text.encode("utf-8"), media_type="text/plain", name="platform-feedback.txt")
            if text.strip() else None
        )
        record = {
            "feedback_id": new_id("sfeedback"), "task_id": task_id, "report_id": report_id,
            "text_ref": artifact.model_dump(mode="json") if artifact else None, "source": source,
            "platform_reference": platform_reference, "attachments": attachment_rows,
            "received_at": received_at or utcnow().isoformat(), "recorded_by": actor_id,
            "trust": "untrusted_research_input_not_authorization",
        }
        self.store.put(task_id, "platform_feedback", record, record_id=record["feedback_id"], parent_id=report_id)
        return record

    def diff(self, task_id: str, old_report_id: str, new_report_id: str) -> dict[str, Any]:
        old = self.reports.get(task_id, old_report_id)
        new = self.reports.get(task_id, new_report_id)
        if old["finding_id"] != new["finding_id"]:
            raise SrcStoreError("version_conflict", "report diff requires versions of one Finding")
        changes = []
        paths = {
            "claims": ("claims",), "impact": ("document", "confirmed_impact"),
            "conditional_impact": ("document", "conditional_impact"),
            "steps": ("document", "steps"), "cvss": ("cvss",),
            "evidence": ("evidence_refs",), "readiness": ("review", "readiness"),
            "template": ("template",), "document": ("document",),
        }
        for name, path in paths.items():
            before, after = old, new
            for key in path:
                before, after = before[key], after[key]
            if before != after:
                changes.append({"field": name, "path": ".".join(path), "before": before, "after": after})
        responses = {row.get("feedback_id"): row for row in new.get("feedback_responses") or []}
        finding_report_ids = {
            str(row.get("report_id") or "")
            for row in self.reports.list(task_id, finding_id=old["finding_id"])
            if int(row.get("report_seq") or 0) <= int(new.get("report_seq") or 0)
        } | {old_report_id, new_report_id}
        feedback = []
        for row in self.store.all(task_id, "platform_feedback"):
            item = row.payload
            if item["report_id"] not in finding_report_ids:
                continue
            response = responses.get(item["feedback_id"], {"status": "unknown"})
            feedback.append({**item, "response": response})
        return {
            "finding_id": old["finding_id"], "old_report_id": old_report_id,
            "new_report_id": new_report_id, "changes": changes, "feedback": feedback,
            "unresolved": [row for row in feedback if row["response"].get("status") != "answered"],
            "note": "包含主张、影响、步骤、CVSS、证据、准备度和模板变化；不仅是正文文本 diff",
        }

    def record_platform_status(
        self, task_id: str, report_id: str, *, status: str,
        actor_id: str, actor_kind: str, base_revision: int,
        platform_reference: str = "", amount: Optional[float] = None,
        notes: str = "",
    ) -> dict[str, Any]:
        if actor_kind != "operator":
            raise SrcStoreError("policy_denied", "only human operator can record platform status")
        if status not in PLATFORM_STATES:
            raise SrcStoreError("version_conflict", f"unsupported platform status {status!r}")
        if amount is not None and (isinstance(amount, bool) or amount < 0):
            raise SrcStoreError("version_conflict", "reward amount must be non-negative")
        report = self.reports.get(task_id, report_id)
        with self.store.platform.transaction():
            history = [row.payload for row in self.store.all(task_id, "platform_status")
                       if row.payload.get("report_id") == report_id]
            current_revision = max((int(row.get("revision") or 0) for row in history), default=0)
            if base_revision != current_revision:
                raise SrcStoreError("version_conflict", "platform status base revision is stale",
                                    detail={"expected": base_revision, "actual": current_revision})
            previous = max(history, key=lambda row: int(row.get("revision") or 0), default=None)
            record = {
                "status_id": new_id("splatform"), "task_id": task_id, "report_id": report_id,
                "finding_id": report["finding_id"], "revision": current_revision + 1,
                "status": status, "platform_reference": platform_reference,
                "amount": amount, "notes": notes,
                "old_value": previous, "new_value": {
                    "status": status, "platform_reference": platform_reference,
                    "amount": amount, "notes": notes,
                },
                "recorded_by": actor_id, "recorded_at": utcnow().isoformat(),
                "verification": "用户记录，未与平台核验",
            }
            self.store.put(task_id, "platform_status", record, record_id=record["status_id"], parent_id=report_id)
        return record

    def platform_status(self, task_id: str, report_id: str) -> dict[str, Any]:
        self.reports.get(task_id, report_id)
        history = [row.payload for row in self.store.all(task_id, "platform_status")
                   if row.payload.get("report_id") == report_id]
        if not history:
            return {
                "task_id": task_id, "report_id": report_id, "revision": 0,
                "status": "not_submitted", "verification": "用户记录，未与平台核验",
            }
        return max(history, key=lambda row: int(row.get("revision") or 0))

    def history(self, task_id: str, report_id: str) -> list[dict[str, Any]]:
        return sorted(
            (row.payload for row in self.store.all(task_id, "platform_status") if row.payload.get("report_id") == report_id),
            key=lambda row: int(row.get("revision") or 0),
        )


__all__ = ["PLATFORM_STATES", "SrcFeedbackService"]
