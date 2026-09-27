"""Run pause/resume and process freeze/thaw control actions."""

from __future__ import annotations

import os
from typing import Any

from muteki.swarm.coordinator_control_command import ControlCommandContext


async def _handle_run_lifecycle_control(self, context: ControlCommandContext) -> bool:
    cmd = context.cmd
    action = context.action
    # operator STOP/COMPLETE: end the run gracefully. Unlike a steer
    # (which only guides workers), this terminates the coordinator loop —
    # the lever for a challenge that never yields a gated flag. Wake the
    # coordinator so it checks the flag at its next iteration boundary.
    if action in ("stop", "complete"):
        self._operator_stop = True
        self._pending_help = []
        if self._operator_event is not None:
            self._operator_event.set()
        self._ack_control(
            cmd, state="effect_observed",
            detail="coordinator termination latch observed",
            metadata={"effect": "termination_requested"})
        return True
    # operator PAUSE/RESUME (#5): soft-pause the coordinator's spawn loop.
    # pause sets a flag the loop checks at its top (no new workers until
    # resume); it does NOT kill running workers or end the run. resume
    # clears it and wakes the loop. This is the contract that actually fits
    # a single-shot swarm — see _operator_paused. Still broadcast on the
    # InsightBus below (the deck reflects pause/resume; a live standby
    # worker process signalling is exclusively FREEZE/THAW; PAUSE and
    # RESUME are dispatcher latches and never touch process state.
    if action == "pause":
        self._operator_paused = True
        active = self._control_target_solvers("global")
        paused_ids: list[str] = []
        pause_failures: list[str] = []
        for worker in active:
            pause_runtime = getattr(worker, "pause_runtime", None)
            if not callable(pause_runtime):
                continue
            sid = str(getattr(worker, "solver_id", "") or "")
            try:
                applied = pause_runtime()
                if hasattr(applied, "__await__"):
                    applied = await applied
            except Exception:
                pause_failures.append(sid)
                continue
            if applied:
                paused_ids.append(sid)
            else:
                pause_failures.append(sid)
        # surface it on the board so the rail shows "paused"
        try:
            await self._emit_coord_bb(
                "operator_paused",
                reason="operator paused the swarm "
                       "(no new workers until resume)")
        except Exception:
            pass
        self._ack_control(
            cmd,
            state="partial" if pause_failures else "effect_observed",
            detail=(
                "dispatcher quiesced; structured Runtime turns interrupted "
                "with resumable sessions retained"
                if paused_ids else "dispatcher quiesced; no active structured Runtime"
            ),
            target_ids=paused_ids,
            metadata={
                "effect": "run_quiesced",
                "runtime_paused": paused_ids,
                "runtime_pause_failures": pause_failures,
            })
        return True
    if action == "resume":
        self._operator_paused = False
        self._operator_draining = False
        active = self._control_target_solvers("global")
        resumed_ids: list[str] = []
        resume_failures: list[str] = []
        for worker in active:
            resume_runtime = getattr(worker, "resume_runtime", None)
            if not callable(resume_runtime):
                continue
            sid = str(getattr(worker, "solver_id", "") or "")
            try:
                applied = resume_runtime()
                if hasattr(applied, "__await__"):
                    applied = await applied
            except Exception:
                resume_failures.append(sid)
                continue
            if applied:
                resumed_ids.append(sid)
            else:
                resume_failures.append(sid)
        if self._operator_event is not None:
            self._operator_event.set()
        self._ack_control(
            cmd,
            state="partial" if resume_failures else "effect_observed",
            detail="dispatcher and resumable structured Runtime sessions resumed",
            target_ids=resumed_ids,
            metadata={
                "effect": "run_resumed",
                "runtime_resumed": resumed_ids,
                "runtime_resume_failures": resume_failures,
            })
        return True
    return False


async def _handle_freeze_control(self, context: ControlCommandContext) -> bool:
    cmd = context.cmd
    action = context.action
    target = context.target
    command_id = context.command_id
    # FREEZE is stronger than pause: quiesce dispatch AND SIGSTOP the
    # selected live subprocess groups.  The graph receives a finite
    # lease guard in the same operation, so frozen owners cannot lose
    # claims merely because the operator paused wall-clock progress.
    if action == "freeze":
        kind, _value = self._control_scope_parts(target)
        run_wide = kind in {"global", "run", "challenge"}
        freeze_key = "__run__" if run_wide else target
        if (self._freeze_suspensions
                and freeze_key not in self._freeze_suspensions):
            self._ack_control(
                cmd, state="failed",
                detail="another freeze scope is active; thaw it before changing scope",
                metadata={"code": "overlapping_freeze_scope"})
            return True
        if freeze_key in self._freeze_suspensions or self._control_frozen:
            already = self._control_target_solvers(target)
            target_ids = [str(getattr(w, "solver_id", "") or "")
                          for w in already
                          if bool(getattr(w, "_paused", False))]
            self._ack_control(
                cmd, state="effect_observed",
                detail="requested scope is already frozen",
                target_ids=target_ids,
                metadata={"effect": "already_frozen"})
            return True
        if run_wide:
            self._operator_paused = True
            self._control_frozen = True
            if self._budget_suspend_started is None:
                import time
                self._budget_suspend_started = time.monotonic()
        confirmed, failures = self._set_control_frozen(target, True)
        if not run_wide and not confirmed and not failures:
            self._ack_control(
                cmd, state="unknown",
                detail="no matching live worker to freeze",
                metadata={"effect": "no_effect"})
            return True
        if failures:
            # Freeze is all-or-nothing. Never leave a hidden subset of
            # workers stopped while the UI correctly refuses to call the
            # command effect_observed.
            _rolled_back, rollback_failures = self._set_control_frozen(
                target, False)
            if run_wide and not rollback_failures:
                self._operator_paused = False
                self._control_frozen = False
                self._budget_suspend_started = None
            if rollback_failures:
                # POSIX/container signalling can itself fail during
                # compensation. Fail closed (dispatcher remains paused)
                # and report the split state explicitly; never claim the
                # rollback succeeded when a process may still be stopped.
                if run_wide:
                    self._operator_paused = True
                    self._control_frozen = True
                containment_cancel_requested = (
                    self._contain_unfrozen_control_workers(target))
                suspension_id = command_id or f"freeze:{target}"
                import time
                self._freeze_suspensions[freeze_key] = suspension_id
                self._freeze_started_at[freeze_key] = time.monotonic()
                lease_affected = 0
                lease_guard_failed = False
                if self.shared_graph is not None:
                    try:
                        lease_scope, lease_scope_id = (
                            self._lease_scope_for_control(target))
                        lease_result = self.shared_graph.suspend_active_leases(
                            actor="control", suspension_id=suspension_id,
                            scope_kind=lease_scope,
                            scope_id=lease_scope_id,
                            guard_s=1_000_000_000_000.0,
                            reason="operator freeze containment")
                        lease_affected = int(
                            lease_result.get("affected") or 0)
                    except Exception:
                        lease_guard_failed = True
                self._ack_control(
                    cmd, state="partial",
                    detail=("worker freeze failed and rollback could not "
                            "be fully confirmed; dispatcher held and "
                            "cancellation requested where possible; "
                            "process exit unconfirmed"),
                    target_ids=self._control_paused_ids(target),
                    metadata={
                        "code": "freeze_rollback_unconfirmed",
                        "apply_failures": failures,
                        "rollback_failures": rollback_failures,
                        "lease_affected": lease_affected,
                        "lease_guard_failed": lease_guard_failed,
                        "containment_cancel_requested":
                            containment_cancel_requested,
                    })
                return True
            self._ack_control(
                cmd, state="failed",
                detail="worker freeze could not be confirmed; rolled back",
                target_ids=confirmed,
                metadata={"code": "freeze_confirmation_failed"})
            return True
        suspension_id = command_id or f"freeze:{target}"
        self._freeze_suspensions[freeze_key] = suspension_id
        import time
        self._freeze_started_at[freeze_key] = time.monotonic()
        lease_info: dict[str, Any] = {"affected": 0}
        lease_failed = False
        if self.shared_graph is not None:
            try:
                scope_kind, scope_id = self._lease_scope_for_control(target)
                try:
                    guard_s = float(os.environ.get(
                        "MUTEKI_FREEZE_LEASE_GUARD_SECONDS",
                        "1000000000000"))
                except (TypeError, ValueError):
                    guard_s = 1_000_000_000_000.0
                guard_s = max(
                    60.0, min(1_000_000_000_000.0, guard_s))
                lease_info = self.shared_graph.suspend_active_leases(
                    actor="control", suspension_id=suspension_id,
                    scope_kind=scope_kind, scope_id=scope_id,
                    guard_s=guard_s, reason="operator freeze")
            except Exception:
                lease_failed = True
        if lease_failed:
            _rolled_back, rollback_failures = self._set_control_frozen(
                target, False)
            if not rollback_failures:
                self._freeze_suspensions.pop(freeze_key, None)
                self._freeze_started_at.pop(freeze_key, None)
            if run_wide and not rollback_failures:
                self._operator_paused = False
                self._control_frozen = False
                self._budget_suspend_started = None
            if rollback_failures:
                containment_cancel_requested = (
                    self._contain_unfrozen_control_workers(target))
                self._ack_control(
                    cmd, state="partial",
                    detail=("lease guard failed and worker rollback was "
                            "not fully confirmed; dispatcher held and "
                            "cancellation requested where possible; "
                            "process exit unconfirmed"),
                    target_ids=self._control_paused_ids(target),
                    metadata={
                        "code": "lease_guard_rollback_unconfirmed",
                        "rollback_failures": rollback_failures,
                        "containment_cancel_requested":
                            containment_cancel_requested,
                    })
                return True
            self._ack_control(
                cmd, state="failed",
                detail="lease guard failed; worker freeze rolled back",
                target_ids=confirmed,
                metadata={"code": "lease_guard_failed"})
            return True
        if self._operator_event is not None:
            self._operator_event.set()
        self._ack_control(
            cmd, state="effect_observed",
            detail=(f"froze {len(confirmed)} live worker(s); "
                    f"guarded {int(lease_info.get('affected') or 0)} lease(s)"),
            target_ids=confirmed,
            metadata={"effect": "run_frozen" if run_wide else "workers_frozen",
                      "lease_affected": int(lease_info.get("affected") or 0),
                      "failures": failures})
        return True
    return False


async def _handle_thaw_control(self, context: ControlCommandContext) -> bool:
    cmd = context.cmd
    action = context.action
    target = context.target
    if action == "thaw":
        kind, _value = self._control_scope_parts(target)
        run_wide = kind in {"global", "run", "challenge"}
        freeze_key = "__run__" if run_wide else target
        suspension_id = self._freeze_suspensions.get(freeze_key, "")
        started = self._freeze_started_at.get(freeze_key)
        if not suspension_id:
            self._ack_control(
                cmd, state="unknown",
                detail="requested scope has no active freeze suspension",
                metadata={"effect": "no_effect"})
            return True
        confirmed, failures = self._set_control_frozen(target, False)
        import time
        duration = max(0.0, time.monotonic() - started) if started else 0.0
        if failures:
            # A partially resumed process set is more dangerous than a
            # failed thaw receipt: compensate with SIGSTOP and preserve
            # the canonical suspension/lease guard for an explicit retry.
            _refrozen, refreeze_failures = self._set_control_frozen(
                target, True)
            if refreeze_failures:
                containment_cancel_requested = (
                    self._contain_unfrozen_control_workers(target))
                self._ack_control(
                    cmd, state="partial",
                    detail=("worker thaw and compensating re-freeze both "
                            "failed; dispatcher held and cancellation "
                            "requested where possible; process exit "
                            "unconfirmed"),
                    target_ids=self._control_paused_ids(target),
                    metadata={
                        "code": "thaw_compensation_unconfirmed",
                        "thaw_failures": failures,
                        "refreeze_failures": refreeze_failures,
                        "containment_cancel_requested":
                            containment_cancel_requested,
                    })
                return True
            self._ack_control(
                cmd, state="failed",
                detail="worker thaw could not be confirmed; freeze preserved",
                target_ids=confirmed,
                metadata={"code": "thaw_confirmation_failed"})
            return True
        lease_info: dict[str, Any] = {"affected": 0, "skipped": []}
        lease_failed = False
        if self.shared_graph is not None:
            try:
                lease_info = self.shared_graph.resume_suspended_leases(
                    actor="control", suspension_id=suspension_id,
                    duration_s=duration, reason="operator thaw")
            except Exception:
                lease_failed = True
        if lease_failed:
            _refrozen, refreeze_failures = self._set_control_frozen(
                target, True)
            if refreeze_failures:
                containment_cancel_requested = (
                    self._contain_unfrozen_control_workers(target))
                self._ack_control(
                    cmd, state="partial",
                    detail=("lease restoration failed and compensating "
                            "re-freeze was not fully confirmed; dispatcher "
                            "held and cancellation requested where possible; "
                            "process exit unconfirmed"),
                    target_ids=self._control_paused_ids(target),
                    metadata={
                        "code": "lease_restore_compensation_unconfirmed",
                        "refreeze_failures": refreeze_failures,
                        "containment_cancel_requested":
                            containment_cancel_requested,
                    })
                return True
            self._ack_control(
                cmd, state="failed",
                detail="lease restoration failed; worker freeze preserved",
                target_ids=confirmed,
                metadata={"code": "lease_restore_failed"})
            return True
        # Commit the in-memory transition only after both OS process
        # groups and the graph lease journal have confirmed the thaw.
        self._freeze_started_at.pop(freeze_key, None)
        self._freeze_suspensions.pop(freeze_key, None)
        if run_wide:
            self._operator_paused = False
            self._control_frozen = False
            if self._budget_suspend_started is not None:
                self._budget_suspended_total += max(
                    0.0, time.monotonic() - self._budget_suspend_started)
                self._budget_suspend_started = None
        if self._operator_event is not None:
            self._operator_event.set()
        skipped = len(lease_info.get("skipped") or [])
        # A skipped lease means ownership changed while frozen.  That is
        # an audited, safe compare-and-swap outcome—not a partial thaw.
        state = "effect_observed"
        self._ack_control(
            cmd, state=state,
            detail=(f"thawed {len(confirmed)} live worker(s) after "
                    f"{duration:.3f}s; restored "
                    f"{int(lease_info.get('affected') or 0)} lease(s)"),
            target_ids=confirmed,
            metadata={"effect": "run_thawed" if run_wide else "workers_thawed",
                      "duration_s": duration,
                      "lease_affected": int(lease_info.get("affected") or 0),
                      "lease_skipped": skipped,
                      "failures": failures})
        return True
    return False
