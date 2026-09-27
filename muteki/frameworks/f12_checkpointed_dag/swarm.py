"""SwarmF12 — checkpointed DAG collaboration framework (F12).

Design baseline: docs/frameworks_2026/12_checkpointed_dag_swarm.md.

A durable source coordination agent owns the COMPLETE task DAG and the exact
per-round schedule; the Coordinator only executes validated exact task sets;
every sub-task terminal transition folds into a persistent settlement
checkpoint that wakes the source agent again.

Product isolation (AGENTS.md 实验框架 rules):

- default-OFF experimental arm; loadable ONLY via the explicit class path
  ``muteki.frameworks.f12_checkpointed_dag:SwarmF12``;
- mechanisms are class-owned (never MUTEKI_* env keys — eval runners clear
  those per cell);
- the Flag Gate, report reproduction and provenance rules are untouched:
  plans, checkpoints, review text, operator directives and the source agent's
  own completion claims are orchestration state, never flag evidence;
- the product default stays ``muteki.swarm.swarm.Swarm``.

Run-shape deviations from the product Coordinator, all class-owned and
documented: race_scout off (an unplanned whole-challenge race would bypass the
exact ready-set), start_workers=0 (no unplanned bootstrap workers), and the
barren soft-pause off (F12 replaces it with source-agent planning termination
+ goal review; termination still goes through the honest operator-stop path).
"""

from __future__ import annotations

import asyncio
import json
from typing import Any, Optional

from muteki.frameworks.f12_checkpointed_dag import goal_review as f12_goal
from muteki.frameworks.f12_checkpointed_dag import materialize as f12_mat
from muteki.frameworks.f12_checkpointed_dag import router as f12_router
from muteki.frameworks.f12_checkpointed_dag import scheduler as f12_sched
from muteki.frameworks.f12_checkpointed_dag import schema as store
from muteki.frameworks.f12_checkpointed_dag.plan import (
    plan_sha256,
    validate_revision,
)
from muteki.frameworks.f12_checkpointed_dag.settlement import (
    canonical_state_sha,
    settle_once,
    write_settlement_checkpoint,
)
from muteki.frameworks.f12_checkpointed_dag.source import (
    SESSIONLESS_DEGRADED,
    SourceCoordinator,
    detect_rejected_feedback,
)
from muteki.swarm.swarm import Swarm

F12_DIRECTIVE_PREFIX = "f12:"
F12_MAX_REVISION_REJECTION_STREAK = 3
F12_MAX_SOURCE_FAILURE_STREAK = 3
F12_MAX_PUMP_ERROR_STREAK = 5


class SwarmF12(Swarm):
    """Checkpointed DAG arm. See module docstring for the product boundary."""

    architecture_name = "f12-checkpointed-dag"
    framework_id = "f12"
    f12_enabled = True
    reason_declaration_mode = None

    # Mechanism defaults are CLASS-OWNED (ablation arms flip them in
    # subclasses; eval runners cannot accidentally disable them by clearing
    # experimental env keys). Per-run knobs (effect/speed/approval_mode/
    # profile_tiers) may additionally come from stage_policy.coordinator["f12"].
    f12_mechanism_defaults: dict[str, Any] = {
        "settlement_wake": True,       # A5 ablation flips this off
        "independent_review": True,    # A3 ablation flips this off
        "serial": False,               # A6 ablation flips this on
        "fixed_profile": "",           # A4 ablation pins one profile
    }

    def __init__(
        self,
        *args: Any,
        f12_effect: Optional[int] = None,
        f12_speed: Optional[int] = None,
        f12_approval_mode: Optional[str] = None,
        f12_profile_tiers: Optional[dict[str, str]] = None,
        f12_settlement_wake: Optional[bool] = None,
        f12_independent_review: Optional[bool] = None,
        f12_serial: Optional[bool] = None,
        f12_fixed_profile: Optional[str] = None,
        **kwargs: Any,
    ) -> None:
        # Class-owned run shape (see module docstring). Explicit caller values
        # still win (setdefault), so a harness MAY re-enable race-scout for a
        # control experiment, but the arm's default is the F12 semantics.
        kwargs.setdefault("race_scout", False)
        kwargs.setdefault("start_workers", 0)
        kwargs.setdefault("barren_limit", 0)
        super().__init__(*args, **kwargs)
        mech = dict(self.f12_mechanism_defaults)
        cfg_src: dict[str, Any] = {}
        try:
            raw = self.stage_policy.coordinator.get("f12")
            if isinstance(raw, dict):
                cfg_src = raw
        except Exception:
            cfg_src = {}
        self._f12_cfg = {
            "effect": int(
                f12_effect if f12_effect is not None else cfg_src.get("effect", 60)
            ),
            "speed": int(
                f12_speed if f12_speed is not None else cfg_src.get("speed", 25)
            ),
            "approval_mode": str(
                f12_approval_mode
                or cfg_src.get("approval_mode")
                or "auto_eval"
            ),
            "profile_tiers": dict(
                f12_profile_tiers or cfg_src.get("profile_tiers") or {}
            ),
            "settlement_wake": bool(
                f12_settlement_wake
                if f12_settlement_wake is not None
                else mech["settlement_wake"]
            ),
            "independent_review": bool(
                f12_independent_review
                if f12_independent_review is not None
                else mech["independent_review"]
            ),
            "serial": bool(f12_serial if f12_serial is not None else mech["serial"]),
            "fixed_profile": str(
                f12_fixed_profile
                if f12_fixed_profile is not None
                else mech["fixed_profile"]
            ),
        }
        if self._f12_cfg["approval_mode"] not in ("auto_eval", "operator_review"):
            self._f12_cfg["approval_mode"] = "auto_eval"
        self._f12_late_init()
        # __init__ runs before the event loop exists in some embedders; the
        # schema/execution bootstrap is pure SQLite and safe to run eagerly.
        self._f12_ensure()

    # -- state --------------------------------------------------------------

    def _f12_late_init(self) -> None:
        """Attribute defaults (also backstops tests that build via __new__)."""
        defaults: dict[str, Any] = {
            "_f12_ready": False,
            "_f12_execution_id": "",
            "_f12_cfg": {
                "effect": 60,
                "speed": 25,
                "approval_mode": "auto_eval",
                "profile_tiers": {},
                "settlement_wake": True,
                "independent_review": True,
                "serial": False,
                "fixed_profile": "",
            },
            "_f12_mirror_seq": 0,
            "_f12_review_poll_seq": 0,
            "_f12_handled_directives": set(),
            "_f12_source_failure_streak": 0,
            "_f12_pump_error_streak": 0,
            "_f12_pumping": False,
            "_f12_run_stop_requested": False,
        }
        for k, v in defaults.items():
            if not hasattr(self, k):
                setattr(self, k, v.copy() if isinstance(v, (dict, list, set)) else v)

    def _f12_generation(self) -> int:
        return int(getattr(self, "_execution_generation", 1) or 1)

    def _f12_execution(self) -> Optional[dict[str, Any]]:
        g = getattr(self, "shared_graph", None)
        eid = getattr(self, "_f12_execution_id", "")
        if g is None or not eid:
            return None
        return store.get_execution(g, eid)

    def _f12_concurrency_target(self) -> int:
        cfg = getattr(self, "_f12_cfg", {})
        if cfg.get("serial"):
            return 1
        return f12_sched.speed_concurrency_target(int(cfg.get("speed") or 25))

    # -- schema / execution bootstrap ----------------------------------------

    def _f12_ensure(self) -> bool:
        self._f12_late_init()
        if self._f12_ready:
            return True
        g = getattr(self, "shared_graph", None)
        if g is None:
            return False
        if not store.ensure_f12_schema_on_graph(g):
            return False
        eid = f12_mat.execution_id_for(
            str(getattr(self, "run_id", "") or getattr(self.challenge, "id", "")),
            str(getattr(self.challenge, "id", "") or "unknown"),
            self._f12_generation(),
        )
        self._f12_execution_id = eid
        cfg = self._f12_cfg
        before = store.get_execution(g, eid)
        store.create_execution(
            g,
            execution_id=eid,
            run_id=str(getattr(self, "run_id", "") or ""),
            challenge_id=str(getattr(self.challenge, "id", "") or ""),
            execution_generation=self._f12_generation(),
            effect=int(cfg.get("effect") or 60),
            speed=int(cfg.get("speed") or 25),
            approval_mode=str(cfg.get("approval_mode") or "auto_eval"),
            degraded_mode=SESSIONLESS_DEGRADED,
        )
        after = store.get_execution(g, eid) or {}
        if before is None:
            # initial activation wake (design §4.1/§4.8).
            write_settlement_checkpoint(
                g,
                execution_id=eid,
                kind="initial",
                wake=True,
                detail="initial activation",
            )
        else:
            # process-restart recovery: heal a half-materialised accepted
            # revision deterministically (design §9 row 1/2).
            active = str(after.get("active_revision_id") or "")
            if active:
                revision = store.get_revision(g, active)
                if revision is not None:
                    try:
                        plan = json.loads(revision.get("plan_json") or "{}")
                    except Exception:
                        plan = None
                    if isinstance(plan, dict) and plan.get("tasks"):
                        f12_mat.restore_materialization(
                            g,
                            execution_id=eid,
                            revision_id=active,
                            revision_no=int(revision.get("revision_no") or 0),
                            plan=plan,
                            generation=self._f12_generation(),
                        )
        self._f12_ready = True
        return True

    # -- framework hooks ------------------------------------------------------

    def framework_prepare_hook(self) -> None:
        self._f12_ensure()

    async def _run_reason(self) -> int:
        """F12 takes over planning entirely: the product Reason planner is
        replaced by the source coordination agent's checkpointed wake cycle.
        No fallback to the product planner — an F12 run without its persistence
        spine fails honestly (queryable state), it never silently re-plans with
        the wrong architecture."""
        if not self._f12_ensure():
            return 0
        return await self._f12_pump(trigger="reason")

    async def framework_after_workers(self, *_a: Any, **_k: Any) -> None:
        if self._f12_ensure():
            await self._f12_pump(trigger="settlement")

    def _open_intents(self) -> list[dict]:
        """The F12 exact ready-set gate over the coordinator's intent view.

        F12-managed intents are visible only while their task is selected into
        the current schedule AND ready; every other intent (review pipeline,
        report reproduction, operator continuations, control intents — all
        structurally identified by not being F12-managed) passes unchanged.
        """
        base = super()._open_intents()
        if not self._f12_ready:
            return base
        g = getattr(self, "shared_graph", None)
        ex = self._f12_execution()
        if g is None or ex is None:
            return base
        return f12_sched.filter_open_intents(
            g,
            base,
            execution_id=str(ex["execution_id"]),
            active_revision_id=str(ex.get("active_revision_id") or ""),
            generation=int(ex.get("execution_generation") or 1),
            concurrency_target=self._f12_concurrency_target(),
        )

    def _has_dispatchable_open_intents(self, tasks: dict) -> bool:
        if super()._has_dispatchable_open_intents(tasks):
            return True
        # An unprocessed source wake is real pending work: it keeps the
        # coordinator loop alive until the wake is consumed (initial
        # activation, settlement follow-up, operator replan, goal review).
        if not self._f12_ready:
            return False
        g = getattr(self, "shared_graph", None)
        ex = self._f12_execution()
        if g is None or ex is None:
            return False
        if str(ex.get("status")) in store.EXECUTION_TERMINAL_STATES:
            return False
        return store.pending_wake_exists(g, str(ex["execution_id"]))

    def framework_worker_guidance_for_intent(self, intent_id: str) -> list[str]:
        """Structured per-task context packet (design §4.6). Only the current
        task's goal/criteria, its revision/checkpoint/generation, the facts and
        artifacts its SUCCEEDED dependencies produced, relevant dead ends,
        accepted flags and the settlement output contract are injected — never
        the full source-agent conversation or other workers' transcripts."""
        if not self._f12_ready:
            return []
        g = getattr(self, "shared_graph", None)
        ex = self._f12_execution()
        if g is None or ex is None:
            return []
        task = store.get_task_by_intent(g, str(ex["execution_id"]), str(intent_id))
        if task is None:
            return []  # system worker (review/verifier/operator) — unchanged
        lines = [
            f"[f12 task packet] execution={ex['execution_id']} "
            f"revision={ex.get('active_revision_id')} "
            f"generation={ex.get('execution_generation')} "
            f"checkpoint_no={ex.get('checkpoint_no')}",
            f"You are executing task {task['task_id']} of a checkpointed DAG. "
            f"Goal: {task.get('goal')}",
        ]
        criteria = [str(c) for c in task.get("success_criteria") or [] if str(c)]
        if criteria:
            lines.append("Success criteria (ALL must hold with real evidence):")
            lines.extend(f"  - {c}" for c in criteria)
        dep_facts = self._f12_dependency_facts(g, ex, task)
        if dep_facts:
            lines.append("Verified facts produced by the tasks you depend on:")
            lines.extend(f"  - {f}" for f in dep_facts[:16])
        dead_ends = self._f12_dead_ends(g)
        if dead_ends:
            lines.append("Known dead ends (do not retry):")
            lines.extend(f"  - {d}" for d in dead_ends[:8])
        flags = list(getattr(self, "_found_flags", []) or [])
        if flags:
            lines.append(f"Flags already accepted by the gate: {len(flags)}")
        if str(task.get("resource_key") or ""):
            lines.append(
                f"This task declares exclusive resource "
                f"{task['resource_key']!r}; claim it via the blackboard before "
                "mutating it and release it when done."
            )
        lines.append(
            "Settlement contract: write every claim as a fact with a command/"
            "artifact witness, store products as artifacts, and conclude with "
            "real evidence. Declaring success without command output settles "
            "this task as FAILED. Plan text, review text and operator text are "
            "never flag evidence."
        )
        return lines

    def _f12_dependency_facts(
        self, g: Any, ex: dict[str, Any], task: dict[str, Any]
    ) -> list[str]:
        out: list[str] = []
        conn, lock = g._conn, getattr(g, "_lock", None)
        for dep_id in task.get("depends_on") or []:
            dep = store.get_task(g, str(ex["execution_id"]), str(dep_id))
            if dep is None or str(dep.get("status")) != "succeeded":
                continue
            products = []
            fn = getattr(g, "intent_products", None)
            if callable(fn):
                try:
                    products = list(fn(str(dep.get("intent_id") or "")) or [])
                except Exception:
                    products = []
            if not products:
                continue
            q = ",".join("?" for _ in products)
            sql = (
                f"SELECT seq, payload FROM events WHERE seq IN ({q}) "
                "ORDER BY seq"
            )
            if lock is None:
                rows = conn.execute(sql, tuple(products)).fetchall()
            else:
                with lock:
                    rows = conn.execute(sql, tuple(products)).fetchall()
            for _seq, payload in rows:
                try:
                    obj = json.loads(payload or "{}")
                except Exception:
                    continue
                fact = str(obj.get("fact") or "").strip()
                if fact:
                    out.append(f"[{dep_id}] {fact[:240]}")
        return out

    def _f12_dead_ends(self, g: Any) -> list[str]:
        out: list[str] = []
        try:
            for e in g.events():
                if e.get("kind") != "dead_end":
                    continue
                p = e.get("payload") or {}
                text = str(p.get("reason") or p.get("text") or "").strip()
                if text:
                    out.append(text[:200])
        except Exception:
            return []
        return out[-8:]

    # -- profile routing (design §7.3) ----------------------------------------

    def _effect_capability_pick_engine(
        self,
        running_engines: list[str],
        healthy: list[str],
        *,
        role: str = "bootstrap",
        intent_id: str = "",
        lane: str = "",
        intent: Optional[dict] = None,
        avoid_engines: Optional[list[str]] = None,
    ) -> Optional[str]:
        del lane
        if not self._f12_ready:
            return None
        g = getattr(self, "shared_graph", None)
        ex = self._f12_execution()
        if g is None or ex is None or not intent_id:
            return None
        task = store.get_task_by_intent(g, str(ex["execution_id"]), str(intent_id))
        if task is None:
            return None  # system/review/verifier workers: product routing
        available = [e for e in (healthy or []) if e not in set(avoid_engines or ())]
        if not available:
            return None
        profiles = [
            self._profiles_by_name[name]
            for name in available
            if name in getattr(self, "_profiles_by_name", {})
        ]
        if not profiles:
            return None  # bare-engine roster: no profile metadata to route on
        cfg = self._f12_cfg
        hint = str(task.get("profile_hint") or "")
        hinted = [
            p for p in profiles if str(p.get("name") or "") == hint
        ] if hint else []
        required = f12_sched.task_effective_tier(task, int(cfg.get("effect") or 60))
        running_counts: dict[str, int] = {}
        for name in running_engines or []:
            running_counts[name] = running_counts.get(name, 0) + 1
        pool = hinted or profiles
        chosen = f12_router.choose_profile(
            pool,
            required_tier=required,
            tiers_meta=dict(cfg.get("profile_tiers") or {}),
            running_counts=running_counts,
            source_provider=self._f12_source_provider_key(),
            fixed_profile=str(cfg.get("fixed_profile") or ""),
        )
        shortfall = False
        if chosen is None and pool:
            # no candidate meets the tier floor: take the closest-highest tier
            # deterministically and RECORD the shortfall (never silently).
            shortfall = True
            chosen = f12_router.choose_profile(
                pool,
                required_tier="C0",
                tiers_meta=dict(cfg.get("profile_tiers") or {}),
                running_counts=running_counts,
                source_provider=self._f12_source_provider_key(),
                fixed_profile=str(cfg.get("fixed_profile") or ""),
            )
        if chosen is None:
            return None
        payload = f12_router.routing_event_payload(
            chosen,
            task_id=str(task["task_id"]),
            intent_id=str(intent_id),
            required_tier=required,
        )
        payload["profile_hint"] = hint
        payload["tier_shortfall"] = shortfall
        store.append_event(g, "f12_profile_routed", payload)
        return str(chosen.get("name") or "")

    def _f12_source_provider_key(self) -> str:
        model = str(getattr(self, "reason_model", "") or "")
        return model.split(":", 1)[0].split("/", 1)[0] if model else ""

    # -- the wake pump ---------------------------------------------------------

    async def _f12_pump(self, *, trigger: str) -> int:
        if self._f12_pumping:
            return 0
        self._f12_pumping = True
        try:
            return await self._f12_pump_inner(trigger=trigger)
        except Exception as exc:  # queryable, never silent
            self._f12_pump_error_streak += 1
            g = getattr(self, "shared_graph", None)
            ex = self._f12_execution()
            if g is not None and ex is not None:
                store.append_event(
                    g,
                    "f12_recovery_required",
                    {
                        "execution_id": ex["execution_id"],
                        "reason": f"pump error: {type(exc).__name__}: {exc}"[:400],
                        "streak": self._f12_pump_error_streak,
                    },
                )
                if self._f12_pump_error_streak >= F12_MAX_PUMP_ERROR_STREAK:
                    store.update_execution(
                        g, str(ex["execution_id"]), status="recovery_required"
                    )
            return 0
        finally:
            self._f12_pumping = False

    async def _f12_pump_inner(self, *, trigger: str) -> int:
        g = getattr(self, "shared_graph", None)
        ex = self._f12_execution()
        if g is None or ex is None:
            return 0
        eid = str(ex["execution_id"])
        gen = int(ex.get("execution_generation") or 1)
        cfg = self._f12_cfg

        self._f12_drain_directives()
        ex = self._f12_execution() or ex
        if str(ex.get("status")) in store.EXECUTION_TERMINAL_STATES:
            self._f12_maybe_stop_run()
            await self._f12_mirror_events()
            return 0

        # 1. settle worker terminal states → tasks (+ checkpoints + wakes).
        try:
            budget = float(self._current_cost_usd() or 0.0)
        except Exception:
            budget = 0.0
        settle_once(
            g,
            execution_id=eid,
            generation=gen,
            budget_highwater=budget,
            settlement_wake=bool(cfg.get("settlement_wake", True)),
        )
        # 2. independent goal-review verdicts + reviewer failure detection.
        self._f12_poll_goal_reviews()
        ex = self._f12_execution() or ex
        if str(ex.get("status")) in store.EXECUTION_TERMINAL_STATES:
            self._f12_maybe_stop_run()
            await self._f12_mirror_events()
            return 0

        # 3. consume the oldest pending wake, if any.
        wake = store.oldest_unacked_wake(g, eid)
        if wake is not None:
            await self._f12_apply_wake(wake, trigger=trigger)
        else:
            self._f12_maybe_stop_run()
        await self._f12_mirror_events()
        return self._f12_dispatchable_count()

    def _f12_dispatchable_count(self) -> int:
        g = getattr(self, "shared_graph", None)
        ex = self._f12_execution()
        if g is None or ex is None:
            return 0
        ready = f12_sched.ready_scheduled_tasks(
            g,
            execution_id=str(ex["execution_id"]),
            active_revision_id=str(ex.get("active_revision_id") or ""),
            generation=int(ex.get("execution_generation") or 1),
            concurrency_target=self._f12_concurrency_target(),
        )
        # only count tasks whose intent is genuinely still open for claim
        conn, lock = g._conn, getattr(g, "_lock", None)
        n = 0
        for t in ready:
            iid = str(t.get("intent_id") or "")

            def _q() -> Any:
                return conn.execute(
                    "SELECT status, dispatch_state FROM intents WHERE intent_id=?",
                    (iid,),
                ).fetchone()

            if lock is None:
                row = _q()
            else:
                with lock:
                    row = _q()
            if row and row[0] == "open" and row[1] == "active":
                n += 1
        return n

    # -- one source wake --------------------------------------------------------

    async def _f12_apply_wake(self, wake: dict[str, Any], *, trigger: str) -> None:
        g = self.shared_graph
        ex = self._f12_execution() or {}
        eid = str(ex["execution_id"])
        status = str(ex.get("status") or "active")
        if status in ("completing", "stopped", "recovery_required"):
            # Planning is done (completing/stopped) or parked for the operator
            # (recovery_required). Consume the wake WITHOUT a source call so a
            # checkpoint can never be double-processed; an f12:replan directive
            # flips recovery_required back to active and queues a fresh wake.
            store.ack_wake(g, eid, int(wake["checkpoint_no"]))
            return
        llm = getattr(self, "llm", None)
        source = SourceCoordinator(
            g,
            execution_id=eid,
            run_id=str(getattr(self, "run_id", "") or ""),
            generation=int(ex.get("execution_generation") or 1),
            model=str(getattr(self, "reason_model", "") or ""),
            llm=llm,
        )
        tasks = store.list_tasks(g, eid)
        rejection_feedback = detect_rejected_feedback(g, eid)
        review_feedback = self._f12_latest_review_feedback(g, eid)
        try:
            summary = g.to_reason_summary(
                standing_guidance=list(getattr(self, "_standing_guidance", []) or [])
            )
        except Exception:
            summary = ""
        prompt = source.build_state_packet(
            challenge=self.challenge,
            wake=wake,
            tasks=tasks,
            facts_summary=summary,
            flags=list(getattr(self, "_found_flags", []) or []),
            dead_ends=self._f12_dead_ends(g),
            budget={
                "cost_usd": self._f12_cost(),
                "cost_budget_usd": getattr(self, "cost_budget_usd", None),
                "max_workers": getattr(self, "max_workers", 0),
                "concurrency_target": self._f12_concurrency_target(),
            },
            rejection_feedback=rejection_feedback,
            review_feedback=review_feedback,
        )
        store.append_event(
            g,
            "f12_source_woken",
            {
                "execution_id": eid,
                "wake_checkpoint_no": wake.get("checkpoint_no"),
                "wake_kind": wake.get("kind"),
                "trigger": trigger,
            },
        )
        result = await source.call(wake=wake, prompt=prompt)
        if not result.get("ok"):
            self._f12_source_failure_streak += 1
            if self._f12_source_failure_streak >= F12_MAX_SOURCE_FAILURE_STREAK:
                store.ack_wake(g, eid, int(wake["checkpoint_no"]))
                store.update_execution(g, eid, status="recovery_required")
                store.append_event(
                    g,
                    "f12_recovery_required",
                    {
                        "execution_id": eid,
                        "reason": f"source agent failed: {result.get('error')}"[:400],
                        "streak": self._f12_source_failure_streak,
                    },
                )
            # unacked wake → retried on a later pump; the loop stays alive via
            # _has_dispatchable_open_intents.
            return
        self._f12_source_failure_streak = 0
        self._f12_apply_plan(source, result, wake)

    def _f12_apply_plan(
        self, source: SourceCoordinator, result: dict[str, Any], wake: dict[str, Any]
    ) -> None:
        g = self.shared_graph
        ex = self._f12_execution() or {}
        eid = str(ex["execution_id"])
        gen = int(ex.get("execution_generation") or 1)
        cfg = self._f12_cfg
        plan = result["plan"]
        turn_no = int(result["turn_no"])
        sha = plan_sha256(plan)

        # Content-addressed replay dedup: the identical plan content already
        # accepted → re-apply materialisation idempotently, no new revision.
        prior = store.find_revision_by_sha(g, eid, sha)
        if prior is not None and str(prior.get("status")) == "accepted":
            rid = str(prior["revision_id"])
            revision_no = int(prior.get("revision_no") or 0)
            f12_mat.restore_materialization(
                g,
                execution_id=eid,
                revision_id=rid,
                revision_no=revision_no,
                plan=plan,
                generation=gen,
            )
            self._f12_apply_schedule(plan, rid)
            store.ack_wake(g, eid, int(wake["checkpoint_no"]))
            source.mark_applied(turn_no, rid)
            self._f12_handle_goal(plan, rid)
            return

        revision_no = store.next_revision_no(g, eid)
        rid = f12_mat.revision_id_for(eid, revision_no, plan)
        parent = str(ex.get("active_revision_id") or "")
        errors = validate_revision(plan, existing_tasks=store.list_tasks(g, eid))
        store.insert_revision(
            g,
            revision_id=rid,
            execution_id=eid,
            revision_no=revision_no,
            parent_revision_id=parent,
            plan_json=json.dumps(plan, ensure_ascii=False, sort_keys=True),
            plan_sha256=sha,
            source_turn=turn_no,
            status="draft",
            schedule=list(plan.get("schedule") or []),
        )
        store.append_event(
            g,
            "f12_plan_revision_created",
            {
                "execution_id": eid,
                "revision_id": rid,
                "revision_no": revision_no,
                "parent_revision_id": parent,
                "plan_sha256": sha,
                "source_turn": turn_no,
                "task_count": len(plan.get("tasks") or []),
                "schedule": list(plan.get("schedule") or []),
                "goal_status": plan.get("goal_status"),
                "summary": plan.get("summary") or "",
            },
        )
        if errors:
            self._f12_reject_revision(source, rid, turn_no, errors, wake)
            return
        if str(cfg.get("approval_mode")) == "operator_review":
            # Draft waits for an explicit f12:approve (domain path implemented;
            # no product UI). The wake is consumed — the source is not called
            # again until the operator decides.
            store.ack_wake(g, eid, int(wake["checkpoint_no"]))
            source.mark_applied(turn_no, rid)
            store.update_execution(g, eid, goal_status="open")
            return
        self._f12_accept_revision(source, rid, turn_no, plan, wake)

    def _f12_reject_revision(
        self,
        source: SourceCoordinator,
        rid: str,
        turn_no: int,
        errors: list[str],
        wake: dict[str, Any],
    ) -> None:
        g = self.shared_graph
        ex = self._f12_execution() or {}
        eid = str(ex["execution_id"])
        reason = "; ".join(errors)[:900]
        store.set_revision_status(g, rid, "rejected", reject_reason=reason)
        store.append_event(
            g,
            "f12_plan_revision_rejected",
            {
                "execution_id": eid,
                "revision_id": rid,
                "source_turn": turn_no,
                "reasons": list(errors)[:12],
            },
        )
        store.ack_wake(g, eid, int(wake["checkpoint_no"]))
        source.mark_rejected(turn_no, rid)
        # Rejection streak: bounded immediate re-wake with the failure reason
        # carried back (design §4.3); persistent invalidity → recovery_required.
        streak = self._f12_rejection_streak(g, eid)
        if streak >= F12_MAX_REVISION_REJECTION_STREAK:
            store.update_execution(g, eid, status="recovery_required")
            store.append_event(
                g,
                "f12_recovery_required",
                {
                    "execution_id": eid,
                    "reason": f"{streak} consecutive rejected plan revisions",
                },
            )
            return
        write_settlement_checkpoint(
            g,
            execution_id=eid,
            kind="revision_rejected",
            wake=True,
            detail=reason,
        )

    def _f12_rejection_streak(self, g: Any, eid: str) -> int:
        conn, lock = g._conn, getattr(g, "_lock", None)

        def _q() -> Any:
            return conn.execute(
                "SELECT COUNT(*) FROM f12_plan_revision "
                "WHERE execution_id=? AND status='rejected' AND revision_no > "
                "COALESCE((SELECT MAX(revision_no) FROM f12_plan_revision "
                "          WHERE execution_id=? AND status='accepted'), 0)",
                (eid, eid),
            ).fetchone()

        if lock is None:
            row = _q()
        else:
            with lock:
                row = _q()
        return int(row[0] if row else 0)

    def _f12_accept_revision(
        self,
        source: SourceCoordinator,
        rid: str,
        turn_no: int,
        plan: dict[str, Any],
        wake: dict[str, Any],
    ) -> None:
        g = self.shared_graph
        ex = self._f12_execution() or {}
        eid = str(ex["execution_id"])
        gen = int(ex.get("execution_generation") or 1)
        parent = str(ex.get("active_revision_id") or "")
        if parent and parent != rid:
            store.set_revision_status(g, parent, "superseded")
        store.set_revision_status(g, rid, "accepted")
        revision = store.get_revision(g, rid) or {}
        store.update_execution(g, eid, active_revision_id=rid)
        f12_mat.materialize_revision(
            g,
            execution_id=eid,
            revision_id=rid,
            revision_no=int(revision.get("revision_no") or 0),
            plan=plan,
            generation=gen,
        )
        self._f12_apply_schedule(plan, rid)
        store.append_event(
            g,
            "f12_plan_revision_accepted",
            {
                "execution_id": eid,
                "revision_id": rid,
                "revision_no": int(revision.get("revision_no") or 0),
                "parent_revision_id": parent,
                "source_turn": turn_no,
            },
        )
        pins = [int(x) for x in plan.get("pinned_facts") or [] if int(x) > 0]
        if pins:
            try:
                g.pin_facts(
                    actor="f12-source",
                    fact_seqs=pins,
                    reason="f12 source coordinator pinned durable facts",
                )
            except Exception:
                pass
        cp_no = write_settlement_checkpoint(
            g,
            execution_id=eid,
            kind="schedule",
            schedule=list(plan.get("schedule") or []),
            wake=False,
            budget_highwater=self._f12_cost(),
            detail=f"revision {rid} schedule applied",
        )
        store.append_event(
            g,
            "f12_schedule_selected",
            {
                "execution_id": eid,
                "revision_id": rid,
                "schedule": list(plan.get("schedule") or []),
                "checkpoint_no": cp_no,
                "source_turn": turn_no,
            },
        )
        store.ack_wake(g, eid, int(wake["checkpoint_no"]))
        source.mark_applied(turn_no, rid)
        self._f12_handle_goal(plan, rid)

    def _f12_apply_schedule(self, plan: dict[str, Any], revision_id: str) -> None:
        """Selected pending tasks become 'scheduled'; previously-selected tasks
        the source no longer wants this round revert to 'pending' (they never
        start without re-selection). Running/terminal tasks are untouched."""
        g = self.shared_graph
        ex = self._f12_execution() or {}
        eid = str(ex["execution_id"])
        selected = {str(t) for t in plan.get("schedule") or []}
        for task in store.list_tasks(g, eid):
            tid = str(task["task_id"])
            status = str(task.get("status") or "")
            if tid in selected and status == "pending":
                if store.set_task_state(g, eid, tid, "scheduled", expected=("pending",)):
                    store.append_event(
                        g,
                        "f12_task_state_changed",
                        {
                            "execution_id": eid,
                            "task_id": tid,
                            "intent_id": task.get("intent_id"),
                            "from": "pending",
                            "to": "scheduled",
                            "revision_id": revision_id,
                        },
                    )
            elif tid not in selected and status == "scheduled":
                if store.set_task_state(g, eid, tid, "pending", expected=("scheduled",)):
                    store.append_event(
                        g,
                        "f12_task_state_changed",
                        {
                            "execution_id": eid,
                            "task_id": tid,
                            "intent_id": task.get("intent_id"),
                            "from": "scheduled",
                            "to": "pending",
                            "revision_id": revision_id,
                            "reason": "not re-selected in the current schedule",
                        },
                    )

    # -- goal review (design §8) -------------------------------------------------

    def _f12_handle_goal(self, plan: dict[str, Any], revision_id: str) -> None:
        g = self.shared_graph
        ex = self._f12_execution() or {}
        eid = str(ex["execution_id"])
        cfg = self._f12_cfg
        gs = str(plan.get("goal_status") or "continue")
        self._f12_record_review_disposition(plan)
        if gs in ("continue", "more_work_required"):
            store.update_execution(g, eid, goal_status="open")
            return
        # goal_satisfied / inconclusive → review path.
        independent = bool(cfg.get("independent_review")) and (
            f12_goal.should_use_independent(
                effect=int(cfg.get("effect") or 60),
                review_mode=str(plan.get("review_mode") or "self"),
            )
            or str(plan.get("review_mode") or "self") == "independent"
        )
        if not independent:
            verdict = gs if gs in f12_goal.VERDICTS else "inconclusive"
            f12_goal.record_self_review(
                g,
                execution_id=eid,
                revision_id=revision_id,
                verdict=verdict,
                detail=str(plan.get("summary") or ""),
            )
            if verdict == "goal_satisfied":
                store.update_execution(
                    g, eid, goal_status="satisfied", status="completing"
                )
                f12_mat.cancel_execution_tasks(
                    g, execution_id=eid, reason="goal_satisfied"
                )
            return
        # independent reviewer (advisory; never decides the run). Bounded: at
        # most 2 independent reviews per execution; past the cap the source's
        # verdict is recorded as a self review WITH the cap noted (not a silent
        # fallback — reviewer failure goes to recovery_required instead).
        reviews = store.list_goal_reviews(g, eid)
        pending = [
            r
            for r in reviews
            if r.get("status") == "pending" and r.get("mode") == "independent"
        ]
        indep_count = sum(1 for r in reviews if r.get("mode") == "independent")
        if pending:
            store.update_execution(g, eid, goal_status="review")
        elif indep_count >= 2:
            f12_goal.record_self_review(
                g,
                execution_id=eid,
                revision_id=revision_id,
                verdict=gs if gs in f12_goal.VERDICTS else "inconclusive",
                detail=(
                    "independent review cap reached; source verdict recorded. "
                    + str(plan.get("summary") or "")
                ),
            )
            if gs == "goal_satisfied":
                store.update_execution(
                    g, eid, goal_status="satisfied", status="completing"
                )
                f12_mat.cancel_execution_tasks(
                    g, execution_id=eid, reason="goal_satisfied"
                )
        else:
            f12_goal.request_independent_review(
                g,
                execution_id=eid,
                revision_id=revision_id,
                challenge_id=str(getattr(self.challenge, "id", "") or ""),
                goal_summary=str(plan.get("summary") or ""),
            )
            store.update_execution(g, eid, goal_status="review")

    def _f12_record_review_disposition(self, plan: dict[str, Any]) -> None:
        """§8.2: a negative/inconclusive independent verdict forces the source
        to add work, agree, or record an evidence-backed disagreement."""
        g = self.shared_graph
        ex = self._f12_execution() or {}
        eid = str(ex["execution_id"])
        reviews = [
            r
            for r in store.list_goal_reviews(g, eid)
            if r.get("status") == "decided"
            and r.get("mode") == "independent"
            and r.get("verdict") in ("more_work_required", "inconclusive")
            and not r.get("disposition")
        ]
        if not reviews:
            return
        latest = reviews[-1]
        gs = str(plan.get("goal_status") or "continue")
        has_work = bool(plan.get("schedule")) or bool(plan.get("tasks"))
        if gs == "goal_satisfied":
            disposition = "disputed"
            detail = str(plan.get("summary") or "")[:600]
        elif has_work or gs in ("continue", "more_work_required"):
            disposition = "acted"
            detail = "source scheduled follow-up work after the negative verdict"
        else:
            disposition = "agreed"
            detail = "source returned no further work"
        store.update_goal_review(
            g, str(latest["review_id"]), disposition=disposition, detail=detail
        )

    def _f12_latest_review_feedback(self, g: Any, eid: str) -> str:
        reviews = [
            r
            for r in store.list_goal_reviews(g, eid)
            if r.get("status") == "decided" and r.get("verdict")
        ]
        if not reviews:
            return ""
        latest = reviews[-1]
        return (
            f"goal review ({latest.get('mode')}): verdict={latest.get('verdict')} "
            f"detail={str(latest.get('detail') or '')[:400]}"
        )

    def _f12_poll_goal_reviews(self) -> None:
        g = self.shared_graph
        ex = self._f12_execution() or {}
        eid = str(ex["execution_id"])
        verdicts, new_seq = f12_goal.poll_independent_verdicts(
            g, execution_id=eid, after_seq=int(self._f12_review_poll_seq or 0)
        )
        self._f12_review_poll_seq = new_seq
        for v in verdicts:
            # the source must react to every independent verdict (design §8.2)
            write_settlement_checkpoint(
                g,
                execution_id=eid,
                kind="goal_review",
                wake=True,
                detail=f"verdict={v.get('verdict')} review={v.get('review_id')}",
            )
        for r in store.list_goal_reviews(g, eid):
            if (
                r.get("status") == "pending"
                and r.get("mode") == "independent"
                and f12_goal.reviewer_run_failed(g, review=r)
            ):
                f12_goal.mark_review_failed(
                    g, execution_id=eid, review_id=str(r["review_id"])
                )
                store.update_execution(g, eid, status="recovery_required")

    # -- operator directives (design: f12: control verbs) ------------------------

    def _f12_drain_directives(self) -> None:
        g = getattr(self, "shared_graph", None)
        ex = self._f12_execution()
        if g is None or ex is None:
            return
        eid = str(ex["execution_id"])
        try:
            rows = g.operator_directives(active_only=False)
        except Exception:
            rows = []
        for row in rows:
            text = str(row.get("text") or "").strip()
            if not text.startswith(F12_DIRECTIVE_PREFIX):
                continue
            did = str(row.get("directive_id") or "")
            if not did or did in self._f12_handled_directives:
                continue
            if str(row.get("status") or "") in (
                "acted",
                "superseded",
                "expired",
                "rejected",
            ):
                self._f12_handled_directives.add(did)
                continue
            self._f12_handled_directives.add(did)
            self._f12_apply_directive(g, eid, did, text)

    def _f12_apply_directive(self, g: Any, eid: str, directive_id: str, text: str) -> None:
        """One f12: control verb, exactly-once, with a queryable outcome. The
        directive text itself never becomes a flag or a verified fact."""
        body = text[len(F12_DIRECTIVE_PREFIX):].strip()
        verb, _, rest = body.partition(" ")
        verb = verb.strip().lower()
        rest = rest.strip()
        outcome = "acted"
        detail = ""
        if verb == "replan":
            write_settlement_checkpoint(
                g,
                execution_id=eid,
                kind="operator_replan",
                wake=True,
                detail=rest[:600] or "operator requested replan",
            )
            # an operator replan also revives a recovery_required execution.
            store.update_execution(g, eid, status="active")
            detail = "replan wake queued"
        elif verb == "review":
            ex = store.get_execution(g, eid) or {}
            pending = [
                r
                for r in store.list_goal_reviews(g, eid)
                if r.get("status") == "pending" and r.get("mode") == "independent"
            ]
            if pending:
                detail = "independent review already pending"
            else:
                f12_goal.request_independent_review(
                    g,
                    execution_id=eid,
                    revision_id=str(ex.get("active_revision_id") or ""),
                    challenge_id=str(getattr(self.challenge, "id", "") or ""),
                    goal_summary=rest[:600],
                )
                store.update_execution(g, eid, status="active", goal_status="review")
                detail = "independent review dispatched"
        elif verb == "stop":
            f12_mat.cancel_execution_tasks(g, execution_id=eid, reason="f12:stop")
            store.update_execution(g, eid, status="stopped")
            detail = "execution stopped; open tasks cancelled"
        elif verb == "approve":
            revision = store.get_revision(g, rest.split()[0] if rest else "")
            if revision is None or str(revision.get("execution_id")) != eid:
                outcome, detail = "rejected", "unknown revision_id"
            elif str(revision.get("status")) != "draft":
                outcome, detail = (
                    "rejected",
                    f"revision is {revision.get('status')}, not draft",
                )
            else:
                plan = json.loads(revision.get("plan_json") or "{}")
                revision_no = int(revision.get("revision_no") or 0)
                errors = validate_revision(
                    plan, existing_tasks=store.list_tasks(g, eid)
                )
                if errors:
                    outcome, detail = "rejected", "; ".join(errors)[:400]
                    store.set_revision_status(
                        g, str(revision["revision_id"]), "rejected",
                        reject_reason=detail,
                    )
                else:
                    self._f12_accept_revision(
                        SourceCoordinator(
                            g,
                            execution_id=eid,
                            run_id=str(getattr(self, "run_id", "") or ""),
                            generation=self._f12_generation(),
                            model=str(getattr(self, "reason_model", "") or ""),
                            llm=getattr(self, "llm", None),
                        ),
                        str(revision["revision_id"]),
                        int(revision.get("source_turn") or 0),
                        plan,
                        {"checkpoint_no": int(ex.get("checkpoint_no") or 0)},
                    )
                    detail = "revision approved and applied"
        elif verb == "reject":
            parts = rest.split(None, 1)
            revision = store.get_revision(g, parts[0]) if parts else None
            reason = parts[1] if len(parts) > 1 else "operator rejected"
            if revision is None or str(revision.get("execution_id")) != eid:
                outcome, detail = "rejected", "unknown revision_id"
            elif str(revision.get("status")) != "draft":
                outcome, detail = (
                    "rejected",
                    f"revision is {revision.get('status')}, not draft",
                )
            else:
                store.set_revision_status(
                    g, str(revision["revision_id"]), "rejected",
                    reject_reason=reason[:600],
                )
                store.append_event(
                    g,
                    "f12_plan_revision_rejected",
                    {
                        "execution_id": eid,
                        "revision_id": revision["revision_id"],
                        "source_turn": int(revision.get("source_turn") or 0),
                        "reasons": [f"operator: {reason[:400]}"],
                    },
                )
                write_settlement_checkpoint(
                    g,
                    execution_id=eid,
                    kind="revision_rejected",
                    wake=True,
                    detail=f"operator rejected: {reason[:400]}",
                )
                detail = "revision rejected; source will re-plan"
        else:
            outcome, detail = "rejected", f"unknown f12 verb {verb!r}"
        try:
            g.update_directive_status(
                directive_id=directive_id,
                status="acted" if outcome == "acted" else "rejected",
                actor="f12",
            )
        except Exception:
            pass
        store.append_event(
            g,
            "f12_operator_directive",
            {
                "execution_id": eid,
                "directive_id": directive_id,
                "verb": verb,
                "outcome": outcome,
                "detail": detail[:400],
            },
        )

    # -- run termination ---------------------------------------------------------

    def _f12_maybe_stop_run(self) -> bool:
        """When the F12 execution is terminal (completed/stopped) and nothing
        F12-managed is still running, end the run through the honest
        operator-stop path. recovery_required parks instead (operator can
        f12:replan); the product Flag/report gates are never bypassed."""
        if self._f12_run_stop_requested:
            return True
        g = getattr(self, "shared_graph", None)
        ex = self._f12_execution()
        if g is None or ex is None:
            return False
        status = str(ex.get("status") or "")
        if status == "completing":
            # completing → completed once no F12 task is still running.
            running = [
                t
                for t in store.list_tasks(g, str(ex["execution_id"]))
                if str(t.get("status")) == "running"
            ]
            if running:
                return False
            store.update_execution(g, str(ex["execution_id"]), status="completed")
            status = "completed"
        if status not in ("completed", "stopped"):
            return False
        # pentest: never cut the report reproduction / review pipeline.
        if str(getattr(self.challenge, "mode", "ctf") or "ctf") == "pentest":
            try:
                if self._pending_report_repro_count() > 0:
                    return False
            except Exception:
                pass
            try:
                if self._active_verifier_count() > 0 or self._active_review_count() > 0:
                    return False
            except Exception:
                pass
        self._f12_run_stop_requested = True
        self._operator_stop = True
        event = getattr(self, "_operator_event", None)
        if isinstance(event, asyncio.Event):
            event.set()
        return True

    # -- bus mirroring (deck visibility; graph events stay authoritative) ---------

    async def _f12_mirror_events(self) -> None:
        g = getattr(self, "shared_graph", None)
        emit = getattr(self, "_emit_coord_bb", None)
        if g is None or not callable(emit):
            return
        try:
            events = g.events_since(
                int(self._f12_mirror_seq or 0), kinds=list(store.F12_FEATURE_KINDS)
            )
        except Exception:
            events = []
        for event in events:
            payload = event.get("payload") or {}
            if not isinstance(payload, dict):
                payload = {}
            fields = {
                k: v for k, v in payload.items() if k not in ("kind", "graph_seq")
            }
            try:
                await emit(
                    str(event.get("kind") or "f12"),
                    graph_seq=int(event.get("seq") or 0),
                    **fields,
                )
            except Exception:
                pass
            self._f12_mirror_seq = max(
                int(self._f12_mirror_seq or 0), int(event.get("seq") or 0)
            )

    def _f12_cost(self) -> float:
        fn = getattr(self, "_current_cost_usd", None)
        if callable(fn):
            try:
                return float(fn() or 0.0)
            except Exception:
                return 0.0
        return 0.0


# ---------------------------------------------------------------------------
# Ablation arms (design §13.1) — each is a pure class-path-loadable subclass
# with class-owned mechanism flips. No env keys, no product defaults.
# ---------------------------------------------------------------------------


class SwarmF12SelfReviewOnly(SwarmF12):
    """A3: F12 without the independent Goal Reviewer (self review only)."""

    architecture_name = "f12-checkpointed-dag-self-review"
    f12_mechanism_defaults = {
        **SwarmF12.f12_mechanism_defaults,
        "independent_review": False,
    }


class SwarmF12FixedProfile(SwarmF12):
    """A4: F12 with joint tier routing removed (product default routing)."""

    architecture_name = "f12-checkpointed-dag-fixed-profile"

    def _effect_capability_pick_engine(self, *a: Any, **k: Any) -> None:
        return None  # product _pick_engine fallback owns every pick


class SwarmF12NoSettlementWake(SwarmF12):
    """A5: F12 keeps the DAG but wakes the source only on all-terminal /
    operator replan / recovery — not on every settlement."""

    architecture_name = "f12-checkpointed-dag-no-settlement-wake"
    f12_mechanism_defaults = {
        **SwarmF12.f12_mechanism_defaults,
        "settlement_wake": False,
    }


class SwarmF12Serial(SwarmF12):
    """A6: F12 with serial scheduling (concurrency target forced to 1)."""

    architecture_name = "f12-checkpointed-dag-serial"
    f12_mechanism_defaults = {
        **SwarmF12.f12_mechanism_defaults,
        "serial": True,
    }
