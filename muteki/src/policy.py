from __future__ import annotations

import ipaddress
import re
from datetime import datetime, timezone
from typing import Any, Iterable, Optional
from urllib.parse import urlsplit

from .contracts import ExecutionPolicy, ExecutionRequest, ExecutionRule, ResourceFact
from .store import SrcStoreError

EVALUATION_POINTS = {
    "file": "filesystem", "process": "tool", "network": "connection",
    "http": "request", "browser": "tool", "mcp": "tool", "control": "tool",
}

_READ_ONLY_BROWSER_ACTIONS = frozenset({"read", "logs", "screenshot", "wait"})
_READ_ONLY_FILE_TOOLS = frozenset({"Read", "Grep", "Glob", "LS"})
_READ_ONLY_CONTROL_VERBS = frozenset({
    "query", "list", "get", "read", "wait", "snapshot", "receipt",
    "describe", "preview", "export", "download",
})


def operation_idempotent(request: ExecutionRequest) -> bool:
    """Whether a trusted operation can be safely repeated after a failure.

    This is derived from the adapter's operation contract, never from an
    ``idempotent`` argument supplied with the model's tool call.
    """
    if request.resource_kind in {"process", "network", "mcp"}:
        return False
    if request.resource_kind == "file":
        return request.tool_name in _READ_ONLY_FILE_TOOLS
    if request.resource_kind == "browser":
        return request.action in _READ_ONLY_BROWSER_ACTIONS
    if request.resource_kind == "http":
        method = str(request.arguments.get("method") or "GET").upper()
        return request.action == "request" and method in {"GET", "HEAD", "OPTIONS"}
    if request.resource_kind == "control":
        verbs = set(request.action.lower().replace(":", ".").split("."))
        return bool(verbs & _READ_ONLY_CONTROL_VERBS)
    return False


TOOL_CAPABILITIES: dict[str, dict[str, Any]] = {
    "structured_http": {
        "trusted_input_fields": ["url", "method", "headers", "body", "identity_ref", "cwd"],
        "observed_resource_fields": ["url", "scheme", "host", "port", "path", "method", "headers", "body_sha256", "connected_ip", "identity_ref", "tenant", "object_id"],
        "pre_dispatch_gate": True, "environment_boundary": "managed-client",
        "network_scope": ["declared", "enforced"], "content_inspection": ["off", "record", "enforce"],
        "cancellation": "before-dispatch-or-close", "receipt_support": "exchange",
    },
    "managed_browser": {
        "trusted_input_fields": ["action", "url", "selector", "context_id"],
        "observed_resource_fields": ["page_url", "request_url", "context_id", "browser_generation"],
        "pre_dispatch_gate": True, "environment_boundary": "task-browser-declared",
        "network_scope": ["declared"], "content_inspection": ["off"],
        "cancellation": "best-effort", "receipt_support": "browser-action",
    },
    "shell": {
        "trusted_input_fields": ["argv", "cwd", "script_sha256", "environment_id"],
        "observed_resource_fields": ["tool_name", "command", "executable", "cwd", "uid", "mounts", "script_sha256", "file_path", "paths"],
        "pre_dispatch_gate": True, "environment_boundary": "container-os-mounts",
        "network_scope": ["declared", "enforced"], "content_inspection": ["off", "record", "enforce"],
        "cancellation": "process-group-best-effort", "receipt_support": "process-receipt",
    },
    "local_mcp": {
        "trusted_input_fields": ["server_id", "tool_name", "schema_version", "arguments"],
        "observed_resource_fields": ["server_id", "tool_name", "environment_id"],
        "pre_dispatch_gate": True, "environment_boundary": "server-specific",
        "network_scope": ["declared", "enforced"], "content_inspection": ["off", "record"],
        "cancellation": "adapter-specific", "receipt_support": "adapter-receipt",
    },
    "remote_mcp": {
        "trusted_input_fields": ["server_id", "tool_name", "schema_version", "arguments", "identity_ref"],
        "observed_resource_fields": ["server_id", "tool_name"],
        "pre_dispatch_gate": True, "environment_boundary": "remote-service-side-effects-not-local",
        "network_scope": ["declared"], "content_inspection": ["off"],
        "cancellation": "remote-receipt-specific", "receipt_support": "remote-receipt",
    },
    "raw_socket": {
        "trusted_input_fields": ["host", "port", "protocol", "payload_sha256"],
        "observed_resource_fields": ["connected_ip", "port", "protocol"],
        "pre_dispatch_gate": True, "environment_boundary": "managed-egress",
        "network_scope": ["declared", "enforced"], "content_inspection": ["off", "record"],
        "cancellation": "close", "receipt_support": "stream-receipt",
    },
}


def normalize_host(value: str) -> str:
    host = str(value or "").strip().rstrip(".").lower()
    if not host:
        return ""
    if host.startswith("[") and host.endswith("]"):
        host = host[1:-1]
    try:
        return str(ipaddress.ip_address(host))
    except ValueError:
        try:
            return host.encode("idna").decode("ascii")
        except UnicodeError:
            return host


def normalize_url(value: str) -> str:
    parts = urlsplit(str(value or "").strip())
    if not parts.scheme or not parts.hostname:
        return ""
    scheme = parts.scheme.lower()
    host = normalize_host(parts.hostname)
    try:
        port = parts.port
    except ValueError:
        return ""
    default = 80 if scheme == "http" else 443 if scheme == "https" else None
    authority = f"[{host}]" if ":" in host else host
    if port is not None and port != default:
        authority += f":{port}"
    path = parts.path or "/"
    return f"{scheme}://{authority}{path}" + (f"?{parts.query}" if parts.query else "")


def path_matches(path: str, prefix: str) -> bool:
    path = path or "/"
    prefix = prefix or "/"
    if prefix == "/":
        return True
    prefix = prefix.rstrip("/")
    return path == prefix or path.startswith(prefix + "/")


def target_matches(url: str, selector: str) -> bool:
    actual = urlsplit(normalize_url(url))
    expected = urlsplit(str(selector or "").strip())
    if not actual.scheme or not expected.scheme or not expected.hostname:
        return False
    if actual.scheme.lower() != expected.scheme.lower():
        return False
    if normalize_host(actual.hostname or "") != normalize_host(expected.hostname or ""):
        return False
    actual_port = actual.port or (80 if actual.scheme == "http" else 443)
    expected_port = expected.port or (80 if expected.scheme == "http" else 443)
    return actual_port == expected_port and path_matches(actual.path, expected.path)


def _fact(request: ExecutionRequest, name: str) -> ResourceFact:
    if name in request.resource_facts:
        return request.resource_facts[name]
    if name == "url":
        value = request.arguments.get("url") or request.arguments.get("target_url")
        return ResourceFact(value=value, origin="tool_input", state="known" if value else "unknown")
    if name in {"scheme", "host", "hostname", "port", "path", "raw_path"}:
        value = request.arguments.get("url") or request.arguments.get("target_url")
        if value:
            parts = urlsplit(str(value))
            derived = {
                "scheme": parts.scheme, "host": parts.hostname,
                "hostname": parts.hostname, "port": parts.port,
                "path": parts.path or "/", "raw_path": parts.path or "/",
            }
            if name == "port" and derived["port"] is None:
                derived["port"] = 80 if parts.scheme == "http" else 443 if parts.scheme == "https" else None
            return ResourceFact(value=derived[name], origin="tool_input", state="known" if derived[name] is not None else "unknown")
        return ResourceFact(origin="tool_input", state="unknown")
    direct = {
        "identity_ref": request.identity_ref, "identity_revision": request.identity_revision,
        "cwd": request.cwd, "environment_id": request.environment_id,
        "environment_generation": request.environment_generation,
        "tool_name": request.tool_name, "resource_kind": request.resource_kind,
        "task_id": request.task_id, "run_id": request.run_id, "worker_id": request.worker_id,
        "arguments_hash": request.arguments_hash,
    }
    if name in direct:
        return ResourceFact(value=direct[name], origin="environment", state="known" if direct[name] not in {None, ""} else "unknown")
    if name in request.arguments:
        return ResourceFact(value=request.arguments[name], origin="tool_input")
    return ResourceFact(origin="tool_input", state="unknown")


def _normalized(name: str, value: Any) -> Any:
    if name in {"url", "page_url", "request_url", "observed_url", "declared_target"}:
        return normalize_url(str(value or "")) if value else value
    if name in {"host", "hostname", "connected_ip", "ip"}:
        return normalize_host(str(value or "")) if value else value
    if name in {"path", "raw_path"}:
        return str(value or "/")
    if name in {"method", "action", "protocol", "scheme", "resource_kind"}:
        return str(value or "").lower()
    return value


def _compare(actual: Any, operator: str, expected: Any, *, name: str) -> bool:
    if operator == "exists":
        return (actual is not None) is bool(expected)
    if operator == "eq":
        return _normalized(name, actual) == _normalized(name, expected)
    if operator == "neq":
        return _normalized(name, actual) != _normalized(name, expected)
    if operator in {"in", "not_in"}:
        values = expected if isinstance(expected, list) else [expected]
        result = _normalized(name, actual) in {_normalized(name, item) for item in values}
        return not result if operator == "not_in" else result
    if operator == "contains":
        return str(expected) in str(actual)
    if operator == "not_contains":
        return str(expected) not in str(actual)
    if operator == "prefix":
        if name in {"path", "raw_path"}:
            return path_matches(str(actual), str(expected))
        return str(actual).startswith(str(expected))
    if operator == "wildcard":
        pattern = "^" + ".*".join(re.escape(part) for part in str(expected).split("*")) + "$"
        return re.match(pattern, str(actual)) is not None
    if operator == "regex":
        return re.search(str(expected), str(actual)) is not None
    if operator == "cidr":
        try:
            return ipaddress.ip_address(str(actual)) in ipaddress.ip_network(str(expected), strict=False)
        except ValueError:
            return False
    if operator in {"lt", "lte", "gt", "gte"}:
        left, right = float(actual), float(expected)
        return {"lt": left < right, "lte": left <= right, "gt": left > right, "gte": left >= right}[operator]
    if operator == "target":
        return target_matches(str(actual), str(expected))
    raise SrcStoreError("policy_unavailable", f"unsupported selector operator {operator}")


def _match_mapping(request: ExecutionRequest, mapping: dict[str, Any], prefix: str = "") -> tuple[bool, list[str]]:
    unknown: list[str] = []
    for raw_name, condition in mapping.items():
        name = f"{prefix}{raw_name}"
        fact = _fact(request, raw_name)
        if fact.state == "not_applicable":
            return False, []
        if fact.state == "unknown":
            unknown.append(name)
            continue
        operators = condition if isinstance(condition, dict) else {"eq": condition}
        if not operators:
            return False, []
        for operator, expected in operators.items():
            if fact.value is None:
                unknown.append(name)
                continue
            if not _compare(fact.value, operator, expected, name=raw_name):
                return False, []
    return not unknown, unknown


def match_rule(request: ExecutionRequest, rule: ExecutionRule) -> tuple[str, list[str]]:
    if not rule.enabled:
        return "no_match", []
    if EVALUATION_POINTS.get(rule.resource_kind) != rule.evaluation_point:
        return "no_match", []
    actual_point = EVALUATION_POINTS.get(request.resource_kind)
    if actual_point != rule.evaluation_point or request.resource_kind != rule.resource_kind:
        return "no_match", []
    if rule.actions and request.action not in rule.actions:
        return "no_match", []
    unknown: list[str] = []
    for mapping in (rule.selector, rule.conditions):
        matched, missing = _match_mapping(request, mapping)
        if not matched and not missing:
            return "no_match", []
        unknown.extend(missing)
    if unknown:
        return "unknown", sorted(set(unknown))
    return "match", []


def request_target(request: ExecutionRequest) -> str:
    for key in ("observed_url", "request_url", "page_url", "url"):
        fact = _fact(request, key)
        if fact.state == "known" and fact.value:
            return str(fact.value)
    return ""


def target_excluded(policy: ExecutionPolicy, request: ExecutionRequest) -> bool:
    target = request_target(request)
    if not target:
        return False
    return any(target_matches(target, excluded) for excluded in policy.excluded_targets)


def authorized_target(policy: ExecutionPolicy, request: ExecutionRequest) -> bool:
    target = request_target(request)
    if not target:
        return request.resource_kind not in {"http", "network"}
    if target_excluded(policy, request):
        return False
    return any(target_matches(target, allowed) for allowed in policy.authorized_targets)


def evaluate(policy: ExecutionPolicy, request: ExecutionRequest) -> dict[str, Any]:
    matched: list[tuple[ExecutionRule, str]] = []
    observed: list[str] = []
    unknown: list[str] = []
    for rule in policy.rules:
        state, missing = match_rule(request, rule)
        if rule.mode == "observe":
            if state != "no_match":
                observed.append(f"{rule.rule_id}:v{rule.version}")
            continue
        if state == "match":
            matched.append((rule, state))
        elif state == "unknown":
            unknown.extend(f"{rule.rule_id}:{field}" for field in missing)
    denies = [rule for rule, _ in matched if rule.effect == "deny"]
    asks = [rule for rule, _ in matched if rule.effect == "ask"]
    allows = [rule for rule, _ in matched if rule.effect == "allow"]
    refs = lambda rows: [f"{rule.rule_id}:v{rule.version}" for rule in rows]
    if denies:
        return {"effect": "deny", "status": "denied", "reason_code": "policy_denied",
                "matched_rule_refs": refs(denies), "unknown_fields": [], "observed_rule_refs": observed}
    if target_excluded(policy, request):
        return {"effect": "deny", "status": "denied", "reason_code": "policy_denied",
                "matched_rule_refs": [], "unknown_fields": [], "observed_rule_refs": observed}
    if unknown:
        return {
            "effect": "deny", "status": "unresolved", "reason_code": "authorization_unresolved",
            "unknown_fields": sorted(set(unknown)),
            "matched_rule_refs": [f"{rule.rule_id}:v{rule.version}" for rule, _ in matched],
            "observed_rule_refs": observed,
        }
    if asks:
        return {"effect": "ask", "status": "waiting_approval", "reason_code": "approval_required",
                "matched_rule_refs": refs(asks), "unknown_fields": [], "observed_rule_refs": observed}
    if not authorized_target(policy, request):
        return {"effect": "ask", "status": "unresolved", "reason_code": "authorization_unresolved",
                "matched_rule_refs": [], "unknown_fields": ["authorized_target"], "observed_rule_refs": observed}
    if allows:
        return {
            "effect": "allow", "status": "allowed", "reason_code": "allowed",
            "matched_rule_refs": refs(allows), "unknown_fields": [], "observed_rule_refs": observed,
        }
    default_effect = str((policy.defaults or {}).get("effect") or "allow").lower()
    if default_effect == "deny":
        return {
            "effect": "deny", "status": "denied", "reason_code": "policy_denied",
            "matched_rule_refs": [], "unknown_fields": [], "observed_rule_refs": observed,
        }
    if default_effect == "ask":
        return {
            "effect": "ask", "status": "waiting_approval", "reason_code": "approval_required",
            "matched_rule_refs": [], "unknown_fields": [], "observed_rule_refs": observed,
        }
    return {
        "effect": "allow", "status": "allowed", "reason_code": "default_allow",
        "matched_rule_refs": [], "unknown_fields": [], "observed_rule_refs": observed,
    }


def policy_digest(policy: ExecutionPolicy) -> str:
    from .store import SrcStore
    return SrcStore.digest(policy.model_copy(update={"digest": "", "activation": {}}).model_dump(mode="json"))


def latest_policy(records: Iterable[Any], task_id: str) -> ExecutionPolicy:
    candidates = [row for row in records if row.kind == "execution_policy"]
    if not candidates:
        raise SrcStoreError("policy_unavailable", "SRC Task has no readable execution policy")
    record = max(candidates, key=lambda row: int(row.payload.get("version") or 0))
    policy = ExecutionPolicy.model_validate(record.payload)
    if policy.task_id != task_id or policy_digest(policy) != policy.digest:
        raise SrcStoreError("policy_unavailable", "execution policy digest is invalid")
    return policy


def validate_activation(policy: ExecutionPolicy, capabilities: dict[str, Any]) -> None:
    if policy.network_mode == "enforced" and not capabilities.get("network_enforced"):
        raise SrcStoreError("capability_unavailable", "actual enforced egress is not available")
    inspection = str(policy.inspection_requirements.get("mode") or "off")
    if inspection == "enforce" and not capabilities.get("content_enforced"):
        raise SrcStoreError("inspection_unavailable", "pre-dispatch content enforcement is not available")
    enforced_rules = [rule for rule in policy.rules if rule.mode == "enforce" and rule.enabled]
    if enforced_rules:
        required_adapters = policy.environment_requirements.get("required_adapters")
        if not isinstance(required_adapters, list) or not required_adapters:
            raise SrcStoreError(
                "capability_unavailable",
                "enforced resource rules require an explicit supported required_adapters list",
            )
        coverage = capabilities.get("adapter_coverage") or {}
        for adapter in required_adapters:
            declaration = coverage.get(str(adapter))
            gate = str((declaration or {}).get("pre_dispatch_gate") or "unsupported")
            if gate.startswith(("unsupported", "limited")):
                raise SrcStoreError(
                    "capability_unavailable",
                    f"adapter {adapter} has no reliable pre-dispatch gate",
                )
    for rule in policy.rules:
        if rule.mode != "enforce" or not rule.enabled:
            continue
        capability = capabilities.get("tools", {}).get(rule.resource_kind)
        if capability is None:
            raise SrcStoreError("capability_unavailable", f"no enforcement boundary for {rule.resource_kind}")
        if not capability.get("pre_dispatch_gate"):
            raise SrcStoreError("capability_unavailable", f"{rule.resource_kind} has no pre-dispatch gate")
        supported = set(capability.get("observed_resource_fields") or [])
        required = set(rule.selector) | set(rule.conditions)
        missing = sorted(required - supported)
        if missing:
            raise SrcStoreError("capability_unavailable", f"unsupported rule fields: {', '.join(missing)}")


def expired(value: datetime) -> bool:
    return value <= datetime.now(timezone.utc)


__all__ = [name for name in globals() if not name.startswith("_")]
