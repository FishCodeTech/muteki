"""Deliver Muteki context parts beside the user's unchanged message.

Each adapter picks the most native channel its runtime offers (system prompt,
Codex additional context, ACP resource blocks). Runtimes whose protocol only
carries one text get the parts as ``<muteki_context>`` envelopes before the
user's text; displays strip those envelopes because Muteki wrote them.
"""

from __future__ import annotations

import re
from typing import Any, Callable, Iterable, Sequence, Union

from muteki.platform.contracts.external_agents import ContextPart

ENVELOPE_TAG = "muteki_context"
CONTEXT_URI_PREFIX = "muteki://context/"

_ENVELOPE_RE = re.compile(
    r'<muteki_context key="muteki\.[a-z0-9][a-z0-9._-]*" kind="[a-z_]+" '
    r'trust="(?:application|untrusted)">\n.*?\n</muteki_context>(?:\n\n|\Z)',
    re.DOTALL,
)
# Codex renders untrusted additionalContext as its own user message.
_CODEX_EXTERNAL_RE = re.compile(
    r"\s*<external_(muteki\.[a-z0-9][a-z0-9._-]*)>.*</external_\1>\s*", re.DOTALL)


def envelope(part: ContextPart) -> str:
    return (f'<{ENVELOPE_TAG} key="{part.key}" kind="{part.kind}" trust="{part.trust}">\n'
            f"{part.text}\n</{ENVELOPE_TAG}>")


def text_with_envelopes(text: str, parts: Sequence[ContextPart]) -> str:
    """One-text channel: context envelopes first, then the user's text unchanged."""
    blocks = [envelope(part) for part in parts]
    if not blocks:
        return text
    return "\n\n".join([*blocks, text] if text else blocks)


def strip_envelopes(text: str) -> str:
    """Remove the leading context envelopes Muteki prepended to a user text."""
    position = 0
    while True:
        match = _ENVELOPE_RE.match(text, position)
        if match is None:
            return text[position:]
        position = match.end()


def visible_user_text(text: str) -> str:
    """A transcript user text without the context Muteki delivered beside it."""
    stripped = strip_envelopes(text)
    return "" if _CODEX_EXTERNAL_RE.fullmatch(stripped) else stripped


def system_append(parts: Iterable[ContextPart]) -> str:
    """Host rules for runtimes that accept appended system-prompt text."""
    return "\n\n".join(envelope(part) for part in parts)


def acp_resource_block(part: ContextPart) -> dict[str, Any]:
    return {
        "type": "resource",
        "annotations": {"audience": ["assistant"]},
        "_meta": {"muteki": {"key": part.key, "kind": part.kind, "trust": part.trust}},
        "resource": {"uri": CONTEXT_URI_PREFIX + part.key, "mimeType": "text/plain",
                     "text": part.text},
    }


def acp_block_channel(part: ContextPart, *, embedded_context: bool) -> str:
    """Untrusted data rides embedded resources (attached content); host rules
    stay text, since agents treat resource text as user-supplied material."""
    return "acp_resource" if embedded_context and part.trust == "untrusted" else "acp_text_block"


def acp_context_blocks(parts: Sequence[ContextPart], *, embedded_context: bool) -> list[dict[str, Any]]:
    return [acp_resource_block(part)
            if acp_block_channel(part, embedded_context=embedded_context) == "acp_resource"
            else {"type": "text", "text": envelope(part)} for part in parts]


REMINDER_TAG = "system-reminder"


def kimi_block_channel(part: ContextPart, *, embedded_context: bool = False) -> str:
    """Kimi's system prompt makes ``<system-reminder>`` text an authoritative
    harness directive; other text in the user turn is material it may refuse."""
    if part.trust == "application":
        return "kimi_system_reminder"
    return acp_block_channel(part, embedded_context=embedded_context)


def kimi_context_blocks(parts: Sequence[ContextPart], *, embedded_context: bool = False) -> list[dict[str, Any]]:
    blocks = []
    for part in parts:
        channel = kimi_block_channel(part, embedded_context=embedded_context)
        if channel == "kimi_system_reminder":
            blocks.append({"type": "text",
                           "text": f"<{REMINDER_TAG}>\n{envelope(part)}\n</{REMINDER_TAG}>"})
        elif channel == "acp_resource":
            blocks.append(acp_resource_block(part))
        else:
            blocks.append({"type": "text", "text": envelope(part)})
    return blocks


def is_context_block(block: Any) -> bool:
    """True for an ACP block (or its text rendering) that carries a Muteki part."""
    if not isinstance(block, dict):
        return False
    if block.get("type") == "resource":
        uri = str((block.get("resource") or {}).get("uri") or "")
        return uri.startswith(CONTEXT_URI_PREFIX)
    if block.get("type") == "text":
        text = str(block.get("text") or "")
        reminder = f"<{REMINDER_TAG}>\n<{ENVELOPE_TAG} "
        if text.startswith(reminder) and text.endswith(f"</{ENVELOPE_TAG}>\n</{REMINDER_TAG}>"):
            return True
        return (text.startswith(f'<resource uri="{CONTEXT_URI_PREFIX}')
                or (text.startswith(f"<{ENVELOPE_TAG} ") and strip_envelopes(text) == ""))
    return False


def codex_additional_context(parts: Iterable[ContextPart]) -> dict[str, dict[str, str]]:
    """Codex ``turn/start.additionalContext``: application rules render as
    developer input, untrusted data as quoted user-side input."""
    return {part.key: {"kind": part.trust, "value": part.text} for part in parts}


def codex_context_items(parts: Iterable[ContextPart]) -> list[dict[str, Any]]:
    """``thread/inject_items`` messages in the form Codex itself renders
    additionalContext: application parts as developer ``<key>`` messages,
    untrusted parts as user ``<external_key>`` messages."""
    items = []
    for part in parts:
        role, tag = ("developer", part.key) if part.trust == "application" else ("user", f"external_{part.key}")
        items.append({"type": "message", "role": role,
                      "content": [{"type": "input_text", "text": f"<{tag}>{part.text}</{tag}>"}]})
    return items


def envelope_text_blocks(parts: Sequence[ContextPart]) -> list[dict[str, str]]:
    return [{"type": "text", "text": envelope(part)} for part in parts]


def ledger_entries(parts: Sequence[ContextPart], channel: Union[str, Callable[[ContextPart], str]],
                   *, scope: str) -> list[dict[str, Any]]:
    return [{"key": part.key, "kind": part.kind, "trust": part.trust, "scope": scope,
             "channel": channel(part) if callable(channel) else channel,
             "sha256": part.sha256, "chars": len(part.text),
             "text": part.text} for part in parts]


__all__ = [
    "CONTEXT_URI_PREFIX",
    "ENVELOPE_TAG",
    "acp_block_channel",
    "acp_context_blocks",
    "acp_resource_block",
    "envelope_text_blocks",
    "codex_additional_context",
    "codex_context_items",
    "envelope",
    "is_context_block",
    "kimi_block_channel",
    "kimi_context_blocks",
    "ledger_entries",
    "strip_envelopes",
    "system_append",
    "text_with_envelopes",
    "visible_user_text",
]
