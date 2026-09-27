"""Shared HTTP helpers moved from server.py."""
from __future__ import annotations

import json
import os
from typing import Any

from fastapi import HTTPException, Request

def _env_float(name: str, default: float) -> float:
    try:
        v = os.environ.get(name)
        return float(v) if v not in (None, "") else default
    except (TypeError, ValueError):
        return default


def _env_int(name: str, default: int) -> int:
    try:
        v = os.environ.get(name)
        return int(v) if v not in (None, "") else default
    except (TypeError, ValueError):
        return default


def _listening_process(port: int) -> dict[str, Any]:
    """尽力识别本机监听端口的进程，只返回诊断元数据。"""
    try:
        import psutil

        for connection in psutil.net_connections(kind="tcp"):
            address = connection.laddr
            if not address or int(address.port) != int(port):
                continue
            if str(connection.status).upper() != "LISTEN":
                continue
            if connection.pid is None:
                return {"pid": None, "process": "unknown"}
            process = psutil.Process(connection.pid)
            return {
                "pid": connection.pid,
                "process": process.name(),
            }
    except Exception:
        pass
    return {"pid": None, "process": "unknown"}


# Upload guards: a CTF handout is small (a cipher blob, a binary, a pcap). Cap
# per-file size and per-request count so a stray drag-drop can't fill the disk.
# Both are configurable for larger handouts (disk images, big pcaps):
#   MUTEKI_MAX_UPLOAD_MB    (default 25)  — per-file size cap, in MB
#   MUTEKI_MAX_UPLOAD_FILES (default 20)  — max files per request
MAX_UPLOAD_BYTES = max(1, _env_int("MUTEKI_MAX_UPLOAD_MB", 25)) * 1024 * 1024
MAX_UPLOAD_FILES = max(1, _env_int("MUTEKI_MAX_UPLOAD_FILES", 20))


async def _require_dict_body(request: "Request", *, allow_empty: bool = False) -> dict[str, Any]:
    """Parse a JSON request body and require it to be a JSON object.

    Routes used to handle this inconsistently: some did a bare `request.json()`
    (`/hitl` → opaque 500 so the operator couldn't even STOP a run), some caught
    only JSONDecodeError but then did `body.get(...)` on a parsed list (AttributeError
    → 500), and PATCH /api/runs used `if "pinned" in body` which is a valid `in` check
    on a list → silent 200 that swallowed a malformed request. This centralizes it:
    a non-object body (list, string, number, null) is always 400.

    `allow_empty`: some routes legitimately accept NO body (e.g. POST .../workers with
    no engine = "let the coordinator pick"). For those a missing/empty body parses to
    {} instead of 400 — but a present-but-non-object body is still rejected."""
    try:
        body = await request.json()
    except (json.JSONDecodeError, ValueError, UnicodeDecodeError):
        if allow_empty:
            return {}
        raise HTTPException(status_code=400, detail="request body must be a JSON object")
    if not isinstance(body, dict):
        raise HTTPException(status_code=400, detail="request body must be a JSON object")
    return body
