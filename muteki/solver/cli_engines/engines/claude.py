"""claude CLI driver. Moved from cli_driver.py."""
from __future__ import annotations

import json
import subprocess
import uuid
from pathlib import Path
from typing import Optional


from muteki.solver.cli_engines.base import CliDriver, _secure_help_preflight
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH, _structured_cli_error,
)

_SOLVER_DIR = Path(__file__).resolve().parents[2]

class ClaudeCodeDriver(CliDriver):
    """`claude -p` — pre-seeds a uuid session; resumes with `-r`. Host CLI,
    --dangerously-skip-permissions (full shell), JSON output for clean parsing."""
    name = "claude"
    secure_prompt_transport = True
    offline_web_isolation = True

    def native_permission_modes(self) -> tuple[str, ...]:
        return ("acceptEdits", "auto", "bypassPermissions", "manual",
                "dontAsk", "plan")

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        mode = self.validate_launch_context(launch)
        if not launch.interactive:
            return ["--dangerously-skip-permissions"]
        return ["--permission-mode", mode] if mode else []

    def new_session(self) -> Optional[str]:
        return str(uuid.uuid4())

    # claude exposes WebSearch + WebFetch by default; deny them for a clean
    # (offline) eval so the agent can't fetch a challenge writeup.
    _WEB_TOOLS = ["WebSearch", "WebFetch"]
    _EMPTY_MCP_CONFIG = '{"mcpServers":{}}'

    def _denied(self, *, web_access: bool, kb_access: bool) -> list[str]:
        """The --disallowed-tools list for this run (empty → flag omitted)."""
        deny: list[str] = []
        if not web_access:
            deny += self._WEB_TOOLS
        if not kb_access and self.KB_TOOL_PREFIX:
            # deny the whole inherited KB MCP by server prefix (only if one is
            # configured — KB_TOOL_PREFIX is empty when MUTEKI_KB_MCP_NAME is unset)
            deny.append(self.KB_TOOL_PREFIX)
        return ["--disallowed-tools", *deny] if deny else []

    def _mcp_isolation(self, *, kb_access: bool) -> list[str]:
        # A user can have several MCP servers, while MUTEKI_KB_MCP_NAME names at
        # most one of them. Offline evaluation must exclude every external MCP
        # without changing user configuration or hiding user Skills. Claude's
        # strict config switch does exactly that for this child process.
        if kb_access:
            return []
        return [
            "--mcp-config", self._EMPTY_MCP_CONFIG,
            "--strict-mcp-config",
        ]

    def _fmt(self, stream: bool) -> list[str]:
        # stream-json emits one event per step (needs --verbose); json is a single
        # final doc. Both parse the same way via parse() on accumulated stdout.
        return (["--output-format", "stream-json", "--verbose"] if stream
                else ["--output-format", "json"])

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        argv = [self.bin, "-p", *self._fmt(stream),
                *self._permission_flags(launch)]
        if session:
            argv += ["--session-id", session]
        argv += self._mcp_isolation(kb_access=kb_access)
        argv += self._denied(web_access=web_access, kb_access=kb_access)
        argv += ["--", prompt]
        return argv

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        # Claude print mode reads text input from stdin when no positional prompt is
        # supplied.  --no-session-persistence is the vendor-supported disk fence;
        # ProfileDriver adds --bare for injected credentials and endpoints. The
        # base driver represents host system login and must retain Keychain access.
        # Keep a trailing `--` sentinel so profile model injection has an unambiguous
        # insertion point, but never put the prompt itself in argv.
        del prompt, session
        return [
            self.bin, "-p", *self._fmt(stream),
            *self._permission_flags(launch),
            "--no-session-persistence",
            *self._mcp_isolation(kb_access=kb_access),
            *self._denied(web_access=web_access, kb_access=kb_access),
            "--",
        ]

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return _secure_help_preflight(
            self.bin, ["--help"],
            ("--no-session-persistence", "--print"))

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return [self.bin, "-r", session, "-p", *self._fmt(stream),
                *self._permission_flags(launch),
                *self._mcp_isolation(kb_access=kb_access),
                *self._denied(web_access=web_access, kb_access=kb_access),
                "--", prompt]

    @staticmethod
    def _usage_tokens(usage: dict) -> tuple[Optional[int], Optional[int]]:
        """claude's result `usage` block → (input, output) tokens for the deck's
        token column. Input counts the fresh + both cache buckets (read/creation);
        output is the completion. None when the block is absent."""
        if not isinstance(usage, dict) or not usage:
            return None, None
        inp = (int(usage.get("input_tokens") or 0)
               + int(usage.get("cache_read_input_tokens") or 0)
               + int(usage.get("cache_creation_input_tokens") or 0))
        outp = int(usage.get("output_tokens") or 0)
        return inp, outp

    def parse(self, stdout: str, stderr: str) -> CliResult:
        result = self._parse_result(stdout, stderr)
        latest = {}
        for line in stdout.splitlines():
            try:
                event = json.loads(line)
            except (ValueError, TypeError):
                continue
            if isinstance(event, dict) and isinstance(event.get("usage"), dict):
                latest = event["usage"]
        if latest:
            result.cache_read_tokens = latest.get("cache_read_input_tokens")
            result.cache_write_tokens = latest.get("cache_creation_input_tokens")
        return result

    def _parse_result(self, stdout: str, stderr: str) -> CliResult:
        # Plain --output-format json: one JSON document.
        try:
            d = json.loads(stdout)
            inp, outp = self._usage_tokens(d.get("usage") or {})
            text = str(d.get("result", ""))
            error = ""
            if d.get("is_error"):
                # Vendor error text lives in ``result``; do not project it as an
                # assistant reply.
                error = text.strip() or _structured_cli_error(stdout)
                text = ""
            return CliResult(
                text=text,
                session=d.get("session_id"),
                cost_usd=d.get("total_cost_usd"),
                input_tokens=inp,
                output_tokens=outp,
                num_turns=d.get("num_turns"),
                raw_stderr=stderr[-2000:],
                error=error or _structured_cli_error(stdout),
            )
        except json.JSONDecodeError:
            pass
        # stream-json: many JSONL lines — the final {"type":"result",...} is the
        # outcome. Scan for it (and fall back to raw text if absent).
        result_text, session, cost, turns, inp, outp = "", None, None, None, None, None
        result_is_error = False
        estimated = False
        message_usage = {}
        # fallback usage from the LAST intermediate assistant message — a worker
        # KILLED mid-run (race loser / steer) never emits the final `result`, but
        # each assistant event carries a cumulative `usage` block, so the latest
        # one is the best estimate of what it burned. Without this, killed claude
        # workers report 0 tokens and their spend silently vanishes from the ledger.
        stream_in = stream_out = None
        assistant_chars = 0
        tool_use_count = 0
        for line in stdout.splitlines():
            line = line.strip()
            if not line or not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "result":
                result_text = str(ev.get("result", ""))
                result_is_error = bool(ev.get("is_error"))
                cost = ev.get("total_cost_usd")
                turns = ev.get("num_turns")
                inp, outp = self._usage_tokens(ev.get("usage") or {})
            if ev.get("type") == "assistant":
                msg = ev.get("message") or {}
                u = msg.get("usage")
                si, so = self._usage_tokens(u or {})
                key = str(msg.get("id") or json.dumps(msg, sort_keys=True))
                message_usage[key] = (si, so)
                stream_in = sum(pair[0] or 0 for pair in message_usage.values()) if any(pair[0] is not None for pair in message_usage.values()) else None
                stream_out = sum(pair[1] or 0 for pair in message_usage.values()) if any(pair[1] is not None for pair in message_usage.values()) else None
                for block in msg.get("content") or []:
                    if not isinstance(block, dict):
                        continue
                    bt = str(block.get("type") or "")
                    if bt == "text":
                        assistant_chars += len(str(block.get("text") or ""))
                    elif bt == "tool_use":
                        tool_use_count += 1
                        assistant_chars += len(json.dumps(block.get("input") or {}))
            if ev.get("session_id"):
                session = ev["session_id"]
        if inp is None and outp is None:  # no final result → use the streamed estimate
            inp, outp = stream_in, stream_out
        # Final result sometimes reports output_tokens=0 while stream usage (or
        # visible assistant/tool content) proves the model produced tokens. Prefer
        # the non-zero stream estimate so VOID cells don't fake "no output".
        if (outp is None or int(outp or 0) == 0) and stream_out:
            outp = stream_out
        if (inp is None or int(inp or 0) == 0) and stream_in:
            inp = stream_in
        if (outp is None or int(outp or 0) == 0) and (assistant_chars or tool_use_count):
            # Last-resort estimate when the vendor omitted usage on a killed turn.
            estimated = True
            outp = max(1, (assistant_chars // 4) + (tool_use_count * 32))
        if (inp is None or int(inp or 0) == 0) and outp:
            # Input is unknown; keep a conservative floor so cost.record can fire.
            estimated = True
            inp = int(outp)
        error = _structured_cli_error(stdout)
        if result_is_error:
            error = (result_text.strip() or error)
            result_text = ""
        return CliResult(text=result_text, session=session, cost_usd=cost,
                         usage_estimated=estimated,
                         input_tokens=inp, output_tokens=outp,
                         num_turns=turns, raw_stderr=stderr[-2000:],
                         error=error)

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        # single-step view (first step of the line); see parse_stream_steps for the
        # all-blocks version the streaming runner uses.
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        # #18: a claude assistant message can carry MULTIPLE content blocks (text +
        # tool_use + more text); emit a StreamStep for EVERY block so a FOUND_FLAG /
        # VERIFIED_FACT in a later block propagates live, not only via final parse().
        line = line.strip()
        if not line or not line.startswith("{"):
            return []
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            return []
        t = ev.get("type")
        if t == "system" and ev.get("session_id"):
            return [StreamStep("session", session=ev["session_id"])]
        steps: list[StreamStep] = []
        if t == "assistant":
            for b in (ev.get("message", {}) or {}).get("content", []) or []:
                bt = b.get("type")
                if bt == "text" and b.get("text", "").strip():
                    steps.append(StreamStep("reasoning", text=b["text"].strip()))
                elif bt == "thinking":
                    thinking = str(b.get("thinking") or b.get("text") or "").strip()
                    if thinking:
                        steps.append(StreamStep(
                            "reasoning", text=thinking, thinking=True))
                elif bt == "tool_use":
                    inp = b.get("input", {}) or {}
                    arg = inp.get("command") or inp.get("query") or inp.get("file_path") or ""
                    steps.append(StreamStep(
                        "tool", tool=str(b.get("name", "")), text=str(arg)[:300],
                        call_id=str(b.get("id") or "")))
        elif t == "user":
            for b in (ev.get("message", {}) or {}).get("content", []) or []:
                if isinstance(b, dict) and b.get("type") == "tool_result":
                    c = b.get("content")
                    txt = c if isinstance(c, str) else json.dumps(c)
                    full = txt or ""
                    # text=truncated for the deck; raw=full for the provenance gate.
                    steps.append(StreamStep(
                        "tool_result", text=full[:600], raw=full,
                        call_id=str(b.get("tool_use_id") or "")))
        return steps

    def _hello_argv(self) -> list[str]:
        # one-turn JSON dry-run; _hello_ok asserts the result envelope came back.
        # Keep the minimal turn non-persistent. ProfileDriver adds --bare only for
        # injected credentials/endpoints; host system login needs Keychain reads.
        return [
            self.bin, "-p", "--output-format", "json", "--max-turns", "1",
            "--dangerously-skip-permissions", "--no-session-persistence",
            *self._mcp_isolation(kb_access=False),
            "--tools", "",
            "--", self.HELLO_PROMPT,
        ]

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        if r.returncode != 0:
            return False
        parsed = self.parse(r.stdout or "", r.stderr or "")
        return bool(parsed.text.strip()) and not parsed.error
