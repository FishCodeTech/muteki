"""Standing guidance, flag invalidation, and context delivery actions."""

from __future__ import annotations

import asyncio
import re

from muteki.core.events import Event, EventType
from muteki.swarm.swarm_support import _STANDING_MAX
from muteki.swarm.coordinator_control_command import ControlCommandContext

# 聚焦 / 转向 / 指令 are one operator action: verbatim text becomes a new
# step. A URL in the text still retargets. Slash aliases stay accepted.
_ASSIGN_STEP_ACTIONS = frozenset({"focus", "redirect", "directive"})
_TEXT_URL_RE = re.compile(r"https?://[^\s\"'<>]+")


async def _handle_clear_guidance_control(self, context: ControlCommandContext) -> bool:
    cmd = context.cmd
    action = context.action
    text = context.text
    # P0 defect-4: clear standing guidance. The list is only-grew before,
    # so an operator who dropped several corrections could not retract a
    # stale one (and the cumulative text bloated every new worker's prompt
    # → claude 36k-token empty-exit). clear_standing wipes all, or one by
    # exact text match (cmd["text"]).
    if action in ("clear_standing", "reset_guidance"):
        companion = (cmd.get("_control_companion")
                     if isinstance(cmd.get("_control_companion"), dict)
                     else {})
        expired_context_count = int(
            companion.get("expired_context_count") or 0)
        matched_source_ids = sorted({
            str(value or "").strip()
            for value in (
                companion.get("matched_source_command_ids") or [])
            if str(value or "").strip()
        })
        if self.shared_graph is None:
            self._ack_control(
                cmd, state=("partial" if expired_context_count else "failed"),
                detail="standing guidance graph is unavailable",
                metadata={
                    "code": "guidance_graph_unavailable",
                    "expired_context_count": expired_context_count,
                })
            return True
        try:
            source_command_id = str(
                cmd.get("command_id") or "").strip()
            if source_command_id:
                clear_result = self.shared_graph.apply_standing_clear(
                    command_id=source_command_id,
                    actor="operator",
                    text=("" if str(text or "").startswith("secret://")
                          else str(text or "")),
                    eligible_command_ids=(
                        matched_source_ids if text else None),
                    match_by_source_ids=str(text or "").startswith(
                        "secret://"),
                )
                expired_directives = list(
                    clear_result.get("expired_directives") or [])
            else:
                # Compatibility for old in-process queue producers.
                # Durable API commands always carry command_id and use
                # the crash-replay marker above.
                expired_directives = (
                    self.shared_graph.expire_standing_directives(
                        actor="operator", text=str(text or "")))
        except Exception:
            self._ack_control(
                cmd, state=("partial" if expired_context_count else "failed"),
                detail="standing directive expiration failed",
                metadata={
                    "code": "guidance_graph_expire_failed",
                    "expired_context_count": expired_context_count,
                })
            return True

        # The ControlActor normally expired typed contexts before this
        # runtime call. Keep the final boundary safe for legacy direct
        # queue producers and verify absence before mutating memory.
        provider = getattr(self, "_context_provider", None)
        expirer = getattr(self, "_context_expirer", None)
        try:
            if callable(provider):
                matching = [
                    resource for resource in provider()
                    if bool(getattr(resource, "standing", False))
                    and (not text or str(
                        getattr(resource, "content", "") or "") == str(text))
                ]
                if matching and not callable(expirer):
                    raise RuntimeError("context expirer unavailable")
                for resource in matching:
                    expirer(
                        str(getattr(resource, "context_id", "")),
                        actor="operator", reason="clear_standing")
                remaining = [
                    resource for resource in provider()
                    if bool(getattr(resource, "standing", False))
                    and (not text or str(
                        getattr(resource, "content", "") or "") == str(text))
                ]
                if remaining:
                    raise RuntimeError("standing context remains active")
        except Exception:
            self._ack_control(
                cmd,
                state=("partial" if (
                    expired_directives or expired_context_count) else "failed"),
                detail="typed standing context expiration was not confirmed",
                metadata={
                    "code": "guidance_context_expire_failed",
                    "expired_directives": expired_directives,
                    "expired_context_count": expired_context_count,
                })
            return True

        if text:
            self._standing_guidance = [
                s for s in self._standing_guidance if s != text]
        else:
            self._standing_guidance = []
        self._ack_control(
            cmd, state="effect_observed",
            detail="standing guidance expired",
            metadata={
                "effect": "guidance_cleared",
                "expired_directives": expired_directives,
                "expired_context_count": expired_context_count,
            })
        return True
    return False


async def _handle_mark_false_control(self, context: ControlCommandContext) -> bool:
    cmd = context.cmd
    action = context.action
    if action == "mark_false":
        raw_flag = cmd.get("flag")
        flag = (
            str(raw_flag) if raw_flag is not None
            else (self._found_flags[0] if self._found_flags else None)
        )
        if flag is None:
            self._ack_control(
                cmd, state="unknown",
                detail="no flag was available to invalidate",
                metadata={"effect": "no_effect"})
            return True
        if self.shared_graph is None:
            self._ack_control(
                cmd, state="failed",
                detail="flag graph is unavailable",
                metadata={"code": "flag_graph_unavailable"})
            return True
        try:
            info = self.shared_graph.reopen_after_false_positive(
                actor="operator", flag=flag)
        except Exception:
            self._ack_control(
                cmd, state="failed",
                detail="flag invalidation was not committed",
                metadata={"code": "flag_invalidation_failed"})
            return True

        # Only project volatile state after the canonical graph write.
        self._found_flags = [f for f in self._found_flags if f != flag]
        try:
            await self._emit_coord_bb(
                "dead_end", reason=info.get("dead_end_reason")
                or f"false positive: {flag}")
            for iid in info.get("reopened", []) or []:
                await self._emit_coord_bb("intent_reopened", intent_id=iid)
            await self._emit_coord_bb("flag_invalidated", flag=flag)
            if self.bus is not None:
                try:
                    await self.bus.emit(Event(
                        event_type=EventType.RUN_REOPENED,
                        run_id=self.run_id,
                        challenge_id=self.challenge.id,
                        payload={"flag": flag},
                    ))
                except Exception:
                    pass
            if self._operator_event is not None:
                self._operator_event.set()
        except Exception:
            # Telemetry is replayable from the graph and never weakens
            # the already-committed invalidation receipt.
            pass
        self._ack_control(
            cmd, state="effect_observed",
            detail="flag invalidated and dependent intents reopened",
            metadata={"effect": "flag_invalidated"})
        return True
    return False


async def apply_control_context(self, context: ControlCommandContext) -> None:
    cmd = context.cmd
    text = context.text
    delivery_text = context.delivery_text
    action = context.action
    original_action = context.original_action
    target = context.target
    request_id = context.request_id
    command_id = context.command_id
    scope_is_global = context.scope_is_global
    continuation_intent_id = context.continuation_intent_id
    # `url` is the NEW target a redirect carries (distinct from `target`,
    # which is the SCOPE: global / solver:<id>). `standing` marks
    # persistent background guidance (VPS/SSH creds) for all workers.
    url = cmd.get("url") or cmd.get("target_url") or ""
    delivery_url = str(url or "")
    if action in _ASSIGN_STEP_ACTIONS and not delivery_url:
        found = _TEXT_URL_RE.search(str(text or ""))
        if found:
            delivery_url = found.group(0).rstrip(".,;)")
            url = delivery_url
    secret_delivery = any(
        str(value or "").startswith("secret://")
        for value in (text, url)
    )
    if secret_delivery:
        typed_secret_available = False
        provider = getattr(self, "_context_provider", None)
        status_provider = getattr(
            self, "_context_status_provider", None)
        if command_id and callable(provider):
            try:
                from muteki.control import context_resource_id_for_command
                context_id = context_resource_id_for_command(command_id)
                if callable(status_provider):
                    typed_secret_available = status_provider(context_id) in {
                        "active", "reserved", "bound",
                    }
                else:
                    # Safe compatibility fallback: default provider()
                    # exposes active rows only, never expired/consumed.
                    typed_secret_available = any(
                        str(getattr(row, "context_id", "")) == context_id
                        for row in provider()
                    )
            except Exception:
                typed_secret_available = False
        if not typed_secret_available:
            self._ack_control(
                cmd, state="unknown",
                detail=("opaque secret reference has no reserved typed "
                        "context delivery boundary"),
                metadata={"code": "secret_delivery_unavailable"},
            )
            return
    standing = bool(cmd.get("standing", False))
    # Enter/send tells the swarm without creating a step. Keep that note on
    # every later Decide/Worker prompt instead of a one-shot that CTF Explore
    # used to drop.
    if action == "hint" and text:
        standing = True
    matched = [
        worker for worker in self._control_target_solvers(target)
        if not self._worker_runtime_exit_confirmed(worker)
    ]
    target_ids = [str(getattr(worker, "solver_id", "") or "")
                  for worker in matched]
    live_context_routed = bool(
        not scope_is_global
        and matched
        and not secret_delivery
        and (delivery_text or delivery_url)
    )
    # persist standing guidance on the coordinator so workers spawned
    # LATER inherit it at turn-1 (live workers also get it via the
    # InsightBus broadcast below). Dedupe so re-sends don't pile up.
    if (scope_is_global and standing and text and not secret_delivery
            and text not in self._standing_guidance):
        self._standing_guidance.append(text)
        # P0 defect-4: LRU cap — keep only the most recent N standing
        # hints so the cumulative text can't bloat every new worker's
        # prompt unbounded (the 36k-token claude empty-exit). The per-
        # worker char budget (cli_solver _standing_block) is the second
        # guard; this bounds the count at the source.
        if len(self._standing_guidance) > _STANDING_MAX:
            self._standing_guidance = self._standing_guidance[-_STANDING_MAX:]
    # Keep non-standing guidance available for the next spawn as a recovery
    # fallback. Matching live Workers also receive it through InsightBus and
    # re-enter their exact session at the forced checkpoint boundary.
    if scope_is_global and not standing and not secret_delivery:
        if url:
            if delivery_url != str(self._target_redirect or ""):
                retired_epoch = str(getattr(self, "_target_epoch", 1) or 1)
                self._target_epoch = int(
                    getattr(self, "_target_epoch", 1) or 1
                ) + 1
                if self.shared_graph is not None:
                    # A redirected target invalidates every old endpoint and
                    # host-owned process.  Retire them before later Workers can
                    # consume a stale run-shared path.
                    await asyncio.to_thread(
                        self.shared_graph.stop_runtime_resources,
                        actor="coordinator", target_epoch=retired_epoch,
                    )
            self._target_redirect = delivery_url
        if text and text not in self._next_worker_guidance:
            self._next_worker_guidance.append(text)
    # B: record the steer as a FIRST-CLASS OperatorDirective (not a fake
    # low-confidence candidate + ordinary intent). The directive carries a
    # preemption policy; soft_rebind (default) supersedes unclaimed
    # conflicting intents so the next worker batch picks up the new
    # direction, without killing a live worker. graceful_drain / force_cancel
    # are honored where the operator explicitly asks for them.
    preempt = str(cmd.get("preempt_policy")
                  or cmd.get("preemption") or "").strip().lower()
    directive_id = ""
    is_decision_answer = original_action in ("answer_decision", "submit")
    if (text and not is_decision_answer
            and not continuation_intent_id
            and not live_context_routed
            and self.shared_graph is not None
            and action in (
                "hint", "focus", "redirect", "directive", "correction"
            )):
        try:
            info = self.shared_graph.add_operator_directive(
                actor="operator", action=action, text=text,
                scope=target or "global", standing=standing,
                preempt_policy=preempt or "soft_rebind",
                source_command_id=command_id,
            )
            directive_id = info["directive_id"]
            policy = info["preempt_policy"]
            intent_id = ""
            status = "queued"
            # Only actions that explicitly assign or change search work become
            # claimable Intents.  A plain hint is context for the next Decide /
            # Worker; binding its prose as an independent solve task caused a
            # second whole-challenge worker to start without source facts.
            # Corrections are likewise evidence for Review/Decide, not tasks.
            creates_intent = action in _ASSIGN_STEP_ACTIONS
            if scope_is_global and not standing and creates_intent:
                intent_id = f"I-{directive_id}"
                self.shared_graph.propose_intent(
                    actor="operator", intent_id=intent_id, goal=text,
                    payload={"source": "operator_directive", "action": action,
                             "directive_id": directive_id,
                             "scope": target,
                             "priority": "operator"},
                )
                status = "bound"
            elif scope_is_global:
                status = "bound"
            self.shared_graph.update_directive_status(
                directive_id=directive_id, status=status,
                generated_intent_id=intent_id or None)
            await self._emit_coord_bb(
                "operator_directive_changed", directive_id=directive_id,
                action=action, text=text, scope=target, status=status,
                preemption=policy, intent_id=intent_id)
            # Assign-step actions must keep the just-created intent. Hint may
            # still retire leftover "ask the operator" work after a resource
            # is supplied (sweep below).
            if (action not in _ASSIGN_STEP_ACTIONS
                    and scope_is_global and policy in
                    ("soft_rebind", "graceful_drain", "force_cancel")):
                for needle in ("operator", "ask", "request"):
                    try:
                        self.shared_graph.supersede_open_intents(
                            actor="coordinator", match=needle,
                            reason=f"superseded by operator directive {directive_id}")
                    except Exception:
                        pass
            if policy == "graceful_drain" and not secret_delivery:
                try:
                    await self.insight.guidance(
                        delivery_text, action="graceful_drain", target=target,
                        standing=False)
                except Exception:
                    pass
            # A contextual hint needs no arbitration.  Route-changing actions
            # and corrections still get the configured Review audit.
            if (action != "hint"
                    and self.review_policy.get("on_operator_hint", True)):
                self._queue_review_request(
                    trigger="operator_hint",
                    directive=(
                        f"Operator {action} directive was added: {text}. "
                        "Audit whether this should become a route change, "
                        "branch split, fact challenge, or focused worker directive."
                    ),
                )
        except Exception:
            pass
    # Broadcast on InsightBus as the live transport. Matching Workers end their
    # bounded call and enter the exact-session checkpoint with this context;
    # history replay stays non-interrupting because it runs before _turn_active.
    if (not secret_delivery and not continuation_intent_id
            and (delivery_text or delivery_url)):
        try:
            await self.insight.guidance(
                delivery_text, action=action, target=target,
                url=delivery_url, standing=standing)
        except Exception:
            # The typed context/graph selector is the durable delivery
            # contract. InsightBus is a live projection and may be
            # replayed; its failure must not turn a committed binding
            # into a terminal FAILED receipt.
            pass
    # Scoped commands have now been routed to their one eligible live
    # worker.  Close the one-shot directive so it cannot later leak into a
    # differently-scoped spawn.  ``acted`` here means routed/consumed by the
    # legacy bus, not that the model semantically obeyed it.
    if directive_id and not scope_is_global and self.shared_graph is not None:
        try:
            bound_worker = (target.split(":", 1)[1]
                            if target.startswith("solver:") else target)
            self.shared_graph.update_directive_status(
                directive_id=directive_id, status="acted",
                bound_worker=bound_worker)
            await self._emit_coord_bb(
                "operator_directive_changed", directive_id=directive_id,
                action=action, text=text, scope=target, status="acted",
                bound_worker=bound_worker)
        except Exception:
            pass
    gave_resource = bool(url) or standing or bool(text)
    # Wake only for an actual answer/resource. Empty ASK/WRITEUP-style
    # commands must not release a pending blocker with an UNKNOWN ACK.
    if (self._operator_event is not None
            and (gave_resource or is_decision_answer)
            and (scope_is_global or continuation_intent_id
                 or not live_context_routed)):
        self._operator_event.set()
    # M5: clear the "waiting for help" asks SCOPED to the command's target.
    # A global command answers every pending ask; a solver-scoped one
    # (target == "solver:<id>") only clears that worker's ask, so a hint
    # addressed to worker B no longer wipes worker A's still-unmet blocker
    # (which would resolve awaiting_operator with no real answer and resume
    # hurling workers at A's wall). Keep the rest pending.
    if (is_decision_answer and request_id):
        self._pending_help = [
            h for h in self._pending_help
            if str(h.get("request_id") or h.get("id") or "") != request_id]
    elif gave_resource and scope_is_global and len(self._pending_help) <= 1:
        self._pending_help = []
    elif gave_resource:
        scoped = target.split(":", 1)[-1] if ":" in target else target
        matching = [h for h in self._pending_help
                    if str(h.get("worker", "")) == scoped]
        # A request-less legacy hint is only an answer when its scope
        # identifies exactly one outstanding decision.  Otherwise keep
        # every card open and require an explicit request_id.
        if len(matching) == 1:
            self._pending_help = [
                h for h in self._pending_help
                if str(h.get("worker", "")) != scoped]
    # M3: RETIRE the now-obsolete "ask the operator for X" intents ONLY when
    # the operator actually SUPPLIED A RESOURCE — a redirect url, standing
    # guidance, or hint text (run-11190: 238-worker loop re-asking for the
    # L2 SSH password after it was supplied). A bare default-action hint with
    # no content used to run this sweep too, and its broad substring needles
    # (operator/unlock/dashboard) could wrongly retire a legitimate in-flight
    # intent on a totally unrelated hint. Gate on a resource being present;
    # for a solver-scoped command, only retire that worker's blocked intents.
    if (self.shared_graph is not None and gave_resource and scope_is_global
            and action not in _ASSIGN_STEP_ACTIONS):
        superseded = 0
        exclude_ids = [
            str(row.get("generated_intent_id") or "")
            for row in self.shared_graph.operator_directives(active_only=False)
            if str(row.get("generated_intent_id") or "").strip()
        ]
        for needle in ("operator", "ssh password", "dashboard",
                       "unlock"):
            try:
                superseded += len(self.shared_graph.supersede_open_intents(
                    actor="coordinator", match=needle,
                    reason=f"operator supplied input ({action})",
                    exclude_ids=exclude_ids))
            except Exception:
                pass
        if superseded:
            try:
                await self.insight.dead_end(
                    "coordinator",
                    f"retired {superseded} obsolete 'ask-operator' "
                    f"intent(s) after operator input")
            except Exception:
                pass
    # Persisting and routing context is not proof that a live model saw it.  A
    # bounded Worker consumes the directive at its forced checkpoint; if that
    # boundary has already passed, the next matching Worker consumes it.  Keep the
    # receipt below at `routed` until an actual prompt transport emits its own
    # delivery observation.
    routable = bool(
        text or url or action in ("focus", "directive", "correction"))
    durable_selector = False
    if command_id:
        try:
            from muteki.control import context_resource_id_for_command
            context_id = context_resource_id_for_command(command_id)
            provider = getattr(self, "_context_provider", None)
            if callable(provider):
                resource = next(
                    (row for row in provider()
                     if str(getattr(row, "context_id", "")) == context_id),
                    None)
                if resource is not None:
                    resource_scope = str(
                        getattr(resource, "scope", "global") or "global")
                    durable_selector = not (
                        resource_scope.startswith(("worker:", "solver:"))
                        and not target_ids)
        except Exception:
            durable_selector = False
    if not scope_is_global and not target_ids and not durable_selector:
        routable = False
    # For an ephemeral Worker selector, routing is possible only while that
    # Worker is live or an exact continuation intent was materialised.
    if (not scope_is_global
            and target.startswith(("solver:", "worker:"))
            and not continuation_intent_id
            and not target_ids):
        routable = False
    self._ack_control(
        cmd,
        state="partial" if routable else "unknown",
        detail=("operator context queued for the matching Worker checkpoint"
                if routable else "no matching live worker or bindable context"),
        target_ids=target_ids,
        metadata={
            "effect": ("decision_answered" if is_decision_answer
                       else "directive_bound"),
            "directive_id": directive_id,
            "delivery": ("same_session_checkpoint" if live_context_routed
                         else "queued_for_checkpoint"),
            "continuation_intent_id": continuation_intent_id,
        },
    )
