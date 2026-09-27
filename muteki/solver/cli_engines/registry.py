"""Driver registry and profile/endpoint wrappers. Moved from cli_driver.py."""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any, Optional

from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    normalize_reasoning_effort,
    profile_uses_endpoint,
)

from muteki.solver.cli_engines.argv import (
    _insert_model_arg, apply_reasoning_effort, _claude_endpoint_model_env, _endpoint_api_model,
)
from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.engines.claude import ClaudeCodeDriver
from muteki.solver.cli_engines.engines.codex import CodexDriver
from muteki.solver.cli_engines.engines.cursor import CursorDriver
from muteki.solver.cli_engines.engines.devin import DevinDriver
from muteki.solver.cli_engines.engines.grok import GrokDriver
from muteki.solver.cli_engines.engines.kimi import KimiCodeDriver
from muteki.solver.cli_engines.engines.omp import OhMyPiDriver
from muteki.solver.cli_engines.engines.opencode import OpenCodeDriver
from muteki.solver.cli_engines.engines.pi import PiDriver
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH,
)

DRIVERS: dict[str, CliDriver] = {
    "claude": ClaudeCodeDriver(),
    "codex": CodexDriver(),
    "cursor": CursorDriver(),
    "devin": DevinDriver(),
    "pi": PiDriver(),
    "omp": OhMyPiDriver(),
    "kimi": KimiCodeDriver(),
    "grok": GrokDriver(),
    "opencode": OpenCodeDriver(),
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

    def version_argv(self) -> list[str]:
        """沿用基础 CLI 的版本命令，避免桥接型 CLI 显示解释器版本。"""
        return self.base.version_argv()

    def native_permission_modes(self) -> tuple[str, ...]:
        return self.base.native_permission_modes()

    def native_sandbox_modes(self) -> tuple[str, ...]:
        return self.base.native_sandbox_modes()

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
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return self._with_profile_options(self.base.build_execute(
            prompt, session, web_access=web_access, kb_access=kb_access,
            stream=stream, launch=launch))

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return self._with_profile_options(self.base.build_execute_stdin(
            prompt, session, web_access=web_access, kb_access=kb_access,
            stream=stream, launch=launch))

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return self.base.secure_prompt_preflight()

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return self._with_profile_options(self.base.build_resume(
            prompt, session, web_access=web_access, kb_access=kb_access,
            stream=stream, launch=launch))

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

    def version_argv(self) -> list[str]:
        """版本属于 CLI 设备本身，与接入端点和模型配置无关。"""
        return self.base.version_argv()

    def native_permission_modes(self) -> tuple[str, ...]:
        return self.base.native_permission_modes()

    def native_sandbox_modes(self) -> tuple[str, ...]:
        return self.base.native_sandbox_modes()

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
        # Name kept for call sites; insertion is *after* the verb (see
        # inject_codex_provider_flags) so --ignore-user-config still honors -c.
        from muteki.solver.cli_engines.codex_provider import (
            inject_codex_provider_flags,
        )
        return inject_codex_provider_flags(argv, self._codex_config_flags())

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return self._with_profile_options(self._inject_before_exec(
            self.base.build_execute(
                prompt, session, web_access=web_access,
                kb_access=kb_access, stream=stream, launch=launch)))

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return self._with_profile_options(self._inject_before_exec(
            self.base.build_execute_stdin(
                prompt, session, web_access=web_access,
                kb_access=kb_access, stream=stream, launch=launch)))

    def secure_prompt_preflight(self) -> "tuple[bool, str]":
        return self.base.secure_prompt_preflight()

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        return self._with_profile_options(self._inject_before_exec(
            self.base.build_resume(
                prompt, session, web_access=web_access,
                kb_access=kb_access, stream=stream, launch=launch)))

    def parse(self, stdout: str, stderr: str) -> CliResult:
        return self.base.parse(stdout, stderr)

    def parse_stream_line(self, line: str) -> Optional["StreamStep"]:
        return self.base.parse_stream_line(line)

    def parse_stream_steps(self, line: str) -> list["StreamStep"]:
        return self.base.parse_stream_steps(line)

    def _hello_argv(self) -> list[str]:
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

        key = self._api_key(probe_env)
        key_env = {
            "claude": "ANTHROPIC_API_KEY",
            "codex": "OPENAI_API_KEY",
            "cursor": "CURSOR_API_KEY",
            "pi": "OPENAI_API_KEY",
            "omp": "OPENAI_API_KEY",
            "opencode": "OPENAI_API_KEY",
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
