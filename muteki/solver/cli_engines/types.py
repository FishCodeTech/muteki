"""CLI driver result and launch types. Moved from cli_driver.py."""
from __future__ import annotations

import json
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Optional


class LaunchPurpose(str, Enum):
    """Why a CLI process is being launched."""

    WORKER = "worker"
    CONVERSATION = "conversation"
    MANAGEMENT = "management"


@dataclass(frozen=True)
class LaunchContext:
    """Launch-time policy input shared by every CLI driver.

    ``permission_mode`` is an opaque identifier owned by the selected Agent.
    An empty value means that interactive launches inherit the Agent default.
    """

    purpose: LaunchPurpose = LaunchPurpose.WORKER
    permission_mode: str = ""
    sandbox_mode: str = ""

    @property
    def interactive(self) -> bool:
        return self.purpose in {
            LaunchPurpose.CONVERSATION,
            LaunchPurpose.MANAGEMENT,
        }


WORKER_LAUNCH = LaunchContext()

@dataclass
class CliResult:
    """One CLI run's outcome, normalized across engines."""
    text: str                       # the agent's final response / transcript tail
    session: Optional[str] = None   # session id, for a resume/conclude turn
    cost_usd: Optional[float] = None
    # token usage for this run, when the engine reports it. None == not reported.
    # claude exposes it via the result `usage` block; codex via turn.completed
    # `usage`. Fed to the cost ledger so the deck can show a token-usage column
    # alongside the $ figure (and so codex — which no longer reports a dollar
    # cost — still gets priced from its tokens). cursor reports neither.
    input_tokens: Optional[int] = None
    output_tokens: Optional[int] = None
    cache_read_tokens: Optional[int] = None
    cache_write_tokens: Optional[int] = None
    reasoning_tokens: Optional[int] = None
    usage_estimated: bool = False
    cost_estimated: bool = False
    usage_id: str = field(default_factory=lambda: __import__("uuid").uuid4().hex)
    num_turns: Optional[int] = None
    elapsed_s: float = 0.0
    # Real subprocess exit status. Parsers normalize vendor output and cannot infer
    # process success from response text alone, so execution layers attach this
    # after the child exits. None means the runner could not observe an exit code.
    returncode: Optional[int] = None
    timed_out: bool = False
    # OOM-killed: the worker's process was SIGKILL'd by the kernel out-of-memory
    # killer (cgroup memory pressure; containers should also carry --memory from
    # worker_memory). This looks IDENTICAL to a wall-clock timeout by
    # exit code alone (the in-container `timeout` wrapper propagates 128+9=137 for
    # BOTH a real timeout AND a SIGKILL'd child), so we discriminate by the cgroup
    # oom_kill counter delta and surface it as its OWN reason — a worker that died
    # at 60s with an empty transcript is an OOM victim, NOT a 2400s timeout, and
    # mislabeling it as "timeout" sent diagnosis down the wrong path.
    oom_killed: bool = False
    # Stream capture exceeded the configured stdout/stderr budget (#170).
    output_limit: bool = False
    # Workdir growth exceeded the configured disk budget (#170).
    disk_limit: bool = False
    cancelled: bool = False         # killed by a cancel_event (winner found / abort)
    steered: bool = False           # ended early by a steer_event — END THIS PASS but
    #   KEEP the session id so the forced checkpoint can resume it with updated
    #   guidance. Distinct from `cancelled` (= retire without a resume).
    finished: bool = False          # commit-step ended this Worker normally
    raw_stderr: str = ""
    # Vendor/protocol failure detail.  Diagnostics stay diagnostics and are never
    # copied into ``text`` as a fabricated assistant response.
    error: str = ""
    runtime_status: dict = field(default_factory=dict)


def _structured_cli_error(stdout: str) -> str:
    """Extract an explicit vendor error from JSON/JSONL output.

    Several CLIs exit zero after reporting a model/provider error in-band.  Only
    error-shaped envelopes are inspected; ordinary assistant ``message`` fields
    are deliberately ignored.
    """
    documents: list[Any] = []
    try:
        documents.append(json.loads(stdout))
    except (json.JSONDecodeError, TypeError):
        for line in stdout.splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                documents.append(json.loads(line))
            except json.JSONDecodeError:
                continue

    def detail(value: Any) -> str:
        if isinstance(value, str):
            return value.strip()
        if isinstance(value, dict):
            for key in ("message", "detail", "errorMessage", "reason", "code"):
                text = detail(value.get(key))
                if text:
                    return text
            return json.dumps(value, ensure_ascii=False)[:1000]
        if isinstance(value, list):
            return "; ".join(filter(None, (detail(item) for item in value)))[:1000]
        return str(value).strip() if value is not None else ""

    for item in documents:
        if not isinstance(item, dict):
            continue
        if item.get("errorMessage"):
            return detail(item["errorMessage"])
        stop_reason = str(item.get("stopReason") or item.get("stop_reason") or "").lower()
        event_type = str(item.get("type") or "").lower()
        status = str(item.get("status") or "").lower()
        turn = item.get("turn") if isinstance(item.get("turn"), dict) else {}
        turn_status = str(turn.get("status") or "").lower()
        failed = (
            bool(item.get("is_error"))
            or stop_reason in {"error", "failed", "refusal"}
            or status in {"error", "failed"}
            or turn_status in {"error", "failed", "interrupted"}
            or event_type in {"error", "turn.failed", "turn/error", "fatal"}
            or event_type.endswith(".error")
        )
        if not failed:
            continue
        for value in (
            item.get("error"), turn.get("error"), item.get("result"),
            item.get("message"), item.get("reason"), stop_reason,
        ):
            text = detail(value)
            if text:
                return text
        return f"CLI reported {event_type or status or turn_status or 'failure'}"
    return ""


def _cli_stderr_failure_detail(stderr: str) -> str:
    """Pick a useful stderr tail, dropping launcher update chatter.

    Host wrappers such as clawgod print ``[clawgod] vX available … run 'claude
    update'`` on every invocation.  That banner is not the turn failure and must
    not replace a structured model/provider error from stdout.
    """
    kept: list[str] = []
    for line in (stderr or "").splitlines():
        text = line.strip()
        if not text:
            continue
        lower = text.lower()
        if "available (installed:" in lower and "update" in lower:
            continue
        if lower.startswith("[clawgod]") and "update" in lower:
            continue
        if "run 'claude update'" in lower or 'run "claude update"' in lower:
            continue
        kept.append(text)
    return "\n".join(kept[-12:])[-1800:]


def finalize_cli_result(
    result: CliResult,
    *,
    driver_name: str,
    stdout: str,
    stderr: str,
    returncode: Optional[int],
) -> CliResult:
    """Attach the process/protocol outcome without manufacturing reply text."""
    result.returncode = returncode
    if not result.error:
        result.error = _structured_cli_error(stdout)
    if returncode not in (None, 0) and not (
        result.cancelled or result.steered or result.timed_out or result.oom_killed or getattr(result, "output_limit", False) or getattr(result, "disk_limit", False)
    ):
        # Prefer the in-band / structured error.  Only fall back to stderr when
        # stdout did not explain the failure (and after dropping update noise).
        if not result.error:
            tail = _cli_stderr_failure_detail(stderr)
            result.error = tail or f"{driver_name} exited with code {returncode}"
    if not (result.text or "").strip() and not result.error and not (
        result.cancelled or result.steered or result.timed_out or result.oom_killed or getattr(result, "output_limit", False) or getattr(result, "disk_limit", False)
    ):
        result.error = f"{driver_name} turn ended without assistant text"
    return result


@dataclass
class StreamStep:
    """One live step parsed from a streaming CLI line — so the deck can show the
    worker thinking/acting in real time instead of a dead pause until it returns.

    kind:
      "reasoning"    — the agent's prose/thought (text block)        → REASONING_DELTA
      "tool"         — a tool/command the agent invoked              → TOOL_CALL
      "tool_result"  — that tool's output                            → TERMINAL_OUTPUT
      "session"      — the engine assigned/echoed a session id
      "runtime_warning" — native retry/runtime diagnostic             → RUNTIME_WARNING
    """
    kind: str
    text: str = ""
    tool: str = ""        # tool name (kind == "tool")
    session: str = ""     # session id (kind == "session")
    # FULL, UNTRUNCATED tool output (kind == "tool_result"). `text` is truncated to
    # 600 chars for the live deck display, but a flag/fact provenance gate MUST see
    # what the command actually printed — a flag past char 600 of a command's output
    # (or in a nested `ssh host '...'` whose remote stdout is forwarded here) is real
    # but invisible in `text` (run-75379 false-negative: the genuine DC flag04 was
    # read on a pivoted host, its output never landed in the truncated chunk or the
    # summarized CliResult.text). Empty for non-tool_result steps; callers fall back
    # to `text` when `raw` is unset.
    raw: str = ""         # untruncated tool output (kind == "tool_result")
    call_id: str = ""     # pairs tool with tool_result when the engine exposes one
    # Cursor can replace large shell output with an outputLocation pointer. Keep it
    # metadata-only here: the pure driver must not read an engine-supplied path, and
    # path text must never masquerade as command output. CliSolver validates and reads
    # it relative to the active worker cwd, where local/container topology is known.
    spill_path: str = ""
    spill_size_bytes: int = -1
    spill_line_count: int = -1
    # True for Runtime-provided thinking/reasoning blocks. Conversation maps these
    # onto its dedicated reasoning stream; they remain excluded from the answer
    # accumulator because message_end snapshots repeat only the answer text.
    thinking: bool = False


class SecurePromptUnsupported(RuntimeError):
    """The selected CLI cannot accept a secret prompt without argv/disk exposure."""
