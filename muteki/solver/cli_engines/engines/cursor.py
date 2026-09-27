"""cursor CLI driver. Moved from cli_driver.py."""
from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path
from typing import Optional


from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, SecurePromptUnsupported, StreamStep,
    WORKER_LAUNCH, _structured_cli_error,
)

_SOLVER_DIR = Path(__file__).resolve().parents[2]

_POLICY_REFUSAL_RE = re.compile(
    r"^\s*(?:sorry[,，:\s-]*)?(?:i\s+)?(?:"
    r"can(?:not|['\N{RIGHT SINGLE QUOTATION MARK}]t)|"
    r"won['\N{RIGHT SINGLE QUOTATION MARK}]t|will\s+not|do\s+not)\s+"
    r"(?:help|assist|comply|support)\b",
    re.IGNORECASE,
)


def _policy_refusal_error(text: str, *, used_tools: bool) -> str:
    """Classify a zero-action Cursor refusal as an execution failure.

    Cursor can exit zero with a normal-looking result envelope after declining
    the assigned Worker role. Treating that as ``explored`` closes the intent
    and lets the same profile consume later work. Keep the matcher deliberately
    narrow and require that no tool call started, so an ordinary conclusion that
    happens to mention a refusal is not reclassified.
    """
    if used_tools or not _POLICY_REFUSAL_RE.search(text or ""):
        return ""
    return "policy_refusal: cursor declined the assigned worker task"


class CursorDriver(CliDriver):
    """Cursor headless driver.

    Worker 固定使用 Cursor 自带的 ``-p --force --trust`` 无交互路径。Cursor
    当前没有可核验的原生 Web 工具禁用参数，因此离线评测在启动校验阶段明确
    拒绝该 Profile，绝不改走 ACP。
    """
    name = "cursor"
    offline_web_isolation = False
    # optional pinned model (e.g. "sonnet-4.5-thinking"); unset → cursor's default.
    _MODEL_ENV = "MUTEKI_CURSOR_MODEL"

    def native_permission_modes(self) -> tuple[str, ...]:
        return ("plan", "ask", "auto-review", "force")

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        mode = self.validate_launch_context(launch)
        if not launch.interactive:
            return ["--force"]
        if mode in ("plan", "ask"):
            return ["--mode", mode]
        return [f"--{mode}"] if mode else []

    def new_session(self) -> Optional[str]:
        # cursor assigns the chat id itself; we scrape it from the stream so a
        # resume/conclude turn can reconnect with --resume.
        return None

    def _model(self) -> list[str]:
        m = os.environ.get(self._MODEL_ENV)
        return ["--model", m] if m else []

    def _fmt(self, stream: bool) -> list[str]:
        # stream-json emits one NDJSON event per step; json is a single final doc.
        # We do NOT pass --stream-partial-output, so each assistant event is one
        # complete message (no per-delta de-duplication needed).
        return (["--output-format", "stream-json"] if stream
                else ["--output-format", "json"])

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        # -p (print/headless) + --force (run all commands) + --trust (skip the
        # workspace-trust prompt in headless mode). Prompt is the trailing POSITIONAL
        # arg (cursor has no `--` separator). cwd is the subprocess cwd, so no
        # explicit --workspace is needed (matches the claude/codex drivers).
        del session, kb_access
        if not web_access:
            raise RuntimeError(
                "Cursor CLI 没有可核验的原生离线 Web 工具开关；"
                "离线评测不允许改走 ACP"
            )
        return [self.bin, "-p", *self._fmt(stream),
                *self._permission_flags(launch), "--trust",
                *self._model(), prompt]

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        # Cursor 2026.07's `commands/build-prompt.ts` does read stdin when headless,
        # stdin is non-TTY, and the positional prompt is empty.  It does *not* expose
        # a --no-session-persistence/--ephemeral equivalent, however: runChat always
        # creates a chat store under the Cursor state root.  Exact operator secrets
        # therefore fail closed instead of being written into that store.
        del prompt, session, web_access, kb_access, stream, launch
        raise SecurePromptUnsupported(
            "cursor accepts headless prompts from stdin but cannot disable chat "
            "session persistence; exact secret context is unsupported")

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del kb_access
        if not web_access:
            raise RuntimeError(
                "Cursor CLI 没有可核验的原生离线 Web 工具开关；"
                "离线评测不允许改走 ACP"
            )
        return [self.bin, "-p", *self._fmt(stream),
                *self._permission_flags(launch), "--trust",
                "--resume", session, *self._model(), prompt]

    @staticmethod
    def _usage_tokens(usage: dict) -> tuple[Optional[int], Optional[int]]:
        """cursor's result `usage` block → (input, output) tokens for the deck's
        token column. cursor uses camelCase + separate cache buckets:
        {inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens}. Input
        counts the fresh + both cache buckets. None when the block is absent.
        Cost stays $0 — cursor is subscription-backed and reports no dollar figure."""
        if not isinstance(usage, dict) or not usage:
            return None, None
        inp = (int(usage.get("inputTokens") or 0)
               + int(usage.get("cacheReadTokens") or 0)
               + int(usage.get("cacheWriteTokens") or 0))
        outp = int(usage.get("outputTokens") or 0)
        return inp, outp

    @staticmethod
    def _tool_summary(tc: dict) -> tuple[str, str]:
        """(tool_name, arg_preview) from cursor's tool_call object. Shapes:
        {"readToolCall": {"args": {...}}} | {"function": {"name","arguments"}}."""
        if not isinstance(tc, dict) or not tc:
            return ("", "")
        key = next(iter(tc))
        body = tc.get(key) or {}
        if key == "function" and isinstance(body, dict):
            return (str(body.get("name", "function")),
                    str(body.get("arguments", ""))[:300])
        name = key[:-8] if key.endswith("ToolCall") else key  # readToolCall → read
        arg = ""
        if isinstance(body, dict) and isinstance(body.get("args"), dict):
            a = body["args"]
            arg = str(a.get("path") or a.get("command") or a.get("query") or "")[:300]
        return (name, arg)

    def parse(self, stdout: str, stderr: str) -> CliResult:
        result = CursorDriver._parse_result(self, stdout, stderr)
        latest = {}
        for line in stdout.splitlines():
            try:
                event = json.loads(line)
            except (ValueError, TypeError):
                continue
            if isinstance(event, dict) and isinstance(event.get("usage"), dict):
                latest = event["usage"]
        if latest:
            result.cache_read_tokens = latest.get("cacheReadTokens")
            result.cache_write_tokens = latest.get("cacheWriteTokens")
        return result

    def _parse_result(self, stdout: str, stderr: str) -> CliResult:
        # --output-format json: one JSON object {type:result, result, session_id, ...}
        try:
            d = json.loads(stdout)
            if isinstance(d, dict) and (d.get("type") == "result" or "result" in d):
                inp, outp = CursorDriver._usage_tokens(d.get("usage") or {})
                result_text = str(d.get("result", ""))
                return CliResult(
                    text=result_text,
                    session=d.get("session_id"),
                    cost_usd=None,         # subscription-backed; no per-run cost
                    input_tokens=inp,
                    output_tokens=outp,
                    num_turns=None,
                    raw_stderr=stderr[-2000:],
                    error=_policy_refusal_error(
                        result_text, used_tools=False),
                )
        except json.JSONDecodeError:
            pass
        # stream-json: NDJSON. The terminal {"type":"result",...} carries the full
        # text + usage; any line may carry session_id (system.init or result).
        result_text, session, inp, outp = "", None, None, None
        used_tools = False
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
                inp, outp = CursorDriver._usage_tokens(ev.get("usage") or {})
            if (ev.get("type") == "tool_call"
                    and ev.get("subtype") == "started"):
                used_tools = True
            if ev.get("session_id"):
                session = ev["session_id"]
        error = _structured_cli_error(stdout)
        if not error:
            error = _policy_refusal_error(result_text, used_tools=used_tools)
        return CliResult(text=result_text, session=session, cost_usd=None,
                         input_tokens=inp, output_tokens=outp,
                         num_turns=None, raw_stderr=stderr[-2000:],
                         error=error)

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        # single-step view (first step of the line); see parse_stream_steps for the
        # all-blocks version the streaming runner uses.
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        # #18: a cursor assistant message can carry MULTIPLE text blocks; emit one
        # StreamStep per block so a FOUND_FLAG/VERIFIED_FACT in a later block isn't
        # lost from live propagation (tool_call/system lines carry one step each).
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
        if t == "assistant":
            steps: list[StreamStep] = []
            for b in (ev.get("message", {}) or {}).get("content", []) or []:
                if b.get("type") == "text" and (b.get("text") or "").strip():
                    steps.append(StreamStep("reasoning", text=b["text"].strip()))
                elif b.get("type") == "thinking":
                    thinking = str(b.get("thinking") or b.get("text") or "").strip()
                    if thinking:
                        steps.append(StreamStep(
                            "reasoning", text=thinking, thinking=True))
            return steps
        if t == "tool_call":
            sub = ev.get("subtype")
            tc = ev.get("tool_call") or {}
            call_id = str(ev.get("call_id") or tc.get("id") or "")
            if sub == "started":
                tool, arg = self._tool_summary(tc)
                return [StreamStep("tool", tool=tool, text=arg, call_id=call_id)]
            if sub == "completed":
                # Cursor tool families expose different success payloads. File tools
                # use content/path; shellToolCall uses interleavedOutput or stdout/stderr.
                body = tc.get(next(iter(tc))) if isinstance(tc, dict) and tc else {}
                res = (body or {}).get("result") if isinstance(body, dict) else None
                content = ""
                spill_path = ""
                spill_size_bytes = -1
                spill_line_count = -1
                if isinstance(res, dict):
                    outcome = res.get("success")
                    if not isinstance(outcome, dict):
                        outcome = res.get("failure")
                    if (not isinstance(outcome, dict)
                            and res.get("case") in {"success", "failure"}
                            and isinstance(res.get("value"), dict)):
                        outcome = res["value"]
                    if isinstance(outcome, dict):
                        content = str(outcome.get("content") or "")
                        if not content:
                            content = str(outcome.get("interleavedOutput") or "")
                        if not content:
                            content = "\n".join(
                                str(outcome.get(key) or "")
                                for key in ("stdout", "stderr")
                                if outcome.get(key))
                        if not content:
                            content = str(outcome.get("path") or "")
                        location = outcome.get("outputLocation")
                        if isinstance(location, dict):
                            spill_path = str(location.get("filePath") or "")
                            try:
                                spill_size_bytes = int(location.get("sizeBytes", -1))
                            except (TypeError, ValueError):
                                spill_size_bytes = -1
                            try:
                                spill_line_count = int(location.get("lineCount", -1))
                            except (TypeError, ValueError):
                                spill_line_count = -1
                # text=truncated for the deck; raw=full for the provenance gate. A
                # spill path remains metadata-only until CliSolver validates it.
                return [StreamStep(
                    "tool_result", text=content[:600], raw=content,
                    call_id=call_id, spill_path=spill_path,
                    spill_size_bytes=spill_size_bytes,
                    spill_line_count=spill_line_count)]
        return []

    def _hello_argv(self) -> list[str]:
        return self.build_execute(
            self.HELLO_PROMPT, None, web_access=True, kb_access=False)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        if r.returncode != 0:
            return False
        parsed = self.parse(r.stdout or "", r.stderr or "")
        return bool(parsed.text.strip()) and not parsed.error
