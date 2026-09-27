#!/usr/bin/env python3
"""hello Agent Plugin 的 Muteki 客户端扩展子进程实现。

只依赖 Python 标准库实现任务书 11.2 的版本化 JSON-RPC over stdio 协议，
证明第三方扩展不需要依赖 muteki 包本体：

- Host → 扩展：initialize / capabilities/list / config/validate / activate /
  health/read / command/handle / projection/read / deactivate / shutdown；
- 扩展 → Host：event/propose（greet 时向宿主提案
  ``ext.org.muteki.examples.hello.greeted`` 事件，由宿主经
  MutekiCommandAPI 校验命名空间与 schema 后落 Domain Event）。

行为细节（供验收测试使用）：

- config.greeting：问候语（默认 "hello"）；config.fail_health=true 时
  health/read 报告 unhealthy（用于制造健康检查失败、触发自动回滚）；
- 每次 greet 把计数写入 ``$MUTEKI_EXTENSION_STATE_DIR/state.json``
  （升级时由宿主迁移状态目录）；
- activate 时把注入的 secret 值打印到 stderr（模拟一次泄漏），宿主归档
  日志时必须脱敏。
"""

import json
import os
import socket
import subprocess
import sys

PROTOCOL_VERSION = 1
EXTENSION_ID = os.environ.get("MUTEKI_EXTENSION_ID", "org.muteki.examples.hello")
GREETED_EVENT = f"ext.{EXTENSION_ID}.greeted"
GREET_COMMAND = f"ext.{EXTENSION_ID}.greet"
SECURITY_PROBE_COMMAND = f"ext.{EXTENSION_ID}.security_probe"

STATE = {
    "config": {},
    "active": False,
    "greetings": 0,
}

_next_host_id = 0


# ---------------------------------------------------------------------------
# 协议帧（换行分隔 JSON-RPC 2.0）
# ---------------------------------------------------------------------------


def _write(message):
    sys.stdout.buffer.write(
        json.dumps(message, ensure_ascii=False).encode("utf-8") + b"\n"
    )
    sys.stdout.buffer.flush()


def _respond(request_id, result=None, error=None):
    message = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        message["error"] = error
    else:
        message["result"] = result if result is not None else {}
    _write(message)


def _request_host(method, params):
    """向宿主发请求并同步等待响应；等待期间照常处理宿主新请求。"""
    global _next_host_id
    _next_host_id += 1
    request_id = _next_host_id
    _write({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": {**params, "protocol_version": PROTOCOL_VERSION},
    })
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise RuntimeError("host closed stdin")
        message = json.loads(line.decode("utf-8"))
        if "method" in message:
            _handle(message)  # 宿主在等待期间发来的请求先处理
            continue
        if message.get("id") != request_id:
            continue
        if "error" in message:
            err = message["error"]
            raise RuntimeError(f"host rejected {method}: {err.get('message')}")
        return message.get("result")


# ---------------------------------------------------------------------------
# 状态持久化（升级时由宿主迁移目录）
# ---------------------------------------------------------------------------


def _state_path():
    root = os.environ.get("MUTEKI_EXTENSION_STATE_DIR", ".")
    return os.path.join(root, "state.json")


def _load_state():
    try:
        with open(_state_path(), encoding="utf-8") as handle:
            STATE["greetings"] = int(json.load(handle).get("greetings", 0))
    except (OSError, ValueError):
        pass


def _save_state():
    with open(_state_path(), "w", encoding="utf-8") as handle:
        json.dump({"greetings": STATE["greetings"]}, handle)


# ---------------------------------------------------------------------------
# 方法实现
# ---------------------------------------------------------------------------


def _initialize(params):
    return {
        "protocol_version": PROTOCOL_VERSION,
        "extension_id": EXTENSION_ID,
        "version": os.environ.get("MUTEKI_EXTENSION_VERSION", "dev"),
    }


def _capabilities(params):
    return {
        "provides": [
            {"type": "domain-module", "id": EXTENSION_ID, "api_version": 1}
        ],
        "commands": [GREET_COMMAND, SECURITY_PROBE_COMMAND],
        "projections": ["summary"],
        "event_schemas": {
            GREETED_EVENT: {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "greeting": {"type": "string"},
                },
                "required": ["name"],
                "additionalProperties": False,
            }
        },
    }


def _config_validate(params):
    config = params.get("config") or {}
    errors = []
    if "greeting" in config and not isinstance(config["greeting"], str):
        errors.append("greeting must be a string")
    if "fail_health" in config and not isinstance(config["fail_health"], bool):
        errors.append("fail_health must be a boolean")
    unknown = sorted(set(config) - {"greeting", "fail_health"})
    if unknown:
        errors.append(f"unknown config keys: {', '.join(unknown)}")
    return {"valid": not errors, "errors": errors}


def _activate(params):
    config = params.get("config") or {}
    STATE["config"] = config
    STATE["active"] = True
    _load_state()
    # 模拟一次 secret 泄漏到 stderr：宿主日志归档必须把它脱敏成 ***
    token = os.environ.get("MUTEKI_SECRET_ENV_HELLO_TOKEN")
    if token:
        print(f"hello extension activated, token={token}", file=sys.stderr)
    return {"activated": True}


def _health(params):
    if not STATE["active"]:
        return {"status": "unhealthy", "detail": "not activated"}
    if STATE["config"].get("fail_health"):
        return {"status": "unhealthy", "detail": "config fail_health=true"}
    return {"status": "healthy", "greetings": STATE["greetings"]}


def _command_handle(params):
    command_type = params.get("command_type") or ""
    payload = params.get("payload") or {}
    if command_type == SECURITY_PROBE_COMMAND:
        operation = str(payload.get("operation") or "")
        try:
            if operation == "file":
                with open(str(payload.get("path") or "/etc/hosts"), encoding="utf-8") as handle:
                    handle.read(1)
            elif operation == "network":
                with socket.create_connection(
                    (str(payload.get("host") or "127.0.0.1"), int(payload.get("port") or 9)),
                    timeout=0.5,
                ):
                    pass
            elif operation == "subprocess":
                subprocess.run(["/usr/bin/true"], check=True, timeout=2)
            else:
                raise ValueError("operation must be file, network or subprocess")
        except Exception as exc:
            return {
                "operation": operation,
                "allowed": False,
                "error": f"{type(exc).__name__}: {exc}",
                "enforcement": os.environ.get("MUTEKI_PERMISSION_ENFORCEMENT", "unknown"),
            }
        return {
            "operation": operation,
            "allowed": True,
            "enforcement": os.environ.get("MUTEKI_PERMISSION_ENFORCEMENT", "unknown"),
        }
    if command_type != GREET_COMMAND:
        raise ValueError(f"unknown command: {command_type}")
    name = str(payload.get("name") or "").strip()
    if not name:
        raise ValueError("payload.name is required")
    greeting = str(STATE["config"].get("greeting") or "hello")
    # 事件提案：经宿主 MutekiCommandAPI 校验命名空间与 schema 后落库
    proposal = _request_host("event/propose", {
        "event_type": GREETED_EVENT,
        "payload": {"name": name, "greeting": greeting},
    })
    STATE["greetings"] += 1
    _save_state()
    return {
        "message": f"{greeting}, {name}!",
        "greetings": STATE["greetings"],
        "event_accepted": bool((proposal or {}).get("accepted")),
    }


def _projection_read(params):
    return {
        "name": params.get("name") or "summary",
        "data": {
            "active": STATE["active"],
            "greetings": STATE["greetings"],
            "greeting": STATE["config"].get("greeting", "hello"),
        },
    }


def _deactivate(params):
    STATE["active"] = False
    return {"deactivated": True}


def _shutdown(params):
    return {"bye": True}


_HANDLERS = {
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


def _handle(message):
    """处理一条宿主请求；shutdown 处理后由主循环退出。"""
    method = message.get("method") or ""
    request_id = message.get("id")
    params = message.get("params") or {}
    handler = _HANDLERS.get(method)
    if handler is None:
        _respond(request_id, error={
            "code": -32601, "message": f"unknown method: {method}"})
        return
    if int(params.get("protocol_version", -1)) != PROTOCOL_VERSION:
        _respond(request_id, error={
            "code": -32602,
            "message": f"protocol major mismatch: {params.get('protocol_version')}"})
        return
    try:
        _respond(request_id, result=handler(params))
    except Exception as exc:  # noqa: BLE001 扩展侧把异常归一化为 RPC error
        _respond(request_id, error={"code": -32603, "message": str(exc)})


def main():
    if len(sys.argv) > 1 and sys.argv[1] != "serve":
        print(f"usage: {sys.argv[0]} serve", file=sys.stderr)
        return 2
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return 0
        try:
            message = json.loads(line.decode("utf-8"))
        except ValueError:
            continue
        if "method" not in message:
            continue
        method = message.get("method")
        _handle(message)
        if method == "shutdown":
            return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
