"""Project Muteki 版本与运行形态识别。"""

from __future__ import annotations

import json
import os
from importlib import metadata
from pathlib import Path
from typing import Any

FALLBACK_VERSION = "0.3.2"


def project_root() -> Path:
    configured = os.environ.get("MUTEKI_RELEASE_ROOT")
    if configured:
        return Path(configured).expanduser().resolve()
    cwd = Path.cwd().resolve()
    if (cwd / "pyproject.toml").is_file() and (cwd / "muteki").is_dir():
        return cwd
    return Path(__file__).resolve().parents[1]


def release_metadata(root: Path | None = None) -> dict[str, Any]:
    base = (root or project_root()).resolve()
    path = base / ".muteki-release.json"
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, TypeError):
        return {}
    return payload if isinstance(payload, dict) else {}


def get_version(root: Path | None = None) -> str:
    payload = release_metadata(root)
    value = str(payload.get("version") or "").strip()
    if value:
        return value.removeprefix("v")
    try:
        return metadata.version("project-muteki")
    except metadata.PackageNotFoundError:
        return FALLBACK_VERSION


def version_payload(root: Path | None = None) -> dict[str, Any]:
    base = (root or project_root()).resolve()
    release = release_metadata(base)
    managed_root = os.environ.get("MUTEKI_INSTALL_ROOT")
    managed = bool(managed_root and (Path(managed_root).expanduser() / "install.json").is_file())
    deployment = os.environ.get("MUTEKI_DEPLOYMENT_MODE")
    return {
        "version": get_version(base),
        "commit": release.get("commit"),
        "built_at": release.get("built_at"),
        "install_kind": "compose" if deployment == "compose" else "managed" if managed else "source",
        "root": str(base),
        "install_root": str(Path(managed_root).expanduser().resolve()) if managed_root else None,
    }
