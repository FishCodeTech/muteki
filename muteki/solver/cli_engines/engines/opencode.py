"""opencode CLI driver. Moved from cli_driver.py."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any, Optional


from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH,
)

_SOLVER_DIR = Path(__file__).resolve().parents[2]

class OpenCodeDriver(CliDriver):
    """OpenCode JSONL transport with stable tool-call identifiers."""

    name = "opencode"
    secure_prompt_transport = False
    offline_web_isolation = True

    def native_permission_modes(self) -> tuple[str, ...]:
        return ("auto",)

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        mode = self.validate_launch_context(launch)
        if not launch.interactive or mode == "auto":
            return ["--auto"]
        return []

    @staticmethod
    def _config(*, web_access: bool) -> str:
        config: dict[str, Any] = {
            "snapshot": False,
            "autoupdate": False,
        }
        if not web_access:
            config["permission"] = {
                "webfetch": "deny",
                "websearch": "deny",
            }
        return json.dumps(config, separators=(",", ":"))

    def env_extra(self) -> "dict[str, str]":
        return {
            "OPENCODE_DISABLE_AUTOUPDATE": "1",
            "OPENCODE_DISABLE_LSP_DOWNLOAD": "1",
        }

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del session, kb_access, stream
        # OPENCODE_CONFIG_CONTENT is scoped to this process.  Keeping it in argv
        # allows the offline permission decision to differ per invocation without
        # mutating the operator's OpenCode configuration.
        return [
            "env", f"OPENCODE_CONFIG_CONTENT={self._config(web_access=web_access)}",
            self.bin, "run", "--pure", "--format", "json",
            *self._permission_flags(launch), prompt,
        ]

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del kb_access, stream
        return [
            "env", f"OPENCODE_CONFIG_CONTENT={self._config(web_access=web_access)}",
            self.bin, "run", "--pure", "--format", "json",
            *self._permission_flags(launch),
            "--session", session, prompt,
        ]

    @staticmethod
    def _tool_text(value: Any) -> str:
        if isinstance(value, str):
            return value
        if isinstance(value, dict) and len(value) == 1:
            only = next(iter(value.values()))
            if isinstance(only, str):
                return only
        try:
            return json.dumps(value, ensure_ascii=False, separators=(",", ":"))
        except TypeError:
            return str(value or "")

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        try:
            event = json.loads(line.strip())
        except (json.JSONDecodeError, TypeError):
            return []
        if not isinstance(event, dict):
            return []
        session = str(event.get("sessionID") or "")
        event_type = str(event.get("type") or "")
        part = event.get("part")
        if not isinstance(part, dict):
            part = {}
        if event_type == "step_start" and session:
            return [StreamStep("session", session=session)]
        if event_type in {"text", "reasoning"}:
            text = str(part.get("text") or "").strip()
            return [StreamStep(
                "reasoning",
                text=text,
                thinking=event_type == "reasoning",
            )] if text else []
        if event_type != "tool_use":
            return []
        state = part.get("state")
        if not isinstance(state, dict):
            state = {}
        call_id = str(part.get("callID") or "")
        tool = str(part.get("tool") or "")
        steps = [StreamStep(
            "tool",
            text=self._tool_text(state.get("input")),
            tool=tool,
            call_id=call_id,
        )]
        status = str(state.get("status") or "")
        if status in {"completed", "error"}:
            output = str(state.get("output") or state.get("error") or "")
            metadata = state.get("metadata")
            if not isinstance(metadata, dict):
                metadata = {}
            steps.append(StreamStep(
                "tool_result",
                text=output[:600],
                raw=output,
                call_id=call_id,
                spill_path=str(metadata.get("outputPath") or ""),
            ))
        return steps

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def parse(self, stdout: str, stderr: str) -> CliResult:
        text_parts: list[str] = []
        session: Optional[str] = None
        input_tokens = output_tokens = cache_read = cache_write = reasoning = 0
        turns = 0
        cost = 0.0
        saw_cost = False
        for line in stdout.splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(event, dict):
                continue
            if event.get("sessionID"):
                session = str(event["sessionID"])
            part = event.get("part")
            if not isinstance(part, dict):
                part = {}
            if event.get("type") == "text" and part.get("text"):
                text_parts.append(str(part["text"]))
            if event.get("type") == "step_finish":
                turns += 1
                tokens = part.get("tokens")
                if not isinstance(tokens, dict):
                    tokens = {}
                cache = tokens.get("cache")
                if not isinstance(cache, dict):
                    cache = {}
                cache_read += int(cache.get("read") or 0)
                cache_write += int(cache.get("write") or 0)
                reasoning += int(tokens.get("reasoning") or 0)
                input_tokens += int(tokens.get("input") or 0) + int(cache.get("read") or 0) + int(cache.get("write") or 0)
                output_tokens += int(tokens.get("output") or 0) + int(tokens.get("reasoning") or 0)
                if part.get("cost") is not None:
                    cost += float(part.get("cost") or 0)
                    saw_cost = True
        return CliResult(
            text="\n".join(text_parts).strip(),
            session=session,
            cost_usd=cost if saw_cost else None,
            input_tokens=input_tokens if turns else None,
            output_tokens=output_tokens if turns else None,
            cache_read_tokens=cache_read if turns else None,
            cache_write_tokens=cache_write if turns else None,
            reasoning_tokens=reasoning if turns else None,
            num_turns=turns or None,
            raw_stderr=stderr[-2000:],
        )

    def _hello_argv(self) -> list[str]:
        return self.build_execute(
            self.HELLO_PROMPT, None, web_access=False, kb_access=False)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        return r.returncode == 0 and bool(self.parse(r.stdout or "", r.stderr or "").text)
