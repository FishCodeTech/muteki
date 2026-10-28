from __future__ import annotations

from datetime import datetime
from typing import Any, Iterable, Mapping, Optional

from pydantic import Field

from muteki.platform.contracts.base import ContractModel, new_id, utcnow

from .artifacts import SrcArtifactStore
from .contracts import ArtifactRef, ReviewRecord
from .cvss import CVSSAssessment, evaluate_cvss
from .store import SrcStore, SrcStoreError

CLAIM_KIND = "finding_claim"
REVIEW_KIND = "finding_review"
STALE_KIND = "finding_stale"
DEDUP_KIND = "finding_dedup"
FROZEN_REPORT_KINDS = ("report", "frozen_report", "report_version")


class ClaimVersion(ContractModel):
    claim_id: str
    finding_id: str
    task_id: str
    version: int
    text: str
    status: str
    evidence_refs: list[ArtifactRef] = Field(default_factory=list)
    counterevidence_refs: list[ArtifactRef] = Field(default_factory=list)
    source_run_id: Optional[str] = None
    fact_ref: str = ""
    exchange_refs: list[str] = Field(default_factory=list)
    target_version: str = ""
    identity_refs: list[str] = Field(default_factory=list)
    limitations: list[str] = Field(default_factory=list)
    missing_proof: list[dict[str, Any]] = Field(default_factory=list)
    created_at: datetime = Field(default_factory=utcnow)
    supersedes: str = ""
    digest: str = ""


class DedupDecision(ContractModel):
    decision_id: str
    task_id: str
    finding_id: str
    outcome: str
    candidate: dict[str, Any] = Field(default_factory=dict)
    comparisons: list[dict[str, Any]] = Field(default_factory=list)
    history_refs: list[dict[str, str]] = Field(default_factory=list)
    history_complete: bool
    reasons: list[str] = Field(default_factory=list)
    created_at: datetime = Field(default_factory=utcnow)


def _require(condition: bool, code: str, message: str, detail: Optional[dict[str, Any]] = None) -> None:
    if not condition:
        raise SrcStoreError(code, message, detail=detail or {})


def _artifact_refs(values: Iterable[Any]) -> list[ArtifactRef]:
    return [value if isinstance(value, ArtifactRef) else ArtifactRef.model_validate(value) for value in values]


def _children(store: SrcStore, task_id: str, kind: str, parent_id: str) -> list[Any]:
    rows: list[Any] = []
    cursor = 0
    while True:
        page = store.list(task_id, kind, parent_id=parent_id, after_rowid=cursor, limit=500)
        if not page:
            return rows
        rows.extend(page)
        cursor = store.rowid(page[-1])


class SrcFindingService:
    def __init__(self, store: SrcStore, artifacts: Optional[SrcArtifactStore] = None) -> None:
        self.store = store
        self.artifacts = artifacts

    def _verify_artifacts(self, task_id: str, refs: Iterable[ArtifactRef]) -> None:
        refs = list(refs)
        if refs and self.artifacts is None:
            raise SrcStoreError("capability_unavailable", "artifact byte verification is required")
        for ref in refs:
            _require(ref.task_id == task_id, "task_binding_invalid", "evidence belongs to another Task")
            if self.artifacts is not None:
                self.artifacts.read(ref, expected_task_id=task_id)

    def _claims(self, task_id: str, finding_id: str) -> list[Any]:
        return _children(self.store, task_id, CLAIM_KIND, finding_id)

    def list_claims(self, task_id: str, finding_id: str) -> list[dict[str, Any]]:
        return [row.payload for row in self._claims(task_id, finding_id)]

    def _current_claims(self, task_id: str, finding_id: str) -> list[dict[str, Any]]:
        latest: dict[str, dict[str, Any]] = {}
        for row in self._claims(task_id, finding_id):
            claim_id = str(row.payload.get("claim_id") or "")
            if claim_id not in latest or int(row.payload.get("version") or 0) > int(latest[claim_id].get("version") or 0):
                latest[claim_id] = row.payload
        return [
            {"claim_id": row.get("claim_id"), "version": row.get("version"), "digest": row.get("digest")}
            for row in latest.values()
        ]

    def get_claim(self, task_id: str, finding_id: str, claim_id: str, version: Optional[int] = None) -> ClaimVersion:
        rows = [row for row in self._claims(task_id, finding_id) if row.payload.get("claim_id") == claim_id]
        _require(bool(rows), "artifact_missing", "claim is unavailable")
        row = max(rows, key=lambda item: int(item.payload.get("version") or 0)) if version is None else next(
            (item for item in rows if int(item.payload.get("version") or 0) == version), None,
        )
        _require(row is not None, "version_conflict", "claim version is unavailable")
        return ClaimVersion.model_validate(row.payload)

    def create_claim(
        self, task_id: str, text: str, status: str, *,
        finding_id: str = "", claim_id: str = "",
        evidence_refs: Iterable[Any] = (), counterevidence_refs: Iterable[Any] = (),
        source_run_id: Optional[str] = None, fact_ref: str = "",
        exchange_refs: Iterable[str] = (), target_version: str = "",
        identity_refs: Iterable[str] = (), limitations: Iterable[str] = (),
        missing_proof: Iterable[dict[str, Any]] = (), idempotency_key: str = "",
    ) -> ClaimVersion:
        _require(status in {"observed", "supported_inference", "unverified"}, "claim_status_invalid", "unsupported claim status")
        _require(bool(str(text or "").strip()), "claim_text_required", "claim text is required")
        evidence = _artifact_refs(list(evidence_refs))
        counterevidence = _artifact_refs(list(counterevidence_refs))
        exchange_ref_list = list(exchange_refs)
        identity_ref_list = list(identity_refs)
        limitations_list = list(limitations)
        missing_proof_list = list(missing_proof)
        if status != "unverified":
            _require(bool(evidence), "evidence_required", "an observed or inferred claim needs source evidence")
        self._verify_artifacts(task_id, [*evidence, *counterevidence])
        request = {
            "finding_id": finding_id, "claim_id": claim_id, "text": text, "status": status,
            "evidence_refs": [ref.model_dump(mode="json") for ref in evidence],
            "counterevidence_refs": [ref.model_dump(mode="json") for ref in counterevidence],
            "source_run_id": source_run_id, "fact_ref": fact_ref,
            "exchange_refs": exchange_ref_list, "target_version": target_version,
            "identity_refs": identity_ref_list, "limitations": limitations_list,
            "missing_proof": missing_proof_list,
        }
        if idempotency_key:
            prior = self.store.idempotent(task_id, "finding.claim", idempotency_key, request)
            if prior is not None:
                return ClaimVersion.model_validate(prior)
        finding_id = finding_id or new_id("sfind")
        claim_id = claim_id or new_id("sclaim")
        with self.store.platform.transaction():
            existing = [row for row in self._claims(task_id, finding_id) if row.payload.get("claim_id") == claim_id]
            version = max((int(row.payload.get("version") or 0) for row in existing), default=0) + 1
            supersedes = max(existing, key=lambda row: int(row.payload.get("version") or 0)).record_id if existing else ""
            claim = ClaimVersion(
                claim_id=claim_id, finding_id=finding_id, task_id=task_id,
                version=version, text=text.strip(), status=status,
                evidence_refs=evidence, counterevidence_refs=counterevidence,
                source_run_id=source_run_id, fact_ref=fact_ref,
                exchange_refs=exchange_ref_list, target_version=target_version,
                identity_refs=identity_ref_list, limitations=limitations_list,
                missing_proof=missing_proof_list, supersedes=supersedes,
            )
            claim = claim.model_copy(update={"digest": self.store.digest(claim.model_dump(mode="json"))})
            self.store.put(
                task_id, CLAIM_KIND, claim.model_dump(mode="json"),
                record_id=f"{claim_id}:v{version}", parent_id=finding_id,
                natural_key=f"{claim_id}:v{version}",
            )
            self._mark_stale(
                task_id, finding_id, "claim_or_evidence_changed",
                dependency_digest=claim.digest, source_refs=[f"{claim_id}:v{version}"],
            )
            if idempotency_key:
                self.store.remember(task_id, "finding.claim", idempotency_key, request, claim.model_dump(mode="json"))
        return claim

    def _reviews(self, task_id: str, finding_id: str) -> list[Any]:
        return _children(self.store, task_id, REVIEW_KIND, finding_id)

    def list_reviews(self, task_id: str, finding_id: str) -> list[dict[str, Any]]:
        return [row.payload for row in self._reviews(task_id, finding_id)]

    def _latest_review(self, task_id: str, finding_id: str) -> Optional[Any]:
        rows = self._reviews(task_id, finding_id)
        return max(rows, key=lambda row: self.store.rowid(row)) if rows else None

    def _mark_stale(
        self, task_id: str, finding_id: str, reason: str, *,
        dependency_digest: str, source_refs: Iterable[str] = (),
    ) -> None:
        latest = self._latest_review(task_id, finding_id)
        if latest is None:
            return
        signature = {
            "finding_id": finding_id, "review_id": latest.payload.get("review_id"),
            "reason": reason, "dependency_digest": dependency_digest,
            "source_refs": list(source_refs),
        }
        key = self.store.digest(signature)
        if self.store.idempotent(task_id, "finding.stale", key, signature) is not None:
            return
        marker = {**signature, "created_at": utcnow().isoformat()}
        record = self.store.put(
            task_id, STALE_KIND, marker, parent_id=finding_id,
            natural_key=key,
        )
        self.store.remember(task_id, "finding.stale", key, signature, record.model_dump(mode="json"))

    def invalidate(
        self, task_id: str, finding_id: str, reason: str, *,
        dependency_digest: str, source_refs: Iterable[str] = (),
    ) -> None:
        _require(bool(reason and dependency_digest), "validation_failed", "stale reason and dependency digest are required")
        with self.store.platform.transaction():
            self._mark_stale(task_id, finding_id, reason, dependency_digest=dependency_digest, source_refs=source_refs)

    def refresh_staleness(
        self, task_id: str, finding_id: str, *,
        dependencies: Optional[Mapping[str, str]] = None,
    ) -> bool:
        latest = self._latest_review(task_id, finding_id)
        if latest is None:
            return False
        claims = []
        for selected in latest.payload.get("claim_versions") or self._current_claims(task_id, finding_id):
            current = self.get_claim(task_id, finding_id, str(selected.get("claim_id") or ""))
            claims.append({"claim_id": current.claim_id, "version": current.version, "digest": current.digest})
        evidence = _artifact_refs(latest.payload.get("evidence_snapshot") or [])
        try:
            self._verify_artifacts(task_id, evidence)
        except SrcStoreError:
            self.invalidate(
                task_id, finding_id, "source_revoked",
                dependency_digest=self.store.digest({"evidence": [ref.sha256 for ref in evidence]}),
                source_refs=[ref.artifact_id for ref in evidence],
            )
            return True
        digest = self.store.digest({
            "claims": claims, "dependencies": dict(dependencies or latest.payload.get("dependencies") or {}),
            "evidence": [ref.sha256 for ref in evidence],
        })
        if digest == latest.payload.get("dependency_digest"):
            return False
        self.invalidate(
            task_id, finding_id, "dependencies_changed", dependency_digest=digest,
            source_refs=[row.record_id for row in self._claims(task_id, finding_id)],
        )
        return True

    def review_status(self, task_id: str, finding_id: str) -> dict[str, Any]:
        latest = self._latest_review(task_id, finding_id)
        if latest is None:
            return {"finding_id": finding_id, "state": "unreviewed", "review": None, "stale": False}
        markers = _children(self.store, task_id, STALE_KIND, finding_id)
        newer = [row for row in markers if self.store.rowid(row) > self.store.rowid(latest) and not row.payload.get("cleared")]
        stale = bool(newer)
        return {
            "finding_id": finding_id, "review": latest.payload,
            "state": "stale" if stale else latest.payload.get("readiness"),
            "stale": stale,
            "stale_reasons": [row.payload.get("reason") for row in newer],
            "stale_refs": [row.payload for row in newer],
        }

    def _verify_execution(self, task_id: str, receipt_ids: Iterable[str]) -> None:
        for receipt_id in receipt_ids:
            receipt = self.store.require(task_id, "execution_receipt", receipt_id).payload
            _require(
                receipt.get("dispatch_state") == "completed" and receipt.get("outcome_known") is True,
                "review_not_live", "live reproduction needs a known completed execution receipt",
                {"receipt_id": receipt_id, "state": receipt.get("dispatch_state")},
            )

    def _validate_review(
        self, task_id: str, validation_type: str, readiness: str,
        reasons: list[str], missing: list[dict[str, Any]],
        alternatives: list[dict[str, Any]], evidence: list[ArtifactRef],
        executed_steps: list[str], skipped_steps: list[dict[str, str]],
        target_version: str, independent_context: bool,
        execution_receipt_ids: list[str], retest_id: str, cvss: Optional[CVSSAssessment],
    ) -> None:
        _require(validation_type in {"artifact_review", "live_reproduction", "repair_retest"}, "validation_type_invalid", "unknown validation type")
        _require(readiness in {"GREEN", "YELLOW", "RED"}, "readiness_invalid", "unknown readiness")
        if validation_type == "repair_retest":
            _require(bool(retest_id), "retest_required", "repair_retest requires retest_id")
            retest_record = self.store.get(task_id, "finding_retest", retest_id) or self.store.require(task_id, "retest", retest_id)
            retest = retest_record.payload
            _require(retest.get("result") in {"fixed", "still_vulnerable", "inconclusive"}, "retest_incomplete", "retest has no terminal result")
        if readiness == "RED":
            _require(bool(reasons), "review_reason_required", "RED requires concrete reasons")
            return
        if readiness == "YELLOW":
            _require(bool(evidence), "evidence_required", "YELLOW requires supporting evidence")
            _require(bool(missing), "review_gap_required", "YELLOW requires precise evidence gaps")
            for gap in missing:
                _require(bool(str(gap.get("code") or "").strip() and str(gap.get("action") or "").strip()), "review_gap_invalid", "each gap needs code and next action")
            return
        _require(validation_type == "live_reproduction", "review_not_live", "GREEN requires independent live reproduction, not artifact review or repair retest")
        _require(independent_context, "review_not_independent", "GREEN requires a reviewer context that did not perform the original exploration")
        _require(bool(executed_steps) and not skipped_steps, "review_steps_incomplete", "GREEN requires all reproduction steps to be executed")
        _require(bool(target_version), "target_version_required", "GREEN requires the verified target version")
        _require(bool(execution_receipt_ids), "review_not_live", "GREEN requires live execution receipts")
        self._verify_execution(task_id, execution_receipt_ids)
        _require(bool(evidence), "evidence_required", "GREEN requires readable validation evidence")
        _require(bool(reasons), "review_reason_required", "GREEN requires the verification rationale")
        _require(not missing, "review_gap_unresolved", "GREEN cannot retain unresolved evidence gaps")
        _require(bool(alternatives), "counterevidence_required", "GREEN requires review of the strongest alternative explanation")
        for alternative in alternatives:
            _require(bool(str(alternative.get("explanation") or "").strip()), "counterevidence_invalid", "alternative explanation text is required")
            _require(alternative.get("status") == "addressed", "counterevidence_unresolved", "GREEN cannot retain an unaddressed alternative explanation")
            _require(bool(str(alternative.get("assessment") or "").strip()), "counterevidence_invalid", "addressed alternatives need an assessment")
        _require(cvss is not None, "cvss_required", "GREEN requires a reasoned CVSS 3.1 assessment")

    def record_review(
        self, task_id: str, finding_id: str, validation_type: str, readiness: str, *,
        report_id: Optional[str] = None, claim_versions: Iterable[dict[str, Any]] = (),
        reasons: Iterable[str] = (), missing: Iterable[dict[str, Any]] = (),
        alternative_explanations: Iterable[dict[str, Any]] = (),
        evidence_snapshot: Iterable[Any] = (), executed_steps: Iterable[str] = (),
        skipped_steps: Iterable[dict[str, str]] = (), reviewer: str,
        target_version: str = "", identity_refs: Iterable[str] = (),
        independent_context: bool = False, execution_receipt_ids: Iterable[str] = (),
        retest_id: str = "", dependencies: Optional[Mapping[str, str]] = None,
        cvss: Optional[CVSSAssessment] = None, idempotency_key: str = "",
    ) -> dict[str, Any]:
        _require(bool(reviewer.strip()), "reviewer_required", "reviewer identity is required")
        reasons_list = list(reasons)
        missing_list = list(missing)
        executed_list = list(executed_steps)
        skipped_list = list(skipped_steps)
        identity_list = list(identity_refs)
        receipt_list = list(execution_receipt_ids)
        evidence = _artifact_refs(list(evidence_snapshot))
        self._verify_artifacts(task_id, evidence)
        claims = list(claim_versions) or self._current_claims(task_id, finding_id)
        for selected in claims:
            claim = self.get_claim(task_id, finding_id, str(selected.get("claim_id") or ""), int(selected.get("version") or 0))
            _require(claim.digest == selected.get("digest"), "artifact_hash_mismatch", "claim digest changed")
        alternatives = []
        for alternative in list(alternative_explanations):
            raw_refs = alternative.get("evidence_refs") or alternative.get("counterevidence_refs") or []
            refs = _artifact_refs(raw_refs)
            if readiness == "GREEN":
                _require(bool(refs), "counterevidence_required", "addressed alternatives need original evidence")
            self._verify_artifacts(task_id, refs)
            alternatives.append({
                **alternative,
                "evidence_refs": [ref.model_dump(mode="json") for ref in refs],
            })
        if cvss is not None:
            verified_cvss = evaluate_cvss(cvss.vector, cvss.metric_reasons)
            _require(
                verified_cvss.base_score == cvss.base_score
                and verified_cvss.temporal_score == cvss.temporal_score
                and verified_cvss.environmental_score == cvss.environmental_score,
                "cvss_mismatch", "CVSS scores do not match the vector and metric reasons",
            )
            cvss = verified_cvss
        self._validate_review(
            task_id, validation_type, readiness, reasons_list, missing_list, alternatives,
            evidence, executed_list, skipped_list, target_version,
            independent_context, receipt_list, retest_id, cvss,
        )
        request = {
            "finding_id": finding_id, "report_id": report_id,
            "validation_type": validation_type, "readiness": readiness,
            "claim_versions": claims, "reasons": reasons_list, "missing": missing_list,
            "alternative_explanations": alternatives,
            "evidence_snapshot": [ref.model_dump(mode="json") for ref in evidence],
            "executed_steps": executed_list, "skipped_steps": skipped_list,
            "reviewer": reviewer, "target_version": target_version,
            "identity_refs": identity_list, "independent_context": independent_context,
            "execution_receipt_ids": receipt_list, "retest_id": retest_id,
            "dependencies": dict(dependencies or {}),
            "cvss": cvss.model_dump(mode="json") if cvss else None,
        }
        if idempotency_key:
            prior = self.store.idempotent(task_id, "finding.review", idempotency_key, request)
            if prior is not None:
                return prior
        review = ReviewRecord(
            finding_id=finding_id, report_id=report_id,
            validation_type=validation_type, readiness=readiness,
            reasons=reasons_list, missing=missing_list, evidence_snapshot=evidence,
            executed_steps=executed_list, skipped_steps=skipped_list,
            reviewer=reviewer, target_version=target_version, identity_refs=identity_list,
        )
        previous = self._latest_review(task_id, finding_id)
        payload = {
            **review.model_dump(mode="json"),
            "claim_versions": claims, "alternative_explanations": alternatives,
            "independent_context": independent_context,
            "execution_receipt_ids": receipt_list, "retest_id": retest_id,
            "dependencies": dict(dependencies or {}),
            "dependency_digest": self.store.digest({
                "claims": claims, "dependencies": dict(dependencies or {}),
                "evidence": [ref.sha256 for ref in evidence],
            }),
            "cvss": cvss.model_dump(mode="json") if cvss else None,
            "supersedes": previous.record_id if previous else "",
        }
        with self.store.platform.transaction():
            record = self.store.put(
                task_id, REVIEW_KIND, payload, record_id=review.review_id,
                parent_id=finding_id, natural_key=review.review_id,
            )
            if idempotency_key:
                self.store.remember(task_id, "finding.review", idempotency_key, request, record.payload)
        return record.payload

    def _principal(self, task_id: str) -> str:
        return str(self.store.require(task_id, "task_binding", task_id).payload.get("principal_id") or "")

    def _history(self, task_id: str, principal: str, task_ids: Iterable[str]) -> Optional[list[dict[str, Any]]]:
        history: list[dict[str, Any]] = []
        for candidate_task in dict.fromkeys(task_ids):
            try:
                if self._principal(candidate_task) != principal:
                    return None
            except SrcStoreError:
                return None
            for kind in FROZEN_REPORT_KINDS:
                for row in self.store.all(candidate_task, kind):
                    data = row.payload
                    frozen = data.get("frozen") is True or bool(data.get("frozen_at")) or (data.get("report_id") and data.get("report_seq"))
                    if frozen:
                        history.append({
                            "task_id": candidate_task, "kind": kind, "record_id": row.record_id,
                            "report_id": str(data.get("report_id") or row.record_id), "report": data,
                        })
        return history

    def deduplicate(
        self, task_id: str, finding_id: str, candidate: Mapping[str, Any], *,
        assessments: Iterable[Mapping[str, Any]] = (), history_task_ids: Optional[Iterable[str]] = None,
        history_complete: bool = True, reasons: Iterable[str] = (), idempotency_key: str = "",
    ) -> DedupDecision:
        _require(bool(str(candidate.get("root_cause") or "").strip()), "dedup_root_cause_required", "candidate root cause is required")
        _require(bool(str(candidate.get("trust_boundary") or "").strip()), "dedup_trust_boundary_required", "candidate trust boundary is required")
        _require(bool(str(candidate.get("attack_path") or "").strip()), "dedup_attack_path_required", "candidate attack path is required")
        _require(bool(str(candidate.get("fix_point") or "").strip()), "dedup_fix_point_required", "candidate fix point is required")
        _require("impact" in candidate, "dedup_impact_required", "candidate proven and conditional impact must be explicit")
        candidate_evidence = _artifact_refs(candidate.get("evidence_refs") or [])
        _require(bool(candidate_evidence), "evidence_required", "dedup requires selected candidate evidence")
        self._verify_artifacts(task_id, candidate_evidence)
        principal = self._principal(task_id)
        task_ids = list(history_task_ids) if history_task_ids is not None else [task_id]
        request = {
            "finding_id": finding_id, "candidate": dict(candidate),
            "assessments": [dict(row) for row in assessments], "history_task_ids": task_ids,
            "history_complete": history_complete, "reasons": list(reasons),
        }
        if idempotency_key:
            prior = self.store.idempotent(task_id, "finding.dedup", idempotency_key, request)
            if prior is not None:
                return DedupDecision.model_validate(prior)
        outcome = "undetermined"
        comparisons: list[dict[str, Any]] = []
        history_refs: list[dict[str, str]] = []
        if not history_complete:
            decision_reasons = [*reasons, "历史检索缺页、水位落后或来源不可读，重复性未知"]
        else:
            history = self._history(task_id, principal, task_ids)
            history_complete = history is not None
            by_id = {row["report_id"]: row for row in history} if history is not None else {}
            supplied = {str(row.get("report_id") or ""): dict(row) for row in assessments}
            _require(not (set(supplied) - set(by_id)), "dedup_history_mismatch", "assessment references a non-frozen or unreadable report")
            decision_reasons = list(reasons)
            if history is None:
                decision_reasons.append("历史来源不可读或权限无法确认，重复性未知")
            elif not history:
                outcome = "independent"
                decision_reasons.append("当前可读冻结报告历史完整，未发现可扩充的既有修复点")
            elif set(supplied) != set(by_id):
                decision_reasons.append("部分冻结报告缺少完整比较，重复性未知")
            else:
                for report_id, row in by_id.items():
                    assessment = supplied[report_id]
                    comparison_refs = _artifact_refs(assessment.get("evidence_refs") or [])
                    complete = assessment.get("comparison_complete") is True and bool(comparison_refs)
                    if comparison_refs:
                        _require(
                            all(ref.task_id == row["task_id"] for ref in comparison_refs),
                            "task_binding_invalid", "comparison evidence belongs to another Task",
                        )
                        for ref in comparison_refs:
                            self._verify_artifacts(ref.task_id, [ref])
                    same_root = assessment.get("same_root_cause")
                    same_fix = assessment.get("same_fix_point")
                    if not complete or not isinstance(same_root, bool) or not isinstance(same_fix, bool):
                        comparisons.append({"report_id": report_id, "outcome": "undetermined", "reason": assessment.get("reason") or "比较字段或证据不完整"})
                        continue
                    result = "extend_existing" if same_root and same_fix else "independent"
                    comparisons.append({
                        "report_id": report_id, "outcome": result,
                        "same_root_cause": same_root, "same_fix_point": same_fix,
                        "reason": assessment.get("reason") or "",
                        "new_impact": assessment.get("new_impact"),
                    })
                    history_refs.append({"task_id": row["task_id"], "record_id": row["record_id"], "report_id": report_id})
                if any(row["outcome"] == "extend_existing" for row in comparisons):
                    outcome = "extend_existing" if all(row["outcome"] != "undetermined" for row in comparisons) else "undetermined"
                elif all(row["outcome"] == "independent" for row in comparisons):
                    outcome = "independent"
                decision_reasons.append("同根因且同修复点才扩充既有报告；不同根因或独立修复点保持独立，标题不参与判定")
        decision = DedupDecision(
            decision_id=new_id("sdedup"), task_id=task_id, finding_id=finding_id,
            outcome=outcome, candidate=dict(candidate), comparisons=comparisons,
            history_refs=history_refs, history_complete=history_complete,
            reasons=decision_reasons,
        )
        with self.store.platform.transaction():
            self.store.put(
                task_id, DEDUP_KIND, decision.model_dump(mode="json"),
                record_id=decision.decision_id, parent_id=finding_id,
                natural_key=decision.decision_id,
            )
            if idempotency_key:
                self.store.remember(task_id, "finding.dedup", idempotency_key, request, decision.model_dump(mode="json"))
        return decision

    def list_deduplications(self, task_id: str, finding_id: str) -> list[dict[str, Any]]:
        return [row.payload for row in _children(self.store, task_id, DEDUP_KIND, finding_id)]


__all__ = ["ClaimVersion", "DedupDecision", "SrcFindingService"]
