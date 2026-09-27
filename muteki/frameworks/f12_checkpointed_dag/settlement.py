"""F12 task settlement + checkpoint writing (design §4.7 / §9).

A sub-run's terminal state is judged from REAL signals only: the intent's
conclusion row, facts/artifacts it produced (intent_products / to_fact_seq),
accepted flags, and the worker-session/lease state. Plan text, review text,
operator text and model claims never settle a task.

Every terminal transition writes a settlement checkpoint with
``wake_source=1`` so the source coordination agent is woken exactly once per
checkpoint (``wake_ack`` single-consumption).
"""

from __future__ import annotations

import hashlib
import json
import time
from typing import Any

from muteki.frameworks.f12_checkpointed_dag import schema as store


def canonical_state_sha(graph: Any, execution_id: str) -> str:
    """Tamper-evident hash over the execution row + all task projections."""
    execution = store.get_execution(graph, execution_id) or {}
    tasks = store.list_tasks(graph, execution_id)
    blob = {
        "execution": {
            k: execution.get(k)
            for k in (
                "execution_id",
                "execution_generation",
                "active_revision_id",
                "source_turn",
                "checkpoint_no",
                "goal_status",
                "status",
            )
        },
        "tasks": [
            {
                "task_id": t.get("task_id"),
                "revision_id": t.get("revision_id"),
                "status": t.get("status"),
                "attempt": t.get("attempt"),
                "result_code": t.get("result_code"),
            }
            for t in tasks
        ],
    }
    return hashlib.sha256(
        json.dumps(blob, sort_keys=True, default=str).encode("utf-8")
    ).hexdigest()


def _fact_highwater(graph: Any) -> int:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE kind IN "
            "('fact_added','hyp_proposed') AND challenge_id=?",
            (graph.challenge.id,),
        ).fetchone()

    try:
        if lock is None:
            row = _q()
        else:
            with lock:
                row = _q()
        return int(row[0] if row else 0)
    except Exception:
        return 0


def _artifact_highwater(graph: Any) -> int:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT COUNT(*) FROM events WHERE artifact_id IS NOT NULL "
            "AND artifact_id!='' AND challenge_id=?",
            (graph.challenge.id,),
        ).fetchone()

    try:
        if lock is None:
            row = _q()
        else:
            with lock:
                row = _q()
        return int(row[0] if row else 0)
    except Exception:
        return 0


def _task_summary(graph: Any, execution_id: str) -> dict[str, Any]:
    summary: dict[str, str] = {}
    for t in store.list_tasks(graph, execution_id):
        summary[str(t["task_id"])] = str(t.get("status") or "")
    return summary


def write_settlement_checkpoint(
    graph: Any,
    *,
    execution_id: str,
    kind: str,
    trigger_task_id: str = "",
    schedule: list[str] | None = None,
    wake: bool = True,
    budget_highwater: float = 0.0,
    detail: str = "",
    not_before: float = 0.0,
) -> int:
    execution = store.get_execution(graph, execution_id) or {}
    no = store.write_checkpoint(
        graph,
        execution_id=execution_id,
        revision_id=str(execution.get("active_revision_id") or ""),
        source_turn=int(execution.get("source_turn") or 0),
        kind=kind,
        trigger_task_id=trigger_task_id,
        schedule=schedule,
        task_summary=_task_summary(graph, execution_id),
        fact_highwater=_fact_highwater(graph),
        artifact_highwater=_artifact_highwater(graph),
        budget_highwater=float(budget_highwater or 0.0),
        state_sha256=canonical_state_sha(graph, execution_id),
        wake_source=wake,
        not_before=not_before,
        detail=detail,
    )
    store.append_event(
        graph,
        "f12_checkpoint_written",
        {
            "execution_id": execution_id,
            "checkpoint_no": no,
            "kind": kind,
            "trigger_task_id": trigger_task_id,
            "wake_source": bool(wake),
            "detail": detail[:400],
        },
    )
    return no


def _retire_intent_row(graph: Any, intent_id: str) -> None:
    """Close an open intent that F12 cancelled (leaves the dispatch pool)."""
    if not intent_id:
        return
    conn, lock = getattr(graph, "_conn", None), getattr(graph, "_lock", None)
    if conn is None:
        return

    def _op() -> None:
        conn.execute(
            "UPDATE intents SET dispatch_state='closed', close_reason=? "
            "WHERE intent_id=? AND status='open'",
            ("f12 generation fence", intent_id),
        )
        conn.commit()

    try:
        if lock is None:
            _op()
        else:
            with lock:
                _op()
    except Exception:
        pass


def _intent_row(graph: Any, intent_id: str) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT status, dispatch_state, close_reason, result_detail, "
            "to_fact_seq, result_seq, worker, lease_until FROM intents "
            "WHERE intent_id=?",
            (intent_id,),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return {
        "status": row[0],
        "dispatch_state": row[1],
        "close_reason": row[2] or "",
        "result_detail": row[3] or "",
        "to_fact_seq": row[4],
        "result_seq": row[5],
        "worker": row[6] or "",
        "lease_until": row[7],
    }


def _intent_product_count(graph: Any, intent_id: str) -> int:
    products = getattr(graph, "intent_products", None)
    if callable(products):
        try:
            return len(products(intent_id) or [])
        except Exception:
            return 0
    return 0


def classify_terminal(
    graph: Any, *, task: dict[str, Any], intent: dict[str, Any]
) -> tuple[str, str, str]:
    """Map one concluded intent to (task_status, result_code, result_ref).

    Only real conclusions count: the intent row must be done/closed (or the
    conclude event exists) — a bare 'claimed' row is never settled here.
    """
    close_reason = str(intent.get("close_reason") or "")
    to_fact_seq = intent.get("to_fact_seq")
    products = _intent_product_count(graph, str(task.get("intent_id") or ""))
    result_ref = ""
    if intent.get("result_seq"):
        result_ref = f"event:{intent['result_seq']}"
    if close_reason == "solved" or to_fact_seq is not None or products > 0:
        ref = result_ref
        if to_fact_seq is not None:
            ref = f"{ref}+fact:{to_fact_seq}" if ref else f"fact:{to_fact_seq}"
        return "succeeded", "succeeded", ref
    if close_reason == "cancelled":
        return "cancelled", "cancelled", result_ref or close_reason
    # done + closed with no produced fact/artifact → the worker finished its
    # turns without meeting the success criteria (barren / giveup / steered).
    return "failed", close_reason or "no_result", result_ref


def sync_task_with_intents(
    graph: Any,
    *,
    execution_id: str,
    generation: int,
    budget_highwater: float = 0.0,
    actor: str = "f12-settlement",
) -> dict[str, Any]:
    """Reconcile task projections with the intents table (the authority on the
    worker side). Idempotent per (task, status); emits events on transitions.

    Returns {running: [...], settled: [...], inconclusive: [...]}.
    """
    running: list[str] = []
    settled: list[str] = []
    inconclusive: list[str] = []
    now = time.time()
    for task in store.list_tasks(graph, execution_id):
        status = str(task.get("status") or "")
        if status in store.TASK_TERMINAL_STATES:
            continue
        # generation fence: a task written by an older generation is isolated,
        # never folded into the current projection (design §9).
        if int(task.get("generation") or 1) != int(generation):
            if store.set_task_state(
                graph,
                execution_id,
                str(task["task_id"]),
                "cancelled",
                result_code="generation_fence",
                result_ref=f"task gen {task.get('generation')} != {generation}",
                expected=("pending", "scheduled", "running"),
            ):
                _retire_intent_row(graph, str(task.get("intent_id") or ""))
                store.append_event(
                    graph,
                    "f12_task_state_changed",
                    {
                        "execution_id": execution_id,
                        "task_id": task["task_id"],
                        "intent_id": task.get("intent_id"),
                        "from": status,
                        "to": "cancelled",
                        "reason": f"generation fence {task.get('generation')} != {generation}",
                    },
                )
                settled.append(str(task["task_id"]))
            continue
        intent = _intent_row(graph, str(task.get("intent_id") or ""))
        if intent is None:
            # task row without an intent row: partial-materialisation crash;
            # the recovery materialiser heals it. Leave untouched.
            continue
        if (
            intent.get("status") == "done"
            or str(intent.get("dispatch_state")) == "closed"
        ):
            new_status, code, ref = classify_terminal(graph, task=task, intent=intent)
            changed = store.set_task_state(
                graph,
                execution_id,
                str(task["task_id"]),
                new_status,
                result_code=code,
                result_ref=ref,
                expected=("pending", "scheduled", "running"),
            )
            if changed:
                settled.append(str(task["task_id"]))
                store.append_event(
                    graph,
                    "f12_task_state_changed",
                    {
                        "execution_id": execution_id,
                        "task_id": task["task_id"],
                        "intent_id": task.get("intent_id"),
                        "from": status,
                        "to": new_status,
                        "result_code": code,
                        "result_ref": ref,
                    },
                )
            continue
        if intent.get("status") == "claimed":
            lease_until = intent.get("lease_until")
            if status != "running":
                attempt = int(task.get("attempt") or 0) + 1
                if store.set_task_state(
                    graph,
                    execution_id,
                    str(task["task_id"]),
                    "running",
                    attempt=attempt,
                    expected=("pending", "scheduled"),
                ):
                    running.append(str(task["task_id"]))
                    store.append_event(
                        graph,
                        "f12_task_state_changed",
                        {
                            "execution_id": execution_id,
                            "task_id": task["task_id"],
                            "intent_id": task.get("intent_id"),
                            "from": status,
                            "to": "running",
                            "attempt": attempt,
                            "worker": intent.get("worker") or "",
                        },
                    )
            else:
                running.append(str(task["task_id"]))
            # worker-lease reclaim (design §9): lease expired, no conclusion →
            # the run state is uncertain → inconclusive, NOT a silent restart.
            try:
                expired = lease_until is not None and float(lease_until) < now
            except (TypeError, ValueError):
                expired = False
            if expired:
                if store.set_task_state(
                    graph,
                    execution_id,
                    str(task["task_id"]),
                    "inconclusive",
                    result_code="lease_expired",
                    result_ref=f"worker={intent.get('worker') or ''}",
                    expected=("running",),
                ):
                    inconclusive.append(str(task["task_id"]))
                    store.append_event(
                        graph,
                        "f12_recovery_required",
                        {
                            "execution_id": execution_id,
                            "task_id": task["task_id"],
                            "intent_id": task.get("intent_id"),
                            "reason": "worker lease expired without conclusion",
                        },
                    )
    return {"running": running, "settled": settled, "inconclusive": inconclusive}


def settle_once(
    graph: Any,
    *,
    execution_id: str,
    generation: int,
    budget_highwater: float = 0.0,
    settlement_wake: bool = True,
) -> dict[str, Any]:
    """One settlement pass: sync task states with the intents table; write one
    settlement checkpoint per settled task (wake_source per config); write one
    all_terminal checkpoint when nothing remains open.

    Returns {settled, inconclusive, all_terminal, checkpoints:[...]}.
    """
    before_open = {
        t["task_id"]
        for t in store.list_tasks(graph, execution_id)
        if str(t.get("status")) in store.TASK_OPEN_STATES
    }
    sync = sync_task_with_intents(
        graph,
        execution_id=execution_id,
        generation=generation,
        budget_highwater=budget_highwater,
    )
    checkpoints: list[int] = []
    execution = store.get_execution(graph, execution_id) or {}
    schedule = []
    if execution.get("active_revision_id"):
        revision = store.get_revision(graph, execution["active_revision_id"])
        if revision:
            schedule = list(revision.get("schedule") or [])
    # Terminal transitions include settled (succeeded/failed/cancelled) AND
    # inconclusive (lease-expired without conclusion) tasks; the inconclusive
    # ones additionally carry a recovery wake even under the A5 ablation —
    # recovery裁定 is a distinct wake condition (design §4.8), not a normal
    # settlement wake.
    transitions = [
        (tid, False) for tid in sync["settled"]
    ] + [
        (tid, True) for tid in sync["inconclusive"]
    ]
    for task_id, is_recovery in transitions:
        task = store.get_task(graph, execution_id, task_id) or {}
        if str(task.get("status")) not in store.TASK_TERMINAL_STATES:
            continue
        wake = (settlement_wake or is_recovery)
        no = write_settlement_checkpoint(
            graph,
            execution_id=execution_id,
            kind="recovery" if is_recovery else "settlement",
            trigger_task_id=task_id,
            schedule=schedule,
            wake=wake,
            budget_highwater=budget_highwater,
            detail=f"task {task_id} -> {task.get('status')}",
        )
        store.set_task_state(
            graph,
            execution_id,
            task_id,
            str(task.get("status")),
            result_checkpoint_no=no,
            expected=store.TASK_TERMINAL_STATES,
        )
        checkpoints.append(no)
        store.append_event(
            graph,
            "f12_task_settled",
            {
                "execution_id": execution_id,
                "task_id": task_id,
                "intent_id": task.get("intent_id"),
                "status": task.get("status"),
                "result_code": task.get("result_code"),
                "checkpoint_no": no,
            },
        )
    after_open = {
        t["task_id"]
        for t in store.list_tasks(graph, execution_id)
        if str(t.get("status")) in store.TASK_OPEN_STATES
    }
    all_terminal = bool(before_open or sync["settled"]) and not after_open
    if all_terminal:
        no = write_settlement_checkpoint(
            graph,
            execution_id=execution_id,
            kind="all_terminal",
            schedule=schedule,
            wake=True,  # all-terminal always wakes (goal review decision)
            budget_highwater=budget_highwater,
            detail="all tasks reached a terminal state",
        )
        checkpoints.append(no)
    return {
        "settled": sync["settled"],
        "inconclusive": sync["inconclusive"],
        "all_terminal": all_terminal,
        "checkpoints": checkpoints,
    }
