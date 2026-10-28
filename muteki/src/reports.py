from __future__ import annotations

import hashlib
import json
from datetime import datetime, timezone
from typing import Any, Iterable, Optional

from muteki.platform.contracts.base import new_id

from .artifacts import SrcArtifactStore
from .contracts import ArtifactRef
from .store import SrcStore, SrcStoreError

REPORT_SCHEMA = "muteki.src.submission.v1"
RENDERER_VERSION = "src-submission-renderer.v1"
TEMPLATE_SCHEMA = "muteki.src.report-template.v1"

CANONICAL_FIELDS = (
    "title", "target", "target_version", "scope", "attacker", "function",
    "mechanism", "normal_behavior", "actual_behavior", "steps",
    "decisive_evidence", "confirmed_impact", "conditional_impact", "cvss",
    "remediation", "counterevidence", "limitations", "restoration", "attachments",
)

DEFAULT_TEMPLATES: dict[str, dict[str, Any]] = {
    "generic-zh-v1": {
        "template_id": "generic-zh-v1", "platform": "generic",
        "template_version": "1.0.0", "mapping_version": "1.0.0",
        "source_url": "builtin://src/generic-zh-v1", "source_fetched_at": "2026-10-26T00:00:00Z",
        "field_mapping": {
            "title": "漏洞标题", "target": "影响目标", "attacker": "攻击者前提",
            "mechanism": "漏洞原理", "steps": "复现步骤", "confirmed_impact": "漏洞危害",
            "cvss": "CVSS 3.1", "remediation": "修复建议",
        },
        "constraints": {"required": ["title", "target", "mechanism", "steps", "confirmed_impact"]},
    },
    "enterprise-src-v1": {
        "template_id": "enterprise-src-v1", "platform": "enterprise_src",
        "template_version": "1.0.0", "mapping_version": "1.0.0",
        "source_url": "builtin://src/enterprise-src-v1", "source_fetched_at": "2026-10-26T00:00:00Z",
        "field_mapping": {
            "title": "漏洞名称", "target": "受影响系统", "attacker": "利用条件",
            "mechanism": "漏洞分析", "steps": "验证过程", "confirmed_impact": "风险与影响",
            "cvss": "风险评分", "remediation": "整改建议",
        },
        "constraints": {"required": ["title", "target", "attacker", "mechanism", "steps", "confirmed_impact", "remediation"]},
    },
    "edusrc-v1": {
        "template_id": "edusrc-v1", "platform": "edusrc",
        "template_version": "1.0.0", "mapping_version": "1.0.0",
        "source_url": "builtin://src/edusrc-v1", "source_fetched_at": "2026-10-26T00:00:00Z",
        "field_mapping": {
            "title": "漏洞标题", "target": "漏洞位置", "attacker": "利用前提",
            "mechanism": "漏洞成因", "steps": "测试过程", "confirmed_impact": "危害说明",
            "cvss": "CVSS", "remediation": "修复方案",
        },
        "constraints": {"required": ["title", "target", "mechanism", "steps", "confirmed_impact"]},
    },
}


def utcnow() -> datetime:
    return datetime.now(timezone.utc)


def artifact_ref(value: ArtifactRef | dict[str, Any]) -> ArtifactRef:
    return value if isinstance(value, ArtifactRef) else ArtifactRef.model_validate(value)


def normalize_template(template: str | dict[str, Any]) -> dict[str, Any]:
    if isinstance(template, str):
        if template not in DEFAULT_TEMPLATES:
            raise SrcStoreError("artifact_missing", f"unknown report template {template!r}")
        return json.loads(json.dumps(DEFAULT_TEMPLATES[template]))
    required = {"template_id", "platform", "template_version", "mapping_version", "source_url", "source_fetched_at", "field_mapping", "constraints"}
    missing = sorted(required - set(template))
    if missing:
        raise SrcStoreError("version_conflict", f"custom template is incomplete: {', '.join(missing)}")
    result = json.loads(json.dumps(template))
    result["schema"] = TEMPLATE_SCHEMA
    if not isinstance(result["field_mapping"], dict) or not isinstance(result["constraints"], dict):
        raise SrcStoreError("version_conflict", "template mapping and constraints must be objects")
    return result


def canonical_document(document: dict[str, Any]) -> dict[str, Any]:
    def items(value: Any) -> list[Any]:
        if value is None:
            return []
        if isinstance(value, str):
            return [value]
        return list(value)

    result = dict(document)
    for field in CANONICAL_FIELDS:
        result.setdefault(field, [] if field in {"steps", "decisive_evidence", "attachments", "counterevidence", "limitations"} else "")
    result["steps"] = [dict(step) if isinstance(step, dict) else {"action": str(step)} for step in items(result["steps"])]
    result["attachments"] = items(result["attachments"])
    result["decisive_evidence"] = items(result["decisive_evidence"])
    result["counterevidence"] = items(result["counterevidence"])
    result["limitations"] = items(result["limitations"])
    missing = [field for field in ("title", "target", "mechanism", "steps", "confirmed_impact") if not result.get(field)]
    result["missing_requirements"] = sorted(set(result.get("missing_requirements") or []) | set(missing))
    return result


class SrcReportService:
    def __init__(self, store: SrcStore, artifacts: SrcArtifactStore) -> None:
        self.store = store
        self.artifacts = artifacts

    def _read_evidence(self, task_id: str, refs: Iterable[ArtifactRef | dict[str, Any]]) -> list[ArtifactRef]:
        verified = []
        for value in refs:
            ref = artifact_ref(value)
            self.artifacts.read(ref, expected_task_id=task_id)
            verified.append(ref)
        return verified

    def freeze(
        self, task_id: str, *, finding_id: str, source_run_id: str,
        document: dict[str, Any], evidence_refs: Iterable[ArtifactRef | dict[str, Any]],
        claims: list[dict[str, Any]], review: dict[str, Any], cvss: dict[str, Any],
        deduplication: dict[str, Any], template: str | dict[str, Any],
        plan_snapshot: dict[str, Any], authorization_snapshot: dict[str, Any],
        source_watermarks: dict[str, Any], idempotency_key: str,
        actor_id: str, supersedes: Optional[str] = None,
        feedback_responses: Optional[list[dict[str, Any]]] = None,
    ) -> dict[str, Any]:
        if not finding_id or not source_run_id:
            raise SrcStoreError("task_binding_invalid", "finding_id and source_run_id are required")
        evidence_list = [artifact_ref(value) for value in evidence_refs]
        request = {
            "finding_id": finding_id, "source_run_id": source_run_id,
            "document": document, "claims": claims, "review": review,
            "evidence_refs": [ref.model_dump(mode="json") for ref in evidence_list],
            "cvss": cvss, "deduplication": deduplication, "template": template,
            "plan_snapshot": plan_snapshot, "authorization_snapshot": authorization_snapshot,
            "source_watermarks": source_watermarks, "supersedes": supersedes,
            "feedback_responses": feedback_responses or [],
        }
        with self.store.platform.transaction():
            previous = self.store.idempotent(task_id, "freeze_report", idempotency_key, request)
            if previous is not None:
                return previous
            evidence = self._read_evidence(task_id, evidence_list)
            canonical = canonical_document(document)
            selected_template = normalize_template(template)
            missing_template = [field for field in selected_template["constraints"].get("required", []) if not canonical.get(field)]
            raw_readiness = review.get("readiness")
            if isinstance(raw_readiness, str):
                readiness_status = raw_readiness.strip().upper()
            elif isinstance(raw_readiness, dict):
                readiness_status = str(raw_readiness.get("status") or "").strip().upper()
            else:
                readiness_status = ""
            if readiness_status == "GREEN":
                if review.get("validation_type") != "live_reproduction":
                    raise SrcStoreError("version_conflict", "GREEN requires live_reproduction, not artifact review")
                reproduction = dict(review.get("independent_reproduction") or {})
                if not reproduction.get("success"):
                    raise SrcStoreError("version_conflict", "GREEN requires a successful independent reproduction record")
                reproduction_id = str(reproduction.get("reproduction_id") or "")
                persisted = self.store.get(task_id, "report_reproduction", reproduction_id)
                if persisted is None or not persisted.payload.get("success"):
                    raise SrcStoreError("version_conflict", "GREEN reproduction reference is missing or unsuccessful")
                if not evidence:
                    raise SrcStoreError("version_conflict", "GREEN requires readable evidence")
            if supersedes:
                old = self.get(task_id, supersedes)
                if old["finding_id"] != finding_id:
                    raise SrcStoreError("version_conflict", "a report version cannot supersede another Finding")
            siblings = [row for row in self.store.all(task_id, "frozen_report") if row.payload.get("finding_id") == finding_id]
            report_seq = max((int(row.payload.get("report_seq") or 0) for row in siblings), default=0) + 1
            report_id = new_id("sreport")
            source_run_ids = {source_run_id}
            source_run_ids.update(ref.source_run_id for ref in evidence if ref.source_run_id)
            source_run_ids.update(str(item.get("source_run_id")) for item in canonical.get("exchanges") or [] if item.get("source_run_id"))
            frozen = {
                "schema": REPORT_SCHEMA, "product_mode": "src", "task_id": task_id,
                "source_run_id": source_run_id, "source_run_ids": sorted(source_run_ids), "finding_id": finding_id,
                "report_id": report_id, "report_seq": report_seq,
                "document": canonical, "claims": claims,
                "evidence_refs": [ref.model_dump(mode="json") for ref in evidence],
                "review": review, "cvss": cvss, "deduplication": deduplication,
                "template": selected_template, "renderer_version": RENDERER_VERSION,
                "plan_snapshot": plan_snapshot, "authorization_snapshot": authorization_snapshot,
                "source_watermarks": source_watermarks,
                "template_missing": missing_template,
                "template_unmapped": [field for field in CANONICAL_FIELDS if field not in selected_template["field_mapping"]],
                "supersedes": supersedes, "invalidates": [],
                "feedback_responses": feedback_responses or [],
                "frozen_by": actor_id, "frozen_at": utcnow().isoformat(),
            }
            frozen["digest"] = self.store.digest(frozen)
            self.store.put(
                task_id, "frozen_report", frozen, record_id=report_id,
                parent_id=finding_id, natural_key=f"{finding_id}:v{report_seq}",
            )
            result = dict(frozen)
            from .submission import SubmissionRenderer
            rendered = SubmissionRenderer(self, self.artifacts).render(result)
            result["rendered_markdown"] = rendered["markdown"]
            result["rendered_canonical"] = rendered["canonical"]
            result["rendered_sha256"] = hashlib.sha256(rendered["markdown"].encode("utf-8")).hexdigest()
            self.store.put(
                task_id, "frozen_report", result, record_id=report_id,
                parent_id=finding_id, natural_key=f"{finding_id}:v{report_seq}",
            )
            self.store.remember(task_id, "freeze_report", idempotency_key, request, result)
            return result

    def templates(self) -> dict[str, Any]:
        from .submission import DEFAULT_TEMPLATES
        return DEFAULT_TEMPLATES

    def get(self, task_id: str, report_id: str) -> dict[str, Any]:
        record = self.store.require(task_id, "frozen_report", report_id)
        report = record.payload
        derived_fields = {"digest", "rendered_markdown", "rendered_canonical", "rendered_sha256"}
        if report.get("digest") != self.store.digest({key: value for key, value in report.items() if key not in derived_fields}):
            raise SrcStoreError("artifact_hash_mismatch", "frozen report payload does not match its digest")
        if report.get("rendered_sha256") and report["rendered_sha256"] != hashlib.sha256(report.get("rendered_markdown", "").encode("utf-8")).hexdigest():
            raise SrcStoreError("artifact_hash_mismatch", "frozen report renderer output does not match its digest")
        return report

    def list(self, task_id: str, *, finding_id: str = "") -> list[dict[str, Any]]:
        return [row.payload for row in self.store.all(task_id, "frozen_report")
                if not finding_id or row.payload.get("finding_id") == finding_id]

    def evidence(self, task_id: str, report: dict[str, Any]) -> list[ArtifactRef]:
        return self._read_evidence(task_id, report.get("evidence_refs") or [])

    def record_image_review(
        self, task_id: str, value: ArtifactRef | dict[str, Any], *,
        reviewer: str, judgment: str, reviewed_at: str = "",
    ) -> dict[str, Any]:
        import io
        from PIL import Image

        ref = artifact_ref(value)
        content = self.artifacts.read(ref, expected_task_id=task_id)
        if not ref.media_type.startswith("image/") or not reviewer or not judgment:
            raise SrcStoreError("version_conflict", "image review requires an image, reviewer and judgment")
        with Image.open(io.BytesIO(content)) as image:
            width, height = image.size
            image.verify()
        record = {
            "review_id": new_id("simgreview"), "sha256": ref.sha256,
            "artifact_sha256": ref.sha256, "media_type": ref.media_type,
            "width": width, "height": height, "reviewed_by": reviewer,
            "reviewed_at": reviewed_at or utcnow().isoformat(),
            "judgment": judgment, "status": "reviewed",
        }
        self.store.put(task_id, "image_review", record, record_id=record["review_id"], parent_id=ref.artifact_id)
        return record

    def record_independent_reproduction(
        self, task_id: str, report_id: str, *, reviewer: str,
        original_researcher: str, executed_steps: list[str],
        skipped_steps: list[dict[str, str]], success: bool,
        evidence_refs: Iterable[ArtifactRef | dict[str, Any]],
        execution: Optional[dict[str, Any]] = None,
        blockers: Optional[list[str]] = None,
    ) -> dict[str, Any]:
        self.get(task_id, report_id)
        if reviewer == original_researcher:
            raise SrcStoreError("version_conflict", "independent reviewer must differ from the original researcher")
        evidence = self._read_evidence(task_id, evidence_refs)
        if success and (
            not executed_steps or skipped_steps or blockers
            or not evidence or not execution
            or not execution.get("command") or int(execution.get("exit_code", -1)) != 0
        ):
            raise SrcStoreError("version_conflict", "successful reproduction requires executed steps, readable evidence, the actual command and zero exit code")
        record = {
            "reproduction_id": new_id("srepro"), "report_id": report_id,
            "reviewer": reviewer, "original_researcher": original_researcher,
            "executed_steps": executed_steps, "skipped_steps": skipped_steps,
            "success": bool(success and executed_steps and evidence and not skipped_steps and not blockers),
            "execution": execution or {},
            "evidence_refs": [ref.model_dump(mode="json") for ref in evidence],
            "blockers": blockers or [], "created_at": utcnow().isoformat(),
        }
        self.store.put(task_id, "report_reproduction", record, record_id=record["reproduction_id"], parent_id=report_id)
        return record

    def templates(self) -> list[dict[str, Any]]:
        return [normalize_template(template_id) for template_id in DEFAULT_TEMPLATES]


__all__ = [
    "CANONICAL_FIELDS", "DEFAULT_TEMPLATES", "RENDERER_VERSION", "REPORT_SCHEMA",
    "SrcReportService", "artifact_ref", "canonical_document", "normalize_template", "utcnow",
]
