"""Build native multimodal / file wire payloads from resolved attachments.

Adapters call these helpers so Codex ``turn/start``, Claude ``query``, and
Pi ``prompt`` receive real image bytes or readable workspace paths — never
unresolved sha256 digests alone.
"""

from __future__ import annotations

import base64
from pathlib import Path
from typing import Any, AsyncIterator, Optional, Sequence

from muteki.platform.contracts.external_agents import AttachmentRef


def _native_images(attachments: Sequence[AttachmentRef]) -> list[AttachmentRef]:
    return [item for item in attachments if item.delivery == "native_image"]


def _read_bytes(item: AttachmentRef) -> bytes:
    for path in (item.path, item.workspace_path, item.cas_path):
        path = path.strip()
        if path and Path(path).is_file():
            return Path(path).read_bytes()
    data_b64 = item.content_base64.strip()
    if data_b64:
        return base64.b64decode(data_b64)
    raise FileNotFoundError(
        f"attachment {item.name or item.sha256 or '?'} has no readable bytes"
    )


def codex_turn_input(
    text: str,
    attachments: Sequence[AttachmentRef] = (),
    runtime_capability: Optional[dict[str, Any]] = None,
) -> list[dict[str, Any]]:
    """Codex ``UserInput[]`` for ``turn/start`` / ``turn/steer``.

    Images use ``localImage`` when a workspace path exists, otherwise an
    inline ``image`` data URL. Non-image files remain workspace context in
    ``text`` (already merged by the Conversation executor).
    """
    items: list[dict[str, Any]] = [{"type": "text", "text": text}]
    capability = runtime_capability or {}
    if capability.get("kind") == "skill":
        invocation = capability.get("invocation") or {}
        if invocation.get("path") and capability.get("name"):
            items.append({"type": "skill", "name": capability["name"], "path": invocation["path"]})
    for item in _native_images(attachments):
        path = (item.workspace_path or item.path or item.cas_path).strip()
        if path and Path(path).is_file():
            items.append({"type": "localImage", "path": path})
            continue
        media_type = item.media_type or "image/png"
        encoded = base64.b64encode(_read_bytes(item)).decode("ascii")
        items.append({
            "type": "image",
            "url": f"data:{media_type};base64,{encoded}",
        })
    return items


def pi_prompt_images(
    attachments: Sequence[AttachmentRef] = (),
) -> list[dict[str, str]]:
    """Pi RPC ``images`` entries: ``{type, data, mimeType}``."""
    return [
        {
            "type": "image",
            "data": base64.b64encode(_read_bytes(item)).decode("ascii"),
            "mimeType": item.media_type or "image/png",
        }
        for item in _native_images(attachments)
    ]


def claude_user_content(
    text: str,
    attachments: Sequence[AttachmentRef] = (),
) -> list[dict[str, Any]]:
    """Anthropic-style content blocks for Claude Agent SDK user messages."""
    content: list[dict[str, Any]] = [{"type": "text", "text": text}]
    for item in _native_images(attachments):
        content.append({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": item.media_type or "image/png",
                "data": base64.b64encode(_read_bytes(item)).decode("ascii"),
            },
        })
    return content


async def claude_query_prompt(
    text: str,
    attachments: Sequence[AttachmentRef] = (),
    *,
    session_id: str = "default",
) -> str | AsyncIterator[dict[str, Any]]:
    """Return a string prompt, or a streaming multimodal user message."""
    content = claude_user_content(text, attachments)
    if len(content) == 1 and content[0].get("type") == "text":
        return text

    async def _messages() -> AsyncIterator[dict[str, Any]]:
        yield {
            "type": "user",
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": None,
            "session_id": session_id,
        }

    return _messages()


__all__ = [
    "claude_query_prompt",
    "claude_user_content",
    "codex_turn_input",
    "pi_prompt_images",
]
