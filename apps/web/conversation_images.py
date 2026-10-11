"""Thread-scoped image byte access for Conversation Markdown previews."""

from __future__ import annotations

import hashlib
from pathlib import Path

from muteki.conversation.computer_control import _IMAGE_EXTENSIONS, _thread_key
from muteki.conversation.workspace_surfaces import guess_media_type


class ThreadImageNotFound(Exception):
    """The requested image is missing or outside the Thread's allowed roots."""


_IMAGE_SIGNATURES = {
    "image/png": lambda raw: raw.startswith(b"\x89PNG\r\n\x1a\n"),
    "image/jpeg": lambda raw: raw.startswith(b"\xff\xd8\xff"),
    "image/gif": lambda raw: raw.startswith((b"GIF87a", b"GIF89a")),
    "image/webp": lambda raw: raw.startswith(b"RIFF") and raw[8:12] == b"WEBP",
    "image/bmp": lambda raw: raw.startswith(b"BM"),
    "image/x-icon": lambda raw: raw.startswith((b"\x00\x00\x01\x00", b"\x00\x00\x02\x00")),
}


def _is_within(path: Path, root: Path) -> bool:
    return path == root or root in path.parents


def _read_image(path: Path, *, content_addressed: bool) -> tuple[bytes, str]:
    media_type = guess_media_type(path)
    if not media_type.startswith("image/"):
        raise ThreadImageNotFound("not an image")
    if not path.is_file():
        raise ThreadImageNotFound("image not found")
    raw = path.read_bytes()
    signature_check = _IMAGE_SIGNATURES.get(media_type)
    if signature_check is not None and not signature_check(raw):
        raise ThreadImageNotFound("image bytes do not match their media type")
    if media_type == "image/svg+xml":
        document = raw.lstrip(b"\xef\xbb\xbf\x00\t\r\n ").lower()
        if not (document.startswith(b"<svg") or document.startswith(b"<?xml")):
            raise ThreadImageNotFound("image bytes do not match their media type")
    if content_addressed and hashlib.sha256(raw).hexdigest() != path.stem:
        raise ThreadImageNotFound("image digest mismatch")
    return raw, media_type


def _chat_root(path_value: str | Path, *, thread_id: str) -> Path:
    root_input = Path(path_value).expanduser()
    expected_name = hashlib.sha256(thread_id.encode()).hexdigest()[:24]
    if root_input.name != expected_name or root_input.is_symlink():
        raise ThreadImageNotFound("invalid Thread image root")
    return root_input.resolve(strict=True)


def _read_from_root(
    requested: Path,
    root: Path,
    *,
    root_label: str,
) -> tuple[bytes, str, str]:
    try:
        candidate = (
            requested.resolve(strict=True)
            if requested.is_absolute()
            else (root / requested).resolve(strict=True)
        )
    except (OSError, RuntimeError) as exc:
        raise ThreadImageNotFound("image not found") from exc
    if not _is_within(candidate, root):
        raise ThreadImageNotFound(f"image is outside the Thread {root_label}")
    image, media_type = _read_image(candidate, content_addressed=False)
    return image, media_type, candidate.name


def read_thread_image_bytes(
    path_value: str,
    *,
    thread_id: str,
    workspace_root: str | None,
    computer_use_root: str | Path,
    chat_workspace_root: str | Path | None = None,
) -> tuple[bytes, str, str]:
    """Read an image only from this Thread's CUA, project, or chat workspace."""
    raw_path = str(path_value or "").strip()
    if not raw_path:
        raise ThreadImageNotFound("image path is empty")
    requested = Path(raw_path).expanduser()
    if not requested.is_absolute():
        # Match the runtime cwd: project-backed chats run in their project;
        # chats without a project run in their private chat workspace.
        if workspace_root:
            root = Path(workspace_root).expanduser().resolve(strict=True)
            return _read_from_root(requested, root, root_label="workspace")
        if chat_workspace_root:
            root = _chat_root(chat_workspace_root, thread_id=thread_id)
            return _read_from_root(requested, root, root_label="image workspace")
        raise ThreadImageNotFound("Thread has no image workspace")

    computer_root_input = Path(computer_use_root).expanduser().absolute()
    computer_root = computer_root_input.resolve(strict=False)
    thread_root = computer_root / _thread_key(thread_id)
    image_root = thread_root / "images"
    requested_image_root = computer_root_input / _thread_key(thread_id) / "images"
    try:
        resolved = requested.resolve(strict=True)
    except (OSError, RuntimeError):
        resolved = None
    if (
        resolved is not None
        and not thread_root.is_symlink()
        and not image_root.is_symlink()
        and not requested.is_symlink()
        and requested.absolute().parent == requested_image_root
        and resolved.parent == image_root
    ):
        image, media_type = _read_image(resolved, content_addressed=True)
        expected_suffix = _IMAGE_EXTENSIONS.get(media_type)
        if expected_suffix is None or resolved.suffix.lower() != f".{expected_suffix}":
            raise ThreadImageNotFound("image extension does not match its media type")
        return image, media_type, resolved.name

    try:
        resolved = requested.resolve(strict=True)
    except (OSError, RuntimeError) as exc:
        raise ThreadImageNotFound("image not found") from exc
    if workspace_root:
        root = Path(workspace_root).expanduser().resolve(strict=True)
        if _is_within(resolved, root):
            return _read_from_root(resolved, root, root_label="workspace")
    if chat_workspace_root:
        root = _chat_root(chat_workspace_root, thread_id=thread_id)
        if _is_within(resolved, root):
            return _read_from_root(resolved, root, root_label="image workspace")
    raise ThreadImageNotFound("image is outside the Thread's allowed roots")


__all__ = ["ThreadImageNotFound", "read_thread_image_bytes"]
