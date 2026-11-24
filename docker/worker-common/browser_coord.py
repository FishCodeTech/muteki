#!/usr/bin/env python3
"""Run-scoped agent-browser CLI shim for cooperating pentest Workers.

The real CLI owns one browser daemon per Run. This shim serializes tab selection
and the following command, while keeping each Worker's active tab in its own
workspace. A shared tab can be leased explicitly for sessionStorage workflows.
"""
from __future__ import annotations

import fcntl
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import time
import uuid


REAL_BROWSER = "/usr/local/bin/agent-browser"
SHARED_LABEL = "muteki-shared"
LEASE_SECONDS = 600
_CONTAINER_PATH = "/home/kali/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
_RUN_GLOBAL_COMMANDS = frozenset({
    "close", "quit", "exit", "connect", "session", "mcp", "dashboard",
    "plugin", "auth", "profiles", "upgrade",
})


def _run(*args: str, capture: bool = False, timeout: float = 60) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    run_root = env.get("MUTEKI_BROWSER_RUN_ROOT")
    if run_root:
        home = Path(run_root) / ".muteki-browser" / "home"
        home.mkdir(parents=True, exist_ok=True)
        env.update({
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "XDG_DATA_HOME": str(home / ".local" / "share"),
        })
    try:
        return subprocess.run([REAL_BROWSER, *args], capture_output=capture, text=True,
                              check=False, env=env, timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"agent-browser 命令超过 {timeout:g} 秒：{args[0] if args else 'help'}") from exc


def _tabs() -> list[dict]:
    response = _run("tab", "list", "--json", capture=True)
    if response.returncode:
        raise RuntimeError(response.stderr.strip() or response.stdout.strip() or
                           "agent-browser tab list failed")
    value = json.loads(response.stdout)
    if not value.get("success"):
        raise RuntimeError(str(value.get("error") or "agent-browser tab list failed"))
    return list(value.get("data", {}).get("tabs", []))


def _state(path: Path, base: str) -> dict:
    try:
        value = json.loads(path.read_text())
        active = str(value["active"])
        owned = list(value["owned"])
        if active == SHARED_LABEL or active in owned:
            return {"active": active, "owned": owned}
    except (OSError, ValueError, KeyError, TypeError):
        pass
    return {"active": base, "owned": [base]}


def _save(path: Path, value: dict) -> None:
    temporary = path.with_name(path.name + "." + uuid.uuid4().hex)
    try:
        temporary.write_text(json.dumps(value))
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def _resolve_owned(ref: str, tabs: list[dict], owned: list[str], base: str) -> str:
    candidates = {label: label for label in owned}
    candidates.update({label.removeprefix(base + "-"): label for label in owned
                       if label.startswith(base + "-")})
    for tab in tabs:
        if tab.get("label") in owned:
            candidates[str(tab.get("tabId"))] = str(tab["label"])
            candidates[str(tab.get("targetId"))] = str(tab["label"])
    if ref not in candidates:
        raise RuntimeError("只能切换或关闭当前 Worker 拥有的标签页；共享页请先申请租约")
    return candidates[ref]


def _ensure_tab(label: str, tabs: list[dict]) -> None:
    if any(tab.get("label") == label for tab in tabs):
        response = _run("tab", label, capture=True)
    else:
        response = _run("tab", "new", "--label", label, capture=True)
    if response.returncode:
        raise RuntimeError(response.stderr.strip() or response.stdout.strip() or
                           "agent-browser tab selection failed")


def _lease(path: Path) -> dict:
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return {}


def _record_screenshot(path_text: str, active_tab: str, worker_id: str,
                       run_id: str) -> None:
    screenshot = Path(path_text).resolve()
    if not screenshot.is_relative_to(Path.cwd().resolve()) or not screenshot.is_file():
        raise RuntimeError("截图必须保存到当前 Worker 工作目录")
    errors = []
    url_result = _run("get", "url", capture=True)
    url = url_result.stdout.strip() if url_result.returncode == 0 else ""
    if not url:
        errors.append("page_url_unavailable")
    requests_result = _run("network", "requests", "--json", capture=True)
    request_ids = []
    if requests_result.returncode == 0:
        try:
            payload = json.loads(requests_result.stdout)
            if payload.get("success"):
                request_ids = list(dict.fromkeys(
                    str(item["requestId"]) for item in payload.get("data", {}).get("requests", [])
                    if isinstance(item, dict) and item.get("requestId")
                ))
            else:
                errors.append("network_requests_unavailable")
        except (ValueError, TypeError):
            errors.append("network_requests_invalid")
    else:
        errors.append("network_requests_unavailable")
    sidecar = screenshot.with_name(screenshot.name + ".muteki-browser.json")
    temporary = sidecar.with_name(sidecar.name + "." + uuid.uuid4().hex)
    metadata = {
        "schema": "muteki-browser-screenshot-v1",
        "run_id": run_id,
        "worker_id": worker_id,
        "tab": active_tab,
        "page_url": url,
        "captured_at": datetime.now(timezone.utc).isoformat(),
        "request_ids": request_ids,
        "request_scope": "active browser tab network log",
        "capture_errors": errors,
    }
    try:
        temporary.write_text(json.dumps(metadata, ensure_ascii=False), encoding="utf-8")
        temporary.chmod(0o600)
        temporary.replace(sidecar)
    finally:
        temporary.unlink(missing_ok=True)


def _main(argv: list[str]) -> int:
    if os.environ.get("MUTEKI_AGENT_BROWSER_ENABLED") != "1":
        raise RuntimeError("此 Worker 的 agent-browser 能力已关闭；可在 Agent 扩展设置中为后续 Worker 启用")
    if not argv or argv[0] in {"--help", "-h", "--version", "-V", "skills", "install", "doctor"}:
        return _run(*argv).returncode
    overrides = {"--session", "--profile", "--cdp", "--auto-connect", "--provider", "--config"}
    if any(arg in overrides or any(arg.startswith(option + "=") for option in overrides)
           for arg in argv):
        raise RuntimeError("Worker 浏览器会话和 Profile 由当前 Run 管理")
    if argv[0] == "batch":
        if not any(not raw.startswith("--") for raw in argv[1:]):
            raise RuntimeError("共享会话中的 batch 请将命令作为参数传入，以便检查标签页操作")
        for raw in argv[1:]:
            if raw.startswith("--"):
                continue
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError:
                if raw.lstrip().startswith("["):
                    raise RuntimeError("batch 命令格式无效") from None
                try:
                    parts = shlex.split(raw)
                except ValueError as exc:
                    raise RuntimeError("batch 命令格式无效") from exc
            else:
                if isinstance(parsed, list) and parsed and all(isinstance(part, str) for part in parsed):
                    parts = parsed
                elif isinstance(parsed, str):
                    parts = shlex.split(parsed)
                else:
                    raise RuntimeError("batch 命令格式无效")
            if parts and parts[0] in _RUN_GLOBAL_COMMANDS | {"tab", "worker-tab", "screenshot"}:
                raise RuntimeError("batch 中的标签页、会话与截图操作请单独执行")
    root_value = os.environ.get("MUTEKI_BROWSER_RUN_ROOT", "")
    worker_id = os.environ.get("MUTEKI_WORKER_ID", "")
    if not root_value or not worker_id:
        raise RuntimeError("Worker 浏览器缺少 Run 或 Worker 身份")
    root = Path(root_value)
    browser_dir = root / ".muteki-browser"
    try:
        config = json.loads((browser_dir / "run.json").read_text(encoding="utf-8"))
        run_id = config["run_id"]
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise RuntimeError("Run 浏览器配置缺失或损坏") from exc
    if not isinstance(run_id, str) or not run_id:
        raise RuntimeError("Run 浏览器会话 ID 无效")
    namespace = str(config["browser_namespace"])
    os.environ["AGENT_BROWSER_SESSION"] = namespace
    os.environ["AGENT_BROWSER_NAMESPACE"] = namespace
    os.environ["AGENT_BROWSER_PROFILE"] = str(root / ".muteki-browser-profile")
    os.environ["AGENT_BROWSER_EXECUTABLE_PATH"] = "/usr/bin/chromium"
    run_temp = os.environ.get("MUTEKI_RUN_TEMP_DIR")
    socket_dir = Path(run_temp) / "browser" if run_temp else Path("/tmp") / ("mb-" + namespace)
    socket_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.environ["AGENT_BROWSER_SOCKET_DIR"] = str(socket_dir)
    browser_dir.mkdir(parents=True, exist_ok=True)
    base = "muteki-" + re.sub(r"[^a-zA-Z0-9_-]", "-", worker_id)[:40]
    worker_states = browser_dir / "worker-tabs"
    worker_states.mkdir(parents=True, exist_ok=True)
    state_path = worker_states / (base + ".json")
    lease_path = browser_dir / "shared-tab-lease.json"
    with (browser_dir / "commands.lock").open("a+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        state = _state(state_path, base)
        tabs = _tabs()
        lease = _lease(lease_path)
        if argv[0] == "worker-tab":
            command = argv[1] if len(argv) > 1 else "status"
            if command == "status":
                print(json.dumps({"active": state["active"], "shared_owner": lease.get("owner", "")},
                                 ensure_ascii=False))
                return 0
            if command == "acquire-shared":
                if lease.get("owner") not in {None, "", worker_id} and (
                    time.time() - float(lease.get("updated", 0)) < LEASE_SECONDS
                ):
                    raise RuntimeError("共享标签页正由另一个 Worker 使用，请稍后重试")
                _ensure_tab(SHARED_LABEL, tabs)
                _save(lease_path, {"owner": worker_id, "updated": time.time()})
                state["active"] = SHARED_LABEL
                _save(state_path, state)
                print("SHARED_TAB_ACQUIRED")
                return 0
            if command == "release-shared":
                if lease.get("owner") != worker_id:
                    raise RuntimeError("当前 Worker 没有共享标签页租约")
                lease_path.unlink(missing_ok=True)
                state["active"] = base
                _save(state_path, state)
                print("SHARED_TAB_RELEASED")
                return 0
            raise RuntimeError("worker-tab 支持 status、acquire-shared、release-shared")
        if argv[0] == "tab":
            command = argv[1] if len(argv) > 1 else "list"
            if command == "list":
                return _run(*argv).returncode
            if command == "new":
                arguments = argv[2:]
                if "--label" in arguments:
                    index = arguments.index("--label")
                    if index + 1 >= len(arguments):
                        raise RuntimeError("tab new --label 缺少名称")
                    requested = arguments[index + 1]
                    del arguments[index:index + 2]
                    suffix = re.sub(r"[^a-zA-Z0-9_-]", "-", requested).strip("-")[:36]
                    if not suffix:
                        raise RuntimeError("标签页名称无效")
                else:
                    suffix = uuid.uuid4().hex[:8]
                label = base + "-" + suffix
                if any(tab.get("label") == label for tab in tabs):
                    raise RuntimeError("当前 Worker 已有同名标签页")
                response = _run("tab", "new", "--label", label, *arguments)
                if response.returncode == 0:
                    state["owned"].append(label)
                    state["active"] = label
                    _save(state_path, state)
                return response.returncode
            if command == "close":
                ref = argv[2] if len(argv) > 2 else state["active"]
                if ref == SHARED_LABEL or state["active"] == SHARED_LABEL and len(argv) <= 2:
                    raise RuntimeError("共享标签页不能关闭；请先释放租约")
                label = _resolve_owned(ref, tabs, state["owned"], base)
                response = _run("tab", "close", label)
                if response.returncode == 0:
                    state["owned"] = [item for item in state["owned"] if item != label]
                    if base not in state["owned"]:
                        state["owned"].insert(0, base)
                    if state["active"] == label:
                        state["active"] = base
                    _save(state_path, state)
                return response.returncode
            label = _resolve_owned(command, tabs, state["owned"], base)
            _ensure_tab(label, tabs)
            state["active"] = label
            _save(state_path, state)
            print(label)
            return 0
        if argv[0] in _RUN_GLOBAL_COMMANDS:
            raise RuntimeError("此命令会改变整个 Run 的浏览器状态，请使用受管 Worker 浏览命令")
        if state["active"] == SHARED_LABEL:
            if lease.get("owner") != worker_id:
                raise RuntimeError("共享标签页租约已失效；请重新申请")
            _save(lease_path, {"owner": worker_id, "updated": time.time()})
        _ensure_tab(state["active"], tabs)
        screenshot_path = ""
        if argv[0] == "screenshot":
            value_options = {"--format", "--quality", "--selector", "--padding", "--threshold"}
            path_index = None
            index = 1
            while index < len(argv):
                if argv[index] in value_options:
                    index += 2
                elif argv[index].startswith("-"):
                    index += 1
                else:
                    path_index = index
                    break
            if path_index is None:
                raise RuntimeError("截图必须指定当前 Worker 工作目录内的路径")
            screenshot = Path(argv[path_index]).resolve()
            if not screenshot.is_relative_to(Path.cwd().resolve()):
                raise RuntimeError("截图必须保存到当前 Worker 工作目录")
            screenshot_path = str(screenshot)
            argv = [*argv]
            argv[path_index] = screenshot_path
        response = _run(*argv)
        if response.returncode == 0 and screenshot_path:
            _record_screenshot(screenshot_path, state["active"], worker_id, run_id)
        return response.returncode


if __name__ == "__main__":
    try:
        raise SystemExit(_main(sys.argv[1:]))
    except (OSError, ValueError, RuntimeError) as exc:
        print(f"agent-browser Worker coordination: {exc}", file=sys.stderr)
        raise SystemExit(2)
