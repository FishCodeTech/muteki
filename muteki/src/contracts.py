from __future__ import annotations

from datetime import datetime
from typing import Any, Literal, Optional

from pydantic import Field

from muteki.platform.contracts.base import ContractModel, new_id, utcnow

ProductMode = Literal["src"]
ExecutionMode = Literal["pentest"]
MatchState = Literal["match", "no_match", "unknown"]
CaptureState = Literal["complete", "partial", "unavailable"]
ExecutionState = Literal[
    "created", "waiting_approval", "denied", "ready", "dispatched",
    "completed", "failed", "outcome_unknown", "cancelled",
]


class SrcRecord(ContractModel):
    record_id: str
    kind: str
    task_id: str
    revision: int = 1
    parent_id: str = ""
    natural_key: str = ""
    generation: int = 0
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)
    payload: dict[str, Any] = Field(default_factory=dict)


class SrcTaskBinding(ContractModel):
    task_id: str
    thread_id: str
    project_id: Optional[str] = None
    product_mode: ProductMode = "src"
    principal_id: str
    revision: int = 1
    created_at: datetime = Field(default_factory=utcnow)


class SrcRunBinding(ContractModel):
    binding_id: str = Field(default_factory=lambda: new_id("srunbind"))
    task_id: str
    thread_id: str
    run_id: str
    binding_key: str
    binding_revision: int
    product_mode: ProductMode = "src"
    execution_mode: ExecutionMode = "pentest"
    target_contract_ref: str
    contract: dict[str, Any] = Field(default_factory=dict)
    contract_artifact: dict[str, Any] = Field(default_factory=dict)
    plan_ref: str = ""
    authorization_ref: str = ""
    source_message_id: str = ""
    related_run_ids: list[str] = Field(default_factory=list)
    created_at: datetime = Field(default_factory=utcnow)


class ArtifactRef(ContractModel):
    artifact_id: str
    sha256: str
    media_type: str = "application/octet-stream"
    size: int = 0
    task_id: str
    source_run_id: Optional[str] = None
    source_event_seq: Optional[int] = None
    version: int = 1
    original: bool = True
    complete: CaptureState = "complete"
    name: str = ""


class ExecutionRule(ContractModel):
    rule_id: str = Field(default_factory=lambda: new_id("srule"))
    version: int = 1
    enabled: bool = True
    effect: Literal["allow", "ask", "deny"]
    evaluation_point: Literal["tool", "filesystem", "connection", "request"] = "tool"
    mode: Literal["enforce", "observe"] = "enforce"
    resource_kind: Literal["file", "process", "network", "http", "browser", "mcp", "control"]
    selector: dict[str, Any] = Field(default_factory=dict)
    actions: list[str] = Field(default_factory=list)
    conditions: dict[str, Any] = Field(default_factory=dict)
    reason: str = ""
    source_refs: list[str] = Field(default_factory=list)
    reusable_approval: bool = False


class ExecutionPolicy(ContractModel):
    policy_id: str = Field(default_factory=lambda: new_id("spolicy"))
    version: int = 1
    product_mode: ProductMode = "src"
    task_id: str
    source_refs: list[str] = Field(default_factory=list)
    rules: list[ExecutionRule] = Field(default_factory=list)
    defaults: dict[str, Any] = Field(default_factory=lambda: {"effect": "allow", "public_research": True})
    authorized_targets: list[str] = Field(default_factory=list)
    excluded_targets: list[str] = Field(default_factory=list)
    environment_requirements: dict[str, Any] = Field(default_factory=dict)
    network_mode: Literal["declared", "enforced"] = "declared"
    inspection_requirements: dict[str, Any] = Field(default_factory=lambda: {"mode": "off"})
    created_by: str
    created_at: datetime = Field(default_factory=utcnow)
    digest: str = ""
    supersedes: Optional[str] = None
    activation: dict[str, Any] = Field(default_factory=dict)


class ResourceFact(ContractModel):
    value: Any = None
    origin: Literal["tool_input", "environment", "connection", "request", "user", "model"]
    state: Literal["known", "unknown", "not_applicable"] = "known"


class ExecutionRequest(ContractModel):
    invocation_id: str = Field(default_factory=lambda: new_id("sinv"))
    product_mode: ProductMode = "src"
    task_id: str
    thread_id: str
    run_id: Optional[str] = None
    worker_id: Optional[str] = None
    tool_name: str
    action: str
    arguments_ref: Optional[ArtifactRef] = None
    arguments_hash: str
    arguments: dict[str, Any] = Field(default_factory=dict)
    resource_kind: str
    resource_facts: dict[str, ResourceFact] = Field(default_factory=dict)
    identity_ref: str = ""
    identity_revision: int = 0
    cwd: str = ""
    environment_id: str = ""
    environment_generation: int = 0
    policy_version: int
    created_at: datetime = Field(default_factory=utcnow)


class PolicyDecision(ContractModel):
    decision_id: str = Field(default_factory=lambda: new_id("sdec"))
    invocation_id: str
    effect: Literal["allow", "ask", "deny"]
    status: Literal["allowed", "waiting_approval", "denied", "unresolved", "unavailable"]
    matched_rule_refs: list[str] = Field(default_factory=list)
    observed_rule_refs: list[str] = Field(default_factory=list)
    unknown_fields: list[str] = Field(default_factory=list)
    policy_version: int
    capability_snapshot: dict[str, Any] = Field(default_factory=dict)
    approval_id: Optional[str] = None
    grant_id: Optional[str] = None
    reason_code: str = ""
    timestamp: datetime = Field(default_factory=utcnow)


class ExecutionApproval(ContractModel):
    approval_id: str = Field(default_factory=lambda: new_id("sapproval"))
    invocation_id: str
    input_digest: str
    request_snapshot: ExecutionRequest
    matched_rule_refs: list[str] = Field(default_factory=list)
    policy_version: int
    status: Literal["pending", "approved", "denied", "expired", "cancelled", "superseded"] = "pending"
    requested_at: datetime = Field(default_factory=utcnow)
    expires_at: datetime
    decided_by: str = ""
    decided_at: Optional[datetime] = None
    decision: Optional[Literal["approve_once", "deny", "approve_task_scope"]] = None
    grant_id: Optional[str] = None
    consumed_at: Optional[datetime] = None
    revision: int = 1


class TaskExecutionGrant(ContractModel):
    grant_id: str = Field(default_factory=lambda: new_id("sgrant"))
    task_id: str
    product_mode: ProductMode = "src"
    rule_refs: list[str] = Field(default_factory=list)
    resource_selector: dict[str, Any] = Field(default_factory=dict)
    actions: list[str] = Field(default_factory=list)
    identity_constraints: dict[str, Any] = Field(default_factory=dict)
    environment_constraints: dict[str, Any] = Field(default_factory=dict)
    input_constraints: dict[str, Any] = Field(default_factory=dict)
    source_approval_id: str
    created_by: str
    created_at: datetime = Field(default_factory=utcnow)
    expires_at: Optional[datetime] = None
    revoked_at: Optional[datetime] = None
    version: int = 1
    policy_digest: str


class ExecutionReceipt(ContractModel):
    invocation_id: str
    decision_id: str
    dispatch_state: ExecutionState
    started_at: Optional[datetime] = None
    ended_at: Optional[datetime] = None
    external_receipt_ref: str = ""
    result_ref: Optional[ArtifactRef] = None
    result: Any = None
    error_code: str = ""
    outcome_known: bool = False
    history: list[dict[str, Any]] = Field(default_factory=list)


class HttpExchange(ContractModel):
    exchange_id: str = Field(default_factory=lambda: new_id("sexchange"))
    task_id: str
    source_run_id: Optional[str] = None
    source_event_seq: Optional[int] = None
    source_kind: Literal["browser", "http_tool", "proxy", "import"]
    actor_ref: str
    context_id: str = "default"
    identity_revision: int = 0
    browser_generation: int = 0
    declared_target: str = ""
    observed_url: str = ""
    method: str
    started_at: datetime = Field(default_factory=utcnow)
    finished_at: Optional[datetime] = None
    request_raw_ref: ArtifactRef
    response_raw_ref: Optional[ArtifactRef] = None
    request_headers: list[list[str]] = Field(default_factory=list)
    response_headers: list[list[str]] = Field(default_factory=list)
    status_code: Optional[int] = None
    protocol: str = "http/1.1"
    redirect_parent: Optional[str] = None
    replay_of: Optional[str] = None
    execution_state: ExecutionState = "completed"
    capture_state: CaptureState = "complete"
    capture_limitations: list[str] = Field(default_factory=list)
    authorization_ref: str = ""
    decision_ref: str = ""
    protocol_stream_ref: str = ""
    tags: list[str] = Field(default_factory=list)
    metadata: dict[str, Any] = Field(default_factory=dict)


class RequestDraft(ContractModel):
    draft_id: str = Field(default_factory=lambda: new_id("sdraft"))
    task_id: str
    source_exchange_id: str
    revision: int = 1
    raw_request_ref: ArtifactRef
    target_url: str
    method: str
    changes: dict[str, Any] = Field(default_factory=dict)
    editor: str
    created_at: datetime = Field(default_factory=utcnow)


class IdentityVerification(ContractModel):
    verification_id: str = Field(default_factory=lambda: new_id("sidentitycheck"))
    task_id: str
    context_id: str
    identity_revision: int
    browser_generation: int
    condition_version: int
    expected: dict[str, Any] = Field(default_factory=dict)
    observed: dict[str, Any] = Field(default_factory=dict)
    status: Literal["verified", "identity_unverified", "anonymous", "forbidden", "challenge", "locked", "expired", "network_error"]
    evidence_refs: list[str] = Field(default_factory=list)
    created_at: datetime = Field(default_factory=utcnow)


class AssetObservation(ContractModel):
    observation_id: str = Field(default_factory=lambda: new_id("sobs"))
    task_id: str
    asset_id: str
    source_kind: str
    source_id: str
    source_run_id: Optional[str] = None
    source_event_seq: Optional[int] = None
    artifact_refs: list[ArtifactRef] = Field(default_factory=list)
    observed_at: Optional[datetime] = None
    ingested_at: datetime = Field(default_factory=utcnow)
    attributes: dict[str, Any] = Field(default_factory=dict)
    raw_values: dict[str, Any] = Field(default_factory=dict)
    parser_version: str
    completeness: CaptureState = "complete"
    success: Optional[bool] = None
    correction_of: Optional[str] = None
    revoked_at: Optional[datetime] = None


class AssetRelation(ContractModel):
    relation_id: str = Field(default_factory=lambda: new_id("srel"))
    task_id: str
    from_asset_id: str
    to_asset_id: str
    relation_type: str
    origin: Literal["observed", "derived", "inferred"]
    evidence_refs: list[str] = Field(default_factory=list)
    valid_from: Optional[datetime] = None
    valid_until: Optional[datetime] = None
    version: int = 1


class MonitorCycle(ContractModel):
    cycle_id: str = Field(default_factory=lambda: new_id("scycle"))
    monitor_id: str
    definition_version: int
    target_snapshot: dict[str, Any] = Field(default_factory=dict)
    scheduled_at: datetime
    started_at: Optional[datetime] = None
    finished_at: Optional[datetime] = None
    status: Literal["scheduled", "running", "completed", "partial", "failed", "skipped", "cancelled", "budget_exhausted"]
    completeness: CaptureState = "unavailable"
    source_watermarks: dict[str, Any] = Field(default_factory=dict)
    cost: dict[str, Any] = Field(default_factory=dict)
    baseline_id: str = ""
    compared_baseline_id: str = ""
    end_reason: str = ""


class ReviewRecord(ContractModel):
    review_id: str = Field(default_factory=lambda: new_id("sreview"))
    finding_id: str
    report_id: Optional[str] = None
    validation_type: Literal["artifact_review", "live_reproduction", "repair_retest"]
    readiness: Literal["GREEN", "YELLOW", "RED"]
    reasons: list[str] = Field(default_factory=list)
    missing: list[dict[str, Any]] = Field(default_factory=list)
    evidence_snapshot: list[ArtifactRef] = Field(default_factory=list)
    executed_steps: list[str] = Field(default_factory=list)
    skipped_steps: list[dict[str, str]] = Field(default_factory=list)
    reviewer: str
    target_version: str = ""
    identity_refs: list[str] = Field(default_factory=list)
    created_at: datetime = Field(default_factory=utcnow)
    stale_reason: str = ""


class RetestRecord(ContractModel):
    retest_id: str = Field(default_factory=lambda: new_id("sretest"))
    task_id: str
    finding_id: str
    report_id: str
    source_run_id: str
    repair_statement: str
    expected_target_version: str
    authorization_ref: str
    identity_refs: list[str] = Field(default_factory=list)
    original_poc_refs: list[str] = Field(default_factory=list)
    baseline_refs: list[str] = Field(default_factory=list)
    requester: str
    idempotency_key: str
    budget: dict[str, Any] = Field(default_factory=dict)
    result: Optional[Literal["fixed", "still_vulnerable", "inconclusive"]] = None
    evidence_refs: list[str] = Field(default_factory=list)
    gaps: list[str] = Field(default_factory=list)
    restoration: dict[str, Any] = Field(default_factory=dict)
    created_at: datetime = Field(default_factory=utcnow)
    finished_at: Optional[datetime] = None


__all__ = [name for name in globals() if not name.startswith("_")]
