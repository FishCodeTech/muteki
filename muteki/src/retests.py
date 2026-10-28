from __future__ import annotations

from typing import Any, Iterable, Optional

from muteki.platform.contracts.runs import RunCommand

from .artifacts import SrcArtifactStore
from .contracts import ArtifactRef
from .reports import SrcReportService, artifact_ref, utcnow
from .store import SrcStore, SrcStoreError

RESTORATION_STATES = {"restored", "partially_restored", "not_restored", "unknown", "not_applicable"}


class SrcRetestService:
    def __init__(self, store: SrcStore, artifacts: SrcArtifactStore, reports: SrcReportService, run_gateway: Any = None) -> None:
        self.store = store
        self.artifacts = artifacts
        self.reports = reports
        self.run_gateway = run_gateway

    def request(
        self, task_id: str, report_id: str, *, finding_id: str,
        repair_statement: str, expected_target_version: str,
        authorization_ref: str, requester: str, idempotency_key: str,
        identity_refs: Optional[list[str]] = None,
        original_poc_refs: Optional[list[str]] = None,
        baseline_refs: Optional[list[str]] = None,
        budget: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        report = self.reports.get(task_id, report_id)
        if report["finding_id"] != finding_id:
            raise SrcStoreError("task_binding_invalid", "report does not belong to this Finding")
        if not repair_statement or not expected_target_version or not authorization_ref:
            raise SrcStoreError("version_conflict", "repair statement, expected target version and current authorization are required")
        request = {
            "report_id": report_id, "finding_id": finding_id,
            "repair_statement": repair_statement, "expected_target_version": expected_target_version,
            "authorization_ref": authorization_ref, "requester": requester,
            "identity_refs": identity_refs or [], "original_poc_refs": original_poc_refs or [],
            "baseline_refs": baseline_refs or [], "budget": budget or {},
        }
        with self.store.platform.transaction():
            prior = self.store.idempotent(task_id, "request_finding_retest", idempotency_key, request)
            if prior is not None:
                return self.store.require(task_id, "finding_retest", prior["retest_id"]).payload
            retest_id = f"sretest-{self.store.digest({**request, 'idempotency_key': idempotency_key})[:24]}"
            record = {
                "retest_id": retest_id, "task_id": task_id, "finding_id": finding_id,
                "report_id": report_id, "source_run_id": report["source_run_id"],
                "repair_statement": repair_statement, "expected_target_version": expected_target_version,
                "authorization_ref": authorization_ref,
                "identity_refs": identity_refs or [], "original_poc_refs": original_poc_refs or [],
                "baseline_refs": baseline_refs or [], "requester": requester,
                "idempotency_key": idempotency_key, "budget": budget or {},
                "result": None, "status": "requested", "evidence_refs": [],
                "gaps": [], "restoration": {"status": "not_applicable"},
                "created_at": utcnow().isoformat(), "finished_at": None,
            }
            self.store.put(task_id, "finding_retest", record, record_id=retest_id, parent_id=finding_id,
                           natural_key=f"{finding_id}:{report_id}:{idempotency_key}")
            self.store.remember(task_id, "request_finding_retest", idempotency_key, request, record)
        return record

    async def dispatch(self, task_id: str, retest_id: str) -> dict[str, Any]:
        current = self.store.require(task_id, "finding_retest", retest_id)
        record = current.payload
        if record["status"] not in {"requested", "dispatch_failed"}:
            return record
        if record.get("recovery_required"):
            return record
        if self.run_gateway is None:
            raise SrcStoreError("capability_unavailable", "RunGateway is unavailable for original-Run retest")
        directive = {
            "kind": "src_finding_retest", "retest_id": retest_id,
            "finding_id": record["finding_id"], "report_id": record["report_id"],
            "repair_statement": record["repair_statement"],
            "expected_target_version": record["expected_target_version"],
            "authorization_ref": record["authorization_ref"],
            "identity_refs": record["identity_refs"],
            "original_poc_refs": record["original_poc_refs"],
            "baseline_refs": record["baseline_refs"],
            "budget": record.get("budget") or {},
            "instruction": "在原 Run/SharedGraph 上验证此 Finding 的指定修复路径。先核验身份、对象、目标版本与正常对照；保存现场证据和恢复读回，不新建 Run。",
        }
        receipt = await self.run_gateway.command(record["source_run_id"], RunCommand(
            command_type="operator_directive", command_id=f"retest-{retest_id}",
            actor_id=record["requester"], payload={"text": directive, "scope": "global"},
        ))
        updated = {
            **record, "status": "dispatch_failed" if receipt.error else "dispatched",
            "directive_receipt": receipt.model_dump(mode="json"),
            "dispatch_error": receipt.error.model_dump(mode="json") if receipt.error else None,
            "recovery_required": bool(receipt.error),
        }
        self.store.put(task_id, "finding_retest", updated, record_id=retest_id,
                       parent_id=record["finding_id"], expected_revision=current.revision,
                       natural_key=current.natural_key)
        return updated

    def complete(
        self, task_id: str, retest_id: str, *,
        preconditions_valid: bool, target_version_confirmed: bool,
        original_path_impact: Optional[bool], normal_control_works: Optional[bool],
        evidence_refs: Iterable[ArtifactRef | dict[str, Any]],
        reviewer: str, gaps: Optional[list[str]] = None,
        restoration: Optional[dict[str, Any]] = None,
        outcome_unknown: bool = False,
        substitute_poc: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        current = self.store.require(task_id, "finding_retest", retest_id)
        record = current.payload
        evidence = []
        for value in evidence_refs:
            ref = artifact_ref(value)
            self.artifacts.read(ref, expected_task_id=task_id)
            evidence.append(ref.model_dump(mode="json"))
        verdict_input = {
            "preconditions_valid": preconditions_valid,
            "target_version_confirmed": target_version_confirmed,
            "original_path_impact": original_path_impact,
            "normal_control_works": normal_control_works,
            "evidence_refs": evidence, "reviewer": reviewer,
            "gaps": gaps or [], "restoration": restoration or {},
            "outcome_unknown": outcome_unknown, "substitute_poc": substitute_poc,
        }
        if record.get("result") is not None:
            if record.get("verdict_input") != verdict_input:
                raise SrcStoreError("version_conflict", "retest is already complete with different evidence")
            return record
        missing = list(gaps or [])
        if not preconditions_valid:
            missing.append("原复现前提未成立")
        if not target_version_confirmed:
            missing.append("修复目标版本未确认")
        if normal_control_works is not True:
            missing.append("正常业务对照未确认工作")
        if original_path_impact is None:
            missing.append("原攻击路径结果未知")
        if outcome_unknown:
            missing.append("执行结果未知，需要核对原调用回执")
        if not evidence:
            missing.append("缺少现场证据")
        if original_path_impact is True and preconditions_valid and target_version_confirmed and evidence and not outcome_unknown and not missing:
            result = "still_vulnerable"
        elif (original_path_impact is False and preconditions_valid and target_version_confirmed
              and normal_control_works is True and evidence and not outcome_unknown and not missing):
            result = "fixed"
        else:
            result = "inconclusive"
        restoration = restoration or {"status": "not_applicable"}
        if restoration.get("status") not in RESTORATION_STATES:
            raise SrcStoreError("version_conflict", "invalid restoration status")
        updated = {
            **record, "result": result, "status": "completed",
            "evidence_refs": evidence, "gaps": list(dict.fromkeys(missing)),
            "restoration": restoration, "reviewer": reviewer,
            "verdict_input": verdict_input, "substitute_poc": substitute_poc,
            "scope_limit": "仅针对原报告路径、已确认目标版本、身份和已测影响，不证明整个资产无漏洞",
            "recovery_required": outcome_unknown,
            "finished_at": utcnow().isoformat(),
        }
        with self.store.platform.transaction():
            self.store.put(task_id, "finding_retest", updated, record_id=retest_id,
                           parent_id=record["finding_id"], expected_revision=current.revision,
                           natural_key=current.natural_key)
            review = {
                "review_id": f"sreview-{retest_id}", "finding_id": record["finding_id"],
                "report_id": record["report_id"], "validation_type": "repair_retest",
                "result": result, "reviewer": reviewer, "evidence_refs": evidence,
                "gaps": updated["gaps"], "created_at": updated["finished_at"],
                "retest_id": retest_id,
            }
            self.store.put(task_id, "retest_review", review, record_id=review["review_id"], parent_id=retest_id)
        return updated

    def recover(self, task_id: str) -> list[dict[str, Any]]:
        recovered = []
        for row in self.store.all(task_id, "finding_retest"):
            record = row.payload
            if record.get("result") is None and record.get("status") == "dispatched":
                updated = {**record, "recovery_required": True,
                           "recovery_note": "已有下发记录但无最终复测结论；先核对原 Run 回执，不自动重发或宣布 fixed"}
                self.store.put(task_id, "finding_retest", updated, record_id=row.record_id,
                               parent_id=row.parent_id, expected_revision=row.revision,
                               natural_key=row.natural_key)
                recovered.append(updated)
        return recovered


__all__ = ["RESTORATION_STATES", "SrcRetestService"]
