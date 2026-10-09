"""Droid model discovery shared by chat and credential catalogs, without SDK/UI imports."""

from __future__ import annotations

import asyncio
import os
import re
import shutil
import subprocess
import threading
import time
from typing import Any

from muteki.platform.contracts.agent_events import redact_secrets


class DroidModelDiscoveryError(RuntimeError):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(redact_secrets(message))
        self.code = code


def parse_droid_models(text: str) -> list[dict[str, Any]]:
    """Parse the CLI's model table and native effort metadata."""
    models: list[tuple[str, str]] = []
    in_models = False
    for line in text.splitlines():
        if line.strip() == "Available Models:":
            in_models = True
            continue
        if in_models:
            if not line.strip() or line.startswith("Model details"):
                break
            parts = line.split()
            if len(parts) >= 2:
                models.append((parts[0], " ".join(parts[1:]).replace("[Deprecated]", "").strip()))
    efforts: dict[str, tuple[list[str], str]] = {}
    for line in text.splitlines():
        value = line.strip()
        if not value.startswith("- ") or "supported:" not in value:
            continue
        label = value[2:].split(":", 1)[0].strip()
        supported = value.split("supported:", 1)[1].split("]", 1)[0].strip(" [")
        default = value.split("default:", 1)[1].strip().rstrip(".") if "default:" in value else ""
        levels = list(dict.fromkeys(item.strip() for item in supported.split(",")
                                    if item.strip() not in {"none", "default"}
                                    and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", item.strip())))
        efforts[label.casefold()] = (levels, default if default in levels else "")
    return [{"id": model_id, "label": label, "reasoning": {
        "supported": bool(levels), "levels": levels, "default": default,
        "kind": "effort", "source": "droid_exec_help",
    }} for model_id, label in models for levels, default in [efforts.get(label.casefold(), ([], ""))]]


_CACHE_LOCK = threading.Lock()
_CACHE: dict[str, tuple[float, tuple[str, ...]]] = {}


def droid_model_ids(binary: str | None = None) -> list[str]:
    """Bounded, cached sync discovery for callers running outside the event loop."""
    try:
        asyncio.get_running_loop()
    except RuntimeError:
        pass
    else:
        raise DroidModelDiscoveryError("droid.models.async_required", "use await discover_droid_model_ids() from an async caller")
    executable = binary or os.environ.get("MUTEKI_DROID_BIN") or "droid"
    executable = shutil.which(executable) or executable
    with _CACHE_LOCK:
        cached = _CACHE.get(executable)
        if cached and cached[0] > time.monotonic():
            return list(cached[1])
        try:
            completed = subprocess.run([executable, "exec", "--help"], capture_output=True,
                                       text=True, timeout=20, check=False)
        except FileNotFoundError as exc:
            raise DroidModelDiscoveryError("droid.models.binary_missing", "Droid CLI is not installed") from exc
        except subprocess.TimeoutExpired as exc:
            raise DroidModelDiscoveryError("droid.models.timeout", "droid exec --help exceeded 20 seconds") from exc
        except OSError as exc:
            raise DroidModelDiscoveryError("droid.models.start_failed", f"Droid model discovery failed: {exc}") from exc
        if completed.returncode != 0:
            raise DroidModelDiscoveryError("droid.models.help_failed", completed.stderr or completed.stdout or f"Droid exited with code {completed.returncode}")
        models = tuple(str(row["id"]) for row in parse_droid_models(completed.stdout))
        if not models:
            raise DroidModelDiscoveryError("droid.models.invalid_response", "Droid help did not contain a model table")
        _CACHE[executable] = (time.monotonic() + 60, models)
        return list(models)


async def discover_droid_model_ids(binary: str | None = None) -> list[str]:
    return await asyncio.to_thread(droid_model_ids, binary)
