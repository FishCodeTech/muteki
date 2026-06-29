"""The provenance + format flag-acceptance gate (§11.2).

This is the ONE hardcoded gate that decides whether a flag the model CLAIMS is
real. It is intentionally NOT a pluggable verifier (§8): a flag counts only if it
(a) matches the challenge's flag format AND (b) is traceable to real execution
output — either it appears verbatim in the raw output, or in the content of a
saved artifact referenced by that output. The model cannot launder a hallucinated
flag through a Result dict or any other side channel.

Extracted to a standalone module so every executor (CLI workers, and historically
the code-driven solver) shares byte-identical acceptance logic instead of one
borrowing the other's method.
"""

from __future__ import annotations

import re
from typing import Any


def referenced_artifacts(text: str) -> list[str]:
    """Artifact ids referenced in `text` (e.g. 'artifact_deadbeef12')."""
    return re.findall(r"artifact[_ ]?([0-9a-f]{8,})", text)


# Inner-body tokens that mean "a flag goes here", not an actual flag. The model
# writes these in prose ("scanning pages for flag{...}", "FOUND_FLAG=<flag>") and
# a blind format-scan would otherwise grab them. Matched against the {...} body,
# case-insensitively, after stripping surrounding punctuation/whitespace.
_PLACEHOLDER_BODIES = {
    # ellipsis / underscore fills
    "...", "…", "..", ".", "____", "___", "__", "_",
    # the word "flag" itself and obvious "put a flag here" phrasings
    "flag", "the flag", "flag here", "your flag here", "your_flag_here",
    "the_flag", "flag_here", "flag_goes_here", "flaghere",
    # unambiguous template tokens (these are never real flag content)
    "uuid", "xxx", "xxxx", "xxxxx", "redacted", "redacted_flag",
    "todo", "tbd", "placeholder", "your_flag",
}
# NOTE: deliberately NOT here — words that COULD be a real flag body:
# real, value, example, sample, x, na. Rejecting those would drop genuine flags
# like flag{real} / flag{example_solved}. Placeholders are caught by the template
# tokens above + the all-punctuation / no-alphanumeric / empty-body rules.
# An angle-bracket template like <flag> / <the flag> / <...> is always a placeholder.
_ANGLE_PLACEHOLDER = re.compile(r"^<[^>]{0,30}>$")


def is_placeholder_flag(flag: str) -> bool:
    """True if `flag` is a template/placeholder the model echoed rather than a real
    recovered flag — e.g. `flag{...}`, `{uuid}`, `<flag>`, `flag{FLAG}`,
    `flag{your_flag_here}`. These are the recurring false-positive shape (run-1619
    `flag{...}`, run-0405 `{uuid}`): they match a loose flag_format and, being
    quoted from the worker's own prose, trivially satisfy the "appears in output"
    provenance check — so the gate must reject them explicitly."""
    f = (flag or "").strip()
    if not f:
        return True
    if _ANGLE_PLACEHOLDER.match(f):
        return True
    m = re.search(r"\{([^}]*)\}", f)
    # an empty / whitespace-only brace body (flag{}, flag{ }) is a placeholder
    if m is not None and not m.group(1).strip():
        return True
    # BARE braces with NO prefix — `{name}`, `{uuid}`, `{1,2,66,67,68}` — are code
    # templates / variable references the worker quoted from prose, NOT flags. Every
    # real flag in history carries a prefix (dalctf{ / HTB{ / flag{ / csawctf{ …);
    # the only prefix-less {...} ever accepted were all false positives. So a {...}
    # whose prefix (text before the first `{`) is empty is a placeholder UNLESS its
    # body already looks like a recovered flag (mixed case + digits, leet, multi-word
    # with separators) — that guard keeps the rule from ever dropping a genuine flag
    # that happens to lack a prefix.
    if m is not None and not f[:m.start()].strip():
        inner = m.group(1).strip()
        # a comma-separated set/list body — `{1,2,66,67,68}`,
        # `{127.0.0.1, localhost, 0.0.0.0, ::1}` — is a code literal the worker
        # quoted (a Python set, a BLOCKED_HOSTS list), NOT a flag. Real flags are a
        # single token; they don't render as comma-separated collections. This holds
        # even when the body has letters+digits (run-1763 fooled the looks_real
        # guard below precisely because `127`/`localhost` look "real").
        if "," in inner:
            return True
        # Bare brace bodies that look like code expressions are not flags. The
        # run-0835 false positive was the literal f-string source
        # `{out3[i:j].decode()}`: it has letters+digits, so the old "looks_real"
        # guard let it through even though the punctuation clearly comes from a
        # Python expression, not a recovered token.
        if re.search(r"[\[\]().:=+*/%$\\`'\"<>]", inner):
            return True
        looks_real = (
            len(inner) >= 8
            and bool(re.search(r"[0-9]", inner))
            and bool(re.search(r"[A-Za-z]", inner))
        )
        if not looks_real:
            return True
    body = (m.group(1) if m else f).strip().strip("`'\"<>").strip()
    low = body.lower()
    if low in _PLACEHOLDER_BODIES:
        return True
    # Truncated flag summaries such as `flag{16fc0d69-...}` / `flag{abc…}` are
    # just human shorthand for a known flag. They can pass both the loose brace
    # regex and the self-referential provenance check because the shorthand appears
    # in the worker's own prose, so reject any brace body containing ellipsis.
    if "..." in body or "…" in body:
        return True
    # all-ellipsis / all-underscore / all-dots bodies (e.g. "....", "______")
    if body and re.fullmatch(r"[.…_\-\s]+", body):
        return True
    # a body with no alphanumerics at all carries no real content
    if body and not re.search(r"[A-Za-z0-9]", body):
        return True
    return False


# Sentinel flag_format for challenges whose "flag" is a bare token, NOT a
# brace-wrapped string — e.g. a Bandit-style ladder where each level's flag IS the
# next level's password (W3lc0m3T0Gh0st), or any platform that hands back a raw
# secret. The operator sets flag_format="token" at dispatch. We CANNOT just drop the
# format check (that reopens the hallucinated-flag hole); instead the token branch
# swaps the brace-format match for a STRENGTH floor while keeping provenance +
# placeholder intact.
TOKEN_FLAG_FORMAT = "token"


def _looks_like_real_token(flag: str) -> bool:
    """A bare-token flag is acceptable only if it's a strong, deliberate secret —
    not a common word or a stray number a confused worker quoted from prose. Require
    length >= 8 and either (letters AND digits) or an explicit separator (_-.), which
    real level passwords / recovered secrets have and English words don't."""
    f = (flag or "").strip().strip("`'\"")
    if len(f) < 8:
        return False
    # shell / regex metacharacters mean this came from a COMMAND or a search
    # PATTERN the worker typed, not a recovered secret. A real bare-token flag is an
    # opaque secret (bl_<hex>, a level password) — it never contains pipes, globs,
    # redirects, quantifiers, or command separators. Reject them outright
    # (run-11550: a worker grepping `FOUND_FLAG=bl_|VERIFIED_FACT=.*L4|...` leaked the
    # grep pattern as a "token" that otherwise passed the strength floor below).
    if re.search(r"[|*?;&$()<>{}\[\]\\^!`]", f):
        return False
    has_alpha = bool(re.search(r"[A-Za-z]", f))
    has_digit = bool(re.search(r"[0-9]", f))
    has_sep = bool(re.search(r"[_\-.]", f))
    # all-whitespace / sentence-like (contains spaces) is prose, not a token
    if re.search(r"\s", f):
        return False
    return has_alpha and (has_digit or has_sep)


def flag_ok(flag: str, raw_output: str, *, flag_format: str, artifacts: Any) -> bool:
    """True iff `flag` matches the format contract AND is NOT a placeholder template
    AND is traceable to real output: present verbatim in `raw_output`, or in the
    content of an artifact referenced by it. `artifacts` is an ArtifactStore (must
    expose read_text(aid)).

    Two format contracts:
      - the usual brace `flag_format` regex (default) — `flag` must match it;
      - the `TOKEN_FLAG_FORMAT` sentinel ("token") — for bare-token challenges, the
        brace match is replaced by a strength floor (_looks_like_real_token), so a
        recovered secret like W3lc0m3T0Gh0st is accepted while a quoted common word is
        not. Provenance + placeholder checks are UNCHANGED in both modes — the moat
        (a flag must trace to real output, never laundered through prose) holds.

    The placeholder check is the fix for the recurring false positive where a worker
    that did NOT solve still gets marked solved because it mentioned `flag{...}`/
    `{uuid}` in its prose and a loose flag_format + the self-referential "appears in
    output" check let it through."""
    if flag_format == TOKEN_FLAG_FORMAT:
        if not _looks_like_real_token(flag):
            return False
    elif not re.search(flag_format, flag):
        return False
    if is_placeholder_flag(flag):
        return False
    if flag in raw_output:
        return True
    for aid in referenced_artifacts(raw_output):
        txt = (artifacts.read_text(aid) if artifacts is not None else "") or ""
        if flag in txt:
            return True
    return False
