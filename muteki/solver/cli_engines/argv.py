"""Shared CLI argv construction. Moved from cli_driver.py."""
from __future__ import annotations

import json
import re
from typing import Any

from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    normalize_reasoning_effort,
    profile_uses_endpoint,
)

from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.types import CliResult  # noqa: F401

def _insert_before_prompt(argv: list[str], extra: list[str], *, engine: str = "") -> list[str]:
    if not extra:
        return argv
    if engine in {"kimi", "grok", "devin"}:
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
    argv: list[str], *, engine: str, reasoning_effort: str, native: bool = False,
) -> list[str]:
    """Translate one persisted effort value into the selected CLI's syntax."""
    effort = str(reasoning_effort or "default").strip() if native else normalize_reasoning_effort(reasoning_effort, "default")
    if effort == "default":
        return argv
    if native:
        if engine not in _ENGINE_REASONING_EFFORTS or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", effort):
            raise ValueError("Unsupported native reasoning option")
    elif effort not in _ENGINE_REASONING_EFFORTS.get(engine, set()):
        return argv
    if engine == "claude" and effort in {"off", "on"}:
        settings = {"alwaysThinkingEnabled": effort == "on"}
        if "--settings" in argv:
            index = argv.index("--settings") + 1
            current = json.loads(argv[index])
            return [*argv[:index], json.dumps({**current, **settings}), *argv[index + 1:]]
        return _insert_before_prompt(argv, ["--settings", json.dumps(settings)], engine=engine)
    if engine == "codex":
        if any("model_reasoning_effort=" in str(arg) for arg in argv):
            return argv
        flag = f'model_reasoning_effort="{effort}"'
        # Subcommand -c must follow exec (same as provider flags).
        try:
            idx = argv.index("exec")
        except ValueError:
            return [argv[0], "-c", flag, *argv[1:]] if argv else ["-c", flag]
        return [*argv[:idx + 1], "-c", flag, *argv[idx + 1:]]
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
    if engine == "opencode":
        # OpenCode 的调用级 policy config 原本通过 argv 中的 env 前缀覆盖了
        # Credential Account 提供的 custom provider，导致有效 Key/端点丢失。
        # 在最终 argv 冻结前合并二者；provider 配置只引用环境变量名，Key
        # 本身仍留在子进程环境中，不进入 argv 或日志。
        runtime_config_text = str(
            env.get("OPENCODE_CONFIG_CONTENT") or "").strip()
        if runtime_config_text:
            prefix = "OPENCODE_CONFIG_CONTENT="
            for index, arg in enumerate(out):
                if not str(arg).startswith(prefix):
                    continue
                try:
                    runtime_config = json.loads(runtime_config_text)
                except (json.JSONDecodeError, TypeError):
                    runtime_config = {}
                try:
                    invocation_config = json.loads(str(arg)[len(prefix):])
                except (json.JSONDecodeError, TypeError):
                    invocation_config = {}
                merged = dict(runtime_config) if isinstance(runtime_config, dict) else {}
                if isinstance(invocation_config, dict):
                    for key, value in invocation_config.items():
                        if isinstance(value, dict) and isinstance(merged.get(key), dict):
                            merged[key] = {**merged[key], **value}
                        else:
                            merged[key] = value
                out[index] = prefix + json.dumps(
                    merged, ensure_ascii=False, separators=(",", ":"))
                break
        provider = str(env.get("MUTEKI_OPENCODE_PROVIDER") or "").strip()
        if provider and model and not model.startswith(f"{provider}/"):
            model = f"{provider}/{model}"
            for flag in ("--model", "-m"):
                if flag in out:
                    index = out.index(flag)
                    if index + 1 < len(out):
                        out[index + 1] = model
                    break
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

    if engine == "codex":
        from muteki.solver.cli_engines.codex_provider import (
            argv_has_model_provider,
            codex_provider_spawn_args,
            inject_codex_provider_flags,
        )
        if not argv_has_model_provider(out):
            # Login CODEX_HOME + --ignore-user-config (conversation metadata)
            # skips config.toml; re-emit provider -c *after* exec from env /
            # config.toml (global -c before exec is ignored with that flag).
            out = inject_codex_provider_flags(out, codex_provider_spawn_args({
                str(k): str(v) for k, v in env.items() if v is not None
            }))

    if engine in {"pi", "omp"}:
        prefix = "MUTEKI_PI" if engine == "pi" else "MUTEKI_OMP"
        system_prompt = str(env.get(f"{prefix}_SYSTEM_PROMPT") or "").strip()
        if system_prompt:
            if "--system-prompt" in out:
                index = out.index("--system-prompt") + 1
                if index < len(out):
                    out[index] = system_prompt
            else:
                out = _insert_before_prompt(
                    out, ["--system-prompt", system_prompt], engine=engine
                )
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
