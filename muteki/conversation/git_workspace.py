"""项目工作目录的 Git 状态读取、检出与 worktree 生命周期。

给对话 Composer 的目录/分支条用：只允许操作已登记 Project / Workspace
的 root_path，不接受客户端任意路径。检出走 ``git switch``；新建隔离
工作树走 ``git worktree add``。
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path
from typing import Any


class GitWorkspaceError(RuntimeError):
    """Git 操作失败（非仓库、命令失败、分支不存在、worktree 占用等）。"""

    def __init__(self, message: str, *, code: str = "conversation.git.error") -> None:
        super().__init__(message)
        self.code = code


def _noninteractive_git_env() -> dict[str, str]:
    env = dict(os.environ)
    # The service has no operator TTY: credential/askpass prompts would block
    # until the timeout instead of failing with git's real message.
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GCM_INTERACTIVE"] = "never"
    env.setdefault("GIT_EDITOR", "true")
    return env


def _run_git(
    root: Path,
    *args: str,
    check: bool = True,
    timeout: float = 30,
    code: str = "conversation.git.error",
) -> subprocess.CompletedProcess[str]:
    try:
        completed = subprocess.run(
            ["git", "-C", str(root), *args],
            check=False,
            capture_output=True,
            stdin=subprocess.DEVNULL,
            text=True,
            timeout=timeout,
            env=_noninteractive_git_env(),
        )
    except FileNotFoundError as exc:
        raise GitWorkspaceError("本机未找到 git 可执行文件") from exc
    except subprocess.TimeoutExpired as exc:
        raise GitWorkspaceError(
            f"git {args[0] if args else ''} 超时（{int(timeout)} 秒）",
            code="conversation.git.timeout",
        ) from exc
    if check and completed.returncode != 0:
        detail = (completed.stderr or completed.stdout or "").strip() or f"exit {completed.returncode}"
        raise GitWorkspaceError(detail, code=code)
    return completed


def _validate_branch_name(branch: str) -> str:
    name = str(branch or "").strip()
    if (
        not name
        or name.startswith("-")
        or name.endswith(".lock")
        or ".." in name
        or "\\" in name
        or any(ch in name for ch in (" ", "\t", "\n", "~", "^", ":", "?", "*", "["))
    ):
        raise GitWorkspaceError("分支名无效", code="conversation.git.invalid_branch")
    return name


def resolve_git_root(root_path: str) -> Path | None:
    """返回目录所属的 Git 工作树根；不是仓库则返回 None。"""
    path = Path(root_path).expanduser()
    try:
        path = path.resolve()
    except OSError:
        return None
    if not path.is_dir():
        return None
    completed = _run_git(path, "rev-parse", "--show-toplevel", check=False)
    if completed.returncode != 0:
        return None
    top = (completed.stdout or "").strip()
    if not top:
        return None
    try:
        return Path(top).resolve()
    except OSError:
        return None


def resolve_git_common_dir(root_path: str) -> Path | None:
    """返回仓库 common dir（多 worktree 共享的 .git）。"""
    root = resolve_git_root(root_path)
    if root is None:
        return None
    completed = _run_git(root, "rev-parse", "--path-format=absolute", "--git-common-dir", check=False)
    if completed.returncode != 0:
        return None
    common = (completed.stdout or "").strip()
    if not common:
        return None
    try:
        return Path(common).resolve()
    except OSError:
        return None


def inspect_git_workspace(root_path: str) -> dict[str, Any]:
    """读取当前分支与本地分支列表。"""
    root = resolve_git_root(root_path)
    if root is None:
        return {
            "is_repo": False,
            "root_path": str(Path(root_path).expanduser()),
            "current_branch": None,
            "detached": False,
            "detached_head": False,
            "detached_sha": None,
            "branches": [],
            "dirty": False,
            "git_common_dir": None,
        }

    head = _run_git(root, "rev-parse", "--abbrev-ref", "HEAD", check=False)
    current = (head.stdout or "").strip() or None
    detached = current == "HEAD"
    if detached:
        short = _run_git(root, "rev-parse", "--short", "HEAD", check=False)
        current = (short.stdout or "").strip() or None

    listed = _run_git(
        root,
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname:short)",
        "refs/heads",
        check=False,
    )
    branches: list[str] = []
    seen: set[str] = set()
    for line in (listed.stdout or "").splitlines():
        name = line.strip()
        if not name or name in seen:
            continue
        seen.add(name)
        branches.append(name)

    dirty_probe = _run_git(root, "status", "--porcelain", check=False)
    changed_files = [line for line in (dirty_probe.stdout or "").splitlines() if line.strip()]
    dirty = bool(changed_files)
    common = resolve_git_common_dir(str(root))

    return {
        "is_repo": True,
        "root_path": str(root),
        "current_branch": None if detached else current,
        "detached_head": detached,
        "detached_sha": current if detached else None,
        "branches": branches,
        "dirty": dirty,
        "changed_file_count": len(changed_files),
        "git_common_dir": str(common) if common is not None else None,
        **_sync_status(root),
    }


def _sync_status(root: Path) -> dict[str, Any]:
    """HEAD 与上游的同步关系；ahead/behind 基于最近一次 fetch 的远端引用。"""
    remotes_probe = _run_git(root, "remote", check=False)
    remotes = [line.strip() for line in (remotes_probe.stdout or "").splitlines() if line.strip()]
    head_probe = _run_git(root, "log", "-1", "--format=%H%x00%s", check=False)
    head_sha, _, head_subject = (head_probe.stdout or "").strip().partition("\x00")
    upstream_probe = _run_git(
        root, "rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}", check=False,
    )
    upstream = (upstream_probe.stdout or "").strip() if upstream_probe.returncode == 0 else ""
    ahead: int | None = None
    behind: int | None = None
    if upstream:
        counts = _run_git(root, "rev-list", "--left-right", "--count", "@{upstream}...HEAD", check=False)
        parts = (counts.stdout or "").split()
        if counts.returncode == 0 and len(parts) == 2:
            behind, ahead = int(parts[0]), int(parts[1])
    return {
        "remotes": remotes,
        "upstream": upstream or None,
        "ahead": ahead,
        "behind": behind,
        "head_sha": head_sha or None,
        "head_subject": head_subject if head_sha else None,
    }


def _require_repo(root_path: str) -> Path:
    root = resolve_git_root(root_path)
    if root is None:
        raise GitWorkspaceError("当前工作目录不是 Git 仓库", code="conversation.git.not_repo")
    return root


def _require_branch(root: Path) -> str:
    head = _run_git(root, "symbolic-ref", "--quiet", "--short", "HEAD", check=False)
    branch = (head.stdout or "").strip()
    if head.returncode != 0 or not branch:
        raise GitWorkspaceError(
            "当前处于 detached HEAD，请先切换到分支",
            code="conversation.git.detached_head",
        )
    return branch


def _workspace_pathspecs(root: Path, paths: list[str]) -> list[str]:
    specs: list[str] = []
    for raw in paths:
        value = str(raw or "").strip()
        if not value or "\0" in value:
            raise GitWorkspaceError("路径不能为空", code="conversation.git.invalid_path")
        candidate = Path(value)
        absolute = candidate if candidate.is_absolute() else root / candidate
        resolved = Path(os.path.normpath(absolute))
        try:
            relative = resolved.relative_to(root)
        except ValueError as exc:
            raise GitWorkspaceError(
                f"路径不在工作区内：{value}", code="conversation.git.path_outside",
            ) from exc
        rel = relative.as_posix()
        if not rel or rel == ".":
            raise GitWorkspaceError(f"路径无效：{value}", code="conversation.git.invalid_path")
        # Literal pathspec: user file names must not be interpreted as globs or magic.
        specs.append(f":(literal){rel}")
    return specs


def commit_git_changes(
    root_path: str,
    *,
    message: str,
    paths: list[str] | None = None,
    amend: bool = False,
) -> dict[str, Any]:
    """暂存（全部或指定路径）并提交；无可提交内容时抛 nothing_to_commit。"""
    text = str(message or "").strip()
    if not text:
        raise GitWorkspaceError("提交信息不能为空", code="conversation.git.empty_message")
    root = _require_repo(root_path)
    branch = _require_branch(root)
    specs = _workspace_pathspecs(root, paths) if paths else []

    if specs:
        _run_git(root, "add", "-A", "--", *specs, code="conversation.git.stage_failed")
    else:
        _run_git(root, "add", "-A", code="conversation.git.stage_failed")

    if not amend:
        staged = _run_git(
            root, "diff", "--cached", "--quiet", *(["--", *specs] if specs else []), check=False,
        )
        if staged.returncode == 0:
            raise GitWorkspaceError("没有可提交的改动", code="conversation.git.nothing_to_commit")
        if staged.returncode != 1:
            detail = (staged.stderr or "").strip() or f"exit {staged.returncode}"
            raise GitWorkspaceError(detail, code="conversation.git.stage_failed")

    commit_args = ["commit", "--quiet", "-m", text]
    if amend:
        commit_args.insert(1, "--amend")
    if specs:
        # --only commits just these paths even if other changes are already staged.
        commit_args += ["--only", "--", *specs]
    _run_git(root, *commit_args, timeout=120, code="conversation.git.commit_failed")

    head = _run_git(root, "log", "-1", "--format=%H%x00%s")
    sha, _, summary = (head.stdout or "").strip().partition("\x00")
    return {
        "sha": sha,
        "short_sha": sha[:7],
        "summary": summary,
        "branch": branch,
        "amend": amend,
        "status": inspect_git_workspace(str(root)),
    }


def _default_push_remote(root: Path) -> str:
    probe = _run_git(root, "remote", check=False)
    remotes = [line.strip() for line in (probe.stdout or "").splitlines() if line.strip()]
    if not remotes:
        raise GitWorkspaceError("仓库没有配置远端", code="conversation.git.no_remote")
    return "origin" if "origin" in remotes else remotes[0]


def _upstream(root: Path) -> str:
    probe = _run_git(
        root, "rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}", check=False,
    )
    return (probe.stdout or "").strip() if probe.returncode == 0 else ""


def _output_text(completed: subprocess.CompletedProcess[str]) -> str:
    return "\n".join(
        part.strip() for part in (completed.stdout or "", completed.stderr or "") if part.strip()
    )


def push_git_branch(
    root_path: str,
    *,
    set_upstream: bool | None = None,
    timeout: float = 120,
) -> dict[str, Any]:
    """推送当前分支；无上游时（或 set_upstream=True）推到默认远端并设置上游。"""
    root = _require_repo(root_path)
    branch = _validate_branch_name(_require_branch(root))
    upstream = _upstream(root)
    if not upstream and set_upstream is False:
        raise GitWorkspaceError(
            f"分支 {branch} 没有上游，需要设置上游后推送",
            code="conversation.git.no_upstream",
        )
    use_upstream = bool(set_upstream) or not upstream
    if use_upstream:
        remote = upstream.split("/", 1)[0] if upstream and set_upstream else _default_push_remote(root)
        args = ["push", "--porcelain", "--set-upstream", remote, f"refs/heads/{branch}:refs/heads/{branch}"]
    else:
        remote = upstream.split("/", 1)[0]
        args = ["push", "--porcelain"]
    completed = _run_git(root, *args, check=False, timeout=timeout)
    output = _output_text(completed)
    if completed.returncode != 0:
        raise GitWorkspaceError(output or f"git push exit {completed.returncode}", code="conversation.git.push_failed")
    return {
        "branch": branch,
        "remote": remote,
        "set_upstream": use_upstream,
        "output": output,
        "status": inspect_git_workspace(str(root)),
    }


def _operation_in_progress(root: Path) -> str:
    for marker, label in (("rebase-merge", "rebase"), ("rebase-apply", "rebase"), ("MERGE_HEAD", "merge")):
        probe = _run_git(root, "rev-parse", "--git-path", marker, check=False)
        location = (probe.stdout or "").strip()
        if probe.returncode == 0 and location and (root / location).exists():
            return label
    return ""


def pull_git_branch(
    root_path: str,
    *,
    rebase: bool = False,
    timeout: float = 120,
) -> dict[str, Any]:
    """拉取上游；默认 --ff-only，rebase=True 时使用 --rebase。"""
    root = _require_repo(root_path)
    branch = _require_branch(root)
    upstream = _upstream(root)
    if not upstream:
        raise GitWorkspaceError(
            f"分支 {branch} 没有上游，无法拉取",
            code="conversation.git.no_upstream",
        )
    mode = "--rebase" if rebase else "--ff-only"
    completed = _run_git(root, "pull", mode, check=False, timeout=timeout)
    output = _output_text(completed)
    if completed.returncode != 0:
        in_progress = _operation_in_progress(root)
        if in_progress:
            raise GitWorkspaceError(
                output or f"git pull exit {completed.returncode}",
                code="conversation.git.pull_conflict",
            )
        raise GitWorkspaceError(output or f"git pull exit {completed.returncode}", code="conversation.git.pull_failed")
    return {
        "branch": branch,
        "upstream": upstream,
        "rebase": rebase,
        "output": output,
        "status": inspect_git_workspace(str(root)),
    }


def upstream_remote_branch(root_path: str) -> dict[str, str]:
    """当前分支的上游远端名与远端分支名（未设置上游时为空）。"""
    root = _require_repo(root_path)
    branch = _require_branch(root)
    upstream = _upstream(root)
    if not upstream:
        return {"branch": branch, "remote": "", "remote_branch": ""}
    remote_probe = _run_git(root, "config", "--get", f"branch.{branch}.remote", check=False)
    merge_probe = _run_git(root, "config", "--get", f"branch.{branch}.merge", check=False)
    remote = (remote_probe.stdout or "").strip()
    merge = (merge_probe.stdout or "").strip()
    remote_branch = merge[len("refs/heads/"):] if merge.startswith("refs/heads/") else merge
    return {"branch": branch, "remote": remote, "remote_branch": remote_branch}


def checkout_git_branch(
    root_path: str,
    *,
    branch: str,
    create: bool = False,
) -> dict[str, Any]:
    """切换到已有分支，或创建并切换到新分支。"""
    name = _validate_branch_name(branch)
    root = resolve_git_root(root_path)
    if root is None:
        raise GitWorkspaceError("当前工作目录不是 Git 仓库")

    if create:
        _run_git(root, "switch", "-c", name)
    else:
        exists = _run_git(root, "show-ref", "--verify", "--quiet", f"refs/heads/{name}", check=False)
        if exists.returncode != 0:
            raise GitWorkspaceError(f"本地分支不存在：{name}")
        _run_git(root, "switch", name)

    return inspect_git_workspace(str(root))


def list_worktrees(root_path: str) -> list[dict[str, Any]]:
    """列出仓库下的全部 worktree（含主检出）。"""
    root = resolve_git_root(root_path)
    if root is None:
        raise GitWorkspaceError("当前工作目录不是 Git 仓库")
    completed = _run_git(root, "worktree", "list", "--porcelain")
    entries: list[dict[str, Any]] = []
    current: dict[str, Any] = {}
    for raw in (completed.stdout or "").splitlines():
        line = raw.rstrip("\n")
        if not line:
            if current.get("path"):
                entries.append(current)
            current = {}
            continue
        if line.startswith("worktree "):
            path = line[len("worktree "):].strip()
            try:
                path = str(Path(path).resolve())
            except OSError:
                pass
            current = {
                "path": path,
                "branch": None,
                "detached": False,
                "bare": False,
                "locked": False,
                "prunable": False,
            }
        elif line.startswith("HEAD "):
            current["head"] = line[len("HEAD "):].strip()
        elif line.startswith("branch "):
            ref = line[len("branch "):].strip()
            current["branch"] = ref.removeprefix("refs/heads/") if ref.startswith("refs/heads/") else ref
        elif line == "detached":
            current["detached"] = True
        elif line == "bare":
            current["bare"] = True
        elif line.startswith("locked"):
            current["locked"] = True
        elif line == "prunable":
            current["prunable"] = True
    if current.get("path"):
        entries.append(current)
    return entries


def add_worktree(
    parent_root: str,
    worktree_path: str,
    *,
    branch: str,
    base_ref: str = "HEAD",
    create_branch: bool = True,
) -> dict[str, Any]:
    """在 ``parent_root`` 仓库下新建 worktree。

    失败时尽量清理半成品目录，避免留下孤儿路径。
    """
    name = _validate_branch_name(branch)
    base = str(base_ref or "HEAD").strip() or "HEAD"
    if base.startswith("-") or ".." in base or any(ch in base for ch in (" ", "\t", "\n", "\\")):
        raise GitWorkspaceError("基线 ref 无效", code="conversation.git.invalid_base_ref")

    parent = resolve_git_root(parent_root)
    if parent is None:
        raise GitWorkspaceError("父目录不是 Git 仓库")

    target = Path(worktree_path).expanduser()
    try:
        target = target.resolve()
    except OSError as exc:
        raise GitWorkspaceError(f"worktree 路径无效：{worktree_path!r}") from exc

    if target.exists():
        raise GitWorkspaceError(
            f"worktree 路径已存在：{target}",
            code="conversation.git.worktree_path_exists",
        )

    target.parent.mkdir(parents=True, exist_ok=True)
    args: list[str] = ["worktree", "add"]
    if create_branch:
        args.extend(["-b", name, str(target), base])
    else:
        args.extend([str(target), name])

    try:
        _run_git(parent, *args)
    except GitWorkspaceError:
        if target.exists():
            shutil.rmtree(target, ignore_errors=True)
            # 若 git 已登记半成品，尝试 prune
            _run_git(parent, "worktree", "prune", check=False)
        raise

    status = inspect_git_workspace(str(target))
    status["branch"] = name
    status["base_ref"] = base
    status["parent_root"] = str(parent)
    return status


def remove_worktree(
    worktree_path: str,
    *,
    force: bool = False,
    allow_dirty: bool = False,
) -> dict[str, Any]:
    """删除一个 worktree。默认拒绝未提交改动；``force`` 才可强删。"""
    root = resolve_git_root(worktree_path)
    if root is None:
        raise GitWorkspaceError("路径不是 Git worktree", code="conversation.git.not_a_worktree")

    status = inspect_git_workspace(str(root))
    if status.get("dirty") and not (force or allow_dirty):
        raise GitWorkspaceError(
            "工作区有未提交改动，拒绝删除 worktree",
            code="conversation.git.worktree_dirty",
        )

    common = resolve_git_common_dir(str(root))
    if common is None:
        raise GitWorkspaceError("无法解析 git common dir")
    # remove 必须从任一同仓库路径发起；用 common 的上级主检出更稳妥
    parent = resolve_git_root(str(common.parent)) or root
    args = ["worktree", "remove", str(root)]
    if force:
        args.append("--force")
    try:
        _run_git(parent, *args)
    except GitWorkspaceError:
        # 主检出不能 remove；或路径已不在 list —— 给出明确错误
        raise
    _run_git(parent, "worktree", "prune", check=False)
    return {"removed": True, "root_path": str(root), "dirty": bool(status.get("dirty"))}


def same_git_repo(left: str, right: str) -> bool:
    """判断两个路径是否属于同一 Git 仓库（共享 common dir）。"""
    a = resolve_git_common_dir(left)
    b = resolve_git_common_dir(right)
    return a is not None and b is not None and a == b


import re as _re


def detect_github_remote(root_path: str, remote: str = "origin") -> dict[str, Any]:
    """C15: Detect GitHub remote and parse owner/repo from the workspace.

    Returns a dict with keys:
      - remote_url: raw remote URL (may be empty)
      - owner: GitHub owner (empty if not GitHub)
      - repo: GitHub repo name without .git (empty if not GitHub)
      - github: True when owner/repo were successfully parsed

    Raises ``GitWorkspaceError`` when:
      - the directory has no git repo
      - git command fails
    Does NOT raise for "no remote" or "not GitHub" — those are returned as state.
    """
    root = Path(root_path).expanduser()
    git_root = resolve_git_root(root_path)
    if git_root is None:
        raise GitWorkspaceError("当前目录不是 Git 仓库", code="conversation.git.not_repo")

    result = _run_git(root, "remote", "get-url", "--", remote, check=False)
    if result.returncode != 0:
        return {"remote_url": "", "owner": "", "repo": "", "github": False}

    remote_url = result.stdout.strip()
    # Parse SSH: git@github.com:owner/repo.git
    ssh_match = _re.match(r"git@github\.com:([^/]+)/(.+?)(?:\.git)?$", remote_url)
    # Parse HTTPS: https://github.com/owner/repo[.git]
    https_match = _re.match(r"https?://(?:[^@]+@)?github\.com/([^/]+)/(.+?)(?:\.git)?$", remote_url)
    match = ssh_match or https_match
    if match:
        return {
            "remote_url": remote_url,
            "owner": match.group(1),
            "repo": match.group(2),
            "github": True,
        }
    return {"remote_url": remote_url, "owner": "", "repo": "", "github": False}
