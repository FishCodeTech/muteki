"""CliDriver base class. Moved from cli_driver.py."""
from __future__ import annotations

import abc
import json
import os
import subprocess
import threading
import time
from typing import Optional


from muteki.solver.cli_engines.bins import KB_MCP_NAME, _ENV_OVERRIDE, resolve_engine_bin
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, SecurePromptUnsupported, StreamStep, WORKER_LAUNCH,
)

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

    def version_argv(self) -> list[str]:
        """只读设备版本探测命令；特殊桥接 Driver 可以覆写。"""
        return [self.bin, "--version"]

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
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        """Build a fresh, non-persistent invocation whose prompt is read from stdin.

        This is deliberately a separate capability from ``build_execute``.  Exact
        operator secrets must never be smuggled through a positional argument (where
        sibling processes can read them from the process table), and the engine must
        offer a documented way to avoid persisting the resulting conversation.  A
        driver that cannot satisfy both requirements fails closed.
        """
        del prompt, session, web_access, kb_access, stream, launch
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

    def native_permission_modes(self) -> tuple[str, ...]:
        """Return the permission mode identifiers accepted by this CLI itself."""
        return ()

    def native_sandbox_modes(self) -> tuple[str, ...]:
        """Return the sandbox identifiers accepted by this CLI itself."""
        return ()

    def validate_launch_context(self, launch: LaunchContext) -> str:
        """Validate and return the opaque native permission identifier."""
        mode = str(launch.permission_mode or "").strip()
        if launch.interactive and mode not in ("", *self.native_permission_modes()):
            raise ValueError(
                f"{self.name} unsupported native permission mode: {mode}")
        return mode

    def validate_sandbox_mode(self, launch: LaunchContext) -> str:
        mode = str(launch.sandbox_mode or "").strip()
        if launch.interactive and mode not in ("", *self.native_sandbox_modes()):
            raise ValueError(f"{self.name} unsupported native sandbox mode: {mode}")
        return mode

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
        launch: LaunchContext = WORKER_LAUNCH,
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
        launch: LaunchContext = WORKER_LAUNCH,
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
        from muteki.solver.cli_engines.argv import apply_runtime_argv
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
        return False, last
