"""Persistent operator policy for independently managed capability sources."""

from __future__ import annotations

import json
import threading
from pathlib import Path
from typing import Any


_LOCK = threading.RLock()
_PATH: Path | None = None
_DEFAULTS = {
    "mcp": {"muteki-control": True, "computer-use": True},
    "skills": {"muteki-blackboard": True, "agent-browser": True},
}


def configure(path: str | Path) -> None:
    global _PATH
    with _LOCK:
        _PATH = Path(path)


def _state() -> dict[str, dict[str, bool]]:
    state = {kind: dict(items) for kind, items in _DEFAULTS.items()}
    path = _PATH
    if path is None or not path.is_file():
        return state
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return state
    if not isinstance(raw, dict):
        return state
    for kind, defaults in state.items():
        values = raw.get(kind)
        if not isinstance(values, dict):
            continue
        for resource_id in defaults:
            if isinstance(values.get(resource_id), bool):
                defaults[resource_id] = values[resource_id]
    return state


def enabled(kind: str, resource_id: str) -> bool:
    with _LOCK:
        return bool(_state().get(kind, {}).get(resource_id, False))


def set_enabled(kind: str, resource_id: str, value: bool) -> dict[str, Any]:
    if kind not in _DEFAULTS or resource_id not in _DEFAULTS[kind]:
        raise KeyError(f"unknown capability resource: {kind}/{resource_id}")
    with _LOCK:
        state = _state()
        state[kind][resource_id] = bool(value)
        path = _PATH
        if path is None:
            raise RuntimeError("capability management path is not configured")
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(path.suffix + ".tmp")
        temporary.write_text(
            json.dumps(state, ensure_ascii=False, indent=2), encoding="utf-8")
        temporary.replace(path)
        return {"kind": kind, "id": resource_id, "enabled": bool(value)}


def snapshot() -> dict[str, dict[str, bool]]:
    with _LOCK:
        return _state()
