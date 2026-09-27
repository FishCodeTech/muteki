"""C11 — impact preview for Retry / Edit-resend / Fork / native rewind.

Pure helpers over current turns/messages/artifacts + workspace dirty probe.
Never mutates conversation or filesystem state.
"""

from __future__ import annotations

from typing import Any, Literal, Optional

from muteki.conversation.models import TURN_SUPERSEDED, TurnRecord
from muteki.conversation.workspace_surfaces import read_workspace_diff

ImpactMode = Literal["retry", "edit_resend", "fork", "native_rewind"]

_EXTERNAL_SIDE_EFFECTS = "cannot_undo"

_MODE_FILE_POLICY: dict[ImpactMode, str] = {
    "retry": "keep_files",
    "edit_resend": "keep_files",
    "fork": "fork_shares_workspace",
    "native_rewind": "keep_files",  # sync_files only when capability verified
}


def _attachment_names(
    turns: list[TurnRecord],
    artifact_rows: dict[str, dict[str, Any]],
) -> list[dict[str, str]]:
    seen: set[str] = set()
    rows: list[dict[str, str]] = []
    for turn in turns:
        for digest in turn.attachments:
            key = str(digest or "").strip()
            if not key or key in seen:
                continue
            seen.add(key)
            meta = artifact_rows.get(key) or {}
            rows.append({
                "sha256": key,
                "name": str(meta.get("name") or key[:12]),
            })
    return rows


def _workspace_impact(
    *,
    root_path: str,
    policy: str,
    file_mode: str = "keep_files",
) -> dict[str, Any]:
    dirty = False
    files: list[str] | str = []
    if root_path:
        try:
            diff = read_workspace_diff(root_path)
            dirty = bool(diff.get("dirty"))
            file_rows = diff.get("files") or []
            names = [
                str(item.get("path") or item.get("name") or "").strip()
                for item in file_rows
                if isinstance(item, dict)
            ]
            files = [name for name in names if name] or (
                "unknown" if dirty else []
            )
        except Exception:
            dirty = False
            files = "unknown"
    else:
        files = "unknown"
    effective = file_mode if policy == "native_rewind" else policy
    return {
        "policy": effective,
        "dirty": dirty,
        "files_touched_since_target": files,
        "root_path": root_path or "",
        "shares_workspace": policy == "fork_shares_workspace",
    }


def resolve_rewind_capability(
    runtime_connection: Optional[dict[str, Any]] = None,
) -> dict[str, Any]:
    """Read #16 matrix rewind row when present; otherwise unknown."""
    connection = dict(runtime_connection or {})
    matrix = connection.get("matrix")
    if isinstance(matrix, dict):
        rows = matrix.get("rows") or []
        for row in rows:
            if not isinstance(row, dict):
                continue
            if str(row.get("key") or "") != "rewind":
                continue
            level = str(row.get("level") or "unknown")
            return {
                "rewind_level": level,
                "invocable": bool(row.get("invocable")) and level == "supported"
                and not bool(matrix.get("stale")),
                "reason": str(row.get("reason") or ""),
                "alternative": str(
                    row.get("alternative")
                    or "可改用 Fork（保留源历史）或 Retry（保留文件）"
                ),
                "source": "matrix",
                "revision": int(matrix.get("revision") or 0),
                "stale": bool(matrix.get("stale")),
            }
    # Soft fallback before #16 lands: never claim supported.
    return {
        "rewind_level": "unknown",
        "invocable": False,
        "reason": "原生回退 / rewind 尚未确认",
        "alternative": "可改用 Fork（保留源历史）或 Retry（保留文件，仅重建对话文本）",
        "source": "static",
        "revision": 0,
        "stale": False,
    }


def build_impact_preview(
    *,
    mode: ImpactMode,
    target: TurnRecord,
    current_turns: list[TurnRecord],
    artifact_rows: Optional[dict[str, dict[str, Any]]] = None,
    workspace_root: str = "",
    file_mode: str = "keep_files",
    runtime_connection: Optional[dict[str, Any]] = None,
    edited_text: str = "",
) -> dict[str, Any]:
    """Compute ImpactPreview for UI confirm + dry-run API."""
    if mode not in _MODE_FILE_POLICY:
        raise ValueError(f"unknown impact mode: {mode}")

    if mode == "fork":
        superseded: list[TurnRecord] = []
    else:
        superseded = [turn for turn in current_turns if turn.seq >= target.seq]

    policy = _MODE_FILE_POLICY[mode]
    if mode == "native_rewind" and file_mode == "sync_files":
        policy_label = "sync_files"
    else:
        policy_label = policy

    rewind = resolve_rewind_capability(runtime_connection)
    alternative = rewind.get("alternative") or ""
    if mode == "native_rewind" and not rewind.get("invocable"):
        alternative = alternative or "可改用 Fork 或 Retry（保留文件）"

    return {
        "mode": mode,
        "target_turn_id": target.turn_id,
        "target_seq": target.seq,
        "target_text_preview": (edited_text or target.text)[:240],
        "edited": bool(edited_text.strip()) and edited_text != target.text,
        "superseded_turn_ids": [turn.turn_id for turn in superseded],
        "superseded_seqs": [turn.seq for turn in superseded],
        "superseded_message_count": len(superseded),
        "attachments_affected": _attachment_names(
            superseded, dict(artifact_rows or {}),
        ),
        "workspace": _workspace_impact(
            root_path=workspace_root,
            policy=policy_label if mode != "fork" else "fork_shares_workspace",
            file_mode=file_mode,
        ),
        "external_side_effects": _EXTERNAL_SIDE_EFFECTS,
        "provider": {
            "rewind_level": rewind.get("rewind_level"),
            "reason": rewind.get("reason"),
            "alternative": alternative,
            "invocable": bool(rewind.get("invocable")),
            "source": rewind.get("source"),
        },
        "guarantees": {
            "conversation": (
                "source_unchanged" if mode == "fork"
                else "supersede_target_and_later"
            ),
            "files": policy_label,
            "provider_session": (
                "native_rewind" if mode == "native_rewind"
                else "muteki_text_reconstruct" if mode in {
                    "retry", "edit_resend",
                }
                else "new_thread_session"
            ),
            "honest_label": (
                "Muteki 仅重建对话文本历史，不等于 Provider 原生 rewind，"
                "也不等于文件回滚"
                if mode in {"retry", "edit_resend"}
                else "Fork 保留源 Thread；当前与源共享工作区"
                if mode == "fork"
                else "仅在 Provider 确认支持时执行原生 rewind"
            ),
        },
    }


def filter_non_superseded(turns: list[TurnRecord]) -> list[TurnRecord]:
    return [turn for turn in turns if turn.status != TURN_SUPERSEDED]


__all__ = [
    "ImpactMode",
    "build_impact_preview",
    "filter_non_superseded",
    "resolve_rewind_capability",
]
