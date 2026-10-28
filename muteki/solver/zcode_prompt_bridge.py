"""Send one ZCode prompt through its native app-server NDJSON protocol."""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
from pathlib import Path
from typing import Any

# This bridge is invoked by absolute path, including from a Worker workspace.
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from muteki.external_agents.rpc import StdioJsonlPeer


class _ZCodePeer(StdioJsonlPeer):
    async def send(self, message: dict[str, Any]) -> None:
        # ZCode decodes each stdin chunk independently. ASCII JSON escaping
        # prevents a pipe chunk boundary from splitting a UTF-8 character.
        if not self.running or self._proc is None or self._proc.stdin is None:
            raise RuntimeError("ZCode protocol process is not running")
        self._proc.stdin.write(json.dumps(message, ensure_ascii=True).encode("ascii") + b"\n")
        await self._proc.stdin.drain()


def emit(event: dict[str, Any]) -> None:
    print(json.dumps(event, ensure_ascii=False), flush=True)


async def run(args: argparse.Namespace, prompt: str) -> int:
    done = asyncio.Event()
    failed = False
    session_id = args.session
    active = False
    peer: StdioJsonlPeer

    async def on_message(message: dict[str, Any]) -> None:
        nonlocal failed
        method = message.get("method")
        if "id" in message and method:
            if method == "session/requestRuntimePreferences":
                await peer.send({"id": message["id"], "result": {
                    "nativeSearchEnhancementsEnabled": False,
                }})
            else:
                await peer.send({"id": message["id"], "error": {
                    "code": -32601, "message": f"Unsupported client request: {method}",
                }})
            return
        if method == "session/event" and active:
            event = message.get("params")
            if isinstance(event, dict) and event.get("sessionId") == session_id:
                emit(event)
                if event.get("type") in {"turn.completed", "turn.failed"}:
                    failed = event.get("type") == "turn.failed" or (
                        event.get("payload", {}).get("resultType") not in (None, "success")
                    )
                    done.set()
        elif method == "state.updated" and active and not done.is_set():
            params = message.get("params") or {}
            if params.get("sessionId") == session_id and params.get("reason") == "prompt_failed":
                failed = True
                emit({"type": "turn.failed", "sessionId": session_id,
                      "payload": {"error": "ZCode prompt failed"}})
                done.set()

    peer = _ZCodePeer(
        [args.binary, "app-server", "--stdio"], cwd=os.getcwd(), label="zcode-protocol",
        request_envelope=lambda id, method, params: {"id": id, "method": method, "params": params},
        on_message=on_message,
    )

    async def request(method: str, params: dict[str, Any]) -> dict[str, Any]:
        response = await peer.request(method, params)
        if "error" in response:
            raise RuntimeError(f"{method}: {json.dumps(response['error'], ensure_ascii=False)}")
        result = response.get("result")
        if not isinstance(result, dict):
            raise RuntimeError(f"{method}: invalid response")
        return result

    try:
        await peer.start()
        cwd = os.getcwd()
        workspace = {"workspacePath": cwd, "workspaceKey": cwd}
        common: dict[str, Any] = {
            "workspace": workspace, "toolDenylist": args.deny_tool,
            "dynamicWorkflowEnabled": False, "offPeakToolEnabled": False,
        }
        if session_id:
            snapshot = await request("session/resume", {**common, "sessionId": session_id})
        else:
            snapshot = await request("session/create", {
                **common, "mode": args.mode, "persistence": "immediate",
            })
            session_id = str((snapshot.get("session") or {}).get("sessionId") or "")
            if not session_id:
                raise RuntimeError("session/create returned no session ID")
        if (snapshot.get("session") or {}).get("sessionId") != session_id:
            raise RuntimeError("ZCode returned a different session ID")
        snapshot = await request("session/setMode", {"sessionId": session_id, "mode": args.mode})
        model_settings = (snapshot.get("settings") or {}).get("model") or {}
        model = model_settings.get("current")
        if not isinstance(model, dict) or not model.get("providerId") or not model.get("modelId"):
            raise RuntimeError(
                "ZCode has no available selected model in the current provider configuration"
            )
        configured = await request("session/setModel", {
            "sessionId": session_id, "model": model,
            "persistAsWorkspaceLastUsed": False,
        })
        selected = ((configured.get("settings") or {}).get("model") or {}).get("current")
        if not isinstance(selected, dict) or any(
            selected.get(key) != model.get(key) for key in ("providerId", "modelId")
        ):
            raise RuntimeError("ZCode did not apply the selected model")
        await request("session/subscribe", {
            "sessionId": session_id, "deliveryKind": "desktop-continuous",
            "includeSnapshot": False,
        })
        emit({"type": "session.resumed" if args.session else "turn.started",
              "sessionId": session_id, "payload": {}})
        active = True
        await request("session/send", {
            "sessionId": session_id, "content": prompt, "toolDenylist": args.deny_tool,
        })
        done_task = asyncio.create_task(done.wait())
        closed_task = asyncio.create_task(peer.wait_closed())
        completed, pending = await asyncio.wait(
            [done_task, closed_task], return_when=asyncio.FIRST_COMPLETED)
        for task in pending:
            task.cancel()
        if not done.is_set():
            raise RuntimeError("ZCode protocol closed before turn completion")
        return 1 if failed else 0
    except Exception as exc:
        emit({"type": "turn.failed", "sessionId": session_id or None,
              "payload": {"error": str(exc)}})
        return 1
    finally:
        await peer.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--mode", required=True)
    parser.add_argument("--session", default="")
    parser.add_argument("--deny-tool", action="append", default=[])
    args = parser.parse_args()
    prompt = sys.stdin.buffer.read().decode("utf-8")
    return asyncio.run(run(args, prompt))


if __name__ == "__main__":
    raise SystemExit(main())
