"""Operator control, pause/steer, and durable context delivery for CliSolver."""
from __future__ import annotations

import hashlib
import signal
import time
from typing import Any, Optional

from muteki.solver.cli_protocol import _IDLE_REPEAT_LIMIT, _IDLE_REPEAT_STEER

def _notify_context_delivery(self, ok: bool) -> None:
    callback = None
    with self._context_delivery_lock:
        callback = self._context_delivery_callback
        self._context_delivery_callback = None
    if callable(callback):
        try:
            callback(bool(ok))
        except Exception:
            pass


def _context_delivery_reservation_snapshot(self) -> tuple[tuple[str, str], ...]:
    """Capture the reservation batch owned by one prompt transport attempt."""
    with self._context_delivery_lock:
        return tuple(self._pending_control_context_reservations)


def _commit_control_context_delivery(
    self, *, actor: str,
    reservations: "Optional[tuple[tuple[str, str], ...]]" = None,
) -> bool:
    """Commit reservations only after their actual transport boundary.

    Ordinary prompts cross that boundary at Popen because the prompt is already
    in argv. Secret-bearing prompts cross it only after the stdin writer (or the
    acknowledged remote supervisor) accepts the complete payload.
    """
    with self._context_delivery_lock:
        current = list(self._pending_control_context_reservations)
        pending = list(
            reservations if reservations is not None else tuple(current))
        binding_worker_id = str(
            self._context_binding_worker_id or self.solver_id)
        if not pending:
            # An explicitly captured empty batch is valid only while no later
            # reservation has appeared.  More importantly, a late callback for
            # a non-empty batch never turns into success merely because another
            # owner prematurely cleared the mutable pending list.
            confirmed = (
                not current and not self._control_context_delivery_unknown)
        else:
            committer = self._context_committer
            confirmed = callable(committer)
            if confirmed:
                for context_id, reservation_id in pending:
                    try:
                        if not committer(
                            context_id, worker_id=binding_worker_id,
                            reservation_id=reservation_id,
                        ):
                            confirmed = False
                            break
                    except Exception:
                        confirmed = False
                        break
            if confirmed:
                consumed = set(pending)
                self._pending_control_context_reservations = [
                    reservation for reservation in current
                    if reservation not in consumed
                ]
                self._control_context_delivery_committed = True
    if confirmed:
        self._notify_context_delivery(True)
        return True
    self._mark_control_context_delivery_unknown(
        actor=actor,
        reason="delivery commit unavailable after prompt transport",
        reservations=tuple(pending),
    )
    return False


def _mark_control_context_delivery_unknown(
    self, *, actor: str, reason: str,
    reservations: "Optional[tuple[tuple[str, str], ...]]" = None,
) -> None:
    with self._context_delivery_lock:
        current = list(self._pending_control_context_reservations)
        pending = list(
            reservations if reservations is not None else tuple(current))
        binding_worker_id = str(
            self._context_binding_worker_id or self.solver_id)
        marker = self._context_delivery_unknown_marker
        if callable(marker):
            for context_id, reservation_id in pending:
                try:
                    marker(
                        context_id, worker_id=binding_worker_id,
                        reservation_id=reservation_id,
                        actor=actor, reason=reason,
                    )
                except Exception:
                    pass
        # Even if the journal marker failed, a post-transport reservation must
        # remain stranded for recovery; generic cleanup may never replay it.
        consumed = set(pending)
        self._pending_control_context_reservations = [
            reservation for reservation in current
            if reservation not in consumed
        ]
        if pending:
            self._control_context_delivery_unknown = True
    self._cancel_event.set()
    self.cancel()
    self._notify_context_delivery(False)

def _set_paused(self, paused: bool) -> bool:
    """SIGSTOP/SIGCONT every live subprocess GROUP — a genuine freeze (same
    PIDs), not a kill. POSIX only; on platforms without SIGSTOP it no-ops."""
    sig = getattr(signal, "SIGSTOP", None) if paused else getattr(signal, "SIGCONT", None)
    with self._procs_lock:
        procs = list(self._live_procs)
    # No child exists yet: recording the desired flag is itself sufficient;
    # _on_proc applies it before the future subprocess can do work.
    if not procs:
        self._paused = paused
        if paused:
            self._paused_event.set()
        else:
            self._paused_event.clear()
        return True
    if sig is None:
        return False
    signalled: list[Any] = []
    for proc in procs:
        if self._signal_proc(proc, sig):
            signalled.append(proc)
            continue
        # All-or-nothing confirmation: compensate every process already
        # signalled, leaving the prior logical state intact.
        inverse = (getattr(signal, "SIGCONT", None) if paused
                   else getattr(signal, "SIGSTOP", None))
        if inverse is not None:
            for prior in signalled:
                self._signal_proc(prior, inverse)
        return False
    self._paused = paused
    if paused:
        self._paused_event.set()
    else:
        self._paused_event.clear()
    return True


def _enable_in_turn_steer(self) -> bool:
    """P2: whether a teammate's new flag / an operator correction may
    END the current turn (via _steer_event) so the worker re-plans immediately
    instead of racing the same flag / ignoring the correction for the rest of a
    long turn. A steer only ends the CURRENT turn cleanly (session kept), it
    never cancels the worker.

    Gated on _turn_active: a steer is ONLY valid while a subprocess turn is
    actually running. A freshly-spawned worker drains the InsightBus history
    backlog (prior FLAGs + standing hints) before/between turns — those must be
    CONSUMED (folded into _already_found / _standing_guidance) but must NOT
    steer-kill the not-yet-started subprocess (run-40726 regression). Disabled
    entirely by a `_no_in_turn_steer` attr (tests / special modes)."""
    if getattr(self, "_no_in_turn_steer", False):
        return False
    return bool(getattr(self, "_turn_active", False))


def _stop_on_sibling_flag(self) -> bool:
    """Whether a sibling FlagFound should end this worker's current pass.

    Only the legacy single-flag path does this. Multi-flag collection keeps
    live workers running until ALL_FLAGS_FOUND, while still recording each peer
    flag in _already_found so prompts avoid re-hunting it.
    """
    if bool(getattr(self.challenge, "multi_flag", False)):
        return False
    return self._expected_flags() <= 1


def _drain_control(self) -> None:
    """Drain the InsightBus inbox for HITL commands + sibling FLAG. Runs from
    the monitor thread (the worker has no between-turn point like base Solver).

    pause→SIGSTOP, resume→SIGCONT, sibling FLAG→record it (single-flag may
    still end the current pass). Operator
    Non-standing hint/focus/redirect guidance ends the current bounded call and
    enters the same-session checkpoint, where the new guidance is present in the
    next prompt. `standing` guidance (VPS/SSH creds) remains background context
    and is injected without interrupting the current call."""
    inbox = self._insight_inbox
    if inbox is None:
        return
    from muteki.swarm.insight_bus import InsightKind
    while True:
        try:
            ins = inbox.get_nowait()
        except Exception:
            break
        try:
            if ins.kind is InsightKind.GUIDANCE:
                target = getattr(ins, "target", "global") or "global"
                intent_id = str(
                    getattr(self, "intent_id_assigned", "")
                    or getattr(self, "_intent_id", "") or "")
                lane = str(getattr(self, "lane", "") or "")
                engine = str(getattr(self, "engine", "") or "")
                eligible_targets = {
                    "global", self.solver_id,
                    f"solver:{self.solver_id}", f"worker:{self.solver_id}",
                }
                if engine:
                    eligible_targets.add(f"engine:{engine}")
                if intent_id:
                    eligible_targets.update({intent_id, f"intent:{intent_id}"})
                if lane:
                    eligible_targets.update({lane, f"lane:{lane}"})
                if target not in eligible_targets:
                    continue
                act = (getattr(ins, "action", "hint") or "hint").lower()
                if act == "pause":
                    self._set_paused(True)
                elif act == "resume":
                    self._set_paused(False)
                elif act in (
                    "hint", "redirect", "focus", "directive", "correction",
                ):
                    text = getattr(ins, "text", "") or ""
                    url = getattr(ins, "url", "") or ""
                    standing = bool(getattr(ins, "standing", False))
                    if standing:
                        # persistent background guidance — held, not consumed.
                        with self._guidance_lock:
                            if text and text not in self._standing_guidance:
                                self._standing_guidance.append(text)
                            if url:
                                self._target_override = url
                        # Standing guidance is persistent background context
                        # (VPS/SSH creds, global constraints). Killing a live
                        # single-shot worker here makes a run that posts standing
                        # guidance just after race start empty-exit every worker.
                        # Keep it for prompts; non-standing hint/redirect/focus
                        # remain the explicit steer/correction path below.
                    else:
                        # End this bounded invocation and resume its exact session
                        # at the checkpoint boundary. History replay happens while
                        # _turn_active is false, so an old hint cannot chain-steer a
                        # newly spawned process.
                        with self._guidance_lock:
                            if text and text not in self._standing_guidance:
                                self._standing_guidance.append(text)
                            if url:
                                self._target_override = url
                        if self._enable_in_turn_steer():
                            self._steer_event.set()
            elif ins.kind is InsightKind.FLAG:
                # a sibling found a flag. Multi-flag: DON'T stop — just note it
                # so we don't re-hunt the same one (and let the worker prompt's
                # "already found" list stay accurate). We only die on the
                # ALL_FLAGS_FOUND signal below.
                txt = getattr(ins, "text", "") or ""
                if txt and txt not in self._already_found:
                    self._already_found.add(txt)
                    # Single-flag compatibility: the first sibling flag can still
                    # end this pass. Multi-flag workers stay alive until the
                    # explicit ALL_FLAGS_FOUND signal; killing them on every
                    # partial FlagFound collapsed a 4-flag race to one worker.
                    if self._stop_on_sibling_flag() and self._enable_in_turn_steer():
                        self._steer_event.set()
            elif ins.kind is InsightKind.ALL_FLAGS_FOUND:
                # the run collected every expected flag — stop wasting budget.
                self.cancel()
            elif ins.kind is InsightKind.FACT or ins.kind is InsightKind.DEAD_END:
                # A teammate confirmed a fact / ruled out a direction mid-turn.
                # We do NOT fold it here: a teammate's verified fact is written to
                # the shared graph by _record_fact (shared_graph.add_evidence), and
                # every worker's next-turn prompt carries the FULL board via
                # _board_markdown() → to_board_markdown() (facts + ruled-out, no
                # truncation). The InsightBus FACT/DEAD_END event is a redundant
                # second channel; consuming it into a per-worker prompt buffer was
                # dead (the renderer had 0 callers). The board IS the channel.
                pass
            elif ins.kind is InsightKind.SUBMIT_LOCKED:
                # a sibling holds the global submit-lock → don't submit now. We do
                # NOT pause the process: the worker keeps reconning / refining its
                # answer; the next turn's prompt tells it to hold submission. A
                # self-clearing lease guarantees a stuck lock can't freeze us.
                self._submit_blocked_until = max(
                    self._submit_blocked_until, time.time() + self._SUBMIT_HOLD_S)
            elif ins.kind is InsightKind.SUBMIT_UNLOCKED:
                # the holder released early → re-open submission immediately
                # (a verifier lockout, if any, is tracked separately and still binds).
                self._submit_blocked_until = 0.0
            elif ins.kind is InsightKind.VERIFIER_LOCKED:
                # the verifier hit a cooldown/burn-lockout → nobody submits until
                # it elapses. Record the absolute deadline so the prompt can show
                # the remaining time and the worker spends it improving the answer.
                try:
                    secs = float(getattr(ins, "text", "0") or 0)
                except (TypeError, ValueError):
                    secs = 0.0
                if secs > 0:
                    self._verifier_locked_until = max(
                        self._verifier_locked_until, time.time() + secs)
        except Exception:
            continue

def _maybe_steer_idle_repeat(self) -> None:
    """Soft-correct a worker repeating the same command with unchanged output."""
    if self.mode in ("review", "fact_verifier"):
        return
    self._stalled_at = None
    cmds = list(getattr(self, "_raw_tool_commands", []) or [])
    outs = list(getattr(self, "_raw_tool_outputs", []) or [])
    n = _IDLE_REPEAT_LIMIT
    if len(cmds) < n or len(outs) < n:
        return
    window_cmds = cmds[-n:]
    window_outs = outs[-n:]
    if len(set(window_cmds)) != 1:
        return
    hashes = [
        hashlib.sha256((out or "").encode("utf-8", errors="replace")).hexdigest()
        for out in window_outs
    ]
    if len(set(hashes)) != 1:
        return
    if getattr(self, "_idle_repeat_steered", False):
        return
    with self._guidance_lock:
        if _IDLE_REPEAT_STEER not in self._standing_guidance:
            self._standing_guidance.append(_IDLE_REPEAT_STEER)
    if self._enable_in_turn_steer():
        self._idle_repeat_steered = True
        self._steer_event.set()


def _finalize_control_context_prompt_manifest(self, prompt: str) -> None:
    """Fence durable context against the *rendered* invocation prompt.

    Context is reserved before worker construction, while ``_standing_block``
    enforces a separate size budget later.  This final pre-Popen reconciliation
    is therefore the first point where we can truthfully say which reservations
    are represented in the bytes handed to the CLI.  Missing optional rows are
    released (restoring one-shot capacity); missing exact-continuation rows abort
    the launch instead of manufacturing a delivery receipt.
    """
    manifest = list(getattr(self, "_control_context_prompt_manifest", []) or [])
    if not manifest:
        return
    pending = set(self._pending_control_context_reservations)
    worker_id = str(self._context_binding_worker_id or self.solver_id)
    included: list[tuple[str, str]] = []
    dropped: list[tuple[str, str]] = []
    included_secret_texts: set[str] = set()
    dropped_texts: set[str] = set()
    for item in manifest:
        reservation = (
            str(item.get("context_id") or ""),
            str(item.get("reservation_id") or ""),
        )
        if reservation not in pending:
            continue
        text = str(item.get("text") or "")
        if text and text in prompt:
            included.append(reservation)
            if bool(item.get("secret")):
                included_secret_texts.add(text)
            continue
        if bool(item.get("required")):
            raise RuntimeError(
                "required operator continuation context is absent from the "
                "rendered worker prompt")
        releaser = self._context_releaser
        if not callable(releaser):
            raise RuntimeError(
                "optional operator context was omitted but its reservation "
                "cannot be released")
        try:
            released = releaser(
                reservation[0], worker_id=worker_id,
                reservation_id=reservation[1])
        except Exception as exc:
            raise RuntimeError(
                "optional operator context reservation release failed") from exc
        if released is not True:
            raise RuntimeError(
                "optional operator context reservation release was not confirmed")
        dropped.append(reservation)
        if text:
            dropped_texts.add(text)

    if dropped:
        dropped_set = set(dropped)
        self._pending_control_context_reservations = [
            reservation
            for reservation in self._pending_control_context_reservations
            if reservation not in dropped_set
        ]
        # Dropped secrets never enter argv/stdin and therefore must not force a
        # secure-transport preflight.  Retain any value represented by another
        # included manifest row.
        self._control_secret_values = [
            value for value in self._control_secret_values
            if value not in dropped_texts or value in included_secret_texts
        ]
    self._control_context_prompt_included = tuple(included)
    self._control_context_prompt_manifest_finalized = True
