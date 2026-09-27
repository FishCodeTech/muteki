"""Conversation attachment resolution and workspace staging (C03).

Turn records store attachment *identities* as sha256 digests. Before a
Runtime send, those digests must become concrete, authorized Artifact
metadata plus bytes the target engine can actually consume:

- native image input when the Adapter declares ``image_input``;
- otherwise an explicit workspace file path the agent can read.

Silent omission is forbidden: missing Artifact bytes, unauthorized digests,
or attachments that cannot be delivered raise before the Adapter is called.
"""

from __future__ import annotations

import base64
import hashlib
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Optional, Sequence

from muteki.platform.contracts.objects import Artifact

#: Image MIME types eligible for native multimodal delivery.
NATIVE_IMAGE_MIME = frozenset({
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/gif",
    "image/webp",
})

_SAFE_NAME = re.compile(r"[^\w.\-+=@]+", re.UNICODE)


class AttachmentDeliveryError(RuntimeError):
    """Raised when an attachment cannot be delivered to the Runtime."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


@dataclass(frozen=True)
class ResolvedAttachment:
    """Authorized Artifact ready for Adapter consumption."""

    sha256: str
    name: str
    media_type: str
    size: int
    cas_path: str
    workspace_path: str = ""
    delivery: str = "workspace_file"  # native_image | workspace_file
    content_base64: str = ""

    @property
    def is_native_image(self) -> bool:
        return self.delivery == "native_image"

    def to_payload(self) -> dict[str, Any]:
        payload = {
            "sha256": self.sha256,
            "name": self.name,
            "media_type": self.media_type,
            "size": self.size,
            "cas_path": self.cas_path,
            "path": self.workspace_path or self.cas_path,
            "workspace_path": self.workspace_path,
            "delivery": self.delivery,
        }
        if self.content_base64:
            payload["content_base64"] = self.content_base64
        return payload


def normalize_media_type(media_type: str, name: str = "") -> str:
    """Return a lowercase MIME, inferring from extension when missing."""
    raw = str(media_type or "").strip().lower()
    if raw == "image/jpg":
        return "image/jpeg"
    if raw:
        return raw.split(";", 1)[0].strip()
    suffix = Path(name or "").suffix.lower()
    return {
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".gif": "image/gif",
        ".webp": "image/webp",
        ".txt": "text/plain",
        ".md": "text/markdown",
        ".json": "application/json",
        ".csv": "text/csv",
        ".pdf": "application/pdf",
    }.get(suffix, "application/octet-stream")


def is_native_image_mime(media_type: str, name: str = "") -> bool:
    return normalize_media_type(media_type, name) in NATIVE_IMAGE_MIME


def safe_attachment_name(name: str, sha256: str) -> str:
    """Produce a cwd-safe basename that still looks like the original."""
    base = Path(str(name or "").strip() or sha256[:12]).name
    cleaned = _SAFE_NAME.sub("_", base).strip("._") or sha256[:12]
    if len(cleaned) > 120:
        stem = Path(cleaned).stem[:80]
        suffix = Path(cleaned).suffix[:20]
        cleaned = f"{stem}{suffix}" or sha256[:12]
    return cleaned


def thread_authorized_sha256s(
    events: Iterable[Any],
) -> set[str]:
    """Collect Artifact digests referenced by a Thread's artifact events."""
    allowed: set[str] = set()
    for event in events:
        event_type = getattr(event, "event_type", None) or (
            event.get("event_type") if isinstance(event, dict) else None
        )
        if event_type not in {
            "core.artifact.attached",
            "core.artifact.created",
        }:
            continue
        payload = getattr(event, "payload", None)
        if payload is None and isinstance(event, dict):
            payload = event.get("payload")
        digest = str((payload or {}).get("sha256") or "").strip().lower()
        if digest:
            allowed.add(digest)
    return allowed


def resolve_attachments(
    digests: Sequence[str],
    *,
    read_content,
    get_artifact,
    authorized_sha256s: Optional[set[str]] = None,
    workspace_root: str = "",
    stage_root: str = "",
    image_input: bool = False,
    stage: bool = True,
) -> list[ResolvedAttachment]:
    """Resolve sha256 digests into staged, deliverable attachments.

    ``read_content(sha256) -> bytes | None`` loads CAS bytes.
    ``get_artifact(sha256) -> Artifact | None`` loads metadata.

    Staging prefers ``workspace_root`` (the Runtime cwd). When a Thread has
    no bound Workspace, ``stage_root`` provides a thread-scoped fallback so
    ordinary files are still readable instead of being silently dropped.
    """
    if not digests:
        return []

    cleaned_workspace = str(workspace_root or "").strip()
    cleaned_stage = str(stage_root or "").strip()
    root: Optional[Path] = None
    if cleaned_workspace:
        root = Path(cleaned_workspace).expanduser().resolve()
    elif cleaned_stage:
        root = Path(cleaned_stage).expanduser().resolve()
    if root is not None and stage:
        (root / ".muteki" / "attachments").mkdir(parents=True, exist_ok=True)

    resolved: list[ResolvedAttachment] = []
    used_names: set[str] = set()

    for raw in digests:
        sha256 = str(raw or "").strip().lower()
        if not sha256:
            raise AttachmentDeliveryError(
                "conversation.attachment.empty_digest",
                "附件摘要为空，无法交付给 Runtime",
            )
        if authorized_sha256s is not None and sha256 not in authorized_sha256s:
            raise AttachmentDeliveryError(
                "conversation.attachment.unauthorized",
                f"附件 {sha256[:12]}… 未授权给当前 Thread，拒绝静默丢弃",
            )
        content = read_content(sha256)
        if content is None:
            raise AttachmentDeliveryError(
                "conversation.attachment.missing",
                f"附件 {sha256[:12]}… 内容缺失，上传成功不等于模型可读",
            )
        digest = hashlib.sha256(content).hexdigest()
        if digest != sha256:
            raise AttachmentDeliveryError(
                "conversation.attachment.digest_mismatch",
                f"附件 {sha256[:12]}… 内容与摘要不一致",
            )

        artifact = get_artifact(sha256)
        if isinstance(artifact, Artifact):
            name = artifact.name or sha256[:12]
            media_type = normalize_media_type(artifact.media_type, name)
            size = int(artifact.size or len(content))
            cas_path = ""
        elif isinstance(artifact, dict):
            name = str(artifact.get("name") or sha256[:12])
            media_type = normalize_media_type(
                str(artifact.get("media_type") or ""), name)
            size = int(artifact.get("size") or len(content))
            cas_path = str(artifact.get("cas_path") or "")
        else:
            name = sha256[:12]
            media_type = normalize_media_type("", name)
            size = len(content)
            cas_path = ""

        native = bool(image_input and is_native_image_mime(media_type, name))
        workspace_path = ""
        content_b64 = ""

        if root is not None and stage:
            filename = safe_attachment_name(name, sha256)
            if filename in used_names:
                stem = Path(filename).stem
                suffix = Path(filename).suffix
                filename = f"{stem}-{sha256[:8]}{suffix}"
            used_names.add(filename)
            dest = root / ".muteki" / "attachments" / filename
            if not dest.exists() or hashlib.sha256(dest.read_bytes()).hexdigest() != sha256:
                tmp = dest.with_suffix(dest.suffix + ".tmp")
                tmp.write_bytes(content)
                tmp.replace(dest)
            workspace_path = str(dest)
            if not cas_path:
                cas_path = workspace_path
        elif not native:
            raise AttachmentDeliveryError(
                "conversation.attachment.undeliverable",
                f"附件 {name!r} 无法交付：Runtime 无工作区路径且不支持原生图片输入",
            )
        else:
            # Native image adapters can consume inline bytes when no cwd is bound.
            content_b64 = base64.b64encode(content).decode("ascii")

        if not cas_path:
            cas_path = workspace_path

        resolved.append(ResolvedAttachment(
            sha256=sha256,
            name=name,
            media_type=media_type,
            size=size,
            cas_path=cas_path or workspace_path,
            workspace_path=workspace_path,
            delivery="native_image" if native else "workspace_file",
            content_base64=content_b64,
        ))
    return resolved


def attachment_context_text(attachments: Sequence[ResolvedAttachment]) -> str:
    """Human-readable workspace file context for the prompt text."""
    if not attachments:
        return ""
    lines = ["[用户上传的附件 — 已写入当前工作区，请直接读取这些路径]"]
    for item in attachments:
        path = item.workspace_path or item.cas_path
        lines.append(
            f"- {item.name} ({item.media_type or 'application/octet-stream'}, "
            f"{item.size} bytes, sha256={item.sha256}) → {path}"
        )
    return "\n".join(lines)


def merge_attachment_context(text: str, attachments: Sequence[ResolvedAttachment]) -> str:
    """Prepend explicit attachment paths when any file is workspace-delivered."""
    context = attachment_context_text(attachments)
    if not context:
        return text
    body = str(text or "").strip()
    if not body:
        return context
    return f"{context}\n\n[当前用户请求]\n{body}"


def attachments_payload(attachments: Sequence[ResolvedAttachment]) -> list[dict[str, Any]]:
    return [item.to_payload() for item in attachments]


__all__ = [
    "AttachmentDeliveryError",
    "NATIVE_IMAGE_MIME",
    "ResolvedAttachment",
    "attachment_context_text",
    "attachments_payload",
    "is_native_image_mime",
    "merge_attachment_context",
    "normalize_media_type",
    "resolve_attachments",
    "safe_attachment_name",
    "thread_authorized_sha256s",
]
