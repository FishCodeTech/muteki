"""Devin CLI Worker driver."""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Optional

from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH,
)


_BRIDGE = Path(__file__).resolve().parents[2] / "devin_cli_bridge.py"


class DevinDriver(CliDriver):
    """Run Devin through ACP and consume the bridge's live JSONL events."""

    name = "devin"
    _HELLO_TIMEOUT = 180

    def _argv(self, prompt: str, session: str = "") -> list[str]:
        argv = [
            sys.executable,
            str(_BRIDGE),
            "--binary", self.bin,
        ]
        if session:
            argv.extend(["--session", session])
        argv.extend(["--prompt", prompt])
        return argv

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del session, web_access, kb_access, stream, launch
        return self._argv(prompt)

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del web_access, kb_access, stream, launch
        return self._argv(prompt, session)

    @staticmethod
    def _event(line: str) -> dict:
        try:
            value = json.loads(line)
        except (json.JSONDecodeError, TypeError):
            return {}
        return value if isinstance(value, dict) else {}

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        event = self._event(line)
        kind = str(event.get("type") or "")
        if kind == "session" and event.get("session_id"):
            return StreamStep("session", session=str(event["session_id"]))
        if kind == "reasoning" and event.get("text"):
            return StreamStep(
                "reasoning", text=str(event["text"]),
                thinking=bool(event.get("thinking")),
            )
        if kind == "tool":
            return StreamStep(
                "tool", tool=str(event.get("tool") or "tool"),
                text=str(event.get("text") or "")[:300],
                call_id=str(event.get("call_id") or ""),
            )
        if kind == "tool_result":
            raw = str(event.get("raw") or event.get("text") or "")
            return StreamStep(
                "tool_result", text=str(event.get("text") or raw)[:600], raw=raw,
                call_id=str(event.get("call_id") or ""),
            )
        return None

    def parse(self, stdout: str, stderr: str) -> CliResult:
        text = ""
        session = None
        usage: dict = {}
        error = ""
        for line in stdout.splitlines():
            event = self._event(line)
            kind = event.get("type")
            if kind == "session" and event.get("session_id"):
                session = str(event["session_id"])
            elif kind == "usage" and isinstance(event.get("usage"), dict):
                usage.update(event["usage"])
            elif kind == "result":
                text = str(event.get("text") or "").strip()
                session = str(event.get("session_id") or session or "") or None
                if isinstance(event.get("usage"), dict):
                    usage.update(event["usage"])
            elif kind == "error":
                error = str(event.get("message") or "").strip()
        return CliResult(
            text=text,
            session=session,
            input_tokens=usage.get("input_tokens"),
            output_tokens=usage.get("output_tokens"),
            cache_read_tokens=usage.get("cache_read_tokens"),
            cache_write_tokens=usage.get("cache_write_tokens"),
            reasoning_tokens=usage.get("reasoning_tokens"),
            cost_usd=usage.get("cost_usd"),
            num_turns=usage.get("num_turns"),
            raw_stderr=stderr,
            error=error,
        )

    def _hello_argv(self) -> list[str]:
        return self.build_execute(self.HELLO_PROMPT, None)

    def _hello_ok(self, result) -> bool:
        parsed = self.parse(result.stdout or "", result.stderr or "")
        return result.returncode == 0 and parsed.text.strip() == "OK"
