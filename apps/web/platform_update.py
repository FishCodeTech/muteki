"""Web 设置页使用的平台升级控制器。"""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from typing import Any

from muteki.updater import UpdateManager


class PlatformUpdateController:
    def __init__(self, install_root: Path | None = None) -> None:
        self.manager = UpdateManager(install_root)
        self._task: asyncio.Task[None] | None = None

    def status(self) -> dict[str, Any]:
        if os.environ.get("MUTEKI_MANAGED_DESKTOP") == "1":
            from muteki.version import get_version
            return {"status": "idle", "running": False, "current_version": get_version(), "install_kind": "desktop", "deployment": "desktop", "available": False, "message": "客户端与后端作为完整桌面版本更新，请使用桌面应用的更新入口。"}
        payload = self.manager.status()
        payload["running"] = bool(self._task and not self._task.done())
        return payload

    async def check(self, target: str | None = None) -> dict[str, Any]:
        if os.environ.get("MUTEKI_MANAGED_DESKTOP") == "1":
            return self.status()
        if self._task and not self._task.done():
            return self.status()
        await asyncio.to_thread(self.manager.check, target)
        return self.status()

    async def start(self, target: str | None = None, *, force: bool = False) -> dict[str, Any]:
        if os.environ.get("MUTEKI_MANAGED_DESKTOP") == "1":
            raise RuntimeError("托管后端随桌面应用整包更新，请使用桌面应用的更新入口。")
        if self._task and not self._task.done():
            return self.status()
        if self.manager.status().get("deployment") == "compose":
            raise RuntimeError("容器部署请在宿主机执行 muteki upgrade --compose")

        async def run() -> None:
            try:
                await asyncio.to_thread(self.manager.upgrade, target, force=force)
            except Exception:
                # UpdateManager 已把可展示错误写入 update-state.json。
                return

        self._task = asyncio.create_task(run())
        await asyncio.sleep(0)
        return self.status()

    async def rollback(self) -> dict[str, Any]:
        if os.environ.get("MUTEKI_MANAGED_DESKTOP") == "1":
            raise RuntimeError("托管后端必须与桌面应用及数据备份一起恢复，请使用桌面应用的恢复入口。")
        if self._task and not self._task.done():
            return self.status()
        if self.manager.status().get("deployment") == "compose":
            raise RuntimeError("容器部署请在宿主机执行 muteki rollback --compose")
        await asyncio.to_thread(self.manager.rollback)
        return self.status()
