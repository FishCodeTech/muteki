"""Pentest exploit-report contract: parse, completeness, identity, value codes.

Product success for pentest is an accepted report in the vulnerability-report
collection. A solver Worker writes a structured file and prints
``SUBMIT_REPORT=path``. Completeness is host-side. Reproduction and value
judgment are separate actors. This module has no shell and does not accept a
finding from the submitter's own output.
"""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from typing import Any, Mapping, TypedDict
from urllib.parse import urlparse

from muteki.solver.gate import finding_in_scope, normalize_finding_class


REPORT_ID_PREFIX = "I-repro-"

FINDING_CLASS_LABELS = {
    "sqli": "SQL 注入",
    "xss": "跨站脚本",
    "rce": "远程代码执行",
    "idor": "越权",
    "ssrf": "服务端请求伪造",
    "csrf": "跨站请求伪造",
    "lfi": "本地文件包含",
    "upload": "文件上传",
    "generic": "其他",
    "other": "其他",
}

CVSS_RATING_LABELS = {
    "critical": "严重",
    "high": "高危",
    "medium": "中危",
    "low": "低危",
}

# Unauth / auth base scores from the SRC CVSS 3.1 cheat sheet.
_CVSS_BY_CLASS = {
    "rce": (
        (9.8, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"),
        (8.8, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H"),
    ),
    "upload": (
        (9.8, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"),
        (8.8, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H"),
    ),
    "sqli": (
        (9.1, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:N"),
        (8.1, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N"),
    ),
    "ssrf": (
        (9.8, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"),
        (8.8, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H"),
    ),
    "idor": (
        (7.5, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N"),
        (6.5, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N"),
    ),
    "lfi": (
        (7.5, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N"),
        (6.5, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N"),
    ),
    "xss": (
        (6.1, "AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"),
        (6.1, "AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"),
    ),
    "csrf": (
        (6.5, "AV:N/AC:L/PR:N/UI:R/S:U/C:N/I:H/A:N"),
        (6.5, "AV:N/AC:L/PR:N/UI:R/S:U/C:N/I:H/A:N"),
    ),
    "generic": (
        (5.3, "AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N"),
        (5.3, "AV:N/AC:L/PR:L/UI:N/S:U/C:L/I:N/A:N"),
    ),
    "other": (
        (5.3, "AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N"),
        (5.3, "AV:N/AC:L/PR:L/UI:N/S:U/C:L/I:N/A:N"),
    ),
}

_UNAUTH_HINTS = (
    "unauthenticated", "unauth", "without authentication", "no authentication",
    "no login", "anonymous", "public endpoint", "未授权", "无需认证", "无需登录",
    "公开端点", "无需登陆",
)

VALUE_OK = "ok"
VALUE_REJECT_SELF_XSS = "self_xss"
VALUE_REJECT_ATTACKER_ONLY = "attacker_only"
VALUE_REJECT_INFORMATIONAL = "informational"
VALUE_REJECT_NOT_REPRODUCIBLE = "not_reproducible"
VALUE_REJECT_OUT_OF_SCOPE = "out_of_scope"
VALUE_REJECT_INCOMPLETE = "incomplete"
VALUE_REJECT_TEMPLATE = "template"
VALUE_REJECT_DUPLICATE = "duplicate"

_PLACEHOLDER_TEXT = frozenset({
    "", "-", "n/a", "na", "none", "null", "unknown", "todo", "tbd",
    "placeholder", "xxx", "redacted", "lorem ipsum", "example",
    "the attacker", "an attacker", "attacker", "self", "myself",
})

_GENERIC_TITLES = re.compile(
    r"^(a |an |the )?(vulnerability|finding|issue|bug|xss|sqli|rce)( (in|on|at) .+)?$",
    re.IGNORECASE,
)

_TEMPLATE_IMPACT = re.compile(
    r"(could potentially|may allow an attacker|steal (session )?cookies|"
    r"cross-site scripting vulnerability exists|inject malicious scripts?)",
    re.IGNORECASE,
)

_SELF_WHO = re.compile(
    r"\b(only )?(the )?(attacker|tester|myself|self|operator|pentester)\b",
    re.IGNORECASE,
)

_OTHER_WHO = re.compile(
    r"\b(other users?|another user|admin|administrator|victim|third.party|"
    r"server|database|credentials?|accounts?|session of)\b",
    re.IGNORECASE,
)

_INFO_HINTS = (
    "informational", "fingerprint", "version disclosure", "missing header",
    "banner", "directory listing", "clickjacking", "missing httponly",
)

_REFLECTED_XSS_HINTS = (
    "xss_r", "xss-r", "reflected", "reflect xss", "reflected xss",
)

_STORED_XSS_HINTS = (
    "xss_s", "xss-s", "stored", "persistent", "guestbook", "message board",
)

_EXAMPLE_CLONES = frozenset({
    "authenticated union sql injection reads users.password",
    "unique dumped substring from the response body",
    "other accounts in the users table.",
    "substring copied from your command output this session",
})

REPORT_FIELD_HELP = (
    "Write a JSON file (not a one-line finding). Required keys:\n"
    "  title, finding_class, resource_id, preconditions, affected_role,\n"
    "  impact.who, impact.what, steps (array, >=2 concrete actions),\n"
    "  replay.command (a curl/python command a stranger can run),\n"
    "  witness (unique substring copied from THIS session's command output; "
    ">= 8 chars).\n"
    "finding_class is one of: sqli, xss, rce, idor, ssrf, csrf, lfi, upload, other.\n"
    "title must name the exact parameter or sink, not 'SQLi in login'.\n"
    "steps must name the login URL, the session/cookie, the vulnerable path, "
    "the parameter, and the payload you actually sent.\n"
    "replay.command must be a copy-pasteable curl/python sequence against the "
    "in-scope origin: obtain a fresh session (extract any hidden CSRF/user_token "
    "from the login form), apply preconditions, then send the exploit request. "
    "Do not leave <session> / COOKIE placeholders; use a cookie jar.\n"
    "witness must be a unique data string copied from the exploit response "
    "(a username+hash, uid= line, or stored payload), not an HTML wrapper.\n"
    "impact.who names who besides the tester is harmed (other users, the "
    "database, the server). Reflected XSS / xss_r / self-XSS is refused. "
    "Stored XSS is acceptable only if another user would load the payload. "
    "Issues that only affect the attacker, and informational notes "
    "(headers, banners, directory listing) are refused.\n"
    "Do not copy the example JSON. Replace every field with this target's "
    "facts. One report per distinct finding_class + resource.\n"
    "When the file is complete, print ONE line:\n"
    "  SUBMIT_REPORT=<relative path from your working directory>\n"
    "FOUND_FINDING= is ignored. A report is not accepted until a different "
    "Worker reproduces it and a host-side value check passes."
)

_PENTEST_REPORT_BLOCK = (
    "## Submit a complete exploit report\n"
    f"{REPORT_FIELD_HELP}\n"
    "Field shape only (do not submit this object; every string below is fake):\n"
    "{{\n"
    '  "title": "<parameter> <class> returns <other-user data>",\n'
    '  "finding_class": "sqli|rce|xss",\n'
    '  "resource_id": "http://in-scope-host/path",\n'
    '  "preconditions": "How you authenticated, cookie names, security level.",\n'
    '  "affected_role": "Who besides you would be harmed.",\n'
    '  "impact": {{"who": "Named other users / server / database.",\n'
    '            "what": "What confidential data or control you actually got."}},\n'
    '  "steps": ["Log in at the real login URL and save the cookie.", '
    '"Open the vulnerable path, send the exact payload, save the body."],\n'
    '  "replay": {{"command": "curl -c cj -b cj -s .../login.php && curl -s -b cj \'http://in-scope-host/path?...\'"}},\n'
    '  "witness": "unique data substring from the exploit response"\n'
    "}}\n"
    "Prefer distinct, high-impact classes on this target: UNION/error SQL "
    "injection that returns other accounts, OS command injection that returns "
    "a command result, stored XSS that other users would render. Do not spend "
    "turns on reflected self-XSS.\n"
)

VERIFIER_PROMPT = (
    "You are a reproduction verifier with a FULL shell. You do NOT hunt new "
    "bugs. You do NOT copy the submitter's files as proof.\n\n"
    "{ctx}\n"
    "## Engagement goal\n{goal}\n\n"
    "## Scope / authorization — operate STRICTLY within this\n{scope}\n\n"
    "## Report to reproduce\n"
    "{report_json}\n\n"
    "Follow the steps against the live in-scope origin. Authenticate yourself "
    "using the report's preconditions (do not reuse the submitter's cookies or "
    "files as proof). If a form has a hidden token (user_token, csrf, _token), "
    "GET the form, extract the token, and send it with the POST. After login, "
    "confirm every precondition (session, role, security level) BEFORE the "
    "exploit request. If replay.command contains placeholders, replace them "
    "with a cookie jar you just created.\n"
    "Run the exploit YOURSELF (curl or python). The witness string MUST appear "
    "in YOUR command stdout/stderr (or a file you just wrote from that output).\n"
    "Print REPRODUCED= only after the exploit request, never before.\n"
    "When done, print exactly:\n"
    "  REPRODUCED=yes\n"
    "  REPRO_WITNESS=<snippet copied from YOUR output>\n"
    "or:\n"
    "  REPRODUCED=no\n"
    "  REPRO_REASON=<why it failed>\n"
    "Do not submit a flag. Do not print SUBMIT_REPORT. Stay inside Scope."
)

VALUE_JUDGE_SYSTEM = (
    "You judge whether a reproduced web vulnerability report has real impact. "
    "You have no shell. Read only the report and the verifier's command output.\n"
    "Reject when:\n"
    "- self_xss: reflected XSS or any XSS that only executes in the tester's "
    "own browser, with no other user or stored sink.\n"
    "- attacker_only: only the person sending the request is affected.\n"
    "- informational: version, headers, fingerprint, missing security header, "
    "no confidentiality/integrity/availability impact.\n"
    "- template: stock writeup with no concrete request/response.\n"
    "- out_of_scope: resource is outside the stated scope.\n"
    "Accept SQL injection that returns other users' data, OS command injection "
    "that returns a command result, stored XSS that other users would load, "
    "IDOR across identities, SSRF that hits an internal resource.\n"
    "Reply with JSON only:\n"
    '{"accept": false, "code": "self_xss", "reason": "one sentence"}'
)


def report_identity(report: Mapping[str, Any]) -> str:
    cls = normalize_finding_class(str(report.get("finding_class") or ""))
    resource = str(report.get("resource_id") or "").strip().lower()
    if resource.endswith("/") and resource.count("/") > 3:
        resource = resource.rstrip("/")
    return f"{cls}::{resource}"


def make_report_id(report: Mapping[str, Any]) -> str:
    key = report_identity(report)
    return hashlib.sha256(key.encode("utf-8")).hexdigest()[:32]


def repro_intent_id(report_id: str) -> str:
    return f"{REPORT_ID_PREFIX}{report_id}"


def report_id_from_intent(intent_id: str) -> str:
    text = str(intent_id or "")
    if text.startswith(REPORT_ID_PREFIX):
        return text[len(REPORT_ID_PREFIX):]
    return ""


def _as_text(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, str):
        return value.strip()
    return str(value).strip()


def _impact_map(raw: Any) -> dict[str, str]:
    if isinstance(raw, Mapping):
        return {
            "who": _as_text(raw.get("who") or raw.get("victim")),
            "what": _as_text(raw.get("what") or raw.get("impact") or raw.get("effect")),
        }
    text = _as_text(raw)
    return {"who": "", "what": text}


def _steps_list(raw: Any) -> list[str]:
    if isinstance(raw, str):
        parts = [p.strip(" \t-•") for p in re.split(r"[\n;]+", raw) if p.strip()]
        return [p for p in parts if p]
    if isinstance(raw, list):
        out: list[str] = []
        for item in raw:
            text = _as_text(item)
            if text:
                out.append(text)
        return out
    return []


def _replay_map(raw: Any) -> dict[str, str]:
    if isinstance(raw, str):
        return {"command": raw.strip()}
    if isinstance(raw, Mapping):
        command = _as_text(
            raw.get("command") or raw.get("curl") or raw.get("request"))
        method = _as_text(raw.get("method"))
        url = _as_text(raw.get("url") or raw.get("resource_id"))
        body = _as_text(raw.get("body") or raw.get("data"))
        if not command and method and url:
            command = f"{method.upper()} {url}"
            if body:
                command = f"{command} body={body[:400]}"
        return {
            "command": command,
            "method": method,
            "url": url,
            "body": body,
        }
    return {"command": "", "method": "", "url": "", "body": ""}


def normalize_report(raw: Mapping[str, Any] | None) -> dict[str, Any]:
    data = dict(raw or {})
    impact = _impact_map(data.get("impact"))
    replay = _replay_map(data.get("replay") or data.get("poc") or data.get("request"))
    steps = _steps_list(data.get("steps") or data.get("reproduction") or data.get("repro_steps"))
    cls = normalize_finding_class(_as_text(data.get("finding_class") or data.get("class")))
    report = {
        "title": _as_text(data.get("title") or data.get("name")),
        "finding_class": cls,
        "resource_id": _as_text(data.get("resource_id") or data.get("url") or data.get("path")),
        "preconditions": _as_text(data.get("preconditions") or data.get("auth")),
        "affected_role": _as_text(data.get("affected_role") or data.get("victim_role")),
        "impact": impact,
        "steps": steps,
        "replay": replay,
        "witness": _as_text(data.get("witness") or data.get("evidence_snippet")),
        "narrative": _as_text(data.get("narrative") or data.get("writeup") or data.get("summary")),
        "evidence_paths": [
            _as_text(p) for p in (data.get("evidence_paths") or []) if _as_text(p)
        ],
    }
    report["report_id"] = _as_text(data.get("report_id")) or make_report_id(report)
    return report


def parse_report_documents(text: str) -> list[dict[str, Any]]:
    s = (text or "").strip()
    if not s:
        return []
    fence = re.search(r"```(?:json)?\s*(\{.*\})\s*```", s, re.DOTALL)
    if fence:
        s = fence.group(1)
    brace = s.find("{")
    if brace < 0:
        return []
    try:
        parsed, _end = json.JSONDecoder().raw_decode(s[brace:])
    except json.JSONDecodeError:
        return []
    if not isinstance(parsed, dict):
        return []
    bundle = parsed.get("reports")
    if isinstance(bundle, list) and "title" not in parsed and "finding_class" not in parsed:
        out: list[dict[str, Any]] = []
        for item in bundle:
            if isinstance(item, dict):
                out.append(normalize_report(item))
        return out
    return [normalize_report(parsed)]


def parse_report_text(text: str) -> dict[str, Any] | None:
    docs = parse_report_documents(text)
    return docs[0] if docs else None


def parse_report_file(path: Path) -> dict[str, Any] | None:
    docs = parse_report_files(path)
    return docs[0] if docs else None


def parse_report_files(path: Path) -> list[dict[str, Any]]:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    return parse_report_documents(text)


def missing_report_fields(report: Mapping[str, Any]) -> list[str]:
    missing: list[str] = []
    title = _as_text(report.get("title"))
    if len(title) < 12 or title.lower() in _PLACEHOLDER_TEXT or _GENERIC_TITLES.match(title):
        missing.append("title")
    cls = _as_text(report.get("finding_class"))
    if not cls:
        missing.append("finding_class")
    resource = _as_text(report.get("resource_id"))
    if len(resource) < 4:
        missing.append("resource_id")
    pre = _as_text(report.get("preconditions"))
    if len(pre) < 8:
        missing.append("preconditions")
    role = _as_text(report.get("affected_role"))
    if len(role) < 8:
        missing.append("affected_role")
    impact = report.get("impact") if isinstance(report.get("impact"), Mapping) else {}
    who = _as_text(impact.get("who") if isinstance(impact, Mapping) else "")
    what = _as_text(impact.get("what") if isinstance(impact, Mapping) else "")
    if len(who) < 16 or who.lower() in _PLACEHOLDER_TEXT:
        missing.append("impact.who")
    if len(what) < 16 or what.lower() in _PLACEHOLDER_TEXT:
        missing.append("impact.what")
    steps = report.get("steps") if isinstance(report.get("steps"), list) else []
    usable_steps = [s for s in steps if isinstance(s, str) and len(s.strip()) >= 12]
    if len(usable_steps) < 2 or not any("/" in s for s in usable_steps):
        missing.append("steps")
    replay = report.get("replay") if isinstance(report.get("replay"), Mapping) else {}
    command = _as_text(replay.get("command") if isinstance(replay, Mapping) else "")
    if len(command) < 16 or not re.search(r"(curl|wget|python|https?://)", command, re.I):
        missing.append("replay.command")
    witness = _as_text(report.get("witness"))
    if len(witness) < 8 or witness.lower() in _PLACEHOLDER_TEXT:
        missing.append("witness")
    return missing


def report_looks_template(report: Mapping[str, Any]) -> bool:
    impact = report.get("impact") if isinstance(report.get("impact"), Mapping) else {}
    what = _as_text(impact.get("what") if isinstance(impact, Mapping) else "")
    who = _as_text(impact.get("who") if isinstance(impact, Mapping) else "")
    narrative = _as_text(report.get("narrative"))
    title = _as_text(report.get("title"))
    witness = _as_text(report.get("witness"))
    if title.lower() in _EXAMPLE_CLONES or witness.lower() in _EXAMPLE_CLONES:
        return True
    if who.lower() in _EXAMPLE_CLONES:
        return True
    blob = f"{what}\n{narrative}"
    if _TEMPLATE_IMPACT.search(blob) and len(blob) < 80:
        return True
    steps = report.get("steps") if isinstance(report.get("steps"), list) else []
    generic = 0
    for step in steps:
        low = _as_text(step).lower()
        if re.match(r"^(step\s*)?\d+[\.:)]?\s*(reproduce|exploit|send payload|visit the page)", low):
            generic += 1
    return generic >= 2 and len(steps) <= 3


def report_in_scope(report: Mapping[str, Any], *, scope: str, target: str) -> bool:
    resource = _as_text(report.get("resource_id"))
    replay = report.get("replay") if isinstance(report.get("replay"), Mapping) else {}
    url = _as_text(replay.get("url") if isinstance(replay, Mapping) else "")
    command = _as_text(replay.get("command") if isinstance(replay, Mapping) else "")
    if not finding_in_scope(resource, scope=scope, target=target):
        return False
    if url and not finding_in_scope(url, scope=scope, target=target):
        return False
    for token in re.findall(r"https?://[^\s'\"\\]+", command):
        if not finding_in_scope(token, scope=scope, target=target):
            return False
    return True


def completeness_code(report: Mapping[str, Any] | None, *, scope: str = "", target: str = "") -> str:
    if not report:
        return VALUE_REJECT_INCOMPLETE
    if missing_report_fields(report):
        return VALUE_REJECT_INCOMPLETE
    if report_looks_template(report):
        return VALUE_REJECT_TEMPLATE
    if (scope or target) and not report_in_scope(report, scope=scope, target=target):
        return VALUE_REJECT_OUT_OF_SCOPE
    return VALUE_OK


def _blob(report: Mapping[str, Any]) -> str:
    impact = report.get("impact") if isinstance(report.get("impact"), Mapping) else {}
    parts = [
        _as_text(report.get("title")),
        _as_text(report.get("finding_class")),
        _as_text(report.get("resource_id")),
        _as_text(report.get("affected_role")),
        _as_text(impact.get("who") if isinstance(impact, Mapping) else ""),
        _as_text(impact.get("what") if isinstance(impact, Mapping) else ""),
        _as_text(report.get("narrative")),
        " ".join(_as_text(s) for s in (report.get("steps") or [])),
    ]
    return "\n".join(parts).lower()


def heuristic_value_code(report: Mapping[str, Any]) -> str | None:
    """Host-side value screen. None means not rejected here (LLM may still reject)."""
    cls = normalize_finding_class(_as_text(report.get("finding_class")))
    blob = _blob(report)
    impact = report.get("impact") if isinstance(report.get("impact"), Mapping) else {}
    who = _as_text(impact.get("who") if isinstance(impact, Mapping) else "")
    what = _as_text(impact.get("what") if isinstance(impact, Mapping) else "")
    if any(h in blob for h in _INFO_HINTS) and cls in {"generic", "other", "informational", ""}:
        return VALUE_REJECT_INFORMATIONAL
    if cls == "xss":
        stored = any(h in blob for h in _STORED_XSS_HINTS)
        reflected = any(h in blob for h in _REFLECTED_XSS_HINTS)
        if reflected and not stored:
            return VALUE_REJECT_SELF_XSS
        if not stored and _SELF_WHO.search(who) and not _OTHER_WHO.search(who + " " + what):
            return VALUE_REJECT_SELF_XSS
    if _SELF_WHO.search(who) and not _OTHER_WHO.search(who + " " + what + " " + blob):
        if cls not in {"sqli", "rce", "idor", "ssrf"}:
            return VALUE_REJECT_ATTACKER_ONLY
    if report_looks_template(report):
        return VALUE_REJECT_TEMPLATE
    return None


def parse_value_judge_reply(text: str) -> tuple[bool, str, str]:
    s = (text or "").strip()
    brace = s.find("{")
    if brace < 0:
        return False, VALUE_REJECT_TEMPLATE, "value judge returned no JSON"
    try:
        parsed, _end = json.JSONDecoder().raw_decode(s[brace:])
    except json.JSONDecodeError:
        return False, VALUE_REJECT_TEMPLATE, "value judge JSON parse failed"
    if not isinstance(parsed, dict):
        return False, VALUE_REJECT_TEMPLATE, "value judge JSON was not an object"
    accept = bool(parsed.get("accept"))
    code = _as_text(parsed.get("code") or (VALUE_OK if accept else VALUE_REJECT_TEMPLATE))
    reason = _as_text(parsed.get("reason"))[:400]
    if accept:
        return True, VALUE_OK, reason
    allowed = {
        VALUE_REJECT_SELF_XSS, VALUE_REJECT_ATTACKER_ONLY, VALUE_REJECT_INFORMATIONAL,
        VALUE_REJECT_NOT_REPRODUCIBLE, VALUE_REJECT_OUT_OF_SCOPE,
        VALUE_REJECT_INCOMPLETE, VALUE_REJECT_TEMPLATE,
    }
    if code not in allowed:
        code = VALUE_REJECT_TEMPLATE
    return False, code, reason


def witness_in_corpus(witness: str, corpus: str) -> bool:
    needle = (witness or "").strip()
    hay = corpus or ""
    if len(needle) < 8 or not hay.strip():
        return False
    return needle in hay


def replay_attempted(report: Mapping[str, Any], commands: list[str]) -> bool:
    blob = "\n".join(commands or []).lower()
    if not blob.strip():
        return False
    if any(tok in blob for tok in ("curl ", "curl\t", "python", "wget ", "http://", "https://")):
        return True
    resource = _as_text(report.get("resource_id"))
    path = urlparse(resource).path if "://" in resource else resource
    path = (path or "").strip()
    if path and path not in {"/", ""} and path.lower() in blob:
        return True
    command = ""
    replay = report.get("replay")
    if isinstance(replay, Mapping):
        command = _as_text(replay.get("command")).lower()
    if command and len(command) >= 16 and command[:40] in blob:
        return True
    return False


def render_repro_intent_goal(report: Mapping[str, Any]) -> str:
    body = json.dumps(dict(report), ensure_ascii=False, indent=2)
    if len(body) > 12000:
        body = body[:12000] + "\n…"
    return (
        "REPRODUCE this vulnerability report against the live in-scope origin. "
        "Do not hunt new bugs. Follow the steps and run the replay command yourself.\n\n"
        f"{body}\n\n"
        "The witness string MUST appear in YOUR command output. "
        "Print REPRODUCED=yes and REPRO_WITNESS=<snippet from YOUR output>."
    )


def report_payload(report: Mapping[str, Any], *, extra: Mapping[str, Any] | None = None) -> dict[str, Any]:
    payload = dict(report)
    if extra:
        payload.update(dict(extra))
    return payload


def _md_text(value: Any) -> str:
    return _as_text(value).replace("\r\n", "\n")


def _md_inline(value: Any) -> str:
    return " ".join(_md_text(value).split())


def _md_or_missing(value: Any) -> str:
    text = _md_text(value)
    return text if text else "（未填写）"


def report_markdown_filename(report: Mapping[str, Any], *, index: int) -> str:
    cls = re.sub(r"[^a-z0-9]+", "", _as_text(report.get("finding_class")).lower()) or "finding"
    rid = re.sub(r"[^a-zA-Z0-9]+", "", _as_text(report.get("report_id")))[:8] or "report"
    return f"{int(index):02d}-{cls}-{rid}.md"


def reports_dir_from_graph_db(db_path: str | Path | None) -> Path | None:
    if not db_path:
        return None
    parent = Path(db_path).resolve().parent
    if parent.name == "graph":
        return parent.parent / "reports"
    return parent / "reports"


def finding_class_label(value: Any) -> str:
    cls = _as_text(value)
    if not cls:
        return "（未填写）"
    key = normalize_finding_class(cls)
    return FINDING_CLASS_LABELS.get(key, cls)


class CvssEstimate(TypedDict):
    score: float
    vector: str
    rating: str
    label: str


def cvss_rating(score: float) -> str:
    if score >= 9.0:
        return "critical"
    if score >= 7.0:
        return "high"
    if score >= 4.0:
        return "medium"
    return "low"


def _requires_privileges(report: Mapping[str, Any]) -> bool:
    blob = " ".join([
        _as_text(report.get("preconditions")),
        _as_text(report.get("title")),
        _as_text(report.get("affected_role")),
    ]).lower()
    return not any(hint in blob for hint in _UNAUTH_HINTS)


def estimate_cvss(report: Mapping[str, Any]) -> CvssEstimate:
    """Host-side CVSS 3.1 base from finding class and auth, not a worker claim."""
    data = report if isinstance(report, Mapping) else {}
    cls = normalize_finding_class(_as_text(data.get("finding_class")))
    pair = _CVSS_BY_CLASS.get(cls) or _CVSS_BY_CLASS["other"]
    score, vector = pair[1] if _requires_privileges(data) else pair[0]
    rating = cvss_rating(score)
    return {
        "score": score,
        "vector": vector,
        "rating": rating,
        "label": CVSS_RATING_LABELS[rating],
    }


def render_report_markdown(report: Mapping[str, Any]) -> str:
    """Host-owned SRC-style delivery document. Derived from gated JSON fields."""
    data = normalize_report(report)
    title = _md_inline(data.get("title")) or "未命名漏洞"
    impact = _impact_map(data.get("impact"))
    replay = _replay_map(data.get("replay"))
    steps = _steps_list(data.get("steps"))
    narrative = _md_text(data.get("narrative"))
    command = _md_text(replay.get("command")) or "# （未填写）"
    proof = _md_text(data.get("witness")) or "（未填写）"
    cls = _as_text(data.get("finding_class"))
    type_label = finding_class_label(cls)
    type_line = f"{type_label} (`{cls}`)" if cls else type_label
    cvss = estimate_cvss(data)
    who = _md_or_missing(impact.get("who"))
    what = _md_or_missing(impact.get("what"))
    lines = [
        f"# {title}",
        "",
        "## 漏洞概要",
        "",
        f"- **类型**：{type_line}",
        f"- **位置**：`{_md_inline(data.get('resource_id')) or '（未填写）'}`",
        f"- **严重程度**：{cvss['label']}（类型估算）",
        f"- **参考向量**：{cvss['score']:.1f}（`{cvss['vector']}`）",
        f"- **先决条件**：{_md_or_missing(data.get('preconditions'))}",
        f"- **影响对象**：{_md_or_missing(data.get('affected_role'))}",
    ]
    if narrative:
        lines.extend(["", narrative])
    lines.extend(["", "## 复现步骤", ""])
    if steps:
        lines.extend(f"{i}. {step}" for i, step in enumerate(steps, 1))
    else:
        lines.append("（未填写）")
    lines.extend([
        "",
        "## PoC",
        "",
        "```bash",
        command,
        "```",
        "",
        "## 证明输出",
        "",
        "```",
        proof,
        "```",
        "",
        "## 影响",
        "",
        f"{who} {what}".strip(),
        "",
    ])
    return "\n".join(lines)


def _demote_markdown_headings(markdown: str, index: int) -> str:
    lines: list[str] = []
    in_fence = False
    first_title = True
    for line in markdown.splitlines():
        stripped = line.lstrip()
        if stripped.startswith("```"):
            in_fence = not in_fence
            lines.append(line)
            continue
        if not in_fence:
            if first_title and line.startswith("# "):
                lines.append(f"## {index}. {line[2:]}")
                first_title = False
                continue
            if line.startswith("#"):
                lines.append("#" + line)
                continue
        lines.append(line)
    return "\n".join(lines)


def render_report_collection_markdown(
    reports: list[Mapping[str, Any]] | tuple[Mapping[str, Any], ...],
    *,
    title: str = "漏洞报告集",
) -> str:
    heading = _md_inline(title) or "漏洞报告集"
    rows = [dict(item) for item in reports if item]
    parts = [f"# {heading}", ""]
    if not rows:
        parts.extend(["（尚无已入库报告）", ""])
        return "\n".join(parts)
    parts.extend([f"共 {len(rows)} 份已入库报告。", ""])
    bodies: list[str] = []
    for index, report in enumerate(rows, 1):
        bodies.append(_demote_markdown_headings(render_report_markdown(report), index))
    parts.append("\n\n---\n\n".join(bodies))
    parts.append("")
    return "\n".join(parts)


def persist_report_collection(
    directory: Path,
    reports: list[Mapping[str, Any]] | tuple[Mapping[str, Any], ...],
    *,
    title: str = "漏洞报告集",
) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    name_re = re.compile(r"^\d{2}-[a-z0-9]+-[a-zA-Z0-9]+\.md$")
    for existing in directory.glob("*.md"):
        if existing.name == "COLLECTION.md" or name_re.match(existing.name):
            existing.unlink(missing_ok=True)
    rows = [dict(item) for item in reports if item]
    for index, report in enumerate(rows, 1):
        path = directory / report_markdown_filename(report, index=index)
        path.write_text(render_report_markdown(report), encoding="utf-8")
    collection = directory / "COLLECTION.md"
    collection.write_text(
        render_report_collection_markdown(rows, title=title), encoding="utf-8")
    return collection


def report_sse_fields(
    report: Mapping[str, Any],
    *,
    include_markdown: bool = False,
) -> dict[str, Any]:
    data = dict(report or {})
    impact = _impact_map(data.get("impact"))
    replay = _replay_map(data.get("replay"))
    fields: dict[str, Any] = {
        "report_id": _as_text(data.get("report_id")),
        "title": _as_text(data.get("title")),
        "finding_class": _as_text(data.get("finding_class")),
        "resource_id": _as_text(data.get("resource_id")),
        "preconditions": _as_text(data.get("preconditions")),
        "affected_role": _as_text(data.get("affected_role")),
        "impact_who": impact.get("who", ""),
        "impact_what": impact.get("what", ""),
        "witness": _as_text(data.get("witness")),
        "steps": _steps_list(data.get("steps")),
        "replay_command": replay.get("command", ""),
        "narrative": _as_text(data.get("narrative")),
        "intent_id": _as_text(data.get("intent_id")),
        "submitter": _as_text(data.get("submitter")),
    }
    if include_markdown:
        fields["markdown"] = _as_text(data.get("markdown")) or render_report_markdown(data)
        path = _as_text(data.get("markdown_path"))
        if path:
            fields["markdown_path"] = path
    return fields
