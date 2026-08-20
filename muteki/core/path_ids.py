"""Injective filesystem encoding for externally supplied run identifiers."""

from __future__ import annotations

import base64
import re


_PLAIN = re.compile(r"[A-Za-z0-9_-]+\Z")
_ENCODED_PREFIX = "~"
_MAX_COMPONENT_BYTES = 180


class RunIdPathError(ValueError):
    """A run id cannot be represented within portable filesystem limits."""


def encode_run_id(run_id: str) -> str:
    """Return one safe path component without collapsing distinct identifiers.

    Common UUID/run-N identifiers keep their historical spelling. Everything else
    is UTF-8 URL-safe base64 with a reserved prefix, so separators, dots, Unicode,
    and punctuation cannot collide through lossy replacement.
    """
    value = str(run_id)
    if not value:
        raise RunIdPathError("run_id cannot be empty")
    if _PLAIN.fullmatch(value):
        component = value
    else:
        encoded = base64.urlsafe_b64encode(value.encode("utf-8")).decode("ascii")
        component = _ENCODED_PREFIX + encoded.rstrip("=")
    if len(component.encode("ascii")) > _MAX_COMPONENT_BYTES:
        raise RunIdPathError("run_id is too long for local storage")
    return component


def decode_run_id(component: str) -> str:
    """Inverse of :func:`encode_run_id`; legacy plain components pass through."""
    value = str(component)
    if not value.startswith(_ENCODED_PREFIX):
        return value
    raw = value[len(_ENCODED_PREFIX):]
    try:
        padded = raw + "=" * (-len(raw) % 4)
        decoded = base64.urlsafe_b64decode(padded.encode("ascii")).decode("utf-8")
    except (ValueError, UnicodeDecodeError, RunIdPathError):
        return value
    return decoded if encode_run_id(decoded) == value else value
