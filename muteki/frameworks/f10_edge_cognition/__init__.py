"""f10 edge cognition swarm framework.

Lazy exports: importing a lightweight submodule (state/shell/schema) must not
pull the whole Swarm stack — cli_solver imports state/shell on the worker path,
and muteki.swarm.swarm (imported by swarm.py) reaches back into solver modules.
"""

from __future__ import annotations

from typing import Any

__all__ = ["SwarmF10", "Swarm"]


def __getattr__(name: str) -> Any:
    if name in ("SwarmF10", "Swarm"):
        from muteki.frameworks.f10_edge_cognition.swarm import (
            Swarm,
            SwarmF10,
        )
        return {"SwarmF10": SwarmF10, "Swarm": Swarm}[name]
    raise AttributeError(name)
