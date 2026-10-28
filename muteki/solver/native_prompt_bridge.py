"""Deliver a Worker prompt to Kimi's existing local-server adapter via stdin."""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import tomllib
from pathlib import Path

# This bridge is invoked by absolute path from the Worker's working directory.
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from muteki.external_agents.kimi import KimiLocalServerAdapter
from muteki.platform.contracts.external_agents import (
    AgentEventType, MessageInput, SessionOptions, SessionStart,
)


def emit(event: dict) -> None:
    print(json.dumps(event, ensure_ascii=False), flush=True)


def selected_model(model: str | None) -> str | None:
    if os.environ.get("KIMI_MODEL_NAME", "").strip():
        return "__kimi_env_model__"
    if model:
        return model
    home = Path(os.environ.get("KIMI_CODE_HOME") or Path.home() / ".kimi-code")
    config = home / "config.toml"
    if not config.is_file():
        return None
    with config.open("rb") as source:
        return tomllib.load(source).get("default_model") or None


async def run(
    binary: str, session_id: str | None, prompt: str,
    model: str | None = None, disabled_tools: list[str] | None = None,
) -> None:
    # The adapter needs the effective Kimi home to resolve the server token.
    # The child already inherits the rest of the runtime environment.
    home = os.environ.get("KIMI_CODE_HOME", "")
    adapter = KimiLocalServerAdapter(
        binary=binary, extra_env={"KIMI_CODE_HOME": home} if home else {},
    )
    # KIMI_MODEL_NAME defines the provider's model ID, not a Kimi alias.
    # Kimi's environment overlay registers that ID under this native alias.
    session = await adapter.start(SessionStart(
        resume_handle=session_id,
        model=selected_model(model),
        options=SessionOptions(
            cwd=os.getcwd(), disabled_tools=list(disabled_tools or [])),
    ))
    try:
        emit({"role": "meta", "type": "session.resume_hint",
              "session_id": session.external_session_id})
        completed = False
        async for event in adapter.send(session, MessageInput(text=prompt)):
            kind, payload = event.event_type, event.payload
            if kind == AgentEventType.MESSAGE_COMPLETED:
                emit({"role": "assistant", "content": payload.get("text", "")})
            elif kind == AgentEventType.REASONING_SUMMARY:
                emit({"role": "meta", "type": "thinking.delta",
                      "content": payload.get("text", "")})
            elif kind == AgentEventType.TOOL_STARTED:
                emit({"role": "assistant", "tool_calls": [{
                    "id": payload.get("tool_call_id"), "function": {
                        "name": payload.get("name"), "arguments": payload.get("input")}}]})
            elif kind == AgentEventType.TOOL_COMPLETED:
                output = payload.get("output")
                emit({"role": "tool", "tool_call_id": payload.get("tool_call_id"),
                      "content": output if isinstance(output, str) else json.dumps(output, ensure_ascii=False)})
            elif kind in {AgentEventType.TURN_FAILED, AgentEventType.RUNTIME_ERROR}:
                emit({"type": "error", "error": payload.get("error", payload)})
                raise RuntimeError("Kimi structured runtime reported a failure")
            elif kind in {AgentEventType.APPROVAL_REQUESTED, AgentEventType.USER_INPUT_REQUESTED}:
                raise RuntimeError("Kimi prompt transport cannot answer an interactive request")
            elif kind == AgentEventType.TURN_COMPLETED:
                completed = True
        if not completed:
            raise RuntimeError("Kimi structured runtime ended without a completed turn")
    finally:
        await adapter.close(session)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--session")
    parser.add_argument("--model")
    parser.add_argument("--disable-tool", action="append", default=[])
    args = parser.parse_args()
    try:
        prompt = sys.stdin.buffer.read().decode("utf-8")
        asyncio.run(run(
            args.binary, args.session, prompt, args.model, args.disable_tool))
    except Exception as exc:
        print(f"Kimi prompt bridge failed: {exc}", file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
