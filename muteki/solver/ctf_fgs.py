"""CTF Fact-Goal-Step helpers: product/flag Findings, unanswered salvage, Decide frontier.

Finding classes are defined by the Goal, not a vulnerability taxonomy:

- ``flag`` counts toward the Goal (submit-flag).
- ``product`` is the answer a Worker published with commit-step.

Timeout salvage is an unadmitted Observation, never a Fact or Finding. It
records the unfinished question and tool references without a verdict.
"""
from __future__ import annotations

import hashlib
import re
from difflib import SequenceMatcher
from typing import Any, Optional
from urllib.parse import urlsplit

from muteki.solver.result_codes import (
    RESULT_CANCELLED,
    RESULT_EXPLORED,
    RESULT_TIMED_OUT,
)
from muteki.solver.worker_result import ObservationClaim
from muteki.swarm.graph_defs import EV_FACT_ADDED, EV_OBSERVATION_ADDED


FINDING_CLASS_FLAG = "flag"
FINDING_CLASS_PRODUCT = "product"
SALVAGE_REASONS = frozenset({"timeout", "cancel", "conclude_timeout"})
_UNANSWERED_TITLE_RE = re.compile(
    r"^UNANSWERED\((timeout|cancel|conclude_timeout)\)\s*:\s*(.*)$"
)
_URL_RE = re.compile(r"https?://[^\s'\"<>]+", re.I)
_PATH_RE = re.compile(
    r"(?:^|[\s'\"=])"
    r"(/(?:etc|var|tmp|home|proc|opt|usr|challenge|app|data|www)[^\s'\"<>]*)"
)
_SALVAGE_STATUSES = {
    RESULT_TIMED_OUT: "timeout",
    RESULT_CANCELLED: "cancel",
    RESULT_EXPLORED: "conclude_timeout",
}
def is_salvage_fact(text: str) -> bool:
    title = str(text or "").split("\n", 1)[0].strip()
    return bool(_UNANSWERED_TITLE_RE.match(title))


def salvage_question(text: str) -> str:
    title = str(text or "").split("\n", 1)[0].strip()
    match = _UNANSWERED_TITLE_RE.match(title)
    if match:
        return str(match.group(2) or "").strip()
    return title


def product_finding_payload(
    *, fact_seq: int, intent_id: str, title: str,
) -> dict[str, Any]:
    clean_title = str(title or "").split("\n", 1)[0].strip()
    return {
        "finding_class": FINDING_CLASS_PRODUCT,
        "resource_id": str(intent_id or "step").strip() or "step",
        "identity_a": str(int(fact_seq)),
        "identity_b": "",
        "title": clean_title,
        "from_fact": int(fact_seq),
        "from_step": str(intent_id or "").strip(),
    }


def flag_finding_payload(*, flag: str, intent_id: str) -> dict[str, Any]:
    digest = hashlib.sha256(str(flag or "").encode("utf-8")).hexdigest()[:16]
    return {
        "finding_class": FINDING_CLASS_FLAG,
        "resource_id": "flag",
        "identity_a": digest,
        "identity_b": "",
        "title": "flag recorded",
        "from_step": str(intent_id or "").strip(),
    }


def _step_question(solver: Any) -> str:
    goal = str(
        getattr(solver, "intent_goal", "")
        or getattr(solver, "expected_observable", "")
        or ""
    ).strip()
    return goal or "unspecified step question"


def _tool_traces(solver: Any) -> list[tuple[str, str, str]]:
    traces: list[tuple[str, str, str]] = []
    for record in list(getattr(solver, "_tool_evidence_records", None) or []):
        traces.append((
            str(getattr(record, "command", "") or ""),
            str(getattr(record, "output", "") or ""),
            str(getattr(record, "artifact_id", "") or ""),
        ))
    for pending in list(getattr(solver, "_pending_tool_calls", None) or []):
        if not isinstance(pending, dict):
            continue
        traces.append((str(pending.get("command") or ""), "", ""))
    return traces


def _target_key(value: str) -> str:
    raw = str(value or "").strip().rstrip(".,;")
    if not raw:
        return ""
    if raw.lower().startswith("http://") or raw.lower().startswith("https://"):
        parts = urlsplit(raw)
        path = parts.path or "/"
        return f"{parts.scheme}://{parts.netloc}{path}"
    return raw


def extract_targets(command: str) -> list[str]:
    found: list[str] = []
    seen: set[str] = set()
    for match in _URL_RE.findall(str(command or "")):
        key = _target_key(match)
        if key and key not in seen:
            seen.add(key)
            found.append(key)
    for match in _PATH_RE.findall(str(command or "")):
        key = _target_key(match)
        if key and key not in seen:
            seen.add(key)
            found.append(key)
    return found


def build_unanswered_fact(
    *,
    reason: str,
    question: str,
    traces: list[tuple[str, ...]],
    draft_title: str = "",
    draft_content: str = "",
) -> str:
    reason = reason if reason in SALVAGE_REASONS else "timeout"
    question = " ".join(str(question or "").split())
    targets: list[str] = []
    seen_targets: set[str] = set()
    for item in traces:
        command = str(item[0] if item else "")
        for target in extract_targets(command):
            if target in seen_targets:
                continue
            seen_targets.add(target)
            targets.append(target)
    call_lines: list[str] = []
    for item in traces:
        command = str(item[0] if item else "")
        output = str(item[1] if len(item) > 1 else "")
        artifact = str(item[2] if len(item) > 2 else "").strip()
        if not command and not output and not artifact:
            continue
        suffix = f" ({len(output)} chars)"
        if artifact:
            suffix += f" artifact:{artifact}"
        call_lines.append(f"- {command}{suffix}")
        if output and not artifact:
            call_lines.append(output)
    lines = [
        f"UNANSWERED({reason}): {question}",
        f"question: {question}",
        f"tool_calls: {len(traces)}",
        "targets:",
    ]
    if targets:
        lines.extend(f"- {item}" for item in targets)
    else:
        lines.append("- (none recorded)")
    lines.append("last_tool_calls:")
    if call_lines:
        lines.extend(call_lines)
    else:
        lines.append("- (none recorded)")
    draft_title = str(draft_title or "").strip()
    draft_content = str(draft_content or "").strip()
    if draft_title or draft_content:
        lines.append("draft (unverified):")
        if draft_title:
            lines.append(draft_title)
        if draft_content:
            lines.extend(draft_content.splitlines())
    return "\n".join(lines).strip()


def salvage_reason_for_status(status: str) -> str:
    return _SALVAGE_STATUSES.get(str(status or ""), "")


def unanswered_observation_claim(
    solver: Any, status: str,
) -> Optional[ObservationClaim]:
    """Keep an unfinished Step as an unadmitted Observation."""
    if getattr(solver, "_step_committed", False):
        return None
    if getattr(getattr(solver, "challenge", None), "mode", "ctf") != "ctf":
        return None
    reason = salvage_reason_for_status(status)
    if not reason:
        return None
    for existing in list(getattr(solver, "_pending_observations", None) or []):
        if is_salvage_fact(getattr(existing, "text", "")):
            return None
    traces = _tool_traces(solver)
    draft = dict(getattr(solver, "_draft_fact", None) or {})
    text = build_unanswered_fact(
        reason=reason,
        question=_step_question(solver),
        traces=traces,
        draft_title=str(draft.get("title") or ""),
        draft_content=str(draft.get("content") or ""),
    )
    from muteki.solver.cli_results import (
        _ctf_step_artifact_refs, _fact_claim_provenance,
    )
    return ObservationClaim(
        text=text,
        claim_verified=False,
        provenance=_fact_claim_provenance(
            solver, {"artifact_refs": _ctf_step_artifact_refs(solver)}
        ),
    )


def _salvage_lineage(shared_graph: Any) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    try:
        events = shared_graph.events()
    except Exception:
        return rows
    for event in events:
        kind = str(event.get("kind") or "")
        if kind not in {EV_FACT_ADDED, EV_OBSERVATION_ADDED}:
            continue
        payload = event.get("payload") or {}
        text = str(payload.get("fact" if kind == EV_FACT_ADDED else "text") or "")
        if not is_salvage_fact(text):
            continue
        try:
            seq = int(event.get("seq") or 0)
        except (TypeError, ValueError):
            seq = 0
        if seq <= 0:
            continue
        intent_id = str(payload.get("intent_id") or "").strip()
        sources: list[int] = []
        if intent_id:
            try:
                sources = [
                    int(row.get("seq") or 0)
                    for row in (shared_graph.intent_source_facts(intent_id) or [])
                    if int(row.get("seq") or 0) > 0
                ]
            except Exception:
                sources = []
        question = salvage_question(text)
        title = str(text or "").split("\n", 1)[0].strip()
        match = _UNANSWERED_TITLE_RE.match(title)
        rows.append({
            "seq": seq,
            "question": question,
            "from_facts": sources,
            "goal": question,
            "reason": str(match.group(1) if match else ""),
            "is_fact": kind == EV_FACT_ADDED,
        })
    return rows


def _same_question(left: str, right: str) -> bool:
    a = " ".join(str(left or "").split()).casefold()
    b = " ".join(str(right or "").split()).casefold()
    if not a or not b:
        return False
    if a[:48] in b or b[:48] in a:
        return True
    return SequenceMatcher(None, a, b).ratio() >= 0.55


def apply_ctf_decide_frontier(result: Any, shared_graph: Any) -> None:
    """Annotate salvage lineage on new Steps. Never drop or rerank a proposed Step."""
    intents = list(getattr(result, "intents", None) or [])
    if not intents or shared_graph is None:
        return
    for salvage in _salvage_lineage(shared_graph):
        if not salvage.get("is_fact"):
            # Observation ids cannot be cited as from_facts.
            continue
        seq = int(salvage.get("seq") or 0)
        if seq <= 0:
            continue
        salvage_from = {
            int(item) for item in (salvage.get("from_facts") or []) if int(item) > 0
        }
        question = str(salvage.get("question") or salvage.get("goal") or "")
        for intent in intents:
            current = [int(item) for item in (intent.from_facts or []) if int(item) > 0]
            if seq in current:
                continue
            overlap = salvage_from & set(current)
            if overlap or _same_question(intent.goal, question):
                intent.from_facts = [*current, seq]


def ctf_frontier_prompt_note() -> str:
    return ""


def cancelled_reopen_rows(shared_graph: Any) -> list[dict[str, Any]]:
    """Cancelled UNANSWERED rows used by stale-reopen checks and Decide preview."""
    rows: list[dict[str, Any]] = []
    for row in _salvage_lineage(shared_graph):
        if str(row.get("reason") or "") != "cancel":
            continue
        try:
            seq = int(row.get("seq") or 0)
        except (TypeError, ValueError):
            seq = 0
        if seq <= 0:
            continue
        rows.append({
            "seq": seq,
            "question": str(row.get("question") or row.get("goal") or ""),
        })
    return rows


def stale_reopen_from_cancelled(
    action: str,
    from_facts: list[int],
    cancelled: list[dict[str, Any]] | None,
) -> bool:
    """True when action repeats a cancelled question and from has no newer Fact."""
    cited = [int(item) for item in from_facts if int(item) > 0]
    for row in cancelled or []:
        try:
            seq = int(row.get("seq") or 0)
        except (TypeError, ValueError):
            continue
        question = str(row.get("question") or row.get("goal") or "")
        if seq <= 0 or not _same_question(action, question):
            continue
        if any(item > seq for item in cited):
            continue
        return True
    return False


def stale_reopen_without_newer_fact(
    action: str, from_facts: list[int], shared_graph: Any,
) -> bool:
    """True when this open_step repeats a cancelled question with no newer Fact."""
    return stale_reopen_from_cancelled(
        action, from_facts, cancelled_reopen_rows(shared_graph),
    )
