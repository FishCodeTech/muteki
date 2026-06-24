"""Sandbox manager — a per-run scratch root with cleanup.

Originally drove per-solver Python kernels; with the CLI executor (which runs its
own shell), it is now just a lightweight owner of a scratch directory: it provides
the run's root path (the shared-graph DB and artifacts live under it unless a
graph_dir is set) and rmtree's it on shutdown.
"""

from __future__ import annotations

import shutil
import tempfile
from pathlib import Path
from typing import Optional

from muteki.core.event_bus import EventBus


class SandboxManager:
    def __init__(
        self,
        bus: Optional[EventBus] = None,
        *,
        root: Optional[str | Path] = None,
        artifacts_dir: Optional[str | Path] = None,
        mem_mb: int = 2048,
        cpu_s: int = 120,
    ) -> None:
        self.bus = bus
        self.mem_mb = mem_mb
        self.cpu_s = cpu_s
        self._root = Path(root) if root else Path(tempfile.mkdtemp(prefix="muteki-sbx-"))
        self._root.mkdir(parents=True, exist_ok=True)
        self._artifacts_dir = (
            Path(artifacts_dir) if artifacts_dir else self._root / "artifacts"
        ).resolve()
        self._artifacts_dir.mkdir(parents=True, exist_ok=True)

    @property
    def root(self) -> Path:
        """Sandbox root dir (the shared graph DB lives under it, per run)."""
        return self._root

    @property
    def artifacts_dir(self) -> Path:
        return self._artifacts_dir

    async def shutdown_all(self) -> None:
        try:
            shutil.rmtree(self._root, ignore_errors=True)
        except OSError:
            pass
