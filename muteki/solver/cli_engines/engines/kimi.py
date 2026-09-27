"""kimi CLI driver. Moved from cli_driver.py."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional


from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH, _structured_cli_error,
)

_SOLVER_DIR = Path(__file__).resolve().parents[2]

class KimiCodeDriver(CliDriver):
    """`kimi -p` using Kimi Code's documented stream-json prompt mode."""

    name = "kimi"
    offline_web_isolation = True
    _ENV_EXTRA = {"KIMI_CODE_NO_AUTO_UPDATE": "1"}
    # Kimi Code does not expose a top-level --no-web flag.  Its documented
    # Agent profile denylist removes tools from the model-visible tool set and
    # enforces the same denylist again before execution.  Keep the profile in
    # the Muteki source tree: no user-level Agent or Skill directory is changed.
    _OFFLINE_AGENT_FILE = _SOLVER_DIR / "kimi_offline_agent.md"

    def native_permission_modes(self) -> tuple[str, ...]:
        return ("yolo", "auto", "plan")

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        mode = self.validate_launch_context(launch)
        if not launch.interactive:
            # Kimi 0.38 的 -p prompt mode 本身是无人值守模式，并拒绝与
            # --auto/--yolo 组合；真实 Worker 工具回合用于验收该行为。
            return []
        flag = {"yolo": "--yolo", "auto": "--auto", "plan": "--plan"}.get(mode)
        return [flag] if flag else []

    def env_extra(self) -> "dict[str, str]":
        return dict(self._ENV_EXTRA)

    def _offline_flags(self, *, web_access: bool) -> list[str]:
        if web_access:
            return []
        if not self._OFFLINE_AGENT_FILE.is_file():
            raise FileNotFoundError(
                f"Kimi offline Agent profile missing: {self._OFFLINE_AGENT_FILE}")
        return ["--agent-file", str(self._OFFLINE_AGENT_FILE)]

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del session, kb_access, stream
        return [
            self.bin,
            *self._permission_flags(launch),
            *self._offline_flags(web_access=web_access),
            "--output-format", "stream-json", "-p", prompt,
        ]

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        # Kimi binds the selected Agent profile when the session is created and
        # restores that binding on resume.  --agent-file cannot be combined with
        # --session, so the initial offline invocation is the enforcement point.
        del web_access, kb_access, stream
        return [
            self.bin, *self._permission_flags(launch), "--session", session,
            "--output-format", "stream-json", "-p", prompt,
        ]

    def parse(self, stdout: str, stderr: str) -> CliResult:
        text_out, session = "", None
        for line in stdout.splitlines():
            try:
                ev = json.loads(line)
            except (json.JSONDecodeError, TypeError):
                continue
            if not isinstance(ev, dict):
                continue
            if ev.get("role") == "assistant" and isinstance(ev.get("content"), str):
                text_out = str(ev["content"])
            if ev.get("role") == "meta" and ev.get("type") == "session.resume_hint":
                session = str(ev.get("session_id") or "") or session
        return CliResult(
            text=text_out, session=session, raw_stderr=stderr[-2000:],
            error=_structured_cli_error(stdout))

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        try:
            ev = json.loads(line.strip())
        except (json.JSONDecodeError, TypeError):
            return []
        if not isinstance(ev, dict):
            return []
        role = ev.get("role")
        if role == "meta" and ev.get("type") == "session.resume_hint":
            sid = str(ev.get("session_id") or "")
            return [StreamStep("session", session=sid)] if sid else []
        if role == "meta" and ev.get("type") == "thinking.delta":
            thinking = str(ev.get("content") or ev.get("text") or "")
            return [StreamStep(
                "reasoning", text=thinking, thinking=True,
            )] if thinking else []
        if role == "assistant":
            steps: list[StreamStep] = []
            content = ev.get("content")
            if isinstance(content, str) and content.strip():
                steps.append(StreamStep("reasoning", text=content.strip()))
            for call in ev.get("tool_calls") or []:
                if not isinstance(call, dict):
                    continue
                fn = call.get("function") if isinstance(call.get("function"), dict) else {}
                steps.append(StreamStep(
                    "tool", tool=str(fn.get("name") or call.get("type") or ""),
                    text=str(fn.get("arguments") or "")[:300],
                    call_id=str(call.get("id") or ""),
                ))
            return steps
        if role == "tool":
            full = str(ev.get("content") or "")
            return [StreamStep(
                "tool_result", text=full[:600], raw=full,
                call_id=str(ev.get("tool_call_id") or ""),
            )]
        return []

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def _hello_argv(self) -> list[str]:
        return self.build_execute(
            self.HELLO_PROMPT, None, web_access=False, kb_access=False)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        if r.returncode != 0:
            return False
        parsed = self.parse(r.stdout or "", r.stderr or "")
        return bool(parsed.text.strip()) and not parsed.error
