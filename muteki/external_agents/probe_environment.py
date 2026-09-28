"""Task-local subprocess environment for capability probes (no global mutation)."""
from __future__ import annotations

from contextvars import ContextVar
import os

PROBE_ENVIRONMENT: ContextVar[dict[str, str] | None] = ContextVar("agent_probe_environment", default=None)


def subprocess_environment(values: dict[str, str] | None = None) -> dict[str, str] | None:
    probe = PROBE_ENVIRONMENT.get()
    if values is None and probe is None:
        return None
    return {**os.environ, **(values or {}), **(probe or {})}
