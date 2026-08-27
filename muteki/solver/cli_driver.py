"""Shelled-CLI worker drivers — claude / codex as full agentic executors.

Why: the local DeepSeek code-driven kernel (one run_python tool-call per step)
lacks the execute→observe→refine depth to actually land an exploit. EXP-AB proved
a shelled `claude -p` solves challenges the code-driven swarm misses, and its flag
still passes the real provenance gate. So we delegate a focused intent to a CLI
agent that runs its OWN shell loop, and gate its output exactly as before.

Each driver is a thin per-CLI adapter: it builds the argv + manages a session id so
the single conclude-fallback turn (on a timeout) can resume the SAME session — there
is no multi-turn resume loop; a worker runs one execute pass and is then discarded.
We run bare-host against the
SUBSCRIPTION CLIs (full-strength model — the reason it solves). codex is included
but may be usage-limited; the swarm degrades to claude-only when a driver's
healthcheck fails.

This module is pure (builds argv + parses output); the solver runs the subprocess.
"""

from __future__ import annotations

import abc
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import uuid
from datetime import datetime, timezone
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Optional

from muteki.core.cost import PRICES, CODEX_CACHED_INPUT_PER_M, _DEFAULT_PRICE
from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    normalize_reasoning_effort,
    profile_uses_endpoint,
)


# ── engine binary resolution ─────────────────────────────────────────────────
# A worker shells `subprocess.run(["claude", ...])`, which resolves the FIRST
# `claude` on PATH. On this host (and easily on others) that can be a BROKEN
# third-party repackage — e.g. `@cometix/claude-code`, a Node "restored" build
# that crashes at parse time (`SyntaxError: Unexpected identifier`) under an
# older Node, never reaching the CLI. A worker pointed at it dies before it can
# solve, and the healthcheck just sees a non-zero exit and silently degrades the
# swarm. So we DON'T trust bare PATH order: resolve each engine to a real,
# runnable OFFICIAL binary and pin it.
#
# Precedence:
#   1. explicit override  — env MUTEKI_CLAUDE_BIN / MUTEKI_CODEX_BIN (operator wins)
#   2. known official install locations, in order
#   3. every `name` on PATH, skipping ones whose realpath looks like a known
#      bad repackage (cometix), taking the first that actually runs
#   4. bare `name` as a last resort (preserves old behavior if nothing else found)
_ENV_OVERRIDE = {
    "claude": "MUTEKI_CLAUDE_BIN",
    "codex": "MUTEKI_CODEX_BIN",
    "cursor": "MUTEKI_CURSOR_BIN",
    "pi": "MUTEKI_PI_BIN",
    "omp": "MUTEKI_OMP_BIN",
    "kimi": "MUTEKI_KIMI_BIN",
    "grok": "MUTEKI_GROK_BIN",
    "opencode": "MUTEKI_OPENCODE_BIN",
    "dsh": "MUTEKI_DSH_PYTHON",
}

# The on-disk binary basename for an engine, when it differs from the engine
# `name` we use everywhere else. Cursor's headless CLI ships as `cursor-agent`
# (the bare `cursor` launcher opens the GUI / is a different tool), so a PATH
# scan for the engine "cursor" must actually look for `cursor-agent`.
_BIN_NAME = {"cursor": "cursor-agent"}

# Official / first-party install locations we trust, highest first. `~` expanded
# at resolve time. The local native installer and Homebrew cask are the two
# blessed macOS paths; /usr/local/bin covers a plain npm global on Linux.
_KNOWN_GOOD = {
    "claude": [
        "~/.local/bin/claude",
        "/opt/homebrew/bin/claude",
        "/usr/local/bin/claude",
    ],
    "codex": [
        "~/.local/bin/codex",
        "/opt/homebrew/bin/codex",
        "/usr/local/bin/codex",
    ],
    "cursor": [
        "~/.local/bin/cursor-agent",
        "/opt/homebrew/bin/cursor-agent",
        "/usr/local/bin/cursor-agent",
    ],
    "pi": [
        "~/.local/bin/pi",
        "/opt/homebrew/bin/pi",
        "/usr/local/bin/pi",
    ],
    # omp is bun-based; the omp.sh installer lands in ~/.bun/bin by default.
    "omp": [
        "~/.bun/bin/omp",
        "~/.local/bin/omp",
        "/opt/homebrew/bin/omp",
        "/usr/local/bin/omp",
    ],
    "kimi": [
        "~/.kimi-code/bin/kimi",
        "~/.local/bin/kimi",
        "/opt/homebrew/bin/kimi",
        "/usr/local/bin/kimi",
    ],
    "grok": [
        "~/.grok/bin/grok",
        "~/.local/bin/grok",
        "/opt/homebrew/bin/grok",
        "/usr/local/bin/grok",
    ],
    "opencode": [
        "~/.local/bin/opencode",
        "/opt/homebrew/bin/opencode",
        "/usr/local/bin/opencode",
    ],
    # The DeepSeek Harness transport is a Python SDK bridge.  The interpreter is
    # still resolved through the common driver path so local/container execution
    # and health probes use the same argv contract.
    "dsh": [sys.executable],
}

# realpath substrings that mark a KNOWN-BAD repackage we must never select.
_BAD_REALPATH_MARKERS = ("@cometix", "cometix")

# Optional knowledge-base MCP. Muteki can let a worker query a KB MCP (your own
# security-intel / CVE / writeup index) as a first-class tool. There is no bundled
# KB service — set MUTEKI_KB_MCP_NAME to the server key from your .mcp.json (and
# enable kb on the run) to use one. Empty (the default) means "no KB", so the
# whole KB path is inert out of the box.
KB_MCP_NAME = os.environ.get("MUTEKI_KB_MCP_NAME", "").strip()


def _looks_bad(path: str) -> bool:
    try:
        real = os.path.realpath(path)
    except OSError:
        real = path
    low = real.lower()
    return any(m in low for m in _BAD_REALPATH_MARKERS)


def _runs_ok(path: str) -> bool:
    """Does this binary actually execute (vs crash at load like the cometix build)?
    `--version` is the cheapest probe that distinguishes a real CLI from a binary
    that dies before parsing argv."""
    try:
        r = subprocess.run([path, "--version"], capture_output=True, text=True, encoding="utf-8", errors="replace",
                           timeout=20)
        return r.returncode == 0
    except Exception:
        return False


def resolve_engine_bin(name: str) -> str:
    """Resolve an engine name to a pinned, runnable binary path (see precedence
    above). Falls back to the bare name so callers always get *something*."""
    # 1. operator override — trusted as-is (don't second-guess an explicit path)
    env = _ENV_OVERRIDE.get(name)
    if env and os.environ.get(env):
        return os.path.expanduser(os.environ[env])

    # 2. known-good install locations
    for cand in _KNOWN_GOOD.get(name, []):
        p = os.path.expanduser(cand)
        if Path(p).exists() and not _looks_bad(p) and _runs_ok(p):
            return p

    # 3. PATH scan, skipping known-bad repackages, first that runs wins. The
    #    on-disk basename may differ from the engine name (cursor → cursor-agent).
    bin_basename = _BIN_NAME.get(name, name)
    for p in _which_all(bin_basename):
        if not _looks_bad(p) and _runs_ok(p):
            return p

    # 4. last resort — bare basename (old behavior). If everything is broken we
    #    at least fail the same way we used to, not worse.
    return bin_basename


def resolve_engine_bin_source(name: str) -> str:
    """Where would resolve_engine_bin() get this engine's binary from?

    Returns one of: "env" (explicit MUTEKI_*_BIN override), "known-good" (a
    blessed install location), "path" (a PATH scan hit), or "fallback" (nothing
    found — bare name). Drives the FE's "you're on an unpinned default path,
    consider setting MUTEKI_<ENGINE>_BIN" guidance for local mode.
    """
    env = _ENV_OVERRIDE.get(name)
    if env and os.environ.get(env):
        return "env"
    for cand in _KNOWN_GOOD.get(name, []):
        p = os.path.expanduser(cand)
        if Path(p).exists() and not _looks_bad(p) and _runs_ok(p):
            return "known-good"
    bin_basename = _BIN_NAME.get(name, name)
    for p in _which_all(bin_basename):
        if not _looks_bad(p) and _runs_ok(p):
            return "path"
    return "fallback"


def _which_all(name: str) -> list[str]:
    """Every `name` found on PATH, in PATH order (shutil.which only returns one)."""
    out: list[str] = []
    seen: set[str] = set()
    for d in (os.environ.get("PATH") or "").split(os.pathsep):
        if not d:
            continue
        cand = os.path.join(d, name)
        if cand not in seen and os.path.isfile(cand) and os.access(cand, os.X_OK):
            seen.add(cand)
            out.append(cand)
    # also let shutil.which have a say (handles PATHEXT etc.) as a backstop
    w = shutil.which(name)
    if w and w not in seen:
        out.append(w)
    return out


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
    num_turns: Optional[int] = None
    elapsed_s: float = 0.0
    # Real subprocess exit status. Parsers normalize vendor output and cannot infer
    # process success from response text alone, so execution layers attach this
    # after the child exits. None means the runner could not observe an exit code.
    returncode: Optional[int] = None
    timed_out: bool = False
    # OOM-killed: the worker's process was SIGKILL'd by the kernel out-of-memory
    # killer (a sibling run's container ballooned and starved the Docker VM — no
    # per-container --memory limit). This looks IDENTICAL to a wall-clock timeout by
    # exit code alone (the in-container `timeout` wrapper propagates 128+9=137 for
    # BOTH a real timeout AND a SIGKILL'd child), so we discriminate by the cgroup
    # oom_kill counter delta and surface it as its OWN reason — a worker that died
    # at 60s with an empty transcript is an OOM victim, NOT a 2400s timeout, and
    # mislabeling it as "timeout" sent diagnosis down the wrong path.
    oom_killed: bool = False
    cancelled: bool = False         # killed by a cancel_event (winner found / abort)
    steered: bool = False           # ended early by a steer_event — END THIS PASS but
    #   KEEP the session id (operator hint/redirect). The worker does NOT resume on
    #   steered (no resume loop under single-shot); the guidance flows to the next
    #   spawned worker. Used only to avoid downgrading _session_established on the cut
    #   pass. Distinct from `cancelled` (= die).
    raw_stderr: str = ""
    runtime_status: dict = field(default_factory=dict)


@dataclass
class StreamStep:
    """One live step parsed from a streaming CLI line — so the deck can show the
    worker thinking/acting in real time instead of a dead pause until it returns.

    kind:
      "reasoning"    — the agent's prose/thought (text block)        → REASONING_DELTA
      "tool"         — a tool/command the agent invoked              → TOOL_CALL
      "tool_result"  — that tool's output                            → TERMINAL_OUTPUT
      "session"      — the engine assigned/echoed a session id
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
    # True for thinking-block deltas (kind == "reasoning"). Thinking is streamed
    # for the live deck but excluded from the replay-seal accumulator: Pi/OMP's
    # message_end snapshot repeats only the answer text, so mixing thinking into
    # the accumulator would make that snapshot a suffix the seal check misses.
    thinking: bool = False


class SecurePromptUnsupported(RuntimeError):
    """The selected CLI cannot accept a secret prompt without argv/disk exposure."""


_SECURE_HELP_CACHE: "dict[tuple[str, int, tuple[str, ...], tuple[str, ...]], tuple[bool, str]]" = {}
_SECURE_HELP_LOCK = threading.Lock()


def _secure_help_preflight(
    binary: str, help_args: list[str], required: tuple[str, ...],
) -> "tuple[bool, str]":
    """Verify the installed CLI advertises every flag/pipe semantic we rely on.

    This sends no model prompt and uses no credentials. The cache key includes the
    resolved binary mtime, so replacing/upgrading a CLI automatically revalidates it.
    """
    resolved = os.path.realpath(binary)
    try:
        mtime_ns = int(os.stat(resolved).st_mtime_ns)
    except OSError:
        mtime_ns = 0
    key = (resolved, mtime_ns, tuple(help_args), required)
    with _SECURE_HELP_LOCK:
        cached = _SECURE_HELP_CACHE.get(key)
    if cached is not None:
        return cached
    try:
        proc = subprocess.run(
            [binary, *help_args], capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=20,
            stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired) as exc:
        result = False, f"secure prompt capability probe failed: {exc}"
    else:
        help_text = f"{proc.stdout or ''}\n{proc.stderr or ''}".lower()
        missing = [token for token in required if token.lower() not in help_text]
        if proc.returncode != 0:
            result = False, f"secure prompt capability probe exited {proc.returncode}"
        elif missing:
            result = False, "secure prompt flags unavailable: " + ", ".join(missing)
        else:
            result = True, ""
    with _SECURE_HELP_LOCK:
        _SECURE_HELP_CACHE[key] = result
    return result


def _redact_probe_secrets(detail: str, env: "dict[str, str] | None") -> str:
    """Remove credential values if a CLI mirrors them in an error message."""
    out = str(detail or "")
    for key, raw in (env or {}).items():
        upper = str(key).upper()
        if not any(marker in upper for marker in ("KEY", "TOKEN", "SECRET", "PASSWORD")):
            continue
        value = str(raw or "")
        if len(value) >= 4:
            out = out.replace(value, "<redacted>")
    return out


class CliDriver(abc.ABC):
    """A thin per-CLI shelled-executor adapter."""
    name: str
    # Scheduler-visible capability: exact secret context may only select drivers
    # that guarantee stdin transport AND non-persistent CLI state.
    secure_prompt_transport = False
    # Scheduler-visible capability: ``web_access=False`` is meaningful only when
    # this transport can make the worker's native web tools unavailable.  Keep
    # this separate from endpoint choice: a Claude CLI pointed at an Anthropic-
    # compatible model endpoint still owns exactly the same local tool surface.
    offline_web_isolation = False

    # resolved once, then cached — the actual binary this driver invokes. We pin
    # to a runnable OFFICIAL install instead of bare `self.name` so a broken
    # third-party `claude` earlier on PATH can't silently take over (see
    # resolve_engine_bin). Override via MUTEKI_CLAUDE_BIN / MUTEKI_CODEX_BIN.
    _bin: Optional[str] = None

    @property
    def bin(self) -> str:
        override = os.environ.get(_ENV_OVERRIDE.get(self.name, ""), "").strip()
        if override:
            return override
        if self._bin is None:
            self._bin = resolve_engine_bin(self.name)
        return self._bin

    def new_session(self) -> Optional[str]:
        """A pre-seeded session id, or None if the engine assigns one itself."""
        return None

    def build_execute_stdin(
        self,
        prompt: str,
        session: Optional[str],
        *,
        web_access: bool = True,
        kb_access: bool = True,
        stream: bool = False,
    ) -> list[str]:
        """Build a fresh, non-persistent invocation whose prompt is read from stdin.

        This is deliberately a separate capability from ``build_execute``.  Exact
        operator secrets must never be smuggled through a positional argument (where
        sibling processes can read them from the process table), and the engine must
        offer a documented way to avoid persisting the resulting conversation.  A
        driver that cannot satisfy both requirements fails closed.
        """
        del prompt, session, web_access, kb_access, stream
        raise SecurePromptUnsupported(
            f"{self.name} does not support non-persistent stdin prompt delivery")

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        """Capability-neutral local check for the exact-secret CLI contract."""
        return False, f"{self.name} has no secure stdin prompt capability"

    def env_extra(self) -> "dict[str, str]":
        """Engine-specific default env for every worker run (merged UNDER any
        credential overlay, so explicit account/env values always win). Default:
        nothing. pi/omp use this to pin their offline/no-setup toggles."""
        return {}

    # The optional KB MCP (if configured via MUTEKI_KB_MCP_NAME) is registered at
    # user scope and inherited by every worker; to run a worker WITHOUT it we deny
    # its mcp tools by server prefix. Empty name → no prefix → nothing to deny.
    KB_TOOL_PREFIX = f"mcp__{KB_MCP_NAME}" if KB_MCP_NAME else ""

    @abc.abstractmethod
    def build_execute(
        self,
        prompt: str,
        session: Optional[str],
        *,
        web_access: bool = True,
        kb_access: bool = True,
        stream: bool = False,
    ) -> list[str]:
        """argv for a fresh focused run.

        web_access=False → strip the agent's internet tools (WebSearch/WebFetch)
        so a bench eval can't be contaminated by looking up a writeup.
        kb_access=False → deny the inherited optional KB MCP tools (default: the
        worker keeps the user-scope KB, if one is configured via
        MUTEKI_KB_MCP_NAME, and can dispatch to it).
        stream=True → emit one JSON event PER STEP (assistant text / tool call /
        tool result) as the run proceeds, so the deck shows live progress instead
        of a dead pause. parse_stream_line() turns each line into a StreamStep;
        parse() still produces the final CliResult from the accumulated stdout.
        """

    def parse_stream_line(self, line: str) -> Optional["StreamStep"]:
        """Turn ONE line of streaming stdout into a live StreamStep (or None to
        ignore it). Default: nothing streams. Overridden by streaming engines.

        Single-step view (the FIRST step of a line). Kept for callers/tests that want
        one representative step; the streaming runner uses parse_stream_steps() to get
        ALL steps so a multi-block message doesn't lose later blocks (#18)."""
        return None

    def parse_stream_steps(self, line: str) -> list["StreamStep"]:
        """ALL live StreamSteps a single line carries. A single assistant message can
        hold several content blocks (text + tool_use + more text); #18: returning only
        the FIRST block dropped any FOUND_FLAG / VERIFIED_FACT in a later block from
        LIVE propagation (it only resurfaced via the final parse()). Default: wrap the
        single-step parse_stream_line (correct for engines that emit at most one step
        per line, e.g. codex). claude + cursor override this to yield every block."""
        step = self.parse_stream_line(line)
        return [step] if step is not None else []

    @abc.abstractmethod
    def build_resume(
        self,
        prompt: str,
        session: str,
        *,
        web_access: bool = True,
        kb_access: bool = True,
        stream: bool = False,
    ) -> list[str]:
        """argv to resume `session` with a follow-up (conclude/refine) turn."""

    @abc.abstractmethod
    def parse(self, stdout: str, stderr: str) -> CliResult:
        """Normalize the engine's stdout into a CliResult."""

    # ── self-check (FE-healthcheck-page) ─────────────────────────────────────
    # The deep probe sends ONE tiny prompt and waits for the engine to answer —
    # this is what actually exercises auth/quota (a `--version` only proves the
    # binary unpacks). All three engines share the same shape via _hello_argv()
    # so the self-check is symmetric: claude no longer the only one that really
    # talks to its backend while codex/cursor merely checked a version string.
    HELLO_PROMPT = "Reply with exactly: OK"
    # DeepSeek-via-Anthropic cold turns (esp. v4 thinking) routinely exceed 30s;
    # a 60s single-shot false-fails the coordinator roster under load.
    _HELLO_TIMEOUT = 120
    _HELLO_RETRIES = 2

    def _hello_argv(self) -> list[str]:
        """argv for a minimal one-turn 'say hello' probe. Engines that can't run a
        real turn cheaply return [] (→ fall back to the `--version` liveness check)."""
        return []

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        """Did the hello turn actually produce a model reply? Default: exit 0 and
        SOME non-empty stdout. Engines with a structured envelope tighten this."""
        return r.returncode == 0 and bool((r.stdout or "").strip())

    def healthcheck(self, *, env: "dict[str, str] | None" = None) -> bool:
        """Cheap-but-real liveness probe — can this CLI complete a turn right now
        (auth + quota ok)? Returns bool for back-compat; health_detail() carries
        the human-readable reason."""
        # Only forward env when set, so a health_detail override/stub that predates
        # the env parameter (no **kwargs) still works through the bool entrypoint.
        if env is None:
            return self.health_detail()[0]
        return self.health_detail(env=env)[0]

    def health_detail(self, *, env: "dict[str, str] | None" = None) -> "tuple[bool, str]":
        """(healthy, detail). Sends a one-turn hello and retries once on a
        transient failure (a single cold/jittery miss shouldn't report red). The
        detail names the failure mode — timeout / non-zero exit / empty reply /
        not-found — so the self-check page can tell connectivity from auth/quota.

        `env`, when given, is the COMPLETE environment for the probe subprocess
        (callers build {**os.environ, **credential_overlay}). Passing it explicitly
        — instead of the old global os.environ overlay — is what makes concurrent
        probes safe: two engines probing in parallel no longer clobber each other's
        CURSOR_API_KEY/etc. None preserves the legacy inherit-os.environ behavior."""
        argv = self._hello_argv()
        if not argv:  # engine has no cheap dry-run → fall back to version liveness
            try:
                r = subprocess.run([self.bin, "--version"], capture_output=True,
                                   text=True, encoding="utf-8", errors="replace", timeout=20, env=env)
                if r.returncode == 0:
                    return True, ""
                return False, "binary not runnable (--version failed)"
            except FileNotFoundError:
                return False, "binary not found on PATH"
            except subprocess.TimeoutExpired:
                return False, "version probe timed out"
            except Exception as e:  # noqa: BLE001
                return False, str(e)[:160]

        # Health callers pass the resolved profile/account environment explicitly.
        # Apply it to argv here too, so Pi/OMP provider/model selection and the
        # other runtime options are identical to a live CliSolver invocation.
        argv = apply_runtime_argv(argv, driver=self, env=env or {})

        last = "no reply"
        for attempt in range(self._HELLO_RETRIES + 1):
            try:
                r = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", errors="replace",
                                   timeout=self._HELLO_TIMEOUT, env=env)
            except FileNotFoundError:
                return False, "binary not found on PATH"
            except subprocess.TimeoutExpired:
                last = f"hello probe timed out (>{self._HELLO_TIMEOUT}s)"
            except Exception as e:  # noqa: BLE001
                last = str(e)[:160]
            else:
                if self._hello_ok(r):
                    return True, ""
                # classify the miss so a retry/the operator knows what happened
                if r.returncode != 0:
                    failed_detail = ""
                    if '"type":"turn.failed"' in (r.stdout or ""):
                        for line in reversed((r.stdout or "").splitlines()):
                            try:
                                ev = json.loads(line)
                            except json.JSONDecodeError:
                                continue
                            if ev.get("type") != "turn.failed":
                                continue
                            err = ev.get("error") or {}
                            failed_detail = str(
                                err.get("message") if isinstance(err, dict) else err
                            )
                            break
                    detail_src = failed_detail or r.stderr or r.stdout or ""
                    tail = detail_src.strip().splitlines()
                    last = (f"hello exited {r.returncode}"
                            + (f": {tail[-1][:300]}" if tail else ""))
                else:
                    last = "hello returned no model reply"
            if attempt < self._HELLO_RETRIES:
                time.sleep(1.0)  # brief backoff, then one more shot
        return False, _redact_probe_secrets(last, env)


_FLAG_LINE = re.compile(r"FOUND_FLAG=\s*(\S+)")


class ClaudeCodeDriver(CliDriver):
    """`claude -p` — pre-seeds a uuid session; resumes with `-r`. Host CLI,
    --dangerously-skip-permissions (full shell), JSON output for clean parsing."""
    name = "claude"
    secure_prompt_transport = True
    offline_web_isolation = True

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
    ) -> list[str]:
        argv = [self.bin, "-p", *self._fmt(stream),
                "--dangerously-skip-permissions"]
        if session:
            argv += ["--session-id", session]
        argv += self._mcp_isolation(kb_access=kb_access)
        argv += self._denied(web_access=web_access, kb_access=kb_access)
        argv += ["--", prompt]
        return argv

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
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
            "--dangerously-skip-permissions",
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
    ) -> list[str]:
        return [self.bin, "-r", session, "-p", *self._fmt(stream),
                "--dangerously-skip-permissions",
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
        return (inp or None), (outp or None)

    def parse(self, stdout: str, stderr: str) -> CliResult:
        # Plain --output-format json: one JSON document.
        try:
            d = json.loads(stdout)
            inp, outp = self._usage_tokens(d.get("usage") or {})
            return CliResult(
                text=str(d.get("result", "")),
                session=d.get("session_id"),
                cost_usd=d.get("total_cost_usd"),
                input_tokens=inp,
                output_tokens=outp,
                num_turns=d.get("num_turns"),
                raw_stderr=stderr[-2000:],
            )
        except json.JSONDecodeError:
            pass
        # stream-json: many JSONL lines — the final {"type":"result",...} is the
        # outcome. Scan for it (and fall back to raw text if absent).
        result_text, session, cost, turns, inp, outp = "", None, None, None, None, None
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
                cost = ev.get("total_cost_usd")
                turns = ev.get("num_turns")
                inp, outp = self._usage_tokens(ev.get("usage") or {})
            if ev.get("type") == "assistant":
                msg = ev.get("message") or {}
                u = msg.get("usage")
                si, so = self._usage_tokens(u or {})
                if si is not None:
                    stream_in = si
                if so is not None:
                    stream_out = so
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
            outp = max(1, (assistant_chars // 4) + (tool_use_count * 32))
        if (inp is None or int(inp or 0) == 0) and outp:
            # Input is unknown; keep a conservative floor so cost.record can fire.
            inp = int(outp)
        if result_text or session or inp or outp:
            return CliResult(text=result_text, session=session, cost_usd=cost,
                             input_tokens=inp, output_tokens=outp,
                             num_turns=turns, raw_stderr=stderr[-2000:])
        return CliResult(text=stdout[-8000:], raw_stderr=stderr[-2000:])

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
        # exit 0 AND a result envelope — proves the turn round-tripped, not just
        # that the process started (a quota/auth refusal still exits with no result).
        return r.returncode == 0 and '"result"' in (r.stdout or "")


class CodexDriver(CliDriver):
    """`codex exec` — engine assigns the session (scraped from stderr 'session id:');
    resumes with `codex exec resume <id>`. May be usage-limited (degrade to claude)."""
    name = "codex"
    secure_prompt_transport = True
    offline_web_isolation = True
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
    ) -> list[str]:
        # `--json` already emits live per-step JSONL, so streaming needs no extra
        # flag — stream is accepted for interface parity.
        return [self.bin, *self._globals(web_access=web_access),
                "exec", *self._config_isolation(
                    web_access=web_access, kb_access=kb_access),
                "--json", "--dangerously-bypass-approvals-and-sandbox",
                "--", prompt]

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        # `codex exec -` is the documented stdin form; --ephemeral prevents session
        # files from being persisted. stream/session remain interface-only.
        del prompt, session, stream
        return [
            self.bin, *self._globals(web_access=web_access),
            "exec", *self._config_isolation(
                web_access=web_access, kb_access=kb_access),
            "--json", "--ephemeral",
            "--dangerously-bypass-approvals-and-sandbox", "-",
        ]

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return _secure_help_preflight(
            self.bin, ["exec", "--help"],
            ("--ephemeral", "read from stdin"))

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        return [self.bin, *self._globals(web_access=web_access),
                "exec", "resume", session,
                *self._config_isolation(
                    web_access=web_access, kb_access=kb_access), "--json",
                "--dangerously-bypass-approvals-and-sandbox",
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
        in_tok = cached_tok = out_tok = 0
        for line in stdout.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                text += line + "\n"      # tolerate non-JSON lines
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
                in_tok += int(u.get("input_tokens") or 0)
                cached_tok += int(u.get("cached_input_tokens") or 0)
                # reasoning tokens bill as output
                out_tok += int(u.get("output_tokens") or 0) + int(
                    u.get("reasoning_output_tokens") or 0)
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
        # Derive API-equivalent cost from tokens when codex didn't report a dollar
        # figure (the subscription path). `input_tokens` from codex INCLUDES the
        # cached portion, so split it: cached billed at the cheaper cached rate,
        # the rest at the full input rate; reasoning already folded into out_tok.
        if cost is None and (in_tok or out_tok):
            price = PRICES.get("codex", _DEFAULT_PRICE)
            fresh_in = max(0, in_tok - cached_tok)
            cost = (
                fresh_in / 1_000_000 * price.input_per_m
                + cached_tok / 1_000_000 * CODEX_CACHED_INPUT_PER_M
                + out_tok / 1_000_000 * price.output_per_m
            )
        if not text.strip() and not stdout.strip() and stderr.strip():
            tail = "\n".join(stderr.strip().splitlines()[-12:])[-1800:]
            text = f"[codex stderr]\n{tail}\n"
        return CliResult(text=text[-8000:] or stdout[-8000:], session=session,
                         cost_usd=cost, num_turns=turns or None,
                         input_tokens=(in_tok or None), output_tokens=(out_tok or None),
                         raw_stderr=stderr[-2000:])

    def _hello_argv(self) -> list[str]:
        # a real one-turn exec (offline, sandboxed) — symmetric with claude/cursor
        # so the self-check actually exercises codex auth, not just `--version`.
        return [self.bin, "exec", *self._config_isolation(
                    web_access=False, kb_access=False), "--json",
                "--dangerously-bypass-approvals-and-sandbox", "--", self.HELLO_PROMPT]

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        # codex --json streams JSONL. A completed model turn proves auth/quota and
        # the backend round-trip even when late MCP/plugin shutdown noise makes the
        # process exit non-zero after stdout already contains the successful turn.
        for line in (r.stdout or "").splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "turn.completed":
                return True
            if ev.get("type") == "item.completed":
                item = ev.get("item") or {}
                if isinstance(item, dict) and item.get("type") == "agent_message":
                    return True
        return False


class CursorDriver(CliDriver):
    """Cursor headless driver.

    Online runs keep Cursor's normal ``-p --force --trust`` path. Offline runs use
    the task-local ACP bridge, which approves local tools and rejects Cursor's
    native search/fetch permission requests without changing user configuration.
    """
    name = "cursor"
    offline_web_isolation = True
    # optional pinned model (e.g. "sonnet-4.5-thinking"); unset → cursor's default.
    _MODEL_ENV = "MUTEKI_CURSOR_MODEL"
    _OFFLINE_BRIDGE = Path(__file__).with_name("offline_acp_bridge.py")

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

    def _offline_argv(self, prompt: str, *, resume: str = "") -> list[str]:
        if not self._OFFLINE_BRIDGE.is_file():
            raise FileNotFoundError(
                f"Cursor offline ACP bridge missing: {self._OFFLINE_BRIDGE}")
        argv = [sys.executable, str(self._OFFLINE_BRIDGE),
                "--agent-bin", self.bin, "--agent-label", "cursor",
                *self._model()]
        if resume:
            argv += ["--resume", resume]
        argv += ["--", prompt]
        return argv

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        # -p (print/headless) + --force (run all commands) + --trust (skip the
        # workspace-trust prompt in headless mode). Prompt is the trailing POSITIONAL
        # arg (cursor has no `--` separator). cwd is the subprocess cwd, so no
        # explicit --workspace is needed (matches the claude/codex drivers).
        del session, kb_access
        if not web_access:
            return self._offline_argv(prompt)
        return [self.bin, "-p", *self._fmt(stream), "--force", "--trust",
                *self._model(), prompt]

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        # Cursor 2026.07's `commands/build-prompt.ts` does read stdin when headless,
        # stdin is non-TTY, and the positional prompt is empty.  It does *not* expose
        # a --no-session-persistence/--ephemeral equivalent, however: runChat always
        # creates a chat store under the Cursor state root.  Exact operator secrets
        # therefore fail closed instead of being written into that store.
        del prompt, session, web_access, kb_access, stream
        raise SecurePromptUnsupported(
            "cursor accepts headless prompts from stdin but cannot disable chat "
            "session persistence; exact secret context is unsupported")

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        del kb_access
        if not web_access:
            return self._offline_argv(prompt, resume=session)
        return [self.bin, "-p", *self._fmt(stream), "--force", "--trust",
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
        return (inp or None), (outp or None)

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
        # --output-format json: one JSON object {type:result, result, session_id, ...}
        try:
            d = json.loads(stdout)
            if isinstance(d, dict) and (d.get("type") == "result" or "result" in d):
                inp, outp = self._usage_tokens(d.get("usage") or {})
                return CliResult(
                    text=str(d.get("result", "")),
                    session=d.get("session_id"),
                    cost_usd=None,         # subscription-backed; no per-run cost
                    input_tokens=inp,
                    output_tokens=outp,
                    num_turns=None,
                    raw_stderr=stderr[-2000:],
                )
        except json.JSONDecodeError:
            pass
        # stream-json: NDJSON. The terminal {"type":"result",...} carries the full
        # text + usage; any line may carry session_id (system.init or result).
        result_text, session, inp, outp = "", None, None, None
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
                inp, outp = self._usage_tokens(ev.get("usage") or {})
            if ev.get("session_id"):
                session = ev["session_id"]
        if result_text or session:
            return CliResult(text=result_text, session=session, cost_usd=None,
                             input_tokens=inp, output_tokens=outp,
                             num_turns=None, raw_stderr=stderr[-2000:])
        return CliResult(text=stdout[-8000:], raw_stderr=stderr[-2000:])

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
        # Probe the stricter ACP path. A successful offline probe also proves the
        # online binary/auth path, while the reverse would leave ACP unverified.
        return self._offline_argv(self.HELLO_PROMPT)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        # exit 0 AND a result field — cursor's json envelope is {type:result,result,...}.
        return r.returncode == 0 and '"result"' in (r.stdout or "")


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
    uses a task-local ACP session with native web tools and MCP servers disabled.
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

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        # kb_access is a no-op (the optional KB MCP lives in claude's user config);
        # --mode json already streams per-step events, so stream needs no flag.
        return [self.bin, "-p", "--mode", "json",
                *self._offline_flags(web_access=web_access),
                *self._provider_model_flags(), prompt]

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        # Headless pi/omp read a missing positional prompt from stdin when it is
        # non-TTY; --no-session keeps the run ephemeral (never persisted to the
        # session store) — the two properties exact-secret delivery requires.
        del prompt, session, kb_access, stream
        return [self.bin, "-p", "--mode", "json", "--no-session",
                *self._offline_flags(web_access=web_access),
                *self._provider_model_flags()]

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return _secure_help_preflight(
            self.bin, ["--help"], ("--no-session", "--mode"))

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        return [self.bin, self._RESUME_FLAG, session, "-p", "--mode", "json",
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
        return (inp or None), (outp or None)

    def parse(self, stdout: str, stderr: str) -> CliResult:
        # --mode json streams JSONL events. The final assistant text is the LAST
        # assistant message_end (fallback: the last assistant message inside
        # agent_end.messages); usage/cost ride on that same message. pi json mode
        # exits 0 even when every turn errored, and a killed worker leaves a
        # partial stream — so parse whatever events exist regardless of exit code.
        text, session, cost = "", None, None
        inp = outp = None
        turns = 0
        saw_assistant = False
        agent_end_text = ""
        for line in stdout.splitlines():
            line = line.strip()
            if not line or not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            et = ev.get("type")
            if et == "session" and ev.get("id"):
                session = str(ev["id"])
            elif et == "message_end":
                msg = ev.get("message") or {}
                if not isinstance(msg, dict) or msg.get("role") != "assistant":
                    continue
                saw_assistant = True
                text = self._message_text(msg)
                inp, outp = self._usage_tokens(msg.get("usage") or {})
                c = (msg.get("usage") or {}).get("cost") or {}
                total = c.get("total") if isinstance(c, dict) else None
                # cost.total of 0 means "not priced" (error/free turn) — keep None.
                cost = float(total) if total else None
            elif et == "turn_end":
                turns += 1
            elif et == "agent_end":
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
        if text or session or inp or outp:
            return CliResult(text=text, session=session, cost_usd=cost,
                             input_tokens=inp, output_tokens=outp,
                             num_turns=turns or (1 if saw_assistant else None),
                             raw_stderr=stderr[-2000:])
        return CliResult(text=stdout[-8000:], raw_stderr=stderr[-2000:])

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
        # unknown/extra event types (omp adds more) are ignored by design.
        return []

    def _hello_argv(self) -> list[str]:
        # one headless JSONL turn — symmetric with claude/codex/cursor so the
        # self-check actually exercises auth/quota, not just `--version`.
        return [self.bin, "-p", "--mode", "json", "--no-session",
                *self._provider_model_flags(), self.HELLO_PROMPT]

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        # pi json mode exits 0 even when the turn errored. Require actual assistant
        # text instead of accepting an empty agent_end/message_end envelope: the
        # startup readiness contract is "the model answered", not "the CLI exited".
        for line in (r.stdout or "").splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            et = ev.get("type")
            if et == "agent_end":
                messages = ev.get("messages") or []
                if any(
                    isinstance(message, dict)
                    and message.get("role") == "assistant"
                    and self._message_text(message).strip()
                    for message in messages
                ):
                    return True
            if et == "message_end":
                msg = ev.get("message") or {}
                if (isinstance(msg, dict) and msg.get("role") == "assistant"
                        and self._message_text(msg).strip()):
                    return True
        return False


class PiDriver(PiLikeDriver):
    """`pi -p --mode json` — the minimal pi coding agent. Offline-safe by design
    (built-in tools are read/bash/edit/write/grep/find/ls — no web tools)."""
    name = "pi"
    _ENV_EXTRA = {"PI_OFFLINE": "1", "PI_SKIP_VERSION_CHECK": "1"}


class OhMyPiDriver(PiLikeDriver):
    """Oh My Pi driver.

    Online runs keep OMP's normal headless path. Offline runs use ACP with an
    empty MCP list plus a task-owned settings overlay that removes native search,
    URL fetch, and browser tools while retaining local tools and user Skills.
    """
    name = "omp"
    _MODEL_ENV = "MUTEKI_OMP_MODEL"
    _PROVIDER_ENV = "MUTEKI_OMP_PROVIDER"
    _RESUME_FLAG = "--resume"
    _ENV_EXTRA = {"OMP_SKIP_SETUP": "1"}
    _OFFLINE_BRIDGE = Path(__file__).with_name("offline_acp_bridge.py")
    _OFFLINE_CONFIG = Path(__file__).with_name("omp_offline_config.yml")

    def _offline_argv(self, prompt: str, *, resume: str = "") -> list[str]:
        for required in (self._OFFLINE_BRIDGE, self._OFFLINE_CONFIG):
            if not required.is_file():
                raise FileNotFoundError(f"OMP offline runtime file missing: {required}")
        argv = [
            sys.executable, str(self._OFFLINE_BRIDGE),
            "--agent-bin", self.bin,
            "--agent-label", "omp",
            "--agent-arg=--config",
            f"--agent-arg={self._OFFLINE_CONFIG}",
        ]
        if resume:
            argv += ["--resume", resume]
        argv += ["--", prompt]
        return argv

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        del session, kb_access, stream
        if not web_access:
            return self._offline_argv(prompt)
        return super().build_execute(prompt, None, web_access=True)

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        del kb_access, stream
        if not web_access:
            return self._offline_argv(prompt, resume=session)
        return super().build_resume(prompt, session, web_access=True)

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        if not web_access:
            raise SecurePromptUnsupported(
                "OMP offline isolation uses an ACP session, which persists prompts; "
                "exact secret context is unsupported")
        return super().build_execute_stdin(
            prompt, session, web_access=True, kb_access=kb_access, stream=stream)

    def parse(self, stdout: str, stderr: str) -> CliResult:
        if '"type":"system"' in stdout or '"type":"result"' in stdout:
            return CursorDriver.parse(self, stdout, stderr)
        return super().parse(stdout, stderr)

    _tool_summary = staticmethod(CursorDriver._tool_summary)

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        try:
            event_type = json.loads(line.strip()).get("type")
        except (json.JSONDecodeError, AttributeError, TypeError):
            event_type = None
        if event_type in {"system", "assistant", "tool_call", "result"}:
            return CursorDriver.parse_stream_steps(self, line)
        return super().parse_stream_steps(line)

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def _hello_argv(self) -> list[str]:
        return self._offline_argv(self.HELLO_PROMPT)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        return r.returncode == 0 and '"type":"result"' in (r.stdout or "")


class KimiCodeDriver(CliDriver):
    """`kimi -p` using Kimi Code's documented stream-json prompt mode."""

    name = "kimi"
    offline_web_isolation = True
    _ENV_EXTRA = {"KIMI_CODE_NO_AUTO_UPDATE": "1"}
    # Kimi Code does not expose a top-level --no-web flag.  Its documented
    # Agent profile denylist removes tools from the model-visible tool set and
    # enforces the same denylist again before execution.  Keep the profile in
    # the Muteki source tree: no user-level Agent or Skill directory is changed.
    _OFFLINE_AGENT_FILE = Path(__file__).with_name("kimi_offline_agent.md")

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
    ) -> list[str]:
        del session, kb_access, stream
        return [
            self.bin,
            *self._offline_flags(web_access=web_access),
            "--output-format", "stream-json", "-p", prompt,
        ]

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        # Kimi binds the selected Agent profile when the session is created and
        # restores that binding on resume.  --agent-file cannot be combined with
        # --session, so the initial offline invocation is the enforcement point.
        del web_access, kb_access, stream
        return [
            self.bin, "--session", session,
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
        if text_out or session:
            return CliResult(
                text=text_out, session=session, raw_stderr=stderr[-2000:])
        return CliResult(text=stdout[-8000:], raw_stderr=stderr[-2000:])

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
        for line in (r.stdout or "").splitlines():
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (isinstance(ev, dict) and ev.get("role") == "assistant"
                    and str(ev.get("content") or "").strip()):
                return True
        return False


class GrokDriver(ClaudeCodeDriver):
    """`grok --single` with Anthropic-compatible streaming message output."""

    name = "grok"
    secure_prompt_transport = False
    offline_web_isolation = True
    _OFFLINE_AGENT_FILE = Path(__file__).with_name("grok_offline_agent.md")
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

    def _offline_prefix(self, *, web_access: bool) -> list[str]:
        # Keep Grok's normal user Skill discovery, while preventing its Claude
        # and Cursor compatibility scanners from starting external MCP servers
        # for an offline task. These variables affect only this child process.
        return ["env", *self._OFFLINE_ENV] if not web_access else []

    @staticmethod
    def _base_flags(*, web_access: bool) -> list[str]:
        flags = [
            "--permission-mode", "bypassPermissions",
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
    ) -> list[str]:
        del session, kb_access, stream
        return [
            *self._offline_prefix(web_access=web_access),
            self.bin, *self._base_flags(web_access=web_access), "-p", prompt,
        ]

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        del prompt, session, web_access, kb_access, stream
        raise SecurePromptUnsupported(
            "grok does not provide a non-persistent stdin prompt mode")

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        del kb_access, stream
        return [
            *self._offline_prefix(web_access=web_access),
            self.bin, "--resume", session,
            *self._base_flags(web_access=web_access), "-p", prompt,
        ]

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        steps = super().parse_stream_steps(line)
        try:
            ev = json.loads(line.strip())
        except (json.JSONDecodeError, TypeError):
            return steps
        if not isinstance(ev, dict) or ev.get("type") != "assistant":
            return steps
        for block in (ev.get("message") or {}).get("content") or []:
            if not isinstance(block, dict) or block.get("type") != "thinking":
                continue
            thinking = str(block.get("thinking") or "").strip()
            if thinking:
                steps.append(StreamStep("reasoning", text=thinking))
        return steps

    def _hello_argv(self) -> list[str]:
        return self.build_execute(
            self.HELLO_PROMPT, None, web_access=False, kb_access=False)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        for line in (r.stdout or "").splitlines():
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(ev, dict):
                continue
            if ev.get("type") == "result" and str(ev.get("result") or "").strip():
                return True
            if ev.get("type") == "assistant":
                return True
        return False


class OpenCodeDriver(CliDriver):
    """OpenCode JSONL transport with stable tool-call identifiers."""

    name = "opencode"
    secure_prompt_transport = False
    offline_web_isolation = True

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
    ) -> list[str]:
        del session, kb_access, stream
        # OPENCODE_CONFIG_CONTENT is scoped to this process.  Keeping it in argv
        # allows the offline permission decision to differ per invocation without
        # mutating the operator's OpenCode configuration.
        return [
            "env", f"OPENCODE_CONFIG_CONTENT={self._config(web_access=web_access)}",
            self.bin, "run", "--pure", "--format", "json", "--auto", prompt,
        ]

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        del kb_access, stream
        return [
            "env", f"OPENCODE_CONFIG_CONTENT={self._config(web_access=web_access)}",
            self.bin, "run", "--pure", "--format", "json", "--auto",
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
            return [StreamStep("reasoning", text=text)] if text else []
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
        input_tokens = output_tokens = 0
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
                input_tokens += int(tokens.get("input") or 0) + int(cache.get("read") or 0)
                output_tokens += int(tokens.get("output") or 0) + int(tokens.get("reasoning") or 0)
                if part.get("cost") is not None:
                    cost += float(part.get("cost") or 0)
                    saw_cost = True
        return CliResult(
            text="\n".join(text_parts).strip(),
            session=session,
            cost_usd=cost if saw_cost else None,
            input_tokens=input_tokens or None,
            output_tokens=output_tokens or None,
            num_turns=turns or None,
            raw_stderr=stderr[-2000:],
        )

    def _hello_argv(self) -> list[str]:
        return self.build_execute(
            self.HELLO_PROMPT, None, web_access=False, kb_access=False)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        return r.returncode == 0 and bool(self.parse(r.stdout or "", r.stderr or "").text)


class DeepSeekHarnessDriver(CliDriver):
    """Official DeepSeek Harness Python SDK transported as Muteki NDJSON."""

    name = "dsh"
    secure_prompt_transport = False
    offline_web_isolation = True
    _BRIDGE = Path(__file__).with_name("deepseek_harness_worker.py")

    def new_session(self) -> Optional[str]:
        return f"session-{uuid.uuid4().hex}"

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        del web_access, kb_access, stream
        return [
            self.bin, str(self._BRIDGE), "--session",
            session or self.new_session() or "", "--", prompt,
        ]

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        return self.build_execute(
            prompt, session, web_access=web_access,
            kb_access=kb_access, stream=stream)

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        try:
            event = json.loads(line.strip())
        except (json.JSONDecodeError, TypeError):
            return []
        if not isinstance(event, dict):
            return []
        kind = str(event.get("type") or "")
        if kind == "session":
            return [StreamStep("session", session=str(event.get("id") or ""))]
        if kind == "reasoning":
            return [StreamStep("reasoning", text=str(event.get("text") or ""))]
        if kind == "tool":
            return [StreamStep(
                "tool",
                text=str(event.get("arguments") or ""),
                tool=str(event.get("tool") or ""),
                call_id=str(event.get("call_id") or ""),
            )]
        if kind == "tool_result":
            output = str(event.get("output") or "")
            return [StreamStep(
                "tool_result", text=output[:600], raw=output,
                call_id=str(event.get("call_id") or ""),
            )]
        return []

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def parse(self, stdout: str, stderr: str) -> CliResult:
        text = ""
        session: Optional[str] = None
        input_tokens = output_tokens = 0
        for line in stdout.splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(event, dict):
                continue
            kind = event.get("type")
            if kind == "session" and event.get("id"):
                session = str(event["id"])
            elif kind == "result":
                text = str(event.get("text") or "")
                session = str(event.get("session_id") or session or "") or None
            elif kind == "usage":
                input_tokens += int(event.get("input") or 0) + int(event.get("cache_read") or 0)
                output_tokens += int(event.get("output") or 0)
        return CliResult(
            text=text,
            session=session,
            input_tokens=input_tokens or None,
            output_tokens=output_tokens or None,
            raw_stderr=stderr[-2000:],
        )

    def _hello_argv(self) -> list[str]:
        return self.build_execute(
            self.HELLO_PROMPT, self.new_session(),
            web_access=False, kb_access=False)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        return r.returncode == 0 and bool(self.parse(r.stdout or "", r.stderr or "").text)


DRIVERS: dict[str, CliDriver] = {
    "claude": ClaudeCodeDriver(),
    "codex": CodexDriver(),
    "cursor": CursorDriver(),
    "pi": PiDriver(),
    "omp": OhMyPiDriver(),
    "kimi": KimiCodeDriver(),
    "grok": GrokDriver(),
    "opencode": OpenCodeDriver(),
    "dsh": DeepSeekHarnessDriver(),
}


def get_driver(name: str) -> CliDriver:
    try:
        return DRIVERS[name]
    except KeyError:
        raise ValueError(
            f"unknown engine {name!r}: expected one of {sorted(DRIVERS)} "
            f"(a profile id like 'codex-sub-container' should be resolved to its "
            f"base engine via driver_for/base_engine_for_profile first)"
        ) from None


def _insert_before_prompt(argv: list[str], extra: list[str], *, engine: str = "") -> list[str]:
    if not extra:
        return argv
    if engine in {"kimi", "grok"}:
        for flag in ("-p", "--prompt", "--single"):
            if flag in argv:
                idx = argv.index(flag)
                return [*argv[:idx], *extra, *argv[idx:]]
    if "--" in argv:
        idx = argv.index("--")
        return [*argv[:idx], *extra, *argv[idx:]]
    if len(argv) <= 1:
        return [*argv, *extra]
    return [*argv[:-1], *extra, argv[-1]]


def _insert_model_arg(argv: list[str], model: str, *, engine: str = "") -> list[str]:
    model = (model or "").strip()
    if not model or "--model" in argv or "-m" in argv:
        return argv
    return _insert_before_prompt(argv, ["--model", model], engine=engine)


_ENGINE_REASONING_EFFORTS: dict[str, set[str]] = {
    "claude": {"low", "medium", "high", "xhigh", "max"},
    "codex": {"none", "minimal", "low", "medium", "high", "xhigh", "max"},
    "cursor": {"low", "medium", "high", "xhigh", "max"},
    "pi": {"none", "minimal", "low", "medium", "high", "xhigh", "max"},
    "omp": {"none", "minimal", "low", "medium", "high", "xhigh", "max"},
    "kimi": {"low", "medium", "high", "xhigh", "max"},
    "grok": {"low", "medium", "high", "xhigh"},
    "opencode": {"none", "minimal", "low", "medium", "high", "xhigh", "max"},
}


def apply_reasoning_effort(
    argv: list[str], *, engine: str, reasoning_effort: str,
) -> list[str]:
    """Translate one persisted effort value into the selected CLI's syntax."""
    effort = normalize_reasoning_effort(reasoning_effort, "default")
    if effort == "default" or effort not in _ENGINE_REASONING_EFFORTS.get(engine, set()):
        return argv
    if engine == "codex":
        if any("model_reasoning_effort=" in str(arg) for arg in argv):
            return argv
        flag = f'model_reasoning_effort="{effort}"'
        try:
            idx = argv.index("exec")
        except ValueError:
            idx = 1
        return [*argv[:idx], "-c", flag, *argv[idx:]]
    if engine == "cursor":
        if "--model" in argv:
            idx = argv.index("--model") + 1
        elif "-m" in argv:
            idx = argv.index("-m") + 1
        else:
            return argv
        if idx >= len(argv):
            return argv
        model = str(argv[idx]).strip()
        if not model or model == "auto":
            return argv
        fast = model.endswith("-fast")
        stem = model[:-5] if fast else model
        match = re.match(r"^(.*)-(low|medium|high|xhigh|max)$", stem)
        base = match.group(1) if match else stem
        # Cursor exposes effort as concrete model variants in `cursor-agent
        # models` (for example gpt-5.3-codex-low / -high / -xhigh). The bare
        # family id is its medium/default variant when present.
        model = base if effort == "medium" else f"{base}-{effort}"
        if fast:
            model += "-fast"
        return [*argv[:idx], model, *argv[idx + 1:]]
    if engine in {"pi", "omp"}:
        if "--thinking" in argv:
            return argv
        value = "off" if effort == "none" else effort
        return _insert_before_prompt(argv, ["--thinking", value], engine=engine)
    if engine == "grok":
        if "--reasoning-effort" in argv or "--effort" in argv:
            return argv
        return _insert_before_prompt(
            argv, ["--reasoning-effort", effort], engine=engine)
    if engine == "kimi":
        # Kimi Code accepts the override through KIMI_MODEL_THINKING_EFFORT.
        return argv
    if engine == "opencode":
        if "--variant" in argv:
            return argv
        return _insert_before_prompt(argv, ["--variant", effort], engine=engine)
    if "--effort" in argv:
        return argv
    return _insert_before_prompt(argv, ["--effort", effort], engine=engine)


def apply_runtime_argv(
    argv: list[str], *, driver: CliDriver, env: dict[str, Any],
) -> list[str]:
    """Apply profile/runtime argv options shared by probes and live workers.

    Keeping this transformation next to the drivers makes the startup probe use
    the exact model/provider/endpoint selection that a subsequently spawned
    ``CliSolver`` uses.  Process-specific wrappers such as macOS ``sandbox-exec``
    remain in ``CliSolver`` because they are unrelated to profile selection.
    """
    out = list(argv)
    engine = driver.name
    model = str(env.get("MUTEKI_WORKER_MODEL") or "").strip()
    if engine == "kimi" and str(env.get("KIMI_MODEL_NAME") or "").strip():
        # KIMI_MODEL_* synthesizes an in-memory provider/model. An explicit
        # --model from the normal OAuth profile has higher priority and would
        # bypass that provider, so remove it for the direct API-key channel.
        cleaned: list[str] = []
        skip_next = False
        for arg in out:
            if skip_next:
                skip_next = False
                continue
            if arg in {"--model", "-m"}:
                skip_next = True
                continue
            cleaned.append(arg)
        out = cleaned
        model = ""
    env_extra = getattr(driver, "env_extra", None)
    driver_env = env_extra() if callable(env_extra) else {}
    claude_model_from_env = (
        engine == "claude" and bool(driver_env.get("ANTHROPIC_MODEL"))
    )
    if model and not claude_model_from_env:
        out = _insert_model_arg(out, model, engine=engine)

    if engine == "cursor":
        endpoint = str(env.get("CURSOR_ENDPOINT") or "").strip()
        if endpoint and "--endpoint" not in out:
            out = _insert_before_prompt(
                out, ["--endpoint", endpoint], engine=engine)

    if engine in {"pi", "omp"}:
        prefix = "MUTEKI_PI" if engine == "pi" else "MUTEKI_OMP"
        provider = str(env.get(f"{prefix}_PROVIDER") or "").strip()
        if provider and "--provider" not in out:
            out = _insert_before_prompt(
                out, ["--provider", provider], engine=engine)
        provider_model = str(env.get(f"{prefix}_MODEL") or "").strip()
        if provider_model:
            out = _insert_model_arg(out, provider_model, engine=engine)

    return apply_reasoning_effort(
        out,
        engine=engine,
        reasoning_effort=str(
            env.get("MUTEKI_WORKER_REASONING_EFFORT") or "default"),
    )


def _claude_endpoint_model_env(profile: dict[str, Any]) -> dict[str, str]:
    """Return the shared Claude Code model environment for a custom endpoint.

    DeepSeek, GLM, Kimi and other Anthropic-compatible Claude Code integrations
    select their endpoint and model through ANTHROPIC_* variables. Provider-only
    context tuning is added after this common model mapping.
    """
    model = str(profile.get("model") or "").strip()
    base_url = str(profile.get("base_url") or "").strip().lower().rstrip("/")
    if (
        base_engine_for_profile(profile) != "claude"
        or not profile_uses_endpoint(profile)
        or not model
    ):
        return {}
    out = {
        "ANTHROPIC_MODEL": model,
        "ANTHROPIC_DEFAULT_FABLE_MODEL": model,
        "ANTHROPIC_DEFAULT_OPUS_MODEL": model,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": model,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": model,
        "CLAUDE_CODE_SUBAGENT_MODEL": model,
    }
    if base_url == "https://api.kimi.com/coding" and model == "k3[1m]":
        out.update({
            "CLAUDE_CODE_EFFORT_LEVEL": "high",
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1048576",
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1048576",
        })
    return out


def _endpoint_api_model(profile: dict[str, Any]) -> str:
    """Translate a CLI-only selector to a literal endpoint model ID."""
    model = str(profile.get("model") or "").strip()
    base_url = str(profile.get("base_url") or "").strip().lower().rstrip("/")
    if base_url == "https://api.kimi.com/coding" and model == "k3[1m]":
        return "k3"
    return model


class ProfileDriver(CliDriver):
    """Profile-bound wrapper for local/subscription workers.

    A worker profile is the unit the operator configures. Health probes and argv
    construction must therefore carry the profile's selected model too; otherwise a
    quota-exhausted default model can mark the whole engine unhealthy.
    """

    def __init__(self, base: CliDriver, profile: dict[str, Any]) -> None:
        self.base = base
        self.profile = dict(profile)
        self.name = base.name
        self.secure_prompt_transport = bool(
            getattr(base, "secure_prompt_transport", False))
        self.offline_web_isolation = bool(
            getattr(base, "offline_web_isolation", False))
        self.HELLO_PROMPT = base.HELLO_PROMPT
        self._HELLO_TIMEOUT = getattr(base, "_HELLO_TIMEOUT", self._HELLO_TIMEOUT)
        self._HELLO_RETRIES = getattr(base, "_HELLO_RETRIES", self._HELLO_RETRIES)

    @property
    def bin(self) -> str:
        return self.base.bin

    def _model(self) -> str:
        return str(self.profile.get("model") or "").strip()

    def _reasoning_effort(self) -> str:
        return normalize_reasoning_effort(
            self.profile.get("reasoning_effort"), "default")

    def _with_profile_options(self, argv: list[str]) -> list[str]:
        out = list(argv)
        credential_kind = str(
            self.profile.get("credential_kind")
            or ("engine_key" if self.profile.get("credential_account") else "system_inherit")
        ).strip()
        if self.name == "claude" and credential_kind != "system_inherit" and "--bare" not in out:
            sentinel = out.index("--") if "--" in out else len(out)
            out.insert(sentinel, "--bare")
        out = _insert_model_arg(out, self._model(), engine=self.name)
        return apply_reasoning_effort(
            out, engine=self.name, reasoning_effort=self._reasoning_effort())

    def new_session(self) -> Optional[str]:
        return self.base.new_session()

    def env_extra(self) -> "dict[str, str]":
        env = self.base.env_extra()
        effort = self._reasoning_effort()
        if self.name == "kimi" and effort != "default":
            env["KIMI_MODEL_THINKING_EFFORT"] = effort
        return env

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        return self._with_profile_options(self.base.build_execute(
            prompt, session, web_access=web_access, kb_access=kb_access, stream=stream))

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        return self._with_profile_options(self.base.build_execute_stdin(
            prompt, session, web_access=web_access, kb_access=kb_access, stream=stream))

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return self.base.secure_prompt_preflight()

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        return self._with_profile_options(self.base.build_resume(
            prompt, session, web_access=web_access, kb_access=kb_access, stream=stream))

    def parse(self, stdout: str, stderr: str) -> CliResult:
        return self.base.parse(stdout, stderr)

    def parse_stream_line(self, line: str) -> Optional["StreamStep"]:
        return self.base.parse_stream_line(line)

    def parse_stream_steps(self, line: str) -> list["StreamStep"]:
        return self.base.parse_stream_steps(line)

    def _hello_argv(self) -> list[str]:
        return self._with_profile_options(self.base._hello_argv())  # noqa: SLF001

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        return self.base._hello_ok(r)  # noqa: SLF001


class EndpointDriver(CliDriver):
    """Profile-bound driver wrapper for custom API endpoints.

    The base driver still owns parsing and CLI-specific behavior; this wrapper
    injects endpoint config. Readiness always executes the real Worker CLI.
    """

    def __init__(self, base: CliDriver, profile: dict[str, Any]) -> None:
        self.base = base
        self.profile = dict(profile)
        self.name = base.name
        self.secure_prompt_transport = bool(
            getattr(base, "secure_prompt_transport", False))
        # Endpoint profiles change model transport/authentication, not the CLI's
        # exposed tool set.  Therefore Claude's explicit WebSearch/WebFetch deny
        # and Codex's opt-in-only --search contract remain enforceable.
        self.offline_web_isolation = bool(
            getattr(base, "offline_web_isolation", False))
        self.HELLO_PROMPT = base.HELLO_PROMPT
        self._HELLO_TIMEOUT = getattr(base, "_HELLO_TIMEOUT", self._HELLO_TIMEOUT)
        self._HELLO_RETRIES = getattr(base, "_HELLO_RETRIES", self._HELLO_RETRIES)

    @property
    def bin(self) -> str:
        return self.base.bin

    def new_session(self) -> Optional[str]:
        return self.base.new_session()

    def env_extra(self) -> "dict[str, str]":
        env = {
            **self.base.env_extra(),
            **_claude_endpoint_model_env(self.profile),
        }
        effort = normalize_reasoning_effort(
            self.profile.get("reasoning_effort"), "default")
        if self.name == "kimi" and effort != "default":
            env["KIMI_MODEL_THINKING_EFFORT"] = effort
        return env

    def _with_profile_options(self, argv: list[str]) -> list[str]:
        out = list(argv)
        if self.name == "claude" and "--bare" not in out:
            sentinel = out.index("--") if "--" in out else len(out)
            out.insert(sentinel, "--bare")
        if not (self.name == "codex" and self._codex_config_flags()):
            selected_model = str(self.profile.get("model") or "").strip()
            if self.name == "opencode" and selected_model and "/" not in selected_model:
                selected_model = f"muteki/{selected_model}"
            out = _insert_model_arg(
                out, selected_model, engine=self.name)
        if self.name == "opencode":
            out = self._opencode_endpoint_config(out)
        return apply_reasoning_effort(
            out,
            engine=self.name,
            reasoning_effort=normalize_reasoning_effort(
                self.profile.get("reasoning_effort"), "default"),
        )

    def _opencode_endpoint_config(self, argv: list[str]) -> list[str]:
        base_url = str(self.profile.get("base_url") or "").strip()
        model = str(self.profile.get("model") or "").strip()
        if not base_url or not model:
            return argv
        out = list(argv)
        for idx, arg in enumerate(out):
            prefix = "OPENCODE_CONFIG_CONTENT="
            if not str(arg).startswith(prefix):
                continue
            try:
                config = json.loads(str(arg)[len(prefix):])
            except json.JSONDecodeError:
                config = {}
            provider = config.setdefault("provider", {})
            provider["muteki"] = {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Muteki",
                "options": {
                    "baseURL": base_url,
                    "apiKey": "{env:OPENAI_API_KEY}",
                },
                "models": {model: {"name": model}},
            }
            out[idx] = prefix + json.dumps(config, separators=(",", ":"))
            break
        return out

    def _codex_config_flags(self) -> list[str]:
        base_url = str(self.profile.get("base_url") or "").strip()
        if self.name != "codex" or not base_url:
            return []
        wire_api = str(self.profile.get("wire_api") or "responses").strip() or "responses"
        model = _endpoint_api_model(self.profile)
        # `name` is REQUIRED by codex: a [model_providers.X] block with no `name`
        # fails config load with "provider name must not be empty", so a custom
        # endpoint never even reaches the request. `env_key` pins which env var
        # holds the bearer token — OPENAI_API_KEY is exactly what the Credential
        # Account injection populates for codex (see _api_key / runtime_env_for_engine),
        # so codex reads the worker's endpoint key instead of silently sending none.
        flags = [
            "-c", "model_provider=muteki",
            "-c", "model_providers.muteki.name=muteki",
            "-c", f"model_providers.muteki.base_url={base_url}",
            "-c", f"model_providers.muteki.wire_api={wire_api}",
            "-c", "model_providers.muteki.env_key=OPENAI_API_KEY",
        ]
        if model:
            flags += ["-c", f"model={model}"]
        return flags

    def _inject_before_exec(self, argv: list[str]) -> list[str]:
        flags = self._codex_config_flags()
        if not flags:
            return argv
        try:
            idx = argv.index("exec")
        except ValueError:
            return [argv[0], *flags, *argv[1:]]
        return [*argv[:idx], *flags, *argv[idx:]]

    def _codex_endpoint_isolation(self, *, kb_access: bool) -> list[str]:
        """Return the stable Codex isolation envelope for custom endpoints.

        Codex 0.143's WebSocket-to-HTTPS fallback drops ``model_provider`` when
        any of its Desktop feature gates are disabled.  The retry then targets
        api.openai.com with the endpoint key, which looks like an invalid-key
        failure.  Custom endpoints therefore retain config isolation without
        adding those feature gates; native web search remains opt-in via
        ``_globals`` just as it is for every other Codex invocation.
        """
        return ["--ignore-user-config"] if not kb_access else []

    def _build_codex_endpoint_execute(
        self, prompt: str, *, web_access: bool, kb_access: bool,
        stdin: bool = False,
    ) -> list[str]:
        flags = self._codex_config_flags()
        argv = [
            self.bin,
            *self.base._globals(web_access=web_access),  # noqa: SLF001
            *flags,
            "exec",
            *self._codex_endpoint_isolation(kb_access=kb_access),
            "--json",
        ]
        if stdin:
            return [
                *argv,
                "--ephemeral",
                "--dangerously-bypass-approvals-and-sandbox",
                "-",
            ]
        return [
            *argv,
            "--dangerously-bypass-approvals-and-sandbox",
            "--",
            prompt,
        ]

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        if self.name == "codex" and self._codex_config_flags():
            return self._with_profile_options(self._build_codex_endpoint_execute(
                prompt, web_access=web_access, kb_access=kb_access))
        return self._with_profile_options(self._inject_before_exec(
            self.base.build_execute(
                prompt, session, web_access=web_access,
                kb_access=kb_access, stream=stream)))

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        if self.name == "codex" and self._codex_config_flags():
            return self._with_profile_options(self._build_codex_endpoint_execute(
                prompt, web_access=web_access, kb_access=kb_access, stdin=True))
        return self._with_profile_options(self._inject_before_exec(
            self.base.build_execute_stdin(
                prompt, session, web_access=web_access,
                kb_access=kb_access, stream=stream)))

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return self.base.secure_prompt_preflight()

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
    ) -> list[str]:
        if self.name == "codex" and self._codex_config_flags():
            argv = [
                self.bin,
                *self.base._globals(web_access=web_access),  # noqa: SLF001
                *self._codex_config_flags(),
                "exec", "resume", session,
                *self._codex_endpoint_isolation(kb_access=kb_access),
                "--json", "--dangerously-bypass-approvals-and-sandbox",
                "--", prompt,
            ]
            return self._with_profile_options(argv)
        return self._with_profile_options(self._inject_before_exec(
            self.base.build_resume(
                prompt, session, web_access=web_access,
                kb_access=kb_access, stream=stream)))

    def parse(self, stdout: str, stderr: str) -> CliResult:
        return self.base.parse(stdout, stderr)

    def parse_stream_line(self, line: str) -> Optional["StreamStep"]:
        return self.base.parse_stream_line(line)

    def parse_stream_steps(self, line: str) -> list["StreamStep"]:
        return self.base.parse_stream_steps(line)

    def _hello_argv(self) -> list[str]:
        if self.name == "codex" and self._codex_config_flags():
            return self._with_profile_options(self._build_codex_endpoint_execute(
                self.HELLO_PROMPT, web_access=False, kb_access=False))
        argv = self.base._hello_argv()  # noqa: SLF001
        if argv:
            return self._with_profile_options(self._inject_before_exec(argv))
        return self.build_execute(
            self.HELLO_PROMPT, None,
            web_access=False, kb_access=False, stream=False,
        )

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        return self.base._hello_ok(r)  # noqa: SLF001

    def _api_key(self, env: "dict[str, str] | None" = None) -> str:
        """Resolve the endpoint API key for the health probe, mirroring how the
        real worker authenticates (#5). The old version only handled `env:NAME`,
        so a FILE-backed Credential Account (api_key_ref empty, secret stored in an
        API_KEY file) made the probe omit the auth header → false-negative health
        even though the live worker authenticates fine via runtime_env_for_engine.
        Resolution order: explicit api_key_ref (env: or file:) → the *_API_KEY_FILE
        / *_API_KEY env the credential injection already populates for this worker.

        `env` (when given) is the credential environment the caller resolved for this
        probe — read it instead of the process-global os.environ so a parallel probe
        sees ITS OWN injected key, not whatever another thread last overlaid."""
        src = env if env is not None else os.environ
        ref = str(self.profile.get("api_key_ref") or "").strip()
        if ref.startswith("env:"):
            return src.get(ref[4:], "")
        if ref.startswith("file:"):
            try:
                return Path(ref[5:]).read_text(encoding="utf-8").strip()
            except OSError:
                return ""
        # No explicit ref → fall back to the env the Credential Account injection
        # sets for this transport: <PROVIDER>_API_KEY_FILE (file-backed) or the
        # bare <PROVIDER>_API_KEY (env-backed).
        env_name = {
            "claude": "ANTHROPIC_API_KEY",
            "dsh": "DEEPSEEK_API_KEY",
        }.get(self.name, "OPENAI_API_KEY")
        file_env = src.get(f"{env_name}_FILE", "").strip()
        if file_env:
            try:
                return Path(file_env).read_text(encoding="utf-8").strip()
            except OSError:
                return ""
        return src.get(env_name, "").strip()

    def health_detail(self, *, env: "dict[str, str] | None" = None) -> "tuple[bool, str]":
        # A direct HTTP request proves only that one endpoint shape accepts one
        # payload. The dispatch contract needs the configured CLI, credentials,
        # provider/model flags and response parser to complete the same turn a
        # Worker will run.
        probe_env = {**os.environ, **self.env_extra(), **(env or {})}
        base_url = str(self.profile.get("base_url") or "").strip()
        if self.name == "claude" and base_url:
            probe_env.setdefault("ANTHROPIC_BASE_URL", base_url)
        elif self.name == "cursor" and base_url:
            probe_env.setdefault("CURSOR_ENDPOINT", base_url)
        elif self.name == "omp" and base_url:
            probe_env.setdefault("OPENAI_BASE_URL", base_url)
        elif self.name == "dsh" and base_url:
            probe_env.setdefault("DEEPSEEK_BASE_URL", base_url)

        key = self._api_key(probe_env)
        key_env = {
            "claude": "ANTHROPIC_API_KEY",
            "codex": "OPENAI_API_KEY",
            "cursor": "CURSOR_API_KEY",
            "pi": "OPENAI_API_KEY",
            "omp": "OPENAI_API_KEY",
            "opencode": "OPENAI_API_KEY",
            "dsh": "DEEPSEEK_API_KEY",
        }.get(self.name)
        if key and key_env:
            probe_env.setdefault(key_env, key)
            if self.name == "claude":
                probe_env.setdefault("ANTHROPIC_AUTH_TOKEN", key)
        return CliDriver.health_detail(self, env=probe_env)


def driver_for(profile_or_name: str | dict[str, Any]) -> CliDriver:
    if isinstance(profile_or_name, dict):
        base_name = base_engine_for_profile(profile_or_name)
        base = get_driver(base_name)
        if profile_uses_endpoint(profile_or_name):
            return EndpointDriver(base, profile_or_name)
        return ProfileDriver(base, profile_or_name)
    # A bare string may be a base engine, a transport, OR a profile id like
    # "codex-sub-container". base_engine_for_profile recovers the base from any of
    # them, so a profile id no longer hits DRIVERS[...] raw (which would KeyError —
    # the "local run crashes on the -sub-container profile" bug).
    return get_driver(base_engine_for_profile(str(profile_or_name)))


# Deep auth-level liveness for the engine bar (FE-quota-display). `--version`
# (`available`) only proves the binary runs — it can't catch an expired headless
# auth (e.g. cursor-agent -p → "Authentication required" even though
# `cursor-agent status` shows logged-in). health_detail() shells a real one-turn
# hello, so it's expensive: cache it on its OWN throttle (>= the deck's 60s poll)
# with last-good reuse, exactly like quota. Decorative + never blocks the bar.
_HEALTH_TTL = 55.0
_health_cache: dict = {"ts": 0.0, "data": None}


import contextlib as _contextlib


@_contextlib.contextmanager
def _patched_env(values: "dict[str, str]"):
    """Temporarily overlay os.environ with `values`, restoring on exit."""
    old = {k: os.environ.get(k) for k in values}
    try:
        os.environ.update(values)
        yield
    finally:
        for k, v in old.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


def _probe_health_with_creds(name: str, drv: "CliDriver",
                             account_root: "Optional[str]") -> "tuple[bool, str]":
    """Run a driver's health_detail() with the engine's DEFAULT-account credential
    env injected (when account_root is known) — so the global probe matches what a
    live worker sees. Critical for cursor: its headless CLI authenticates ONLY via
    CURSOR_API_KEY, so a bare probe falsely reports "Authentication required" and the
    engine bar shows a healthy engine as down. account_root=None → bare probe
    (no account store available, e.g. a TUI/test context)."""
    if account_root is None:
        return drv.health_detail()
    try:
        from muteki.solver.credential_accounts import runtime_env_for_engine
        # Local Codex subscription auth is the host's default CODEX_HOME
        # (~/.codex). A stale persisted codex-main account must not make the engine
        # bar or dispatch preflight report Codex down when the host login works.
        account_id = "" if name == "codex" else None
        env = runtime_env_for_engine(
            name, account_root=account_root, account_id=account_id, container=False).env
    except Exception:
        env = {}
    if not env:
        return drv.health_detail()
    with _patched_env(env):
        return drv.health_detail()


def engine_liveness(account_root: "Optional[str]" = None) -> dict:
    """Best-effort {engine: {healthy: bool, detail: str}} from a DEEP one-turn
    probe, throttled to one real run per _HEALTH_TTL with last-good reuse. This is
    what lets the engine bar show "cursor unavailable: Authentication required"
    instead of a green dot, even when no run is active. NEVER raises / blocks.

    `account_root` (the credential-account store) lets the probe inject each engine's
    default-account auth so cursor (CURSOR_API_KEY-only headless) isn't falsely
    reported down — mirrors the live-worker / _healthy_engines credential path."""
    now = time.time()
    cached = _health_cache.get("data")
    if cached is not None and now - _health_cache["ts"] < _HEALTH_TTL:
        return cached
    out: dict = {}
    for name, drv in DRIVERS.items():
        try:
            healthy, detail = _probe_health_with_creds(name, drv, account_root)
        except Exception as exc:  # noqa: BLE001 — bar must never break
            healthy, detail = False, str(exc)[:160]
        out[name] = {"healthy": bool(healthy), "detail": detail or ""}
    _health_cache["data"] = out
    _health_cache["ts"] = now
    return out


def _claude_oauth() -> "Optional[tuple[str, int]]":
    """(access_token, expires_at_ms) from env / macOS Keychain / creds file.
    Returns None when no credential is found. Used by credential_accounts for
    login detection. Never raises."""
    env_tok = (os.environ.get("CLAUDE_CODE_OAUTH_TOKEN")
               or os.environ.get("ANTHROPIC_AUTH_TOKEN")
               or os.environ.get("ANTHROPIC_API_KEY"))
    if env_tok and env_tok.strip():
        return env_tok.strip(), 0
    raw: Optional[str] = None
    try:
        r = subprocess.run(
            ["security", "find-generic-password", "-s", "Claude Code-credentials", "-w"],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5)
        if r.returncode == 0 and r.stdout.strip():
            raw = r.stdout.strip()
    except Exception:
        pass
    if not raw:
        try:
            p = Path.home() / ".claude" / ".credentials.json"
            if p.exists():
                raw = p.read_text()
        except Exception:
            pass
    if not raw:
        return None
    try:
        d = json.loads(raw)
        o = d.get("claudeAiOauth") or d
        tok = o.get("accessToken")
        exp = int(o.get("expiresAt") or 0)
        if tok:
            return tok, exp
    except Exception:
        pass
    return None


def _cursor_session_cookie() -> "Optional[str]":
    """`WorkosCursorSessionToken=<userId>::<JWT>` from the macOS Keychain +
    cli-config, or None. Never raises. (Linux Cursor stores the token elsewhere;
    we only support the Keychain path today → None elsewhere.)"""
    tok: Optional[str] = None
    try:
        r = subprocess.run(
            ["security", "find-generic-password", "-s", "cursor-access-token", "-w"],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5)
        if r.returncode == 0 and r.stdout.strip():
            tok = r.stdout.strip()
    except Exception:
        pass
    if not tok:
        return None
    uid: Optional[str] = None
    try:
        cfg = Path.home() / ".cursor" / "cli-config.json"
        if cfg.exists():
            uid = str(json.loads(cfg.read_text()).get("authInfo", {}).get("userId") or "")
    except Exception:
        pass
    if not uid:
        return None
    # cookie value is "<userId>::<JWT>", url-encoded (:: → %3A%3A)
    return f"WorkosCursorSessionToken={uid}%3A%3A{tok}"


def engine_status(account_root: "Optional[str]" = None,
                  backend: str = "local",
                  profiles: "Optional[list[dict[str, Any]]]" = None) -> list[dict]:
    """Cheap per-dispatched-worker status for the deck's always-on engine bar.

    This endpoint is polled by the browser, so it must not spend model tokens. It
    only checks that the configured engine binary can start (`--version`) and
    annotates the selected worker profile/model when available. Two seats that
    share a base engine (two Pi credentials) stay two rows. Token-spending
    model probes live in `/api/engines/health`, the model-test button, and the
    dispatch-time health gate.
    """
    profile_rows = [p for p in (profiles or []) if isinstance(p, dict)]
    if profile_rows:
        # One row per dispatched worker. Two Pi seats with different
        # credentials are two engines on the deck bar, not one collapsed Pi.
        selected: list[tuple[str, dict[str, Any] | None]] = []
        for p in profile_rows:
            name = base_engine_for_profile(p)
            if name in DRIVERS:
                selected.append((name, p))
    else:
        selected = [(name, None) for name in DRIVERS]
    out: list[dict] = []
    for name, profile in selected:
        drv = driver_for(profile) if profile else DRIVERS[name]
        try:
            b = drv.bin
            ok = _runs_ok(b)
        except Exception:
            b, ok = name, False
        row = {
            "engine": name,
            "bin": b,
            "available": ok,
            # None means "not deep-probed by the always-on poll". The frontend only
            # treats explicit False as degraded; run-scoped failures and on-demand
            # checks still surface their concrete reasons.
            "healthy": None,
            "health_detail": "",
        }
        if profile:
            row.update({
                "profile_id": profile.get("id") or "",
                "profile_name": (
                    profile.get("label")
                    or profile.get("name")
                    or profile.get("id")
                    or name
                ),
                "model": str(profile.get("model") or ""),
                "backend": backend,
            })
        out.append(row)
    return out


def engine_health(backend: str = "local",
                  account_root: "Optional[str]" = None,
                  profiles: "Optional[list[dict[str, Any]]]" = None) -> list[dict]:
    """A DEEP per-engine self-check (FE-healthcheck-page). `backend` selects WHAT
    is checked, because local and container exercise different things:

    - "local"     → run each driver's real healthcheck ON THE HOST (claude does a
                    1-turn dry run that exercises the host's default login + auth).
                    Answers "is the host's default CLI healthy?".
    - "container" → `docker run --rm` the worker image and verify each engine's
                    CLI launches INSIDE the container (image present + binary on
                    the container PATH). Answers "can the worker image actually
                    start each engine?". Auth-in-container is account-specific and
                    is covered by the per-account connectivity test, not here.

    When `profiles` is provided for local mode, self-check those configured worker
    profiles instead of the bare engines: that makes the button exercise the same
    credential account and selected model a real worker will use. Returns {engine,
    bin, version, healthy, detail, backend}. On-demand only."""
    if (backend or "").strip() == "container":
        return _engine_health_container()
    profile_rows = [p for p in (profiles or []) if isinstance(p, dict)]
    if profile_rows:
        from muteki.solver.credential_accounts import runtime_env_for_engine

        def _insert_model(argv: list[str], model: str) -> list[str]:
            model = (model or "").strip()
            if not model or "--model" in argv or "-m" in argv:
                return argv
            if "--" in argv:
                idx = argv.index("--")
                return [*argv[:idx], "--model", model, *argv[idx:]]
            if len(argv) <= 1:
                return [*argv, "--model", model]
            return [*argv[:-1], "--model", model, argv[-1]]

        out: list[dict] = []
        for profile in profile_rows:
            name = base_engine_for_profile(profile)
            drv = driver_for(profile)
            b, version, healthy, detail = name, "", False, ""
            try:
                b = drv.bin
                r = subprocess.run([b, "--version"], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=20)
                raw = (r.stdout or r.stderr or "").strip()
                version = raw.splitlines()[0][:80] if raw else ""
                if r.returncode != 0:
                    detail = "binary not runnable (--version failed)"
                else:
                    account_id = str(profile.get("credential_account") or "").strip()
                    resolved_account_id = account_id if account_id else ""
                    env = runtime_env_for_engine(
                        name,
                        account_root=Path(account_root) if account_root else None,
                        account_id=resolved_account_id,
                        container=False,
                    ).env
                    old = {k: os.environ.get(k) for k in env}
                    try:
                        os.environ.update(env)
                        if profile_uses_endpoint(profile):
                            healthy, detail = drv.health_detail()
                        else:
                            argv = _insert_model(
                                drv._hello_argv(),  # noqa: SLF001 - self-check mirrors driver probe.
                                str(profile.get("model") or ""))
                            if not argv:
                                healthy, detail = False, "driver has no hello probe"
                            else:
                                rr = subprocess.run(
                                    argv, capture_output=True, text=True, encoding="utf-8", errors="replace",
                                    timeout=getattr(drv, "_HELLO_TIMEOUT", 90))
                                healthy = bool(drv._hello_ok(rr))  # noqa: SLF001
                                if not healthy:
                                    tail = (rr.stderr or rr.stdout or "").strip().splitlines()
                                    detail = (f"hello exited {rr.returncode}"
                                              + (f": {tail[-1][:120]}" if tail else ""))
                    finally:
                        for k, v in old.items():
                            if v is None:
                                os.environ.pop(k, None)
                            else:
                                os.environ[k] = v
            except FileNotFoundError:
                detail = "binary not found on PATH"
            except subprocess.TimeoutExpired:
                detail = "probe timed out"
            except Exception as e:  # noqa: BLE001
                detail = str(e)[:160]
            out.append({"engine": name, "profile_id": profile.get("id") or "",
                        "profile_name": profile.get("name") or profile.get("id") or name,
                        "model": str(profile.get("model") or ""),
                        "bin": b, "version": version, "healthy": healthy,
                        "detail": detail, "backend": "local",
                        "bin_source": resolve_engine_bin_source(name),
                        "bin_env": _ENV_OVERRIDE.get(name, "")})
        return out
    out: list[dict] = []
    for name, drv in DRIVERS.items():
        b, version, healthy, detail = name, "", False, ""
        try:
            b = drv.bin
            r = subprocess.run([b, "--version"], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=20)
            raw = (r.stdout or r.stderr or "").strip()
            version = raw.splitlines()[0][:80] if raw else ""
            if r.returncode != 0:
                detail = "binary not runnable (--version failed)"
            else:
                # deep probe: a real one-turn hello (with one retry on a transient
                # miss). detail names the failure mode so red is actionable, not a
                # blanket "check login / quota". Inject the default-account creds so
                # cursor (CURSOR_API_KEY-only headless) isn't falsely reported down.
                healthy, detail = _probe_health_with_creds(name, drv, account_root)
        except FileNotFoundError:
            detail = "binary not found on PATH"
        except subprocess.TimeoutExpired:
            detail = "probe timed out"
        except Exception as e:  # noqa: BLE001 — surface the message to the operator
            detail = str(e)[:160]
        # bin_source tells the FE whether this path was explicitly pinned (env) or
        # auto-discovered (known-good / path) so it can warn that an unpinned local
        # default may resolve to the wrong version, and point at the env var to fix.
        out.append({"engine": name, "bin": b, "version": version,
                    "healthy": healthy, "detail": detail, "backend": "local",
                    "bin_source": resolve_engine_bin_source(name),
                    "bin_env": _ENV_OVERRIDE.get(name, "")})
    return out


# in-container worker binary per engine (mirrors container_exec._CONTAINER_BIN).
_CONTAINER_ENGINE_BIN = {
    "claude": "claude",
    "codex": "codex",
    "cursor": "/home/kali/.local/bin/cursor-agent",
    "pi": "pi",
    "omp": "/home/kali/.local/bin/omp",
    "kimi": "kimi",
    "grok": "/home/kali/.grok/bin/grok",
    "opencode": "opencode",
    "dsh": "python3",
}


def _engine_health_container() -> list[dict]:
    """Container self-check: one `docker run --rm` per engine verifying the worker
    image has a launchable CLI. No account/bench mounts — this checks the image +
    binary plumbing only (auth is the per-account test's job)."""
    import shutil

    out: list[dict] = []
    docker = shutil.which("docker")
    # image presence is shared across engines — probe once.
    from muteki.solver.container_exec import WORKER_IMAGE
    image_ok = False
    image_detail = ""
    if not docker:
        image_detail = "docker not found"
    else:
        try:
            r = subprocess.run([docker, "image", "inspect", WORKER_IMAGE],
                               capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=20)
            image_ok = r.returncode == 0
            if not image_ok:
                image_detail = f"image missing: {WORKER_IMAGE}"
        except subprocess.TimeoutExpired:
            image_detail = "docker image inspect timed out"
        except Exception as e:  # noqa: BLE001
            image_detail = str(e)[:120]

    for name in DRIVERS:
        bin_in = _CONTAINER_ENGINE_BIN.get(name, name)
        healthy, version, detail = False, "", ""
        if not image_ok:
            detail = image_detail
        else:
            try:
                r = subprocess.run(
                    # the image ENTRYPOINT is the runtime supervisor (a daemon); a
                    # one-shot self-check must override it with a shell via
                    # --entrypoint, else `-lc <cmd>` becomes args to the supervisor.
                    [docker, "run", "--rm", "--network", "none",
                     "--entrypoint", "bash", WORKER_IMAGE,
                     "-lc", f"{bin_in} --version 2>&1 || echo MUTEKI_CLI_FAIL"],
                    capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=60)
                raw = (r.stdout or "").strip()
                if "MUTEKI_CLI_FAIL" in raw or r.returncode != 0:
                    detail = f"{name} CLI not launchable in container"
                else:
                    healthy = True
                    version = raw.splitlines()[0][:80] if raw else ""
            except subprocess.TimeoutExpired:
                detail = "container probe timed out"
            except Exception as e:  # noqa: BLE001
                detail = str(e)[:120]
        out.append({"engine": name, "bin": bin_in, "version": version,
                    "healthy": healthy, "detail": detail, "backend": "container"})
    return out


def _local_process_table() -> "dict[int, tuple[int, int, str]]":
    """Return pid -> (ppid, pgid, start identity) for local process ownership.

    The start identity prevents a PID reused during a long task from being treated
    as a process that belongs to an older Worker.  An empty table means the
    best-effort ``ps`` sample failed; callers retain their previous observations.
    """
    try:
        out = subprocess.run(
            ["ps", "-axo", "pid=,ppid=,pgid=,lstart="],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=10,
        ).stdout
    except Exception:
        return {}
    rows: "dict[int, tuple[int, int, str]]" = {}
    for line in out.splitlines():
        parts = line.strip().split(None, 3)
        if len(parts) != 4:
            continue
        try:
            pid, ppid, pgid = int(parts[0]), int(parts[1]), int(parts[2])
        except ValueError:
            continue
        rows[pid] = (ppid, pgid, parts[3].strip())
    return rows


_LOCAL_PROCESS_OWNERS_LOCK = threading.Lock()
_LOCAL_PROCESS_OWNERS: "set[_LocalProcessOwner]" = set()
_LOCAL_PROCESS_OBSERVER: "Optional[threading.Thread]" = None
_LOCAL_PROCESS_CWD_LOCK = threading.Lock()
_LOCAL_PROCESS_CWD_CACHE: "tuple[float, dict[int, str]]" = (0.0, {})


def _local_process_cwds() -> "dict[int, str]":
    """Return current working directories, with a short shared cache.

    This is sampled only during a control or cleanup action.  It closes the
    double-fork gap where an intermediate parent starts and exits between process
    table samples, leaving a command in the Worker's private workspace.
    """
    global _LOCAL_PROCESS_CWD_CACHE
    now = time.monotonic()
    with _LOCAL_PROCESS_CWD_LOCK:
        cached_at, cached = _LOCAL_PROCESS_CWD_CACHE
        if now - cached_at < 0.25:
            return dict(cached)
        try:
            result = subprocess.run(
                ["lsof", "-a", "-d", "cwd", "-Fn"],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=10,
            )
        except Exception:
            return {}
        found: "dict[int, str]" = {}
        current_pid: "Optional[int]" = None
        for line in (result.stdout or "").splitlines():
            if line.startswith("p"):
                try:
                    current_pid = int(line[1:])
                except ValueError:
                    current_pid = None
            elif line.startswith("n") and current_pid is not None:
                found[current_pid] = line[1:]
        _LOCAL_PROCESS_CWD_CACHE = (time.monotonic(), found)
        return dict(found)


def _observe_local_process_owners() -> None:
    """Sample one process table for every running local Worker."""
    global _LOCAL_PROCESS_OBSERVER
    while True:
        with _LOCAL_PROCESS_OWNERS_LOCK:
            owners = tuple(_LOCAL_PROCESS_OWNERS)
            if not owners:
                _LOCAL_PROCESS_OBSERVER = None
                return
        rows = _local_process_table()
        if rows:
            for owner in owners:
                owner.observe(rows)
        time.sleep(0.05)


def _register_local_process_owner(owner: "_LocalProcessOwner") -> None:
    global _LOCAL_PROCESS_OBSERVER
    with _LOCAL_PROCESS_OWNERS_LOCK:
        _LOCAL_PROCESS_OWNERS.add(owner)
        if _LOCAL_PROCESS_OBSERVER is None or not _LOCAL_PROCESS_OBSERVER.is_alive():
            _LOCAL_PROCESS_OBSERVER = threading.Thread(
                target=_observe_local_process_owners,
                name="muteki-local-process-observer",
                daemon=True,
            )
            _LOCAL_PROCESS_OBSERVER.start()


def _unregister_local_process_owner(owner: "_LocalProcessOwner") -> None:
    with _LOCAL_PROCESS_OWNERS_LOCK:
        _LOCAL_PROCESS_OWNERS.discard(owner)


class _LocalProcessOwner:
    """Persistent ownership record for one local Worker invocation.

    CLI tools may create a new session and be reparented to PID 1 while the Worker
    is still running.  A stop-time descendant scan can no longer associate those
    processes with the Worker.  This record observes descendants while the parent
    link still exists and retains their identity until they exit.
    """

    def __init__(self, proc: "subprocess.Popen", *, cwd: str) -> None:
        self.proc = proc
        self.root_pid = int(proc.pid)
        self.cwd = str(Path(cwd).resolve())
        self._lock = threading.Lock()
        self._tracked: "dict[int, str]" = {}
        self._closed = False
        rows = _local_process_table()
        root = rows.get(self.root_pid)
        self._tracked[self.root_pid] = root[2] if root is not None else ""
        if rows:
            self.observe(rows)
        _register_local_process_owner(self)

    def observe(self, rows: "dict[int, tuple[int, int, str]]") -> None:
        if not rows:
            return
        with self._lock:
            if self._closed:
                return
            live: "dict[int, str]" = {}
            for pid, started in self._tracked.items():
                row = rows.get(pid)
                if row is None:
                    continue
                if started and row[2] != started:
                    continue
                live[pid] = row[2]

            # The initial snapshot can rarely race process startup. Bind the root
            # to its real start identity on the first successful observation.
            if self.root_pid in rows and self.root_pid not in live:
                previous = self._tracked.get(self.root_pid)
                if previous == "":
                    live[self.root_pid] = rows[self.root_pid][2]

            # Repeat until grandchildren and deeper descendants from this same
            # process-table snapshot have all been adopted.
            changed = True
            while changed:
                changed = False
                live_pids = set(live)
                for pid, (ppid, _pgid, started) in rows.items():
                    if pid in live or ppid not in live_pids:
                        continue
                    live[pid] = started
                    changed = True
            self._tracked = live

    def _targets(
        self, rows: "dict[int, tuple[int, int, str]]"
    ) -> "tuple[set[int], dict[int, int]]":
        cwd_prefix = self.cwd + os.sep
        cwd_matches = {
            pid for pid, process_cwd in _local_process_cwds().items()
            if process_cwd == self.cwd or process_cwd.startswith(cwd_prefix)
        }
        if cwd_matches:
            with self._lock:
                if not self._closed:
                    for pid in cwd_matches:
                        row = rows.get(pid)
                        if row is not None:
                            self._tracked[pid] = row[2]
        self.observe(rows)
        with self._lock:
            tracked = dict(self._tracked)
        own_pid = os.getpid()
        try:
            own_pgid = os.getpgrp()
        except Exception:
            own_pgid = -1
        pids: "dict[int, int]" = {}
        pgids: "set[int]" = set()
        for pid, started in tracked.items():
            row = rows.get(pid)
            if row is None or (started and row[2] != started) or pid == own_pid:
                continue
            pgid = row[1]
            pids[pid] = pgid
            if pgid > 1 and pgid != own_pgid:
                pgids.add(pgid)
        return pgids, pids

    def signal(self, sig: int) -> bool:
        """Signal every live process and process group owned by this invocation."""
        with self._lock:
            if self._closed:
                return True
        rows = _local_process_table()
        if not rows:
            # Retain the original process-group fallback when ``ps`` is temporarily
            # unavailable; this still covers the ordinary non-detached path.
            try:
                os.killpg(os.getpgid(self.root_pid), sig)
                return True
            except ProcessLookupError:
                return True
            except Exception:
                return False
        pgids, pids = self._targets(rows)
        if not pids:
            return True

        failed_pgids: "set[int]" = set()
        hard_failure = False
        for pgid in pgids:
            try:
                os.killpg(pgid, sig)
            except ProcessLookupError:
                failed_pgids.add(pgid)
            except Exception:
                failed_pgids.add(pgid)
                hard_failure = True
        # A PID fallback covers a group that disappeared between the sample and
        # killpg, as well as any process with an unusable group id.
        for pid, pgid in pids.items():
            if pgid in pgids and pgid not in failed_pgids:
                continue
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                continue
            except Exception:
                hard_failure = True
        return not hard_failure

    def kill_leftovers(self) -> None:
        # Two samples close the small race where a child is created at the same
        # time the parent exits.  The observer retains an adopted child after it is
        # reparented, so the second signal still reaches it.
        self.signal(signal.SIGKILL)
        time.sleep(0.06)
        self.signal(signal.SIGKILL)

    def has_live_processes(self) -> bool:
        rows = _local_process_table()
        if rows:
            self.observe(rows)
        with self._lock:
            return bool(self._tracked)

    def close(self) -> None:
        self.kill_leftovers()
        with self._lock:
            self._closed = True
            self._tracked.clear()
        _unregister_local_process_owner(self)


def _descendant_pids(root_pid: int) -> "list[int]":
    """Every descendant PID of root_pid (depth-first), via `ps -axo pid=,ppid=`.

    killpg only reaches the worker's ORIGINAL process group. A child that calls
    setsid() (a backgrounded daemon, `docker run -d`'s client, an agent helper
    that detaches) becomes its own group leader and survives killpg — it gets
    reparented to init and keeps running, holding CPU / ports / a concurrency
    slot (the "worker shows closed but its process is still alive" leak). We walk
    the live ppid table to catch those escapees too. Best-effort; [] on any error.
    """
    try:
        rows = _local_process_table()
    except Exception:
        return []
    children: "dict[int, list[int]]" = {}
    for pid, (ppid, _pgid, _started) in rows.items():
        children.setdefault(ppid, []).append(pid)
    out_pids: "list[int]" = []
    stack = list(children.get(root_pid, []))
    seen: "set[int]" = set()
    while stack:
        pid = stack.pop()
        if pid in seen or pid == root_pid:
            continue
        seen.add(pid)
        out_pids.append(pid)
        stack.extend(children.get(pid, []))
    return out_pids


def _kill_proc_tree(proc: "subprocess.Popen", *, pgid: "Optional[int]" = None) -> None:
    """Kill a worker AND its full descendant tree, then REAP it.

    The CLI agent spawns helpers (curl, sh, python, docker); killing only the
    parent can leave a child holding the stdout pipe or running detached. Three
    layers, each best-effort:
      1. os.killpg(SIGKILL) on the worker's process group (start_new_session=True
         makes the worker a group leader, so this takes down everything that
         stayed in the group at once);
      2. enumerate every descendant PID via the live ppid table and SIGKILL each
         individually — this catches children that setsid()'d out of the group
         (the orphan/leak case killpg alone misses);
      3. proc.wait() to reap the parent so it doesn't linger as a <defunct>
         zombie occupying a process-table slot.
    """
    owner = getattr(proc, "_muteki_process_owner", None)
    if isinstance(owner, _LocalProcessOwner):
        owner.signal(signal.SIGKILL)

    # 2 first: snapshot descendants BEFORE killpg, since killpg + reparent can
    # mutate the ppid table out from under us.
    descendants = _descendant_pids(proc.pid)
    try:
        target_pgid = pgid if pgid is not None else os.getpgid(proc.pid)
        os.killpg(target_pgid, signal.SIGKILL)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass
    for pid in descendants:
        try:
            os.kill(pid, signal.SIGKILL)
        except Exception:
            pass
    # reap the parent (avoid a defunct zombie). short timeout: it's been SIGKILL'd.
    try:
        proc.wait(timeout=5)
    except Exception:
        pass


def run_cli(driver: CliDriver, argv: list[str], *, cwd: str, timeout: int,
            env: Optional[dict] = None, container: "Optional[object]" = None,
            stdin_text: "Optional[str]" = None) -> CliResult:
    """Run a CLI driver's argv as a subprocess and parse the result. `env`, if
    given, OVERLAYS os.environ (so the worker inherits PATH etc. plus our vars).

    `container`: if a ContainerHandle is given, the worker runs INSIDE that
    isolated Docker container (can't read the host bench tree) instead of bare on
    the host. None → host subprocess (default, unchanged)."""
    # engine-default env (pi/omp offline toggles) sits UNDER any credential overlay.
    # getattr: duck-typed driver doubles predate the hook.
    _env_extra = getattr(driver, "env_extra", None)
    extra = _env_extra() if callable(_env_extra) else {}
    if extra:
        env = {**extra, **(env or {})}
    if container is not None:
        from muteki.solver.container_exec import run_cli_container
        return run_cli_container(driver, argv, handle=container, cwd=cwd,
                                 timeout=timeout, env=env, stdin_text=stdin_text)
    t0 = time.time()
    run_env = {**os.environ, **env} if env else None
    try:
        input_kwargs = ({"input": stdin_text} if stdin_text is not None
                        else {"stdin": subprocess.DEVNULL})
        proc = subprocess.run(
            argv, cwd=cwd, capture_output=True, text=True, encoding="utf-8",
            errors="replace", timeout=timeout, env=run_env, **input_kwargs)
    except subprocess.TimeoutExpired as e:
        out = e.stdout if isinstance(e.stdout, str) else ""
        err = e.stderr if isinstance(e.stderr, str) else ""
        res = driver.parse(out or "", err or "")
        res.timed_out = True
        res.elapsed_s = time.time() - t0
        return res
    res = driver.parse(proc.stdout or "", proc.stderr or "")
    res.returncode = proc.returncode
    res.elapsed_s = time.time() - t0
    return res


def run_cli_streaming(
    driver: CliDriver, argv: list[str], *, cwd: str, timeout: int,
    on_step: "Callable[[StreamStep], None]", env: Optional[dict] = None,
    inherit_env: bool = True,
    cancel_event: "Optional[threading.Event]" = None,
    on_proc: "Optional[Callable[[subprocess.Popen], None]]" = None,
    on_start_uncertain: "Optional[Callable[[], None]]" = None,
    on_stdin_delivered: "Optional[Callable[[], None]]" = None,
    on_stdin_uncertain: "Optional[Callable[[], None]]" = None,
    steer_event: "Optional[threading.Event]" = None,
    paused_event: "Optional[threading.Event]" = None,
    container: "Optional[object]" = None,
    stdin_text: "Optional[str]" = None,
    popen_wrapper: "Optional[Callable[[Callable[[], subprocess.Popen]], subprocess.Popen]]" = None,
    on_raw_streams: "Optional[Callable[[str, str], None]]" = None,
) -> CliResult:
    """Like run_cli, but reads stdout LINE BY LINE and fires on_step(StreamStep)
    for each parsed line as it arrives — so a caller can surface live progress.
    The full stdout is still accumulated and run through driver.parse() for the
    final CliResult (flag/cost/session), identical to the non-streaming path.
    `env`, if given, normally overlays ``os.environ``.  A host authority may set
    ``inherit_env=False`` when it has sealed a complete launch environment rather
    than an override set.

    Runtime control (dispatcher control over a stateless worker subprocess):
      - `cancel_event`: when set, the subprocess is KILLED immediately (not just
        the asyncio task — that left the CLI agent running, see bug #2). A watcher
        thread kills it the instant the event fires, even if the model is mid-think
        and stdout is quiet (the per-line loop alone could wait minutes).
      - `on_proc`: invoked once with the live Popen so the caller can SIGSTOP /
        SIGCONT it for HITL pause/resume. The worker keeps the same PID, so a paused
        agent is genuinely frozen, not killed.
      - `stdin_text`: optional in-memory prompt transported through a private pipe,
        never argv. `on_stdin_delivered` fires only after the whole payload is
        accepted by that pipe; write/close failure or an unjoined writer fires
        `on_stdin_uncertain` instead. At most one of those callbacks is emitted.
      - `paused_event`: set by the caller while the worker is SIGSTOP-frozen (HITL
        pause). The timeout is computed against wall-clock MINUS time spent paused, so
        a long operator pause can't trip the turn timeout and mislabel a deliberately
        frozen worker as `timed_out` (M7).
      - `steer_event`: like cancel, but means END THIS PASS without marking the worker
        dead — an operator hint/redirect/focus cuts the current pass so the swarm can
        respawn a worker that picks up the queued guidance. The subprocess is killed
        and res.steered=True; there is NO resume loop (single-shot), so the caller does
        not reconnect — steered only keeps the session id from being downgraded.
        cancel_event takes PRECEDENCE: a stop during a steer must still die.

    `container`: if a ContainerHandle is given, the worker runs INSIDE that
    isolated Docker container; all control (cancel/steer/pause) routes in via
    `docker exec kill`. None → host subprocess (default, unchanged).
    """
    # engine-default env (pi/omp offline toggles) sits UNDER any credential overlay.
    # getattr: duck-typed driver doubles predate the hook.
    _env_extra = getattr(driver, "env_extra", None)
    extra = _env_extra() if callable(_env_extra) else {}
    if extra:
        env = {**extra, **(env or {})}
    if container is not None:
        from muteki.solver.container_exec import run_cli_streaming_container
        delivery_kwargs = {}
        if on_stdin_delivered is not None:
            delivery_kwargs["on_stdin_delivered"] = on_stdin_delivered
        if on_stdin_uncertain is not None:
            delivery_kwargs["on_stdin_uncertain"] = on_stdin_uncertain
        return run_cli_streaming_container(
            driver, argv, handle=container, cwd=cwd, timeout=timeout,
            on_step=on_step, env=env, cancel_event=cancel_event,
            on_proc=on_proc, on_start_uncertain=on_start_uncertain,
            steer_event=steer_event, paused_event=paused_event,
            stdin_text=stdin_text, **delivery_kwargs)
    import subprocess as _sp

    if type(inherit_env) is not bool:
        raise TypeError("inherit_env must be an exact boolean")

    t0 = time.time()
    # M7: pause-aware timeout. `paused_accum` is the total wall-clock the worker spent
    # SIGSTOP-frozen by the operator; `pause_since` marks the start of the current
    # freeze (None when running). active_elapsed() subtracts paused time so a paused
    # worker can't be killed as `timed_out`. _pause_lock guards the two counters since
    # the watcher thread and the read loop both call active_elapsed().
    _pause_lock = threading.Lock()
    _pause_state = {"accum": 0.0, "since": None}  # mutated under _pause_lock

    def active_elapsed() -> float:
        """Wall-clock since t0 MINUS time spent paused. Folds the in-progress freeze
        in live so a worker paused RIGHT NOW doesn't keep accruing toward timeout."""
        now = time.time()
        if paused_event is not None and paused_event.is_set():
            with _pause_lock:
                if _pause_state["since"] is None:
                    _pause_state["since"] = now          # freeze just began
                paused = _pause_state["accum"] + (now - _pause_state["since"])
        else:
            with _pause_lock:
                if _pause_state["since"] is not None:    # freeze just ended → bank it
                    _pause_state["accum"] += now - _pause_state["since"]
                    _pause_state["since"] = None
                paused = _pause_state["accum"]
        return (now - t0) - paused

    run_env = (
        ({**os.environ, **(env or {})} if env else None)
        if inherit_env
        else dict(env or {})
    )
    # start_new_session=True puts the worker (and every descendant — the CLI agent
    # spawns curl/python/sh helpers) in its OWN process group. Killing just the
    # parent leaves a `sleep`/`curl` child holding the stdout pipe open, so the read
    # loop blocks until timeout (the deeper form of bug #2). We kill the whole GROUP.
    def _spawn_local() -> "subprocess.Popen":
        child = _sp.Popen(
            argv,
            cwd=cwd,
            stdout=_sp.PIPE,
            stderr=_sp.PIPE,
            stdin=(_sp.PIPE if stdin_text is not None else _sp.DEVNULL),
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
            env=run_env,
            start_new_session=True,
        )
        # Register inside the spawn callback. The C6 wrapper may perform work after
        # Popen returns and before this function's caller regains control; observing
        # here keeps that interval inside the ownership boundary.
        setattr(child, "_muteki_process_owner", _LocalProcessOwner(child, cwd=cwd))
        return child

    # The C6 host adapter supplies this one narrowly-scoped wrapper so it can hold
    # its interlock across the actual Popen instruction and its immediate canonical
    # start receipt.  Ordinary worker execution keeps the historical direct path.
    proc = popen_wrapper(_spawn_local) if popen_wrapper is not None else _spawn_local()
    process_owner = getattr(proc, "_muteki_process_owner", None)
    try:
        proc_pgid: "Optional[int]" = os.getpgid(proc.pid)
    except Exception:
        proc_pgid = None
    proc_registered = True
    if on_proc is not None:
        try:
            on_proc(proc)
        except Exception:
            proc_registered = False

    # Feed the prompt only after Popen/on_proc established the disclosure fence.
    # A thread avoids deadlocking on a prompt larger than the pipe buffer while the
    # main thread concurrently drains worker output.  The text is never part of argv.
    stdin_thread: "Optional[threading.Thread]" = None
    stdin_notice_lock = threading.Lock()
    stdin_notice_sent = False

    def _notify_stdin(callback: "Optional[Callable[[], None]]") -> None:
        nonlocal stdin_notice_sent
        with stdin_notice_lock:
            if stdin_notice_sent:
                return
            stdin_notice_sent = True
        if callable(callback):
            try:
                callback()
            except Exception:
                pass

    if stdin_text is not None and proc_registered and not (
        cancel_event is not None and cancel_event.is_set()
    ):
        def _feed_stdin() -> None:
            delivered = False
            try:
                if proc.stdin is not None:
                    proc.stdin.write(stdin_text)
                    proc.stdin.close()
                    delivered = True
            except (BrokenPipeError, OSError, ValueError):
                pass
            _notify_stdin(
                on_stdin_delivered if delivered else on_stdin_uncertain)

        stdin_thread = threading.Thread(
            target=_feed_stdin, name="cli-secret-stdin", daemon=True)
        stdin_thread.start()
    elif proc.stdin is not None:
        # A pre-start cancellation or failed context journal commit kills the child
        # before disclosure.  Close the pipe without writing the secret.
        try:
            proc.stdin.close()
        except (OSError, ValueError):
            pass
        _notify_stdin(on_stdin_uncertain)

    cancelled = False
    steered = False
    timed_out = False
    # Watcher thread: kill the subprocess the moment cancel OR steer fires, AND
    # enforce the wall-clock timeout. Without it, a control signal during a long
    # model "think" (no stdout) wouldn't be observed until the next line — which may
    # never come — and, more critically, a worker that emits ZERO stdout would block
    # the `for line in proc.stdout` read loop FOREVER (the in-loop timeout check at
    # the bottom never runs because the iterator never yields). The watcher is the
    # ONLY thing that can break a silent hang, so it ALWAYS runs — its startup is
    # deliberately NOT gated on cancel/steer being present (it used to be, which left
    # a bare `run_cli_streaming(..., timeout=N)` call with no timeout enforcement at
    # all). Killing the proc tree closes stdout, which unblocks the read loop.
    watcher_stop = threading.Event()

    def _watch() -> None:
        nonlocal cancelled, steered, timed_out
        while not watcher_stop.is_set():
            # cancel takes precedence over steer: a stop during a steer must die.
            if cancel_event is not None and cancel_event.is_set():
                cancelled = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                return
            if steer_event is not None and steer_event.is_set():
                steered = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                return
            if active_elapsed() > timeout:
                # Enforce the timeout HERE: the main read loop may be blocked on a
                # silent process and can't self-time-out. Kill the tree (unblocks the
                # read loop) and mark timed_out so the result reflects it. Uses
                # pause-aware elapsed so a frozen worker isn't killed for being paused.
                timed_out = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                return
            # A CLI can exit while a detached tool command keeps an inherited pipe
            # open. End those owned commands immediately so the stdout reader and
            # runtime-exit fence can complete.
            if proc.poll() is not None:
                if isinstance(process_owner, _LocalProcessOwner):
                    process_owner.kill_leftovers()
                return
            watcher_stop.wait(0.1)

    watcher = threading.Thread(target=_watch, name="cli-control-watch", daemon=True)
    watcher.start()

    out_lines: list[str] = []
    err_lines: list[str] = []

    def _drain_stderr() -> None:
        try:
            assert proc.stderr is not None
            for err_line in proc.stderr:
                err_lines.append(err_line)
        except Exception:
            pass

    stderr_thread = threading.Thread(
        target=_drain_stderr, name="cli-stderr-drain", daemon=True)
    stderr_thread.start()
    try:
        assert proc.stdout is not None
        for line in proc.stdout:
            out_lines.append(line)
            if cancel_event is not None and cancel_event.is_set():
                cancelled = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                break
            if steer_event is not None and steer_event.is_set():
                steered = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                break
            if active_elapsed() > timeout:
                _kill_proc_tree(proc, pgid=proc_pgid)
                timed_out = True
                break
            try:
                steps = driver.parse_stream_steps(line)  # #18: ALL blocks, not just first
            except Exception:
                steps = []
            for step in steps:
                try:
                    on_step(step)
                except Exception:
                    pass  # a deck-emit failure must never kill the worker
        proc.wait(timeout=max(1, timeout - int(active_elapsed())))
    except _sp.TimeoutExpired:
        _kill_proc_tree(proc, pgid=proc_pgid)
        timed_out = True
    except Exception:
        _kill_proc_tree(proc, pgid=proc_pgid)
    finally:
        watcher_stop.set()
        if watcher is not None:
            watcher.join(timeout=1)
        # Some CLIs spawn sidecars that inherit stderr and outlive the parent. A
        # blocking proc.stderr.read() here keeps the worker task alive forever even
        # though the CLI parent is gone, so drain stderr in a thread and tear down
        # any leftover process-group holders if EOF does not arrive promptly.
        stderr_thread.join(timeout=1)
        if stderr_thread.is_alive():
            _kill_proc_tree(proc, pgid=proc_pgid)
            try:
                if proc.stderr:
                    proc.stderr.close()
            except Exception:
                pass
            stderr_thread.join(timeout=1)
        if stdin_thread is not None:
            stdin_thread.join(timeout=1)
            if stdin_thread.is_alive():
                _notify_stdin(on_stdin_uncertain)
        if isinstance(process_owner, _LocalProcessOwner):
            process_owner.close()
    stdout = "".join(out_lines)
    stderr = "".join(err_lines)
    # The callback is deliberately after both pipes reached their bounded terminal
    # and before driver parsing can discard or normalize any content.  C6 uses this
    # narrow host-only seam to seal the exact text observed by this audited Popen
    # reader.  Callback failure propagates: an execution whose evidence could not be
    # sealed is UNKNOWN, never a successful unaccounted observation.
    if on_raw_streams is not None:
        on_raw_streams(stdout, stderr)
    res = driver.parse(stdout, stderr or "")
    res.returncode = proc.returncode
    res.timed_out = timed_out
    res.cancelled = cancelled
    res.steered = steered
    res.elapsed_s = time.time() - t0
    return res
