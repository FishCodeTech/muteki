"""Canonical shared-graph text: visible encoding of non-text control characters.

External tool output may contain bytes that are legal in storage but illegal as
process arguments or misleading as YAML. This module does not rewrite stored
Facts. Renderers call ``encode_graph_text`` so prompts stay representable and
auditable.
"""
from __future__ import annotations

from typing import Any


GRAPH_TEXT_REPR = "graph-text-v1"
_PASSTHROUGH_CONTROLS = frozenset({"\n", "\t", "\r"})


class ContextRenderError(RuntimeError):
    """Required shared-graph context could not be produced."""


def encode_graph_text(text: Any) -> tuple[str, int]:
    """Return (visible text, encoded-control count).

    NUL and other C0/DEL controls become a six-character ``\\u00XX`` sequence so
    the stored original can still be recovered from the escape, while argv and
    YAML stay legal. Newline, tab, and carriage return are kept.
    """
    encoded = 0
    parts: list[str] = []
    for char in str(text or ""):
        code = ord(char)
        if (code < 32 and char not in _PASSTHROUGH_CONTROLS) or code == 127:
            parts.append(f"\\u{code:04x}")
            encoded += 1
        else:
            parts.append(char)
    return "".join(parts), encoded


def encode_graph_lines(text: Any) -> tuple[list[str], int]:
    body, encoded = encode_graph_text(text)
    return body.splitlines(), encoded
