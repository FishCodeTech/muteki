from __future__ import annotations

import asyncio
import json
import logging
import os
import threading
import time
from pathlib import Path
from typing import Any

from .contracts import ExecutionApproval, ExecutionRequest, ResourceFact
from .execution import AuthorizationError
from .store import SrcStoreError

LOG = logging.getLogger(__name__)


def _atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(mode=0o2750, parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{time.time_ns()}.tmp")
    try:
        with temporary.open("w", encoding="utf-8") as handle:
            json.dump(value, handle, ensure_ascii=False, sort_keys=True)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, 0o640)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (OSError, ValueError):
        return {}


class SrcInspectionBridge:
    def __init__(self, manager: Any) -> None:
        self.manager = manager
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._sync_lock = threading.RLock()

    @property
    def execution(self) -> Any:
        execution = getattr(self.manager, "_execution_service", None)
        if execution is None:
            raise SrcStoreError(
                "capability_unavailable",
                "SRC ExecutionService is not registered with the egress manager",
            )
        return execution

    def start(self) -> None:
        self.execution
        with self._sync_lock:
            if self._thread and self._thread.is_alive():
                return
            self._stop.clear()
            self._thread = threading.Thread(
                target=self._run, name="src-inspection-bridge", daemon=True,
            )
            self._thread.start()
            self.sync()

    def stop(self) -> None:
        self._stop.set()
        thread = self._thread
        if thread and thread.is_alive():
            thread.join(timeout=10)
        self._thread = None

    def _run(self) -> None:
        while not self._stop.wait(0.2):
            try:
                self.sync()
            except Exception:
                LOG.exception("SRC inspection bridge iteration failed")

    def sync(self) -> None:
        with self._sync_lock:
            for policy_path in self.manager.root.glob("*/policy.json"):
                policy = _read_json(policy_path)
                task_id = str(policy.get("task_id") or "")
                if not task_id:
                    continue
                for request_path in policy_path.parent.glob("inspection_requests/*.json"):
                    payload = _read_json(request_path)
                    request_id = request_path.stem
                    if payload:
                        self._sync_request(task_id, request_id, payload)
                self._sync_receipts(task_id, policy_path.parent / "traffic")

    def _link(self, task_id: str, request_id: str) -> Any:
        return self.execution.store.get(task_id, "egress_inspection_link", request_id)

    def _save_link(self, task_id: str, request_id: str, payload: dict[str, Any]) -> None:
        existing = self._link(task_id, request_id)
        if existing is None or existing.payload != payload:
            self.execution.store.put(
                task_id, "egress_inspection_link", payload,
                record_id=request_id, parent_id=payload.get("approval_id") or "",
            )

    def _approval(self, task_id: str, request_id: str) -> ExecutionApproval | None:
        link = self._link(task_id, request_id)
        approval_id = str(link.payload.get("approval_id") or "") if link is not None else ""
        if not approval_id:
            candidates = [
                row for row in self.execution.store.all(task_id, "execution_approval")
                if row.payload.get("invocation_id") == request_id
            ]
            if not candidates:
                return None
            approval_id = candidates[-1].record_id
        row = self.execution.store.require(task_id, "execution_approval", approval_id)
        return ExecutionApproval.model_validate(row.payload)

    def _sync_request(self, task_id: str, request_id: str, payload: dict[str, Any]) -> None:
        receipt = self.execution.store.get(task_id, "execution_receipt", request_id)
        if receipt is not None and receipt.payload.get("dispatch_state") not in {"created", "waiting_approval"}:
            return
        approval = self._approval(task_id, request_id)
        if approval is not None and approval.status == "pending":
            existing = self._link(task_id, request_id)
            self._save_link(task_id, request_id, {
                **(existing.payload if existing is not None else {}),
                "request_id": request_id, "approval_id": approval.approval_id,
                "invocation_id": request_id, "state": "pending",
            })
            return
        if approval is not None and approval.status in {"denied", "expired", "cancelled", "superseded"}:
            self._write_decision(task_id, request_id, {
                "approved": False, "code": "approval_expired" if approval.status == "expired" else "policy_denied",
            })
            return
        if approval is not None and approval.status == "approved":
            existing = self._link(task_id, request_id)
            request = self._execution_request(task_id, request_id, payload)
            try:
                decision = self.execution.decision(task_id, request_id)
                self.execution.mark_dispatched(request, decision)
            except (AuthorizationError, SrcStoreError) as exc:
                if getattr(exc, "code", "") != "invocation_replayed":
                    self._write_decision(task_id, request_id, {
                        "approved": False, "code": getattr(exc, "code", "approval_stale"),
                    })
                    return
            self._save_link(task_id, request_id, {
                **(existing.payload if existing is not None else {}),
                "request_id": request_id, "approval_id": approval.approval_id,
                "invocation_id": request_id, "state": "approved",
                "request_digest": (existing.payload.get("request_digest") if existing is not None else None) or payload.get("digest"),
            })
            self._write_decision(task_id, request_id, {
                "approved": True, "code": "approved", "grant_id": approval.grant_id,
            })
            return
        try:
            request = self._execution_request(task_id, request_id, payload)
            decision = asyncio.run(self.execution.authorize(request, wait_for_approval=False))
            approval = self._approval(task_id, request_id)
            self._save_link(task_id, request_id, {
                "request_id": request_id, "approval_id": approval.approval_id if approval else "",
                "invocation_id": request_id, "state": "allow",
                "policy_version": decision.policy_version,
                "request_digest": payload.get("digest"),
            })
            self.execution.mark_dispatched(request, decision)
            self._write_decision(task_id, request_id, {
                "approved": True, "code": "task_grant" if decision.grant_id else "approved",
                "grant_id": decision.grant_id,
            })
        except AuthorizationError as exc:
            receipt = exc.receipt
            approval = self._approval(task_id, request_id)
            self._save_link(task_id, request_id, {
                "request_id": request_id,
                "approval_id": approval.approval_id if approval else "",
                "invocation_id": request_id,
                "state": receipt.dispatch_state if receipt else "denied",
                "policy_version": receipt.decision_id if receipt else "",
                "request_digest": payload.get("digest"),
            })
            if receipt and receipt.dispatch_state in {"created", "waiting_approval"}:
                return
            self._write_decision(task_id, request_id, {
                "approved": False,
                "code": receipt.error_code if receipt else exc.code,
            })
        except Exception as exc:
            self._write_decision(task_id, request_id, {
                "approved": False, "code": getattr(exc, "code", "capability_unavailable"),
                "message": f"{type(exc).__name__}: {exc}",
            })

    def _execution_request(self, task_id: str, request_id: str, payload: dict[str, Any]) -> ExecutionRequest:
        facts = dict(payload.get("facts") or {})
        binding = self.execution.src.binding(task_id)
        policy = self.execution.current_policy(task_id)
        raw_path = self.manager._task_root(task_id) / "traffic" / f"{request_id}.request"
        raw = raw_path.read_bytes() if raw_path.is_file() else b""
        import base64
        arguments = {
            "url": facts.get("url"), "method": facts.get("method"),
            "headers": facts.get("headers") or [],
            "body_sha256": facts.get("body_sha256"),
            "raw_request_b64": base64.b64encode(raw).decode("ascii"),
            "idempotent": False,
        }
        resource_facts = {
            key: ResourceFact(value=value, origin="request", state="known")
            for key, value in facts.items()
        }
        return ExecutionRequest(
            invocation_id=request_id, task_id=task_id, thread_id=binding.thread_id,
            tool_name="src.http.transparent_proxy", action="request",
            arguments=arguments, arguments_hash=self.execution.store.digest(arguments),
            resource_kind="http", resource_facts=resource_facts,
            identity_ref=str(facts.get("identity_ref") or "default"),
            environment_id=self.manager._name(task_id),
            environment_generation=int(policy.activation.get("generation") or policy.version),
            policy_version=policy.version,
        )

    def _write_decision(self, task_id: str, request_id: str, decision: dict[str, Any]) -> None:
        link = self._link(task_id, request_id)
        original = _read_json(self.manager._task_root(task_id) / "inspection_requests" / f"{request_id}.json")
        request_digest = str(
            (link.payload.get("request_digest") if link is not None else "")
            or original.get("digest")
            or ""
        )
        path = self.manager._task_root(task_id) / "inspection_decisions" / f"{request_id}.json"
        payload = {
            "request_id": request_id, "request_digest": request_digest,
            "decided_at": time.time(), **decision,
        }
        if _read_json(path) != payload:
            _atomic_json(path, payload)

    def _sync_receipts(self, task_id: str, traffic_root: Path) -> None:
        for path in traffic_root.glob("*.json"):
            meta = _read_json(path)
            request_id = str(meta.get("exchange_id") or path.stem)
            link = self._link(task_id, request_id)
            if link is None:
                continue
            request = self.execution.store.require(task_id, "execution_request", request_id)
            request = ExecutionRequest.model_validate(request.payload)
            errors = list(meta.get("errors") or [])
            approval = self._approval(task_id, request_id)
            if approval and approval.status == "pending" and any(
                str(error).startswith("approval_required") for error in errors
            ):
                row = self.execution.store.require(task_id, "execution_approval", approval.approval_id)
                updated = approval.model_copy(update={"status": "expired"})
                self.execution.store.put(
                    task_id, "execution_approval", updated.model_dump(mode="json"),
                    record_id=approval.approval_id, parent_id=approval.invocation_id,
                    expected_revision=row.revision,
                )
                self.execution._wake(approval.invocation_id)
                self._save_link(task_id, request_id, {**link.payload, "state": "expired"})
                continue
            receipt = self.execution.store.get(task_id, "execution_receipt", request_id)
            if receipt is None or receipt.payload.get("dispatch_state") not in {"dispatched", "outcome_unknown"}:
                continue
            if errors:
                self.execution.record_outcome(
                    request, error=RuntimeError("; ".join(errors)),
                    unknown=meta.get("capture_state") != "complete",
                )
            elif meta.get("capture_state") in {"complete", "uninspected"}:
                self.execution.record_result(request, meta)

    def requests(self, task_id: str) -> list[dict[str, Any]]:
        self.sync()
        rows = []
        for link in self.execution.store.all(task_id, "egress_inspection_link"):
            request_id = link.record_id
            approval = self._approval(task_id, request_id)
            original = _read_json(self.manager._task_root(task_id) / "inspection_requests" / f"{request_id}.json")
            rows.append({
                **link.payload, "approval": approval.model_dump(mode="json") if approval else None,
                "request": original or None, "persistent": True,
            })
        return rows

    def decide(self, task_id: str, request_id: str, *, approved: bool, actor_id: str, task_scope: bool = False) -> ExecutionApproval:
        approval = self._approval(task_id, request_id)
        if approval is None:
            raise SrcStoreError("artifact_missing", "persistent inspection approval is unavailable")
        row = self.execution.store.require(task_id, "execution_approval", approval.approval_id)
        decision = "deny" if not approved else "approve_task_scope" if task_scope else "approve_once"
        updated = self.execution.decide_approval(
            task_id, approval.approval_id, decision,
            actor_id=actor_id, base_revision=row.revision,
        )
        self.sync()
        return updated

    def revoke_grant(self, task_id: str, digest: str, *, actor_id: str) -> None:
        for link in self.execution.store.all(task_id, "egress_inspection_link"):
            if link.payload.get("request_digest") != digest:
                continue
            approval = self._approval(task_id, link.record_id)
            if approval and approval.grant_id:
                row = self.execution.store.require(task_id, "task_grant", approval.grant_id)
                self.execution.revoke_grant(task_id, approval.grant_id, base_revision=row.revision)
                self.sync()
                return
        raise SrcStoreError("artifact_missing", "persistent inspection grant is unavailable")


__all__ = ["SrcInspectionBridge"]
