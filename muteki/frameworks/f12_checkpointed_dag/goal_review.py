"""F12 Goal Review (design §8).

Separated from the product completion gates: Goal Review only decides whether
PLANNING continues. CTF/pentest completion still comes from the existing Flag
Gate / report pipeline.

- self review: the source agent's own ``goal_status`` is recorded verbatim.
- independent review: a separate review-class worker with a strictly read-only
  briefing judges goal completion and files a ``review_proposal`` with marker
  ``F12_GOAL`` — advisory only. A negative/inconclusive verdict forces the
  source agent to add tasks, agree, or record an evidence-backed disagreement.
- reviewer run failure → ``recovery_required``; there is NO silent fallback to
  self review.
"""

from __future__ import annotations

import hashlib
import time
from typing import Any

from muteki.frameworks.f12_checkpointed_dag import schema as store

GOAL_REVIEW_MARKER = "F12_GOAL"
GOAL_REVIEW_SOURCE = "f12_goal_review"

VERDICTS = ("goal_satisfied", "more_work_required", "inconclusive")


def review_id_for(execution_id: str, revision_id: str, mode: str, seq: int) -> str:
    return f"f12g-{hashlib.sha256(f'{execution_id}|{revision_id}|{mode}|{seq}'.encode()).hexdigest()[:16]}"


def review_intent_id(execution_id: str, review_id: str) -> str:
    return f"f12gr-{hashlib.sha256(f'{execution_id}|{review_id}'.encode()).hexdigest()[:16]}"


def should_use_independent(
    *, effect: int, review_mode: str, conflicting_facts: bool = False,
    consecutive_failures: int = 0,
) -> bool:
    """§8.2: C3 effect, conflicting facts, repeated failures, or an explicit
    source request all force an independent review."""
    if review_mode == "independent":
        return True
    if int(effect) >= 85:
        return True
    if conflicting_facts or int(consecutive_failures) >= 3:
        return True
    return False


def request_independent_review(
    graph: Any,
    *,
    execution_id: str,
    revision_id: str,
    challenge_id: str,
    goal_summary: str,
) -> dict[str, Any]:
    """Dispatch the read-only reviewer as a REAL review-class intent.

    The intent is deliberately NOT registered in f12_plan_task: it is a
    system-level review pipeline member, so the F12 exact-ready-set gate lets
    it through and the normal review capacity policy applies.
    """
    seq_no = len(store.list_goal_reviews(graph, execution_id)) + 1
    rid = review_id_for(execution_id, revision_id, "independent", seq_no)
    iid = review_intent_id(execution_id, rid)
    created = store.insert_goal_review(
        graph,
        review_id=rid,
        execution_id=execution_id,
        revision_id=revision_id,
        mode="independent",
        reviewer_intent_id=iid,
    )
    if not created:
        return {"review_id": rid, "intent_id": iid, "created": False}
    try:
        graph.propose_intent(
            actor="f12-goal-review",
            intent_id=iid,
            goal=(
                "Independent read-only goal review (f12). Judge ONLY from "
                "verified facts, artifacts, accepted flags and report receipts "
                "on the shared board whether the operator goal is satisfied. "
                "You must not run destructive or mutating commands. File your "
                f"verdict as a review proposal with marker {GOAL_REVIEW_MARKER} "
                "and verdict one of goal_satisfied|more_work_required|"
                f"inconclusive. Goal under review: {goal_summary[:800]}"
            ),
            payload={
                "worker_class": "review",
                "source": GOAL_REVIEW_SOURCE,
                "f12_execution": execution_id,
                "f12_review_id": rid,
                "priority": "high",
            },
        )
    except Exception:
        pass
    store.append_event(
        graph,
        "f12_goal_review_requested",
        {
            "execution_id": execution_id,
            "review_id": rid,
            "revision_id": revision_id,
            "mode": "independent",
            "reviewer_intent_id": iid,
        },
    )
    return {"review_id": rid, "intent_id": iid, "created": True}


def record_self_review(
    graph: Any,
    *,
    execution_id: str,
    revision_id: str,
    verdict: str,
    detail: str = "",
) -> dict[str, Any]:
    """Record the source agent's own goal verdict (advisory record only)."""
    if verdict not in VERDICTS:
        verdict = "inconclusive"
    seq_no = len(store.list_goal_reviews(graph, execution_id)) + 1
    rid = review_id_for(execution_id, revision_id, "self", seq_no)
    store.insert_goal_review(
        graph,
        review_id=rid,
        execution_id=execution_id,
        revision_id=revision_id,
        mode="self",
    )
    store.update_goal_review(
        graph,
        rid,
        status="decided",
        verdict=verdict,
        detail=detail[:800],
        decided_at=time.time(),
    )
    store.append_event(
        graph,
        "f12_goal_reviewed",
        {
            "execution_id": execution_id,
            "review_id": rid,
            "revision_id": revision_id,
            "mode": "self",
            "verdict": verdict,
        },
    )
    return {"review_id": rid, "verdict": verdict}


def poll_independent_verdicts(
    graph: Any, *, execution_id: str, after_seq: int
) -> tuple[list[dict[str, Any]], int]:
    """Scan review proposals for F12_GOAL verdicts addressed to this execution.

    Returns (verdicts, new_after_seq). The reviewer's proposal is ADVISORY: the
    row records the verdict; the source agent must still agree / add tasks /
    record a disagreement on its next wake.
    """
    verdicts: list[dict[str, Any]] = []
    max_seq = after_seq
    try:
        events = graph.events_since(after_seq, kinds=["review_proposal"])
    except Exception:
        events = []
    pending = {
        r["reviewer_intent_id"]: r
        for r in store.list_goal_reviews(graph, execution_id)
        if r.get("status") == "pending" and r.get("mode") == "independent"
    }
    for event in events:
        max_seq = max(max_seq, int(event.get("seq") or 0))
        payload = event.get("payload") or {}
        if not isinstance(payload, dict):
            continue
        if str(payload.get("marker") or "") != GOAL_REVIEW_MARKER:
            continue
        if str(payload.get("execution_id") or "") not in ("", execution_id):
            continue
        verdict = str((payload.get("payload") or payload).get("verdict") or "")
        detail = str((payload.get("payload") or payload).get("detail") or "")
        if verdict not in VERDICTS:
            verdict = "inconclusive"
        matched = None
        for r in pending.values():
            matched = r
            break
        if matched is None:
            continue
        store.update_goal_review(
            graph,
            matched["review_id"],
            status="decided",
            verdict=verdict,
            detail=detail[:800],
            proposal_seq=int(event.get("seq") or 0),
            decided_at=time.time(),
        )
        store.append_event(
            graph,
            "f12_goal_reviewed",
            {
                "execution_id": execution_id,
                "review_id": matched["review_id"],
                "revision_id": matched.get("revision_id") or "",
                "mode": "independent",
                "verdict": verdict,
                "proposal_seq": int(event.get("seq") or 0),
            },
        )
        verdicts.append(
            {"review_id": matched["review_id"], "verdict": verdict, "detail": detail}
        )
    return verdicts, max_seq


def reviewer_run_failed(graph: Any, *, review: dict[str, Any]) -> bool:
    """The reviewer worker's intent concluded without filing a verdict."""
    iid = str(review.get("reviewer_intent_id") or "")
    if not iid:
        return False
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT status, dispatch_state FROM intents WHERE intent_id=?", (iid,)
        ).fetchone()

    try:
        if lock is None:
            row = _q()
        else:
            with lock:
                row = _q()
    except Exception:
        return False
    if row is None:
        return False
    done = row[0] == "done" or row[1] == "closed"
    return done and str(review.get("status") or "") == "pending"


def mark_review_failed(graph: Any, *, execution_id: str, review_id: str) -> None:
    """§8.2: reviewer failure → recovery_required, no silent self fallback."""
    store.update_goal_review(
        graph, review_id, status="failed", decided_at=time.time()
    )
    store.append_event(
        graph,
        "f12_recovery_required",
        {
            "execution_id": execution_id,
            "review_id": review_id,
            "reason": "independent goal reviewer concluded without a verdict",
        },
    )
