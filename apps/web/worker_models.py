"""Manually maintained worker models, on-demand discovery, and model probes.

The public catalog is reviewed and edited as a normal source file. Discovery is
an explicit operator action against the configured CLI/account environment; it
does not run on a timer and never replaces the public catalog.

Custom endpoints use an operator-provided model id and validate it with the
real model probe. Built-in connections keep the curated Worker model choices.
"""

from __future__ import annotations

import json
import os
import re
import signal
import shlex
import subprocess
import tempfile
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import ExitStack
from pathlib import Path
from typing import Any

from muteki.solver.cli_driver import (
    _redact_probe_secrets,
    apply_runtime_argv,
    driver_for,
)
from muteki.solver.credential_accounts import (
    CONTAINER_ACCOUNTS_ROOT,
    CredentialAccountStore,
    account_store_root,
    engine_account_id,
    project_account_root,
    runtime_env_for_engine,
)
from muteki.solver.worker_profiles import base_engine_for_profile, profile_uses_endpoint


ModelOption = dict[str, Any]

_MANUAL_CATALOG_PATH = Path(__file__).with_name("worker_models.manual.json")


def _read_manual_catalog() -> dict[str, Any]:
    raw = json.loads(_MANUAL_CATALOG_PATH.read_text(encoding="utf-8"))
    models = raw.get("models")
    if not isinstance(models, dict):
        raise ValueError("worker_models.manual.json must contain a models object")
    return raw


_MANUAL_CATALOG = _read_manual_catalog()
WORKER_MODEL_OPTIONS: dict[str, list[ModelOption]] = _MANUAL_CATALOG["models"]


def _manual_options(engine: str, options: list[ModelOption]) -> list[ModelOption]:
    fallback = (_MANUAL_CATALOG.get("reasoning") or {}).get(engine)
    out: list[ModelOption] = []
    for option in options:
        item = dict(option)
        if not isinstance(item.get("reasoning"), dict) and isinstance(fallback, dict):
            item["reasoning"] = dict(fallback)
        out.append(item)
    return out

_CONTAINER_BIN = {
    "claude": "claude",
    "codex": "codex",
    "cursor": "/home/kali/.local/bin/cursor-agent",
    "pi": "pi",
    "omp": "/home/kali/.local/bin/omp",
    "opencode": "opencode",
    "dsh": "python3",
    "kimi": "kimi",
    "grok": "/home/kali/.grok/bin/grok",
}

_CONTAINER_OFFLINE_BRIDGE = "/opt/muteki/offline_acp_bridge.py"
_CONTAINER_OMP_OFFLINE_CONFIG = "/opt/muteki/omp_offline_config.yml"
_CONTAINER_KIMI_OFFLINE_AGENT = "/opt/muteki/kimi_offline_agent.md"
_CONTAINER_GROK_OFFLINE_AGENT = "/opt/muteki/grok_offline_agent.md"

_CONTAINER_BASE_ENV = {
    "PATH": "/home/kali/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME": "/home/kali",
    "USER": "kali",
    "LOGNAME": "kali",
    "LANG": "C.UTF-8",
    "PYTHONUNBUFFERED": "1",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
}


class WorkerModelDiscoveryStore:
    """Last successful on-demand discovery, scoped by worker profile."""

    def __init__(self, sessions_root: str | Path) -> None:
        self.path = Path(sessions_root) / "_worker_model_discovery.json"

    def read(self) -> dict[str, Any]:
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {"version": 1, "profiles": {}}
        profiles = raw.get("profiles") if isinstance(raw, dict) else None
        if not isinstance(profiles, dict):
            return {"version": 1, "profiles": {}}
        return {"version": 1, "profiles": profiles}

    def save_results(self, results: list[dict[str, Any]]) -> None:
        data = self.read()
        profiles = data["profiles"]
        for result in results:
            if not result.get("ok"):
                continue
            profile_id = str(result.get("profile_id") or "").strip()
            engine = str(result.get("engine") or "").strip()
            models = result.get("models")
            if not profile_id or not engine or not isinstance(models, list):
                continue
            profiles[profile_id] = {
                "engine": engine,
                "source": str(result.get("source") or "cli"),
                "updated_at": float(result.get("updated_at") or time.time()),
                "models": models,
            }
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_name(f".{self.path.name}.{os.getpid()}.tmp")
        tmp.write_text(
            json.dumps(data, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        tmp.replace(self.path)


def _dedupe_models(*groups: list[ModelOption]) -> list[ModelOption]:
    out: list[ModelOption] = []
    seen: set[str] = set()
    for group in groups:
        for item in group:
            mid = str(item.get("id") or "").strip()
            if not mid:
                continue
            if mid in seen:
                existing = next(row for row in out if row["id"] == mid)
                if (not isinstance(existing.get("reasoning"), dict)
                        and isinstance(item.get("reasoning"), dict)):
                    existing["reasoning"] = _dedupe_models([item])[0]["reasoning"]
                continue
            seen.add(mid)
            normalized: ModelOption = {
                "id": mid,
                "label": str(item.get("label") or mid),
            }
            reasoning = item.get("reasoning")
            if isinstance(reasoning, dict):
                levels = [
                    str(level).strip().lower()
                    for level in (reasoning.get("levels") or [])
                    if str(level).strip().lower() in {
                        "none", "minimal", "low", "medium", "high", "xhigh", "max",
                    }
                ]
                supported = bool(reasoning.get("supported", bool(levels)))
                normalized["reasoning"] = {
                    "supported": supported,
                    "levels": list(dict.fromkeys(levels)),
                    "default": str(reasoning.get("default") or "").strip().lower(),
                }
            out.append(normalized)
    return out


def worker_model_options_payload(
    sessions_root: str | Path | None = None,
) -> dict[str, Any]:
    manual = {
        engine: _dedupe_models(_manual_options(engine, options))
        for engine, options in WORKER_MODEL_OPTIONS.items()
    }
    discovered_by_engine: dict[str, list[ModelOption]] = {}
    discovered_by_profile: dict[str, list[ModelOption]] = {}
    discovery: dict[str, Any] = {}

    if sessions_root is not None:
        stored = WorkerModelDiscoveryStore(sessions_root).read()
        for profile_id, record in stored["profiles"].items():
            if not isinstance(record, dict):
                continue
            engine = str(record.get("engine") or "").strip()
            raw_models = record.get("models")
            if not engine or not isinstance(raw_models, list):
                continue
            models = _dedupe_models(raw_models)
            discovered_by_profile[str(profile_id)] = models
            discovered_by_engine[engine] = _dedupe_models(
                discovered_by_engine.get(engine, []), models
            )
            discovery[str(profile_id)] = {
                "engine": engine,
                "source": str(record.get("source") or "cli"),
                "updated_at": record.get("updated_at"),
                "count": len(models),
            }

    merged = {
        engine: _dedupe_models(
            manual.get(engine, []), discovered_by_engine.get(engine, [])
        )
        for engine in set(manual) | set(discovered_by_engine)
    }
    models_by_profile = {
        profile_id: _dedupe_models(
            manual.get(str(discovery[profile_id]["engine"]), []), models
        )
        for profile_id, models in discovered_by_profile.items()
    }
    return {
        "allow_custom": True,
        "manual_updated_at": _MANUAL_CATALOG.get("updated_at"),
        "manual_sources": _MANUAL_CATALOG.get("sources") or {},
        "manual_models": manual,
        "discovered_models": discovered_by_engine,
        "models_by_profile": models_by_profile,
        "discovery": discovery,
        "models": merged,
    }


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


def _detail(returncode: int, stdout: str, stderr: str) -> str:
    for raw in (stderr, stdout):
        for line in reversed((raw or "").strip().splitlines()):
            try:
                doc = json.loads(line)
            except (json.JSONDecodeError, TypeError):
                continue
            if not isinstance(doc, dict):
                continue
            error = doc.get("error")
            error_message = (
                str(error.get("message") or "") if isinstance(error, dict) else str(error or "")
            ).strip()
            message = str(doc.get("result") or error_message or "").strip()
            status = doc.get("api_error_status")
            if message:
                prefix = f"HTTP {status}，" if status else ""
                return f"模型测试退出 {returncode}：{prefix}{message[:300]}"
    tail = (stderr or stdout or "").strip().splitlines()
    if tail:
        return f"模型测试退出 {returncode}：{tail[-1][:300]}"
    return f"模型测试退出 {returncode}"


def _safe_output(value: Any, *, limit: int = 12000) -> str:
    """Keep model-test output useful without returning unbounded terminal data."""
    text = str(value or "").replace("\x00", "")
    if len(text) <= limit:
        return text
    return f"{text[:limit]}\n… 输出已截断（原始长度 {len(text)} 字符）"


def _test_log(stream: str, message: str, elapsed_ms: int) -> dict[str, Any]:
    return {
        "stream": stream,
        "message": _safe_output(message, limit=4000),
        "elapsed_ms": max(0, int(elapsed_ms)),
    }


def _claude_actual_models(stdout: Any) -> list[str]:
    """Read the model IDs Claude Code reports in its result envelope."""
    text = str(stdout or "").strip()
    if not text:
        return []
    docs: list[dict[str, Any]] = []
    try:
        parsed = json.loads(text)
        if isinstance(parsed, dict):
            docs.append(parsed)
    except json.JSONDecodeError:
        for line in text.splitlines():
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(parsed, dict):
                docs.append(parsed)
    found: list[str] = []
    for doc in docs:
        usage = doc.get("modelUsage") or doc.get("model_usage") or {}
        if isinstance(usage, dict):
            for value in usage:
                model_id = str(value or "").strip()
                if model_id and model_id not in found:
                    found.append(model_id)
    return found


def _model_matches(expected: str, actual: list[str]) -> bool:
    wanted = str(expected or "").strip().casefold()
    return bool(wanted) and any(item.casefold() == wanted for item in actual)


def _process_result(
    *,
    ok: bool,
    detail: str,
    engine: str,
    model: str,
    backend: str,
    command: str,
    started: float,
    returncode: int | None = None,
    stdout: Any = "",
    stderr: Any = "",
    layer: str | None = None,
    actual_models: list[str] | None = None,
) -> dict[str, Any]:
    elapsed_ms = int((time.perf_counter() - started) * 1000)
    clean_stdout = _safe_output(stdout)
    clean_stderr = _safe_output(stderr)
    logs = [
        _test_log("system", f"启动 {engine} Worker 模型测试", 0),
        _test_log("command", command, 0),
    ]
    if clean_stdout.strip():
        logs.append(_test_log("stdout", clean_stdout, elapsed_ms))
    if clean_stderr.strip():
        logs.append(_test_log("stderr", clean_stderr, elapsed_ms))
    if actual_models is not None:
        actual_text = "、".join(actual_models) if actual_models else "未返回模型 ID"
        logs.append(_test_log(
            "success" if ok else "error",
            f"配置模型：{model or 'Worker 默认模型'}；实际模型：{actual_text}",
            elapsed_ms,
        ))
    exit_message = (
        f"执行完成，退出码 {returncode}，耗时 {elapsed_ms} ms"
        if returncode is not None
        else f"执行结束，耗时 {elapsed_ms} ms"
    )
    logs.append(_test_log("success" if ok else "error", exit_message, elapsed_ms))
    result: dict[str, Any] = {
        "ok": bool(ok),
        "detail": detail,
        "engine": engine,
        "model": model,
        "backend": backend,
        "command": command,
        "stdout": clean_stdout,
        "stderr": clean_stderr,
        "exit_code": returncode,
        "elapsed_ms": elapsed_ms,
        "logs": logs,
        "actual_models": list(actual_models or []),
    }
    if layer:
        result["layer"] = layer
    return result


class ProbeCancelled(RuntimeError):
    """A task-level Worker probe was cancelled by its owning run."""


class ProbeProcessOwner:
    """Track every subprocess/container started by one task preflight.

    ``asyncio.to_thread`` cancellation does not stop the underlying thread.  The
    run therefore owns an explicit cancellation token and a set of concrete
    process handles.  Cancellation terminates the process group, removes any
    named one-shot container, and ``wait`` confirms that all registered handles
    have been reaped before the execution generation can finish.
    """

    def __init__(self) -> None:
        self._cancelled = threading.Event()
        self._condition = threading.Condition()
        self._processes: dict[subprocess.Popen, str] = {}

    @property
    def cancelled(self) -> bool:
        return self._cancelled.is_set()

    def register(self, process: subprocess.Popen, container_name: str = "") -> bool:
        with self._condition:
            self._processes[process] = container_name
            cancelled = self._cancelled.is_set()
        if cancelled:
            self._terminate(process)
        return not cancelled

    def unregister(self, process: subprocess.Popen) -> None:
        with self._condition:
            self._processes.pop(process, None)
            self._condition.notify_all()

    @staticmethod
    def _terminate(process: subprocess.Popen) -> None:
        if process.poll() is not None:
            return
        try:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGTERM)
            else:
                process.terminate()
        except (OSError, ProcessLookupError):
            pass
        try:
            process.wait(timeout=1.0)
            return
        except (subprocess.TimeoutExpired, OSError):
            pass
        try:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
        except (OSError, ProcessLookupError):
            pass

    def cancel(self) -> None:
        self._cancelled.set()
        with self._condition:
            owned = list(self._processes.items())
        for process, _container_name in owned:
            self._terminate(process)
        for _process, container_name in owned:
            if not container_name:
                continue
            try:
                subprocess.run(
                    ["docker", "rm", "-f", container_name],
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    timeout=10,
                )
            except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
                pass

    def wait(self, timeout: float = 15.0) -> bool:
        deadline = time.monotonic() + max(0.0, float(timeout))
        with self._condition:
            while self._processes:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                self._condition.wait(timeout=min(0.2, remaining))
            return True


def _run_owned_process(
    argv: list[str], *, timeout: float, owner: ProbeProcessOwner,
    env: dict[str, str] | None = None, container_name: str = "",
) -> subprocess.CompletedProcess:
    if owner.cancelled:
        raise ProbeCancelled("task preflight cancelled")
    process = subprocess.Popen(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
        start_new_session=(os.name == "posix"),
    )
    owner.register(process, container_name)
    deadline = time.monotonic() + max(0.01, float(timeout))
    try:
        while True:
            if owner.cancelled:
                owner._terminate(process)
                stdout, stderr = process.communicate()
                raise ProbeCancelled("task preflight cancelled")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                owner._terminate(process)
                stdout, stderr = process.communicate()
                raise subprocess.TimeoutExpired(
                    argv, timeout, output=stdout, stderr=stderr)
            try:
                stdout, stderr = process.communicate(timeout=min(0.2, remaining))
                return subprocess.CompletedProcess(
                    argv, process.returncode, stdout, stderr)
            except subprocess.TimeoutExpired:
                continue
    finally:
        owner.unregister(process)


def _docker(
    *args: str, timeout: float = 30.0,
    owner: ProbeProcessOwner | None = None, container_name: str = "",
) -> subprocess.CompletedProcess:
    argv = ["docker", *args]
    if owner is not None:
        return _run_owned_process(
            argv, timeout=timeout, owner=owner,
            container_name=container_name)
    return subprocess.run(
        argv, capture_output=True, text=True, encoding="utf-8",
        errors="replace", timeout=timeout)


def _containerize_argv(engine: str, argv: list[str]) -> list[str]:
    if not argv:
        return argv
    bin_in = _CONTAINER_BIN.get(engine)
    out = list(argv)
    if len(out) >= 2 and os.path.basename(out[1]) == "offline_acp_bridge.py":
        out[0] = "python3"
        out[1] = _CONTAINER_OFFLINE_BRIDGE
        for index, arg in enumerate(out):
            if arg == "--agent-bin" and index + 1 < len(out):
                out[index + 1] = bin_in or os.path.basename(out[index + 1])
            elif arg.startswith("--agent-arg=") and os.path.basename(
                    arg.removeprefix("--agent-arg=")) == "omp_offline_config.yml":
                out[index] = f"--agent-arg={_CONTAINER_OMP_OFFLINE_CONFIG}"
        return out
    if engine == "opencode" and len(out) >= 3 and out[0] == "env":
        out[2] = bin_in or "opencode"
    elif engine == "grok" and len(out) >= 4 and out[0] == "env":
        out[3] = bin_in or "grok"
    else:
        out[0] = bin_in or os.path.basename(out[0])
    if engine == "dsh" and len(out) >= 2:
        out[1] = "/opt/muteki/deepseek_harness_worker.py"
    if engine == "kimi":
        for index, arg in enumerate(out[:-1]):
            if arg == "--agent-file":
                out[index + 1] = _CONTAINER_KIMI_OFFLINE_AGENT
    if engine == "grok":
        for index, arg in enumerate(out[:-1]):
            if arg == "--agent":
                out[index + 1] = _CONTAINER_GROK_OFFLINE_AGENT
    return out


def _probe_ok(profile: dict[str, Any], r: subprocess.CompletedProcess) -> bool:
    drv = driver_for(profile)
    # EndpointDriver's build_execute output is still the base engine's envelope.
    # Use the base checker when present so codex keeps its tolerant JSONL success
    # predicate instead of the generic "rc 0 + non-empty stdout" fallback.
    checker = getattr(drv, "base", drv)
    return bool(checker._hello_ok(r))  # noqa: SLF001


def _probe_argv_for_profile(
    profile: dict[str, Any], engine: str, model: str, *,
    runtime_env: dict[str, Any] | None = None,
    container: bool = False,
) -> list[str]:
    drv = driver_for(profile)
    argv = drv._hello_argv()  # noqa: SLF001 - same minimal model turn as health checks.
    if not argv:
        prompt = getattr(drv, "HELLO_PROMPT", "Reply with exactly: OK")
        argv = drv.build_execute(
            prompt, None, web_access=False, kb_access=False, stream=False
        )
        # EndpointDriver injects Codex provider/model flags itself. Claude Code
        # custom endpoints receive model selection through their shared
        # ANTHROPIC_* environment mapping.
        model_from_env = engine == "claude" and bool(
            (getattr(drv, "env_extra", lambda: {})() or {}).get("ANTHROPIC_MODEL")
        )
        if not (profile_uses_endpoint(profile) and engine == "codex") and not model_from_env:
            argv = _insert_model(argv, model)
    argv = apply_runtime_argv(
        argv,
        driver=drv,
        env={
            **(runtime_env or {}),
            "MUTEKI_WORKER_MODEL": model,
            "MUTEKI_WORKER_REASONING_EFFORT": str(
                profile.get("reasoning_effort") or "default"),
        },
    )
    return _containerize_argv(engine, argv) if container else argv


def _worker_container_model_probe(
    *,
    profile: dict[str, Any],
    model: str,
    sessions_root: str | Path,
    engine: str,
    runtime: dict[str, Any] | None = None,
    owner: ProbeProcessOwner | None = None,
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

    started = time.perf_counter()
    root = account_store_root(sessions_root)
    account_id = str(profile.get("credential_account") or "").strip() or None
    effective_account_id = account_id or engine_account_id(engine)
    acct = CredentialAccountStore(root).inspect(effective_account_id)
    if acct is None or not acct.present:
        return _process_result(
            ok=False, detail=f"容器模型测试需要已登记账号: {effective_account_id}",
            engine=engine, model=model, backend="container",
            command="准备 Worker 容器", started=started, layer="auth",
        )

    try:
        img = _docker(
            "image", "inspect", WORKER_IMAGE, timeout=20,
            **({"owner": owner} if owner is not None else {}),
        )
    except FileNotFoundError:
        return _process_result(
            ok=False, detail="docker 不可用", engine=engine, model=model,
            backend="container", command="docker image inspect", started=started,
            layer="image",
        )
    except subprocess.TimeoutExpired:
        return _process_result(
            ok=False, detail="docker image inspect 超时", engine=engine, model=model,
            backend="container", command="docker image inspect", started=started,
            layer="image",
        )
    if img.returncode != 0:
        return _process_result(
            ok=False, detail=f"worker 镜像缺失或不可用: {WORKER_IMAGE}",
            engine=engine, model=model, backend="container",
            command=f"docker image inspect {WORKER_IMAGE}", started=started,
            returncode=img.returncode, stdout=img.stdout, stderr=img.stderr,
            layer="image",
        )

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
            return _process_result(
                ok=False, detail=f"凭据投影失败: {str(exc)[:120]}",
                engine=engine, model=model, backend="container",
                command="投影模型服务连接", started=started, layer="mount",
            )

        agent_state_dir = os.path.join(workspace, f".{engine}-agent-state")
        agent_state_container = f"{CONTAINER_WORKSPACE}/.{engine}-agent-state"
        resolved = runtime_env_for_engine(
            engine,
            account_root=root,
            account_id=effective_account_id,
            container=True,
            agent_state_dir=agent_state_dir,
            agent_state_container_path=agent_state_container,
            model=model,
        )
        if engine in {"pi", "omp", "opencode", "dsh"}:
            from muteki.solver.container_exec import _chown_tree_to_worker
            _chown_tree_to_worker(agent_state_dir)

        drv = driver_for(profile)
        env_extra = getattr(drv, "env_extra", None)
        profile_env = env_extra() if callable(env_extra) else {}
        env = {
            **_CONTAINER_BASE_ENV,
            **profile_env,
            **resolved.env,
            "MUTEKI_WORKER_MODEL": model,
            "MUTEKI_WORKER_REASONING_EFFORT": str(
                profile.get("reasoning_effort") or "default"),
        }
        argv = _probe_argv_for_profile(
            profile, engine, model, runtime_env=env, container=True)
        if not argv:
            return _process_result(
                ok=False, detail="该引擎没有可用的容器内模型探针",
                engine=engine, model=model, backend="container",
                command=f"{engine} <minimal-model-turn>", started=started,
            )
        verify_claude_model = engine == "claude" and profile_uses_endpoint(profile)
        if verify_claude_model:
            env["CLAUDE_CONFIG_DIR"] = f"{CONTAINER_WORKSPACE}/.muteki-claude-config"
        prelude = [
            'if [ -n "$CLAUDE_CONFIG_DIR" ]; then mkdir -p "$CLAUDE_CONFIG_DIR"; fi',
            'if [ -n "$MUTEKI_CODEX_HOME_SEED" ] && [ -d "$MUTEKI_CODEX_HOME_SEED" ]; then '
            'export CODEX_HOME="${CODEX_HOME:-$HOME/.codex-muteki-model-test}"; '
            'rm -rf "$CODEX_HOME"; mkdir -p "$CODEX_HOME"; '
            'cp -R "$MUTEKI_CODEX_HOME_SEED"/. "$CODEX_HOME"/; '
            'chmod -R u+rwX "$CODEX_HOME"; fi',
            'if [ -r "$CLAUDE_CODE_OAUTH_TOKEN_FILE" ]; then '
            'export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE")"; fi',
            'if [ -r "$ANTHROPIC_AUTH_TOKEN_FILE" ]; then '
            'export ANTHROPIC_AUTH_TOKEN="$(cat "$ANTHROPIC_AUTH_TOKEN_FILE")"; fi',
            'if [ -r "$CURSOR_API_KEY_FILE" ]; then '
            'export CURSOR_API_KEY="$(cat "$CURSOR_API_KEY_FILE")"; fi',
            'if [ -r "$ANTHROPIC_API_KEY_FILE" ]; then '
            'export ANTHROPIC_API_KEY="$(cat "$ANTHROPIC_API_KEY_FILE")"; fi',
            'if [ -r "$OPENAI_API_KEY_FILE" ]; then '
            'export OPENAI_API_KEY="$(cat "$OPENAI_API_KEY_FILE")"; fi',
            'if [ -r "$OPENCODE_API_KEY_FILE" ]; then '
            'export OPENCODE_API_KEY="$(cat "$OPENCODE_API_KEY_FILE")"; fi',
            'if [ -r "$DEEPSEEK_API_KEY_FILE" ]; then '
            'export DEEPSEEK_API_KEY="$(cat "$DEEPSEEK_API_KEY_FILE")"; fi',
            'if [ -r "$KIMI_MODEL_API_KEY_FILE" ]; then '
            'export KIMI_MODEL_API_KEY="$(cat "$KIMI_MODEL_API_KEY_FILE")"; fi',
            'if [ -r "$XAI_API_KEY_FILE" ]; then '
            'export XAI_API_KEY="$(cat "$XAI_API_KEY_FILE")"; fi',
        ]
        timeout_s = max(1, int(getattr(driver_for(profile), "_HELLO_TIMEOUT", 90)))
        script = (
            "; ".join(prelude)
            + f"; exec timeout -s KILL {timeout_s}s {shlex.join(argv)} < /dev/null"
        )

        runtime = runtime or {}
        network = str(
            runtime.get("network")
            or os.environ.get("MUTEKI_WORKER_NETWORK")
            or "bridge"
        ).strip() or "bridge"
        container_name = (
            f"muteki-preflight-{os.getpid()}-{uuid.uuid4().hex[:12]}"
            if owner is not None else ""
        )
        run_cmd = [
            "run", "--rm", "--init",
            *(["--name", container_name] if container_name else []),
            "--network", network,
            "--user", "kali",
            "--workdir", CONTAINER_WORKSPACE,
            "--entrypoint", "bash",
            "--mount", f"type=bind,source={_mount_source(workspace)},target={CONTAINER_WORKSPACE}",
            "--mount", f"type=bind,source={_mount_source(projection)},target={CONTAINER_ACCOUNTS_ROOT}",
        ]
        if network != "host":
            run_cmd += ["--add-host", "host.docker.internal:host-gateway"]
        memory = str(runtime.get("memory") or "").strip()
        cpus = str(runtime.get("cpus") or "").strip()
        pids_limit = int(runtime.get("pids_limit") or 0)
        if memory:
            run_cmd += ["--memory", memory]
        if cpus:
            run_cmd += ["--cpus", cpus]
        if pids_limit > 0:
            run_cmd += ["--pids-limit", str(pids_limit)]
        for k, v in env.items():
            run_cmd += ["-e", f"{k}={v}"]
        run_cmd += [WORKER_IMAGE, "-lc", script]

        try:
            run = _docker(
                *run_cmd, timeout=timeout_s + 30,
                **({"owner": owner, "container_name": container_name}
                   if owner is not None else {}),
            )
        except FileNotFoundError:
            return _process_result(
                ok=False, detail="docker 不可用", engine=engine, model=model,
                backend="container", command="docker run … " + shlex.join(argv),
                started=started, layer="image",
            )
        except subprocess.TimeoutExpired:
            return _process_result(
                ok=False, detail=f"worker 容器模型测试超时（>{timeout_s}s）",
                engine=engine, model=model, backend="container",
                command="docker run … " + shlex.join(argv), started=started,
                layer="auth",
            )

    reply_ok = _probe_ok(profile, run)
    actual_models = _claude_actual_models(run.stdout) if verify_claude_model else None
    model_ok = (
        (not actual_models or _model_matches(model, actual_models))
        if verify_claude_model else True
    )
    ok = reply_ok and model_ok
    if reply_ok and not model_ok:
        actual_text = "、".join(actual_models or []) or "未返回模型 ID"
        detail = f"模型不匹配：配置为 {model}，实际调用 {actual_text}"
    else:
        detail = (
            "worker 容器内模型可用，实际模型与配置一致"
            if ok and verify_claude_model
            else "worker 容器内模型可用（已完成真实对话）"
            if ok
            else "worker 容器模型测试失败: " + _detail(
                run.returncode, run.stdout, run.stderr
            )
        )
    detail = _redact_probe_secrets(detail, env)
    return _process_result(
        ok=ok,
        detail=detail,
        engine=engine, model=model, backend="container",
        command="docker run … " + shlex.join(argv), started=started,
        returncode=run.returncode, stdout=run.stdout, stderr=run.stderr,
        layer=None if ok else ("model" if reply_ok and not model_ok else "auth"),
        actual_models=actual_models,
    )


def probe_worker_model(
    *,
    profile: dict[str, Any],
    model: str,
    reasoning_effort: str = "default",
    sessions_root: str | Path,
    backend: str = "local",
    runtime: dict[str, Any] | None = None,
    owner: ProbeProcessOwner | None = None,
) -> dict[str, Any]:
    """Run one minimal turn with the selected model for this worker profile."""

    started = time.perf_counter()
    profile = dict(profile or {})
    model = str(model or profile.get("model") or "").strip()
    if model:
        profile["model"] = model
    profile["reasoning_effort"] = str(reasoning_effort or "default").strip().lower()
    engine = base_engine_for_profile(profile)
    if profile_uses_endpoint(profile) and not model:
        return _process_result(
            ok=False,
            detail="自定义 API 需要明确的模型 ID，已停止默认模型回退",
            engine=engine,
            model="",
            backend=backend if backend in ("local", "container") else "local",
            command=f"{engine} <model-required>",
            started=started,
            layer="model",
        )

    # In compose deploys the web container does not ship engine CLIs; run the
    # selected profile/model in the worker image instead of shelling the host/web
    # filesystem. This spends one minimal model turn by design: the operator
    # explicitly clicked "test model".
    if backend == "container":
        return _worker_container_model_probe(
            profile=profile,
            model=model,
            sessions_root=sessions_root,
            engine=engine,
            runtime=runtime,
            owner=owner,
        )

    account_id = str(profile.get("credential_account") or "").strip()
    # In local mode an empty credential_account means "use the host CLI login"
    # (e.g. ~/.codex), matching the live swarm worker path. Passing None here
    # would silently fall back to the default <engine>-main account and can pick
    # up a stale registered Codex home.
    resolved_account_id = account_id if account_id else ("" if backend == "local" else None)
    root = account_store_root(sessions_root)
    drv = driver_for(profile)
    env_extra = getattr(drv, "env_extra", None)
    profile_env = env_extra() if callable(env_extra) else {}

    with ExitStack() as stack:
        agent_state_dir = None
        if engine in {"pi", "omp", "opencode", "dsh"}:
            agent_state_dir = stack.enter_context(
                tempfile.TemporaryDirectory(
                    prefix=f"muteki-{engine}-model-test-"))
        credential_env = runtime_env_for_engine(
            engine,
            account_root=root,
            account_id=resolved_account_id,
            container=False,
            agent_state_dir=agent_state_dir,
            model=model,
        ).env
        env = {
            **profile_env,
            **credential_env,
            "MUTEKI_WORKER_MODEL": model,
            "MUTEKI_WORKER_REASONING_EFFORT": str(reasoning_effort or "default"),
        }
        verify_claude_model = engine == "claude" and profile_uses_endpoint(profile)
        if verify_claude_model:
            env["CLAUDE_CONFIG_DIR"] = stack.enter_context(
                tempfile.TemporaryDirectory(prefix="muteki-claude-model-test-")
            )
        # A model test must exercise the same CLI envelope as a real worker.  The old
        # custom-endpoint branch called EndpointDriver.health_detail(), which only
        # issued a curl for Claude endpoints: the endpoint/key could be green while
        # Claude Code itself rejected the selected model or failed to launch.  Build
        # the real profile argv for every local probe (including endpoint profiles),
        # matching the already-correct container model-test path.
        argv = _probe_argv_for_profile(
            profile, engine, model, runtime_env=env)
        if not argv:
            return _process_result(
                ok=False, detail="该引擎没有可用的最小模型探针",
                engine=engine, model=model,
                backend=backend if backend in ("local", "container") else "local",
                command=f"{engine} <minimal-model-turn>", started=started,
            )
        command = shlex.join(argv)
        try:
            process_env = {**os.environ, **env}
            if owner is not None:
                r = _run_owned_process(
                    argv,
                    timeout=getattr(drv, "_HELLO_TIMEOUT", 90),
                    owner=owner,
                    env=process_env,
                )
            else:
                r = subprocess.run(
                    argv,
                    capture_output=True,
                    text=True,
                    encoding="utf-8", errors="replace",
                    timeout=getattr(drv, "_HELLO_TIMEOUT", 90),
                    env=process_env,
                )
        except FileNotFoundError:
            return _process_result(
                ok=False, detail="CLI 不存在", engine=engine, model=model,
                backend=backend if backend in ("local", "container") else "local",
                command=command, started=started, layer="cli",
            )
        except subprocess.TimeoutExpired as exc:
            return _process_result(
                ok=False, detail="模型测试超时", engine=engine, model=model,
                backend=backend if backend in ("local", "container") else "local",
                command=command, started=started,
                stdout=getattr(exc, "stdout", ""), stderr=getattr(exc, "stderr", ""),
                layer="auth",
            )
        except ProbeCancelled:
            raise
        except Exception as exc:  # noqa: BLE001
            return _process_result(
                ok=False, detail=_redact_probe_secrets(str(exc)[:600], env),
                engine=engine, model=model,
                backend=backend if backend in ("local", "container") else "local",
                command=command, started=started,
            )

        reply_ok = drv._hello_ok(r)  # noqa: SLF001 - same model round-trip predicate.
        actual_models = _claude_actual_models(r.stdout) if verify_claude_model else None
        model_ok = (
            (not actual_models or _model_matches(model, actual_models))
            if verify_claude_model else True
        )
        ok = reply_ok and model_ok
        if reply_ok and not model_ok:
            actual_text = "、".join(actual_models or []) or "未返回模型 ID"
            detail = f"模型不匹配：配置为 {model}，实际调用 {actual_text}"
        else:
            detail = (
                "模型可用，实际模型与配置一致"
                if ok and verify_claude_model
                else "模型可用，已完成真实对话"
                if ok
                else _redact_probe_secrets(
                    _detail(r.returncode, r.stdout, r.stderr), env)
            )
        return _process_result(
            ok=bool(ok),
            detail=detail,
            engine=engine, model=model,
            backend=backend if backend in ("local", "container") else "local",
            command=command, started=started, returncode=r.returncode,
            stdout=r.stdout, stderr=r.stderr,
            layer=None if ok else ("model" if reply_ok and not model_ok else "auth"),
            actual_models=actual_models,
        )


def _worker_container_model_batch_probe(
    *,
    items: list[dict[str, Any]],
    sessions_root: str | Path,
    runtime: dict[str, Any] | None = None,
    owner: ProbeProcessOwner | None = None,
) -> dict[str, Any]:
    """Run all requested model turns in one disposable worker container."""

    from muteki.solver.container_exec import (
        CONTAINER_WORKSPACE,
        WORKER_IMAGE,
        _HOST_DATA_ROOT,
        _mount_source,
    )

    root = account_store_root(sessions_root)
    store = CredentialAccountStore(root)
    results: list[dict[str, Any] | None] = [None] * len(items)
    runnable: list[dict[str, Any]] = []

    for index, item in enumerate(items):
        profile = dict(item.get("profile") or {})
        model = str(item.get("model") or profile.get("model") or "").strip()
        if model:
            profile["model"] = model
        profile["reasoning_effort"] = str(
            item.get("reasoning_effort")
            or profile.get("reasoning_effort")
            or "default"
        ).strip().lower()
        engine = base_engine_for_profile(profile)
        profile_id = str(
            item.get("profile_id")
            or profile.get("id")
            or profile.get("name")
            or f"profile-{index}"
        )
        started = time.perf_counter()
        if profile_uses_endpoint(profile) and not model:
            result = _process_result(
                ok=False,
                detail="自定义 API 需要明确的模型 ID，已停止默认模型回退",
                engine=engine,
                model="",
                backend="container",
                command=f"{engine} <model-required>",
                started=started,
                layer="model",
            )
            result["profile_id"] = profile_id
            results[index] = result
            continue

        account_id = str(profile.get("credential_account") or "").strip() or None
        effective_account_id = account_id or engine_account_id(engine)
        account = store.inspect(effective_account_id)
        if account is None or not account.present:
            result = _process_result(
                ok=False,
                detail=f"容器模型测试需要已登记账号: {effective_account_id}",
                engine=engine,
                model=model,
                backend="container",
                command="准备 Worker 批量检查容器",
                started=started,
                layer="auth",
            )
            result["profile_id"] = profile_id
            results[index] = result
            continue
        runnable.append({
            "index": index,
            "profile_id": profile_id,
            "profile": profile,
            "model": model,
            "engine": engine,
            "account_id": effective_account_id,
        })

    if not runnable:
        return {
            "backend": "container",
            "container_count": 0,
            "results": [result for result in results if result is not None],
        }

    image_started = time.perf_counter()
    try:
        image = _docker(
            "image", "inspect", WORKER_IMAGE, timeout=20,
            **({"owner": owner} if owner is not None else {}),
        )
    except FileNotFoundError:
        image = None
        image_detail = "docker 不可用"
    except subprocess.TimeoutExpired:
        image = None
        image_detail = "docker image inspect 超时"
    else:
        image_detail = (
            "" if image.returncode == 0
            else f"worker 镜像缺失或不可用: {WORKER_IMAGE}"
        )
    if image is None or image.returncode != 0:
        for task in runnable:
            result = _process_result(
                ok=False,
                detail=image_detail,
                engine=task["engine"],
                model=task["model"],
                backend="container",
                command=f"docker image inspect {WORKER_IMAGE}",
                started=image_started,
                returncode=image.returncode if image is not None else None,
                stdout=image.stdout if image is not None else "",
                stderr=image.stderr if image is not None else "",
                layer="image",
            )
            result["profile_id"] = task["profile_id"]
            results[task["index"]] = result
        return {
            "backend": "container",
            "container_count": 0,
            "results": [result for result in results if result is not None],
        }

    tmp_base = None
    if _HOST_DATA_ROOT:
        tmp_base = os.path.join(
            os.environ.get("MUTEKI_CONTAINER_DATA_ROOT") or _HOST_DATA_ROOT,
            "_tmp",
            "model-batch-tests",
        )
        try:
            os.makedirs(tmp_base, exist_ok=True)
        except OSError:
            tmp_base = None

    runtime = runtime or {}
    network = str(
        runtime.get("network")
        or os.environ.get("MUTEKI_WORKER_NETWORK")
        or "bridge"
    ).strip() or "bridge"
    container_name = f"muteki-model-batch-{os.getpid()}-{uuid.uuid4().hex[:12]}"
    container_started = False

    with tempfile.TemporaryDirectory(
        prefix="muteki-model-batch-test-", dir=tmp_base
    ) as td:
        workspace = os.path.join(td, "ws")
        projection = os.path.join(td, "accounts")
        os.makedirs(workspace, exist_ok=True)
        try:
            os.chmod(workspace, 0o777)
            project_account_root(root, projection)
        except OSError as exc:
            for task in runnable:
                result = _process_result(
                    ok=False,
                    detail=f"凭据投影失败: {str(exc)[:120]}",
                    engine=task["engine"],
                    model=task["model"],
                    backend="container",
                    command="投影模型服务连接",
                    started=time.perf_counter(),
                    layer="mount",
                )
                result["profile_id"] = task["profile_id"]
                results[task["index"]] = result
            return {
                "backend": "container",
                "container_count": 0,
                "results": [result for result in results if result is not None],
            }

        prepared: list[dict[str, Any]] = []
        for task in runnable:
            engine = task["engine"]
            profile = task["profile"]
            model = task["model"]
            task_name = f"{task['index']:03d}-{re.sub(r'[^a-zA-Z0-9_.-]+', '-', task['profile_id'])[:80]}"
            task_host = os.path.join(workspace, "batch", task_name)
            task_container = f"{CONTAINER_WORKSPACE}/batch/{task_name}"
            home_host = os.path.join(task_host, "home")
            os.makedirs(home_host, exist_ok=True)
            for directory in (os.path.dirname(task_host), task_host, home_host):
                try:
                    os.chmod(directory, 0o777)
                except OSError:
                    pass

            state_host = None
            state_container = None
            if engine in {"pi", "omp", "opencode", "dsh"}:
                state_host = os.path.join(task_host, f".{engine}-agent-state")
                state_container = f"{task_container}/.{engine}-agent-state"
            resolved = runtime_env_for_engine(
                engine,
                account_root=root,
                account_id=task["account_id"],
                container=True,
                agent_state_dir=state_host,
                agent_state_container_path=state_container,
                model=model,
            )

            drv = driver_for(profile)
            env_extra = getattr(drv, "env_extra", None)
            profile_env = env_extra() if callable(env_extra) else {}
            env = {
                **_CONTAINER_BASE_ENV,
                **profile_env,
                **resolved.env,
                "HOME": f"{task_container}/home",
                "MUTEKI_WORKER_MODEL": model,
                "MUTEKI_WORKER_REASONING_EFFORT": str(
                    profile.get("reasoning_effort") or "default"
                ),
            }
            state_seeds = (
                ("CODEX_HOME", "MUTEKI_CODEX_HOME_SEED", ".codex-muteki-model-test"),
                ("KIMI_CODE_HOME", "MUTEKI_KIMI_CODE_HOME_SEED", ".kimi-muteki-model-test"),
                ("GROK_HOME", "MUTEKI_GROK_HOME_SEED", ".grok-muteki-model-test"),
            )
            for state_var, seed_var, dirname in state_seeds:
                seed = str(env.get(state_var) or "").strip()
                if not seed:
                    continue
                env[seed_var] = seed
                env[state_var] = f"{task_container}/{dirname}"
            argv = _probe_argv_for_profile(
                profile, engine, model, runtime_env=env, container=True
            )
            if not argv:
                result = _process_result(
                    ok=False,
                    detail="该引擎没有可用的容器内模型探针",
                    engine=engine,
                    model=model,
                    backend="container",
                    command=f"{engine} <minimal-model-turn>",
                    started=time.perf_counter(),
                )
                result["profile_id"] = task["profile_id"]
                results[task["index"]] = result
                continue

            verify_claude_model = engine == "claude" and profile_uses_endpoint(profile)
            if verify_claude_model:
                env["CLAUDE_CONFIG_DIR"] = f"{task_container}/.muteki-claude-config"
            prelude = [
                'if [ -n "$CLAUDE_CONFIG_DIR" ]; then mkdir -p "$CLAUDE_CONFIG_DIR"; fi',
                'if [ -n "$MUTEKI_CODEX_HOME_SEED" ] && [ -d "$MUTEKI_CODEX_HOME_SEED" ]; then '
                'export CODEX_HOME="${CODEX_HOME:-$HOME/.codex-muteki-model-test}"; '
                'rm -rf "$CODEX_HOME"; mkdir -p "$CODEX_HOME"; '
                'cp -R "$MUTEKI_CODEX_HOME_SEED"/. "$CODEX_HOME"/; '
                'chmod -R u+rwX "$CODEX_HOME"; fi',
                'if [ -n "$MUTEKI_KIMI_CODE_HOME_SEED" ] && [ -d "$MUTEKI_KIMI_CODE_HOME_SEED" ]; then '
                'rm -rf "$KIMI_CODE_HOME"; mkdir -p "$KIMI_CODE_HOME"; '
                'cp -R "$MUTEKI_KIMI_CODE_HOME_SEED"/. "$KIMI_CODE_HOME"/; '
                'chmod -R u+rwX "$KIMI_CODE_HOME"; fi',
                'if [ -n "$MUTEKI_GROK_HOME_SEED" ] && [ -d "$MUTEKI_GROK_HOME_SEED" ]; then '
                'rm -rf "$GROK_HOME"; mkdir -p "$GROK_HOME"; '
                'cp -R "$MUTEKI_GROK_HOME_SEED"/. "$GROK_HOME"/; '
                'chmod -R u+rwX "$GROK_HOME"; fi',
                'if [ -r "$CLAUDE_CODE_OAUTH_TOKEN_FILE" ]; then '
                'export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE")"; fi',
                'if [ -r "$ANTHROPIC_AUTH_TOKEN_FILE" ]; then '
                'export ANTHROPIC_AUTH_TOKEN="$(cat "$ANTHROPIC_AUTH_TOKEN_FILE")"; fi',
                'if [ -r "$CURSOR_API_KEY_FILE" ]; then '
                'export CURSOR_API_KEY="$(cat "$CURSOR_API_KEY_FILE")"; fi',
                'if [ -r "$ANTHROPIC_API_KEY_FILE" ]; then '
                'export ANTHROPIC_API_KEY="$(cat "$ANTHROPIC_API_KEY_FILE")"; fi',
                'if [ -r "$OPENAI_API_KEY_FILE" ]; then '
                'export OPENAI_API_KEY="$(cat "$OPENAI_API_KEY_FILE")"; fi',
                'if [ -r "$OPENCODE_API_KEY_FILE" ]; then '
                'export OPENCODE_API_KEY="$(cat "$OPENCODE_API_KEY_FILE")"; fi',
                'if [ -r "$DEEPSEEK_API_KEY_FILE" ]; then '
                'export DEEPSEEK_API_KEY="$(cat "$DEEPSEEK_API_KEY_FILE")"; fi',
                'if [ -r "$KIMI_MODEL_API_KEY_FILE" ]; then '
                'export KIMI_MODEL_API_KEY="$(cat "$KIMI_MODEL_API_KEY_FILE")"; fi',
                'if [ -r "$XAI_API_KEY_FILE" ]; then '
                'export XAI_API_KEY="$(cat "$XAI_API_KEY_FILE")"; fi',
            ]
            timeout_s = max(1, int(getattr(drv, "_HELLO_TIMEOUT", 90)))
            prepared.append({
                **task,
                "argv": argv,
                "env": {str(key): str(value) for key, value in env.items()},
                "script": "; ".join(prelude)
                + f"; exec timeout -s KILL {timeout_s}s {shlex.join(argv)} < /dev/null",
                "task_container": task_container,
                "timeout": timeout_s,
                "verify_claude_model": verify_claude_model,
            })

        if not prepared:
            return {
                "backend": "container",
                "container_count": 0,
                "results": [result for result in results if result is not None],
            }

        run_cmd = [
            "run", "-d", "--rm", "--init",
            "--name", container_name,
            "--network", network,
            "--user", "kali",
            "--workdir", CONTAINER_WORKSPACE,
            "--entrypoint", "sleep",
            "--mount",
            f"type=bind,source={_mount_source(workspace)},target={CONTAINER_WORKSPACE}",
            "--mount",
            f"type=bind,source={_mount_source(projection)},target={CONTAINER_ACCOUNTS_ROOT}",
        ]
        if network != "host":
            run_cmd += ["--add-host", "host.docker.internal:host-gateway"]
        memory = str(runtime.get("memory") or "").strip()
        cpus = str(runtime.get("cpus") or "").strip()
        pids_limit = int(runtime.get("pids_limit") or 0)
        if memory:
            run_cmd += ["--memory", memory]
        if cpus:
            run_cmd += ["--cpus", cpus]
        if pids_limit > 0:
            run_cmd += ["--pids-limit", str(pids_limit)]
        run_cmd += [WORKER_IMAGE, "infinity"]

        try:
            started = time.perf_counter()
            container = _docker(
                *run_cmd,
                timeout=30,
                **({"owner": owner, "container_name": container_name}
                   if owner is not None else {}),
            )
            if container.returncode != 0:
                for task in prepared:
                    result = _process_result(
                        ok=False,
                        detail="Worker 批量检查容器启动失败: "
                        + _detail(container.returncode, container.stdout, container.stderr),
                        engine=task["engine"],
                        model=task["model"],
                        backend="container",
                        command="docker run --rm <worker-model-batch>",
                        started=started,
                        returncode=container.returncode,
                        stdout=container.stdout,
                        stderr=container.stderr,
                        layer="cli",
                    )
                    result["profile_id"] = task["profile_id"]
                    results[task["index"]] = result
            else:
                container_started = True

                ownership = _docker(
                    "exec",
                    "--user", "root",
                    container_name,
                    "chown", "-R", "kali:kali", f"{CONTAINER_WORKSPACE}/batch",
                    timeout=20,
                    **({"owner": owner, "container_name": container_name}
                       if owner is not None else {}),
                )
                if ownership.returncode != 0:
                    for task in prepared:
                        result = _process_result(
                            ok=False,
                            detail="批量检查容器无法准备独立 Worker 目录: "
                            + _detail(
                                ownership.returncode,
                                ownership.stdout,
                                ownership.stderr,
                            ),
                            engine=task["engine"],
                            model=task["model"],
                            backend="container",
                            command="docker exec <worker-model-batch> chown",
                            started=started,
                            returncode=ownership.returncode,
                            stdout=ownership.stdout,
                            stderr=ownership.stderr,
                            layer="mount",
                        )
                        result["profile_id"] = task["profile_id"]
                        results[task["index"]] = result

                def run_task(task: dict[str, Any]) -> tuple[dict[str, Any], float, Any]:
                    task_started = time.perf_counter()
                    exec_cmd = [
                        "exec",
                        "--user", "kali",
                        "--workdir", task["task_container"],
                    ]
                    for key, value in task["env"].items():
                        exec_cmd += ["-e", f"{key}={value}"]
                    exec_cmd += [container_name, "bash", "-lc", task["script"]]
                    try:
                        run = _docker(
                            *exec_cmd,
                            timeout=task["timeout"] + 30,
                            **({"owner": owner, "container_name": container_name}
                               if owner is not None else {}),
                        )
                        return task, task_started, run
                    except Exception as exc:  # noqa: BLE001
                        return task, task_started, exc

                max_concurrent = max(1, min(4, len(prepared)))
                if ownership.returncode == 0:
                    with ThreadPoolExecutor(max_workers=max_concurrent) as pool:
                        futures = [pool.submit(run_task, task) for task in prepared]
                        for future in as_completed(futures):
                            task, task_started, outcome = future.result()
                            if isinstance(outcome, subprocess.TimeoutExpired):
                                result = _process_result(
                                    ok=False,
                                    detail=(
                                        "worker 容器模型测试超时"
                                        f"（>{task['timeout']}s）"
                                    ),
                                    engine=task["engine"],
                                    model=task["model"],
                                    backend="container",
                                    command="docker exec <worker-model-batch> … "
                                    + shlex.join(task["argv"]),
                                    started=task_started,
                                    stdout=getattr(outcome, "stdout", ""),
                                    stderr=getattr(outcome, "stderr", ""),
                                    layer="auth",
                                )
                            elif isinstance(outcome, Exception):
                                result = _process_result(
                                    ok=False,
                                    detail=str(outcome)[:600],
                                    engine=task["engine"],
                                    model=task["model"],
                                    backend="container",
                                    command="docker exec <worker-model-batch> … "
                                    + shlex.join(task["argv"]),
                                    started=task_started,
                                    layer="cli",
                                )
                            else:
                                reply_ok = _probe_ok(task["profile"], outcome)
                                actual_models = (
                                    _claude_actual_models(outcome.stdout)
                                    if task["verify_claude_model"] else None
                                )
                                model_ok = (
                                    (not actual_models or _model_matches(
                                        task["model"], actual_models
                                    ))
                                    if task["verify_claude_model"] else True
                                )
                                ok = reply_ok and model_ok
                                if reply_ok and not model_ok:
                                    actual_text = (
                                        "、".join(actual_models or [])
                                        or "未返回模型 ID"
                                    )
                                    detail = (
                                        f"模型不匹配：配置为 {task['model']}，"
                                        f"实际调用 {actual_text}"
                                    )
                                else:
                                    detail = (
                                        "worker 批量检查容器内模型可用，"
                                        "实际模型与配置一致"
                                        if ok and task["verify_claude_model"]
                                        else "worker 批量检查容器内模型可用"
                                        "（已完成真实对话）"
                                        if ok
                                        else "worker 容器模型测试失败: "
                                        + _detail(
                                            outcome.returncode,
                                            outcome.stdout,
                                            outcome.stderr,
                                        )
                                    )
                                detail = _redact_probe_secrets(detail, task["env"])
                                result = _process_result(
                                    ok=ok,
                                    detail=detail,
                                    engine=task["engine"],
                                    model=task["model"],
                                    backend="container",
                                    command="docker exec <worker-model-batch> … "
                                    + shlex.join(task["argv"]),
                                    started=task_started,
                                    returncode=outcome.returncode,
                                    stdout=outcome.stdout,
                                    stderr=outcome.stderr,
                                    layer=None if ok else (
                                        "model"
                                        if reply_ok and not model_ok else "auth"
                                    ),
                                    actual_models=actual_models,
                                )
                            result["profile_id"] = task["profile_id"]
                            results[task["index"]] = result
        except FileNotFoundError:
            for task in prepared:
                result = _process_result(
                    ok=False,
                    detail="docker 不可用",
                    engine=task["engine"],
                    model=task["model"],
                    backend="container",
                    command="docker run --rm <worker-model-batch>",
                    started=time.perf_counter(),
                    layer="image",
                )
                result["profile_id"] = task["profile_id"]
                results[task["index"]] = result
        except subprocess.TimeoutExpired:
            for task in prepared:
                result = _process_result(
                    ok=False,
                    detail="Worker 批量检查容器启动超时",
                    engine=task["engine"],
                    model=task["model"],
                    backend="container",
                    command="docker run --rm <worker-model-batch>",
                    started=time.perf_counter(),
                    layer="cli",
                )
                result["profile_id"] = task["profile_id"]
                results[task["index"]] = result
        finally:
            if container_started:
                try:
                    _docker("rm", "-f", container_name, timeout=15)
                except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
                    pass

    return {
        "backend": "container",
        "container_count": 1 if container_started else 0,
        "max_concurrent": max(1, min(4, len(prepared))),
        "results": [result for result in results if result is not None],
    }


def probe_worker_models_batch(
    *,
    items: list[dict[str, Any]],
    sessions_root: str | Path,
    backend: str = "local",
    runtime: dict[str, Any] | None = None,
    owner: ProbeProcessOwner | None = None,
) -> dict[str, Any]:
    """Run one real model turn for every requested profile."""

    normalized = [item for item in items if isinstance(item, dict)]
    if backend == "container":
        return _worker_container_model_batch_probe(
            items=normalized,
            sessions_root=sessions_root,
            runtime=runtime,
            owner=owner,
        )

    results: list[dict[str, Any] | None] = [None] * len(normalized)

    def run_local(index: int, item: dict[str, Any]) -> tuple[int, dict[str, Any]]:
        profile = dict(item.get("profile") or {})
        profile_id = str(
            item.get("profile_id")
            or profile.get("id")
            or profile.get("name")
            or f"profile-{index}"
        )
        result = probe_worker_model(
            profile=profile,
            model=str(item.get("model") or ""),
            reasoning_effort=str(item.get("reasoning_effort") or "default"),
            sessions_root=sessions_root,
            backend="local",
            runtime=runtime,
            owner=owner,
        )
        result["profile_id"] = profile_id
        return index, result

    if normalized:
        with ThreadPoolExecutor(max_workers=len(normalized)) as pool:
            futures = [
                pool.submit(run_local, index, item)
                for index, item in enumerate(normalized)
            ]
            for future in as_completed(futures):
                index, result = future.result()
                results[index] = result
    return {
        "backend": "local",
        "container_count": 0,
        "results": [result for result in results if result is not None],
    }


def parse_cursor_models(text: str) -> list[ModelOption]:
    """Small parser kept for future refresh tooling and tests."""

    rows: list[tuple[str, str]] = []
    for line in text.splitlines():
        if " - " not in line or line.lower().startswith("available models"):
            continue
        mid, label = line.split(" - ", 1)
        mid = mid.strip()
        label = label.strip()
        if mid:
            rows.append((mid, label or mid))

    variant_re = re.compile(r"^(.*)-(low|medium|high|xhigh|max)(-fast)?$")
    groups: dict[tuple[str, bool], set[str]] = {}
    bare: set[tuple[str, bool]] = set()
    for mid, _ in rows:
        match = variant_re.match(mid)
        if match:
            groups.setdefault((match.group(1), bool(match.group(3))), set()).add(match.group(2))
        else:
            fast = mid.endswith("-fast")
            key = (mid[:-5] if fast else mid, fast)
            bare.add(key)
    for key in bare:
        if key in groups:
            groups[key].add("medium")

    order = ["low", "medium", "high", "xhigh", "max"]
    out: list[ModelOption] = []
    for mid, label in rows:
        match = variant_re.match(mid)
        if match:
            key = (match.group(1), bool(match.group(3)))
            default = match.group(2)
        else:
            fast = mid.endswith("-fast")
            key = (mid[:-5] if fast else mid, fast)
            default = "medium" if key in groups else ""
        levels = [level for level in order if level in groups.get(key, set())]
        out.append({
            "id": mid,
            "label": label,
            "reasoning": {
                "supported": bool(levels),
                "levels": levels,
                "default": default,
            },
        })
    return out


def parse_openai_models(text: str) -> list[ModelOption]:
    data = json.loads(text)
    models = None
    if isinstance(data, dict):
        models = data.get("models") or data.get("data")
    if not isinstance(models, list):
        return []
    out: list[ModelOption] = []
    for item in models:
        if not isinstance(item, dict):
            continue
        mid = str(item.get("slug") or item.get("id") or "").strip()
        if mid:
            raw_levels = item.get("supported_reasoning_levels") or []
            levels = []
            for level in raw_levels:
                effort = level.get("effort") if isinstance(level, dict) else level
                effort = str(effort or "").strip().lower()
                if effort in {
                    "none", "minimal", "low", "medium", "high", "xhigh", "max",
                }:
                    levels.append(effort)
            out.append({
                "id": mid,
                "label": str(item.get("display_name") or mid),
                "reasoning": {
                    "supported": bool(levels),
                    "levels": list(dict.fromkeys(levels)),
                    "default": str(item.get("default_reasoning_level") or "").strip().lower(),
                },
            })
    return out


def parse_kimi_models(text: str) -> list[ModelOption]:
    data = json.loads(text)
    models = data.get("models") if isinstance(data, dict) else None
    if not isinstance(models, dict):
        return []
    return [
        {"id": str(alias), "label": str(alias)}
        for alias in models
        if str(alias).strip()
    ]


def parse_grok_models(text: str) -> list[ModelOption]:
    clean = re.sub(r"\x1b\[[0-9;]*m", "", text)
    out: list[ModelOption] = []
    for line in clean.splitlines():
        match = re.match(r"\s*[-*]\s+([^\s(]+)", line)
        if not match:
            continue
        mid = match.group(1).strip()
        out.append({
            "id": mid,
            "label": mid,
            "reasoning": {
                "supported": True,
                "levels": ["low", "medium", "high", "xhigh"],
                "default": "",
            },
        })
    return out


def parse_pi_models(text: str) -> list[ModelOption]:
    out: list[ModelOption] = []
    for line in text.splitlines()[1:]:
        cols = line.split()
        if len(cols) < 2:
            continue
        thinking = len(cols) >= 5 and cols[4].lower() == "yes"
        out.append({
            "id": cols[1],
            "label": f"{cols[1]} ({cols[0]})",
            "reasoning": {
                "supported": thinking,
                "levels": (["none", "minimal", "low", "medium", "high", "xhigh", "max"]
                           if thinking else []),
                "default": "",
            },
        })
    return out


def parse_omp_models(text: str) -> list[ModelOption]:
    data = json.loads(text)
    models = data.get("models") if isinstance(data, dict) else None
    if not isinstance(models, list):
        return []
    out: list[ModelOption] = []
    for item in models:
        if not isinstance(item, dict):
            continue
        mid = str(item.get("selector") or item.get("id") or "").strip()
        if not mid:
            continue
        thinking = bool(item.get("reasoning") or item.get("thinking"))
        out.append({
            "id": mid,
            "label": str(item.get("name") or mid),
            "reasoning": {
                "supported": thinking,
                "levels": (["none", "minimal", "low", "medium", "high", "xhigh", "max"]
                           if thinking else []),
                "default": "",
            },
        })
    return out


class ModelDiscoveryError(RuntimeError):
    pass


def _discovery_argv(engine: str, binary: str, *, bundled: bool = False) -> list[str]:
    if engine == "codex":
        return [binary, "debug", "models", *(["--bundled"] if bundled else [])]
    if engine == "cursor":
        return [binary, "models"]
    if engine == "pi":
        return [binary, "--list-models"]
    if engine == "omp":
        return [binary, "models", "--json"]
    if engine == "kimi":
        return [binary, "provider", "list", "--json"]
    if engine == "grok":
        return [binary, "models"]
    if engine == "opencode":
        return [binary, "models"]
    raise ModelDiscoveryError(f"{engine} 当前没有非交互模型发现命令")


def _run_local_discovery(
    profile: dict[str, Any], sessions_root: str | Path, *, bundled: bool = False,
) -> subprocess.CompletedProcess:
    engine = base_engine_for_profile(profile)
    account_id = str(profile.get("credential_account") or "").strip()
    resolved_account_id = account_id if account_id else ""
    resolved = runtime_env_for_engine(
        engine,
        account_root=account_store_root(sessions_root),
        account_id=resolved_account_id,
        container=False,
    )
    binary = driver_for(profile).bin
    argv = _discovery_argv(engine, binary, bundled=bundled)
    try:
        return subprocess.run(
            argv,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=45,
            env={
                **os.environ,
                **driver_for(profile).env_extra(),
                **resolved.env,
            },
        )
    except FileNotFoundError as exc:
        raise ModelDiscoveryError("CLI 不存在") from exc
    except subprocess.TimeoutExpired as exc:
        raise ModelDiscoveryError("模型发现超时（>45s）") from exc


def _run_container_discovery(
    profile: dict[str, Any], sessions_root: str | Path, *, bundled: bool = False,
) -> subprocess.CompletedProcess:
    from muteki.solver.container_exec import (
        CONTAINER_WORKSPACE,
        WORKER_IMAGE,
        _HOST_DATA_ROOT,
        _mount_source,
    )

    engine = base_engine_for_profile(profile)
    try:
        image = _docker("image", "inspect", WORKER_IMAGE, timeout=20)
    except FileNotFoundError as exc:
        raise ModelDiscoveryError("docker 不可用") from exc
    except subprocess.TimeoutExpired as exc:
        raise ModelDiscoveryError("worker 镜像检查超时") from exc
    if image.returncode != 0:
        raise ModelDiscoveryError(f"worker 镜像缺失或不可用: {WORKER_IMAGE}")

    root = account_store_root(sessions_root)
    account_id = str(profile.get("credential_account") or "").strip() or None
    resolved = runtime_env_for_engine(
        engine, account_root=root, account_id=account_id, container=True
    )
    binary = _CONTAINER_BIN.get(engine) or engine
    argv = _discovery_argv(engine, binary, bundled=bundled)
    tmp_base = None
    if _HOST_DATA_ROOT:
        tmp_base = os.path.join(
            os.environ.get("MUTEKI_CONTAINER_DATA_ROOT") or _HOST_DATA_ROOT,
            "_tmp",
            "model-discovery",
        )
        try:
            os.makedirs(tmp_base, exist_ok=True)
        except OSError:
            tmp_base = None

    with tempfile.TemporaryDirectory(prefix="muteki-model-discovery-", dir=tmp_base) as td:
        workspace = os.path.join(td, "ws")
        projection = os.path.join(td, "accounts")
        os.makedirs(workspace, exist_ok=True)
        try:
            os.chmod(workspace, 0o777)
            project_account_root(root, projection)
        except OSError as exc:
            raise ModelDiscoveryError(f"凭据投影失败: {str(exc)[:120]}") from exc

        prelude = [
            'if [ -r "$CLAUDE_CODE_OAUTH_TOKEN_FILE" ]; then '
            'export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE")"; fi',
            'if [ -r "$ANTHROPIC_AUTH_TOKEN_FILE" ]; then '
            'export ANTHROPIC_AUTH_TOKEN="$(cat "$ANTHROPIC_AUTH_TOKEN_FILE")"; fi',
            'if [ -r "$CURSOR_API_KEY_FILE" ]; then '
            'export CURSOR_API_KEY="$(cat "$CURSOR_API_KEY_FILE")"; fi',
            'if [ -r "$ANTHROPIC_API_KEY_FILE" ]; then '
            'export ANTHROPIC_API_KEY="$(cat "$ANTHROPIC_API_KEY_FILE")"; fi',
            'if [ -r "$OPENAI_API_KEY_FILE" ]; then '
            'export OPENAI_API_KEY="$(cat "$OPENAI_API_KEY_FILE")"; fi',
            'if [ -r "$OPENCODE_API_KEY_FILE" ]; then '
            'export OPENCODE_API_KEY="$(cat "$OPENCODE_API_KEY_FILE")"; fi',
            'if [ -r "$DEEPSEEK_API_KEY_FILE" ]; then '
            'export DEEPSEEK_API_KEY="$(cat "$DEEPSEEK_API_KEY_FILE")"; fi',
            'if [ -r "$KIMI_MODEL_API_KEY_FILE" ]; then '
            'export KIMI_MODEL_API_KEY="$(cat "$KIMI_MODEL_API_KEY_FILE")"; fi',
            'if [ -r "$XAI_API_KEY_FILE" ]; then '
            'export XAI_API_KEY="$(cat "$XAI_API_KEY_FILE")"; fi',
        ]
        script = "; ".join(prelude) + f"; exec timeout -s KILL 45s {shlex.join(argv)} < /dev/null"
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
        for key, value in {**_CONTAINER_BASE_ENV, **resolved.env}.items():
            run_cmd += ["-e", f"{key}={value}"]
        run_cmd += [WORKER_IMAGE, "-lc", script]
        try:
            return _docker(*run_cmd, timeout=75)
        except FileNotFoundError as exc:
            raise ModelDiscoveryError("docker 不可用") from exc
        except subprocess.TimeoutExpired as exc:
            raise ModelDiscoveryError("worker 容器模型发现超时（>45s）") from exc


def _parse_discovery(engine: str, output: str) -> list[ModelOption]:
    try:
        if engine == "codex":
            return _dedupe_models(parse_openai_models(output))
        if engine == "cursor":
            return _dedupe_models(parse_cursor_models(output))
        if engine == "pi":
            return _dedupe_models(parse_pi_models(output))
        if engine == "omp":
            return _dedupe_models(parse_omp_models(output))
        if engine == "kimi":
            return _dedupe_models(parse_kimi_models(output))
        if engine == "grok":
            return _dedupe_models(parse_grok_models(output))
        if engine == "opencode":
            return _dedupe_models([
                {"id": line.strip(), "label": line.strip()}
                for line in output.splitlines()
                if line.strip() and "/" in line.strip()
            ])
    except (json.JSONDecodeError, TypeError, ValueError):
        return []
    return []


def discover_worker_models(
    *,
    profile: dict[str, Any],
    sessions_root: str | Path,
    backend: str,
) -> dict[str, Any]:
    """Discover one profile's models only when the operator requests it."""

    profile = dict(profile or {})
    engine = base_engine_for_profile(profile)
    profile_id = str(profile.get("id") or profile.get("name") or engine).strip()
    base = {
        "profile_id": profile_id,
        "engine": engine,
        "updated_at": time.time(),
        "models": [],
    }
    if engine == "claude":
        return {
            **base,
            "ok": False,
            "source": "manual_public_catalog",
            "detail": (
                "Claude Code 没有可供脚本调用的订阅模型列表命令；未配置账号时使用手工维护的公开模型和官方别名"
            ),
        }
    if engine not in {"codex", "cursor", "pi", "omp", "kimi", "grok", "opencode"}:
        return {
            **base,
            "ok": False,
            "source": "unsupported",
            "detail": f"{engine} 当前未接入模型自动发现",
        }

    runner = _run_container_discovery if backend == "container" else _run_local_discovery
    try:
        result = runner(profile, sessions_root, bundled=False)
    except ModelDiscoveryError as exc:
        return {**base, "ok": False, "source": "cli", "detail": str(exc)}

    models = _parse_discovery(engine, result.stdout or "")
    source = f"{engine}_cli"
    detail = ""
    if engine == "codex" and (result.returncode != 0 or not models):
        remote_detail = _detail(result.returncode, result.stdout, result.stderr)
        try:
            bundled_result = runner(profile, sessions_root, bundled=True)
        except ModelDiscoveryError as exc:
            return {
                **base,
                "ok": False,
                "source": "codex_cli",
                "detail": f"远程目录失败；内置目录失败: {exc}",
            }
        models = _parse_discovery(engine, bundled_result.stdout or "")
        result = bundled_result
        source = "codex_cli_bundled"
        detail = f"远程目录不可用，已读取 CLI 内置目录；{remote_detail}"

    if result.returncode != 0 or not models:
        return {
            **base,
            "ok": False,
            "source": source,
            "detail": _detail(result.returncode, result.stdout, result.stderr),
        }
    return {
        **base,
        "ok": True,
        "source": source,
        "models": models,
        "detail": detail or f"发现 {len(models)} 个模型",
    }
