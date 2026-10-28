"""ZCode Agent CLI transport using its native stream-json and resume protocol."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any

from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.types import (
    WORKER_LAUNCH,
    CliResult,
    LaunchContext,
    StreamStep,
)


def _event(line: str) -> dict[str, Any]:
    try:
        value = json.loads(line)
    except (json.JSONDecodeError, TypeError):
        return {}
    return value if isinstance(value, dict) else {}


def _text(value: Any) -> str:
    return value if isinstance(value, str) else json.dumps(value, ensure_ascii=False)


class ZCodeDriver(CliDriver):
    name = "zcode"
    offline_web_isolation = True

    def native_permission_modes(self) -> tuple[str, ...]:
        return ("build", "edit", "plan", "yolo", "guarded")

    def build_execute(
        self,
        prompt: str,
        session: str | None,
        *,
        web_access: bool = True,
        kb_access: bool = True,
        stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del stream
        mode = self.validate_launch_context(launch) or (
            "build" if launch.interactive else "yolo"
        )
        argv = [
            self.bin,
            "--mode",
            mode,
            "--workflow-mode",
            "disabled",
            "--no-browser",
            "--output-format",
            "stream-json",
        ]
        denied = [] if web_access else ["WebSearch", "WebFetch", "Agent"]
        if not kb_access and self.KB_TOOL_PREFIX:
            denied.append(f"{self.KB_TOOL_PREFIX}*")
        if denied:
            argv += ["--disallowed-tools", ",".join(denied)]
        if session:
            argv += ["--resume", session]
        return [*argv, "--prompt", prompt]

    def build_resume(self, prompt: str, session: str, **kwargs: Any) -> list[str]:
        return self.build_execute(prompt, session, **kwargs)

    def build_execute_input(
        self, prompt: str, session: str | None, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> tuple[list[str], str | None]:
        del stream
        mode = self.validate_launch_context(launch) or (
            "build" if launch.interactive else "yolo"
        )
        argv = [
            sys.executable,
            str(Path(__file__).resolve().parents[2] / "zcode_prompt_bridge.py"),
            "--binary", self.bin, "--mode", mode,
        ]
        if session:
            argv += ["--session", session]
        denied = [] if web_access else ["WebSearch", "WebFetch", "Agent"]
        if not kb_access and self.KB_TOOL_PREFIX:
            raise ValueError("zcode protocol cannot enforce a wildcard knowledge-base tool denylist")
        for tool in denied:
            argv += ["--deny-tool", tool]
        return argv, prompt

    def build_resume_input(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> tuple[list[str], str | None]:
        if not session:
            raise ValueError("zcode resume requires a session ID")
        return self.build_execute_input(
            prompt, session, web_access=web_access, kb_access=kb_access,
            stream=stream, launch=launch)

    def parse(self, stdout: str, stderr: str) -> CliResult:
        result = CliResult(text="", raw_stderr=stderr)
        turns = 0
        saw_activity = False
        for line in stdout.splitlines():
            event = _event(line)
            kind = event.get("type")
            payload = event.get("payload") or {}
            if not isinstance(payload, dict):
                continue
            result.session = event.get("sessionId") or result.session
            if kind in {"model.streaming", "tool.updated"}:
                saw_activity = True
            if kind == "turn.failed":
                result.error = _text(payload.get("error") or payload)
            if kind == "turn.completed" and payload.get("resultType") not in {
                None,
                "success",
            }:
                result.error = _text(payload)
            if kind in {"turn.completed", "result"}:
                if kind == "turn.completed":
                    turns += 1
                saw_activity = True
                final = event if kind == "result" else payload
                result.text = str(final.get("response") or result.text)
                usage = final.get("usage") or {}
                for source, target in (
                    ("inputTokens", "input_tokens"),
                    ("outputTokens", "output_tokens"),
                    ("cacheReadTokens", "cache_read_tokens"),
                    ("cacheWriteTokens", "cache_write_tokens"),
                    ("reasoningTokens", "reasoning_tokens"),
                ):
                    if isinstance(usage.get(source), int):
                        setattr(result, target, usage[source])
            if kind == "session.updated" and payload.get("stopReason") == "length":
                result.model_truncated = True
        result.num_turns = turns or (1 if saw_activity else None)
        return result

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        event = _event(line)
        kind = event.get("type")
        payload = event.get("payload") or {}
        if not isinstance(payload, dict):
            return []
        steps = []
        if kind in {"turn.started", "session.resumed", "result"} and event.get(
            "sessionId"
        ):
            steps.append(StreamStep("session", session=str(event["sessionId"])))
        if kind == "model.streaming" and payload.get("kind") in {
            "text_delta",
            "reasoning_delta",
        }:
            steps.append(
                StreamStep(
                    "reasoning",
                    text=str(payload.get("delta") or ""),
                    thinking=payload["kind"] == "reasoning_delta",
                )
            )
        if kind == "tool.updated":
            call_id = str(payload.get("toolCallId") or "")
            if payload.get("kind") == "scheduled":
                steps.append(
                    StreamStep(
                        "tool",
                        tool=str(payload.get("toolName") or ""),
                        text=_text(payload.get("input")),
                        call_id=call_id,
                    )
                )
            elif payload.get("kind") in {"result", "error"}:
                value = (
                    payload.get("result")
                    if payload["kind"] == "result"
                    else payload.get("error")
                )
                full = _text(value)
                steps.append(
                    StreamStep("tool_result", text=full, raw=full, call_id=call_id)
                )
        if kind == "turn.failed":
            steps.append(StreamStep("runtime_warning", text=_text(payload)))
        return steps

    def parse_stream_line(self, line: str) -> StreamStep | None:
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def _hello_argv(self) -> list[str]:
        return self.build_execute(
            self.HELLO_PROMPT, None, web_access=False, kb_access=False
        )

    def _hello_ok(self, r: subprocess.CompletedProcess) -> bool:
        parsed = self.parse(r.stdout or "", r.stderr or "")
        return r.returncode == 0 and bool(parsed.text.strip()) and not parsed.error
