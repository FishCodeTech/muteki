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

import json
import re
from typing import Any, Mapping, NamedTuple
from urllib.parse import unquote, urlsplit


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


# ── pentest finding gate (hardcoded, not pluggable; same module as flag_ok) ──

IDOR_FINDING_CLASSES = frozenset({
    "idor",
    "bac",
    "bola",
    "broken_access",
    "broken_object_level_authorization",
    "privilege",
    "priv",
})

RCE_FINDING_CLASSES = frozenset({
    "rce",
    "command_injection",
    "cmdi",
    "os_command",
    "os_command_injection",
})

SQLI_FINDING_CLASSES = frozenset({
    "sqli", "sql_injection", "sql-injection", "sqlinjection",
})

XSS_FINDING_CLASSES = frozenset({
    "xss", "cross_site_scripting", "cross-site-scripting",
})

SSRF_FINDING_CLASSES = frozenset({
    "ssrf", "server_side_request_forgery", "server-side-request-forgery",
})

_IDOR_CLASS_HINTS = (
    "idor", "bola", "broken access", "broken_access", "insecure direct",
    "越权", "水平越权", "垂直越权", "未授权",
)

_RCE_CLASS_HINTS = (
    "rce", "远程代码", "命令注入", "command injection", "os command",
)

_SQLI_CLASS_HINTS = (
    "sqli", "sql注入", "sql injection",
)

_XSS_CLASS_HINTS = (
    "xss", "跨站脚本", "cross-site scripting", "cross site scripting",
)

_SSRF_CLASS_HINTS = (
    "ssrf", "服务端请求伪造", "server-side request forgery",
)

_PLACEHOLDER_IDENTITY = frozenset({
    "", "-", "n/a", "na", "none", "null", "unknown", "todo", "tbd",
    "placeholder", "xxx", "xxxx", "redacted", "身份",
})

_NON_EVIDENCE_PREFIXES = (
    "VERIFIED_FACT=",
    "DEADEND=",
    "DEAD_END=",
    "FOUND_FLAG=",
    "FOUND_FINDING=",
    "SUBMIT_REPORT=",
    "REPRODUCED=",
    "NEED_INPUT=",
    "NEED_KIND=",
    "POC_SAVE=",
    "READY_TO_SUBMIT=",
    "ALL_FLAGS_FOUND=",
)

_VERBAL_IDOR = re.compile(
    r"^(this is |这是|it is )?(an? )?(idor|越权|broken access|bola)\b",
    re.IGNORECASE,
)

_VERBAL_RCE = re.compile(
    r"^(this is |这是|it is )?(an? )?(rce|远程代码|命令注入|command injection)\b",
    re.IGNORECASE,
)

_BENIGN_NET_MARKERS = (
    "ttl=",
    "bytes from",
    "icmp_seq",
    "packets transmitted",
    "packet loss",
    "icmp echo",
)

_FINDING_FIELD_ALIASES = {
    "class": "finding_class",
    "finding_class": "finding_class",
    "identity_a": "identity_a",
    "identity_b": "identity_b",
    "id_a": "identity_a",
    "id_b": "identity_b",
    "principal_a": "identity_a",
    "principal_b": "identity_b",
    "resource": "resource_id",
    "resource_id": "resource_id",
    "auth_a": "auth_result_a",
    "auth_b": "auth_result_b",
    "auth_result_a": "auth_result_a",
    "auth_result_b": "auth_result_b",
    "input": "input",
    "trigger": "input",
    "param": "input",
    "witness": "witness",
    "output": "witness",
    "stdout": "witness",
}

_FINDING_KEYS = (
    "finding_class", "identity_a", "identity_b",
    "resource_id", "auth_result_a", "auth_result_b",
    "input", "witness",
)


def normalize_finding_class(value: str) -> str:
    raw = (value or "").strip()
    low = raw.lower().replace(" ", "_")
    compact = low.replace("-", "_")
    if compact in IDOR_FINDING_CLASSES or any(h in raw or h in value.lower() for h in _IDOR_CLASS_HINTS):
        return "idor"
    if compact in RCE_FINDING_CLASSES or any(h in raw or h in value.lower() for h in _RCE_CLASS_HINTS):
        return "rce"
    if compact in SQLI_FINDING_CLASSES or any(h in raw or h in value.lower() for h in _SQLI_CLASS_HINTS):
        return "sqli"
    if compact in XSS_FINDING_CLASSES or any(h in raw or h in value.lower() for h in _XSS_CLASS_HINTS):
        return "xss"
    if compact in SSRF_FINDING_CLASSES or any(h in raw or h in value.lower() for h in _SSRF_CLASS_HINTS):
        return "ssrf"
    if compact in {"generic", "vuln", "vulnerability", ""}:
        return "generic"
    return value.strip().lower()


def finding_key(finding: Mapping[str, Any]) -> str:
    cls = normalize_finding_class(str(finding.get("finding_class") or ""))
    resource = str(finding.get("resource_id") or "").strip()
    if cls == "idor":
        a = str(finding.get("identity_a") or "").strip()
        b = str(finding.get("identity_b") or "").strip()
        ids = tuple(sorted((a, b), key=str.casefold))
        return f"{cls}::{resource}::{ids[0]}::{ids[1]}"
    inp = str(finding.get("input") or "").strip()
    return f"{cls}::{resource}::{inp}"


def _parse_finding_json_object(raw: str) -> dict[str, Any] | None:
    """First JSON object in `raw`, allowing trailing prose on the same line."""
    s = (raw or "").strip()
    brace = s.find("{")
    if brace < 0:
        return None
    try:
        parsed, _end = json.JSONDecoder().raw_decode(s[brace:])
    except json.JSONDecodeError:
        return None
    if not isinstance(parsed, dict):
        return None
    return parsed


def _line_is_finding_claim_json(line: str) -> bool:
    """True when the line is the claim object itself, not request/response output."""
    obj = _parse_finding_json_object(line)
    if not obj:
        return False
    return "finding_class" in obj or "class" in obj


def parse_finding_claim(raw: str) -> dict[str, str] | None:
    """Parse a FOUND_FINDING= tail (JSON object or compact key=value)."""
    s = (raw or "").strip().strip("`").strip()
    if not s or s.upper() in {"NONE", "NULL"}:
        return None
    data: dict[str, Any]
    json_obj = _parse_finding_json_object(s)
    if json_obj is not None:
        data = json_obj
    elif s.startswith("{"):
        return None
    else:
        data = {}
        for part in re.split(r"\s+(?=[A-Za-z_][A-Za-z0-9_]*=)", s):
            if "=" not in part:
                if "finding_class" not in data and part.strip():
                    data["finding_class"] = part.strip()
                continue
            k, v = part.split("=", 1)
            data[k.strip()] = v.strip().strip("`'\"")
    out: dict[str, str] = {k: "" for k in _FINDING_KEYS}
    for k, v in data.items():
        canon = _FINDING_FIELD_ALIASES.get(str(k).strip().lower())
        if canon:
            out[canon] = str(v).strip()
    if not any(out.values()):
        return None
    if out["finding_class"]:
        out["finding_class"] = normalize_finding_class(out["finding_class"])
    return out


def _strip_non_evidence(text: str) -> str:
    """Drop marker lines, Reason complete_why, and Review summaries from a corpus.

    Those texts are claims/planning, not request/response evidence."""
    kept: list[str] = []
    for line in (text or "").splitlines():
        s = line.strip()
        if any(s.startswith(p) for p in _NON_EVIDENCE_PREFIXES):
            continue
        if _line_is_finding_claim_json(s):
            continue
        low = s.lower()
        if low.startswith("complete_why") or low.startswith('"complete_why"'):
            continue
        if low.startswith("review ") or low.startswith("## review"):
            continue
        kept.append(line)
    return "\n".join(kept)


def _artifact_is_non_evidence(text: str) -> bool:
    body = _strip_non_evidence(text)
    if not body.strip():
        return True
    # A dump that is only the worker restating the claim is not a request/response.
    compact = " ".join(body.split()).lower()
    if _VERBAL_IDOR.match(compact):
        return True
    return False


def finding_evidence_corpus(raw_output: str, artifacts: Any) -> str:
    """flag_ok analog: stripped raw_output plus referenced artifact bodies.

    VERIFIED_FACT lines, Reason complete_why, and Review summaries are excluded."""
    parts = [_strip_non_evidence(raw_output or "")]
    for aid in referenced_artifacts(raw_output or ""):
        txt = (artifacts.read_text(aid) if artifacts is not None else "") or ""
        if _artifact_is_non_evidence(txt):
            continue
        parts.append(_strip_non_evidence(txt))
    return "\n".join(parts)


def _is_placeholder_identity(value: str) -> bool:
    return value.strip().casefold() in _PLACEHOLDER_IDENTITY


def _both_identities_issued_requests(identity_a: str, identity_b: str, corpus: str) -> bool:
    """True when both principals appear as request session witnesses.

    Horizontal IDOR often returns the same 200 body for both identities.
    Identical auth_result strings are allowed only if both sessions actually
    issued a request in the evidence corpus.
    """
    a = (identity_a or "").strip()
    b = (identity_b or "").strip()
    if not a or not b:
        return False
    markers = (
        (f"sess={a}", f"sess={b}"),
        (f'"user": "{a}"', f'"user": "{b}"'),
        (f'"user":"{a}"', f'"user":"{b}"'),
    )
    return any(left in corpus and right in corpus for left, right in markers)


def _looks_like_benign_net_tool(witness: str) -> bool:
    w = (witness or "").lower()
    hits = sum(1 for marker in _BENIGN_NET_MARKERS if marker in w)
    return hits >= 2


def _idor_ok(claim: dict[str, str], corpus: str) -> bool:
    identity_a = (claim.get("identity_a") or "").strip()
    identity_b = (claim.get("identity_b") or "").strip()
    resource = (claim.get("resource_id") or "").strip()
    auth_a = (claim.get("auth_result_a") or "").strip()
    auth_b = (claim.get("auth_result_b") or "").strip()
    if _is_placeholder_identity(identity_a) or _is_placeholder_identity(identity_b):
        return False
    if identity_a.casefold() == identity_b.casefold():
        return False
    if not resource or not auth_a or not auth_b:
        return False
    if _VERBAL_IDOR.match(identity_a) or _VERBAL_IDOR.match(identity_b):
        return False
    if _VERBAL_IDOR.match(resource) or _VERBAL_IDOR.match(auth_a) or _VERBAL_IDOR.match(auth_b):
        return False
    for field in (identity_a, identity_b, resource, auth_a, auth_b):
        if field not in corpus:
            return False
    if auth_a == auth_b:
        if not _both_identities_issued_requests(identity_a, identity_b, corpus):
            return False
    return True


_HTTP_METHOD_PREFIX = re.compile(
    r"^(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)\s+", re.IGNORECASE)


def _rce_field_in_corpus(field: str, corpus: str) -> bool:
    """True when `field` is a captured request/response fragment.

    Workers often wrap a real captured URL as `GET /path?q=...` or paste a
    JSON envelope around stdout. Those wrappers are not evidence; the inner
    captured fragment still must appear verbatim in the corpus. CTF flag_ok
    does not use this helper.
    """
    if not field:
        return False
    if field in corpus:
        return True
    rest = _HTTP_METHOD_PREFIX.sub("", field, count=1)
    if rest != field and rest in corpus:
        return True
    obj = _parse_finding_json_object(field)
    if obj:
        for key in ("stdout", "output", "witness"):
            val = obj.get(key)
            if isinstance(val, str) and val.strip() and val.strip() in corpus:
                return True
    m = re.search(r"[?&]hostname=([^&\s]+)", field, re.IGNORECASE)
    if not m:
        m = re.search(r"hostname=([^&\s]+)", field, re.IGNORECASE)
    if m:
        raw = m.group(1)
        if raw in corpus:
            return True
        decoded = unquote(raw.replace("+", " "))
        if decoded and decoded in corpus:
            return True
    return False


def _rce_ok(claim: dict[str, str], corpus: str) -> bool:
    resource = (claim.get("resource_id") or "").strip()
    trigger = (claim.get("input") or "").strip()
    witness = (claim.get("witness") or "").strip()
    if _is_placeholder_identity(resource) or _is_placeholder_identity(trigger):
        return False
    if _is_placeholder_identity(witness):
        return False
    if not resource or not trigger or not witness:
        return False
    if _VERBAL_RCE.match(resource) or _VERBAL_RCE.match(trigger) or _VERBAL_RCE.match(witness):
        return False
    if trigger.casefold() == witness.casefold():
        return False
    if _looks_like_benign_net_tool(witness):
        return False
    obj = _parse_finding_json_object(witness)
    if obj and isinstance(obj.get("stdout"), str):
        stdout = obj["stdout"].strip()
        if stdout and _looks_like_benign_net_tool(stdout):
            return False
    if not _rce_field_in_corpus(resource, corpus):
        return False
    if not _rce_field_in_corpus(trigger, corpus):
        return False
    if not _rce_field_in_corpus(witness, corpus):
        return False
    return True


def _generic_ok(claim: dict[str, str], corpus: str) -> bool:
    resource = (claim.get("resource_id") or "").strip()
    witness = (claim.get("witness") or claim.get("auth_result_a") or "").strip()
    trigger = (claim.get("input") or "").strip()
    if not resource:
        return False
    if not witness and not trigger:
        return False
    filled = [f for f in (resource, witness, trigger) if f]
    for field in filled:
        if field not in corpus:
            return False
    return True


_SCOPE_URL = re.compile(r"https?://[^\s\"'<>]+", re.IGNORECASE)
_SCOPE_HOST = re.compile(
    r"(?:\*\.)?(?:localhost|\[::1\]|(?:\d{1,3}\.){3}\d{1,3}|(?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,})(?::\d{1,5})?",
    re.IGNORECASE,
)
_LOOPBACK_HOSTS = frozenset({"localhost", "127.0.0.1", "::1", "[::1]"})


class ScopeOrigin(NamedTuple):
    host: str
    port: int | None
    path_prefix: str


def _norm_host(host: str) -> str:
    h = (host or "").strip().lower().rstrip(".")
    if h.startswith("[") and h.endswith("]"):
        h = h[1:-1]
    return h


def _hosts_match(left: str, right: str) -> bool:
    a = _norm_host(left)
    b = _norm_host(right)
    if not a or not b:
        return False
    if a == b:
        return True
    if a in _LOOPBACK_HOSTS and b in _LOOPBACK_HOSTS:
        return True
    if a.startswith("*."):
        suffix = a[2:]
        return b == suffix or b.endswith("." + suffix)
    if b.startswith("*."):
        suffix = b[2:]
        return a == suffix or a.endswith("." + suffix)
    return False


_ORIGIN_URL_CUT = re.compile(r"[）】」』，。；、（【「『<>\"'\s]")


def _origin_from_url(raw: str) -> ScopeOrigin | None:
    text = (raw or "").strip()
    cut = _ORIGIN_URL_CUT.search(text)
    if cut:
        text = text[:cut.start()]
    text = text.rstrip(".,;)")
    if not text:
        return None
    parsed = urlsplit(text if "://" in text else f"http://{text}")
    host = _norm_host(parsed.hostname or "")
    if not host:
        return None
    try:
        port = parsed.port
    except ValueError:
        hostport = parsed.netloc.rsplit("@", 1)[-1]
        if ":" in hostport and not hostport.startswith("["):
            maybe = hostport.rsplit(":", 1)[-1]
            digits = re.match(r"(\d{1,5})", maybe or "")
            port = int(digits.group(1)) if digits else None
        else:
            port = None
    if port is None and parsed.scheme == "https":
        port = 443
    elif port is None and parsed.scheme == "http":
        port = 80
    path = parsed.path or "/"
    if not path.startswith("/"):
        path = "/" + path
    if path != "/" and path.endswith("/"):
        path = path.rstrip("/") or "/"
    return ScopeOrigin(host=host, port=port, path_prefix=path)


def parse_engagement_scope(scope: str, target: str = "") -> tuple[ScopeOrigin, ...]:
    """Host/path allowlist from free-text scope plus the challenge target.

    Empty parse result means no extra constraint (keep current finding_ok).
    """
    found: list[ScopeOrigin] = []
    seen: set[tuple[str, int | None, str]] = set()

    def add(origin: ScopeOrigin | None) -> None:
        if origin is None:
            return
        key = (origin.host, origin.port, origin.path_prefix)
        if key in seen:
            return
        seen.add(key)
        found.append(origin)

    add(_origin_from_url(target))
    text = scope or ""
    for match in _SCOPE_URL.finditer(text):
        add(_origin_from_url(match.group(0)))
    consumed = {m.group(0) for m in _SCOPE_URL.finditer(text)}
    remainder = text
    for blob in consumed:
        remainder = remainder.replace(blob, " ")
    for match in _SCOPE_HOST.finditer(remainder):
        token = match.group(0)
        if token.startswith("*."):
            host = token[2:]
            port = None
            if ":" in host and host.rsplit(":", 1)[-1].isdigit():
                host, port_s = host.rsplit(":", 1)
                port = int(port_s)
            add(ScopeOrigin(host="*." + _norm_host(host), port=port, path_prefix="/"))
        else:
            add(_origin_from_url(token))
    return tuple(found)


def _resource_origin(resource_id: str, target: str = "") -> ScopeOrigin | None:
    raw = _HTTP_METHOD_PREFIX.sub("", (resource_id or "").strip(), count=1).strip()
    if not raw:
        return None
    if "://" in raw or _SCOPE_HOST.match(raw.split("/", 1)[0] or ""):
        return _origin_from_url(raw)
    if raw.startswith("/"):
        base = _origin_from_url(target)
        if base is None:
            return None
        path = raw.split("?", 1)[0]
        if path != "/" and path.endswith("/"):
            path = path.rstrip("/") or "/"
        return ScopeOrigin(host=base.host, port=base.port, path_prefix=path)
    return _origin_from_url(raw)


def _path_allowed(resource_path: str, prefix: str) -> bool:
    if not prefix or prefix == "/":
        return True
    path = resource_path or "/"
    if path != "/" and path.endswith("/"):
        path = path.rstrip("/") or "/"
    return path == prefix or path.startswith(prefix.rstrip("/") + "/")


def finding_in_scope(resource_id: str, scope: str = "", target: str = "") -> bool:
    """True when resource_id is inside the parsed host/path allowlist.

    No parseable allowlist → True (no extra constraint).
    Unparseable resource against a non-empty allowlist → False.
    """
    allow = parse_engagement_scope(scope, target)
    if not allow:
        return True
    origin = _resource_origin(resource_id, target)
    if origin is None:
        return False
    for entry in allow:
        if not _hosts_match(origin.host, entry.host):
            continue
        if entry.port is not None and origin.port is not None and entry.port != origin.port:
            continue
        if _path_allowed(origin.path_prefix, entry.path_prefix):
            return True
    return False


def finding_ok(
    finding: Mapping[str, Any] | str,
    raw_output: str,
    *,
    expected_class: str = "",
    artifacts: Any = None,
    scope: str = "",
    target: str = "",
) -> bool:
    """True iff the claimed finding matches the type contract AND its evidence
    fields appear verbatim in real request/response output. Hardcoded.

    idor: two identities, one resource, two authorization-result snippets.
    rce: resource, request input fragment, response witness (not ping stats).
    generic: resource plus input or witness, all filled fields in the corpus.
    Marker lines cannot satisfy provenance.
    Non-empty parseable scope rejects a resource_id outside the allowlist."""
    claim: dict[str, str] | None
    if isinstance(finding, str):
        claim = parse_finding_claim(finding)
    else:
        claim = parse_finding_claim(json.dumps(dict(finding), ensure_ascii=False))
        if claim is None and isinstance(finding, Mapping):
            claim = {k: str(finding.get(k) or "").strip() for k in _FINDING_KEYS}
            if claim["finding_class"]:
                claim["finding_class"] = normalize_finding_class(claim["finding_class"])
    if not claim:
        return False
    cls = claim.get("finding_class") or ""
    expected = normalize_finding_class(expected_class) if expected_class else ""
    if expected and expected not in {"generic", ""}:
        if expected != cls:
            return False
    if not finding_in_scope(claim.get("resource_id") or "", scope=scope, target=target):
        return False
    corpus = finding_evidence_corpus(raw_output, artifacts)
    if not corpus.strip():
        return False
    if cls == "idor":
        return _idor_ok(claim, corpus)
    if cls == "rce":
        return _rce_ok(claim, corpus)
    if not cls:
        return False
    return _generic_ok(claim, corpus)
