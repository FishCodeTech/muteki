"""Static worker model choices plus a real selected-model probe.

The catalog is intentionally static. Dynamic provider discovery was too heavy and
too inconsistent across subscription CLIs; operators can still type a custom
model id and validate it with the probe.
"""

from __future__ import annotations

import json
import os
import subprocess
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterator

from muteki.solver.cli_driver import driver_for
from muteki.solver.credential_accounts import account_store_root, runtime_env_for_engine
from muteki.solver.worker_profiles import base_engine_for_profile, profile_uses_endpoint


ModelOption = dict[str, str]

WORKER_MODEL_OPTIONS: dict[str, list[ModelOption]] = {
    "claude": [
        {"id": "sonnet", "label": "Sonnet (alias)"},
        {"id": "opus", "label": "Opus (alias)"},
        {"id": "fable", "label": "Fable (alias)"},
        {"id": "claude-sonnet-4-6", "label": "Claude Sonnet 4.6"},
        {"id": "claude-opus-4-8", "label": "Claude Opus 4.8"},
        {"id": "claude-fable-5", "label": "Claude Fable 5"},
        {"id": "claude-sonnet-4-5-20250929", "label": "Claude Sonnet 4.5"},
    ],
    "codex": [
        {"id": "gpt-5.5", "label": "GPT-5.5"},
        {"id": "gpt-5.4", "label": "GPT-5.4"},
        {"id": "gpt-5.4-mini", "label": "GPT-5.4 Mini"},
        {"id": "gpt-5.3-codex-spark", "label": "GPT-5.3 Codex Spark"},
        {"id": "gpt-5.2", "label": "GPT-5.2"},
        {"id": "gpt-5.1", "label": "GPT-5.1"},
        {"id": "gpt-5-mini", "label": "GPT-5 Mini"},
    ],
    "cursor": [
        {"id": "auto", "label": "Auto"},
        {"id": "composer-2.5-fast", "label": "Composer 2.5 Fast"},
        {"id": "composer-2.5", "label": "Composer 2.5"},
        {"id": "gpt-5.3-codex", "label": "Codex 5.3"},
        {"id": "gpt-5.3-codex-high", "label": "Codex 5.3 High"},
        {"id": "gpt-5.2", "label": "GPT-5.2"},
        {"id": "claude-4.5-sonnet", "label": "Sonnet 4.5"},
        {"id": "claude-4.5-sonnet-thinking", "label": "Sonnet 4.5 Thinking"},
    ],
}


def worker_model_options_payload() -> dict[str, Any]:
    return {"allow_custom": True, "models": WORKER_MODEL_OPTIONS}


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


@contextmanager
def _patched_env(values: dict[str, str]) -> Iterator[None]:
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


def _detail(returncode: int, stdout: str, stderr: str) -> str:
    tail = (stderr or stdout or "").strip().splitlines()
    if tail:
        return f"模型测试退出 {returncode}: {tail[-1][:160]}"
    return f"模型测试退出 {returncode}"


def probe_worker_model(
    *,
    profile: dict[str, Any],
    model: str,
    sessions_root: str | Path,
    backend: str = "local",
) -> dict[str, Any]:
    """Run one minimal turn with the selected model for this worker profile."""

    profile = dict(profile or {})
    model = str(model or "").strip()
    if model:
        profile["model"] = model
    engine = base_engine_for_profile(profile)
    account_id = str(profile.get("credential_account") or "").strip()
    # In local mode an empty credential_account means "use the host CLI login"
    # (e.g. ~/.codex), matching the live swarm worker path. Passing None here
    # would silently fall back to the default <engine>-main account and can pick
    # up a stale registered Codex home.
    resolved_account_id = account_id if account_id else ("" if backend == "local" else None)
    root = account_store_root(sessions_root)
    env = runtime_env_for_engine(
        engine,
        account_root=root,
        account_id=resolved_account_id,
        container=False,
    ).env

    with _patched_env(env):
        if profile_uses_endpoint(profile):
            ok, detail = driver_for(profile).health_detail()
            return {
                "ok": bool(ok),
                "detail": detail or ("模型可用" if ok else "模型测试失败"),
                "engine": engine,
                "model": model,
                "backend": backend if backend in ("local", "container") else "local",
            }

        drv = driver_for(profile)
        argv = _insert_model(drv._hello_argv(), model)  # noqa: SLF001 - worker probe mirrors driver self-check.
        if not argv:
            return {
                "ok": False,
                "detail": "该引擎没有可用的最小模型探针",
                "engine": engine,
                "model": model,
                "backend": backend if backend in ("local", "container") else "local",
            }
        try:
            r = subprocess.run(
                argv,
                capture_output=True,
                text=True,
                timeout=getattr(drv, "_HELLO_TIMEOUT", 90),
            )
        except FileNotFoundError:
            return {"ok": False, "detail": "CLI 不存在", "engine": engine, "model": model}
        except subprocess.TimeoutExpired:
            return {"ok": False, "detail": "模型测试超时", "engine": engine, "model": model}
        except Exception as exc:  # noqa: BLE001
            return {"ok": False, "detail": str(exc)[:160], "engine": engine, "model": model}

        ok = drv._hello_ok(r)  # noqa: SLF001 - exact same success predicate as health check.
        return {
            "ok": bool(ok),
            "detail": "模型可用" if ok else _detail(r.returncode, r.stdout, r.stderr),
            "engine": engine,
            "model": model,
            "backend": backend if backend in ("local", "container") else "local",
        }


def parse_cursor_models(text: str) -> list[ModelOption]:
    """Small parser kept for future refresh tooling and tests."""

    out: list[ModelOption] = []
    for line in text.splitlines():
        if " - " not in line or line.lower().startswith("available models"):
            continue
        mid, label = line.split(" - ", 1)
        mid = mid.strip()
        label = label.strip()
        if mid:
            out.append({"id": mid, "label": label or mid})
    return out


def parse_openai_models(text: str) -> list[ModelOption]:
    data = json.loads(text)
    models = data.get("data") if isinstance(data, dict) else None
    if not isinstance(models, list):
        return []
    out: list[ModelOption] = []
    for item in models:
        if not isinstance(item, dict):
            continue
        mid = str(item.get("id") or "").strip()
        if mid:
            out.append({"id": mid, "label": str(item.get("display_name") or mid)})
    return out
