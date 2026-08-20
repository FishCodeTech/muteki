#!/usr/bin/env python3
"""DeepSeek Harness SDK to Muteki NDJSON bridge.

The official Python SDK owns the JSON-RPC runtime process.  This wrapper keeps
that protocol out of ``CliSolver`` and presents the same line-oriented event
surface as the other Worker engines.  Durable tool call/result ids and complete
tool output remain intact so the existing provenance gate can stay unchanged.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
import uuid
from pathlib import Path
from typing import Any


def _emit(payload: dict[str, Any]) -> None:
    print(json.dumps(payload, ensure_ascii=False, separators=(",", ":")), flush=True)


def _content_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts: list[str] = []
    for block in content:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text":
            parts.append(str(block.get("text") or ""))
        elif block.get("type") == "image":
            parts.append("[image]")
        elif block.get("type") == "tool-result":
            parts.append(_content_text(block.get("content")))
    return "".join(parts)


def _usage_dict(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict):
        return {}
    input_tokens = int(value.get("inputTokens") or value.get("input") or 0)
    output_tokens = (
        int(value.get("outputTokens") or value.get("output") or 0)
        + int(value.get("reasoningTokens") or 0)
    )
    cache_read = int(value.get("cacheReadTokens") or value.get("cacheRead") or 0)
    cache_write = int(value.get("cacheWriteTokens") or value.get("cacheWrite") or 0)
    return {
        "input": input_tokens,
        "output": output_tokens,
        "cache_read": cache_read,
        "cache_write": cache_write,
        "total": int(
            value.get("totalTokens")
            or input_tokens + output_tokens + cache_read + cache_write
        ),
        "cost": value.get("cost") if isinstance(value.get("cost"), dict) else {},
    }


def _event_from_notification(notification: Any, root_session: str) -> None:
    method = str(getattr(notification, "method", "") or "")
    payload = getattr(notification, "payload", None)
    if not isinstance(payload, dict):
        return
    if method != "session.event":
        return
    session_id = str(payload.get("sessionId") or root_session)
    event = payload.get("event")
    if not isinstance(event, dict):
        return
    event_type = str(event.get("type") or "")
    data = event.get("data")
    if not isinstance(data, dict):
        data = {}

    if event_type == "tool/call":
        call_id = f"{session_id}:{data.get('callId') or ''}"
        _emit({
            "type": "tool",
            "session_id": session_id,
            "root_session_id": root_session,
            "call_id": call_id,
            "tool": str(data.get("name") or ""),
            "arguments": str(data.get("arguments") or ""),
        })
        return

    if event_type == "tool/result":
        message = data.get("message")
        if not isinstance(message, dict):
            message = {}
        source = message.get("source")
        if not isinstance(source, dict):
            source = {}
        blocks = message.get("content")
        if not isinstance(blocks, list):
            blocks = []
        tool_block = next((
            item for item in blocks
            if isinstance(item, dict) and item.get("type") == "tool-result"
        ), {})
        call_id = f"{session_id}:{source.get('callId') or tool_block.get('toolCallId') or ''}"
        text = _content_text(message.get("content"))
        is_error = bool(
            message.get("isError") or tool_block.get("isError") or data.get("error"))
        if is_error and text:
            text = f"[error] {text}"
        _emit({
            "type": "tool_result",
            "session_id": session_id,
            "root_session_id": root_session,
            "call_id": call_id,
            "tool": str(message.get("toolName") or tool_block.get("toolName") or ""),
            "output": text,
            "is_error": is_error,
        })
        return

    if event_type == "assistant/message" and session_id == root_session:
        message = data.get("message")
        if not isinstance(message, dict):
            message = data
        text = _content_text(message.get("content"))
        if text:
            _emit({"type": "reasoning", "session_id": session_id, "text": text})
        usage = _usage_dict(data.get("usage") or message.get("usage"))
        if usage:
            _emit({"type": "usage", "session_id": session_id, **usage})


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Muteki DeepSeek Harness SDK Worker")
    parser.add_argument("--version", action="store_true")
    parser.add_argument("--session", default="")
    parser.add_argument("--provider", default="deepseek-official")
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--max-tokens", type=int, default=0)
    parser.add_argument("--session-root", default="")
    parser.add_argument("prompt", nargs=argparse.REMAINDER)
    return parser


def main() -> int:
    args = _parser().parse_args()
    if args.version:
        print("muteki-dsh-sdk-worker 0.1.0rc6")
        return 0

    prompt_parts = list(args.prompt)
    if prompt_parts and prompt_parts[0] == "--":
        prompt_parts = prompt_parts[1:]
    prompt = " ".join(prompt_parts)
    if not prompt and not sys.stdin.isatty():
        prompt = sys.stdin.read()
    if not prompt.strip():
        print("DeepSeek Harness Worker requires a prompt", file=sys.stderr)
        return 2

    session_id = str(args.session or f"session-{uuid.uuid4().hex}")
    session_root = Path(
        args.session_root
        or os.environ.get("DSH_SESSION_ROOT")
        or str(Path(tempfile.gettempdir()) / "muteki-dsh-sessions")
    ).resolve()
    session_root.mkdir(parents=True, exist_ok=True)
    runtime_cwd = Path(os.environ.get("DSH_CWD") or Path.cwd()).resolve()
    os.environ.setdefault("DSH_TELEMETRY_DISABLED", "1")
    _emit({"type": "session", "id": session_id})

    try:
        from deepseek_harness import DeepSeekHarness

        options: dict[str, Any] = {
            "provider": str(args.provider),
            "model": str(args.model),
            "cwd": str(runtime_cwd),
            "runtime_cwd": str(runtime_cwd),
            "session_root": str(session_root),
        }
        if args.max_tokens > 0:
            options["max_tokens"] = args.max_tokens
        with DeepSeekHarness(**options) as harness:
            result = harness.run(
                prompt,
                session_id=session_id,
                on_notification=lambda item: _event_from_notification(item, session_id),
            )
        _emit({
            "type": "result",
            "session_id": result.session_id,
            "text": result.final_response,
            "finish_reason": result.finish_reason,
        })
        return 0 if result.finish_reason not in {"error", "aborted"} else 1
    except Exception as exc:  # noqa: BLE001 - structured process boundary
        _emit({
            "type": "error",
            "session_id": session_id,
            "error": f"{type(exc).__name__}: {exc}",
        })
        print(f"DeepSeek Harness Worker failed: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
