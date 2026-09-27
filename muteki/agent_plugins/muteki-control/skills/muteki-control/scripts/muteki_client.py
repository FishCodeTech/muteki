#!/usr/bin/env python3
"""Skills-only fallback client for the Muteki capability gateway."""

from __future__ import annotations

import argparse
import json
import os
import urllib.error
import urllib.request
from typing import Any, Optional, Sequence


def _call(method: str, params: dict[str, Any]) -> dict[str, Any]:
    endpoint = os.environ.get("MUTEKI_CAPABILITY_ENDPOINT", "").strip()
    token = os.environ.get("MUTEKI_CAPABILITY_TOKEN", "").strip()
    if not endpoint or not token:
        raise RuntimeError(
            "MUTEKI_CAPABILITY_ENDPOINT and MUTEKI_CAPABILITY_TOKEN must be "
            "provided by the agent session"
        )
    request = urllib.request.Request(
        endpoint,
        data=json.dumps({
            "jsonrpc": "2.0",
            "id": "agent-skill",
            "method": method,
            "params": params,
        }).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        return {
            "error": {
                "code": exc.code,
                "message": exc.read().decode("utf-8", "replace"),
            }
        }
    if not isinstance(payload, dict):
        return {"error": {"code": -32603, "message": "non-object response"}}
    return payload


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="muteki-client")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("describe")
    call = sub.add_parser("call")
    call.add_argument("tool_name")
    call.add_argument("--args", default="{}")
    args = parser.parse_args(argv)
    try:
        if args.command == "describe":
            payload = _call("muteki.describe", {})
        else:
            arguments = json.loads(args.args)
            if not isinstance(arguments, dict):
                raise ValueError("--args must be a JSON object")
            payload = _call("muteki.invoke", {
                "tool_name": args.tool_name,
                "arguments": arguments,
            })
    except (RuntimeError, ValueError, json.JSONDecodeError) as exc:
        print(json.dumps({"error": {"message": str(exc)}}, ensure_ascii=False))
        return 2
    print(json.dumps(payload, ensure_ascii=False, indent=2))
    return 0 if "error" not in payload else 1


if __name__ == "__main__":
    raise SystemExit(main())
