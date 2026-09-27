"""C23 approval request queue helpers.

ThreadState keeps ``pending_approvals`` keyed by ``approval_id`` and a
compat singleton ``pending_approval`` (oldest actionable entry) for
list badges / inbox deep-links (#29).
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any, Mapping, Optional  # noqa: F401 — Optional used below


def _as_str(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, str):
        return value.strip()
    if isinstance(value, (list, tuple)):
        parts = [str(item).strip() for item in value if str(item).strip()]
        return " ".join(parts)
    return str(value).strip()


def _first_str(payload: Mapping[str, Any], *keys: str) -> str:
    for key in keys:
        if key not in payload:
            continue
        text = _as_str(payload.get(key))
        if text:
            return text
    return ""


def _infer_kind(payload: Mapping[str, Any]) -> str:
    raw = _first_str(
        payload,
        "approval_kind",
        "kind",
        "type",
        "permission_kind",
    ).lower().replace("-", "_")
    aliases = {
        "command": "command_execution",
        "commandexecution": "command_execution",
        "command_execution": "command_execution",
        "shell": "command_execution",
        "exec": "command_execution",
        "file_change": "file_change",
        "filechange": "file_change",
        "apply_patch": "file_change",
        "applypatch": "file_change",
        "patch": "file_change",
        "permissions": "permissions",
        "permission": "permissions",
        "mcp_tool_call": "mcp_tool_call",
        "mcp": "mcp_tool_call",
        "tool": "tool",
    }
    if raw in aliases:
        return aliases[raw]
    if _first_str(payload, "diff", "patch", "unified_diff"):
        return "file_change"
    if _first_str(payload, "command", "cmd"):
        return "command_execution"
    return raw or "unknown"


def _files_from_mapping(raw: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Normalize v1 applyPatchApproval ``file_changes`` map → files list."""
    out: list[dict[str, Any]] = []
    for path_key, change in raw.items():
        path = _as_str(path_key)
        if not path:
            continue
        row: dict[str, Any] = {"path": path}
        if isinstance(change, Mapping):
            status = _first_str(change, "type", "kind", "status", "change_type")
            if status:
                row["status"] = status
            diff = _first_str(
                change,
                "diff",
                "patch",
                "unified_diff",
                "unifiedDiff",
                "content",
            )
            if diff:
                row["diff"] = diff
            move_to = _first_str(change, "move_path", "movePath")
            if move_to:
                row["move_path"] = move_to
        else:
            diff = _as_str(change)
            if diff:
                row["diff"] = diff
        out.append(row)
    return out


def _coerce_files_list(raw: Any) -> list[dict[str, Any]]:
    if isinstance(raw, list):
        out: list[dict[str, Any]] = []
        for item in raw:
            if not isinstance(item, Mapping):
                continue
            path = _first_str(item, "path", "filename", "file", "file_path")
            if not path:
                continue
            row: dict[str, Any] = {"path": path}
            status = _first_str(item, "status", "change_type", "kind", "type")
            if status:
                row["status"] = status
            for key in ("additions", "deletions"):
                if key in item:
                    row[key] = item[key]
            diff = _first_str(
                item,
                "diff",
                "patch",
                "unified_diff",
                "unifiedDiff",
                "content",
            )
            if diff:
                row["diff"] = diff
            out.append(row)
        return out
    if isinstance(raw, Mapping):
        return _files_from_mapping(raw)
    return []


def _diff_from_files(files: list[Mapping[str, Any]]) -> str:
    chunks: list[str] = []
    for item in files:
        chunk = _first_str(item, "diff", "patch", "unified_diff", "unifiedDiff", "content")
        if not chunk:
            continue
        path = _first_str(item, "path", "filename", "file")
        if path and not chunk.lstrip().startswith(("diff ", "--- ")):
            chunks.append(f"--- a/{path}\n+++ b/{path}\n{chunk}")
        else:
            chunks.append(chunk)
    return "\n".join(chunks)


def _extract_diff(payload: Mapping[str, Any]) -> str:
    direct = _first_str(payload, "diff", "patch", "unified_diff", "unifiedDiff")
    if direct:
        return direct
    native = payload.get("native")
    if isinstance(native, Mapping):
        nested = _first_str(
            native,
            "diff",
            "patch",
            "unified_diff",
            "unifiedDiff",
            "fileDiff",
            "file_diff",
        )
        if nested:
            return nested
        files = _coerce_files_list(
            native.get("files")
            or native.get("changes")
            or native.get("file_changes")
            or native.get("fileChanges")
        )
        composed = _diff_from_files(files)
        if composed:
            return composed
    files = _coerce_files_list(
        payload.get("files")
        or payload.get("changes")
        or payload.get("file_changes")
        or payload.get("fileChanges")
    )
    return _diff_from_files(files)


def _extract_files(payload: Mapping[str, Any]) -> list[dict[str, Any]]:
    files = _coerce_files_list(
        payload.get("files")
        or payload.get("changes")
        or payload.get("file_changes")
        or payload.get("fileChanges")
    )
    if files:
        return files
    native = payload.get("native")
    if isinstance(native, Mapping):
        return _coerce_files_list(
            native.get("files")
            or native.get("changes")
            or native.get("file_changes")
            or native.get("fileChanges")
        )
    return []


def normalize_approval_payload(
    payload: Mapping[str, Any],
    *,
    default_status: str = "pending",
) -> dict[str, Any]:
    """Normalize adapter/native approval payloads for the queue + UI."""
    data = dict(payload)
    approval_id = _first_str(data, "approval_id", "request_id", "id")
    if not approval_id:
        raise ValueError("approval request requires approval_id")

    kind = _infer_kind(data)
    command = _first_str(data, "command", "cmd")
    if not command:
        native = data.get("native")
        if isinstance(native, Mapping):
            command = _first_str(native, "command", "cmd")
            if not command:
                cmd = native.get("command")
                command = _as_str(cmd)
    cwd = _first_str(data, "cwd", "working_directory", "workspace", "workdir")
    if not cwd:
        native = data.get("native")
        if isinstance(native, Mapping):
            cwd = _first_str(
                native,
                "cwd",
                "working_directory",
                "workdir",
                "grantRoot",
                "grant_root",
            )
    if not cwd:
        cwd = _first_str(data, "grantRoot", "grant_root")

    diff = _extract_diff(data)
    files = _extract_files(data)
    path = _first_str(data, "path", "file", "filename", "file_path")
    if not path and len(files) == 1:
        path = str(files[0].get("path") or "")
    paths = [
        str(row.get("path") or "")
        for row in files
        if isinstance(row, Mapping) and row.get("path")
    ]
    if not paths:
        raw_paths = data.get("paths")
        if isinstance(raw_paths, list):
            paths = [str(item).strip() for item in raw_paths if str(item).strip()]
    title = _first_str(
        data,
        "title",
        "action",
        "tool_name",
        "tool",
        "name",
        "requester",
    )
    if not title:
        if kind == "command_execution" and command:
            title = "命令执行"
        elif kind == "file_change":
            title = "文件变更"
        elif kind == "mcp_tool_call":
            title = "MCP 工具调用"
        else:
            title = "Runtime 操作"

    status = _first_str(data, "status") or default_status
    if status not in {"pending", "expired", "resolving"}:
        status = default_status

    requested_at = _first_str(data, "requested_at")
    if not requested_at:
        requested_at = datetime.now(timezone.utc).isoformat()

    normalized: dict[str, Any] = {
        **data,
        "approval_id": approval_id,
        "approval_kind": kind,
        "status": status,
        "title": title,
        "requested_at": requested_at,
    }
    if command:
        normalized["command"] = command
    if cwd:
        normalized["cwd"] = cwd
    if path:
        normalized["path"] = path
    if paths:
        normalized["paths"] = paths
    if diff:
        normalized["diff"] = diff
    if files:
        normalized["files"] = files
    expires_at = _first_str(data, "expires_at", "expires")
    if expires_at:
        normalized["expires_at"] = expires_at
    reason = _first_str(data, "reason", "message", "detail")
    if kind == "file_change" and not diff and not files and not path:
        # Explicit missing marker so UI can show "预览暂未取得" instead of blank auth.
        missing = "文件变更预览暂未取得"
        if reason and missing not in reason:
            reason = f"{reason}（{missing}）"
        elif not reason:
            reason = missing
    if reason:
        normalized["reason"] = reason
    return normalized


def hydrate_approvals(
    pending_approvals: Optional[Mapping[str, Any]],
    pending_approval: Optional[Mapping[str, Any]],
) -> dict[str, dict[str, Any]]:
    """Build a queue map from persisted state (migrates singleton → map)."""
    out: dict[str, dict[str, Any]] = {}
    if isinstance(pending_approvals, Mapping):
        for key, value in pending_approvals.items():
            if not isinstance(value, Mapping):
                continue
            try:
                row = normalize_approval_payload(value)
            except ValueError:
                continue
            out[str(row["approval_id"])] = row
    if not out and isinstance(pending_approval, Mapping) and pending_approval:
        try:
            row = normalize_approval_payload(pending_approval)
            out[str(row["approval_id"])] = row
        except ValueError:
            pass
    return out


def is_actionable_approval(row: Optional[Mapping[str, Any]]) -> bool:
    """True when an approval entry can still be decided."""
    if not isinstance(row, Mapping) or not row:
        return False
    return str(row.get("status") or "pending") == "pending"


def has_actionable_approvals(
    pending_approvals: Optional[Mapping[str, Any]] = None,
    pending_approval: Optional[Mapping[str, Any]] = None,
) -> bool:
    """True if the queue/singleton still has a pending (non-expired) approval.

    Expired rows may remain in ``pending_approvals`` for explainability, but they
    must not suppress stream-end failures or keep inbox/attention actionable.
    """
    if isinstance(pending_approvals, Mapping):
        for value in pending_approvals.values():
            if is_actionable_approval(value if isinstance(value, Mapping) else None):
                return True
    return is_actionable_approval(
        pending_approval if isinstance(pending_approval, Mapping) else None
    )


def primary_approval(
    pending_approvals: Mapping[str, Mapping[str, Any]],
) -> Optional[dict[str, Any]]:
    """Oldest *pending* entry for compat singleton / actionable deep-links.

    Expired entries stay in the queue map for UI explainability, but they are
    not selected as ``pending_approval`` so badges and resolve binding stay
    pending-only.
    """
    pending_rows = [
        dict(row) for row in pending_approvals.values()
        if str(row.get("status") or "pending") == "pending"
    ]
    if not pending_rows:
        return None

    def sort_key(row: Mapping[str, Any]) -> tuple[str, str]:
        return (
            str(row.get("requested_at") or ""),
            str(row.get("approval_id") or ""),
        )

    pending_rows.sort(key=sort_key)
    return dict(pending_rows[0])


def upsert_approval(
    pending_approvals: Mapping[str, Mapping[str, Any]],
    payload: Mapping[str, Any],
) -> tuple[dict[str, dict[str, Any]], Optional[dict[str, Any]]]:
    queue = {str(k): dict(v) for k, v in pending_approvals.items()}
    row = normalize_approval_payload(payload, default_status="pending")
    approval_id = str(row["approval_id"])
    existing = queue.get(approval_id)
    if existing and existing == row:
        return queue, primary_approval(queue)
    queue[approval_id] = row
    return queue, primary_approval(queue)


def remove_approval(
    pending_approvals: Mapping[str, Mapping[str, Any]],
    approval_id: str,
) -> tuple[dict[str, dict[str, Any]], Optional[dict[str, Any]]]:
    queue = {str(k): dict(v) for k, v in pending_approvals.items()}
    queue.pop(str(approval_id or "").strip(), None)
    return queue, primary_approval(queue)


def expire_approvals(
    pending_approvals: Mapping[str, Mapping[str, Any]],
    *,
    reason: str,
) -> tuple[dict[str, dict[str, Any]], Optional[dict[str, Any]]]:
    queue: dict[str, dict[str, Any]] = {}
    for key, value in pending_approvals.items():
        row = dict(value)
        if str(row.get("status") or "pending") == "pending":
            row["status"] = "expired"
            row["reason"] = reason or row.get("reason") or "请求已过期"
        queue[str(key)] = row
    return queue, primary_approval(queue)


def clear_approvals() -> tuple[dict[str, dict[str, Any]], None]:
    return {}, None


def lookup_approval(
    pending_approvals: Mapping[str, Mapping[str, Any]],
    approval_id: str,
) -> Optional[dict[str, Any]]:
    row = pending_approvals.get(str(approval_id or "").strip())
    return dict(row) if isinstance(row, Mapping) else None


__all__ = [
    "clear_approvals",
    "expire_approvals",
    "has_actionable_approvals",
    "hydrate_approvals",
    "is_actionable_approval",
    "lookup_approval",
    "normalize_approval_payload",
    "primary_approval",
    "remove_approval",
    "upsert_approval",
]
