"""Pure CLI stream helpers shared by solver execution paths."""
from __future__ import annotations

import re
from pathlib import Path

_IDLE_REPEAT_LIMIT = 5

_IDLE_REPEAT_STEER = (
    "你在重复读取黑板且内容无变化，停止重复读取，按当前 intent 目标执行实际操作"
)


def _is_reasoning_replay(accumulated: str, incoming: str) -> bool:
    acc = (accumulated or "").strip()
    text = (incoming or "").strip()
    if not acc or not text:
        return False
    if text == acc:
        return True
    return len(text) >= len(acc) and text.startswith(acc)


_WORKER_PATH_PREFIX = (
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/Applications/Wireshark.app/Contents/MacOS",
)

_REPO_BLACKBOARD_SCRIPT = (
    Path(__file__).resolve().parent.parent.parent
    / "skills" / "muteki-blackboard" / "blackboard.py"
)

_LOCKOUT_RE = re.compile(
    r"(?:lock(?:ed|out)?|cooldown|wait|try again|rate.?limit|too many|burn)\D{0,40}?"
    r"(\d+(?:\.\d+)?)\s*(seconds?|secs?|s|minutes?|mins?|m|hours?|hrs?|h)\b",
    re.IGNORECASE)

_VERIFIER_VERDICT_RE = re.compile(
    r"burn-?lock(?:out)?\s*[:\-]|"
    r"\d+\s*burns?\s+in\s+(?:the\s+)?last|"
    r"attempts?\s+(?:left|remaining)|"
    r"too\s+many\s+(?:wrong\s+)?attempts?|"
    r"locked\s+for\s+\d|"
    r"wait\s+(?:for\s+)?(?:the\s+)?cooldown",
    re.IGNORECASE)

_DOC_READ_RE = re.compile(
    r"(?:^|\n)\s*read:\s|"
    r"PROBLEM_verifier|BRIEFING|known_intel|missions?\.json|"
    r"\.md\b|DESIGN_|SOP_",
    re.IGNORECASE)

_VERIFIER_INVOKE_RE = re.compile(
    r"(?:specter-verify|verify-[a-z0-9-]+\.sh|/opt/verify-)", re.IGNORECASE)


def _parse_lockout_seconds(text: str) -> float:
    best = 0.0
    for match in _LOCKOUT_RE.finditer(text or ""):
        try:
            value = float(match.group(1))
        except (TypeError, ValueError):
            continue
        unit = (match.group(2) or "s").lower().rstrip("s")
        if unit in ("minute", "min", "m"):
            value *= 60
        elif unit in ("hour", "hr", "h"):
            value *= 3600
        best = max(best, value)
    return best


def _looks_like_verifier_output(text: str) -> bool:
    body = text or ""
    return bool(
        _VERIFIER_VERDICT_RE.search(body)
        and _VERIFIER_INVOKE_RE.search(body)
        and not _DOC_READ_RE.search(body)
    )


@staticmethod
def _fact_witnessed_in_chunk(fact: str, text: str) -> bool:
    fact = (fact or "").strip()
    raw = (text or "").strip().lower()
    if not fact or not raw:
        return False
    fact_l = fact.lower()
    if fact_l in raw:
        return True
    tokens = [
        token for token in re.findall(r"[a-z0-9_./:-]{4,}", fact_l)
        if token not in {
            "http", "https", "true", "false", "with", "from", "that",
            "this", "there", "have", "confirmed",
        }
    ]
    if not tokens:
        return False
    hits = sum(1 for token in dict.fromkeys(tokens) if token in raw)
    needed = max(2, int(len(set(tokens)) * 0.6 + 0.5))
    return hits >= needed


__all__ = [
    '_IDLE_REPEAT_LIMIT',
    '_IDLE_REPEAT_STEER',
    '_is_reasoning_replay',
    '_WORKER_PATH_PREFIX',
    '_REPO_BLACKBOARD_SCRIPT',
    '_LOCKOUT_RE',
    '_parse_lockout_seconds',
    '_VERIFIER_VERDICT_RE',
    '_DOC_READ_RE',
    '_VERIFIER_INVOKE_RE',
    '_looks_like_verifier_output',
    '_fact_witnessed_in_chunk',
]
