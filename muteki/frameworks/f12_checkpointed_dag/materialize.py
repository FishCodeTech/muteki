"""F12 deterministic Task → Intent materialisation (design §4.4 / §5).

Identity scheme (spec):

    execution_id = hash(run_id, challenge_id, generation)
    revision_id  = hash(execution_id, revision_no, canonical_plan)
    intent_id    = hash(execution_id, task_id)
    attempt_id   = hash(execution_id, revision_id, task_id, attempt)

Replaying the same revision never creates a second set of intents: intent rows
are INSERT-OR-IGNOREd under their deterministic id and the events table dedupes
on ``intent::<intent_id>``. A crash mid-pass is healed by re-running the same
materialisation (partial materialisation recovery).
"""

from __future__ import annotations

import hashlib
import time
from typing import Any

from muteki.frameworks.f12_checkpointed_dag import schema as store
from muteki.frameworks.f12_checkpointed_dag.plan import canonical_plan_json


def _h(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def execution_id_for(run_id: str, challenge_id: str, generation: int) -> str:
    return f"f12-{_h(f'{run_id}|{challenge_id}|{int(generation)}')[:16]}"


def revision_id_for(execution_id: str, revision_no: int, plan: dict[str, Any]) -> str:
    canon = canonical_plan_json(plan)
    return f"f12r-{_h(f'{execution_id}|{int(revision_no)}|{canon}')[:16]}"


def intent_id_for(execution_id: str, task_id: str) -> str:
    return f"f12i-{_h(f'{execution_id}|{task_id}')[:16]}"


def attempt_id_for(
    execution_id: str, revision_id: str, task_id: str, attempt: int
) -> str:
    return f"f12a-{_h(f'{execution_id}|{revision_id}|{task_id}|{int(attempt)}')[:16]}"


def _topo_order(tasks: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Dependencies first. The validator already rejected cycles/unknown deps,
    so a plain Kahn pass is total here; any residual is appended stably."""
    by_id = {str(t.get("id")): t for t in tasks}
    done: set[str] = set()
    out: list[dict[str, Any]] = []
    remaining = list(tasks)
    while remaining:
        progressed = False
        for task in list(remaining):
            deps = [d for d in task.get("depends_on") or [] if d in by_id]
            if all(d in done for d in deps):
                out.append(task)
                done.add(str(task.get("id")))
                remaining.remove(task)
                progressed = True
        if not progressed:
            out.extend(remaining)
            break
    return out


def materialize_revision(
    graph: Any,
    *,
    execution_id: str,
    revision_id: str,
    revision_no: int,
    plan: dict[str, Any],
    generation: int,
    actor: str = "f12-materializer",
) -> dict[str, Any]:
    """Materialise every task of an accepted revision into real intents with
    real ``intent_dependencies`` rows, and retire un-executed tasks that the
    revision dropped.

    Returns {created: [...], carried: [...], cancelled: [...]}. Fully
    idempotent: re-running for the same revision is a no-op for already
    materialised tasks.
    """
    now = time.time()
    created: list[str] = []
    carried: list[str] = []
    cancelled: list[str] = []
    tasks = {str(t.get("id")): t for t in plan.get("tasks") or []}
    existing = {t["task_id"]: t for t in store.list_tasks(graph, execution_id)}

    # 1. tasks the revision dropped: pending/scheduled (never started) rows
    #    become explicitly cancelled (superseded); their intents leave the
    #    dispatch pool. Running/terminal tasks were forced into the revision by
    #    the validator, so they are never dropped here.
    for task_id, prior in existing.items():
        if task_id in tasks:
            continue
        if str(prior.get("status")) in ("pending", "scheduled"):
            if store.set_task_state(
                graph,
                execution_id,
                task_id,
                "cancelled",
                result_code="superseded",
                result_ref=revision_id,
                expected=("pending", "scheduled"),
            ):
                cancelled.append(task_id)
                _retire_intent(graph, str(prior.get("intent_id") or ""), actor)
                store.append_event(
                    graph,
                    "f12_task_state_changed",
                    {
                        "execution_id": execution_id,
                        "task_id": task_id,
                        "intent_id": str(prior.get("intent_id") or ""),
                        "from": prior.get("status"),
                        "to": "cancelled",
                        "reason": f"superseded by revision {revision_id}",
                        "revision_id": revision_id,
                    },
                )

    # 2. materialise in dependency order (dep intents exist before dependents
    #    reference them in intent_dependencies).
    for task in _topo_order(list(tasks.values())):
        task_id = str(task["id"])
        iid = intent_id_for(execution_id, task_id)
        if task_id in existing:
            carried.append(task_id)
            # same revision replay: nothing to do; newer revision carrying the
            # task unchanged: identity fields are immutable (validator), keep
            # the original intent + row.
            continue
        dep_intents = [intent_id_for(execution_id, d) for d in task.get("depends_on") or []]
        store.upsert_task(
            graph,
            {
                "execution_id": execution_id,
                "task_id": task_id,
                "revision_id": revision_id,
                "intent_id": iid,
                "goal": task.get("goal", ""),
                "worker_class": task.get("worker_class", "code"),
                "depends_on": task.get("depends_on") or [],
                "success_criteria": task.get("success_criteria") or [],
                "required_tier": task.get("required_tier", "C1"),
                "profile_hint": task.get("profile_hint", ""),
                "execution_directory": task.get("execution_directory", ""),
                "route_hash": task.get("route_hash", ""),
                "lane_key": task.get("lane_key", ""),
                "risk_class": task.get("risk_class", ""),
                "resource_key": task.get("resource_key", ""),
                "priority": int(task.get("priority") or 0),
                "generation": int(generation),
            },
        )
        try:
            graph.propose_intent(
                actor=actor,
                intent_id=iid,
                goal=str(task.get("goal") or "")[:2000],
                payload={
                    "worker_class": task.get("worker_class", "code"),
                    "priority": int(task.get("priority") or 0),
                    "route_hash": task.get("route_hash", ""),
                    "lane_key": task.get("lane_key", ""),
                    "risk_class": task.get("risk_class", ""),
                    "resource_key": task.get("resource_key", ""),
                    "depends_on": dep_intents,
                    "source": "f12",
                    "f12_execution": execution_id,
                    "f12_revision": revision_id,
                    "f12_revision_no": int(revision_no),
                    "f12_task_id": task_id,
                    "f12_generation": int(generation),
                    "required_tier": task.get("required_tier", "C1"),
                },
            )
        except Exception:
            # The f12_plan_task row exists; the intent row may be missing after
            # a crash. The recovery pass (restore_materialization) re-runs this
            # same function and fills the gap deterministically.
            pass
        created.append(task_id)

    if created or carried:
        store.append_event(
            graph,
            "f12_plan_materialized",
            {
                "execution_id": execution_id,
                "revision_id": revision_id,
                "revision_no": int(revision_no),
                "created": created,
                "carried": carried,
                "cancelled": cancelled,
                "task_count": len(tasks),
            },
        )
    return {"created": created, "carried": carried, "cancelled": cancelled}


def _retire_intent(graph: Any, intent_id: str, actor: str) -> None:
    """Close a cancelled task's intent so it leaves the dispatch pool."""
    if not intent_id:
        return
    conn = getattr(graph, "_conn", None)
    lock = getattr(graph, "_lock", None)
    if conn is None:
        return

    def _op() -> None:
        conn.execute(
            "UPDATE intents SET dispatch_state='closed', close_reason=? "
            "WHERE intent_id=? AND status='open'",
            ("f12 superseded", intent_id),
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


def restore_materialization(
    graph: Any,
    *,
    execution_id: str,
    revision_id: str,
    revision_no: int,
    plan: dict[str, Any],
    generation: int,
) -> dict[str, Any]:
    """Crash-recovery pass (design §9): revision accepted but materialisation
    interrupted → re-run the same deterministic materialisation; Task/Intent
    mismatches (task row without intent row) are healed by re-proposing the
    deterministic intent id (INSERT OR IGNORE → no duplicate).
    """
    result = materialize_revision(
        graph,
        execution_id=execution_id,
        revision_id=revision_id,
        revision_no=revision_no,
        plan=plan,
        generation=generation,
        actor="f12-recovery",
    )
    healed: list[str] = []
    conn = getattr(graph, "_conn", None)
    lock = getattr(graph, "_lock", None)
    if conn is not None:
        for task in store.list_tasks(graph, execution_id):
            iid = str(task.get("intent_id") or "")
            if not iid:
                continue

            def _q() -> Any:
                return conn.execute(
                    "SELECT status FROM intents WHERE intent_id=?", (iid,)
                ).fetchone()

            if lock is None:
                row = _q()
            else:
                with lock:
                    row = _q()
            if row is not None:
                continue
            if str(task.get("status")) in store.TASK_TERMINAL_STATES:
                continue  # terminal work is never revived
            # The task row survived the crash but its intent row did not.
            # materialize_revision carries existing tasks (never re-proposes
            # them), so heal the gap here: re-propose the SAME deterministic
            # intent id (INSERT OR IGNORE → no duplicate, same payload as the
            # original materialisation pass).
            dep_intents = [
                intent_id_for(execution_id, str(d))
                for d in task.get("depends_on") or []
            ]
            try:
                graph.propose_intent(
                    actor="f12-recovery",
                    intent_id=iid,
                    goal=str(task.get("goal") or "")[:2000],
                    payload={
                        "worker_class": task.get("worker_class", "code"),
                        "priority": int(task.get("priority") or 0),
                        "route_hash": task.get("route_hash", ""),
                        "lane_key": task.get("lane_key", ""),
                        "risk_class": task.get("risk_class", ""),
                        "resource_key": task.get("resource_key", ""),
                        "depends_on": dep_intents,
                        "source": "f12",
                        "f12_execution": execution_id,
                        "f12_revision": revision_id,
                        "f12_revision_no": int(revision_no),
                        "f12_task_id": str(task.get("task_id")),
                        "f12_generation": int(generation),
                        "required_tier": task.get("required_tier", "C1"),
                    },
                )
            except Exception:
                continue
            healed.append(str(task.get("task_id")))
    if healed:
        store.append_event(
            graph,
            "f12_plan_materialized",
            {
                "execution_id": execution_id,
                "revision_id": revision_id,
                "revision_no": int(revision_no),
                "created": [],
                "carried": [],
                "cancelled": [],
                "healed_missing_intents": healed,
                "recovery": True,
            },
        )
    return {**result, "healed": healed}


def cancel_execution_tasks(
    graph: Any, *, execution_id: str, reason: str, actor: str = "f12"
) -> list[str]:
    """Cancel every open task (generation fence / f12:stop / execution close).
    Running tasks are left to their worker-session reclaim; only never-started
    tasks flip here."""
    cancelled: list[str] = []
    for task in store.list_tasks(graph, execution_id):
        if str(task.get("status")) not in ("pending", "scheduled"):
            continue
        if store.set_task_state(
            graph,
            execution_id,
            str(task["task_id"]),
            "cancelled",
            result_code="execution_closed",
            result_ref=reason,
            expected=("pending", "scheduled"),
        ):
            _retire_intent(graph, str(task.get("intent_id") or ""), actor)
            cancelled.append(str(task["task_id"]))
    return cancelled
