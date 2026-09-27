"""Intent-to-worker scheduling stage for the coordinator loop."""

from __future__ import annotations

from muteki.solver.vuln_report import report_id_from_intent
from muteki.solver.worker_profiles import worker_identity_event_fields

async def dispatch_stage(self, state) -> str:
    from functools import partial
    from muteki.swarm.coordinator_state import (
        emit_scheduler_bb,
        running_engines as scheduler_running_engines,
        stop_for_budget as scheduler_stop_for_budget,
    )
    from muteki.swarm.swarm_support import (
        ContextCapabilityUnavailable,
        ControlShutdownIncomplete,
        RequiredContextUnavailable,
        WorkerBudgetExhausted,
        WorkerSpawnRejected,
    )

    emit_bb = partial(emit_scheduler_bb, self, state)
    running_engines = partial(scheduler_running_engines, state)
    stop_for_budget = partial(scheduler_stop_for_budget, self, state)

    async def _block_intent_context(iid: str, *, missing: list[str],
                                    reason: str, phase: str) -> None:
        """Block an intent whose context/capability gap is standing config,
        not transient scarcity — exactly one graph transition + one bb event,
        and the intent is NOT requeued (blocked rows drop out of every
        dispatchable/claimable query)."""
        blocked = False
        try:
            blocked = bool(self.shared_graph.block_intent_context(
                actor="coordinator", intent_id=iid,
                missing=list(missing), reason=reason))
        except Exception:
            blocked = False
        if blocked:
            await emit_bb("blocked_context_capability", intent_id=iid,
                          missing=list(missing), reason=reason, phase=phase)
    # ── Phase: Explore — fill free slots with intent workers ─────
    if getattr(self.challenge, "mode", "ctf") == "pentest":
        await self._drain_report_pipeline()
        state.open_intents = self._open_intents()
    # Fixed mode ramps by `explore_spawn_batch` (default 1) per loop iteration.
    # Auto mode drains currently ready, non-conflicting intents only while the
    # run-wide Worker ceiling has free slots.
    state.open_intents = self._dispatchable_open_intents(
        state.open_intents, state.tasks)
    # Lease expiry is a planning boundary.  Keep the reopened Intent queued
    # until Reason has evaluated the checkpoint against its stop condition;
    # otherwise the next loop immediately starts a replacement Worker even
    # when the checkpoint already proves this branch complete.
    ctf_mode = getattr(self.challenge, "mode", "ctf") == "ctf"
    checkpoint_replan_ids = (
        set() if ctf_mode else set(state.checkpoint_replan_wm)
    )
    if checkpoint_replan_ids:
        state.open_intents = [
            intent for intent in state.open_intents
            if str(intent.get("intent_id") or "") not in checkpoint_replan_ids
        ]
    state.open_intents = self._capacity_dispatchable_open_intents(state.open_intents, state.tasks)
    # Cognitive cluster planner: reorder open intents so the next
    # explore worker takes the highest-evidence-value direction, not
    # FIFO creation order. Stage-4b: lives behind the experimental hook
    # registry (ExperimentalSwarm + the cluster-planner flag);
    # default no-op, state.open_intents is mutated in place when hooked.
    self._experiment_call(
        "dispatch_reorder", state, running_engines(), default=None)
    spawned_this_round = 0
    batch_engines: list[str] = []
    scan_remaining = len(state.open_intents)
    dispatch_batch = (
        scan_remaining
        if self._auto_dispatch_enabled()
        else self.explore_spawn_batch
    )
    while (state.open_intents
           and spawned_this_round < dispatch_batch
           and scan_remaining > 0):
        intent = state.open_intents.pop(0)
        scan_remaining -= 1
        iid = intent["intent_id"]
        worker_class = str(intent.get("worker_class") or "code")
        if worker_class == "review":
            worker_mode = "review"
            worker_role = "review"
        elif worker_class == "verifier":
            worker_mode = (
                "report_reproducer"
                if report_id_from_intent(iid)
                else "fact_verifier"
            )
            worker_role = "verifier"
        else:
            worker_mode = "explore"
            worker_role = "explore"
        required_capabilities = set() if ctf_mode else {
            str(item).strip().casefold()
            for item in (intent.get("requires_capabilities") or [])
            if str(item).strip()
        }
        if required_capabilities and self.shared_graph is not None:
            try:
                available_capabilities = self.shared_graph.active_capability_keys(
                    str(getattr(self, "_target_epoch", "") or ""))
            except Exception:
                available_capabilities = set()
            missing_capabilities = sorted(
                required_capabilities - set(available_capabilities))
            if missing_capabilities:
                try:
                    self.shared_graph.report_capability_gap(
                        actor="coordinator", intent_id=iid,
                        target_epoch=str(getattr(self, "_target_epoch", "") or "1"),
                        description=(
                            f"intent {iid} waits for reusable capabilities: "
                            f"{', '.join(missing_capabilities)}"
                        ),
                        required_capabilities=missing_capabilities,
                        consumers=[iid],
                    )
                except Exception:
                    pass
                state.open_intents.append(intent)
                continue
        intent_lane = "" if ctf_mode else str(intent.get("lane_key") or "")
        intent_resource = "" if ctf_mode else str(intent.get("resource_key") or "")
        if intent_lane and self.shared_graph is not None:
            try:
                intent_lane = self.shared_graph.normalize_lane_key(intent_lane)
                lane_busy = any(
                    str(row.get("lane_key") or "") == intent_lane
                    for row in self.shared_graph.active_lanes()
                )
            except Exception:
                lane_busy = False
            if lane_busy:
                # A locked resource is transient queue pressure. Keep the intent
                # open and scan a different lane without constructing a Worker,
                # claiming the row, or creating lane-deferred/revive churn.
                state.open_intents.append(intent)
                continue
        if self.shared_graph is not None and (intent_resource or intent_lane):
            try:
                conflict = self.shared_graph.check_resource_conflicts(
                    resource_key=intent_resource or intent_lane,
                    lane_key=intent_lane,
                )
            except Exception:
                conflict = {"conflict": False}
            if conflict.get("conflict"):
                # Workers can acquire the unified resource lock from inside a
                # turn. Do not start a declared-lane successor while that
                # resource is still owned, even when no coordinator lane row
                # exists yet.
                state.open_intents.append(intent)
                continue
        failed_engines = list(state.intent_failed_engines.get(iid, set()))
        excluded_engines = list(
            state.role_quarantined_engines.get(worker_role, set()))
        try:
            engine = self._pick_engine(
                running_engines(), state.healthy, role=worker_role,
                intent_id=iid, lane=intent_lane,
                intent=intent,
                avoid_engines=[*failed_engines, *batch_engines],
                exclude_engines=excluded_engines)
        except ContextCapabilityUnavailable as exc:
            # The secure-context / role-capability filter emptied the candidate
            # pool for a STANDING config reason. Block the intent once; only
            # generic engine scarcity (RuntimeError below) is requeued.
            await _block_intent_context(
                iid, missing=(getattr(exc, "missing", None)
                              or ["secure_prompt_transport"]),
                reason=str(exc), phase=worker_mode)
            continue
        except RuntimeError as exc:
            await emit_bb("worker_spawn_rejected", reason=str(exc),
                           phase=worker_mode, intent_id=iid)
            # Temporary role/profile scarcity for one queue head must not block
            # a different role that can run in the same pass.  Rotate locally;
            # the durable intent remains open and is reconsidered next tick.
            state.open_intents.append(intent)
            continue
        # build the worker FIRST so we can claim the intent under ITS
        # unique solver_id — that makes the worker the intent's OWNER, so
        # conclude_intent's owner-fence lets exactly this worker conclude it.
        try:
            worker_kwargs = {
                "mode": worker_mode,
                "intent_goal": intent["goal"],
                "intent_id": iid,
            }
            if (
                getattr(self.challenge, "mode", "ctf") == "ctf"
                and worker_mode == "fact_verifier"
            ):
                worker_kwargs["timeout_override"] = min(
                    int(self.explore_timeout), 120
                )
            if (
                getattr(self.challenge, "mode", "ctf") != "ctf"
                and worker_mode == "explore"
                and worker_class != "shell_agent"
            ):
                raw_priority = intent.get("priority")
                if isinstance(raw_priority, (int, float)):
                    priority = (
                        "high" if raw_priority >= 50
                        else "low" if raw_priority < 0
                        else "normal"
                    )
                else:
                    priority = str(raw_priority or "normal").lower()
                timeout_cap = {
                    "high": 480,
                    "normal": 360,
                    "low": 240,
                }.get(priority, 360)
                if intent_lane:
                    timeout_cap = min(timeout_cap, 300)
                worker_kwargs["timeout_override"] = min(
                    int(self.explore_timeout), timeout_cap
                )
            if intent_lane:
                worker_kwargs["lane"] = intent_lane
            w = self._make_cli_worker(engine, **worker_kwargs)
            w.intent_worker_class = worker_class
            intent_batch = int(
                state.ctf_intent_batches.get(iid, state.ctf_batch_id) or 0
            )
            w._ctf_batch_id = intent_batch
            self._apply_step_contract(w, intent)
        except RequiredContextUnavailable as exc:
            # Reconcile again at the live failure boundary.  TTL expiry,
            # explicit EXPIRE_CONTEXT, or an already bound/unknown exact
            # resource is terminal and must close its graph edge now.
            # A still-open intent after reconcile carries a required-context
            # dependency the current control plane cannot deliver: block it
            # (audit + one bb event, operator reopen reverses) instead of
            # leaving it open to loop spawn/reject every dispatch pass.
            await self._reconcile_control_continuations()
            try:
                still_open = any(
                    str(row.get("intent_id") or "") == iid
                    for row in self._open_intents())
            except Exception:
                still_open = True
            if still_open:
                await _block_intent_context(
                    iid,
                    missing=(list(getattr(exc, "missing", None) or [])
                             or ["required_operator_context"]),
                    reason=str(exc), phase=worker_mode)
            else:
                await emit_bb(
                    "worker_spawn_rejected", reason=str(exc),
                    engine=str(engine), phase=worker_mode,
                    intent_id=iid, deferred=False, retired=True)
            continue
        except WorkerSpawnRejected as exc:
            # intent not claimed yet (claim happens after build) → just
            # defer this spawn; the intent stays open for a later worker while a
            # different queue item may still use the available capacity now.
            await emit_bb("worker_spawn_rejected", reason=str(exc),
                           engine=str(engine), phase=worker_mode)
            state.open_intents.append(intent)
            continue
        except WorkerBudgetExhausted as exc:
            terminal = await stop_for_budget(str(exc))
            return "break" if terminal else "continue"
        # Atomic open→claimed transition; ownership is released only by the
        # runtime retirement paths below.
        won = False
        try:
            won = self.shared_graph.claim_intent(
                worker=w.solver_id, intent_id=iid)
        except Exception:
            won = False
        if not won:
            if not await self._retire_worker_account(
                    w, reason="intent claim not acquired"):
                raise ControlShutdownIncomplete(
                    "claim-lost worker rollback incomplete")
            continue  # someone else holds a live claim; drop this worker
        lane_key = str(intent.get("lane_key") or "")
        locked_lane = ""
        if (not ctf_mode and lane_key and worker_mode != "review"
                and self.shared_graph is not None):
            try:
                lock = self.shared_graph.lock_lane(  # type: ignore[attr-defined]
                    actor="coordinator",
                    lane_key=lane_key,
                    risk_class=str(intent.get("risk_class") or ""),
                    owner_worker=w.solver_id,
                    owner_intent=iid,
                    lease_s=float(self.explore_timeout) + 300.0,
                )
            except Exception:
                lock = {"acquired": False, "held_seq": 0}
            if not lock.get("acquired"):
                await emit_bb(
                    "intent_lane_waiting",
                    intent_id=iid,
                    lane_key=lane_key,
                    held_by=str(lock.get("held_by") or ""),
                    held_seq=int(lock.get("held_seq") or 0),
                    cooldown_until=float(lock.get("cooldown_until") or 0.0),
                )
                if not await self._retire_worker_account(
                        w, intent_id=iid,
                        reason="lane unavailable before worker process start"):
                    raise ControlShutdownIncomplete(
                        "lane-waiting worker rollback incomplete")
                state.open_intents.append(intent)
                continue
            locked_lane = str(lock.get("lane_key") or lane_key)
            try:
                self.shared_graph.add_coordinator_directive(  # type: ignore[attr-defined]
                    actor="coordinator",
                    action="lane_lock",
                    directive=(
                        f"lane {locked_lane} is exclusively held by {w.solver_id}; "
                        "do not start destructive/exclusive work on that resource."
                    ),
                    priority="high",
                )
            except Exception:
                pass
            await emit_bb("lane_locked", **lock, intent_id=iid)
        t = await self._schedule_control_worker(
            w, name=f"{worker_mode}-{engine}",
            intent_id=iid, lane_key=locked_lane)
        state.tasks[t] = engine
        state.task_solvers[t] = w
        state.task_intents[t] = iid
        if locked_lane:
            state.task_lanes[t] = locked_lane
        if worker_mode == "review":
            self._active_review_tasks.add(t)
            self._review_workers_spawned += 1
        elif worker_mode in {"fact_verifier", "report_reproducer"}:
            self._active_verifier_tasks.add(t)
            self._verifier_workers_spawned += 1
        elif (
            getattr(self.challenge, "mode", "ctf") == "ctf"
            and intent_batch == state.ctf_batch_id
            and not state.ctf_batch_sealed
        ):
            state.ctf_batch_spawned += 1
        spawned_this_round += 1
        batch_engines.append(str(engine))
        await emit_bb("worker_spawned", worker=w.solver_id,
                       phase=worker_mode, intent_id=iid,
                       dispatch_mode=self.dispatch_mode,
                       timeout_s=int(getattr(w, "timeout", 0) or 0),
                       ctf_batch_id=int(getattr(w, "_ctf_batch_id", 0) or 0),
                       ctf_batch_spawned=state.ctf_batch_spawned,
                       worker_role=(
                           "review" if worker_mode == "review"
                           else "verifier" if worker_mode in {
                               "fact_verifier", "report_reproducer"}
                           else "worker"),
                       **worker_identity_event_fields(w))
        state.open_intents = self._capacity_dispatchable_open_intents(state.open_intents, state.tasks)
    if (
        getattr(self.challenge, "mode", "ctf") == "ctf"
        and not state.ctf_batch_sealed
        and state.ctf_batch_spawned > 0
    ):
        state.ctf_batch_sealed = True
        await emit_bb(
            "ctf_batch_sealed",
            ctf_batch_id=state.ctf_batch_id,
            cohort_size=state.ctf_batch_spawned,
        )
    return "proceed"
