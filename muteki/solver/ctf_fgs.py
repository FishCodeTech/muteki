"""CTF Fact-Goal-Step helpers: product/flag Findings, unanswered salvage, Decide frontier.

Finding classes are defined by the Goal, not a vulnerability taxonomy:

- ``flag`` counts toward the Goal (submit-flag).
- ``product`` is the answer a Worker published with commit-step.

Timeout salvage is an unadmitted Observation, never a Fact or Finding. It
records the unfinished question and tool references without a verdict.
"""
from __future__ import annotations

import hashlib
from pathlib import Path
import re
from typing import Any, Optional
from urllib.parse import urlsplit

from muteki.solver.result_codes import (
    RESULT_CANCELLED,
    RESULT_EXPLORED,
    RESULT_TIMED_OUT,
)
from muteki.solver.worker_result import ObservationClaim
from muteki.swarm.graph_defs import EV_DEAD_END, EV_FACT_ADDED, EV_OBSERVATION_ADDED


FINDING_CLASS_FLAG = "flag"
FINDING_CLASS_PRODUCT = "product"
SALVAGE_REASONS = frozenset(
    {"timeout", "cancel", "conclude_timeout", "step_tool_cap", "runtime_error"}
)
_UNANSWERED_TITLE_RE = re.compile(
    r"^UNANSWERED\((timeout|cancel|conclude_timeout|step_tool_cap|runtime_error)\)\s*:\s*(.*)$"
)
_TOOL_CALLS_RE = re.compile(r"(?m)^tool_calls:\s*(\d+)\s*$")
_URL_RE = re.compile(r"https?://[^\s'\"<>]+", re.I)
_PATH_RE = re.compile(
    r"(?:^|[\s'\"=])"
    r"(/(?:etc|var|tmp|home|proc|opt|usr|challenge|app|data|www)[^\s'\"<>]*)"
)
_SALVAGE_STATUSES = {
    RESULT_TIMED_OUT: "timeout",
    RESULT_CANCELLED: "cancel",
    RESULT_EXPLORED: "conclude_timeout",
    "error": "runtime_error",
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
    total_tool_calls: Optional[int] = None,
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
            call_lines.append(f"- artifact:{artifact}{suffix}")
        else:
            call_lines.append(f"- {command}{suffix}")
            if output:
                call_lines.append(output)
    recorded_calls = (
        max(len(traces), int(total_tool_calls))
        if total_tool_calls is not None
        else len(traces)
    )
    lines = [
        f"UNANSWERED({reason}): {question}",
        f"tool_calls: {recorded_calls}",
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


def _shared_step_file_excerpts(solver: Any) -> list[str]:
    total_calls = int(getattr(solver, "_tool_evidence_total_count", 0) or 0)
    if total_calls <= 0 and not _tool_traces(solver):
        return []
    workdir_fn = getattr(solver, "_workdir_path", None)
    wd = workdir_fn() if callable(workdir_fn) else None
    if wd is None:
        raw_wd = getattr(solver, "_current_workdir", None) or getattr(solver, "_workdir", None)
        wd = Path(str(raw_wd)).resolve() if raw_wd else None
    if wd is None:
        return []
    shared_dir = wd / "shared"
    if not shared_dir.exists():
        return []
    intent_id = str(
        getattr(solver, "intent_id_assigned", "")
        or getattr(solver, "_intent_id", "")
        or ""
    ).strip()
    started_at = float(getattr(solver, "_worker_started_at", 0.0) or 0.0)
    candidates: list[tuple[int, float, Path]] = []
    try:
        for path in shared_dir.rglob("*"):
            if not path.is_file() or path.name.startswith("."):
                continue
            try:
                st = path.stat()
            except OSError:
                continue
            if st.st_size <= 0 or st.st_size > 512 * 1024:
                continue
            rel_str = str(path.relative_to(shared_dir))
            matches_step = bool(intent_id and intent_id.lower() in rel_str.lower())
            is_recent = bool(started_at > 0 and st.st_mtime >= started_at - 1.0)
            if not matches_step and not is_recent:
                continue
            priority = 0 if matches_step else 1
            candidates.append((priority, -float(st.st_mtime), path))
    except OSError:
        return []
    excerpts: list[str] = []
    for _prio, _neg_mtime, path in sorted(candidates)[:3]:
        try:
            raw = path.read_text(encoding="utf-8", errors="replace").strip()
        except OSError:
            continue
        if not raw or "\x00" in raw:
            continue
        rel_str = f"shared/{path.relative_to(shared_dir)}"
        snippet_lines = raw.splitlines()[:25]
        snippet = "\n".join(snippet_lines)[:1200]
        excerpts.append(f"- {rel_str}:\n{snippet}")
    return excerpts


def unanswered_observation_claim(
    solver: Any, status: str,
) -> Optional[ObservationClaim]:
    """In CTF mode, raw tool outputs remain in CAS artifacts and SQLite events;
    never inject UNANSWERED salvage ObservationClaims into the FGS graph."""
    del solver, status
    return None


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
        tc_match = _TOOL_CALLS_RE.search(text)
        tool_calls = int(tc_match.group(1)) if tc_match else None
        prov = payload.get("evidence_provenance") or payload.get("provenance") or {}
        target_epoch = str(
            prov.get("target_epoch") or payload.get("target_epoch") or ""
        ).strip()
        rows.append({
            "seq": seq,
            "question": question,
            "from_facts": sources,
            "goal": question,
            "reason": str(match.group(1) if match else ""),
            "is_fact": kind == EV_FACT_ADDED,
            "tool_calls": tool_calls,
            "target_epoch": target_epoch,
            "coverage_key": str(prov.get("coverage_key") or ""),
        })
    return rows


def _same_question(left: str, right: str) -> bool:
    a = " ".join(str(left or "").split()).casefold()
    b = " ".join(str(right or "").split()).casefold()
    if not a or not b:
        return False
    return a == b


def _normalize_coverage_key(value: str) -> str:
    return re.sub(r"[\s_]+", "-", str(value or "").strip().casefold()).strip("-")


def _same_coverage_surface(left: str, right: str) -> bool:
    norm_a = _normalize_coverage_key(left)
    norm_b = _normalize_coverage_key(right)
    return bool(norm_a and norm_b and norm_a == norm_b)


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


def closed_reopen_rows(shared_graph: Any) -> list[dict[str, Any]]:
    """Cancelled handoffs and bounded exclusions used by reopen checks.

    Zero-tool-call cancellations and rows from an older or unknown target epoch
    are excluded because they never tested the current target instance.
    """
    rows: list[dict[str, Any]] = []
    current_epoch = str(
        getattr(shared_graph, "_target_epoch", "")
        or getattr(getattr(shared_graph, "challenge", None), "target_epoch", "")
        or ""
    ).strip()
    for row in _salvage_lineage(shared_graph):
        if str(row.get("reason") or "") != "cancel":
            continue
        if getattr(getattr(shared_graph, "challenge", None), "mode", "ctf") == "ctf":
            continue
        if row.get("tool_calls") == 0:
            continue
        row_epoch = str(row.get("target_epoch") or "").strip()
        if current_epoch and row_epoch and row_epoch != current_epoch:
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
            "tool_calls": row.get("tool_calls"),
            "target_epoch": row_epoch,
            "coverage_key": str(row.get("coverage_key") or ""),
        })
    if getattr(getattr(shared_graph, "challenge", None), "mode", "ctf") != "ctf":
        return rows
    intent_goals: dict[str, str] = {}
    intent_coverages: dict[str, str] = {}
    all_events = shared_graph.events()
    for event in all_events:
        if event.get("kind") == "intent_proposed":
            ep = event.get("payload") or {}
            iid = str(ep.get("intent_id") or "").strip()
            if iid:
                intent_goals[iid] = str(ep.get("goal") or "").strip()
                cov = str(ep.get("coverage_key") or "").strip()
                if cov:
                    intent_coverages[iid] = cov
    for event in all_events:
        kind = str(event.get("kind") or "")
        payload = event.get("payload") or {}
        if kind == EV_DEAD_END:
            coverage = str(payload.get("coverage_key") or "").strip()
            if (
                not payload.get("tested_scope")
                or not payload.get("observed_result")
            ):
                continue
            row_epoch = str(payload.get("target_epoch") or "").strip()
            if current_epoch and row_epoch and row_epoch != current_epoch:
                continue
            dead_intent_id = str(payload.get("intent_id") or "").strip()
            rows.append({
                "seq": int(event["seq"]),
                "question": intent_goals.get(dead_intent_id, ""),
                "tested_scope": str(payload.get("tested_scope") or "").strip(),
                "coverage_key": coverage,
                "target_epoch": row_epoch,
            })
        elif kind == "intent_concluded":
            conclude_result = str(payload.get("result") or "").strip().lower()
            if conclude_result in {
                "cancelled",
                "timed_out",
                "superseded",
                "operator_stop",
                "interrupted",
                "handoff_missing",
                "steered",
                "error",
                "oom",
            } or bool(payload.get("handoff_missing")):
                continue
            row_epoch = str(payload.get("target_epoch") or "").strip()
            if current_epoch and row_epoch and row_epoch != current_epoch:
                continue
            raw_ids = payload.get("intent_ids")
            if isinstance(raw_ids, list) and raw_ids:
                iids = [str(item or "").strip() for item in raw_ids if str(item or "").strip()]
            else:
                raw_iid = str(payload.get("intent_id") or "").strip()
                iids = [part.strip() for part in raw_iid.split(",") if part.strip()]
            to_fact = payload.get("to_fact_seq")
            has_positive_fact = isinstance(to_fact, int) and to_fact > 0
            if has_positive_fact:
                continue
            for iid in iids:
                coverage = str(
                    payload.get("coverage_key") or intent_coverages.get(iid, "")
                ).strip()
                rows.append({
                    "seq": int(event["seq"]),
                    "question": intent_goals.get(iid, ""),
                    "tested_scope": str(payload.get("result_detail") or payload.get("reason") or "").strip(),
                    "coverage_key": coverage,
                    "target_epoch": str(payload.get("target_epoch") or "").strip(),
                })
    return rows


def cancelled_reopen_rows(shared_graph: Any) -> list[dict[str, Any]]:
    """Compatibility name for the closed-route preview catalog."""
    return closed_reopen_rows(shared_graph)


def stale_reopen_from_cancelled(
    action: str,
    from_facts: list[int],
    cancelled: list[dict[str, Any]] | None,
    *,
    coverage_key: str = "",
) -> bool:
    """Compare stable coverage IDs or exact questions, without semantic guesses."""
    cited = [int(item) for item in from_facts if int(item) > 0]
    for row in cancelled or []:
        if row.get("tool_calls") == 0:
            continue
        try:
            seq = int(row.get("seq") or 0)
        except (TypeError, ValueError):
            continue
        question = str(row.get("question") or row.get("goal") or "")
        tested_scope = str(row.get("tested_scope") or "")
        same_action = (
            _same_question(action, question)
            or (bool(tested_scope) and _same_question(action, tested_scope))
        )
        same_coverage = _same_coverage_surface(
            coverage_key, str(row.get("coverage_key") or "")
        )
        if seq <= 0 or (not same_action and not same_coverage):
            continue
        if any(item > seq for item in cited):
            continue
        return True
    return False


def stale_reopen_without_newer_fact(
    action: str,
    from_facts: list[int],
    shared_graph: Any,
    *,
    coverage_key: str = "",
) -> bool:
    """Reject the same closed route without a newer Fact or a changed epoch."""
    return stale_reopen_from_cancelled(
        action,
        from_facts,
        closed_reopen_rows(shared_graph),
        coverage_key=coverage_key,
    )
