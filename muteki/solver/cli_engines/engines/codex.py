"""codex CLI driver. Moved from cli_driver.py."""
from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path
from typing import Optional

from muteki.core.cost import PRICES, CODEX_CACHED_INPUT_PER_M, _DEFAULT_PRICE

from muteki.solver.cli_engines.base import CliDriver, _secure_help_preflight
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH, _structured_cli_error,
)

_SOLVER_DIR = Path(__file__).resolve().parents[2]

class CodexDriver(CliDriver):
    """`codex exec` — engine assigns the session (scraped from stderr 'session id:');
    resumes with `codex exec resume <id>`. May be usage-limited (degrade to claude)."""
    name = "codex"
    secure_prompt_transport = True
    offline_web_isolation = True

    def native_permission_modes(self) -> tuple[str, ...]:
        return ("on-request", "never")

    def native_sandbox_modes(self) -> tuple[str, ...]:
        return ("read-only", "workspace-write", "danger-full-access")

    def _permission_globals(self, launch: LaunchContext) -> list[str]:
        mode = self.validate_launch_context(launch)
        sandbox = self.validate_sandbox_mode(launch)
        if not launch.interactive:
            return []
        return (
            (["--ask-for-approval", mode] if mode else [])
            + (["--sandbox", sandbox] if sandbox else [])
        )

    @staticmethod
    def _permission_exec_flags(launch: LaunchContext) -> list[str]:
        return ([] if launch.interactive
                else ["--dangerously-bypass-approvals-and-sandbox"])
    # Codex CLI can burn ~100s on websocket retries before falling back to HTTPS.
    # Keep the deep probe truthful: a completed fallback turn is healthy, not red.
    _HELLO_TIMEOUT = 150
    _SESSION_RE = re.compile(r"session id:\s*([0-9a-fA-F-]+)")
    # Codex Desktop can inject app/browser/plugin tools independently from the
    # CLI's native --search switch. An offline Worker must remove those tool
    # providers for this child process while leaving CODEX_HOME itself in place
    # so the user's Skills and subscription authentication still work.
    _OFFLINE_FEATURES = (
        "code_mode",
        "deferred_executor",
        "tool_suggest",
        "apps",
        "browser_use",
        "browser_use_external",
        "browser_use_full_cdp_access",
        "in_app_browser",
        "computer_use",
        "image_generation",
        "plugins",
        "plugin_sharing",
        "remote_plugin",
        "enable_mcp_apps",
        "tool_call_mcp_elicitation",
        "auth_elicitation",
        "multi_agent",
        "multi_agent_v2",
    )

    def _globals(self, *, web_access: bool) -> list[str]:
        # `--search` is a GLOBAL codex flag (before the `exec` subcommand) that
        # enables the native web_search tool. codex exec has NO web tool unless it
        # is passed → offline is the default; we only opt IN when web_access is on.
        # (The optional KB MCP lives in claude's user config, not codex's ~/.codex,
        # so codex doesn't see it — claude is the KB consumer.)
        return ["--search"] if web_access else []

    @classmethod
    def _config_isolation(
        cls, *, web_access: bool, kb_access: bool,
    ) -> list[str]:
        # Codex MCP servers are declared in CODEX_HOME/config.toml. The exec-only
        # switch skips that file for this child process while auth and the Skills
        # directories under CODEX_HOME remain available. Desktop-provided tools
        # are feature-gated separately, so offline runs also disable every remote
        # provider and the code-mode bridge that otherwise exposes web__run.
        out = ["--ignore-user-config"] if not kb_access else []
        if not web_access:
            out += ["-c", 'web_search="disabled"']
            for feature in cls._OFFLINE_FEATURES:
                out += ["--disable", feature]
        return out

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        # `--json` already emits live per-step JSONL, so streaming needs no extra
        # flag — stream is accepted for interface parity.
        return [self.bin, *self._globals(web_access=web_access),
                *self._permission_globals(launch),
                "exec", *self._config_isolation(
                    web_access=web_access, kb_access=kb_access),
                "--json", *self._permission_exec_flags(launch),
                "--", prompt]

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        # `codex exec -` is the documented stdin form; --ephemeral prevents session
        # files from being persisted. stream/session remain interface-only.
        del prompt, session, stream
        return [
            self.bin, *self._globals(web_access=web_access),
            *self._permission_globals(launch),
            "exec", *self._config_isolation(
                web_access=web_access, kb_access=kb_access),
            "--json", "--ephemeral",
            *self._permission_exec_flags(launch), "-",
        ]

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return _secure_help_preflight(
            self.bin, ["exec", "--help"],
            ("--ephemeral", "read from stdin"))

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return [self.bin, *self._globals(web_access=web_access),
                *self._permission_globals(launch),
                "exec", "resume", session,
                *self._config_isolation(
                    web_access=web_access, kb_access=kb_access), "--json",
                *self._permission_exec_flags(launch),
                "--", prompt]

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        line = line.strip()
        if not line or not line.startswith("{"):
            return None
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            return None
        t = ev.get("type")
        if t == "thread.started" and ev.get("thread_id"):
            return StreamStep("session", session=ev["thread_id"])
        item = ev.get("item") or {}
        it = item.get("type")
        # a shell command the agent is about to / did run
        if t == "item.started" and it == "command_execution":
            return StreamStep(
                "tool", tool="shell", text=str(item.get("command", ""))[:300],
                call_id=str(item.get("id") or ""))
        if t == "item.completed":
            if it == "command_execution":
                # aggregated_output carries the command's FULL stdout/stderr — including
                # a nested `ssh host '...'` whose remote stdout the outer ssh forwards
                # here. text=truncated for the deck; raw=full for the provenance gate.
                out = str(item.get("aggregated_output") or item.get("output") or "")
                return StreamStep(
                    "tool_result", text=out[:600], raw=out,
                    call_id=str(item.get("id") or ""))
            if it == "agent_message":
                txt = (item.get("text") or "").strip()
                if txt:
                    return StreamStep("reasoning", text=txt)
            if it == "reasoning":
                value = item.get("text") or item.get("summary") or item.get("content") or ""
                if isinstance(value, list):
                    value = "".join(
                        str(part.get("text") or "")
                        if isinstance(part, dict) else str(part)
                        for part in value
                    )
                txt = str(value).strip()
                if txt:
                    return StreamStep("reasoning", text=txt, thinking=True)
        return None

    def parse(self, stdout: str, stderr: str) -> CliResult:
        # codex --json emits JSONL events. codex 0.133–0.137 shape:
        #   {"type":"thread.started","thread_id":"<uuid>"}        ← session for resume
        #   {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
        #   {"type":"turn.completed","usage":{"input_tokens":...,"cached_input_tokens":
        #      ...,"output_tokens":...,"reasoning_output_tokens":...}}
        # Subscription codex NO LONGER reports total_cost_usd, so we re-derive an
        # API-EQUIVALENT cost from the per-turn token usage (sum across turns).
        # Older shapes ({"msg":{...}}, total_cost_usd) are still tolerated.
        text, cost, turns, session = "", None, 0, None
        in_tok = cached_tok = out_tok = reasoning_tok = cache_write_tok = 0
        observed_usage_fields = set()
        for line in stdout.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                # Non-JSON output is a diagnostic, never an assistant message.
                continue
            et = ev.get("type")
            # session id (for a resume/conclude turn)
            if et == "thread.started" and ev.get("thread_id"):
                session = ev["thread_id"]
            # assistant output: 0.133 wraps it in item.completed → item.agent_message
            if et == "item.completed":
                item = ev.get("item") or {}
                if item.get("type") in ("agent_message", "assistant", "message"):
                    text += str(item.get("text") or item.get("message") or "") + "\n"
            if et == "turn.completed":
                turns += 1
                u = ev.get("usage") or {}
                observed_usage_fields.update(u)
                in_tok += int(u.get("input_tokens") or 0)
                cached_tok += int(u.get("cached_input_tokens") or 0)
                # Codex output_tokens already includes the reasoning subset.
                out_tok += int(u.get("output_tokens") or 0)
                reasoning_tok += int(u.get("reasoning_output_tokens") or 0)
                cache_write_tok += int(u.get("cache_write_input_tokens") or 0)
            # legacy / alternate shapes
            msg = ev.get("msg") or ev
            if isinstance(msg, dict):
                if msg.get("type") in ("agent_message", "assistant", "message"):
                    text += str(msg.get("message") or msg.get("text") or "") + "\n"
                if "total_cost_usd" in msg:
                    cost = msg["total_cost_usd"]
        if session is None:
            m = self._SESSION_RE.search(stderr)
            if m:
                session = m.group(1)
        # The JSONL stream does not identify a billing tariff. Unknown models
        # remain unpriced; configured profiles can supply an explicit estimate.
        estimated_cost = False
        return CliResult(text=text[-8000:], session=session,
                         cost_usd=cost, num_turns=turns or None,
                         input_tokens=(in_tok if turns else None), output_tokens=(out_tok if turns else None),
                         cache_read_tokens=cached_tok if turns else None,
                         cache_write_tokens=cache_write_tok if "cache_write_input_tokens" in observed_usage_fields else None,
                         reasoning_tokens=reasoning_tok if "reasoning_output_tokens" in observed_usage_fields else None,
                         cost_estimated=estimated_cost,
                         raw_stderr=stderr[-2000:],
                         error=_structured_cli_error(stdout))

    def _hello_argv(self) -> list[str]:
        # a real one-turn exec (offline, sandboxed) — symmetric with claude/cursor
        # so the self-check actually exercises codex auth, not just `--version`.
        return [self.bin, "exec", *self._config_isolation(
                    web_access=False, kb_access=False), "--json",
                "--dangerously-bypass-approvals-and-sandbox", "--", self.HELLO_PROMPT]

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        # Health is green only when the real process exits successfully and an
        # actual assistant message is followed by a completed turn.
        if r.returncode != 0:
            return False
        saw_text = False
        saw_completed = False
        for line in (r.stdout or "").splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "turn.completed":
                saw_completed = True
            if ev.get("type") == "item.completed":
                item = ev.get("item") or {}
                if (isinstance(item, dict)
                        and item.get("type") == "agent_message"
                        and str(item.get("text") or "").strip()):
                    saw_text = True
        return saw_text and saw_completed
