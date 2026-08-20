"""ACP bridge used by Muteki's offline Worker mode.

ACP exposes tool permission requests to the client and lets the client provide an
empty MCP server list.  This bridge approves local tools for the current
invocation, rejects native search/fetch tools, and translates ACP updates to the
JSONL shape consumed by Muteki's offline drivers.

The bridge is deliberately task-local.  It does not write Cursor's user or
project configuration and it does not change the online ``cursor-agent -p`` path.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import threading
from typing import Any, NoReturn


_BLOCKED_TOOL_KINDS = {"search", "fetch"}
_EFFORT_SUFFIX = re.compile(
    r"^(?P<base>.*?)(?:-(?P<effort>low|medium|high|xhigh|max))?(?P<fast>-fast)?$"
)


class AcpError(RuntimeError):
    """The ACP process returned an invalid or unsuccessful response."""


def _write_json(stream: Any, payload: dict[str, Any]) -> None:
    stream.write(json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n")
    stream.flush()


def _tool_output_text(raw: Any) -> str:
    if isinstance(raw, str):
        return raw
    if isinstance(raw, dict):
        chunks = [str(raw.get(key) or "") for key in ("stdout", "stderr")]
        text = "\n".join(chunk for chunk in chunks if chunk)
        if text:
            return text
        if raw.get("rejected"):
            return str(raw.get("reason") or "permission rejected")
    if raw is None:
        return ""
    return json.dumps(raw, ensure_ascii=False, separators=(",", ":"))


def _model_options(session_result: dict[str, Any]) -> list[dict[str, str]]:
    models = session_result.get("models") or {}
    rows = models.get("availableModels") if isinstance(models, dict) else None
    if not isinstance(rows, list):
        rows = []
        for option in session_result.get("configOptions") or []:
            if isinstance(option, dict) and option.get("id") == "model":
                rows = option.get("options") or []
                break
    out: list[dict[str, str]] = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        value = str(row.get("modelId") or row.get("value") or "").strip()
        if value:
            out.append({"value": value, "name": str(row.get("name") or "").strip()})
    return out


def _resolve_model(
    requested: str, session_result: dict[str, Any], *, agent_label: str,
) -> str:
    requested = requested.strip()
    if not requested:
        return ""
    options = _model_options(session_result)
    for row in options:
        if requested in {row["value"], row["name"]}:
            return row["value"]

    if agent_label != "cursor":
        available = ", ".join(row["name"] or row["value"] for row in options)
        raise AcpError(
            f"{agent_label} ACP model {requested!r} is unavailable"
            + (f"; available models: {available}" if available else "")
        )

    selector = requested.removeprefix("cursor-")
    match = _EFFORT_SUFFIX.fullmatch(selector)
    base = match.group("base") if match else selector
    effort = match.group("effort") if match else None
    wants_fast = bool(match and match.group("fast"))
    candidates = [row for row in options if row["name"] == base]
    if effort:
        exact = [
            row for row in candidates
            if f"effort={effort}" in row["value"]
            or f"reasoning={effort}" in row["value"]
        ]
        if exact:
            candidates = exact
    if wants_fast:
        fast = [row for row in candidates if "fast=true" in row["value"]]
        if fast:
            candidates = fast
    if candidates:
        return candidates[0]["value"]
    available = ", ".join(row["name"] or row["value"] for row in options)
    raise AcpError(
        f"Cursor ACP model {requested!r} is unavailable"
        + (f"; available models: {available}" if available else "")
    )


class OfflineAcpClient:
    def __init__(
        self, agent_bin: str, *, agent_args: list[str], agent_label: str,
        agent_mode: str,
    ) -> None:
        command = [agent_bin, *agent_args]
        command.append(agent_mode)
        self.agent_label = agent_label
        self.proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        if self.proc.stdin is None or self.proc.stdout is None or self.proc.stderr is None:
            raise AcpError(f"failed to open {agent_label} ACP pipes")
        self._next_id = 1
        self._assistant_chunks: list[str] = []
        self._stderr_thread = threading.Thread(target=self._forward_stderr, daemon=True)
        self._stderr_thread.start()

    def _forward_stderr(self) -> None:
        assert self.proc.stderr is not None
        for line in self.proc.stderr:
            sys.stderr.write(line)
            sys.stderr.flush()

    def _send(self, payload: dict[str, Any]) -> None:
        assert self.proc.stdin is not None
        _write_json(self.proc.stdin, payload)

    def _reply_permission(self, message: dict[str, Any]) -> None:
        params = message.get("params") or {}
        tool = params.get("toolCall") or {}
        kind = str(tool.get("kind") or "").lower()
        title = str(tool.get("title") or "").lower()
        blocked = kind in _BLOCKED_TOOL_KINDS or "web search" in title or "fetch url" in title
        options = [row for row in params.get("options") or [] if isinstance(row, dict)]
        wanted = "reject" if blocked else "allow_once"
        selected = next(
            (row for row in options if str(row.get("kind") or "").lower() == wanted),
            None,
        )
        if selected is None and not blocked:
            selected = next(
                (row for row in options if str(row.get("kind") or "").lower().startswith("allow")),
                None,
            )
        if selected is None:
            selected = next(
                (row for row in options if str(row.get("kind") or "").lower().startswith("reject")),
                None,
            )
        if selected is None:
            self._send({
                "jsonrpc": "2.0",
                "id": message.get("id"),
                "error": {"code": -32603, "message": "no compatible permission option"},
            })
            return
        self._send({
            "jsonrpc": "2.0",
            "id": message.get("id"),
            "result": {
                "outcome": {
                    "outcome": "selected",
                    "optionId": selected.get("optionId"),
                }
            },
        })

    def _handle_agent_request(self, message: dict[str, Any]) -> None:
        if message.get("method") == "session/request_permission":
            self._reply_permission(message)
            return
        self._send({
            "jsonrpc": "2.0",
            "id": message.get("id"),
            "error": {"code": -32601, "message": "method not supported by Muteki ACP client"},
        })

    def _emit_update(self, message: dict[str, Any]) -> None:
        params = message.get("params") or {}
        update = params.get("update") or {}
        update_type = str(update.get("sessionUpdate") or "")
        session_id = str(params.get("sessionId") or "")
        content = update.get("content") or {}
        text = str(content.get("text") or "") if isinstance(content, dict) else ""
        if update_type in {"agent_message_chunk", "agent_thought_chunk"} and text:
            if update_type == "agent_message_chunk":
                self._assistant_chunks.append(text)
            _write_json(sys.stdout, {
                "type": "assistant",
                "session_id": session_id,
                "message": {"content": [{"type": "text", "text": text}]},
            })
            return
        if update_type == "tool_call":
            raw_input = update.get("rawInput") or {}
            _write_json(sys.stdout, {
                "type": "tool_call",
                "subtype": "started",
                "session_id": session_id,
                "call_id": str(update.get("toolCallId") or ""),
                "tool_call": {"function": {
                    "name": str(update.get("kind") or "tool"),
                    "arguments": json.dumps(raw_input, ensure_ascii=False),
                }},
            })
            return
        if update_type == "tool_call_update" and update.get("status") == "completed":
            raw_output = update.get("rawOutput")
            _write_json(sys.stdout, {
                "type": "tool_call",
                "subtype": "completed",
                "session_id": session_id,
                "call_id": str(update.get("toolCallId") or ""),
                "tool_call": {"function": {
                    "name": "tool",
                    "arguments": "",
                    "result": {"success": {"content": _tool_output_text(raw_output)}},
                }},
            })

    def request(
        self, method: str, params: dict[str, Any], *, emit_updates: bool = False,
    ) -> dict[str, Any]:
        request_id = self._next_id
        self._next_id += 1
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        assert self.proc.stdout is not None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                code = self.proc.poll()
                raise AcpError(
                    f"{self.agent_label} ACP exited before responding (exit {code})")
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                sys.stderr.write(
                    f"{self.agent_label} ACP emitted invalid JSON: {line[:500]}\n")
                sys.stderr.flush()
                continue
            if "method" in message and "id" in message:
                self._handle_agent_request(message)
                continue
            if message.get("method") == "session/update":
                if emit_updates:
                    self._emit_update(message)
                continue
            if message.get("id") != request_id:
                continue
            if message.get("error"):
                raise AcpError(
                    f"{self.agent_label} ACP {method} failed: {message['error']}")
            result = message.get("result")
            return result if isinstance(result, dict) else {}

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=3)
        self._stderr_thread.join(timeout=1)

    @property
    def assistant_text(self) -> str:
        return "".join(self._assistant_chunks)


def _die(message: str) -> NoReturn:
    sys.stderr.write(message.rstrip() + "\n")
    raise SystemExit(1)


def main() -> int:
    parser = argparse.ArgumentParser(description="Run an agent with offline ACP permissions")
    parser.add_argument("--agent-bin", required=True)
    parser.add_argument("--agent-label", default="agent")
    parser.add_argument("--agent-mode", default="acp")
    parser.add_argument("--agent-arg", action="append", default=[])
    parser.add_argument("--endpoint", default="")
    parser.add_argument("--model", default="")
    parser.add_argument("--provider", default="")
    parser.add_argument("--thinking", default="")
    parser.add_argument("--resume", default="")
    parser.add_argument("prompt")
    args = parser.parse_args()

    del args.provider
    agent_args = list(args.agent_arg)
    if args.endpoint:
        agent_args += ["--endpoint", args.endpoint]
    client = OfflineAcpClient(
        args.agent_bin,
        agent_args=agent_args,
        agent_label=args.agent_label,
        agent_mode=args.agent_mode,
    )
    try:
        client.request("initialize", {
            "protocolVersion": 1,
            "clientInfo": {"name": "muteki", "title": "Muteki", "version": "1"},
            "clientCapabilities": {},
        })
        session_method = "session/load" if args.resume else "session/new"
        session_params: dict[str, Any] = {"cwd": os.getcwd(), "mcpServers": []}
        if args.resume:
            session_params["sessionId"] = args.resume
        session_result = client.request(session_method, session_params)
        session_id = args.resume or str(session_result.get("sessionId") or "")
        if not session_id:
            raise AcpError(f"{args.agent_label} ACP did not return a session id")
        _write_json(sys.stdout, {
            "type": "system", "subtype": "init", "session_id": session_id,
        })
        model = _resolve_model(
            args.model, session_result, agent_label=args.agent_label)
        if model:
            client.request("session/set_config_option", {
                "sessionId": session_id,
                "configId": "model",
                "value": model,
            })
        if args.thinking:
            thinking = "off" if args.thinking == "none" else args.thinking
            client.request("session/set_config_option", {
                "sessionId": session_id,
                "configId": "thinking",
                "value": thinking,
            })
        client.request("session/prompt", {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": args.prompt}],
        }, emit_updates=True)
        _write_json(sys.stdout, {
            "type": "result",
            "subtype": "success",
            "result": client.assistant_text,
            "session_id": session_id,
        })
        return 0
    except (AcpError, OSError) as exc:
        _die(str(exc))
    finally:
        client.close()


if __name__ == "__main__":
    raise SystemExit(main())
