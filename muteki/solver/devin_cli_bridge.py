"""Run one Devin ACP turn and normalize its live events for ``CliSolver``."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from typing import Any, Optional

from muteki.external_agents.acp import (
    AcpError,
    AcpTransport,
    check_response,
    normalize_session_update,
)
from muteki.platform.contracts.external_agents import AgentEventType


def _emit(event: dict[str, Any]) -> None:
    print(json.dumps(event, ensure_ascii=False), flush=True)


def _permission_option(params: dict[str, Any]) -> Optional[str]:
    options = [
        item for item in (params.get("options") or [])
        if isinstance(item, dict)
    ]
    for kind in ("allow_always", "allow_once"):
        for option in options:
            if option.get("kind") == kind and option.get("optionId") is not None:
                return str(option["optionId"])
    return None


def _usage(value: Any) -> dict[str, int | float]:
    if not isinstance(value, dict):
        return {}
    meta = value.get("_meta") if isinstance(value.get("_meta"), dict) else {}
    aliases = {
        "input_tokens": ("inputTokens", "input_tokens", "cognition.ai/inputTokens"),
        "output_tokens": ("outputTokens", "output_tokens", "cognition.ai/outputTokens"),
        "cache_read_tokens": (
            "cachedReadTokens", "cacheReadTokens", "cache_read_tokens",
            "cognition.ai/cachedReadTokens",
        ),
        "cache_write_tokens": (
            "cacheWriteTokens", "cache_write_tokens",
            "cognition.ai/cacheWriteTokens",
        ),
        "reasoning_tokens": (
            "reasoningTokens", "reasoning_tokens",
            "cognition.ai/reasoningTokens",
        ),
    }
    normalized: dict[str, int | float] = {}
    for target, names in aliases.items():
        for name in names:
            raw = value.get(name, meta.get(name))
            if isinstance(raw, int) and not isinstance(raw, bool) and raw >= 0:
                normalized[target] = raw
                break
    for name in ("costUsd", "cost_usd"):
        raw_cost = value.get(name, meta.get(name))
        if (isinstance(raw_cost, (int, float))
                and not isinstance(raw_cost, bool) and raw_cost >= 0):
            normalized["cost_usd"] = float(raw_cost)
            break
    return normalized


def _merge_output(previous: str, incoming: str) -> str:
    if not incoming or incoming == previous or previous.startswith(incoming):
        return previous
    if not previous or incoming.startswith(previous):
        return incoming
    return previous + ("\n" if not previous.endswith("\n") else "") + incoming


class _EventBridge:
    def __init__(self) -> None:
        self.message_parts: list[str] = []
        self.tool_outputs: dict[str, str] = {}
        self.usage: dict[str, int | float] = {}

    def on_update(
        self, _session_id: str, update: dict[str, Any], replay: bool,
    ) -> None:
        if replay:
            return
        self.usage.update(_usage(update))
        for event_type, _native_kind, payload in normalize_session_update(update):
            if event_type is AgentEventType.MESSAGE_DELTA:
                text = str(payload.get("text") or "")
                if not text:
                    continue
                commentary = payload.get("phase") == "commentary"
                if not commentary:
                    self.message_parts.append(text)
                _emit({
                    "type": "reasoning",
                    "text": text,
                    "thinking": commentary,
                })
            elif event_type is AgentEventType.TOOL_STARTED:
                call_id = str(payload.get("call_id") or "")
                raw_input = payload.get("input")
                text = (
                    json.dumps(raw_input, ensure_ascii=False)
                    if raw_input is not None else ""
                )
                _emit({
                    "type": "tool",
                    "tool": str(payload.get("tool") or "tool"),
                    "call_id": call_id,
                    "text": text,
                })
            elif event_type in {
                AgentEventType.TOOL_PROGRESS,
                AgentEventType.TOOL_COMPLETED,
            }:
                call_id = str(payload.get("call_id") or "")
                output = _merge_output(
                    self.tool_outputs.get(call_id, ""),
                    str(payload.get("output") or ""),
                )
                self.tool_outputs[call_id] = output
                if event_type is AgentEventType.TOOL_COMPLETED:
                    _emit({
                        "type": "tool_result",
                        "call_id": call_id,
                        "text": output[:600],
                        "raw": output,
                    })
                    self.tool_outputs.pop(call_id, None)


async def _configure_session(
    transport: AcpTransport, *, session: str, model: str,
) -> str:
    hello = await transport.initialize(timeout=30)
    cwd = os.getcwd()
    if session:
        if hello.resume:
            await transport.resume_session(session, cwd, [], timeout=60)
        elif hello.load_session:
            await transport.load_session(session, cwd, [], timeout=120)
        else:
            raise AcpError("Devin ACP does not support session resume or load")
        session_id = session
    else:
        session_id = await transport.new_session(cwd, [], timeout=60)

    setup = transport.session_setup(session_id)
    modes = {
        str(item.get("id")) for item in
        (setup.get("modes") or {}).get("availableModes", [])
        if isinstance(item, dict)
    }
    if "bypass" not in modes:
        raise AcpError("Devin does not advertise permission mode 'bypass'")
    await transport.set_mode(session_id, "bypass")

    if model:
        model_option = next((
            item for item in setup.get("configOptions", [])
            if isinstance(item, dict)
            and (item.get("category") == "model" or item.get("id") == "model")
        ), None)
        if model_option is None:
            raise AcpError("Devin did not advertise a model configuration option")
        response = await transport.peer.request("session/set_config_option", {
            "sessionId": session_id,
            "configId": model_option["id"],
            "value": model,
        }, timeout=30)
        check_response("session/set_config_option", response)
    return session_id


async def _run(args: argparse.Namespace) -> int:
    events = _EventBridge()
    argv = [args.binary, "acp"]
    if args.model:
        argv.extend(["--model", args.model])
    transport = AcpTransport(
        argv,
        cwd=os.getcwd(),
        on_update=events.on_update,
        permission_handler=_permission_option,
    )
    try:
        await transport.start()
        session_id = await _configure_session(
            transport, session=args.session, model=args.model)
        _emit({"type": "session", "session_id": session_id})
        result = await transport.prompt(session_id, args.prompt, timeout=None)
        events.usage.update(_usage(result.get("usage")))
        events.usage.setdefault("num_turns", 1)
        _emit({"type": "usage", "usage": events.usage})

        stop_reason = str(result.get("stopReason") or "")
        final_text = "".join(events.message_parts).strip()
        if stop_reason not in {"cancelled", "refusal"} and final_text:
            _emit({
                "type": "result",
                "text": final_text,
                "session_id": session_id,
                "usage": events.usage,
            })
            return 0
        if stop_reason == "cancelled":
            detail = "Devin ACP turn was cancelled"
        elif stop_reason == "refusal":
            detail = "Devin ACP turn was refused"
        else:
            detail = "Devin ACP turn ended without assistant text"
        _emit({"type": "error", "message": detail})
        return 1
    except Exception as exc:  # noqa: BLE001
        if events.usage:
            _emit({"type": "usage", "usage": events.usage})
        diagnostics = transport.peer.diagnostics()
        stderr_tail = str(diagnostics.get("stderr_tail") or "").strip()
        detail = str(exc).strip() or type(exc).__name__
        if stderr_tail and stderr_tail not in detail:
            detail = f"{detail}: {stderr_tail[-1200:]}"
        _emit({"type": "error", "message": detail[-1800:]})
        return 1
    finally:
        await transport.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--model", default="")
    parser.add_argument("--session", default="")
    parser.add_argument("--prompt", required=True)
    return asyncio.run(_run(parser.parse_args()))


if __name__ == "__main__":
    raise SystemExit(main())
