"""声明式 Extension UI Contribution 的受控 schema。"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

UI_SCHEMA_VERSION = 1
MAX_UI_BYTES = 64 * 1024
MAX_COMPONENTS = 64
ALLOWED_TOP_LEVEL = frozenset({
    "schema_version", "navigation", "command_forms", "status_labels",
    "board", "artifact_viewers",
})
FORBIDDEN_KEYS = frozenset({
    "script", "scripts", "javascript", "srcdoc", "html", "dangerouslySetInnerHTML",
})


class UIContributionError(ValueError):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


def _scan(value: Any, path: str = "ui") -> int:
    count = 0
    if isinstance(value, dict):
        for key, item in value.items():
            if str(key) in FORBIDDEN_KEYS:
                raise UIContributionError(
                    "extension.ui_script_denied",
                    f"arbitrary script/html field is not allowed: {path}.{key}",
                )
            count += _scan(item, f"{path}.{key}")
    elif isinstance(value, list):
        count += len(value)
        for index, item in enumerate(value):
            count += _scan(item, f"{path}[{index}]")
    return count


def load_ui_contributions(package_dir: str | Path, relative: str) -> dict[str, Any]:
    root = Path(package_dir).resolve()
    path = (root / relative).resolve()
    if root not in path.parents or not path.is_file():
        raise UIContributionError(
            "extension.ui_unreadable", "UI contribution path is unavailable")
    size = path.stat().st_size
    if size > MAX_UI_BYTES:
        raise UIContributionError(
            "extension.ui_too_large",
            f"UI contribution is {size} bytes; maximum is {MAX_UI_BYTES}",
        )
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise UIContributionError(
            "extension.ui_unreadable", f"cannot parse UI contribution: {exc}") from exc
    if not isinstance(data, dict):
        raise UIContributionError(
            "extension.ui_invalid", "UI contribution root must be an object")
    if data.get("schema_version") != UI_SCHEMA_VERSION:
        raise UIContributionError(
            "extension.ui_schema_version",
            f"schema_version must be {UI_SCHEMA_VERSION}",
        )
    unknown = sorted(set(data) - ALLOWED_TOP_LEVEL)
    if unknown:
        raise UIContributionError(
            "extension.ui_component_denied",
            "unsupported UI contribution fields: " + ", ".join(unknown),
        )
    count = _scan(data)
    if count > MAX_COMPONENTS:
        raise UIContributionError(
            "extension.ui_too_many_components",
            f"UI contribution declares {count} items; maximum is {MAX_COMPONENTS}",
        )
    for key in ("navigation", "command_forms", "artifact_viewers"):
        if key in data and not isinstance(data[key], list):
            raise UIContributionError(
                "extension.ui_invalid", f"{key} must be an array")
    return data


__all__ = [
    "ALLOWED_TOP_LEVEL", "MAX_UI_BYTES", "UIContributionError",
    "UI_SCHEMA_VERSION", "load_ui_contributions",
]
