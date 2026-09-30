"""Finished-run control application extracted from RunManager control setup."""

from __future__ import annotations

import asyncio
import logging
from typing import Any, Optional

from muteki.control import SQLiteControlJournal
from apps.web.run_state import Run

LOG = logging.getLogger("apps.web.run_manager")


async def apply_standby_control(
    self,
    *,
    run: Run,
    journal: SQLiteControlJournal,
    claim_timeout: float,
    wire: dict[str, Any],
) -> Any:
    action = str(wire.get("action") or "").lower()
    busy = self._standby_busy(run)
    target = str(wire.get("target") or "global")
    exact_text = str(wire.get("text") or wire.get("hint") or "").strip()
    if action == "ask" and not exact_text:
        return {
            "state": "unknown",
            "detail": "ask requires a question",
            "target_ids": [],
            "metadata": {"code": "followup_question_required"},
        }

    if action in self._OFFLINE_CONTROL_ACTIONS:
        # The actor already expired typed ContextResources. Atomically
        # expire the evidence-graph projection too so a restart cannot
        # resurrect guidance that an offline command claimed to clear.
        graph_db = self.graph_dir(run.run_id) / "shared_graph.db"
        expired_directives: list[str] = []
        companion = (wire.get("_control_companion")
                     if isinstance(wire.get("_control_companion"), dict)
                     else {})
        expired_context_count = int(
            companion.get("expired_context_count") or 0)
        graph = None
        if graph_db.exists():
            try:
                from muteki.models.solve_graph import Challenge
                from muteki.swarm.shared_graph import SQLiteSharedGraph
                graph = SQLiteSharedGraph.open(
                    db_path=graph_db,
                    challenge=Challenge(
                        id=run.run_id,
                        name=run.name or run.run_id,
                        category=run.category or "web",
                    ),
                )
                source_command_id = str(
                    wire.get("command_id") or "").strip()
                matched_source_ids = sorted({
                    str(value or "").strip()
                    for value in (
                        companion.get("matched_source_command_ids") or [])
                    if str(value or "").strip()
                })
                if source_command_id:
                    clear_result = graph.apply_standing_clear(
                        command_id=source_command_id,
                        actor="operator",
                        text=("" if exact_text.startswith("secret://")
                              else exact_text),
                        eligible_command_ids=(
                            matched_source_ids if exact_text else None),
                        match_by_source_ids=exact_text.startswith(
                            "secret://"),
                    )
                    expired_directives = list(
                        clear_result.get("expired_directives") or [])
                else:
                    expired_directives = graph.expire_standing_directives(
                        actor="operator", text=exact_text)
                remaining = [
                    row for row in graph.operator_directives(active_only=True)
                    if row.get("standing")
                    and (not exact_text or row.get("text") == exact_text)
                ]
                if remaining:
                    raise RuntimeError("standing directive remained active")
            except Exception:
                return {
                    "state": ("partial" if expired_context_count else "failed"),
                    "detail": "offline standing guidance expiration failed",
                    "target_ids": [],
                    "metadata": {
                        "code": "guidance_graph_expire_failed",
                        "expired_context_count": expired_context_count,
                    },
                }
            finally:
                if graph is not None:
                    graph.close()
        remaining_context = [
            resource for resource in journal.context_resources(active_only=True)
            if resource.standing
            and (not exact_text or resource.content == exact_text)
        ]
        if remaining_context:
            return {
                "state": ("partial" if (
                    expired_directives or expired_context_count) else "failed"),
                "detail": "offline standing context expiration was not confirmed",
                "target_ids": [],
                "metadata": {
                    "code": "guidance_context_expire_failed",
                    "expired_context_count": expired_context_count,
                },
            }
        return {
            "state": "effect_observed",
            "detail": "offline standing guidance durably expired",
            "target_ids": [],
            "metadata": {
                "effect": "guidance_cleared",
                "expired_directives": expired_directives,
                "expired_context_count": expired_context_count,
            },
        }

    if not self._standby_scope_matches_winner(run, target):
        return {
            "state": "unknown",
            "detail": "standby winner identity does not match control scope",
            "target_ids": [],
            "metadata": {"code": "standby_scope_unresolved"},
        }
    if action == "mark_false":
        raw_flag = wire.get("flag")
        flag = str(raw_flag) if raw_flag is not None else run.flag
        if flag is None:
            return {
                "state": "unknown",
                "detail": "no flag was available to invalidate",
                "target_ids": [],
                "metadata": {"code": "flag_unavailable"},
            }
        graph_db = self.graph_dir(run.run_id) / "shared_graph.db"
        if not graph_db.exists():
            return {
                "state": "failed",
                "detail": "offline flag graph is unavailable",
                "target_ids": [],
                "metadata": {"code": "flag_graph_unavailable"},
            }
        graph = None
        try:
            from muteki.models.solve_graph import Challenge
            from muteki.swarm.shared_graph import SQLiteSharedGraph
            graph = SQLiteSharedGraph.open(
                db_path=graph_db,
                challenge=Challenge(
                    id=run.run_id,
                    name=run.name or run.run_id,
                    category=run.category or "web",
                ),
            )
            info = graph.reopen_after_false_positive(
                actor="operator", flag=flag)
        except Exception:
            return {
                "state": "failed",
                "detail": "offline flag invalidation was not committed",
                "target_ids": [],
                "metadata": {"code": "flag_invalidation_failed"},
            }
        finally:
            if graph is not None:
                graph.close()

        run.invalidate_flag(flag)
        try:
            self.update_winner_continuation_flags(
                run.run_id, list(run.flags))
        except Exception:
            pass
        existing_recovery = getattr(run, "recovery_task", None)
        if existing_recovery is not None and not existing_recovery.done():
            return {
                "state": "partial",
                "detail": "flag invalidated; Coordinator recovery is already queued",
                "target_ids": [],
                "metadata": {
                    "effect": "flag_invalidated",
                    "code": "coordinator_recovery_busy",
                    "reopened": list(info.get("reopened") or []),
                },
            }

        async def _recover_full_coordinator() -> bool:
            # Let this control callback return before resolve() drains the old
            # actor epoch; otherwise actor.join() would wait on its own command.
            await asyncio.sleep(0)
            return bool(await self.resolve(
                run.run_id,
                {"race_scout": False, "cold_start": False},
            ))

        recovery = asyncio.create_task(
            _recover_full_coordinator(),
            name=f"coordinator-recovery-{run.run_id}",
        )
        run.recovery_task = recovery

        def _clear_recovery(done: asyncio.Task) -> None:
            if run.recovery_task is done:
                run.recovery_task = None
            try:
                started = bool(done.result())
            except asyncio.CancelledError:
                return
            except Exception:
                LOG.exception(
                    "full Coordinator recovery failed for %s", run.run_id)
                return
            if not started:
                LOG.error(
                    "full Coordinator recovery was not admitted for %s", run.run_id)

        recovery.add_done_callback(_clear_recovery)
        return {
            "state": "routed",
            "detail": "flag invalidated and full Coordinator recovery queued",
            "target_ids": [],
            "metadata": {
                "effect": "flag_invalidated",
                "resolve_scheduled": True,
                "recovery": "full_coordinator",
                "standby_busy_at_invalidation": bool(busy),
                "reopened": list(info.get("reopened") or []),
            },
        }
    if action in {"stop", "complete", "force_cancel"} and busy:
        return await self._cancel_standby(
            run, timeout=self._standby_cancel_timeout())
    if action not in self._STANDBY_ACTIONS:
        return None
    if busy:
        return {
            "state": "unknown",
            "detail": "standby worker is already serving another command",
            "target_ids": [],
            "metadata": {"code": "standby_busy"},
        }
    runtime_wire = dict(wire)  # remains opaque; driver resolves after reserve
    command_id = str(wire.get("command_id") or "")
    reservation: Optional[tuple[str, str]] = None
    reservation_owner = f"standby:{command_id}" if command_id else ""
    context_status = "missing"
    if command_id:
        try:
            from muteki.control import context_resource_id_for_command
            context_id = context_resource_id_for_command(command_id)
            context_status = journal.context_delivery_status(context_id)
            if context_status == "active":
                reservation_id = journal.reserve_context(
                    context_id, worker_id=reservation_owner)
                if not reservation_id:
                    raise RuntimeError("reservation unavailable")
                reservation = (context_id, str(reservation_id))
            elif context_status != "missing":
                raise RuntimeError(
                    f"context state {context_status} is not deliverable")
        except Exception:
            return {
                "state": "unknown",
                "detail": "standby context is not deliverable",
                "target_ids": [],
                "metadata": {"code": "standby_context_unavailable"},
            }

    def _has_secret_ref(value: Any) -> bool:
        if isinstance(value, dict):
            return any(_has_secret_ref(child) for child in value.values())
        if isinstance(value, (list, tuple)):
            return any(_has_secret_ref(child) for child in value)
        return isinstance(value, str) and value.startswith("secret://")

    carries_prompt_context = (
        action in {"ask", "hint", "focus", "redirect"}
        and any(wire.get(key) for key in (
            "text", "hint", "url", "target_url", "context"))
    )
    if reservation is None and (
            _has_secret_ref(wire) or carries_prompt_context):
        return {
            "state": "unknown",
            "detail": "standby prompt context has no active reservation",
            "target_ids": [],
            "metadata": {"code": "standby_context_unavailable"},
        }
    loop = asyncio.get_running_loop()
    delivery_ack: "asyncio.Future[bool]" = loop.create_future()
    runtime_wire["_standby_delivery_ack"] = delivery_ack
    runtime_wire["_control_context_reservations"] = (
        [reservation] if reservation is not None else [])
    runtime_wire["_control_context_owner"] = reservation_owner
    from apps.web.run_recovery import WorkerRuntimePolicyUnavailable
    policy_error = None
    try:
        accepted = self._ensure_standby(run.run_id, runtime_wire)
    except WorkerRuntimePolicyUnavailable as exc:
        accepted = False
        policy_error = exc
    if not accepted:
        if reservation is not None:
            released = False
            try:
                released = bool(journal.release_context_reservation(
                    reservation[0], worker_id=reservation_owner,
                    reservation_id=reservation[1]))
            except Exception:
                pass
            if not released:
                self._ensure_standby_context_cleanup(
                    run, owner=reservation_owner,
                    reservations=[reservation])
        return {
            "state": "unknown",
            "detail": str(policy_error) if policy_error else "standby worker could not be started",
            "target_ids": [],
            "metadata": {"code": policy_error.code if policy_error else "standby_start_failed"},
        }
    try:
        delivered = await asyncio.wait_for(
            asyncio.shield(delivery_ack), timeout=max(0.05, claim_timeout))
    except asyncio.TimeoutError:
        await self._cancel_standby(
            run, timeout=self._standby_cancel_timeout())
        return {
            "state": "unknown",
            "detail": "standby prompt delivery was not confirmed",
            "target_ids": [],
            "metadata": {"code": "standby_delivery_timeout"},
        }
    return {
        "state": "effect_observed" if delivered else "unknown",
        "detail": ("standby prompt delivery confirmed" if delivered
                   else ("standby prompt delivery was not confirmed and "
                         "may have crossed the process/stdin boundary")),
        "target_ids": [],
        "metadata": {
            "effect": ("standby_prompt_started" if delivered
                       else "delivery_unknown"),
            "context_reserved": reservation is not None,
            "process_start_unknown": not delivered,
        },
    }
