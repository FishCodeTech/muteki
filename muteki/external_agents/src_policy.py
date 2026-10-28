from __future__ import annotations

import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

from muteki.src.contracts import ExecutionReceipt, ExecutionRequest, PolicyDecision
from muteki.src.execution import AuthorizationError, SrcExecutionService
from muteki.src.store import SrcStoreError

_SERVICES: dict[str, Any] = {}
_LOCK = threading.RLock()


def register_src_service(service: Any) -> None:
    """Use the Web stack's service when it is available.

    Structured adapters call this lazily through ``src_execution``; tests and
    standalone adapter processes can construct the same store-backed service.
    """
    key = str(Path(service.platform.db_path).resolve())
    with _LOCK:
        _SERVICES[key] = service


def _service(store: Any) -> Any:
    key = str(Path(store.db_path).resolve())
    with _LOCK:
        service = _SERVICES.get(key)
        if service is None:
            from muteki.src import SrcService
            service = SrcService(
                store, artifact_root=Path(store.db_path).parent / "src" / "tasks",
                conversation=None, run_gateway=None,
            )
            _SERVICES[key] = service
        return service


def src_task_id(store: Any, thread_id: Optional[str]) -> str:
    if not thread_id or store is None:
        return ""
    with store.lock:
        row = store.conn.execute(
            "SELECT task_id FROM src_records WHERE kind='task_binding' AND natural_key=?",
            (thread_id,),
        ).fetchone()
    return str(row["task_id"]) if row is not None else ""


def src_execution(store: Any) -> Optional[SrcExecutionService]:
    return _service(store).execution


@dataclass
class NativeAuthorization:
    request: ExecutionRequest
    decision: PolicyDecision
    receipt: ExecutionReceipt
    service: SrcExecutionService


class NativeToolError(SrcStoreError):
    def __init__(self, error: AuthorizationError) -> None:
        super().__init__(error.code, error.message, detail={
            "dispatch_state": error.receipt.dispatch_state if error.receipt else "",
            "outcome_known": error.receipt.outcome_known if error.receipt else False,
        })
        self.receipt = error.receipt


def resource_kind_for(approval_kind: str, tool_name: str) -> str:
    if approval_kind == "command_execution":
        return "process"
    if approval_kind in {"file_change", "permissions"}:
        return "file"
    if approval_kind == "browser":
        return "browser"
    if approval_kind == "http":
        return "http"
    if approval_kind in {"mcp_tool_call", "tool"}:
        return "mcp"
    return "control"


def assert_src_access_mode(
    store: Any, thread_id: Optional[str], access_mode: Optional[str], adapter_id: str,
) -> None:
    if not src_task_id(store, thread_id):
        return
    declaration = ADAPTER_EXECUTION_COVERAGE.get(adapter_id) or {}
    gate = str(declaration.get("pre_dispatch_gate") or "unsupported")
    full_gate = not gate.startswith(("unsupported", "limited"))
    unattended = access_mode is None or not str(access_mode).strip()
    if not full_gate and (unattended or access_mode in {"full-access", "auto"}):
        raise NativeToolError(AuthorizationError(
            "capability_unavailable",
            f"adapter {adapter_id} cannot enforce SRC policy in access mode {access_mode or 'unattended'}",
        ))


async def authorize_native_tool(
    store: Any, *, thread_id: Optional[str], invocation_id: str,
    tool_name: str, arguments: dict[str, Any], action: str,
    approval_kind: str = "tool", run_id: Optional[str] = None,
    worker_id: Optional[str] = None, cwd: str = "",
    identity_ref: str = "default", environment_id: str = "",
    environment_generation: int = 0, idempotent: bool = False,
    wait_for_approval: bool = True, adapter_id: str = "",
) -> Optional[NativeAuthorization]:
    task_id = src_task_id(store, thread_id)
    if not task_id:
        return None
    service = _service(store).execution
    policy = service.current_policy(task_id)
    required_adapters = policy.environment_requirements.get("required_adapters") or []
    if required_adapters and adapter_id not in required_adapters:
        raise NativeToolError(AuthorizationError(
            "capability_unavailable",
            f"adapter {adapter_id or 'unknown'} is not enabled by this policy",
        ))
    declaration = ADAPTER_EXECUTION_COVERAGE.get(adapter_id) or {}
    gate = str(declaration.get("pre_dispatch_gate") or "unsupported")
    if required_adapters and gate.startswith(("unsupported", "limited")):
        raise NativeToolError(AuthorizationError(
            "capability_unavailable",
            f"adapter {adapter_id or 'unknown'} has no universal pre-dispatch gate",
        ))
    binding = _service(store).binding(task_id)
    resource_kind = resource_kind_for(approval_kind, tool_name)
    arguments = dict(arguments)
    arguments.pop("idempotent", None)
    request = ExecutionRequest(
        invocation_id=invocation_id, task_id=task_id, thread_id=binding.thread_id,
        run_id=run_id, worker_id=worker_id, tool_name=tool_name, action=action,
        arguments=arguments, arguments_hash=_service(store).store.digest(arguments),
        resource_kind=resource_kind, identity_ref=identity_ref,
        environment_id=environment_id, environment_generation=environment_generation,
        cwd=cwd, policy_version=policy.version,
    )
    try:
        decision = await service.authorize(request, wait_for_approval=wait_for_approval)
        receipt = service.mark_dispatched(request, decision)
    except AuthorizationError as exc:
        raise NativeToolError(exc) from exc
    return NativeAuthorization(request, decision, receipt, service)


def complete_native_tool(
    authorization: NativeAuthorization, *, result: Any = None,
    error: Optional[Exception] = None, unknown: bool = False,
) -> ExecutionReceipt:
    return authorization.service.record_outcome(
        authorization.request, result=result, error=error, unknown=unknown,
    )


def adapter_coverage(hook: str, *, completion_events: bool) -> dict[str, Any]:
    return {
        "trusted_input_fields": ["tool_name", "arguments", "cwd", "identity_ref"],
        "observed_resource_fields": ["tool_name", "arguments", "resource_facts"],
        "pre_dispatch_gate": hook,
        "environment_boundary": "existing-runtime-and-container",
        "network_scope": "task-environment",
        "content_inspection": "task-environment",
        "cancellation": "existing-adapter",
        "receipt_support": "dispatch-and-completion" if completion_events else "dispatch-only",
    }


ADAPTER_EXECUTION_COVERAGE = {
    "claude.agent_sdk": adapter_coverage("PreToolUse", completion_events=True),
    "acp": adapter_coverage("limited: native permission requests, not every tool", completion_events=True),
    "codex.app_server": adapter_coverage("limited: native approval requests only", completion_events=False),
    "droid.rpc": adapter_coverage("limited: native PermissionRequest only", completion_events=False),
    "opencode": adapter_coverage("limited: permission.asked only", completion_events=False),
    "kimi": adapter_coverage("limited: native approval notification only", completion_events=False),
    "devin.acp": adapter_coverage("limited: terminal/create and native permission requests", completion_events=False),
    "cursor.sdk": adapter_coverage("unsupported: no interactive pre-dispatch hook", completion_events=False),
    "cli": adapter_coverage("unsupported: no in-band pre-dispatch hook", completion_events=False),
    "pi.rpc": adapter_coverage("unsupported: no in-band approval", completion_events=False),
    "structured_http": adapter_coverage("managed pre-dispatch", completion_events=True),
    "managed_browser": adapter_coverage("managed pre-dispatch", completion_events=True),
    "local_mcp": adapter_coverage("managed Gateway", completion_events=True),
}


__all__ = [
    "NativeAuthorization", "NativeToolError", "adapter_coverage",
    "authorize_native_tool", "complete_native_tool", "register_src_service",
    "resource_kind_for", "src_execution", "src_task_id",
]
