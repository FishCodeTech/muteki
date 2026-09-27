"""F12 source coordination agent (design §2.1 / §4 / §6).

One persistent identity per execution: ``f12-source`` bound to
(run_id, execution_generation). It does NOT run commands, write verified
facts, accept flags, or touch worker lifecycle — it only emits structured
plan revisions + exact schedules.

Muteki's coordinator-side cognition (f05/f07/f11 lead) uses sessionless
``llm.chat`` calls; F12 follows that path explicitly: every turn persists the
full canonical state hash, source turn number and wake checkpoint BEFORE the
call, and records the degraded/sessionless mode on the execution row. A reply
that arrived but whose application is uncertain is recovered by reading the
source-turn row back (turn → reply sha → revision sha → idempotent apply),
never by blindly re-calling the model.
"""

from __future__ import annotations

import hashlib
import json
import time
from typing import Any

from muteki.frameworks.f12_checkpointed_dag import schema as store
from muteki.frameworks.f12_checkpointed_dag.plan import (
    PLAN_SCHEMA_ID,
    PlanParseError,
    parse_source_reply,
    plan_sha256,
)

SOURCE_SOLVER_ID = "f12-source"
SESSIONLESS_DEGRADED = "sessionless"  # provider has no durable session; full
# canonical state is persisted per turn instead (design §3 activation note).

SOURCE_SYSTEM = (
    "You are the source coordination agent of a CTF/pentest solver swarm "
    "(f12-checkpointed-dag). You own the WHOLE task DAG: you plan, you pick "
    "the exact set of tasks that may start this round, you react to "
    "settlement checkpoints, and you say whether the goal is satisfied.\n"
    "You never execute commands yourself. Workers only see their own task.\n"
    "Rules:\n"
    "- Reply with ONE JSON object, schema "
    f"{PLAN_SCHEMA_ID!r}.\n"
    "- 'tasks' is always the COMPLETE current task graph, not a diff.\n"
    "- Once a task has started or finished you must keep it in 'tasks' with "
    "an unchanged id/goal/worker_class/depends_on. To change the meaning of "
    "work, add a NEW task id and drop the un-started old one.\n"
    "- 'schedule' lists the exact task ids allowed to START now. Only pick "
    "pending tasks whose dependencies all succeeded. A task not in 'schedule' "
    "will NOT start, even if its dependencies are done.\n"
    "- After a failed dependency, revise: cancel the blocked successors (drop "
    "them if they never started) or add replacement tasks.\n"
    "- goal_status: 'continue' while work remains; 'goal_satisfied' only when "
    "the success evidence on the board is complete; 'more_work_required' when "
    "you need another round; 'inconclusive' when you cannot tell.\n"
    "- review_mode: 'self' for a normal round, 'independent' when you want an "
    "independent read-only reviewer to judge goal completion.\n"
    "Be terse. Evidence-backed claims only."
)


def _sha(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


class SourceCoordinator:
    """Durable-identity planning agent for one F12 execution."""

    def __init__(
        self,
        graph: Any,
        *,
        execution_id: str,
        run_id: str,
        generation: int,
        model: str,
        llm: Any,
        source_id: str = SOURCE_SOLVER_ID,
    ) -> None:
        self.graph = graph
        self.execution_id = execution_id
        self.run_id = run_id
        self.generation = int(generation)
        self.model = model
        self.llm = llm
        self.source_id = source_id

    # -- canonical state the source reasons over ---------------------------

    def build_state_packet(
        self,
        *,
        challenge: Any,
        wake: dict[str, Any],
        tasks: list[dict[str, Any]],
        facts_summary: str,
        flags: list[str],
        dead_ends: list[str],
        budget: dict[str, Any],
        rejection_feedback: str = "",
        review_feedback: str = "",
    ) -> str:
        """The full canonical state for one wake (sessionless provider ⇒ the
        whole spec state travels with every call)."""
        execution = store.get_execution(self.graph, self.execution_id) or {}
        contract = getattr(challenge, "task_contract", None)
        raw_instruction = (
            str(getattr(contract, "raw_instruction", "") or "").strip()
            or str(getattr(challenge, "description", "") or "").strip()
        )
        task_lines = []
        for t in tasks:
            deps = ",".join(str(d) for d in t.get("depends_on") or []) or "-"
            task_lines.append(
                f"- {t['task_id']} [{t['status']}] attempt={t['attempt']} "
                f"class={t['worker_class']} tier={t['required_tier']} "
                f"deps={deps} rev={str(t.get('revision_id') or '')[-6:]}: "
                f"{str(t.get('goal') or '')[:220]}"
                + (
                    f"  result={t.get('result_code')}:{str(t.get('result_ref'))[:160]}"
                    if t.get("result_code")
                    else ""
                )
            )
        packet = {
            "schema": PLAN_SCHEMA_ID,
            "execution_id": self.execution_id,
            "run_id": self.run_id,
            "generation": self.generation,
            "source_turn_next": int(execution.get("source_turn") or 0) + 1,
            "wake_checkpoint": wake.get("checkpoint_no"),
            "wake_kind": wake.get("kind"),
            "wake_trigger_task": wake.get("trigger_task_id"),
            "wake_detail": wake.get("detail", ""),
            "goal_status": execution.get("goal_status", "open"),
            "effect": execution.get("effect"),
            "speed": execution.get("speed"),
            "operator_task": {
                "mode": str(getattr(challenge, "mode", "ctf") or "ctf"),
                "raw_instruction": raw_instruction[:3000],
                "target": str(getattr(challenge, "target", "") or ""),
                "category": str(getattr(challenge, "category", "") or ""),
                "expected_flags": int(getattr(challenge, "expected_flags", 1) or 1),
            },
            "tasks": task_lines,
            "board_summary": facts_summary[:6000],
            "accepted_flags": list(flags)[:8],
            "dead_ends": list(dead_ends)[:12],
            "budget": budget,
            "rejection_feedback": rejection_feedback[:800],
            "goal_review_feedback": review_feedback[:800],
        }
        return json.dumps(packet, ensure_ascii=False, indent=1)

    # -- one wake turn ------------------------------------------------------

    async def call(
        self,
        *,
        wake: dict[str, Any],
        prompt: str,
    ) -> dict[str, Any]:
        """Run one source turn for ``wake``. Idempotent:

        - turn row exists with status 'replied'/'applied' → reuse the stored
          reply (no second model call);
        - turn row exists with status 'called' → the reply was lost; the model
          is re-called ONCE (application is idempotent by revision sha);
        - otherwise the row is inserted first, then the call is made.
        """
        execution = store.get_execution(self.graph, self.execution_id) or {}
        turn_no = int(execution.get("source_turn") or 0) + 1
        prompt_sha = _sha(prompt)
        existing = store.get_source_turn(self.graph, self.execution_id, turn_no)
        if existing is not None and existing.get("reply_text"):
            reply = str(existing["reply_text"])
            reused = True
        else:
            if existing is None:
                store.begin_source_turn(
                    self.graph,
                    execution_id=self.execution_id,
                    turn_no=turn_no,
                    wake_checkpoint_no=int(wake.get("checkpoint_no") or 0),
                    prompt_sha256=prompt_sha,
                )
            reply = await self._chat(prompt)
            reused = False
            if not reply:
                store.update_source_turn(
                    self.graph,
                    self.execution_id,
                    turn_no,
                    status="failed",
                    error="empty or failed model reply",
                )
                return {"ok": False, "turn_no": turn_no, "error": "empty reply"}
            store.update_source_turn(
                self.graph,
                self.execution_id,
                turn_no,
                status="replied",
                reply_text=reply,
                reply_sha256=_sha(reply),
            )
        try:
            plan = parse_source_reply(reply)
        except PlanParseError as exc:
            store.update_source_turn(
                self.graph,
                self.execution_id,
                turn_no,
                status="failed",
                error=f"parse: {exc}",
            )
            return {
                "ok": False,
                "turn_no": turn_no,
                "error": f"parse: {exc}",
                "reply_sha256": _sha(reply),
            }
        store.update_source_turn(
            self.graph,
            self.execution_id,
            turn_no,
            plan_sha256=plan_sha256(plan),
        )
        return {
            "ok": True,
            "turn_no": turn_no,
            "plan": plan,
            "reply_sha256": _sha(reply),
            "reused_reply": reused,
        }

    def mark_applied(self, turn_no: int, revision_id: str) -> None:
        store.update_source_turn(
            self.graph,
            self.execution_id,
            turn_no,
            status="applied",
            revision_id=revision_id,
        )
        store.update_execution(
            self.graph, self.execution_id, source_turn=int(turn_no)
        )

    def mark_rejected(self, turn_no: int, revision_id: str) -> None:
        store.update_source_turn(
            self.graph,
            self.execution_id,
            turn_no,
            status="applied",
            revision_id=revision_id,
        )
        store.update_execution(
            self.graph, self.execution_id, source_turn=int(turn_no)
        )

    async def _chat(self, prompt: str) -> str:
        llm = self.llm
        if llm is None or not callable(getattr(llm, "chat", None)):
            return ""
        try:
            resp = await llm.chat(
                model=self.model,
                messages=[
                    {"role": "system", "content": SOURCE_SYSTEM},
                    {"role": "user", "content": prompt[:16000]},
                ],
                max_tokens=4000,
                stream=False,
                run_id=self.run_id,
                challenge_id=str(self.execution_id),
                solver_id=self.source_id,
            )
        except Exception:
            return ""
        return str(getattr(resp, "content", "") or "").strip()


def detect_rejected_feedback(graph: Any, execution_id: str) -> str:
    """The most recent rejection reason, fed to the source on its next wake so
    it can produce a full replacement revision (design §4.3)."""
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT reject_reason FROM f12_plan_revision "
            "WHERE execution_id=? AND status='rejected' "
            "ORDER BY revision_no DESC LIMIT 1",
            (execution_id,),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    return str(row[0] or "") if row else ""


def has_unapplied_reply(graph: Any, execution_id: str) -> dict[str, Any] | None:
    """Recovery query: a source turn whose reply is stored but not yet marked
    applied/failed (process died between reply and apply)."""
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT turn_no FROM f12_source_turn "
            "WHERE execution_id=? AND status='replied' "
            "ORDER BY turn_no DESC LIMIT 1",
            (execution_id,),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return store.get_source_turn(graph, execution_id, int(row[0]))
