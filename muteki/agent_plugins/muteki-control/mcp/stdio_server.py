#!/usr/bin/env python3
"""Portable stdio MCP bridge for the Muteki Agent Plugin.

This file is intentionally self-contained and uses only the Python standard
library. It converts the MCP tools surface expected by Agent Plugins clients to
Muteki's authenticated HTTP JSON-RPC capability gateway. Tool discovery remains
dynamic and binding-scoped; this bridge does not maintain a second tool catalog.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Optional, Sequence


PROTOCOL_VERSIONS = (
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
)
METHOD_DESCRIBE = "muteki.describe"
METHOD_INVOKE = "muteki.invoke"
# The default bounded wait is 30 s. Leave time for the response and transport.
GATEWAY_TIMEOUT_SECONDS = 45.0


class BridgeError(RuntimeError):
    """A connection, configuration, or upstream JSON-RPC failure."""


def _load_connection(path: Path) -> tuple[str, str]:
    endpoint = os.environ.get("MUTEKI_CAPABILITY_ENDPOINT", "").strip()
    token = os.environ.get("MUTEKI_CAPABILITY_TOKEN", "").strip()
    if path.is_file():
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise BridgeError(f"invalid Muteki connection file {path}: {exc}") from exc
        if not isinstance(data, dict):
            raise BridgeError(f"Muteki connection file must contain a JSON object: {path}")
        endpoint = str(data.get("endpoint") or endpoint).strip()
        token = str(data.get("bearer_token") or token).strip()
    if not endpoint or not token:
        raise BridgeError(
            "Muteki connection is not configured. The Agent Plugins client must "
            "materialize endpoint and bearer_token in ${PLUGIN_DATA}/connection.json."
        )
    return endpoint, token


def _gateway_call(
    config_path: Path,
    method: str,
    params: Optional[dict[str, Any]] = None,
) -> Any:
    endpoint, token = _load_connection(config_path)
    request = urllib.request.Request(
        endpoint,
        data=json.dumps({
            "jsonrpc": "2.0",
            "id": "agent-plugin",
            "method": method,
            "params": params or {},
        }).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=GATEWAY_TIMEOUT_SECONDS) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", "replace")
        raise BridgeError(f"Muteki gateway returned HTTP {exc.code}: {detail}") from exc
    except (urllib.error.URLError, TimeoutError, OSError, json.JSONDecodeError) as exc:
        raise BridgeError(f"Muteki gateway request failed: {exc}") from exc
    if not isinstance(payload, dict):
        raise BridgeError("Muteki gateway returned a non-object JSON-RPC response")
    error = payload.get("error")
    if isinstance(error, dict):
        raise BridgeError(str(error.get("message") or error))
    return payload.get("result")


def _response(request_id: Any, result: Any) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def _error(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": code, "message": message},
    }


def _tool_rows(descriptor: Any) -> list[dict[str, Any]]:
    if not isinstance(descriptor, dict):
        raise BridgeError("Muteki describe result is not an object")
    tools = descriptor.get("tools") or []
    if not isinstance(tools, list):
        raise BridgeError("Muteki describe result tools field is not an array")
    rows: list[dict[str, Any]] = []
    for raw in tools:
        if not isinstance(raw, dict) or not str(raw.get("name") or "").strip():
            continue
        schema = raw.get("input_schema") or {"type": "object"}
        rows.append({
            "name": str(raw["name"]),
            "description": str(raw.get("description") or ""),
            "inputSchema": schema if isinstance(schema, dict) else {"type": "object"},
        })
    return rows


def _handle(request: dict[str, Any], config_path: Path) -> Optional[dict[str, Any]]:
    request_id = request.get("id")
    method = request.get("method")
    params = request.get("params") or {}
    if "id" not in request:
        return None
    if method == "initialize":
        requested = str(params.get("protocolVersion") or "") if isinstance(params, dict) else ""
        version = requested if requested in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0]
        return _response(request_id, {
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "muteki-control", "version": "1.0.0"},
        })
    if method == "ping":
        return _response(request_id, {})
    if method == "tools/list":
        descriptor = _gateway_call(config_path, METHOD_DESCRIBE)
        return _response(request_id, {"tools": _tool_rows(descriptor)})
    if method == "tools/call":
        if not isinstance(params, dict):
            return _error(request_id, -32602, "tools/call params must be an object")
        name = str(params.get("name") or "").strip()
        arguments = params.get("arguments") or {}
        if not name or not isinstance(arguments, dict):
            return _error(request_id, -32602, "tools/call requires name and object arguments")
        payload = _gateway_call(config_path, METHOD_INVOKE, {
            "tool_name": name,
            "arguments": arguments,
        })
        if not isinstance(payload, dict):
            payload = {"ok": True, "result": payload}
        images = payload.pop("images", None) or []
        return _response(request_id, {
            "content": [{
                "type": "text",
                "text": json.dumps(payload, ensure_ascii=False),
            }, *(image for image in images if isinstance(image, dict))],
            "structuredContent": payload,
            "isError": not bool(payload.get("ok", True)),
        })
    return _error(request_id, -32601, f"method not found: {method!r}")


def serve(config_path: Path) -> int:
    for line in sys.stdin:
        if not line.strip():
            continue
        request: Any = None
        try:
            request = json.loads(line)
            if not isinstance(request, dict) or request.get("jsonrpc") != "2.0":
                response = _error(None, -32600, "invalid JSON-RPC request")
            else:
                response = _handle(request, config_path)
        except json.JSONDecodeError:
            response = _error(None, -32700, "parse error")
        except BridgeError as exc:
            response = _error(
                request.get("id") if isinstance(request, dict) else None,
                -32001,
                str(exc),
            )
        except Exception as exc:  # keep the stdio server alive for the next request
            response = _error(
                request.get("id") if isinstance(request, dict) else None,
                -32603,
                f"internal error: {type(exc).__name__}: {exc}",
            )
        if response is not None:
            sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
            sys.stdout.flush()
    return 0


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="muteki-control-mcp")
    parser.add_argument("--config", required=True, type=Path)
    args = parser.parse_args(argv)
    return serve(args.config.expanduser())


if __name__ == "__main__":
    raise SystemExit(main())
