"""Send an exact prompt through the native Pi JSONL RPC interface."""
from __future__ import annotations

import argparse
import json
import subprocess
import sys


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--binary", required=True)
    args, pi_args = parser.parse_known_args()
    prompt = sys.stdin.buffer.read().decode("utf-8")
    child = subprocess.Popen(
        [args.binary, "--mode", "rpc", *pi_args],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    )
    assert child.stdin is not None and child.stdout is not None
    error = ""
    acknowledged = False
    session_seen = False
    ended = False
    assistant_error = ""
    try:
        for command in (
            {"id": "muteki-state", "type": "get_state"},
            {"id": "muteki-prompt", "type": "prompt", "message": prompt},
        ):
            child.stdin.write(json.dumps(command, ensure_ascii=True).encode("ascii") + b"\n")
            child.stdin.flush()
        for raw in child.stdout:
            try:
                event = json.loads(raw)
                if not isinstance(event, dict):
                    raise ValueError("event is not an object")
            except (ValueError, UnicodeDecodeError) as exc:
                error = f"invalid Pi RPC output: {exc}"
                break
            if event.get("type") == "response":
                if event.get("id") in {"muteki-state", "muteki-prompt"}:
                    if not event.get("success"):
                        error = f"Pi {event.get('command')} failed: {event.get('error')}"
                        break
                    if event["id"] == "muteki-state":
                        state = event.get("data") or {}
                        session_id = state.get("sessionId") if isinstance(state, dict) else None
                        if not session_id:
                            error = "Pi get_state returned no session ID"
                            break
                        print(json.dumps({"type": "session", "id": session_id}), flush=True)
                        session_seen = True
                    else:
                        acknowledged = True
                continue
            sys.stdout.buffer.write(raw)
            sys.stdout.buffer.flush()
            if event.get("type") == "message_end":
                message = event.get("message") or {}
                if isinstance(message, dict) and message.get("role") == "assistant":
                    stop = str(message.get("stopReason") or "").lower()
                    assistant_error = str(message.get("errorMessage") or "")
                    if not assistant_error and stop in {"error", "failed", "refusal", "aborted"}:
                        assistant_error = stop
            if event.get("type") == "agent_end":
                messages = event.get("messages") or []
                for message in reversed(messages):
                    if isinstance(message, dict) and message.get("role") == "assistant":
                        stop = str(message.get("stopReason") or "").lower()
                        assistant_error = str(message.get("errorMessage") or "")
                        if not assistant_error and stop in {"error", "failed", "refusal", "aborted"}:
                            assistant_error = stop
                        break
                ended = not event.get("willRetry") and event.get("isTerminal") is not False
            if event.get("type") == "agent_settled" and ended:
                if event.get("aborted"):
                    error = "Pi agent turn was aborted"
                elif assistant_error:
                    error = f"Pi agent turn failed: {assistant_error}"
                break
        else:
            error = "Pi RPC exited before turn completion"
        if not error and (not acknowledged or not session_seen or not ended):
            error = "Pi RPC did not confirm the prompt and session"
    except (BrokenPipeError, OSError) as exc:
        error = f"Pi RPC transport failed: {exc}"
    finally:
        try:
            child.stdin.close()
        except (BrokenPipeError, OSError):
            pass
        if error and child.poll() is None:
            child.terminate()
        returncode = child.wait()
    if error:
        print(error, file=sys.stderr, flush=True)
        return returncode if returncode > 0 else 1
    return returncode if returncode >= 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
