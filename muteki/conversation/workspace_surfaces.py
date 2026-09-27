"""会话右侧工作面板使用的真实工作区读取能力。"""

from __future__ import annotations

import os
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import quote

from muteki.conversation.git_workspace import _run_git, inspect_git_workspace, resolve_git_root


class WorkspaceSurfaceError(RuntimeError):
    pass


_IGNORED_DIRS = {".git", ".next", "node_modules", "__pycache__", ".venv", "venv"}
_MAX_FILE_BYTES = 512_000
_MAX_OUTPUT_BYTES = 256_000
_DIFF_HEADER_RE = re.compile(r"^diff --git a/(.*?) b/(.*?)$")
_RENAME_FROM_RE = re.compile(r"^rename from (.+)$")
_RENAME_TO_RE = re.compile(r"^rename to (.+)$")
_BINARY_RE = re.compile(r"^Binary files .+ differ$")
_VALID_STAGING = {"staged", "unstaged", "untracked"}

_IMAGE_EXTS = {".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".ico", ".svg"}
_PDF_EXTS = {".pdf"}
_HTML_EXTS = {".html", ".htm"}
_MARKDOWN_EXTS = {".md", ".markdown", ".mdx"}
_CODE_EXTS = {
    ".py", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".json", ".css",
    ".rs", ".go", ".java", ".kt", ".c", ".h", ".cpp", ".hpp", ".cs", ".rb",
    ".php", ".swift", ".sh", ".bash", ".zsh", ".yml", ".yaml", ".toml",
    ".xml", ".sql", ".r", ".lua", ".vue", ".svelte", ".txt", ".log", ".diff",
    ".patch", ".ini", ".cfg", ".env", ".gitignore", ".dockerfile",
}
_MEDIA_BY_EXT = {
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".gif": "image/gif",
    ".webp": "image/webp",
    ".bmp": "image/bmp",
    ".ico": "image/x-icon",
    ".svg": "image/svg+xml",
    ".pdf": "application/pdf",
    ".html": "text/html",
    ".htm": "text/html",
    ".md": "text/markdown",
    ".markdown": "text/markdown",
    ".mdx": "text/markdown",
    ".json": "application/json",
    ".css": "text/css",
    ".js": "text/javascript",
    ".mjs": "text/javascript",
    ".ts": "text/typescript",
    ".tsx": "text/typescript",
    ".py": "text/x-python",
    ".txt": "text/plain",
    ".log": "text/plain",
    ".yml": "text/yaml",
    ".yaml": "text/yaml",
}


def guess_media_type(path: Path) -> str:
    ext = path.suffix.lower()
    if ext in _MEDIA_BY_EXT:
        return _MEDIA_BY_EXT[ext]
    return "application/octet-stream"


# Navigable document types that must never be served same-origin as inline HTML/SVG.
_UNSAFE_INLINE_MEDIA_PREFIXES = (
    "text/html",
    "application/xhtml+xml",
    "image/svg+xml",
    "text/xml",
    "application/xml",
)


def is_unsafe_inline_media_type(media_type: str | None) -> bool:
    """True when browsers may execute markup if the response is opened inline."""
    mime = str(media_type or "").split(";", 1)[0].strip().lower()
    if not mime:
        return False
    return any(mime == prefix or mime.startswith(prefix + "+") for prefix in _UNSAFE_INLINE_MEDIA_PREFIXES)


def _sanitize_download_filename(filename: str) -> str:
    """Basename-only name safe for Content-Disposition parameters."""
    raw = str(filename or "download").replace("\\", "/").split("/")[-1]
    cleaned = (
        raw.replace('"', "_")
        .replace("\n", "_")
        .replace("\r", "_")
        .replace("\x00", "_")
    )
    # Strip remaining C0 controls.
    cleaned = "".join(ch if ord(ch) >= 0x20 else "_" for ch in cleaned).strip()
    return cleaned or "download"


def ascii_filename_fallback(filename: str) -> str:
    """Latin-1-safe ASCII ``filename=`` token (RFC 6266 fallback)."""
    name = _sanitize_download_filename(filename)
    ascii_chars: list[str] = []
    for ch in name:
        o = ord(ch)
        if 0x20 <= o <= 0x7E and ch not in "<>:\"|?*\\":
            ascii_chars.append(ch)
        else:
            ascii_chars.append("_")
    ascii_name = "".join(ascii_chars).strip(" ._") or "download"
    # Keep a usable ASCII extension when the original had one.
    suffix = Path(name).suffix
    if (
        suffix
        and all(0x20 <= ord(c) <= 0x7E for c in suffix)
        and not ascii_name.lower().endswith(suffix.lower())
    ):
        ascii_name = f"{ascii_name}{suffix}"
    return ascii_name[:180] or "download"


def content_disposition_header(
    filename: str,
    *,
    disposition: str = "inline",
) -> str:
    """RFC 6266 / 5987 Content-Disposition (ASCII filename + UTF-8 filename*).

    Starlette encodes response headers as latin-1; putting raw Unicode into
    ``filename="…"`` raises UnicodeEncodeError and surfaces as HTTP 500 (#217).
    """
    kind = "attachment" if str(disposition or "").strip().lower() == "attachment" else "inline"
    utf8_name = _sanitize_download_filename(filename)
    ascii_name = ascii_filename_fallback(utf8_name)
    # Quoted-string ASCII token is always latin-1 safe.
    header = f'{kind}; filename="{ascii_name}"'
    if utf8_name != ascii_name or any(ord(ch) > 0x7E for ch in utf8_name):
        # RFC 5987: filename*=UTF-8''percent-encoded-octets
        encoded = quote(utf8_name, safe="")
        header = f"{header}; filename*=UTF-8''{encoded}"
    return header


def safe_raw_content_headers(
    *,
    media_type: str,
    filename: str,
    download: bool = False,
) -> tuple[str, dict[str, str]]:
    """Return (media_type, headers) that cannot XSS the Muteki origin.

    HTML/SVG/XML are always forced to ``attachment`` + ``application/octet-stream``
    so a same-origin ``/workspace/file/raw`` navigation cannot run scripts with
    session cookies. Typed preview still uses JSON ``content`` + ``srcDoc`` sandbox.

    Filenames use ASCII ``filename`` + RFC 5987 ``filename*`` so Unicode names
    (Chinese / emoji / long-paste auto titles) never break latin-1 header encoding.
    """
    if is_unsafe_inline_media_type(media_type):
        return (
            "application/octet-stream",
            {
                "Content-Disposition": content_disposition_header(
                    filename, disposition="attachment",
                ),
                "X-Content-Type-Options": "nosniff",
                "Cache-Control": "no-store",
            },
        )
    disposition = "attachment" if download else "inline"
    return (
        media_type or "application/octet-stream",
        {
            "Content-Disposition": content_disposition_header(
                filename, disposition=disposition,
            ),
            "X-Content-Type-Options": "nosniff",
            "Cache-Control": "no-store",
        },
    )


def classify_preview_kind(path: Path, raw: bytes | None, size: int) -> str:
    """Return preview_kind for MIME-safe routing (C17)."""
    if size > _MAX_FILE_BYTES:
        return "too_large"
    ext = path.suffix.lower()
    if ext in _IMAGE_EXTS:
        return "image"
    if ext in _PDF_EXTS:
        return "pdf"
    if ext in _HTML_EXTS:
        return "html"
    if ext in _MARKDOWN_EXTS:
        return "markdown"
    if raw is not None and b"\x00" in raw[:8192]:
        return "binary"
    if ext in _CODE_EXTS:
        # Plain logs/txt stay "text"; language files use "code" for highlighting.
        if ext in {".txt", ".log"}:
            return "text"
        return "code"
    if raw is not None and _looks_like_text(raw):
        return "text"
    return "binary"


def _looks_like_text(raw: bytes) -> bool:
    if not raw:
        return True
    if b"\x00" in raw[:8192]:
        return False
    sample = raw[:8192]
    try:
        sample.decode("utf-8")
    except UnicodeDecodeError:
        return False
    return True


def read_workspace_file_bytes(root_path: str, relative_path: str) -> tuple[Path, Path, bytes]:
    """Resolve and read raw workspace file bytes (for download / binary preview)."""
    try:
        root, path = resolve_workspace_path(root_path, relative_path)
    except FileNotFoundError as exc:
        raise WorkspaceSurfaceError("文件不存在或已删除") from exc
    if not path.is_file():
        raise WorkspaceSurfaceError("文件不存在或已删除")
    try:
        return root, path, path.read_bytes()
    except OSError as exc:
        raise WorkspaceSurfaceError(f"无法读取文件：{exc}") from exc


def resolve_workspace_path(root_path: str, relative_path: str = "") -> tuple[Path, Path]:
    root = Path(root_path).expanduser().resolve(strict=True)
    candidate = (root / relative_path).resolve(strict=True)
    if candidate != root and root not in candidate.parents:
        raise WorkspaceSurfaceError("路径超出当前工作区")
    return root, candidate


def list_workspace_directory(root_path: str, relative_path: str = "") -> dict[str, Any]:
    root, directory = resolve_workspace_path(root_path, relative_path)
    if not directory.is_dir():
        raise WorkspaceSurfaceError("目标不是目录")
    rows: list[dict[str, Any]] = []
    try:
        children = sorted(directory.iterdir(), key=lambda item: (not item.is_dir(), item.name.casefold()))
    except OSError as exc:
        raise WorkspaceSurfaceError(str(exc)) from exc
    for child in children:
        if child.name in _IGNORED_DIRS or child.is_symlink():
            continue
        try:
            relative = child.relative_to(root).as_posix()
            is_dir = child.is_dir()
            rows.append({
                "name": child.name,
                "path": relative,
                "kind": "directory" if is_dir else "file",
                "size": None if is_dir else child.stat().st_size,
            })
        except OSError:
            continue
    return {"path": directory.relative_to(root).as_posix() if directory != root else "", "entries": rows}


def read_workspace_file(
    root_path: str,
    relative_path: str,
    *,
    line: int | None = None,
) -> dict[str, Any]:
    """Typed workspace file preview (C17).

    Text/code/markdown/html under the size cap return ``content``.
    Image/PDF return metadata only — clients fetch raw bytes via the download
    endpoint so downloads keep original bytes and the UI never dumps binary as text.
    """
    try:
        root, path, raw = read_workspace_file_bytes(root_path, relative_path)
    except WorkspaceSurfaceError:
        raise
    except OSError as exc:
        raise WorkspaceSurfaceError(f"文件不存在或不可访问：{exc}") from exc

    size = len(raw)
    media_type = guess_media_type(path)
    kind = classify_preview_kind(path, raw, size)
    relative = path.relative_to(root).as_posix()
    focus_line = line if isinstance(line, int) and line > 0 else None
    base: dict[str, Any] = {
        "path": relative,
        "size": size,
        "media_type": media_type,
        "preview_kind": kind,
        "max_preview_bytes": _MAX_FILE_BYTES,
        "truncated": False,
        "downloadable": True,
        "line": focus_line,
        "content": None,
        "message": None,
    }

    if kind == "too_large":
        base["message"] = (
            f"文件超过 {_MAX_FILE_BYTES // 1024} KB 预览上限"
            f"（当前 {size / 1024:.1f} KB）。请下载原文件查看。"
        )
        return base

    if kind in {"image", "pdf", "binary"}:
        if kind == "binary":
            base["message"] = "此类型暂不支持内联预览，请下载原文件。"
        return base

    # Text-like kinds: decode for preview.
    text = raw.decode("utf-8", errors="replace")
    base["content"] = text
    if kind == "html":
        base["message"] = "HTML 预览在沙箱 iframe 中渲染，脚本权限受限。"
    return base


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _count_patch_lines(patch: str) -> tuple[int, int]:
    additions = 0
    deletions = 0
    for row in patch.splitlines():
        if row.startswith("+") and not row.startswith("+++"):
            additions += 1
        elif row.startswith("-") and not row.startswith("---"):
            deletions += 1
    return additions, deletions


def _short_head_sha(root: Path) -> str | None:
    completed = _run_git(root, "rev-parse", "--short", "HEAD", check=False)
    if completed.returncode != 0:
        return None
    value = (completed.stdout or "").strip()
    return value or None


def split_unified_diff_files(patch: str) -> list[dict[str, Any]]:
    """Split a multi-file unified diff into per-path patches with meta."""
    text = (patch or "").strip()
    if not text:
        return []
    lines = text.splitlines()
    chunks: list[list[str]] = []
    current: list[str] = []
    for line in lines:
        if line.startswith("diff --git ") and current:
            chunks.append(current)
            current = [line]
        else:
            current.append(line)
    if current:
        chunks.append(current)

    files: list[dict[str, Any]] = []
    for chunk in chunks:
        header = chunk[0] if chunk else ""
        match = _DIFF_HEADER_RE.match(header)
        path = match.group(2) if match else "unknown"
        old_path = match.group(1) if match else None
        rename_from = None
        rename_to = None
        binary = False
        status = "M"
        for line in chunk[1:]:
            if line.startswith("new file mode"):
                status = "A"
            elif line.startswith("deleted file mode"):
                status = "D"
            elif line.startswith("rename from "):
                rename_from = _RENAME_FROM_RE.match(line)
                rename_from = rename_from.group(1) if rename_from else line[12:]
                status = "R"
            elif line.startswith("rename to "):
                rename_to = _RENAME_TO_RE.match(line)
                rename_to = rename_to.group(1) if rename_to else line[10:]
                path = rename_to or path
                status = "R"
            elif _BINARY_RE.match(line) or line.startswith("GIT binary patch"):
                binary = True
        raw = "\n".join(chunk)
        additions, deletions = _count_patch_lines(raw)
        files.append({
            "path": path,
            "old_path": rename_from or (old_path if old_path and old_path != path and status == "R" else None),
            "status": status,
            "binary": binary,
            "additions": additions,
            "deletions": deletions,
            "patch": raw,
        })
    return files


def _git_diff_patch(root: Path, *extra: str, path: str | None = None) -> str:
    args = ["diff", "--no-ext-diff", "--no-color", "--find-renames", *extra]
    if path:
        args.extend(["--", path])
    else:
        args.append("--")
    completed = _run_git(root, *args, check=False)
    return (completed.stdout or "").strip()


def _untracked_files(root: Path, path_filter: str | None = None) -> list[str]:
    porcelain = _run_git(root, "status", "--porcelain=v1", "-uall", check=False)
    rows: list[str] = []
    for line in (porcelain.stdout or "").splitlines():
        if not line.startswith("??"):
            continue
        candidate = line[3:].strip()
        if path_filter and candidate != path_filter:
            continue
        rows.append(candidate)
    return rows


def _synthetic_untracked_patch(root: Path, relative: str) -> dict[str, Any] | None:
    target = root / relative
    if not target.is_file():
        return None
    try:
        raw_bytes = target.read_bytes()
    except OSError:
        return None
    binary = b"\x00" in raw_bytes[:8192]
    if binary:
        return {
            "path": relative,
            "old_path": None,
            "status": "A",
            "staging": "untracked",
            "binary": True,
            "additions": 0,
            "deletions": 0,
            "patch": f"diff --git a/{relative} b/{relative}\nnew file mode 100644\nBinary files /dev/null and b/{relative} differ\n",
        }
    if len(raw_bytes) > _MAX_FILE_BYTES:
        return {
            "path": relative,
            "old_path": None,
            "status": "A",
            "staging": "untracked",
            "binary": False,
            "additions": 0,
            "deletions": 0,
            "patch": (
                f"diff --git a/{relative} b/{relative}\n"
                f"new file mode 100644\n"
                f"--- /dev/null\n+++ b/{relative}\n"
                f"@@ -0,0 +1 @@\n+/* truncated: file exceeds {_MAX_FILE_BYTES} bytes */\n"
            ),
        }
    text = raw_bytes.decode("utf-8", errors="replace")
    body = "\n".join(f"+{row}" for row in text.splitlines())
    patch = (
        f"diff --git a/{relative} b/{relative}\n"
        f"new file mode 100644\n"
        f"--- /dev/null\n+++ b/{relative}\n"
        f"{body}"
    )
    additions, deletions = _count_patch_lines(patch)
    return {
        "path": relative,
        "old_path": None,
        "status": "A",
        "staging": "untracked",
        "binary": False,
        "additions": additions,
        "deletions": deletions,
        "patch": patch,
    }


def _section_payload(patch: str, staging: str) -> dict[str, Any]:
    files = []
    for item in split_unified_diff_files(patch):
        files.append({
            **item,
            "staging": staging,
        })
    additions, deletions = _count_patch_lines(patch)
    return {
        "patch": patch,
        "additions": additions,
        "deletions": deletions,
        "files": files,
    }


def read_workspace_diff(
    root_path: str,
    *,
    path: str | None = None,
    staging: str | None = None,
) -> dict[str, Any]:
    """Return live worktree Diff with staged/unstaged/untracked sections.

    ``path`` scopes the response to one file. ``staging`` filters to
    staged | unstaged | untracked. Top-level ``patch`` remains for
    backward compatibility but is labeled so it is not an unexplained mix.
    """
    status = inspect_git_workspace(root_path)
    root = resolve_git_root(root_path)
    captured_at = _utc_now_iso()
    empty = {
        **status,
        "baseline": "worktree",
        "head_sha": None,
        "captured_at": captured_at,
        "files": [],
        "sections": {
            "staged": {"patch": "", "additions": 0, "deletions": 0, "files": []},
            "unstaged": {"patch": "", "additions": 0, "deletions": 0, "files": []},
            "untracked": {"patch": "", "additions": 0, "deletions": 0, "files": []},
        },
        "patch": "",
        "additions": 0,
        "deletions": 0,
    }
    if root is None:
        return empty

    path_filter = str(path or "").strip() or None
    staging_filter = str(staging or "").strip().lower() or None
    if staging_filter and staging_filter not in _VALID_STAGING:
        raise WorkspaceSurfaceError("staging 仅支持 staged / unstaged / untracked")

    head_sha = _short_head_sha(root)
    staged_patch = _git_diff_patch(root, "--cached", path=path_filter) if staging_filter in (None, "staged") else ""
    unstaged_patch = _git_diff_patch(root, path=path_filter) if staging_filter in (None, "unstaged") else ""

    untracked_files: list[dict[str, Any]] = []
    untracked_patches: list[str] = []
    if staging_filter in (None, "untracked"):
        for relative in _untracked_files(root, path_filter):
            item = _synthetic_untracked_patch(root, relative)
            if item is None:
                continue
            untracked_files.append(item)
            untracked_patches.append(item["patch"])

    staged_section = _section_payload(staged_patch, "staged")
    unstaged_section = _section_payload(unstaged_patch, "unstaged")
    untracked_patch = "\n".join(part for part in untracked_patches if part).strip()
    untracked_section = {
        "patch": untracked_patch,
        "additions": sum(int(item["additions"]) for item in untracked_files),
        "deletions": sum(int(item["deletions"]) for item in untracked_files),
        "files": untracked_files,
    }

    files: list[dict[str, Any]] = []
    files.extend({k: v for k, v in item.items() if k != "patch"} for item in staged_section["files"])
    files.extend({k: v for k, v in item.items() if k != "patch"} for item in unstaged_section["files"])
    files.extend({k: v for k, v in item.items() if k != "patch"} for item in untracked_section["files"])

    labeled_parts: list[str] = []
    if staged_section["patch"]:
        labeled_parts.append("# staged\n" + staged_section["patch"])
    if unstaged_section["patch"]:
        labeled_parts.append("# unstaged\n" + unstaged_section["patch"])
    if untracked_section["patch"]:
        labeled_parts.append("# untracked\n" + untracked_section["patch"])
    labeled_patch = "\n\n".join(labeled_parts)

    # When a single staging+path is requested, return that file's raw patch at top-level
    # so lazy loaders do not need to re-split labeled multi-section text.
    focused_patch = labeled_patch
    if path_filter and staging_filter:
        section = {
            "staged": staged_section,
            "unstaged": unstaged_section,
            "untracked": untracked_section,
        }[staging_filter]
        focused_patch = str(section.get("patch") or "")

    additions = (
        staged_section["additions"]
        + unstaged_section["additions"]
        + untracked_section["additions"]
    )
    deletions = (
        staged_section["deletions"]
        + unstaged_section["deletions"]
        + untracked_section["deletions"]
    )

    return {
        **status,
        "baseline": "worktree",
        "head_sha": head_sha,
        "captured_at": captured_at,
        "files": files,
        "sections": {
            "staged": {
                "patch": staged_section["patch"],
                "additions": staged_section["additions"],
                "deletions": staged_section["deletions"],
            },
            "unstaged": {
                "patch": unstaged_section["patch"],
                "additions": unstaged_section["additions"],
                "deletions": unstaged_section["deletions"],
            },
            "untracked": {
                "patch": untracked_section["patch"],
                "additions": untracked_section["additions"],
                "deletions": untracked_section["deletions"],
            },
        },
        "selected_path": path_filter,
        "selected_staging": staging_filter,
        "patch": focused_patch,
        "additions": additions,
        "deletions": deletions,
    }


def run_workspace_command(root_path: str, command: str) -> dict[str, Any]:
    root = Path(root_path).expanduser().resolve(strict=True)
    value = str(command or "").strip()
    if not value:
        raise WorkspaceSurfaceError("请输入命令")
    try:
        completed = subprocess.run(
            [os.environ.get("SHELL") or "/bin/zsh", "-lc", value],
            cwd=root,
            capture_output=True,
            timeout=30,
        )
    except subprocess.TimeoutExpired as exc:
        raise WorkspaceSurfaceError("命令执行超过 30 秒") from exc
    output = (completed.stdout + completed.stderr)[:_MAX_OUTPUT_BYTES]
    return {
        "command": value,
        "cwd": str(root),
        "exit_code": completed.returncode,
        "output": output.decode("utf-8", errors="replace"),
        "truncated": len(completed.stdout) + len(completed.stderr) > _MAX_OUTPUT_BYTES,
    }


# ── C18: cross-directory file quick-open + content search ─────────────────────

_SEARCH_IGNORED_DIRS = _IGNORED_DIRS | {"dist", "build", "coverage", ".turbo", ".cache"}
_MAX_SEARCH_FILE_BYTES = 2_000_000  # skip files larger than 2 MB for content search
_MAX_CONTENT_HITS = 30
_MAX_FILE_HITS = 60
_SNIPPET_CONTEXT_LINES = 1  # lines of context around each content hit


def _matches_query(text: str, query: str) -> bool:
    """Case-insensitive substring check."""
    return query.lower() in text.lower()


def search_workspace_files(
    root_path: str,
    query: str,
    max_results: int = _MAX_FILE_HITS,
) -> dict[str, Any]:
    """Fuzzy-match file/directory paths across the whole workspace tree.

    Returns results ordered by: exact name match → name prefix → path contains.
    Ignored dirs are listed in the response so the client can show scope info.
    """
    root = Path(root_path).expanduser().resolve()
    if not root.is_dir():
        raise WorkspaceSurfaceError("工作区根目录不可用")

    q = query.strip()
    if not q:
        return {"results": [], "truncated": False, "ignored_dirs": sorted(_SEARCH_IGNORED_DIRS), "query": q}

    results: list[dict[str, Any]] = []
    truncated = False

    for dirpath, dirnames, filenames in os.walk(root):
        # Prune ignored directories in-place so os.walk skips them
        dirnames[:] = [d for d in dirnames if d not in _SEARCH_IGNORED_DIRS]

        rel_dir = Path(dirpath).relative_to(root).as_posix()

        for name in filenames:
            rel = (rel_dir + "/" + name).lstrip("./") if rel_dir != "." else name
            if _matches_query(name, q) or _matches_query(rel, q):
                try:
                    size = (root / rel).stat().st_size
                except OSError:
                    size = None
                results.append({"name": name, "path": rel, "kind": "file", "size": size})
                if len(results) >= max_results:
                    truncated = True
                    break

        # Also include matching directories
        for name in dirnames:
            rel = (rel_dir + "/" + name).lstrip("./") if rel_dir != "." else name
            if _matches_query(name, q) or _matches_query(rel, q):
                results.append({"name": name, "path": rel, "kind": "directory", "size": None})
                if len(results) >= max_results:
                    truncated = True
                    break

        if truncated:
            break

    # Sort: exact filename match first, then prefix, then path contains
    def _rank(item: dict[str, Any]) -> int:
        name_lo = item["name"].lower()
        q_lo = q.lower()
        if name_lo == q_lo:
            return 0
        if name_lo.startswith(q_lo):
            return 1
        return 2

    results.sort(key=_rank)
    return {
        "results": results[:max_results],
        "truncated": truncated,
        "ignored_dirs": sorted(_SEARCH_IGNORED_DIRS),
        "query": q,
    }


def search_workspace_content(
    root_path: str,
    query: str,
    max_results: int = _MAX_CONTENT_HITS,
) -> dict[str, Any]:
    """Full-text content search across the workspace using ripgrep (or grep fallback).

    Returns up to *max_results* hits with file path, 1-based line number, and a
    snippet of context.  Binary and oversized files are silently skipped.
    """
    root = Path(root_path).expanduser().resolve()
    if not root.is_dir():
        raise WorkspaceSurfaceError("工作区根目录不可用")

    q = query.strip()
    if not q:
        return {"results": [], "truncated": False, "query": q}

    # Build ignore-dir flags
    rg_available = bool(subprocess.run(["which", "rg"], capture_output=True).returncode == 0)

    try:
        if rg_available:
            ignore_flags: list[str] = []
            for d in _SEARCH_IGNORED_DIRS:
                ignore_flags += ["--glob", f"!{d}/**"]
            cmd = [
                "rg",
                "--line-number",
                "--color=never",
                "--max-count=3",          # at most 3 hits per file
                f"--max-filesize={_MAX_SEARCH_FILE_BYTES}",
                "--no-heading",
                "--smart-case",
                *ignore_flags,
                q,
                str(root),
            ]
        else:
            # grep fallback
            exclude_dirs = " ".join(f"--exclude-dir={d}" for d in _SEARCH_IGNORED_DIRS)
            cmd = [
                "grep",
                "-rn",
                "--binary-files=without-match",
                *[f"--exclude-dir={d}" for d in _SEARCH_IGNORED_DIRS],
                "-i",
                q,
                str(root),
            ]

        completed = subprocess.run(cmd, capture_output=True, timeout=20)
        raw = (completed.stdout).decode("utf-8", errors="replace")
    except subprocess.TimeoutExpired:
        raise WorkspaceSurfaceError("内容搜索超时（20 秒）")
    except FileNotFoundError:
        raise WorkspaceSurfaceError("搜索工具不可用")

    results: list[dict[str, Any]] = []
    seen_files: set[str] = set()
    truncated = False

    for line in raw.splitlines():
        # Format: /abs/path/file.py:42:matched content
        parts = line.split(":", 2)
        if len(parts) < 3:
            continue
        abs_path, lineno_str, snippet = parts[0], parts[1], parts[2]
        if not lineno_str.isdigit():
            continue
        try:
            rel = Path(abs_path).relative_to(root).as_posix()
        except ValueError:
            continue
        results.append({
            "path": rel,
            "line": int(lineno_str),
            "snippet": snippet[:200],
        })
        seen_files.add(rel)
        if len(results) >= max_results:
            truncated = True
            break

    return {"results": results, "truncated": truncated, "query": q}
