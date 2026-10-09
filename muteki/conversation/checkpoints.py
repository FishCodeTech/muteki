"""Turn-level workspace checkpoints owned by Muteki.

Before a turn runs, the workspace (tracked + untracked, honouring .gitignore)
is written as a git tree and pinned under ``refs/muteki/checkpoints``. Restoring
a checkpoint brings the files inside the workspace scope back to that tree:
changed or deleted files are checked out again and files created since are
removed. The repository index, HEAD and branches are never touched, and
ignored files are outside both snapshots so they are left alone.
"""

from __future__ import annotations

import os
import re
import subprocess
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

from muteki.conversation.git_workspace import GitWorkspaceError, resolve_git_root

REF_PREFIX = "refs/muteki/checkpoints"
_SAFE_ID = re.compile(r"^[A-Za-z0-9_.-]+$")
_GIT_TIMEOUT_S = 120


class CheckpointError(RuntimeError):
    def __init__(self, message: str, *, code: str = "conversation.checkpoint.failed") -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class _Scope:
    top: Path
    git_dir: Path
    pathspec: str


def _git(
    scope_top: Path,
    *args: str,
    env: Optional[dict[str, str]] = None,
    stdin: Optional[str] = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    try:
        completed = subprocess.run(
            ["git", "-C", str(scope_top), *args],
            check=False,
            capture_output=True,
            text=True,
            input=stdin,
            env=env,
            timeout=_GIT_TIMEOUT_S,
        )
    except FileNotFoundError as exc:
        raise CheckpointError("本机未找到 git 可执行文件") from exc
    except subprocess.TimeoutExpired as exc:
        raise CheckpointError(f"git {args[0]} 超时") from exc
    if check and completed.returncode != 0:
        detail = (completed.stderr or completed.stdout or "").strip() or f"exit {completed.returncode}"
        raise CheckpointError(f"git {args[0]} 失败：{detail}")
    return completed


def _scope(root_path: str) -> Optional[_Scope]:
    if not str(root_path or "").strip():
        return None
    try:
        top = resolve_git_root(root_path)
    except GitWorkspaceError as exc:
        raise CheckpointError(str(exc)) from exc
    if top is None:
        return None
    try:
        relative = Path(root_path).expanduser().resolve().relative_to(top)
    except (OSError, ValueError):
        return None
    git_dir = Path(_git(top, "rev-parse", "--absolute-git-dir").stdout.strip())
    pathspec = relative.as_posix() if relative.parts else "."
    return _Scope(top=top, git_dir=git_dir, pathspec=pathspec)


def checkpoint_ref(thread_id: str, turn_id: str) -> str:
    for value in (thread_id, turn_id):
        if not value or not _SAFE_ID.match(value):
            raise CheckpointError(f"检查点标识无效：{value!r}", code="conversation.checkpoint.invalid_id")
    return f"{REF_PREFIX}/{thread_id}/{turn_id}"


def _with_temp_index(scope: _Scope, seed_from_index: bool) -> tuple[Path, dict[str, str]]:
    temp = scope.git_dir / f"muteki-checkpoint-{uuid.uuid4().hex}.index"
    real = scope.git_dir / "index"
    if seed_from_index and real.exists():
        # Seeding with the real index lets `git add` reuse cached stat data.
        temp.write_bytes(real.read_bytes())
    return temp, {**os.environ, "GIT_INDEX_FILE": str(temp)}


def _snapshot_tree(scope: _Scope) -> str:
    temp, env = _with_temp_index(scope, seed_from_index=True)
    try:
        _git(scope.top, "add", "-A", "--", scope.pathspec, env=env)
        return _git(scope.top, "write-tree", env=env).stdout.strip()
    finally:
        temp.unlink(missing_ok=True)


def capture_checkpoint(root_path: str, thread_id: str, turn_id: str) -> Optional[str]:
    """Pin the current workspace state for ``turn_id``. None outside a git repo."""
    scope = _scope(root_path)
    if scope is None:
        return None
    ref = checkpoint_ref(thread_id, turn_id)
    tree = _snapshot_tree(scope)
    _git(scope.top, "update-ref", ref, tree)
    return tree


def _resolve_checkpoint(scope: _Scope, ref: str) -> Optional[str]:
    completed = _git(scope.top, "rev-parse", "--verify", "--quiet", f"{ref}^{{tree}}", check=False)
    tree = completed.stdout.strip()
    return tree if completed.returncode == 0 and tree else None


def _changes(scope: _Scope, checkpoint: str) -> list[dict[str, str]]:
    current = _snapshot_tree(scope)
    output = _git(
        scope.top, "diff", "--no-renames", "--name-status", "-z",
        checkpoint, current, "--", scope.pathspec,
    ).stdout
    parts = [part for part in output.split("\0") if part]
    changes: list[dict[str, str]] = []
    for index in range(0, len(parts) - 1, 2):
        status, path = parts[index], parts[index + 1]
        # Relative to restoring: present now but absent then means delete.
        action = "delete" if status.startswith("A") else "restore"
        changes.append({"path": path, "status": status[:1], "action": action})
    return changes


def checkpoint_status(root_path: str, thread_id: str, turn_id: str) -> dict[str, Any]:
    """What restoring ``turn_id``'s checkpoint would change, for previews."""
    scope = _scope(root_path)
    if scope is None:
        return {"available": False, "reason": "not_git", "changes": []}
    checkpoint = _resolve_checkpoint(scope, checkpoint_ref(thread_id, turn_id))
    if checkpoint is None:
        return {"available": False, "reason": "missing", "changes": []}
    return {"available": True, "reason": "", "changes": _changes(scope, checkpoint)}


def _prune_empty_dirs(path: Path, stop: Path) -> None:
    parent = path.parent
    while parent != stop and stop in parent.parents:
        try:
            parent.rmdir()
        except OSError:
            return
        parent = parent.parent


def restore_checkpoint(root_path: str, thread_id: str, turn_id: str) -> dict[str, list[str]]:
    """Bring workspace files back to ``turn_id``'s checkpoint."""
    scope = _scope(root_path)
    if scope is None:
        raise CheckpointError("工作区不是 Git 仓库，无法回退文件", code="conversation.checkpoint.not_git")
    checkpoint = _resolve_checkpoint(scope, checkpoint_ref(thread_id, turn_id))
    if checkpoint is None:
        raise CheckpointError(
            "这一轮没有文件检查点（早于检查点功能，或开始时未能保存），只能保留文件",
            code="conversation.checkpoint.missing",
        )
    changes = _changes(scope, checkpoint)
    deleted = [item["path"] for item in changes if item["action"] == "delete"]
    restored = [item["path"] for item in changes if item["action"] == "restore"]
    for relative in deleted:
        target = scope.top / relative
        if target.is_symlink() or target.is_file():
            target.unlink()
            _prune_empty_dirs(target, scope.top)
    if restored:
        temp, env = _with_temp_index(scope, seed_from_index=False)
        try:
            _git(scope.top, "read-tree", checkpoint, env=env)
            _git(scope.top, "checkout-index", "-f", "-z", "--stdin", env=env, stdin="\0".join(restored) + "\0")
        finally:
            temp.unlink(missing_ok=True)
    return {"restored": restored, "deleted": deleted}


__all__ = [
    "CheckpointError",
    "capture_checkpoint",
    "checkpoint_ref",
    "checkpoint_status",
    "restore_checkpoint",
]
