"""Task-contract Flag format helpers.

Flag acceptance does not live in this module. A model submits the exact value via
the Muteki Blackboard Skill, and the runtime records that declaration directly.
The optional flag-format fields below are prompt metadata only.
"""

from __future__ import annotations

from typing import Any, NamedTuple


TOKEN_FLAG_FORMAT = "token"
DEFAULT_BRACE_FLAG_FORMAT = "flag{...}"


class FlagFormatError(ValueError):
    """Retained API name; Flag prompt metadata is no longer executable."""


def normalize_flag_format(flag_format: Any) -> str:
    """Keep the operator's Flag shape as non-executable prompt metadata."""
    raw = str(flag_format or "").strip()
    if raw.casefold() == "brace":
        return DEFAULT_BRACE_FLAG_FORMAT
    if raw.casefold() in {"", "custom"}:
        return ""
    return raw


class NormalizedFlagContract(NamedTuple):
    """Flag-shape prompt metadata and its optional readable wrapper."""

    flag_format: str
    flag_format_wrapper: str


def normalize_flag_wrapper(flag_format_wrapper: Any) -> str:
    """Normalize the human-facing wrapper field shared by all dispatch paths."""
    wrapper = str(flag_format_wrapper or "").strip()
    if not wrapper:
        return ""
    # A wrapper is a compact human sample rather than a free-form regex.  Keep
    # the existing Web input semantics for direct/legacy callers as well.
    return "".join(wrapper.split())


def normalize_flag_contract(
    flag_format: Any,
    flag_format_wrapper: Any = "",
) -> NormalizedFlagContract:
    """Resolve Flag-shape prompt metadata without validating submitted values."""
    raw_format = str(flag_format or "").strip()
    wrapper = normalize_flag_wrapper(flag_format_wrapper)
    selector = raw_format.casefold()

    if selector == TOKEN_FLAG_FORMAT:
        return NormalizedFlagContract(TOKEN_FLAG_FORMAT, "")
    if not raw_format or selector == "custom":
        return NormalizedFlagContract(
            normalize_flag_format(wrapper if wrapper else raw_format),
            wrapper,
        )
    return NormalizedFlagContract(normalize_flag_format(raw_format), wrapper)
