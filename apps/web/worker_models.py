"""Static worker model choices plus a real selected-model probe.

The catalog is intentionally static. Dynamic provider discovery was too heavy and
too inconsistent across subscription CLIs; operators can still type a custom
model id and validate it with the probe.
"""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import tempfile
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterator

from muteki.solver.cli_driver import driver_for
from muteki.solver.credential_accounts import (
    CONTAINER_ACCOUNTS_ROOT,
    CredentialAccountStore,
    account_store_root,
    project_account_root,
    runtime_env_for_engine,
)
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

_CONTAINER_BIN = {
    "claude": "claude",
    "codex": "codex",
    "cursor": "/home/kali/.local/bin/cursor-agent",
}

_CONTAINER_BASE_ENV = {
    "PATH": "/home/kali/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME": "/home/kali",
    "USER": "kali",
    "LOGNAME": "kali",
    "LANG": "C.UTF-8",
    "PYTHONUNBUFFERED": "1",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
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


def _docker(*args: str, timeout: float = 30.0) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["docker", *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
    )


def _containerize_argv(engine: str, argv: list[str]) -> list[str]:
    if not argv:
        return argv
    bin_in = _CONTAINER_BIN.get(engine)
    return [bin_in or os.path.basename(argv[0]), *argv[1:]]


def _probe_ok(profile: dict[str, Any], r: subprocess.CompletedProcess) -> bool:
    drv = driver_for(profile)
    # EndpointDriver's build_execute output is still the base engine's envelope.
    # Use the base checker when present so codex keeps its tolerant JSONL success
    # predicate instead of the generic "rc 0 + non-empty stdout" fallback.
    checker = getattr(drv, "base", drv)
    return bool(checker._hello_ok(r))  # noqa: SLF001


def _probe_argv_for_profile(profile: dict[str, Any], engine: str, model: str) -> list[str]:
    drv = driver_for(profile)
    argv = drv._hello_argv()  # noqa: SLF001 - same minimal model turn as health checks.
    if not argv:
        prompt = getattr(drv, "HELLO_PROMPT", "Reply with exactly: OK")
        argv = drv.build_execute(
            prompt, None, web_access=False, kb_access=False, stream=False
        )
        # EndpointDriver injects codex's provider/model config itself, but claude
        # endpoint profiles still need the regular --model flag on the CLI argv.
        if not (profile_uses_endpoint(profile) and engine == "codex"):
            argv = _insert_model(argv, model)
    return _containerize_argv(engine, argv)


def _worker_container_model_probe(
    *,
    profile: dict[str, Any],
    model: str,
    sessions_root: str | Path,
    engine: str,
) -> dict[str, Any]:
    """Run the selected profile/model inside the actual worker image.

    This is intentionally a one-shot `docker run --rm`, not the long-lived
    per-run supervisor container: the settings button needs a fresh, bounded
    validation that the worker image, projected credentials, network, CLI, and
    selected model can complete one minimal turn.
    """

    from muteki.solver.container_exec import (
        CONTAINER_WORKSPACE,
        WORKER_IMAGE,
        _HOST_DATA_ROOT,
        _mount_source,
    )

    root = account_store_root(sessions_root)
    account_id = str(profile.get("credential_account") or "").strip() or None
    resolved = runtime_env_for_engine(
        engine,
        account_root=root,
        account_id=account_id,
        container=True,
    )
    effective_account_id = resolved.account_id
    acct = CredentialAccountStore(root).inspect(effective_account_id)
    if acct is None or not acct.present:
        return {
            "ok": False,
            "detail": f"容器模型测试需要已登记账号: {effective_account_id}",
            "engine": engine,
            "model": model,
            "backend": "container",
            "layer": "auth",
        }

    try:
        img = _docker("image", "inspect", WORKER_IMAGE, timeout=20)
    except FileNotFoundError:
        return {"ok": False, "detail": "docker 不可用", "engine": engine, "model": model, "backend": "container"}
    except subprocess.TimeoutExpired:
        return {"ok": False, "detail": "docker image inspect 超时", "engine": engine, "model": model, "backend": "container"}
    if img.returncode != 0:
        return {
            "ok": False,
            "detail": f"worker 镜像缺失或不可用: {WORKER_IMAGE}",
            "engine": engine,
            "model": model,
            "backend": "container",
            "layer": "image",
        }

    tmp_base = None
    if _HOST_DATA_ROOT:
        tmp_base = os.path.join(
            os.environ.get("MUTEKI_CONTAINER_DATA_ROOT") or _HOST_DATA_ROOT,
            "_tmp",
            "model-tests",
        )
        try:
            os.makedirs(tmp_base, exist_ok=True)
        except OSError:
            tmp_base = None

    with tempfile.TemporaryDirectory(prefix="muteki-model-test-", dir=tmp_base) as td:
        workspace = os.path.join(td, "ws")
        projection = os.path.join(td, "accounts")
        os.makedirs(workspace, exist_ok=True)
        try:
            os.chmod(workspace, 0o777)
        except OSError:
            pass
        try:
            project_account_root(root, projection)
        except OSError as exc:
            return {
                "ok": False,
                "detail": f"凭据投影失败: {str(exc)[:120]}",
                "engine": engine,
                "model": model,
                "backend": "container",
                "layer": "mount",
            }

        argv = _probe_argv_for_profile(profile, engine, model)
        if not argv:
            return {
                "ok": False,
                "detail": "该引擎没有可用的容器内模型探针",
                "engine": engine,
                "model": model,
                "backend": "container",
            }

        env = {**_CONTAINER_BASE_ENV, **resolved.env}
        prelude = [
            'if [ -n "$MUTEKI_CODEX_HOME_SEED" ] && [ -d "$MUTEKI_CODEX_HOME_SEED" ]; then '
            'export CODEX_HOME="${CODEX_HOME:-$HOME/.codex-muteki-model-test}"; '
            'rm -rf "$CODEX_HOME"; mkdir -p "$CODEX_HOME"; '
            'cp -R "$MUTEKI_CODEX_HOME_SEED"/. "$CODEX_HOME"/; '
            'chmod -R u+rwX "$CODEX_HOME"; fi',
            'if [ -r "$CLAUDE_CODE_OAUTH_TOKEN_FILE" ]; then '
            'export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE")"; fi',
            'if [ -r "$CURSOR_API_KEY_FILE" ]; then '
            'export CURSOR_API_KEY="$(cat "$CURSOR_API_KEY_FILE")"; fi',
            'if [ -r "$ANTHROPIC_API_KEY_FILE" ]; then '
            'export ANTHROPIC_API_KEY="$(cat "$ANTHROPIC_API_KEY_FILE")"; fi',
            'if [ -r "$OPENAI_API_KEY_FILE" ]; then '
            'export OPENAI_API_KEY="$(cat "$OPENAI_API_KEY_FILE")"; fi',
        ]
        timeout_s = max(1, int(getattr(driver_for(profile), "_HELLO_TIMEOUT", 90)))
        script = (
            "; ".join(prelude)
            + f"; exec timeout -s KILL {timeout_s}s {shlex.join(argv)} < /dev/null"
        )

        network = (os.environ.get("MUTEKI_WORKER_NETWORK") or "bridge").strip() or "bridge"
        run_cmd = [
            "run", "--rm", "--init",
            "--network", network,
            "--user", "kali",
            "--workdir", CONTAINER_WORKSPACE,
            "--entrypoint", "bash",
            "--mount", f"type=bind,source={_mount_source(workspace)},target={CONTAINER_WORKSPACE}",
            "--mount", f"type=bind,source={_mount_source(projection)},target={CONTAINER_ACCOUNTS_ROOT}",
        ]
        if network != "host":
            run_cmd += ["--add-host", "host.docker.internal:host-gateway"]
        for k, v in env.items():
            run_cmd += ["-e", f"{k}={v}"]
        run_cmd += [WORKER_IMAGE, "-lc", script]

        try:
            run = _docker(*run_cmd, timeout=timeout_s + 30)
        except FileNotFoundError:
            return {
                "ok": False,
                "detail": "docker 不可用",
                "engine": engine,
                "model": model,
                "backend": "container",
                "layer": "image",
            }
        except subprocess.TimeoutExpired:
            return {
                "ok": False,
                "detail": f"worker 容器模型测试超时（>{timeout_s}s）",
                "engine": engine,
                "model": model,
                "backend": "container",
                "layer": "auth",
            }

    ok = _probe_ok(profile, run)
    if ok:
        return {
            "ok": True,
            "detail": "worker 容器内模型可用（已完成真实 hello）",
            "engine": engine,
            "model": model,
            "backend": "container",
        }
    return {
        "ok": False,
        "detail": "worker 容器模型测试失败: "
        + _detail(run.returncode, run.stdout, run.stderr),
        "engine": engine,
        "model": model,
        "backend": "container",
        "layer": "auth",
    }


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

    # In compose deploys the web container does not ship engine CLIs; run the
    # selected profile/model in the worker image instead of shelling the host/web
    # filesystem. This spends one minimal model turn by design: the operator
    # explicitly clicked "test model".
    from muteki.core.runtime_env import is_web_container

    if backend == "container" and is_web_container():
        return _worker_container_model_probe(
            profile=profile,
            model=model,
            sessions_root=sessions_root,
            engine=engine,
        )

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
                encoding="utf-8", errors="replace",  # P2-v3: UTF-8, not host codepage
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
