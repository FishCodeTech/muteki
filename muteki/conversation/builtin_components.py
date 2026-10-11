"""Muteki built-in chat tools the user can explicitly select in the composer.

Computer use, the Muteki browser and visualize are delivered to every gateway
session already; selecting one turns it into an explicit directive for the
current turn (optionally with a target macOS app resolved from the host).
"""

from __future__ import annotations

import plistlib
import time
from pathlib import Path
from typing import Any

from muteki.capability_management import enabled as capability_enabled

COMPUTER = "computer-use"
BROWSER = "browser"
VISUALIZE = "visualize"
_PREFIX = "component:"
_APP_PREFIX = f"{_PREFIX}{COMPUTER}:app:"
_SOURCE = "Muteki 内置"
_APP_ROWS_LIMIT = 30
_INVENTORY_TTL_SECONDS = 60.0


class BuiltinComponentError(ValueError):
    """A selected built-in tool or target app is unknown or unavailable."""


def _app_dirs() -> tuple[Path, ...]:
    return (
        Path("/Applications"),
        Path("/Applications/Utilities"),
        Path("/System/Applications"),
        Path("/System/Applications/Utilities"),
        Path.home() / "Applications",
    )


_inventory_cache: tuple[float, list[dict[str, str]], list[dict[str, Any]]] | None = None


def mac_app_inventory() -> tuple[list[dict[str, str]], list[dict[str, Any]]]:
    """Installed apps (bundle id, name, path) plus per-app read errors."""
    global _inventory_cache
    now = time.monotonic()
    if _inventory_cache is not None and now - _inventory_cache[0] < _INVENTORY_TTL_SECONDS:
        return _inventory_cache[1], _inventory_cache[2]
    apps: dict[str, dict[str, str]] = {}
    errors: list[dict[str, Any]] = []
    for directory in _app_dirs():
        if not directory.is_dir():
            continue
        for bundle in sorted(directory.glob("*.app")):
            info = bundle / "Contents" / "Info.plist"
            try:
                with info.open("rb") as handle:
                    plist = plistlib.load(handle)
            except FileNotFoundError:
                continue
            except (OSError, plistlib.InvalidFileException, ValueError) as exc:
                errors.append({"section": "apps", "code": "conversation.composer.app_unreadable",
                               "message": f"{bundle}: {exc}", "exception_type": type(exc).__name__})
                continue
            bundle_id = str(plist.get("CFBundleIdentifier") or "").strip()
            if not bundle_id or bundle_id in apps:
                continue
            name = str(plist.get("CFBundleDisplayName") or plist.get("CFBundleName") or bundle.stem).strip()
            apps[bundle_id] = {"bundle_id": bundle_id, "name": name or bundle.stem, "path": str(bundle)}
    listed = sorted(apps.values(), key=lambda app: app["name"].casefold())
    _inventory_cache = (now, listed, errors)
    return listed, errors


def _availability(component: str, engine: str, *, gateway: bool) -> tuple[bool, str, str]:
    if component == VISUALIZE:
        return True, "", ""
    if not gateway:
        return (False, "当前 Runtime 未接入 Muteki 工具网关，无法使用该内置工具",
                "请改用已接入 Muteki 工具网关的 Agent")
    if component == COMPUTER:
        from muteki.conversation.computer_control import server_status
        if not capability_enabled("mcp", COMPUTER):
            return False, "电脑操控已在能力管理中停用", "在设置 → 能力管理中启用电脑操控"
        status = server_status()
        if not status["available"]:
            return False, f"{status['code']}: {status['message']}", "安装或修复 Codex Computer Use 后重试"
    return True, "", ""


def _row(component: str, *, engine: str, gateway: bool, name: str, description: str,
         item_id: str = "", target: dict[str, str] | None = None) -> dict[str, Any]:
    invocable, reason, alternative = _availability(component, engine, gateway=gateway)
    row: dict[str, Any] = {
        "id": item_id or f"{_PREFIX}{component}",
        "kind": "component",
        "component": component,
        "name": name,
        "description": description,
        "source": _SOURCE,
        "scope": "thread",
        "engine": engine,
        "channel": "session_config",
        "delivery": "guaranteed" if invocable else "local",
        "verification": "verified",
        "status": "available" if invocable else "unavailable",
        "support_level": "supported" if invocable else "unsupported",
        "reason": reason,
        "alternative": alternative,
        "invocable": invocable,
        "action": "select-capability" if invocable else "inspect-runtime-capability",
    }
    if target is not None:
        row["target"] = {"type": "mac_app", **target}
    return row


def _app_row(app: dict[str, str], *, engine: str, gateway: bool) -> dict[str, Any]:
    return _row(COMPUTER, engine=engine, gateway=gateway, name=app["name"],
                description=f"用电脑操控本机应用 · {app['bundle_id']}",
                item_id=f"{_APP_PREFIX}{app['bundle_id']}", target=app)


def component_rows(engine: str, *, gateway: bool, query: str = "", include_apps: bool = True,
                   errors: list[dict[str, Any]] | None = None) -> list[dict[str, Any]]:
    rows = [
        _row(COMPUTER, engine=engine, gateway=gateway, name="电脑操控 Computer Use",
             description="让 Agent 操作这台 Mac；输入应用名可指定目标应用"),
        _row(BROWSER, engine=engine, gateway=gateway, name="Muteki 浏览器 Browser",
             description="让 Agent 使用右侧内置浏览器打开、读取和操作网页"),
        _row(VISUALIZE, engine=engine, gateway=gateway, name="Visualize 图形展示",
             description="让 Agent 用 muteki-visualize 输出可交互图形"),
    ]
    needle = query.casefold().strip()
    if include_apps and needle:
        apps, app_errors = mac_app_inventory()
        if errors is not None:
            errors.extend(app_errors)
        matched = [app for app in apps
                   if needle in app["name"].casefold() or needle in app["bundle_id"].casefold()]
        rows.extend(_app_row(app, engine=engine, gateway=gateway) for app in matched[:_APP_ROWS_LIMIT])
    return rows


def is_component_id(item_id: str) -> bool:
    return item_id.startswith(_PREFIX)


def resolve_component(item_id: str, *, engine: str, gateway: bool) -> dict[str, Any]:
    if item_id.startswith(_APP_PREFIX):
        bundle_id = item_id.removeprefix(_APP_PREFIX)
        apps, _ = mac_app_inventory()
        app = next((candidate for candidate in apps if candidate["bundle_id"] == bundle_id), None)
        if app is None:
            raise BuiltinComponentError(f"本机未找到应用 {bundle_id}，请重新选择目标应用")
        row = _app_row(app, engine=engine, gateway=gateway)
    else:
        rows = {row["id"]: row for row in component_rows(engine, gateway=gateway, include_apps=False)}
        row = rows.get(item_id)
        if row is None:
            raise BuiltinComponentError("所选内置工具不存在，请重新选择")
    if not row["invocable"]:
        raise BuiltinComponentError(f"{row['name']} 当前不可用：{row['reason']}")
    return row


def component_instruction(items: list[dict[str, Any]]) -> str:
    """One directive block for every built-in tool selected in this turn."""
    if not items:
        return ""
    selected = {str(item.get("component")) for item in items}
    apps = [item["target"] for item in items if isinstance(item.get("target"), dict)]
    lines = ["[用户显式选择的 Muteki 内置工具]"]
    if COMPUTER in selected:
        lines.append(
            "- 电脑操控：本轮请求通过 muteki-control 网关的 chat_computer_js（必要时 chat_computer_js_reset）"
            "操作本机；若工具延迟加载，先通过工具发现搜索这些名字。不要调用原生 cua_repl，"
            "也不要用 shell、AppleScript 等其他入口替代。")
        for app in apps:
            lines.append(
                f"  目标应用：{app['name']}（bundle ID {app['bundle_id']}，{app['path']}）。"
                f"用 await cua.getApp(\"{app['bundle_id']}\") 绑定；绑定失败时如实报告错误，不要换用其他应用。")
    if BROWSER in selected:
        lines.append(
            "- Muteki 浏览器：本轮使用右侧 Muteki 浏览器的 chat_browser_* 工具"
            "（open、navigate、read、click、type、screenshot 等）完成网页操作；不要改用其他浏览器、"
            "电脑操控或 curl 代替。工具返回 chat.browser.* 错误时如实报告。")
    if VISUALIZE in selected:
        lines.append("- Visualize：本轮回答用 muteki-visualize 围栏输出图形或交互界面，遵循 [Muteki 聊天展示接口] 的约定。")
    return "\n".join(lines)


__all__ = [
    "BuiltinComponentError", "component_instruction", "component_rows",
    "is_component_id", "mac_app_inventory", "resolve_component",
]
