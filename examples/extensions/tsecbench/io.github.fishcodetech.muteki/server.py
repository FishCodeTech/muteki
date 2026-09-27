#!/usr/bin/env python3
"""Tsecbench PlatformAdapter — Muteki Extension Host 子进程。

实现 PlatformAdapter 全套动词（probe / sync_competition / fetch_artifact /
acquire_instance / renew_instance / release_instance / submit），经 JSON-RPC
stdio 与宿主通信。不依赖 muteki 包本体。

凭据：宿主在 RPC payload 中注入 ``credential``（BENCHMARK_TOKEN 明文），
本进程不把 token 写入 state/config。所有平台操作都调用真实 Tsecbench API。
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import sys
import tempfile
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from typing import Any
from urllib.parse import urlencode

PROTOCOL_VERSION = 1
EXTENSION_ID = os.environ.get(
    "MUTEKI_EXTENSION_ID", "org.muteki.platforms.tsecbench"
)
PROVIDE_ID = "tsecbench"
CMD_PREFIX = f"ext.{EXTENSION_ID}.platform_adapter.{PROVIDE_ID}."
CHALLENGE_ORDER = (
    "d-02", "e1-06", "d-01", "e2-01", "e2-03", "a-10", "e1-02",
    "d-05", "a-01", "f1-04", "e1-01", "d-03", "a-08", "e3-02",
    "a-04", "e3-03", "e1-05", "e1-03", "a-06", "a-09", "e3-01",
    "a-02", "f1-03", "f2-03", "a-15", "f1-05", "f2-02", "a-12",
    "a-11", "c-07", "f1-02", "c-05", "f2-08", "d-06", "c-01",
    "f2-07", "e1-04", "f1-01", "a-17", "f2-04", "d-04", "e2-04",
    "e2-02", "c-08", "a-13", "c-04", "a-14", "c-06", "f2-06",
    "a-07", "c-03", "a-03", "c-09", "f2-01", "c-02", "a-16",
    "a-18", "a-05", "e3-04", "f2-05", "b-01", "b-02", "b-03",
)
TERMINAL_PHASE_CHALLENGES = (
    "e3-04", "f2-05", "b-01", "b-02", "b-03",
)
RUN_DURATION_SECONDS = 6 * 60 * 60
VPN_HEALTHCHECK_URL = "http://10.0.100.58"
VPN_CONNECT_COMMAND = (
    "sudo /opt/homebrew/sbin/openvpn --config "
    "~/Downloads/<tsecbench-vpn-config>.ovpn"
)
# Tsecbench 的健康地址与靶场 API 必须沿系统路由直连。宿主可能为模型 API
# 配置 HTTP_PROXY/ALL_PROXY；urllib 默认继承这些变量，会把 VPN 内网请求错误
# 送入本地代理，导致隧道已经建立仍被判断为未连接。
DIRECT_HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))

STATE: dict[str, Any] = {
    "config": {},
    "active": False,
    # connection_id::unique_code -> observed remote environment
    "instances": {},
    # Persisted across releases so a reopened challenge always receives a new
    # platform_instance_id, even when Tsecbench reuses the same address.
    "environment_sequences": {},
    # connection_id::submission_id -> dispatch journal.  Values never contain
    # the flag itself; the journal lets a restarted host recover a response
    # without blindly submitting the same candidate again.
    "submissions": {},
    # connection_id -> immutable batch clock established by the first
    # authenticated OpenAPI call.  Tsecbench starts the six-hour countdown on
    # that call, so the plugin can expose a stable end time without browser
    # cookies or replaying the console event history.
    "run_clocks": {},
    # Runtime-only health observations. VPN is connected manually by the user;
    # the plugin only caches the safe result of the official health endpoint.
    "vpn_status": {},
}


def _write(message: dict[str, Any]) -> None:
    sys.stdout.buffer.write(
        json.dumps(message, ensure_ascii=False).encode("utf-8") + b"\n"
    )
    sys.stdout.buffer.flush()


def _respond(request_id: Any, result: Any = None, error: Any = None) -> None:
    message: dict[str, Any] = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        message["error"] = error
    else:
        message["result"] = result if result is not None else {}
    _write(message)


def _state_path() -> str:
    root = os.environ.get("MUTEKI_EXTENSION_STATE_DIR", ".")
    return os.path.join(root, "state.json")


def _load_state() -> None:
    try:
        with open(_state_path(), encoding="utf-8") as handle:
            data = json.load(handle)
    except FileNotFoundError:
        STATE["instances"] = {}
        STATE["environment_sequences"] = {}
        STATE["submissions"] = {}
        STATE["run_clocks"] = {}
        return
    if not isinstance(data, dict) or not isinstance(data.get("active_instances", {}), dict):
        raise ValueError("state.json: active_instances must be an object")
    instances = data.get("active_instances", {})
    if not all(isinstance(item, dict) for item in instances.values()):
        raise ValueError("state.json: instance entries must be objects")
    for instance in instances.values():
        instance.pop("timebox_seconds", None)
        instance.pop("expires_at", None)
    sequences = data.get("environment_sequences", {})
    if not isinstance(sequences, dict) or not all(
        isinstance(value, int) and not isinstance(value, bool) and value >= 0
        for value in sequences.values()
    ):
        raise ValueError("state.json: environment_sequences must contain non-negative integers")
    submissions = data.get("submissions", {})
    if not isinstance(submissions, dict) or not all(
        isinstance(value, dict) for value in submissions.values()
    ):
        raise ValueError("state.json: submissions must be an object of records")
    run_clocks = data.get("run_clocks", {})
    if not isinstance(run_clocks, dict) or not all(
        isinstance(value, dict) for value in run_clocks.values()
    ):
        raise ValueError("state.json: run_clocks must be an object of records")
    STATE["instances"] = dict(instances)
    STATE["environment_sequences"] = dict(sequences)
    STATE["submissions"] = dict(submissions)
    STATE["run_clocks"] = dict(run_clocks)


def _save_state() -> None:
    path = _state_path()
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", dir=os.path.dirname(path) or ".",
            prefix=".state-", suffix=".json", delete=False,
        ) as handle:
            temporary = handle.name
            json.dump(
                {
                    "active_instances": STATE["instances"],
                    "environment_sequences": STATE["environment_sequences"],
                    "submissions": STATE["submissions"],
                    "run_clocks": STATE["run_clocks"],
                },
                handle,
            )
        os.replace(temporary, path)
    finally:
        if temporary is not None and os.path.exists(temporary):
            os.unlink(temporary)


def _auth_header() -> str:
    return str(STATE["config"].get("auth_header") or "BENCHMARK_TOKEN")


def _base_url(payload: dict[str, Any]) -> str:
    conn = payload.get("connection") or {}
    endpoint = str(conn.get("endpoint") or "").rstrip("/")
    if endpoint:
        return endpoint
    return str(
        STATE["config"].get("base_url_default")
        or "https://tsecbench.zc.tencent.com"
    ).rstrip("/")


def _token(payload: dict[str, Any]) -> str:
    cred = _credential(payload)
    if isinstance(cred, dict):
        return str(
            cred.get("value")
            or cred.get("token")
            or cred.get("BENCHMARK_TOKEN")
            or ""
        )
    if isinstance(cred, str):
        return cred.strip()
    return ""


def _credential(payload: dict[str, Any]) -> Any:
    """Decode the connection secret.

    New connections store a plain token. JSON remains valid because a caller
    may include authoritative ``started_at`` / ``ends_at`` values with the
    token to align the six-hour clock.
    """
    cred = payload.get("credential")
    if not isinstance(cred, str):
        return cred
    text = cred.strip()
    if not text.startswith("{"):
        return text
    try:
        parsed = json.loads(text)
    except json.JSONDecodeError:
        return text
    return parsed if isinstance(parsed, dict) else text


def _check_vpn() -> dict[str, Any]:
    req = urllib.request.Request(VPN_HEALTHCHECK_URL, headers={"Accept": "application/json"})
    try:
        with DIRECT_HTTP.open(req, timeout=10) as resp:
            body = json.loads(resp.read(16 * 1024).decode("utf-8"))
    except Exception as exc:
        raise RuntimeError(
            "Tsecbench VPN is not connected; run: " + VPN_CONNECT_COMMAND
        ) from exc
    if int(resp.status) != 200 or not isinstance(body, dict) or body.get("status") != "ok":
        raise RuntimeError(
            "Tsecbench VPN health check failed; run: " + VPN_CONNECT_COMMAND
        )
    return {
        "state": "connected",
        "managed": False,
        "client_ip": str(body.get("client_ip") or ""),
        "connect_command": VPN_CONNECT_COMMAND,
        "checked_at": datetime.now(timezone.utc).isoformat(),
    }


def _clock_for(payload: dict[str, Any], *, establish: bool = False) -> dict[str, Any]:
    connection_id = _connection_id(payload)
    clock = STATE["run_clocks"].get(connection_id)
    if not isinstance(clock, dict) and establish:
        credential = _credential(payload)
        supplied_start = (
            str(credential.get("started_at") or "").strip()
            if isinstance(credential, dict) else ""
        )
        supplied_end = (
            str(credential.get("ends_at") or "").strip()
            if isinstance(credential, dict) else ""
        )
        try:
            started_dt = datetime.fromisoformat(
                supplied_start.replace("Z", "+00:00")
            )
            ends_dt = datetime.fromisoformat(
                supplied_end.replace("Z", "+00:00")
            )
            if started_dt.tzinfo is None:
                started_dt = started_dt.replace(tzinfo=timezone.utc)
            if ends_dt.tzinfo is None:
                ends_dt = ends_dt.replace(tzinfo=timezone.utc)
            duration = int((ends_dt - started_dt).total_seconds())
            if duration != RUN_DURATION_SECONDS:
                raise ValueError("unexpected Tsecbench duration")
            source = "platform_run_status"
        except (TypeError, ValueError):
            started = time.time()
            started_dt = datetime.fromtimestamp(started, tz=timezone.utc)
            ends_dt = datetime.fromtimestamp(
                started + RUN_DURATION_SECONDS, tz=timezone.utc
            )
            duration = RUN_DURATION_SECONDS
            source = "first_authenticated_openapi_call"
        clock = {
            "started_at": started_dt.astimezone(timezone.utc).isoformat(),
            "ends_at": ends_dt.astimezone(timezone.utc).isoformat(),
            "duration_seconds": duration,
            "source": source,
        }
        STATE["run_clocks"][connection_id] = clock
        _save_state()
    if not isinstance(clock, dict):
        return {}
    result = dict(clock)
    try:
        ends = datetime.fromisoformat(
            str(result.get("ends_at") or "").replace("Z", "+00:00")
        ).timestamp()
        result["remaining_seconds"] = max(0, int(math.ceil(ends - time.time())))
        result["status"] = "running" if result["remaining_seconds"] > 0 else "ended"
    except ValueError:
        result["remaining_seconds"] = 0
        result["status"] = "unknown"
    return result


def _http(
    method: str,
    url: str,
    *,
    token: str,
    body: dict[str, Any] | None = None,
    timeout: float = 30.0,
) -> tuple[int, Any]:
    data = None
    headers = {_auth_header(): token, "Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with DIRECT_HTTP.open(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            status = int(resp.status)
            try:
                return status, json.loads(raw) if raw else {}
            except json.JSONDecodeError:
                return status, {"raw": raw}
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        try:
            parsed = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            parsed = {"raw": raw}
        return int(exc.code), parsed
    except urllib.error.URLError as exc:
        return 0, {"error": str(exc.reason if hasattr(exc, "reason") else exc)}
    except OSError as exc:
        return 0, {"error": str(exc)}


def _err(code: str, message: str, **extra: Any) -> dict[str, Any]:
    body = {"error": {"code": code, "message": message, **extra}}
    return body


def _connection_id(payload: dict[str, Any]) -> str:
    connection = payload.get("connection") or {}
    if isinstance(connection, dict) and connection.get("connection_id"):
        return str(connection["connection_id"])
    for field in ("challenge", "lease", "request"):
        nested = payload.get(field) or {}
        if isinstance(nested, dict) and nested.get("connection_id"):
            return str(nested["connection_id"])
    return "default"


def _instance_key(payload: dict[str, Any], code: str) -> str:
    return f"{_connection_id(payload)}::{code}"


def _timestamp(value: Any) -> float:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    if isinstance(value, str) and value.strip():
        try:
            return float(value)
        except ValueError:
            try:
                return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
            except ValueError:
                pass
    return time.time()


def _held_instance(payload: dict[str, Any], code: str) -> dict[str, Any] | None:
    """Read a namespaced entry and lazily migrate the pre-1.0.1 key shape."""
    key = _instance_key(payload, code)
    held = STATE["instances"].get(key)
    if isinstance(held, dict):
        return held
    legacy = STATE["instances"].pop(code, None)
    if isinstance(legacy, dict):
        migrated = dict(legacy)
        addresses = _normalise_addresses(migrated.get("addrs"))
        started_at = _timestamp(migrated.get("started_at"))
        sequence = max(1, int(STATE["environment_sequences"].get(key) or 0))
        STATE["environment_sequences"][key] = sequence
        migrated.update({
            "connection_id": _connection_id(payload),
            "challenge_key": code,
            "generation": sequence,
            # Preserve the old adapter's platform_instance_id for the currently
            # running environment. Migration alone must not look like a new
            # environment and relaunch the host Run.
            "lease_id": str(migrated.get("lease_id") or f"tsec-{code}"),
            "addrs": addresses,
            "address_fingerprint": _address_fingerprint(addresses),
            "container_status": "available",
            "started_at": started_at,
            "observed_at": time.time(),
        })
        migrated.pop("timebox_seconds", None)
        migrated.pop("expires_at", None)
        STATE["instances"][key] = migrated
        _save_state()
        return migrated
    return None


def _normalise_addresses(value: Any) -> list[str]:
    if not isinstance(value, list):
        return []
    addresses: list[str] = []
    for raw in value:
        address = str(raw or "").strip()
        if address and address not in addresses:
            addresses.append(address)
    return addresses


def _address_fingerprint(addresses: list[str]) -> str:
    return hashlib.sha256(
        json.dumps(addresses, ensure_ascii=False, separators=(",", ":")).encode()
    ).hexdigest()[:12]


def _endpoint_value(address: str) -> str:
    # Preserve the existing adapter contract: Tsecbench addresses are exposed as
    # HTTP endpoints unless the platform already supplied an explicit scheme.
    return address if "://" in address else f"http://{address}"


def _endpoints(addresses: list[str]) -> dict[str, str]:
    if not addresses:
        return {}
    endpoints = {"http": _endpoint_value(addresses[0])}
    for index, address in enumerate(addresses[1:], start=2):
        endpoints[f"address_{index}"] = _endpoint_value(address)
    return endpoints


def _remember_instance(
    payload: dict[str, Any],
    code: str,
    addresses: list[str],
    *,
    force_new: bool = False,
) -> dict[str, Any]:
    """Persist one observed environment and allocate a stable local epoch.

    Tsecbench does not expose an instance UUID. A fresh successful start therefore
    receives a new synthetic id even if its address is reused. During an observed
    continuous lifetime, an address change also creates a new id so the host can
    fence stale execution state while retaining the original Run history.
    """
    key = _instance_key(payload, code)
    held = _held_instance(payload, code)
    fingerprint = _address_fingerprint(addresses)
    same_environment = bool(
        held
        and not force_new
        and held.get("address_fingerprint") == fingerprint
    )
    if same_environment:
        instance = dict(held)
        instance.update({
            "addrs": addresses,
            "container_status": "available",
            "observed_at": time.time(),
        })
        instance.pop("timebox_seconds", None)
        instance.pop("expires_at", None)
    else:
        sequence = int(STATE["environment_sequences"].get(key) or 0) + 1
        STATE["environment_sequences"][key] = sequence
        scope = hashlib.sha256(_connection_id(payload).encode()).hexdigest()[:8]
        started_at = time.time()
        instance = {
            "connection_id": _connection_id(payload),
            "challenge_key": code,
            "generation": sequence,
            "lease_id": f"tsec-{scope}-{code}-g{sequence}-{fingerprint}",
            "addrs": addresses,
            "address_fingerprint": fingerprint,
            "container_status": "available",
            "started_at": started_at,
            "observed_at": started_at,
        }
    STATE["instances"][key] = instance
    _save_state()
    return instance


def _forget_instance(payload: dict[str, Any], code: str) -> dict[str, Any] | None:
    key = _instance_key(payload, code)
    held = STATE["instances"].pop(key, None)
    # Remove an unmigrated legacy entry as well.
    if held is None:
        held = STATE["instances"].pop(code, None)
    _save_state()
    return held if isinstance(held, dict) else None


def _instance_result(
    payload: dict[str, Any],
    code: str,
    instance: dict[str, Any],
    *,
    previous_lease: dict[str, Any] | None = None,
) -> dict[str, Any]:
    previous_lease = previous_lease or {}
    fencing = max(1, int(previous_lease.get("fencing_token") or 0) + 1)
    addresses = _normalise_addresses(instance.get("addrs"))
    lease_result = {
        "lease_id": str(instance.get("lease_id") or ""),
        "connection_id": _connection_id(payload),
        "challenge_key": code,
        "fencing_token": fencing,
    }
    return {
        "result": {
            "lease": lease_result,
            "endpoints": _endpoints(addresses),
        }
    }


def _find_challenge(items: list[dict[str, Any]], code: str) -> dict[str, Any] | None:
    return next(
        (item for item in items if str(item.get("unique_code") or "") == code),
        None,
    )


def _remote_error(body: Any) -> tuple[str, str]:
    """Extract the documented SDK error code/message from common envelopes."""
    candidates: list[dict[str, Any]] = []
    if isinstance(body, dict):
        candidates.append(body)
        for key in ("error", "detail"):
            nested = body.get(key)
            if isinstance(nested, dict):
                candidates.append(nested)
    code = ""
    message = ""
    for candidate in candidates:
        code = code or str(
            candidate.get("code")
            or candidate.get("error_code")
            or candidate.get("category")
            or ""
        ).strip()
        message = message or str(candidate.get("message") or "").strip()
    return code.lower(), message[:300]


def _http_error(
    operation: str,
    status: int,
    body: Any,
    *,
    uncertain_mutation: bool = False,
) -> dict[str, Any]:
    remote_code, remote_message = _remote_error(body)
    detail = f"{remote_code}: {remote_message}".strip(": ")
    message = f"{operation} HTTP {status}" + (f" ({detail})" if detail else "")
    extra = {"remote_code": remote_code, "status_code": status}
    if status == 0:
        fallback = "request result is unknown" if uncertain_mutation else "network error"
        return _err("unknown" if uncertain_mutation else "transport", f"{operation}: {fallback}", **extra)
    if status == 401:
        return _err("auth_required", message, **extra)
    if status == 403:
        return _err("permission", message, **extra)
    if status == 404:
        return _err("not_found", message, **extra)
    if status == 409:
        if "max" in remote_message.lower() and "active" in remote_message.lower():
            return _err("rate_limited", message, retry_after_seconds=5, **extra)
        return _err("invalid_state", message, **extra)
    if status == 429:
        return _err("rate_limited", message, retry_after_seconds=5, **extra)
    if status >= 500:
        if uncertain_mutation and remote_code not in {"resource_unavailable"}:
            return _err("unknown", message, **extra)
        return _err("infra", message, retry_after_seconds=5, **extra)
    return _err("transport", message, **extra)


def _snapshot(item: dict[str, Any]) -> dict[str, Any]:
    code = str(item.get("unique_code") or "")
    difficulty = str(item.get("difficulty") or "medium")
    flag_count = int(item.get("flag_count") or 1)
    completed = bool(item.get("is_completed"))
    # Keep raw_payload small: full platform objects can be large enough to
    # blow the host command/handle timeout when syncing 60+ challenges.
    raw = {
        key: item.get(key)
        for key in (
            "unique_code",
            "difficulty",
            "total_score",
            "flag_count",
            "correct_flag_count",
            "is_completed",
            "container_status",
            "container_addr",
        )
        if key in item
    }
    return {
        "external_challenge_id": code,
        "name": code,
        "category": f"tsec/{difficulty}",
        "remote_state": "solved_remote" if completed else "open",
        "hidden": False,
        "points": float(item.get("total_score") or 0),
        "description": str(item.get("description") or "")[:2000],
        "target": "",
        "flag_format": r".+\{.+\}",
        "hints": [],
        "prerequisites": [],
        "multi_flag": flag_count > 1,
        "expected_flags": flag_count,
        "artifacts": [],
        "revision_extensions": {
            "difficulty": difficulty,
            "correct_flag_count": int(item.get("correct_flag_count") or 0),
            "flag_count": flag_count,
            "container_status": str(item.get("container_status") or ""),
            "container_addr": _normalise_addresses(item.get("container_addr")),
        },
        "raw_payload": raw,
    }


def _policy_hints() -> dict[str, Any]:
    return {
        "policy_profile": "tsec_eval",
        "max_instances": int(STATE["config"].get("max_instances") or 3),
        "max_concurrent_runs": 3,
        "working_set": 3,
        "keepalive_max": 0,
        "round_timeboxes_s": [],
        "visit_floor_s": 0,
        "fill_idle_revisits": False,
        "challenge_order": list(CHALLENGE_ORDER),
        "terminal_phase_challenge_ids": list(TERMINAL_PHASE_CHALLENGES),
        "persistent_challenge_ids": list(CHALLENGE_ORDER),
        "terminal_phase_fill_idle": False,
        "total_budget_s": 21300,
        "automation_mode": "autonomous",
    }


def _list_challenges(payload: dict[str, Any]) -> tuple[list[dict[str, Any]] | None, dict[str, Any] | None]:
    token = _token(payload)
    if not token:
        return None, _err("auth_required", "BENCHMARK_TOKEN credential missing")
    try:
        vpn_status = _check_vpn()
    except Exception as exc:
        vpn_status = {
            "state": "disconnected",
            "managed": False,
            "connect_command": VPN_CONNECT_COMMAND,
        }
        STATE["vpn_status"][_connection_id(payload)] = vpn_status
        return None, _err(
            "transport",
            str(exc),
            vpn_required=True,
            connect_command=VPN_CONNECT_COMMAND,
        )
    STATE["vpn_status"][_connection_id(payload)] = vpn_status
    base = _base_url(payload)
    timeout = float(STATE["config"].get("timeout_seconds") or 30)
    status, body = _http("GET", f"{base}/openapi/v1/challenges", token=token, timeout=timeout)
    if status == 0 or status >= 400:
        return None, _http_error("list challenges", status, body)
    if not isinstance(body, list):
        return None, _err("unknown", "challenges list is not an array")
    _clock_for(payload, establish=True)
    return body, None


# ---------------------------------------------------------------------------
# PlatformAdapter verbs
# ---------------------------------------------------------------------------


def op_probe(payload: dict[str, Any]) -> dict[str, Any]:
    items, err = _list_challenges(payload)
    if err:
        return err
    assert items is not None
    return {
        "capabilities": {
            "platform_kind": PROVIDE_ID,
            "sync": True,
            "artifacts": False,
            "dynamic_instances": True,
            "submit": True,
            "scoreboard": False,
            "detail": {
                "auth": f"header:{_auth_header()}",
                "max_instances": int(STATE["config"].get("max_instances") or 3),
                "multi_flag": True,
                "challenge_count": len(items),
                "policy_hints": _policy_hints(),
                "vpn_required": True,
                "vpn": dict(
                    STATE["vpn_status"].get(_connection_id(payload)) or {}
                ),
                "competition_clock": _clock_for(payload),
            },
        }
    }


def op_sync_competition(payload: dict[str, Any]) -> dict[str, Any]:
    request = payload.get("request") or {}
    cleanup: list[dict[str, Any]] = []
    clock = _clock_for(payload)
    if clock.get("status") == "ended":
        prefix = f"{_connection_id(payload)}::"
        held_codes = [
            key[len(prefix):]
            for key in list(STATE["instances"])
            if key.startswith(prefix)
        ]
        for code in held_codes:
            _forget_instance(payload, code)
            cleanup.append({
                "challenge_key": code,
                "released": True,
                "remote_status": "competition_ended",
            })
    items, err = _list_challenges(payload)
    if err:
        return err
    assert items is not None
    if clock.get("status") != "ended":
        for item in items:
            code = str(item.get("unique_code") or "")
            if not code or not bool(item.get("is_completed")):
                continue
            if _held_instance(payload, code) is None:
                continue
            released = _close_remote_instance(payload, code)
            cleanup.append({
                "challenge_key": code,
                "released": bool(released.get("released")),
                "error": released.get("error"),
            })
    snapshots = [_snapshot(item) for item in items]
    digest = hashlib.sha256(
        json.dumps(
            [(s["external_challenge_id"], s["remote_state"], s["points"]) for s in snapshots],
            sort_keys=True,
        ).encode()
    ).hexdigest()[:16]
    vpn = {
        key: value
        for key, value in dict(
            STATE["vpn_status"].get(_connection_id(payload)) or {}
        ).items()
        if key != "checked_at"
    }
    return {
        "result": {
            "connection_id": str(request.get("connection_id") or ""),
            "cursor": digest,
            "synced_challenges": len(snapshots),
            "detail": {
                "snapshots": snapshots,
                "snapshot_complete": True,
                "competition_clock": clock,
                "platform_status": {
                    "vpn": vpn,
                    "remote": {
                        "state": clock.get("status", "unknown"),
                    },
                    "completed_instance_cleanup": cleanup,
                },
            },
        }
    }


def op_fetch_artifact(payload: dict[str, Any]) -> dict[str, Any]:
    return _err("not_found", "tsecbench has no downloadable artifacts")


def op_acquire_instance(payload: dict[str, Any]) -> dict[str, Any]:
    challenge = payload.get("challenge") or {}
    code = str(challenge.get("challenge_key") or "")
    if not code:
        return _err("not_found", "challenge_key required")
    max_n = int(STATE["config"].get("max_instances") or 3)
    items, err = _list_challenges(payload)
    if err:
        return err
    assert items is not None
    item = _find_challenge(items, code)
    if item is None:
        return _err("not_found", f"challenge not found: {code}")
    container_status = str(item.get("container_status") or "").lower()
    addresses = _normalise_addresses(item.get("container_addr"))
    if container_status == "available":
        if not addresses:
            return _err("unknown", f"{code} is available but has no container_addr")
        instance = _remember_instance(payload, code, addresses)
        return _instance_result(payload, code, instance)
    if container_status in {"pending", "stop_pending"}:
        return _err("invalid_state", f"{code} container is {container_status}")
    if container_status != "stopped":
        return _err("unknown", f"{code} has unknown container_status: {container_status or '<empty>'}")
    active_count = sum(
        str(candidate.get("container_status") or "").lower()
        in {"pending", "available", "stop_pending"}
        for candidate in items
    )
    if active_count >= max_n:
        return _err(
            "rate_limited",
            f"max active instances ({max_n})",
            retry_after_seconds=3,
        )

    token = _token(payload)
    if not token:
        return _err("auth_required", "BENCHMARK_TOKEN credential missing")
    base = _base_url(payload)
    timeout = float(STATE["config"].get("timeout_seconds") or 30)
    q = urlencode({"unique_code": code})
    status, body = _http(
        "POST",
        f"{base}/openapi/v1/challenges/start?{q}",
        token=token,
        timeout=timeout,
    )
    if status == 0 or status >= 400:
        return _http_error(
            "start challenge", status, body, uncertain_mutation=True
        )
    addresses = _normalise_addresses(
        body.get("container_addr") if isinstance(body, dict) else None
    )
    if not addresses:
        return _err("unknown", "start returned no container_addr")
    instance = _remember_instance(payload, code, addresses, force_new=True)
    return _instance_result(payload, code, instance)


def op_renew_instance(payload: dict[str, Any]) -> dict[str, Any]:
    lease = payload.get("lease") or {}
    code = str(lease.get("challenge_key") or "")
    if not code:
        return _err("not_found", "challenge_key required")
    held = _held_instance(payload, code)
    if not held:
        return _err("not_found", f"instance not held: {code}")
    items, err = _list_challenges(payload)
    if err:
        return err
    assert items is not None
    item = _find_challenge(items, code)
    if item is None:
        return _err("not_found", f"challenge not found: {code}")
    container_status = str(item.get("container_status") or "").lower()
    if container_status == "stopped":
        _forget_instance(payload, code)
        return _err("not_found", f"remote container stopped: {code}")
    if container_status != "available":
        return _err("invalid_state", f"{code} container is {container_status or 'unknown'}")
    addresses = _normalise_addresses(item.get("container_addr"))
    if not addresses:
        return _err("unknown", f"{code} is available but has no container_addr")
    instance = _remember_instance(payload, code, addresses)
    return _instance_result(payload, code, instance, previous_lease=lease)


def _close_remote_instance(payload: dict[str, Any], code: str) -> dict[str, Any]:
    """Close one Tsec environment and forget it only after confirmation."""
    # Persist a legacy environment as generation 1 before attempting the
    # remote mutation. A successful close removes only the active record; the
    # sequence survives so a later reopen is always generation 2.
    _held_instance(payload, code)
    token = _token(payload)
    if not token:
        return _err("auth_required", "BENCHMARK_TOKEN credential missing")
    base = _base_url(payload)
    timeout = float(STATE["config"].get("timeout_seconds") or 30)
    q = urlencode({"unique_code": code})
    status, _body = _http(
        "POST",
        f"{base}/openapi/v1/challenges/close?{q}",
        token=token,
        timeout=timeout,
    )
    closed = bool(isinstance(_body, dict) and _body.get("closed") is True)
    if 200 <= status < 300 and closed:
        held = _forget_instance(payload, code)
        return {
            "released": True,
            "http_status": status,
            "remote_status": "stopped",
            "environment_id": str((held or {}).get("lease_id") or ""),
        }

    remote_code, remote_message = _remote_error(_body)
    if (
        status == 409
        and remote_code == "invalid_state"
        and "already finished" in remote_message.lower()
    ):
        held = _forget_instance(payload, code)
        return {
            "released": True,
            "http_status": status,
            "remote_status": "finished",
            "environment_id": str((held or {}).get("lease_id") or ""),
        }

    # A close response can be lost or report an invalid state after the remote
    # transition completed. Confirm through the authoritative list before
    # claiming success or deleting the local observation.
    items, list_err = _list_challenges(payload)
    item = _find_challenge(items or [], code) if list_err is None else None
    confirmed_stopped = bool(
        item
        and str(item.get("container_status") or "").lower() == "stopped"
        and not _normalise_addresses(item.get("container_addr"))
    )
    if confirmed_stopped:
        held = _forget_instance(payload, code)
        return {
            "released": True,
            "http_status": status,
            "remote_status": "stopped",
            "environment_id": str((held or {}).get("lease_id") or ""),
        }
    if 200 <= status < 300:
        return _err(
            "unknown",
            f"close challenge HTTP {status} was not confirmed (closed != true)",
            status_code=status,
        )
    # A 404 can mean task_not_found rather than an already-closed challenge.
    # Keep the local record and report unknown unless the list confirmed stopped.
    if status == 404:
        return _err(
            "unknown",
            f"close challenge HTTP 404 ({remote_code or 'unclassified'})",
            remote_code=remote_code,
            status_code=status,
        )
    return _http_error(
        "close challenge", status, _body, uncertain_mutation=True
    )


def op_release_instance(payload: dict[str, Any]) -> dict[str, Any]:
    lease = payload.get("lease") or {}
    code = str(lease.get("challenge_key") or "")
    if not code:
        return _err("not_found", "challenge_key required")
    return _close_remote_instance(payload, code)


def op_submit(payload: dict[str, Any]) -> dict[str, Any]:
    request = payload.get("request") or {}
    code = str(request.get("challenge_key") or "")
    flag = str(request.get("flag") or "")
    if not code or not flag:
        return _err("not_found", "challenge_key and flag required")
    token = _token(payload)
    if not token:
        return _err("auth_required", "BENCHMARK_TOKEN credential missing")
    submission_id = str(request.get("idempotency_key") or "").strip()
    if not submission_id:
        return _err("not_found", "submission idempotency_key required")
    journal_key = f"{_connection_id(payload)}::{submission_id}"
    existing = STATE["submissions"].get(journal_key)
    if isinstance(existing, dict) and existing.get("state") == "terminal":
        result = existing.get("result")
        if isinstance(result, dict):
            return {"result": result}

    items, list_err = _list_challenges(payload)
    if list_err:
        return list_err
    item = _find_challenge(items or [], code)
    if item is None:
        return _err("not_found", f"challenge not found: {code}")
    before_correct = max(0, int(item.get("correct_flag_count") or 0))
    entry = {
        "submission_id": submission_id,
        "connection_id": _connection_id(payload),
        "challenge_key": code,
        "digest": hashlib.sha256(flag.encode()).hexdigest(),
        "before_correct_flags": before_correct,
        "state": "dispatching",
        "created_at": time.time(),
        "updated_at": time.time(),
    }
    STATE["submissions"][journal_key] = entry
    _save_state()

    base = _base_url(payload)
    timeout = float(STATE["config"].get("timeout_seconds") or 30)
    status, body = _http(
        "POST",
        f"{base}/openapi/v1/challenges/submit",
        token=token,
        body={"unique_code": code, "flag": flag},
        timeout=timeout,
    )
    if status == 409:
        remote_code, _ = _remote_error(body)
        if remote_code == "duplicate":
            result = {
                "status": "duplicate",
                "detail": body if isinstance(body, dict) else {},
            }
            entry.update({
                "state": "terminal", "result": result,
                "updated_at": time.time(),
            })
            _save_state()
            return {"result": result}
        error = _http_error(
            "submit flag", status, body, uncertain_mutation=True
        )
        entry.update({"state": "unknown", "updated_at": time.time()})
        _save_state()
        return error
    if status == 0 or status >= 400:
        error = _http_error(
            "submit flag", status, body, uncertain_mutation=True
        )
        entry.update({"state": "unknown", "updated_at": time.time()})
        _save_state()
        return error
    correct = bool(isinstance(body, dict) and body.get("correct"))
    result = {
        "status": "correct" if correct else "incorrect",
        "detail": body if isinstance(body, dict) else {},
    }
    entry.update({
        "state": "terminal", "result": result,
        "updated_at": time.time(),
    })
    _save_state()
    if correct and isinstance(body, dict):
        correct_count = int(body.get("correct_flag_count") or 0)
        total_count = int(body.get("total_flag_count") or 0)
        completed = bool(body.get("is_completed")) or (
            total_count > 0 and correct_count >= total_count
        )
        if completed and _held_instance(payload, code) is not None:
            release = _close_remote_instance(payload, code)
            detail = dict(result["detail"])
            detail["instance_release"] = {
                "released": bool(release.get("released")),
                "error": release.get("error"),
            }
            result["detail"] = detail
            entry.update({"result": result, "updated_at": time.time()})
            _save_state()
    return {"result": result}


def op_reconcile_submission(payload: dict[str, Any]) -> dict[str, Any]:
    """Return a persisted verdict or conservatively report it as pending."""
    request = payload.get("request") or {}
    submission_id = str(request.get("submission_id") or "").strip()
    code = str(request.get("challenge_key") or "").strip()
    if not submission_id or not code:
        return _err("not_found", "submission_id and challenge_key required")
    journal_key = f"{_connection_id(payload)}::{submission_id}"
    entry = STATE["submissions"].get(journal_key)
    if not isinstance(entry, dict):
        return {
            "result": {
                "status": "pending",
                "detail": {"reason": "submission_not_in_adapter_journal"},
            }
        }
    result = entry.get("result")
    if entry.get("state") == "terminal" and isinstance(result, dict):
        return {"result": result}

    items, list_err = _list_challenges(payload)
    if list_err:
        return list_err
    item = _find_challenge(items or [], code)
    if item is None:
        return _err("not_found", f"challenge not found: {code}")
    before = max(0, int(entry.get("before_correct_flags") or 0))
    current = max(0, int(item.get("correct_flag_count") or 0))
    later_dispatch_exists = any(
        isinstance(other, dict)
        and str(other.get("connection_id") or "") == _connection_id(payload)
        and str(other.get("challenge_key") or "") == code
        and str(other.get("submission_id") or "") != submission_id
        and float(other.get("created_at") or 0) > float(entry.get("created_at") or 0)
        for other in STATE["submissions"].values()
    )
    if current > before and not later_dispatch_exists:
        result = {
            "status": "correct",
            "detail": {
                "reconciled_from_progress": True,
                "correct_flag_count": current,
            },
        }
        entry.update({
            "state": "terminal", "result": result,
            "updated_at": time.time(),
        })
        _save_state()
        return {"result": result}
    return {
        "result": {
            "status": "pending",
            "detail": {
                "reason": (
                    "later_submission_prevents_attribution"
                    if later_dispatch_exists
                    else "remote_progress_unchanged"
                ),
                "correct_flag_count": current,
            },
        }
    }


OPS = {
    "probe": op_probe,
    "sync_competition": op_sync_competition,
    "fetch_artifact": op_fetch_artifact,
    "acquire_instance": op_acquire_instance,
    "renew_instance": op_renew_instance,
    "release_instance": op_release_instance,
    "submit": op_submit,
    "reconcile_submission": op_reconcile_submission,
}


# ---------------------------------------------------------------------------
# Host protocol
# ---------------------------------------------------------------------------


def _initialize(params: dict[str, Any]) -> dict[str, Any]:
    return {
        "protocol_version": PROTOCOL_VERSION,
        "extension_id": EXTENSION_ID,
        "version": os.environ.get("MUTEKI_EXTENSION_VERSION", "1.2.1"),
    }


def _capabilities(params: dict[str, Any]) -> dict[str, Any]:
    commands = [CMD_PREFIX + name for name in OPS]
    return {
        "provides": [
            {"type": "platform-adapter", "id": PROVIDE_ID, "api_version": 1}
        ],
        "commands": commands,
        "projections": ["summary"],
        "event_schemas": {},
    }


def _config_validate(params: dict[str, Any]) -> dict[str, Any]:
    config = params.get("config") or {}
    errors = []
    allowed = {
        "base_url_default", "timeout_seconds", "auth_header",
        "max_instances",
    }
    unknown = sorted(set(config) - allowed)
    if unknown:
        errors.append(f"unknown config keys: {', '.join(unknown)}")
    timeout = config.get("timeout_seconds", 30)
    if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not math.isfinite(timeout) or timeout < 5:
        errors.append("timeout_seconds must be a finite number >= 5")
    maximum = config.get("max_instances", 3)
    if isinstance(maximum, bool) or not isinstance(maximum, int) or not 1 <= maximum <= 3:
        errors.append("max_instances must be an integer between 1 and 3")
    return {"valid": not errors, "errors": errors}


def _activate(params: dict[str, Any]) -> dict[str, Any]:
    STATE["config"] = dict(params.get("config") or {})
    _load_state()
    STATE["vpn_status"] = {}
    STATE["active"] = True
    return {"activated": True}


def _health(params: dict[str, Any]) -> dict[str, Any]:
    if not STATE["active"]:
        return {"status": "unhealthy", "detail": "not activated"}
    return {
        "status": "healthy",
        "held_instances": len(STATE["instances"]),
        "vpn_connections": {
            key: value.get("state", "unknown")
            for key, value in STATE["vpn_status"].items()
        },
    }


def _command_handle(params: dict[str, Any]) -> dict[str, Any]:
    command_type = str(params.get("command_type") or "")
    payload = params.get("payload") or {}
    if not command_type.startswith(CMD_PREFIX):
        raise ValueError(f"unknown command: {command_type}")
    verb = command_type[len(CMD_PREFIX):]
    handler = OPS.get(verb)
    if handler is None:
        raise ValueError(f"unknown platform-adapter verb: {verb}")
    return handler(payload if isinstance(payload, dict) else {})


def _projection_read(params: dict[str, Any]) -> dict[str, Any]:
    name = str(params.get("name") or "")
    if name != "summary":
        raise ValueError(f"unknown projection: {name}")
    return {
        "held_instances": sorted(STATE["instances"]),
        "run_clocks": {
            key: _clock_for({"connection": {"connection_id": key}})
            for key in sorted(STATE["run_clocks"])
        },
        "vpn_connections": dict(STATE["vpn_status"]),
        "active": STATE["active"],
    }


def _deactivate(params: dict[str, Any]) -> dict[str, Any]:
    STATE["active"] = False
    _save_state()
    return {"deactivated": True}


def _shutdown(params: dict[str, Any]) -> dict[str, Any]:
    _save_state()
    return {"shutdown": True}


HANDLERS = {
    "initialize": _initialize,
    "capabilities/list": _capabilities,
    "config/validate": _config_validate,
    "activate": _activate,
    "health/read": _health,
    "command/handle": _command_handle,
    "projection/read": _projection_read,
    "deactivate": _deactivate,
    "shutdown": _shutdown,
}


def _handle(message: dict[str, Any]) -> None:
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    handler = HANDLERS.get(str(method or ""))
    if handler is None:
        _respond(request_id, error={
            "code": -32601,
            "message": f"method not found: {method}",
        })
        return
    try:
        result = handler(params if isinstance(params, dict) else {})
        _respond(request_id, result=result)
        if method == "shutdown":
            raise SystemExit(0)
    except Exception as exc:
        _respond(request_id, error={"code": -32000, "message": str(exc)})


def main() -> None:
    if len(sys.argv) > 1 and sys.argv[1] != "serve":
        print(f"usage: {sys.argv[0]} serve", file=sys.stderr)
        raise SystemExit(2)
    for line in sys.stdin.buffer:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line.decode("utf-8"))
        except json.JSONDecodeError:
            continue
        if "method" in message:
            _handle(message)


if __name__ == "__main__":
    main()
