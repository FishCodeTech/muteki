"""Web 设置页使用的平台升级控制器。"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

from muteki.updater import UpdateManager


class PlatformUpdateController:
    def __init__(self, install_root: Path | None = None) -> None:
        self.manager = UpdateManager(install_root)
        self._task: asyncio.Task[None] | None = None

    def status(self) -> dict[str, Any]:
        payload = self.manager.status()
        payload["running"] = bool(self._task and not self._task.done())
        return payload

    async def check(self, target: str | None = None) -> dict[str, Any]:
        if self._task and not self._task.done():
            return self.status()
        await asyncio.to_thread(self.manager.check, target)
        return self.status()

    async def start(self, target: str | None = None, *, force: bool = False) -> dict[str, Any]:
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
        if self._task and not self._task.done():
            return self.status()
        if self.manager.status().get("deployment") == "compose":
            raise RuntimeError("容器部署请在宿主机执行 muteki rollback --compose")
        await asyncio.to_thread(self.manager.rollback)
        return self.status()
