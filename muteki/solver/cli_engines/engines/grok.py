"""grok CLI driver. Moved from cli_driver.py."""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import Optional


from muteki.solver.cli_engines.types import (
    LaunchContext, SecurePromptUnsupported, WORKER_LAUNCH,
)
from muteki.solver.cli_engines.engines.claude import ClaudeCodeDriver

_SOLVER_DIR = Path(__file__).resolve().parents[2]

class GrokDriver(ClaudeCodeDriver):
    """`grok --single` with Anthropic-compatible streaming message output."""

    name = "grok"
    secure_prompt_transport = False
    offline_web_isolation = True
    _OFFLINE_AGENT_FILE = _SOLVER_DIR / "grok_offline_agent.md"
    _OFFLINE_TOOLS = (
        "web_search",
        "web_fetch",
        "search_tool",
        "use_tool",
        "Agent",
    )
    _OFFLINE_ENV = (
        "GROK_CLAUDE_MCPS_ENABLED=false",
        "GROK_CURSOR_MCPS_ENABLED=false",
    )

    def native_permission_modes(self) -> tuple[str, ...]:
        return ("default", "acceptEdits", "auto", "dontAsk",
                "bypassPermissions", "plan")

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        mode = self.validate_launch_context(launch)
        if not launch.interactive:
            mode = "bypassPermissions"
        return ["--permission-mode", mode] if mode else []

    def _offline_prefix(self, *, web_access: bool) -> list[str]:
        # Keep Grok's normal user Skill discovery, while preventing its Claude
        # and Cursor compatibility scanners from starting external MCP servers
        # for an offline task. These variables affect only this child process.
        return ["env", *self._OFFLINE_ENV] if not web_access else []

    def _base_flags(
        self, *, web_access: bool, launch: LaunchContext,
    ) -> list[str]:
        flags = [
            *self._permission_flags(launch),
            "--no-subagents",
            "--output-format", "streaming-messages-json",
        ]
        if not web_access:
            if not GrokDriver._OFFLINE_AGENT_FILE.is_file():
                raise FileNotFoundError(
                    "Grok offline Agent profile missing: "
                    f"{GrokDriver._OFFLINE_AGENT_FILE}")
            flags += [
                "--agent", str(GrokDriver._OFFLINE_AGENT_FILE),
                "--disable-web-search",
                "--disallowed-tools", ",".join(GrokDriver._OFFLINE_TOOLS),
                "--deny", "MCPTool",
            ]
        return flags

    def new_session(self) -> Optional[str]:
        return None

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del session, kb_access, stream
        return [
            *self._offline_prefix(web_access=web_access),
            self.bin, *self._base_flags(
                web_access=web_access, launch=launch), "-p", prompt,
        ]

    def build_execute_input(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> tuple[list[str], Optional[str]]:
        argv = self.build_execute(
            "", session, web_access=web_access, kb_access=kb_access,
            stream=stream, launch=launch)
        prefix = self._offline_prefix(web_access=web_access)
        return [*prefix, sys.executable, str(_SOLVER_DIR / "grok_prompt_bridge.py"),
                "--binary", self.bin, *argv[len(prefix) + 1:-2]], prompt

    def build_resume_input(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> tuple[list[str], Optional[str]]:
        argv = self.build_resume(
            "", session, web_access=web_access, kb_access=kb_access,
            stream=stream, launch=launch)
        prefix = self._offline_prefix(web_access=web_access)
        return [*prefix, sys.executable, str(_SOLVER_DIR / "grok_prompt_bridge.py"),
                "--binary", self.bin, *argv[len(prefix) + 1:-2]], prompt

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del prompt, session, web_access, kb_access, stream, launch
        raise SecurePromptUnsupported(
            "grok does not provide a non-persistent stdin prompt mode")

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del kb_access, stream
        return [
            *self._offline_prefix(web_access=web_access),
            self.bin, "--resume", session,
            *self._base_flags(
                web_access=web_access, launch=launch), "-p", prompt,
        ]

    def _hello_argv(self) -> list[str]:
        return self.build_execute(
            self.HELLO_PROMPT, None, web_access=False, kb_access=False)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        if r.returncode != 0:
            return False
        parsed = self.parse(r.stdout or "", r.stderr or "")
        return bool(parsed.text.strip()) and not parsed.error
