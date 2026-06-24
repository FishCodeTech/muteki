"""Confidence-gated, server-authoritative flag submission helper.

A typed SDK helper any track's solver code can call to submit a flag over a single
HTTP POST. Two principles:
  - submit ONLY when confident — the flag must look like a flag before we ever hit
    the endpoint, so submission is never used as a brute-force oracle;
  - trust the SERVER's verdict — `correct` comes from the response, never a local
    assumption of success.

This is a tool, NOT orchestration: it does not auto-submit. A solver calls
`submit_flag(...)` only when it has a provenance-verified flag in hand.

The submit host/token default to env vars; `url` overrides the full endpoint and
`header`/`MUTEKI_SUBMIT_HEADER` overrides the auth header name, so any contest's
submission API can be targeted.

Usage:
    from muteki_kit.submit import submit_flag
    r = submit_flag("flag{...}", code="web-042")  # uses MUTEKI_SUBMIT_HOST/MUTEKI_SUBMIT_TOKEN
    if r.correct: ...
"""

from __future__ import annotations

import os
import re
from typing import Any, Optional

import requests
from pydantic import BaseModel, Field

# default flag shape used as the confidence gate — same family as the rest of the
# kit. A token must look like a flag before we ever hit the endpoint.
_FLAG_RE_DEFAULT = r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}"

# Submission endpoint conventions, all overridable per-call or via env so any
# contest API can be targeted (the defaults are just a common shape).
_DEFAULT_SUBMIT_PATH = "/api/submit"          # appended to the host when no `url` given
_DEFAULT_AUTH_HEADER = os.environ.get("MUTEKI_SUBMIT_HEADER", "Agent-Token")


class FlagSubmission(BaseModel):
    """Typed result of a submit attempt. `correct` reflects the SERVER's verdict,
    never a local assumption."""

    submitted: bool = False          # did we actually POST (vs blocked by the gate)
    correct: Optional[bool] = None   # server verdict; None = unknown/unparseable
    already_solved: bool = False
    status: int = 0                  # HTTP status
    message: str = ""                # short human-readable reason / server message
    flag: str = ""
    raw: Optional[str] = None        # raw server body (truncated), for inspection


# Token-mode sentinel: a challenge whose flag is a bare secret, not flag{...}.
# Mirrors muteki.solver.gate.TOKEN_FLAG_FORMAT so a token flag isn't blocked by the
# confidence gate before we POST it (the gate there swaps the brace regex for a
# structural strength floor).
_TOKEN_FLAG_FORMAT = "token"


def _looks_like_real_token(flag: str) -> bool:
    """A bare-token flag is a strong secret, not a stray word/sentence: no
    whitespace, has a letter AND (a digit OR a _-. separator), len >= 8. Mirrors
    muteki.solver.gate._looks_like_real_token."""
    f = (flag or "").strip().strip("`'\"")
    if len(f) < 8 or re.search(r"\s", f):
        return False
    return bool(re.search(r"[A-Za-z]", f)) and bool(re.search(r"[0-9_\-.]", f))


def _looks_like_flag(flag: str, flag_format: str) -> bool:
    if flag_format == _TOKEN_FLAG_FORMAT:
        return _looks_like_real_token(flag)
    return bool(re.search(flag_format, flag or ""))


def _interpret(body: Any) -> tuple[Optional[bool], bool, str]:
    """Best-effort read of a contest API's verdict. Returns
    (correct, already_solved, message). Conservative: unknown -> (None, ...)."""
    # JSON object is the common case
    if isinstance(body, dict):
        msg = str(body.get("message") or body.get("msg") or body.get("detail") or "")
        already = bool(body.get("already_solved") or body.get("solved") or body.get("duplicate"))
        # explicit boolean-ish fields first
        for key in ("correct", "success", "ok", "is_correct", "valid"):
            if key in body:
                return bool(body[key]), already, msg
        # status string conventions
        status = str(body.get("status") or body.get("result") or "").lower()
        if status in {"correct", "success", "accepted", "solved", "ok", "true"}:
            return True, already or status == "solved", msg or status
        if status in {"incorrect", "wrong", "fail", "failed", "rejected", "false"}:
            return False, already, msg or status
        return None, already, msg
    # plain text fallback
    text = str(body or "").strip()
    low = text.lower()
    if any(w in low for w in ("correct", "success", "accepted", "congrat", "solved")):
        return True, "solved" in low or "already" in low, text[:200]
    if any(w in low for w in ("incorrect", "wrong", "invalid", "fail")):
        return False, False, text[:200]
    return None, False, text[:200]


def submit_flag(
    flag: str,
    *,
    code: str = "",
    server: Optional[str] = None,
    token: Optional[str] = None,
    url: Optional[str] = None,
    header: Optional[str] = None,
    flag_format: str = _FLAG_RE_DEFAULT,
    force: bool = False,
    timeout: float = 15.0,
) -> FlagSubmission:
    """Submit a flag to the contest API. Confidence-gated and server-authoritative.

    - Confidence gate: unless `force=True`, the flag must match `flag_format` before
      we POST — the endpoint is not a brute-force oracle.
    - Server-authoritative: `correct` comes from the response, we never assume success.

    server/token default to the MUTEKI_SUBMIT_HOST / MUTEKI_SUBMIT_TOKEN env vars.
    `url` overrides the full endpoint; `header` (or MUTEKI_SUBMIT_HEADER) overrides the
    auth header name — so any contest's submission API can be targeted.
    """
    flag = (flag or "").strip()
    if not flag:
        return FlagSubmission(submitted=False, message="empty flag — nothing to submit", flag=flag)

    if not force and not _looks_like_flag(flag, flag_format):
        return FlagSubmission(
            submitted=False, flag=flag,
            message=(f"blocked by confidence gate: {flag!r} does not match flag_format "
                     f"{flag_format!r}. Do not use submit as a brute-force oracle; pass "
                     "force=True only if you are sure of the format."),
        )

    server = server or os.environ.get("MUTEKI_SUBMIT_HOST", "")
    token = token or os.environ.get("MUTEKI_SUBMIT_TOKEN", "")
    endpoint = url or (f"http://{server}{_DEFAULT_SUBMIT_PATH}" if server else "")
    if not endpoint:
        return FlagSubmission(
            submitted=False, flag=flag,
            message="no submit endpoint: set `server`/`url` or MUTEKI_SUBMIT_HOST env",
        )

    headers = {"Content-Type": "application/json"}
    if token:
        headers[header or _DEFAULT_AUTH_HEADER] = token
    try:
        resp = requests.post(endpoint, json={"code": code, "flag": flag},
                             headers=headers, timeout=timeout)
    except requests.RequestException as e:
        return FlagSubmission(submitted=False, flag=flag,
                              message=f"submit request failed: {e}")

    try:
        body: Any = resp.json()
    except ValueError:
        body = resp.text
    correct, already, msg = _interpret(body)
    return FlagSubmission(
        submitted=True, correct=correct, already_solved=already,
        status=resp.status_code, message=msg or f"HTTP {resp.status_code}",
        flag=flag, raw=(resp.text or "")[:500],
    )
