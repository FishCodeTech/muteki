"""可用 Agent CLI 的只读版本检查。

这里仅读取发布元数据，或调用 CLI 明确提供的 ``--check`` 子命令。模块不
包含安装、升级、下载二进制或改写用户配置的路径；检查失败也不会影响 CLI
健康状态。
"""

from __future__ import annotations

import asyncio
import json
import os
import re
from typing import Any
from urllib.parse import quote

import time

import httpx

from muteki.platform.contracts.base import utcnow

from .capabilities import ProbeCommandResult, run_probe_command

#: Release metadata changes slowly; one lookup per install per five minutes.
VERSION_CHECK_TTL_S = 300.0


_NPM_PACKAGES = {
    "claude": "@anthropic-ai/claude-code",
    "codex": "@openai/codex",
    "pi": "@earendil-works/pi-coding-agent",
}

_CURSOR_VERSION_RE = re.compile(r"\b\d{4}\.\d{2}\.\d{2}-[0-9a-z]+\b", re.I)
_VERSION_RE = re.compile(
    r"(?<!\d)(\d+(?:\.\d+){1,3}(?:(?:a|b|alpha|beta|rc|dev)\d+)?"
    r"(?:-[0-9a-z][0-9a-z._-]*)?)(?!\d)",
    re.I,
)
_CURSOR_DOWNLOAD_RE = re.compile(
    r"https://downloads\.cursor\.com/lab/([^/\"'\s]+)/", re.I)


def installed_version(engine: str, runtime_version: str) -> str:
    """从 ``--version`` 原始输出中抽取用于比较的版本号。"""
    text = str(runtime_version or "").strip()
    if not text:
        return ""
    if engine == "cursor":
        match = _CURSOR_VERSION_RE.search(text)
        return match.group(0) if match else ""
    match = _VERSION_RE.search(text)
    return match.group(1) if match else ""


def compare_versions(installed: str, latest: str) -> int | None:
    """比较常见 CLI 版本；返回 -1/0/1，无法可靠比较时返回 None。"""
    left = installed.strip().lower().removeprefix("v")
    right = latest.strip().lower().removeprefix("v")
    if left == right:
        return 0

    def parse(value: str) -> tuple[tuple[int, ...], tuple[int, int] | None, str] | None:
        match = re.fullmatch(r"(\d+(?:\.\d+){1,3})(.*)", value)
        if not match:
            return None
        core = tuple(int(part) for part in match.group(1).split("."))
        core += (0,) * (4 - len(core))
        suffix = match.group(2).lstrip("-+._")
        if not suffix:
            prerelease: tuple[int, int] | None = (4, 0)
        else:
            pre = re.fullmatch(r"(dev|a|alpha|b|beta|rc)[.-]?(\d*)", suffix)
            if pre:
                ranks = {"dev": 0, "a": 1, "alpha": 1, "b": 2, "beta": 2, "rc": 3}
                prerelease = (ranks[pre.group(1)], int(pre.group(2) or 0))
            else:
                prerelease = None
        return core, prerelease, suffix

    parsed_left = parse(left)
    parsed_right = parse(right)
    if parsed_left is None or parsed_right is None:
        return None
    left_core, left_pre, _ = parsed_left
    right_core, right_pre, _ = parsed_right
    if left_core != right_core:
        return -1 if left_core < right_core else 1
    if left_pre is None or right_pre is None:
        return None
    if left_pre == right_pre:
        return 0
    return -1 if left_pre < right_pre else 1


async def _http_text(url: str) -> str:
    timeout = httpx.Timeout(6.0, connect=4.0)
    headers = {"Accept": "application/json, text/plain;q=0.9, */*;q=0.8"}
    http_proxy = next((
        value for value in (
            os.environ.get("HTTPS_PROXY"), os.environ.get("https_proxy"),
            os.environ.get("HTTP_PROXY"), os.environ.get("http_proxy"),
        )
        if value and value.lower().startswith(("http://", "https://"))
    ), None)
    async with httpx.AsyncClient(
        timeout=timeout, follow_redirects=True, headers=headers,
        proxy=http_proxy, trust_env=False,
    ) as client:
        response = await client.get(url)
        response.raise_for_status()
        return response.text


async def _npm_latest(package: str, *, major: int | None = None) -> str:
    payload = json.loads(await _http_text(
        f"https://registry.npmjs.org/{quote(package, safe='')}/latest"))
    latest = str(payload.get("version") or "").strip()
    if major is None or latest.split(".", 1)[0] == str(major):
        return latest
    # T3's OpenCode updates stay on the installed major, including when a
    # package's latest tag has moved to the next runtime generation.
    metadata = json.loads(await _http_text(
        f"https://registry.npmjs.org/{quote(package, safe='')}"))
    versions = metadata.get("versions") if isinstance(metadata, dict) else None
    candidates = [value for value in versions or {}
                  if re.fullmatch(r"\d+\.\d+\.\d+", value) and value.split(".", 1)[0] == str(major)]
    if not candidates:
        raise ValueError(f"No stable {package} release for installed major {major}")
    return max(candidates, key=lambda value: tuple(int(part) for part in value.split(".")))


async def _run_check_only(binary: str, *args: str) -> str:
    """执行厂商声明的只读检查命令；调用处只允许传入 ``--check``。"""
    if args not in {("update", "--check"), ("update", "--check", "--json")}:
        raise ValueError("版本子进程只允许厂商的只读检查参数")
    env = dict(os.environ)
    env.update({"NO_COLOR": "1", "TERM": "dumb", "CI": "1", "OMP_SKIP_SETUP": "1"})
    result = await run_probe_command([binary, *args], timeout=12.0, env=env)
    if not result.ok:
        raise VersionCheckCommandError(result)
    return (result.stdout or result.stderr).strip()


class VersionCheckCommandError(RuntimeError):
    """The vendor ``update --check`` command failed; carries the full result."""

    def __init__(self, result: ProbeCommandResult) -> None:
        self.result = result
        self.code = result.error_code.value if result.error_code else ""
        super().__init__(result.describe())


async def _latest_for(engine: str, binary_path: str, version: str = "") -> tuple[str, str]:
    if engine == "opencode":
        major = int(version.split(".", 1)[0])
        package = "opencode-ai" if major == 1 else "@opencode/cli"
        return await _npm_latest(package, major=major), f"npm:{package}"
    if engine in _NPM_PACKAGES:
        package = _NPM_PACKAGES[engine]
        return await _npm_latest(package), f"npm:{package}"
    if engine == "omp":
        output = await _run_check_only(binary_path, "update", "--check")
        available = re.search(r"New version available:\s*v?([^\s]+)", output, re.I)
        current = re.search(r"Current version:\s*v?([^\s]+)", output, re.I)
        return str((available or current).group(1) if available or current else ""), "omp:check"
    if engine == "grok":
        payload = json.loads(await _run_check_only(
            binary_path, "update", "--check", "--json"))
        return str(payload.get("latestVersion") or "").strip(), "grok:check"
    if engine == "kimi":
        return (await _http_text("https://code.kimi.ai/kimi-code/latest")).strip(), "kimi:release"
    if engine == "cursor":
        script = await _http_text("https://cursor.com/install")
        match = _CURSOR_DOWNLOAD_RE.search(script)
        return (match.group(1) if match else ""), "cursor:installer-metadata"
    raise ValueError(f"没有 {engine!r} 的版本元数据来源")


async def check_cli_version(
    engine: str,
    binary_path: str,
    runtime_version: str,
) -> dict[str, Any]:
    """返回一次只读版本检查结果；任何失败都归一为 ``unknown``。"""
    checked_at = utcnow().isoformat()
    installed = installed_version(engine, runtime_version)
    base: dict[str, Any] = {
        "installed_version": installed,
        "latest_version": "",
        "status": "unknown",
        "update_available": False,
        "checked_at": checked_at,
        "source": "",
        "detail": "最新版本未获取",
        "stale": False,
    }
    if not installed:
        base["detail"] = "尚未取得已安装版本"
        return base
    if not binary_path:
        base["detail"] = "尚未找到本机 CLI"
        return base
    try:
        latest_raw, source = await _latest_for(engine, binary_path, installed)
        latest = installed_version(engine, latest_raw)
        if not latest:
            raise ValueError("发布元数据中没有可识别的版本号")
        comparison = compare_versions(installed, latest)
        base.update({"latest_version": latest, "source": source})
        if comparison is None:
            base["detail"] = f"已获取最新版本 {latest}，版本格式暂无法比较"
        elif comparison < 0:
            base.update({
                "status": "update_available",
                "update_available": True,
                "detail": f"可更新至 {latest}",
            })
        else:
            base.update({"status": "current", "detail": "已是最新版本"})
    except Exception as exc:  # noqa: BLE001 — 版本源故障不影响 CLI 健康状态
        base["detail"] = "最新版本未获取"
        base["error"] = f"{type(exc).__name__}: {exc}"
        base["error_code"] = getattr(exc, "code", "version_check.request_failed" if isinstance(exc, httpx.HTTPError)
                                         else "version_check.metadata_invalid")
    return base


_VERSION_RESULTS: dict[tuple[str, str, str], tuple[float, dict[str, Any]]] = {}
_VERSION_FLIGHTS: dict[tuple[str, str, str], "asyncio.Task[dict[str, Any]]"] = {}


async def cached_check_cli_version(
    engine: str,
    binary_path: str,
    runtime_version: str,
    *,
    force: bool = False,
    ttl_s: float = VERSION_CHECK_TTL_S,
) -> dict[str, Any]:
    """``check_cli_version`` with a per-install TTL and single-flight.

    Keyed by engine + binary path + installed version, so an upgrade is
    checked again immediately.  ``force`` skips a fresh result.
    """
    key = (engine, binary_path, runtime_version)
    cached = _VERSION_RESULTS.get(key)
    if cached is not None and not force and time.monotonic() - cached[0] < ttl_s:
        return dict(cached[1])
    loop = asyncio.get_running_loop()
    task = _VERSION_FLIGHTS.get(key)
    if task is None or task.get_loop() is not loop:
        async def perform() -> dict[str, Any]:
            result = await check_cli_version(engine, binary_path, runtime_version)
            _VERSION_RESULTS[key] = (time.monotonic(), dict(result))
            return result

        task = loop.create_task(perform())
        _VERSION_FLIGHTS[key] = task

        def forget(done: "asyncio.Task[dict[str, Any]]") -> None:
            if _VERSION_FLIGHTS.get(key) is done:
                _VERSION_FLIGHTS.pop(key, None)

        task.add_done_callback(forget)
    return dict(await asyncio.shield(task))


__all__ = [
    "VERSION_CHECK_TTL_S",
    "VersionCheckCommandError",
    "cached_check_cli_version",
    "check_cli_version",
    "compare_versions",
    "installed_version",
]
