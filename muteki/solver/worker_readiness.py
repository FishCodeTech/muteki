"""Opt-in reuse of successful, real Worker CLI model probes.

This cache is separate from Runtime capability reports: only the CLI model
probe's result can enter it. Entries are process-local and expire; concurrent
Runs share an in-flight probe until its last waiter leaves.
"""

from __future__ import annotations

import asyncio
import copy
import math
import os
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Awaitable, Callable

from muteki.external_agents.probe_cache import BinaryIdentity, config_hash
from muteki.solver.credential_accounts import CredentialAccountStore, account_store_root

READINESS_TTL_ENV = "MUTEKI_WORKER_READINESS_CACHE_TTL_SECONDS"


def readiness_cache_ttl() -> float:
    """Ordinary UI Runs retain their existing behavior unless explicitly enabled."""
    value = float(os.environ.get(READINESS_TTL_ENV, "0"))
    if not math.isfinite(value) or value < 0:
        raise ValueError(f"{READINESS_TTL_ENV} must be finite and >= 0")
    return value


def readiness_identity(
    profile: dict[str, Any], *, runtime: dict[str, Any], backend: str,
    sessions_root: Path,
) -> str | None:
    """Identify the selected credential, executable and execution environment.

    Unbound host logins and uninspectable images cannot supply a trustworthy
    identity, so their real probe runs without reuse. No credential bytes are
    read for this key: CredentialAccountStore.revision is public metadata.
    """
    account = str(profile.get("credential_account") or "").strip()
    if not account:
        return None
    root = account_store_root(sessions_root)
    revision = CredentialAccountStore(root).revision(account)
    if not revision:
        return None
    if backend == "container":
        from muteki.solver.container_exec import WORKER_IMAGE
        from muteki.solver.workspace import worker_image_identity

        image = worker_image_identity(WORKER_IMAGE)
        if not image.get("id"):
            return None
        executable: Any = {"image": image}
    else:
        from muteki.solver.cli_driver import driver_for

        binary = BinaryIdentity.of(driver_for(profile).bin)
        if not binary.exists:
            return None
        executable = {
            "cli": binary.to_dict(),
            "node": BinaryIdentity.of("node").to_dict(),
            "python": BinaryIdentity.of(sys.executable).to_dict(),
        }
    return config_hash({
        "contract": "worker-cli-model-v3",
        "profile": profile,
        "credential_root": str(root.resolve()),
        "credential_revision": revision,
        "backend": backend,
        "runtime": runtime,
        "executable": executable,
        "uid": os.getuid() if hasattr(os, "getuid") else None,
        # Environment overlays can change model transport, proxy or CLI config.
        # Only its digest is retained, including for inherited secret env vars.
        "environment": config_hash(dict(os.environ)),
    })


@dataclass
class _Flight:
    task: asyncio.Task[dict[str, Any]]
    generation: int
    waiters: int = 0


class WorkerReadinessCache:
    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._successes: dict[str, tuple[float, dict[str, Any]]] = {}
        self._flights: dict[tuple[asyncio.AbstractEventLoop, str], _Flight] = {}
        self._quarantined: dict[tuple[Any, ...], str] = {}
        self._generation = 0

    def invalidate(self) -> None:
        """A known runtime failure/configuration change requires a fresh probe."""
        with self._lock:
            self._generation += 1
            self._successes.clear()

    def quarantine(self, key: tuple[Any, ...], detail: str) -> None:
        """Record a deterministic profile initialization failure for this config/revision."""
        with self._lock:
            self._quarantined[tuple(key)] = str(
                detail or "worker profile initialization failed"
            )

    def quarantined_detail(self, key: tuple[Any, ...]) -> str | None:
        with self._lock:
            return self._quarantined.get(tuple(key))

    def clear_quarantine(self) -> None:
        with self._lock:
            self._quarantined.clear()

    async def get(
        self, key: str, probe: Callable[[], Awaitable[dict[str, Any]]], *,
        ttl_seconds: float,
    ) -> dict[str, Any]:
        if not math.isfinite(ttl_seconds) or ttl_seconds <= 0:
            raise ValueError("Worker readiness cache TTL must be finite and > 0")
        loop = asyncio.get_running_loop()
        flight_key = (loop, key)
        with self._lock:
            now = time.monotonic()
            for stale in [k for k, (expires, _) in self._successes.items() if expires <= now]:
                self._successes.pop(stale)
            hit = self._successes.get(key)
            if hit is not None:
                return copy.deepcopy(hit[1])
            flight = self._flights.get(flight_key)
            if flight is None or flight.generation != self._generation:
                task = loop.create_task(probe())
                flight = _Flight(task=task, generation=self._generation)
                self._flights[flight_key] = flight
            flight.waiters += 1
        try:
            result = await asyncio.shield(flight.task)
            with self._lock:
                # A successful capability/structured report is not CLI evidence.
                if (flight.generation == self._generation
                        and result.get("ok") is True
                        and result.get("exit_code") == 0
                        and result.get("command")
                        and result.get("backend") in {"local", "container"}):
                    self._successes[key] = (
                        time.monotonic() + ttl_seconds, copy.deepcopy(result))
            return copy.deepcopy(result)
        finally:
            with self._lock:
                flight.waiters -= 1
                abandoned = flight.waiters == 0
                if abandoned and self._flights.get(flight_key) is flight:
                    self._flights.pop(flight_key, None)
            if abandoned and not flight.task.done():
                flight.task.cancel()
                await asyncio.gather(flight.task, return_exceptions=True)


worker_readiness_cache = WorkerReadinessCache()
