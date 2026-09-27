"""pi CLI driver. Moved from cli_driver.py."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional


from muteki.solver.cli_engines.base import CliDriver, _secure_help_preflight
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH, _structured_cli_error,
)

_SOLVER_DIR = Path(__file__).resolve().parents[2]

class PiLikeDriver(CliDriver):
    """Shared adapter for the pi CLI family (`pi` and oh-my-pi `omp`).

    Both speak the same headless protocol:
      - execute:  `[bin, -p, --mode, json, ...flags, PROMPT]` — the prompt is a
        trailing POSITIONAL (like cursor; there is no `--` separator).
      - stdin:    same argv with `--no-session` and NO prompt; the prompt is piped
        via stdin (ephemeral, exact-secret safe).
      - resume:   pi `[bin, --session, <id>, -p, --mode, json, ..., PROMPT]`;
        omp uses `--resume <id>` instead of `--session`.
      - output:   JSONL events, one object per line. The engine assigns the
        session — scraped from the first {"type":"session","id":...} header.
        `--mode json` already emits one event per step, so `stream` needs no
        extra flag (accepted for interface parity, like codex).

    pi's built-in tools (read/bash/edit/write/grep/find/ls) have NO web access,
    so `web_access=False` needs no argv change. OMP overrides these methods and
    keeps the native ``-p`` path with a run-scoped config that disables web tools.
    Neither inherits claude's user-scope KB MCP on the normal headless path.
    """
    name = "pi"
    secure_prompt_transport = True
    offline_web_isolation = True

    # optional pinned model/provider (e.g. "muteki"/"deepseek-v4-flash:0731-cloud");
    # unset → the CLI's own default. A model id may contain ":" — passed verbatim.
    _MODEL_ENV = "MUTEKI_PI_MODEL"
    _PROVIDER_ENV = "MUTEKI_PI_PROVIDER"
    _RESUME_FLAG = "--session"        # omp overrides with --resume
    _CONTEXT_FLAGS: tuple[str, ...] = ()
    # Optional native tool allowlist for Pi-compatible drivers.
    _OFFLINE_TOOLS: tuple[str, ...] = ()
    _ENV_EXTRA: dict[str, str] = {}

    def new_session(self) -> Optional[str]:
        # the engine assigns the session id itself; we scrape it from the
        # {"type":"session"} stream header so a resume/conclude turn can reconnect.
        return None

    def env_extra(self) -> "dict[str, str]":
        return dict(self._ENV_EXTRA)

    def _provider_model_flags(self) -> list[str]:
        # Provider/model are resolved per profile and injected by
        # apply_runtime_argv(). Reading os.environ here made concurrent probes use
        # unrelated process-global values before their explicit env was applied.
        return []

    def _offline_flags(self, *, web_access: bool) -> list[str]:
        # A subclass may replace the default tool set with a local-only list.
        if web_access or not self._OFFLINE_TOOLS:
            return []
        return ["--tools", ",".join(self._OFFLINE_TOOLS)]

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        self.validate_launch_context(launch)
        return []

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        # kb_access is a no-op (the optional KB MCP lives in claude's user config);
        # --mode json already streams per-step events, so stream needs no flag.
        return [self.bin, "-p", "--mode", "json",
                *self._permission_flags(launch),
                *self._CONTEXT_FLAGS,
                *self._offline_flags(web_access=web_access),
                *self._provider_model_flags(), prompt]

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        # Headless pi/omp read a missing positional prompt from stdin when it is
        # non-TTY; --no-session keeps the run ephemeral (never persisted to the
        # session store) — the two properties exact-secret delivery requires.
        del prompt, session, kb_access, stream
        return [self.bin, "-p", "--mode", "json", "--no-session",
                *self._permission_flags(launch),
                *self._CONTEXT_FLAGS,
                *self._offline_flags(web_access=web_access),
                *self._provider_model_flags()]

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return _secure_help_preflight(
            self.bin, ["--help"], ("--no-session", "--mode"))

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return [self.bin, self._RESUME_FLAG, session, "-p", "--mode", "json",
                *self._permission_flags(launch),
                *self._CONTEXT_FLAGS,
                *self._offline_flags(web_access=web_access),
                *self._provider_model_flags(), prompt]

    @staticmethod
    def _message_text(message: dict) -> str:
        """Concatenated text blocks of one message's content[] (thinking blocks
        are reasoning, not the answer text)."""
        out: list[str] = []
        for block in (message.get("content") or []):
            if isinstance(block, dict) and block.get("type") == "text":
                out.append(str(block.get("text") or ""))
        return "".join(out)

    @staticmethod
    def _usage_tokens(usage: dict) -> tuple[Optional[int], Optional[int]]:
        """pi/omp assistant-message usage → (input, output) tokens. Shape:
        {input, output, cacheRead, cacheWrite, totalTokens, cost:{...}}. Input
        counts the fresh + both cache buckets (same convention as claude/cursor).
        None when the block is absent."""
        if not isinstance(usage, dict) or not usage:
            return None, None
        inp = (int(usage.get("input") or 0)
               + int(usage.get("cacheRead") or 0)
               + int(usage.get("cacheWrite") or 0))
        outp = int(usage.get("output") or 0)
        return inp, outp

    def parse(self, stdout: str, stderr: str) -> CliResult:
        # --mode json streams JSONL events. The final assistant text is the LAST
        # assistant message_end (fallback: the last assistant message inside
        # agent_end.messages); usage/cost ride on that same message. pi json mode
        # exits 0 even when every turn errored, and a killed worker leaves a
        # partial stream — so parse whatever events exist regardless of exit code.
        text, session, cost = "", None, None
        explicit_error = ""
        inp = outp = None
        turns = 0
        saw_assistant = False
        agent_end_text = ""
        usage_messages: dict[str, dict] = {}
        def remember(message):
            # Native timestamp identifies the same message in message_end and agent_end.
            key = str(message.get("id") or message.get("timestamp") or json.dumps(message, sort_keys=True))
            usage_messages[key] = message.get("usage") or {}
        for line in stdout.splitlines():
            line = line.strip()
            if not line or not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            et = ev.get("type")
            if ev.get("errorMessage"):
                explicit_error = str(ev.get("errorMessage") or "")
            stop_reason = str(ev.get("stopReason") or "").lower()
            if stop_reason in {"error", "failed", "refusal"} and not explicit_error:
                explicit_error = str(ev.get("error") or stop_reason)
            if et == "session" and ev.get("id"):
                session = str(ev["id"])
            elif et == "message_end":
                msg = ev.get("message") or {}
                if not isinstance(msg, dict) or msg.get("role") != "assistant":
                    continue
                message_error = str(msg.get("errorMessage") or "").strip()
                message_stop = str(msg.get("stopReason") or "").lower()
                if message_error or message_stop in {"error", "failed", "refusal"}:
                    explicit_error = message_error or message_stop
                else:
                    # Pi writes every failed provider attempt and the eventual
                    # successful retry into one JSONL stream.  The last
                    # assistant message is authoritative for the completed
                    # turn, so a successful retry clears the earlier attempt's
                    # transient error.
                    explicit_error = ""
                saw_assistant = True
                remember(msg)
                text = self._message_text(msg)
                inp, outp = self._usage_tokens(msg.get("usage") or {})
                c = (msg.get("usage") or {}).get("cost") or {}
                total = c.get("total") if isinstance(c, dict) else None
                # cost.total of 0 means "not priced" (error/free turn) — keep None.
                cost = float(total) if total else None
            elif et == "turn_end":
                turns += 1
            elif et == "agent_end":
                for message in ev.get("messages") or []:
                    if isinstance(message, dict) and message.get("role") == "assistant":
                        remember(message)
                for m in reversed(ev.get("messages") or []):
                    if isinstance(m, dict) and m.get("role") == "assistant":
                        agent_end_text = self._message_text(m)
                        if not saw_assistant:
                            # killed mid-stream before message_end: the agent_end
                            # assistant message is the best text/usage we have.
                            saw_assistant = True
                            if inp is None and outp is None:
                                inp, outp = self._usage_tokens(m.get("usage") or {})
                                c = (m.get("usage") or {}).get("cost") or {}
                                total = c.get("total") if isinstance(c, dict) else None
                                cost = float(total) if total else None
                        break
        if not text and agent_end_text:
            text = agent_end_text
        samples = list(usage_messages.values())
        pairs = [self._usage_tokens(u) for u in samples]
        inp = sum(v[0] or 0 for v in pairs) if any(v[0] is not None for v in pairs) else None
        outp = sum(v[1] or 0 for v in pairs) if any(v[1] is not None for v in pairs) else None
        costs = [(u.get("cost") or {}).get("total") for u in samples]
        cost = sum(c for c in costs if isinstance(c, (float, int))) if any(isinstance(c, (float, int)) for c in costs) else None
        final_error = (
            explicit_error if saw_assistant else _structured_cli_error(stdout)
        )
        return CliResult(text=text, session=session, cost_usd=cost,
                         cache_read_tokens=sum(u.get("cacheRead") or 0 for u in samples) if samples else None,
                         cache_write_tokens=sum(u.get("cacheWrite") or 0 for u in samples) if samples else None,
                         input_tokens=inp, output_tokens=outp,
                         num_turns=turns or (1 if saw_assistant else None),
                         raw_stderr=stderr[-2000:], error=final_error)

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        line = line.strip()
        if not line or not line.startswith("{"):
            return []
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            return []
        t = ev.get("type")
        if t == "session" and ev.get("id"):
            return [StreamStep("session", session=str(ev["id"]))]
        if t in {"auto_retry_start", "auto_retry_end"}:
            if t == "auto_retry_start":
                detail = (
                    f"Pi model retry {int(ev.get('attempt') or 0)}/"
                    f"{int(ev.get('maxAttempts') or 0)} in "
                    f"{int(ev.get('delayMs') or 0)}ms: "
                    f"{str(ev.get('errorMessage') or '')}"
                )
            else:
                detail = (
                    f"Pi model retry ended: attempt="
                    f"{int(ev.get('attempt') or 0)}, success="
                    f"{bool(ev.get('success'))}, error="
                    f"{str(ev.get('finalError') or '')}"
                )
            return [StreamStep("runtime_warning", text=detail, raw=line)]
        if t == "message_update":
            # assistantMessageEvent carries the streaming deltas (partial stripped).
            ame = ev.get("assistantMessageEvent") or {}
            if not isinstance(ame, dict):
                return []
            if ame.get("type") in ("text_delta", "thinking_delta"):
                delta = str(ame.get("delta") or "")
                if delta.strip():
                    return [StreamStep(
                        "reasoning", text=delta,
                        thinking=ame.get("type") == "thinking_delta")]
            return []
        if t == "message_end":
            msg = ev.get("message") or {}
            if isinstance(msg, dict) and msg.get("role") == "assistant":
                txt = self._message_text(msg).strip()
                if txt:
                    return [StreamStep("reasoning", text=txt)]
            return []
        if t == "tool_execution_start":
            args = ev.get("args")
            arg = ""
            if isinstance(args, dict):
                arg = str(args.get("command") or args.get("path")
                          or args.get("query") or "")[:300]
                if not arg and args:
                    arg = json.dumps(args, ensure_ascii=False)[:300]
            return [StreamStep(
                "tool", tool=str(ev.get("toolName") or ""), text=arg,
                call_id=str(ev.get("toolCallId") or ""))]
        if t == "tool_execution_end":
            res = ev.get("result")
            if isinstance(res, str):
                full = res
            elif isinstance(res, dict) and isinstance(res.get("content"), list):
                # pi tool results are MCP-shaped: {content:[{type:"text",...}]}.
                full = "".join(
                    str(b.get("text") or "")
                    for b in res["content"]
                    if isinstance(b, dict) and b.get("type") == "text"
                ) or json.dumps(res, ensure_ascii=False)
            else:
                full = json.dumps(res, ensure_ascii=False) if res is not None else ""
            if ev.get("isError"):
                full = f"[error] {full}" if full else "[error]"
            # text=truncated for the deck; raw=full for the provenance gate.
            return [StreamStep(
                "tool_result", text=full[:600], raw=full,
                call_id=str(ev.get("toolCallId") or ""))]
        # Unknown/extra event types (omp adds more) are ignored by design.
        return []

    def _hello_argv(self) -> list[str]:
        # one headless JSONL turn — symmetric with claude/codex/cursor so the
        # self-check actually exercises auth/quota, not just `--version`.
        return [self.bin, "-p", "--mode", "json", "--no-session",
                *self._provider_model_flags(), self.HELLO_PROMPT]

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        # pi json mode exits 0 even when the turn errored. Require actual assistant
        # text instead of accepting an empty agent_end/message_end envelope or a
        # lone agent_settled event: the startup readiness contract is "the model
        # answered", not "the CLI exited".
        if r.returncode != 0:
            return False
        parsed = self.parse(r.stdout or "", r.stderr or "")
        return bool(parsed.text.strip()) and not parsed.error


class PiDriver(PiLikeDriver):
    """`pi -p --mode json` — the minimal pi coding agent. Offline-safe by design
    (built-in tools are read/bash/edit/write/grep/find/ls — no web tools)."""
    name = "pi"
    _CONTEXT_FLAGS = (
        "--no-skills",
        "--no-context-files",
        "--system-prompt",
        "You are an autonomous agent. Use the available tools to complete the assigned task.",
    )
    _ENV_EXTRA = {"PI_OFFLINE": "1", "PI_SKIP_VERSION_CHECK": "1"}

    def native_permission_modes(self) -> tuple[str, ...]:
        # Pi has no tool-operation permission modes. Project-trust --approve
        # exists only on pi ≥0.84; 0.73.1 rejects it with Unknown option.
        return ()

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        self.validate_launch_context(launch)
        # Do not pass --approve: unsupported on pi 0.73.1 (chat send path).
        # Project skills/extensions, when needed, are loaded via explicit
        # --extension / --skill rather than project-trust override.
        return []
