"""Build native multimodal / file wire payloads from resolved attachments.

Adapters call these helpers so Codex ``turn/start``, Claude ``query``, and
Pi ``prompt`` receive real image bytes or readable workspace paths — never
unresolved sha256 digests alone.
"""

from __future__ import annotations

import base64
from pathlib import Path
from typing import Any, AsyncIterator, Optional


def _items(payload: Optional[dict[str, Any]]) -> list[dict[str, Any]]:
    raw = (payload or {}).get("attachments")
    if not isinstance(raw, list):
        return []
    return [item for item in raw if isinstance(item, dict)]


def _read_bytes(item: dict[str, Any]) -> bytes:
    for key in ("path", "workspace_path", "cas_path"):
        path = str(item.get(key) or "").strip()
        if path and Path(path).is_file():
            return Path(path).read_bytes()
    data_b64 = str(item.get("content_base64") or "").strip()
    if data_b64:
        return base64.b64decode(data_b64)
    raise FileNotFoundError(
        f"attachment {item.get('name') or item.get('sha256') or '?'} has no readable bytes"
    )


def codex_turn_input(
    text: str,
    payload: Optional[dict[str, Any]] = None,
) -> list[dict[str, Any]]:
    """Codex ``UserInput[]`` for ``turn/start`` / ``turn/steer``.

    Images use ``localImage`` when a workspace path exists, otherwise an
    inline ``image`` data URL. Non-image files remain workspace context in
    ``text`` (already merged by the Conversation executor).
    """
    items: list[dict[str, Any]] = [{"type": "text", "text": text}]
    capability = (payload or {}).get("runtime_capability") or {}
    if capability.get("kind") == "skill":
        invocation = capability.get("invocation") or {}
        if invocation.get("path") and capability.get("name"):
            items.append({"type": "skill", "name": capability["name"], "path": invocation["path"]})
    for item in _items(payload):
        if str(item.get("delivery") or "") != "native_image":
            continue
        path = str(
            item.get("workspace_path")
            or item.get("path")
            or item.get("cas_path")
            or ""
        ).strip()
        if path and Path(path).is_file():
            items.append({"type": "localImage", "path": path})
            continue
        media_type = str(item.get("media_type") or "image/png")
        raw = _read_bytes(item)
        encoded = base64.b64encode(raw).decode("ascii")
        items.append({
            "type": "image",
            "url": f"data:{media_type};base64,{encoded}",
        })
    return items


def pi_prompt_images(
    payload: Optional[dict[str, Any]] = None,
) -> list[dict[str, str]]:
    """Pi RPC ``images`` entries: ``{type, data, mimeType}``."""
    images: list[dict[str, str]] = []
    for item in _items(payload):
        if str(item.get("delivery") or "") != "native_image":
            continue
        media_type = str(item.get("media_type") or "image/png")
        raw = _read_bytes(item)
        images.append({
            "type": "image",
            "data": base64.b64encode(raw).decode("ascii"),
            "mimeType": media_type,
        })
    return images


def claude_user_content(
    text: str,
    payload: Optional[dict[str, Any]] = None,
) -> list[dict[str, Any]]:
    """Anthropic-style content blocks for Claude Agent SDK user messages."""
    content: list[dict[str, Any]] = [{"type": "text", "text": text}]
    for item in _items(payload):
        if str(item.get("delivery") or "") != "native_image":
            continue
        media_type = str(item.get("media_type") or "image/png")
        raw = _read_bytes(item)
        content.append({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": base64.b64encode(raw).decode("ascii"),
            },
        })
    return content


async def claude_query_prompt(
    text: str,
    payload: Optional[dict[str, Any]] = None,
    *,
    session_id: str = "default",
) -> str | AsyncIterator[dict[str, Any]]:
    """Return a string prompt, or a streaming multimodal user message."""
    content = claude_user_content(text, payload)
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


def summarize_wire_attachments(
    adapter: str,
    text: str,
    payload: Optional[dict[str, Any]] = None,
) -> dict[str, Any]:
    """Test/debug summary of what an Adapter would send on the wire."""
    items = _items(payload)
    if adapter == "codex":
        wire = codex_turn_input(text, payload)
        return {
            "adapter": "codex",
            "input": wire,
            "native_image_count": sum(
                1 for part in wire if part.get("type") in {"localImage", "image"}
            ),
            "attachment_count": len(items),
        }
    if adapter == "pi":
        images = pi_prompt_images(payload)
        return {
            "adapter": "pi",
            "prompt": {"message": text, "images": images},
            "native_image_count": len(images),
            "attachment_count": len(items),
        }
    if adapter == "claude":
        content = claude_user_content(text, payload)
        return {
            "adapter": "claude",
            "content": content,
            "native_image_count": sum(
                1 for part in content if part.get("type") == "image"
            ),
            "attachment_count": len(items),
        }
    raise ValueError(f"unknown adapter for wire summary: {adapter}")


__all__ = [
    "claude_query_prompt",
    "claude_user_content",
    "codex_turn_input",
    "pi_prompt_images",
    "summarize_wire_attachments",
]
