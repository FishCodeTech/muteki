from __future__ import annotations

import asyncio
import json
from datetime import datetime, timedelta, timezone
from typing import Any, Awaitable, Callable, Optional

from muteki.platform.contracts.events import EventEnvelope

from .contracts import (
    ExecutionApproval, ExecutionPolicy, ExecutionReceipt, ExecutionRequest,
    ExecutionRule, PolicyDecision, TaskExecutionGrant,
)
from .policy import (
    TOOL_CAPABILITIES, evaluate, expired, latest_policy, match_rule,
    operation_idempotent, policy_digest, request_target, validate_activation,
)
from .store import SrcStore, SrcStoreError


class AuthorizationError(SrcStoreError):
    def __init__(self, code: str, message: str, *, receipt: Optional[ExecutionReceipt] = None) -> None:
        super().__init__(code, message)
        self.receipt = receipt


class SrcExecutionService:
    def __init__(self, src: Any) -> None:
        self.src = src
        self.store: SrcStore = src.store
        self.artifacts = src.artifacts
        self._waiters: dict[str, asyncio.Event] = {}

    def _register_egress(self) -> None:
        egress = getattr(self.src, "egress", None)
        if egress is not None and getattr(egress, "_execution_service", None) is not self:
            egress.register_execution_service(self)

    def ensure_default_policy(self, task_id: str, targets: list[str], created_by: str) -> ExecutionPolicy:
        records = self.store.all(task_id, "execution_policy")
        if records:
            return latest_policy(records, task_id)
        policy = ExecutionPolicy(
            task_id=task_id, authorized_targets=list(targets), created_by=created_by,
            environment_requirements={
                "required_adapters": ["structured_http", "managed_browser", "local_mcp", "claude.agent_sdk"],
            },
        )
        policy = policy.model_copy(update={"digest": policy_digest(policy)})
        self.store.put(
            task_id, "execution_policy", policy.model_dump(mode="json"),
            record_id=f"{policy.policy_id}:v1", parent_id=policy.policy_id,
            natural_key=f"{policy.policy_id}:v1",
        )
        return policy

    def current_policy(self, task_id: str) -> ExecutionPolicy:
        self._register_egress()
        return latest_policy(self.store.all(task_id, "execution_policy"), task_id)

    @staticmethod
    def _rule_key(rule: ExecutionRule) -> str:
        return f"{rule.rule_id}:v{rule.version}"

    def _compatible_grant(self, grant: TaskExecutionGrant, policy: ExecutionPolicy) -> bool:
        if grant.policy_digest == policy.digest:
            return True
        rules = {self._rule_key(rule): rule for rule in policy.rules}
        if any(ref not in rules for ref in grant.rule_refs):
            return False
        snapshot_row = self.store.get(grant.task_id, "task_grant_environment", grant.grant_id)
        if snapshot_row is None:
            return False
        snapshot = snapshot_row.payload
        return (
            snapshot.get("environment_requirements") == policy.environment_requirements
            and snapshot.get("network_mode") == policy.network_mode
            and snapshot.get("inspection_requirements") == policy.inspection_requirements
        )

    def revise_policy(
        self, task_id: str, payload: dict[str, Any], *, actor_id: str,
        environment_capabilities: Optional[dict[str, Any]] = None,
    ) -> ExecutionPolicy:
        payload = dict(payload)
        for envelope_field in ("command_id", "idempotency_key", "expected_version", "correlation_id"):
            payload.pop(envelope_field, None)
        current = self.current_policy(task_id)
        base_version = payload.pop("base_version", None)
        if base_version is not None and int(base_version) != current.version:
            raise SrcStoreError("version_conflict", "execution policy base version is stale")
        previous_rules = {rule.rule_id: rule for rule in current.rules}
        candidate_rules = [ExecutionRule.model_validate(row) for row in payload.get("rules", [])] if "rules" in payload else current.rules
        for rule in candidate_rules:
            previous = previous_rules.get(rule.rule_id)
            if previous is not None and previous.version == rule.version and previous != rule:
                raise SrcStoreError("version_conflict", f"rule {rule.rule_id} content changed without a version increment")
        policy = ExecutionPolicy.model_validate({
            **current.model_dump(mode="json"), **payload,
            "rules": [rule.model_dump(mode="json") for rule in candidate_rules],
            "task_id": task_id, "version": current.version + 1,
            "created_by": actor_id, "created_at": datetime.now(timezone.utc),
            "digest": "", "supersedes": f"{current.policy_id}:v{current.version}",
            "activation": {"state": "pending"},
        })
        policy = policy.model_copy(update={"digest": policy_digest(policy)})
        capabilities = dict(environment_capabilities or self.environment_capabilities(task_id))
        if policy.network_mode == "enforced":
            egress = getattr(self.src, "egress", None)
            capabilities["network_enforced"] = bool(egress and egress.supported())
        if str(policy.inspection_requirements.get("mode") or "off") == "enforce":
            egress = getattr(self.src, "egress", None)
            content_supported = getattr(egress, "content_supported", None)
            capabilities["content_enforced"] = bool(content_supported and content_supported())
        validate_activation(policy, capabilities)
        record = self.store.put(
            task_id, "execution_policy", policy.model_dump(mode="json"),
            record_id=f"{policy.policy_id}:v{policy.version}",
            parent_id=policy.policy_id, natural_key=f"{policy.policy_id}:v{policy.version}",
        )
        inspection_mode = str(policy.inspection_requirements.get("mode") or "off")
        activation: dict[str, Any] = {"state": "ready", "generation": policy.version}
        try:
            if policy.network_mode == "enforced" or inspection_mode in {"record", "enforce"}:
                egress = getattr(self.src, "egress", None)
                if egress is None:
                    if policy.network_mode == "enforced" or inspection_mode == "enforce":
                        raise SrcStoreError("capability_unavailable", "SRC egress manager is not configured")
                    activation = {"state": "declared", "generation": policy.version, "inspection_mode": inspection_mode}
                elif policy.network_mode == "enforced" or egress.supported():
                    activation = dict(egress.activate(task_id, policy))
                    activation["generation"] = activation.get("generation") or policy.version
                else:
                    activation = {"state": "declared", "generation": policy.version, "inspection_mode": inspection_mode}
        except Exception as exc:
            policy = policy.model_copy(update={"activation": {
                "state": "failed", "error": f"{type(exc).__name__}: {exc}",
            }})
            self.store.put(
                task_id, "execution_policy", policy.model_dump(mode="json"),
                record_id=record.record_id, parent_id=policy.policy_id,
                natural_key=record.natural_key, expected_revision=record.revision,
            )
            raise
        policy = policy.model_copy(update={"activation": activation})
        self.store.put(
            task_id, "execution_policy", policy.model_dump(mode="json"),
            record_id=record.record_id, parent_id=policy.policy_id,
            natural_key=record.natural_key, expected_revision=record.revision,
        )
        for row in self.store.all(task_id, "task_grant"):
            grant = TaskExecutionGrant.model_validate(row.payload)
            if grant.revoked_at:
                continue
            if not self._compatible_grant(grant, policy):
                self.store.put(
                    task_id, "task_grant", {
                        **row.payload, "revoked_at": datetime.now(timezone.utc).isoformat(),
                        "revocation_reason": "policy_incompatible",
                    }, record_id=row.record_id, expected_revision=row.revision,
                )
        for row in self.store.all(task_id, "execution_approval"):
            approval = ExecutionApproval.model_validate(row.payload)
            if approval.status == "pending" and approval.policy_version != policy.version:
                updated = approval.model_copy(update={"status": "superseded"})
                self.store.put(
                    task_id, "execution_approval", updated.model_dump(mode="json"),
                    record_id=row.record_id, parent_id=row.parent_id,
                    expected_revision=row.revision,
                )
                self._wake(approval.invocation_id)
        return policy

    @staticmethod
    def input_digest(request: ExecutionRequest) -> str:
        return SrcStore.digest(request.model_copy(update={
            "created_at": datetime.min.replace(tzinfo=timezone.utc), "arguments_ref": None,
        }).model_dump(mode="json"))

    def _wake(self, invocation_id: str) -> None:
        waiter = self._waiters.get(invocation_id)
        if waiter is not None:
            waiter.set()

    def _queue_approval(self, approval: ExecutionApproval) -> None:
        request = approval.request_snapshot
        self.src.binding(request.task_id)
        reusable = bool(approval.matched_rule_refs) and all(
            rule.reusable_approval
            for rule in self.current_policy(request.task_id).rules
            if f"{rule.rule_id}:v{rule.version}" in approval.matched_rule_refs
        )
        payload = {
            "approval_id": approval.approval_id, "approval_kind": "src_execution",
            "src_task_id": request.task_id, "task_id": request.task_id,
            "thread_id": request.thread_id, "run_id": request.run_id,
            "worker_id": request.worker_id, "tool_name": request.tool_name,
            "title": f"{request.tool_name} · {request.action}",
            "reason": "命中 SRC 执行审批规则" if approval.matched_rule_refs else "新的主动测试目标需要明确授权",
            "requested_at": approval.requested_at.isoformat(),
            "expires_at": approval.expires_at.isoformat(),
            "input_digest": approval.input_digest, "arguments": request.arguments,
            "arguments_ref": request.arguments_ref.model_dump(mode="json") if request.arguments_ref else None,
            "resource_facts": {key: value.model_dump(mode="json") for key, value in request.resource_facts.items()},
            "identity_ref": request.identity_ref, "identity_revision": request.identity_revision,
            "cwd": request.cwd, "environment_id": request.environment_id,
            "environment_generation": request.environment_generation,
            "matched_rule_refs": approval.matched_rule_refs,
            "policy_version": approval.policy_version,
            "reusable_approval": reusable,
            "scopes": ["once", "task_scope", "session"] if reusable else ["once"],
        }
        self.store.platform.append_events(EventEnvelope(
            aggregate_type="thread", aggregate_id=request.thread_id,
            event_type="core.approval.requested", producer="builtin.src",
            actor_id=request.worker_id or request.run_id or "src",
            correlation_id=request.invocation_id, payload=payload,
        ))

    def _create_approval(self, request: ExecutionRequest, decision: PolicyDecision) -> ExecutionApproval:
        approval = ExecutionApproval(
            invocation_id=request.invocation_id, input_digest=self.input_digest(request),
            request_snapshot=request, matched_rule_refs=decision.matched_rule_refs,
            policy_version=request.policy_version,
            expires_at=datetime.now(timezone.utc) + timedelta(minutes=30),
        )
        decision = decision.model_copy(update={"approval_id": approval.approval_id})
        self.store.put(
            request.task_id, "execution_approval", approval.model_dump(mode="json"),
            record_id=approval.approval_id, parent_id=request.invocation_id,
        )
        self.store.put(
            request.task_id, "policy_decision", decision.model_dump(mode="json"),
            record_id=decision.decision_id, parent_id=request.invocation_id,
            expected_revision=1,
        )
        self._queue_approval(approval)
        return approval

    def _valid_grants(self, request: ExecutionRequest, policy: ExecutionPolicy, ask_refs: list[str]) -> Optional[TaskExecutionGrant]:
        for row in self.store.all(request.task_id, "task_grant"):
            grant = TaskExecutionGrant.model_validate(row.payload)
            if grant.revoked_at or (grant.expires_at and expired(grant.expires_at)):
                continue
            if not self._compatible_grant(grant, policy) or not set(ask_refs).issubset(set(grant.rule_refs)):
                continue
            pseudo = ExecutionRule(
                effect="allow", resource_kind=request.resource_kind,
                evaluation_point={"file": "filesystem", "network": "connection", "http": "request"}.get(request.resource_kind, "tool"),
                selector={**grant.resource_selector, **grant.identity_constraints, **grant.environment_constraints},
                actions=grant.actions, conditions=grant.input_constraints,
            )
            state, _missing = match_rule(request, pseudo)
            if state == "match":
                return grant
        return None

    def evaluate_request(self, request: ExecutionRequest) -> PolicyDecision:
        if request.run_id and request.resource_kind == "control":
            try:
                self.src.run_binding(request.task_id, request.run_id)
            except SrcStoreError as exc:
                if exc.code == "artifact_missing":
                    raise SrcStoreError("task_binding_invalid", exc.message) from exc
                raise
        policy = self.current_policy(request.task_id)
        if policy.activation.get("state") == "failed":
            raise SrcStoreError("policy_unavailable", "execution policy activation failed")
        if policy.version != request.policy_version:
            raise SrcStoreError("authorization_stale", "execution policy version is stale")
        existing = self.store.get(request.task_id, "execution_request", request.invocation_id)
        if existing is not None:
            if self.input_digest(ExecutionRequest.model_validate(existing.payload)) != self.input_digest(request):
                raise SrcStoreError("approval_stale", "invocation id is already bound to different input")
            for row in reversed(self.store.all(request.task_id, "policy_decision")):
                if row.payload.get("invocation_id") == request.invocation_id:
                    return PolicyDecision.model_validate(row.payload)
            raise SrcStoreError("policy_unavailable", "execution decision is missing")
        raw = evaluate(policy, request)
        grant = None
        if raw["effect"] == "ask" and raw["matched_rule_refs"]:
            grant = self._valid_grants(request, policy, raw["matched_rule_refs"])
            if grant is not None:
                raw.update({"effect": "allow", "status": "allowed", "reason_code": "task_grant"})
        with self.store.platform.transaction():
            arguments_ref = self.artifacts.put(
                request.task_id,
                json.dumps(request.arguments, ensure_ascii=False, separators=(",", ":")).encode("utf-8"),
                media_type="application/json", name=f"{request.invocation_id}-input.json",
            )
            request = request.model_copy(update={"arguments_ref": arguments_ref})
            decision = PolicyDecision(
                invocation_id=request.invocation_id, effect=raw["effect"], status=raw["status"],
                matched_rule_refs=raw["matched_rule_refs"], observed_rule_refs=raw["observed_rule_refs"],
                unknown_fields=raw["unknown_fields"], policy_version=policy.version,
                capability_snapshot=dict(TOOL_CAPABILITIES), grant_id=grant.grant_id if grant else None,
                reason_code=raw["reason_code"],
            )
            self.store.put(
                request.task_id, "execution_request", request.model_dump(mode="json"),
                record_id=request.invocation_id,
            )
            self.store.put(
                request.task_id, "policy_decision", decision.model_dump(mode="json"),
                record_id=decision.decision_id, parent_id=request.invocation_id,
            )
            if decision.effect == "ask":
                self._create_approval(request, decision)
            return decision

    def decision(self, task_id: str, invocation_id: str) -> PolicyDecision:
        for row in reversed(self.store.all(task_id, "policy_decision")):
            if row.payload.get("invocation_id") == invocation_id:
                return PolicyDecision.model_validate(row.payload)
        raise SrcStoreError("artifact_missing", "execution decision is unavailable")

    def _approval_for_invocation(self, task_id: str, invocation_id: str) -> Optional[ExecutionApproval]:
        for row in reversed(self.store.all(task_id, "execution_approval")):
            if row.payload.get("invocation_id") == invocation_id:
                return ExecutionApproval.model_validate(row.payload)
        return None

    def _approval_satisfied(self, approval: Optional[ExecutionApproval], request: ExecutionRequest) -> bool:
        return bool(
            approval and approval.status == "approved" and approval.consumed_at is None
            and approval.input_digest == self.input_digest(request)
            and approval.policy_version == request.policy_version
            and approval.expires_at > datetime.now(timezone.utc)
        )

    def _receipt(self, task_id: str, invocation_id: str) -> Optional[ExecutionReceipt]:
        row = self.store.get(task_id, "execution_receipt", invocation_id)
        return ExecutionReceipt.model_validate(row.payload) if row is not None else None

    def _save_receipt(self, task_id: str, receipt: ExecutionReceipt) -> None:
        self.store.put(
            task_id, "execution_receipt", receipt.model_dump(mode="json"),
            record_id=receipt.invocation_id, parent_id=receipt.decision_id,
        )

    def _terminal_receipt(self, request: ExecutionRequest, decision: PolicyDecision, state: str, code: str) -> ExecutionReceipt:
        receipt = ExecutionReceipt(
            invocation_id=request.invocation_id, decision_id=decision.decision_id,
            dispatch_state=state, error_code=code, outcome_known=state != "outcome_unknown",
        )
        self._save_receipt(request.task_id, receipt)
        return receipt

    def _expire(self, approval: ExecutionApproval) -> ExecutionApproval:
        row = self.store.require(approval.request_snapshot.task_id, "execution_approval", approval.approval_id)
        if approval.status == "pending" and expired(approval.expires_at):
            updated = approval.model_copy(update={"status": "expired"})
            self.store.put(
                approval.request_snapshot.task_id, "execution_approval", updated.model_dump(mode="json"),
                record_id=approval.approval_id, parent_id=approval.invocation_id,
                expected_revision=row.revision,
            )
            self._wake(approval.invocation_id)
            return updated
        return approval

    async def authorize(self, request: ExecutionRequest, *, wait_for_approval: bool = True, approval_timeout: float = 1800.0) -> PolicyDecision:
        deadline = asyncio.get_running_loop().time() + approval_timeout
        while True:
            prior = self._receipt(request.task_id, request.invocation_id)
            if prior is not None and prior.dispatch_state not in {"created", "waiting_approval"}:
                if prior.dispatch_state == "dispatched":
                    prior = self._terminal_receipt(request, self.decision(request.task_id, request.invocation_id), "outcome_unknown", "outcome_unknown")
                raise AuthorizationError(
                    "outcome_unknown" if prior.dispatch_state == "outcome_unknown" else "invocation_replayed",
                    f"invocation cannot be dispatched again: {prior.dispatch_state}", receipt=prior,
                )
            decision = self.evaluate_request(request)
            if decision.effect == "deny":
                receipt = self._terminal_receipt(
                    request, decision, "denied" if decision.status == "denied" else "created", decision.reason_code,
                )
                raise AuthorizationError(decision.reason_code, "execution was not authorized", receipt=receipt)
            approval = self._approval_for_invocation(request.task_id, request.invocation_id)
            if approval is not None:
                approval = self._expire(approval)
            if approval and approval.status in {"denied", "expired", "cancelled", "superseded"}:
                code = "approval_expired" if approval.status == "expired" else "policy_denied"
                receipt = self._terminal_receipt(request, decision, "denied", code)
                raise AuthorizationError(code, f"approval is {approval.status}", receipt=receipt)
            if decision.effect != "ask" or self._approval_satisfied(approval, request):
                return decision
            receipt = self._terminal_receipt(
                request, decision, "waiting_approval" if decision.status == "waiting_approval" else "created",
                decision.reason_code,
            )
            if not wait_for_approval:
                raise AuthorizationError(decision.reason_code, "execution approval is pending", receipt=receipt)
            remaining = deadline - asyncio.get_running_loop().time()
            if remaining <= 0:
                if approval and approval.status == "pending":
                    row = self.store.require(request.task_id, "execution_approval", approval.approval_id)
                    expired_approval = approval.model_copy(update={"status": "expired"})
                    self.store.put(
                        request.task_id, "execution_approval", expired_approval.model_dump(mode="json"),
                        record_id=approval.approval_id, parent_id=approval.invocation_id,
                        expected_revision=row.revision,
                    )
                receipt = self._terminal_receipt(request, decision, "denied", "approval_expired")
                raise AuthorizationError("approval_expired", "approval wait timed out", receipt=receipt)
            await self.wait_for_decision(request.task_id, request.invocation_id, timeout=remaining)

    def mark_dispatched(self, request: ExecutionRequest, decision: Optional[PolicyDecision] = None) -> ExecutionReceipt:
        self.src.require_active(request.task_id, request.run_id or "")
        current = self.current_policy(request.task_id)
        if current.version != request.policy_version:
            raise AuthorizationError("authorization_stale", "execution policy version changed before dispatch")
        decision = decision or self.decision(request.task_id, request.invocation_id)
        if decision.grant_id:
            grant_row = self.store.get(request.task_id, "task_grant", decision.grant_id)
            if grant_row is None:
                raise AuthorizationError("approval_stale", "Task Grant is no longer available")
            grant = TaskExecutionGrant.model_validate(grant_row.payload)
            if grant.revoked_at or (grant.expires_at and expired(grant.expires_at)) or not self._compatible_grant(grant, current):
                raise AuthorizationError("approval_stale", "Task Grant was revoked or expired before dispatch")
        with self.store.platform.transaction():
            prior = self._receipt(request.task_id, request.invocation_id)
            if prior is not None and prior.dispatch_state not in {"created", "waiting_approval"}:
                raise AuthorizationError("invocation_replayed", f"invocation is already {prior.dispatch_state}", receipt=prior)
            approval = self._approval_for_invocation(request.task_id, request.invocation_id)
            if approval is not None:
                approval = self._expire(approval)
            if decision.effect == "ask" and not self._approval_satisfied(approval, request):
                raise AuthorizationError("approval_stale", "approval is not valid for the fixed request")
            now = datetime.now(timezone.utc)
            receipt = ExecutionReceipt(
                invocation_id=request.invocation_id, decision_id=decision.decision_id,
                dispatch_state="dispatched", started_at=now, outcome_known=False,
                history=[{"state": "dispatched", "at": now.isoformat()}],
            )
            self._save_receipt(request.task_id, receipt)
            if approval is not None and self._approval_satisfied(approval, request):
                row = self.store.require(request.task_id, "execution_approval", approval.approval_id)
                consumed = approval.model_copy(update={"consumed_at": now})
                self.store.put(
                    request.task_id, "execution_approval", consumed.model_dump(mode="json"),
                    record_id=approval.approval_id, parent_id=request.invocation_id,
                    expected_revision=row.revision,
                )
            return receipt

    async def execute(
        self, request: ExecutionRequest,
        executor: Callable[[ExecutionRequest], Awaitable[Any]], *,
        wait_for_approval: bool = False, approval_timeout: float = 1800.0,
    ) -> ExecutionReceipt:
        try:
            decision = await self.authorize(
                request, wait_for_approval=wait_for_approval, approval_timeout=approval_timeout,
            )
            receipt = self.mark_dispatched(request, decision)
        except AuthorizationError as exc:
            if exc.receipt is not None:
                return exc.receipt
            raise
        try:
            result = await executor(request)
        except (asyncio.TimeoutError, TimeoutError) as exc:
            return self._finish_execution(request, receipt, error=exc, unknown=True)
        except asyncio.CancelledError as exc:
            self._finish_execution(
                request, receipt, error=exc, unknown=not operation_idempotent(request),
            )
            raise
        except Exception as exc:
            return self._finish_execution(
                request, receipt, error=exc, unknown=not operation_idempotent(request),
            )
        return self._finish_execution(request, receipt, result=result)

    def record_result(self, request: ExecutionRequest, result: Any) -> ExecutionReceipt:
        receipt = self._receipt(request.task_id, request.invocation_id)
        if receipt is None:
            raise SrcStoreError("artifact_missing", "cannot record a result before dispatch")
        if receipt.dispatch_state not in {"dispatched", "outcome_unknown"}:
            return receipt
        return self._finish_execution(request, receipt, result=result)

    def record_outcome(
        self, request: ExecutionRequest, *, result: Any = None,
        error: Optional[Exception] = None, unknown: bool = False,
    ) -> ExecutionReceipt:
        receipt = self._receipt(request.task_id, request.invocation_id)
        if receipt is None:
            raise SrcStoreError("artifact_missing", "cannot record an outcome before dispatch")
        if receipt.dispatch_state not in {"dispatched", "outcome_unknown"}:
            return receipt
        return self._finish_execution(request, receipt, result=result, error=error, unknown=unknown)

    def _store_result(self, request: ExecutionRequest, result: Any) -> tuple[Any, Any]:
        from muteki.platform.contracts.capabilities import CapabilityResult
        if isinstance(result, CapabilityResult):
            serializable: Any = {
                "__contract__": "CapabilityResult",
                "payload": result.model_dump(mode="json"),
            }
        elif hasattr(result, "model_dump"):
            serializable = result.model_dump(mode="json")
        else:
            serializable = result
        raw = result if isinstance(result, bytes) else json.dumps(
            serializable, ensure_ascii=False, default=str,
        ).encode("utf-8")
        result_ref = self.artifacts.put(
            request.task_id, raw,
            media_type="application/octet-stream" if isinstance(result, bytes) else "application/json",
            name=f"{request.invocation_id}-result",
        )
        inline = result if isinstance(result, CapabilityResult) else (
            serializable if not isinstance(result, bytes) and len(raw) <= 16384 else None
        )
        return result_ref, inline

    def read_result(self, task_id: str, receipt: ExecutionReceipt) -> Any:
        from muteki.platform.contracts.capabilities import CapabilityResult
        if isinstance(receipt.result, CapabilityResult):
            return receipt.result
        if isinstance(receipt.result, dict) and receipt.result.get("__contract__") == "CapabilityResult":
            return CapabilityResult.model_validate(receipt.result["payload"])
        if receipt.result_ref is None:
            return receipt.result
        raw = self.artifacts.read(receipt.result_ref, expected_task_id=task_id)
        if receipt.result_ref.media_type == "application/json":
            decoded = json.loads(raw.decode("utf-8"))
            if isinstance(decoded, dict) and decoded.get("__contract__") == "CapabilityResult":
                return CapabilityResult.model_validate(decoded["payload"])
            return decoded
        return raw

    def _finish_execution(
        self, request: ExecutionRequest, receipt: ExecutionReceipt, *,
        result: Any = None, error: Optional[Exception] = None, unknown: bool = False,
    ) -> ExecutionReceipt:
        ended = datetime.now(timezone.utc)
        state = "outcome_unknown" if unknown else "failed" if error else "completed"
        result_ref = None
        inline = None
        if error is None:
            result_ref, inline = self._store_result(request, result)
        updated = receipt.model_copy(update={
            "dispatch_state": state, "ended_at": ended, "outcome_known": not unknown,
            "result_ref": result_ref, "result": inline,
            "error_code": "outcome_unknown" if unknown else getattr(error, "code", type(error).__name__) if error else "",
            "history": [*receipt.history, {
                "state": state, "at": ended.isoformat(),
                "error": f"{type(error).__name__}: {error}" if error else "",
            }],
        })
        self._save_receipt(request.task_id, updated)
        return updated

    def reconcile_execution(
        self, task_id: str, invocation_id: str, *, state: str,
        external_receipt_ref: str, result: Any = None, error_code: str = "",
    ) -> ExecutionReceipt:
        if state not in {"completed", "failed"} or not external_receipt_ref:
            raise SrcStoreError("version_conflict", "a confirmed external receipt and terminal state are required")
        receipt = self._receipt(task_id, invocation_id)
        if receipt is None or receipt.dispatch_state not in {"dispatched", "outcome_unknown"}:
            raise SrcStoreError("version_conflict", "only an in-flight or unknown execution can be reconciled")
        result_ref = None
        inline = None
        if state == "completed":
            result_ref, inline = self._store_result(
                ExecutionRequest.model_validate(
                    self.store.require(task_id, "execution_request", invocation_id).payload
                ),
                result,
            )
        updated = receipt.model_copy(update={
            "dispatch_state": state, "ended_at": datetime.now(timezone.utc),
            "external_receipt_ref": external_receipt_ref,
            "result_ref": result_ref, "result": inline,
            "error_code": error_code, "outcome_known": True,
        })
        self._save_receipt(task_id, updated)
        return updated

    def decide_approval(
        self, task_id: str, approval_id: str, decision: str, *, actor_id: str,
        base_revision: int, scope: Optional[dict[str, Any]] = None,
    ) -> ExecutionApproval:
        with self.store.platform.transaction():
            row = self.store.require(task_id, "execution_approval", approval_id)
            approval = ExecutionApproval.model_validate(row.payload)
            if row.revision != base_revision:
                raise SrcStoreError("version_conflict", "approval revision is stale")
            if approval.status != "pending":
                raise SrcStoreError("version_conflict", "approval has already been decided")
            if expired(approval.expires_at):
                raise SrcStoreError("approval_expired", "approval has expired")
            if decision not in {"approve_once", "deny", "approve_task_scope"}:
                raise SrcStoreError("policy_denied", "unknown approval decision")
            if decision == "approve_task_scope":
                rules = {
                    f"{rule.rule_id}:v{rule.version}": rule
                    for rule in self.current_policy(task_id).rules
                }
                if not approval.matched_rule_refs or any(
                    ref not in rules or not rules[ref].reusable_approval
                    for ref in approval.matched_rule_refs
                ):
                    raise SrcStoreError("policy_denied", "the matched rules do not allow a reusable Task Grant")
            now = datetime.now(timezone.utc)
            grant = None
            if decision == "approve_task_scope":
                selected = dict(scope or {})
                request = approval.request_snapshot
                policy = self.current_policy(task_id)
                target = request_target(request)
                default_selector = {"tool_name": request.tool_name}
                if target:
                    default_selector["url"] = {"target": target}
                if request.cwd:
                    default_selector["cwd"] = request.cwd
                default_identity: dict[str, Any] = {}
                if request.identity_ref:
                    default_identity["identity_ref"] = request.identity_ref
                    if request.identity_revision:
                        default_identity["identity_revision"] = request.identity_revision
                default_input: dict[str, Any] = {}
                if request.arguments_hash:
                    default_input["arguments_hash"] = request.arguments_hash
                ttl_seconds = int(selected.get("ttl_seconds") or 0)
                expires_at = (
                    datetime.fromisoformat(str(selected["expires_at"]))
                    if selected.get("expires_at")
                    else (now + timedelta(seconds=ttl_seconds)) if ttl_seconds > 0
                    else (now + timedelta(hours=4))
                )
                grant = TaskExecutionGrant(
                    task_id=task_id, rule_refs=approval.matched_rule_refs,
                    resource_selector=dict(selected.get("resource_selector") or default_selector),
                    actions=list(selected.get("actions") or [request.action]),
                    identity_constraints=dict(
                        selected["identity_constraints"] if "identity_constraints" in selected else default_identity
                    ),
                    environment_constraints=dict(selected.get("environment_constraints") or ({"environment_id": request.environment_id, "environment_generation": request.environment_generation} if request.environment_id else {})),
                    input_constraints=dict(
                        selected["input_constraints"] if "input_constraints" in selected else default_input
                    ),
                    source_approval_id=approval_id, created_by=actor_id,
                    expires_at=expires_at, policy_digest=policy.digest,
                )
                self.store.put(task_id, "task_grant", grant.model_dump(mode="json"), record_id=grant.grant_id)
                self.store.put(
                    task_id, "task_grant_environment", {
                        "environment_requirements": policy.environment_requirements,
                        "network_mode": policy.network_mode,
                        "inspection_requirements": policy.inspection_requirements,
                    }, record_id=grant.grant_id, parent_id=grant.grant_id,
                )
            updated = approval.model_copy(update={
                "status": "denied" if decision == "deny" else "approved",
                "decision": decision, "decided_by": actor_id, "decided_at": now,
                "grant_id": grant.grant_id if grant else None,
            })
            self.store.put(
                task_id, "execution_approval", updated.model_dump(mode="json"),
                record_id=approval_id, parent_id=approval.invocation_id,
                expected_revision=base_revision,
            )
        self._wake(approval.invocation_id)
        return updated

    async def decide_from_conversation(
        self, thread_id: str, approval_id: str, choice: str, scope: str, *,
        actor_id: str = "",
    ) -> None:
        binding = self.src.task_for_thread(thread_id)
        row = self.store.require(binding.task_id, "execution_approval", approval_id)
        decision = (
            "deny" if choice == "deny"
            else "approve_task_scope" if scope in {"task_scope", "session"}
            else "approve_once"
        )
        self.decide_approval(
            binding.task_id, approval_id, decision,
            actor_id=actor_id or binding.principal_id,
            base_revision=row.revision,
        )

    async def wait_for_decision(self, task_id: str, invocation_id: str, timeout: float = 30.0) -> PolicyDecision:
        deadline = asyncio.get_running_loop().time() + timeout
        waiter = self._waiters.setdefault(invocation_id, asyncio.Event())
        while True:
            decision = self.decision(task_id, invocation_id)
            approval = self._approval_for_invocation(task_id, invocation_id)
            if decision.effect != "ask" or (approval and approval.status != "pending"):
                return decision
            waiter.clear()
            remaining = deadline - asyncio.get_running_loop().time()
            if remaining <= 0:
                return decision
            try:
                await asyncio.wait_for(waiter.wait(), timeout=min(0.25, remaining))
            except asyncio.TimeoutError:
                pass

    def environment_capabilities(self, task_id: Optional[str] = None) -> dict[str, Any]:
        network = False
        content = False
        generation = None
        if task_id:
            egress = getattr(self.src, "egress", None)
            policy = self.current_policy(task_id)
            if egress is not None and (policy.network_mode == "enforced" or policy.inspection_requirements.get("mode") == "enforce"):
                try:
                    current = egress.current(task_id, policy)
                    network = bool(current.get("network_enforced"))
                    content = bool(current.get("content_enforced"))
                    generation = current.get("generation")
                except Exception:
                    network = False
                    content = False
        from muteki.external_agents.src_policy import ADAPTER_EXECUTION_COVERAGE
        shell = TOOL_CAPABILITIES["shell"]
        browser = TOOL_CAPABILITIES["managed_browser"]
        if task_id:
            try:
                browser = self.src.domain("browser").capabilities(task_id)
            except Exception as exc:
                browser = {
                    "pre_dispatch_gate": True,
                    "environment_boundary": "unavailable",
                    "network_scope": ["declared"], "content_inspection": ["off"],
                    "activation_error": f"{type(exc).__name__}: {exc}",
                }
        return {
            "network_enforced": network, "content_enforced": content,
            "environment_generation": generation,
            "activation": dict(policy.activation) if task_id else {},
            "adapter_coverage": ADAPTER_EXECUTION_COVERAGE,
            "tools": {
                "http": TOOL_CAPABILITIES["structured_http"],
                "browser": browser,
                "file": shell, "process": shell,
                "network": TOOL_CAPABILITIES["raw_socket"],
                "mcp": TOOL_CAPABILITIES["local_mcp"],
                "control": {
                    "pre_dispatch_gate": True,
                    "observed_resource_fields": ["task_id", "run_id", "tool_name", "action", "identity_ref"],
                },
            },
        }

    def revoke_grant(self, task_id: str, grant_id: str, *, base_revision: int) -> None:
        row = self.store.require(task_id, "task_grant", grant_id)
        if row.revision != base_revision:
            raise SrcStoreError("version_conflict", "grant revision is stale")
        self.store.put(
            task_id, "task_grant", {**row.payload, "revoked_at": datetime.now(timezone.utc).isoformat()},
            record_id=grant_id, expected_revision=base_revision,
        )


__all__ = ["AuthorizationError", "SrcExecutionService"]
