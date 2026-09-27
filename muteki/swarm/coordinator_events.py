"""Coordinator bus and graph-bridge events. Moved from coordinator_flags.py."""

from __future__ import annotations

import json
from pathlib import Path
from typing import TYPE_CHECKING, Any, Optional

if TYPE_CHECKING:
    pass

from muteki.core.events import Event, EventType, blackboard_delta_payload

_GRAPH_BRIDGE_KINDS = {
        "observation_added",
        "fact_added",
        "fact_observed",
        "fact_promoted",
        "dead_end",
        "intent_proposed",
        "intent_claimed",
        "intent_concluded",
        "intent_state_changed",
        "fact_challenged",
        "fact_revalidated",
        "fact_rejected",
        "fact_merged",
        "fact_superseded",
        "flag_found",
        "flag_invalidated",
        "poc_saved",
        "poc_claimed",
        "poc_concluded",
        "review_finding",
        "route_suppressed",
        "route_reopened",
        "branch_split",
        "branch_resolved",
        "coordinator_directive",
        "capability_published",
        "capability_retired",
        "capability_gap_reported",
        "access_path_published",
        "access_path_state_changed",
        "runtime_resource_registered",
        "runtime_resource_state_changed",
        "value_receipt",
    }

async def _emit_bb_bus(self, kind: str, **fields) -> None:
    """Emit one BLACKBOARD_DELTA from anywhere (finalize, resolve, etc.) — the
    coordinator loop has its own `_emit_bb` closure, but lifecycle transitions at
    run finish happen outside it and must still reach the JSONL/SSE stream the UI
    reads. Best-effort; a bus failure never masks the outcome."""
    if self.bus is None:
        return
    try:
        actor = str(fields.pop("actor", "") or "coordinator")
        await self.bus.emit(Event(
            event_type=EventType.BLACKBOARD_DELTA, run_id=self.run_id,
            challenge_id=self.challenge.id,
            payload=blackboard_delta_payload(kind, actor=actor, **fields)))
    except Exception:
        pass


def install_coordinator_sinks(self) -> None:
    import asyncio

    from muteki.swarm.swarm_support import _PENDING_HELP_MAX

    # event the coordinator waits on when it pauses for operator help; set by
    # _drain_hitl on any operator command.
    self._operator_event = asyncio.Event()
    # sink: capture workers' HITL_REQUEST (NEED_INPUT / env_down) off the shared
    # bus so the coordinator knows a direction is blocked on the operator. Mirror
    # of RunManager's meta sink. Best-effort; never raises into a worker's emit.
    if self.bus is not None:
        async def _help_sink(ev: Event) -> None:
            if ev.event_type is EventType.HITL_REQUEST:
                # M6: dedup on (worker, need) and cap the list. The per-worker
                # marker dedup is per-worker, so the SAME blocker raised by N
                # workers (or re-emitted by a re-bootstrapped worker) used to
                # append N entries — inflating the awaiting_operator `count`,
                # pushing the earliest (often most important) asks past the
                # [-3:] summary window, and growing unbounded on a never-give-up
                # run. Keyed dedup + cap fixes all three.
                payload = dict(ev.payload or {})
                need_text = str(payload.get("need", "")).strip()
                worker = str(payload.get("worker", ""))
                # NEED_INPUT is already an explicit operator request. Do not
                # reinterpret its prose as a route failure, lane request, or
                # ordinary uncertainty; every valid hand-raise pauses the run.
                need_kind = "external_blocker"
                payload["need_kind"] = need_kind
                # Keep the historical need_kind column/event field for durable
                # request compatibility. It now has one behavioral meaning.
                if self.shared_graph is not None and need_text:
                    try:
                        self.shared_graph.add_hitl_request(
                            worker=worker or "worker", need=need_text,
                            need_kind=need_kind,
                            request_id=str(payload.get("request_id") or "") or None,
                            classification_confidence=float(
                                payload.get("classification_confidence") or 1.0),
                            status="awaiting_operator")
                    except Exception:
                        pass
                # Emit the compatibility delta OUT-OF-BAND (scheduled, not awaited):
                # this sink runs INSIDE bus.emit, so awaiting another emit here would
                # re-enter the bus and reorder/deadlock the NEED_INPUT pause path.
                try:
                    asyncio.create_task(self._emit_coord_bb(
                        "hitl_classified", worker=worker, need=need_text[:200],
                        need_kind=need_kind, pauses_behavior=True))
                except Exception:
                    pass
                key = (str(payload.get("worker", "")),
                       str(payload.get("need", "")).strip())
                for h in self._pending_help:
                    if (str(h.get("worker", "")),
                            str(h.get("need", "")).strip()) == key:
                        break  # already pending — don't duplicate
                else:
                    self._pending_help.append(payload)
                    if len(self._pending_help) > _PENDING_HELP_MAX:
                        del self._pending_help[
                            : len(self._pending_help) - _PENDING_HELP_MAX]
                    # translate the (often English) hand-raise to zh in the
                    # background so the operator reads it more easily — same
                    # fire-and-forget pattern as node summaries; the deck swaps
                    # the card text to zh when HITL_TRANSLATED arrives. Only the
                    # FIRST occurrence of a (worker, need) is translated (we're in
                    # the dedup-miss branch), so a re-raise won't re-translate.
                    if self.llm is not None:
                        try:
                            from muteki.solver.summarizer import translate_need
                            asyncio.create_task(translate_need(
                                str(payload.get("need", "")),
                                worker=str(payload.get("worker", "")),
                                model=self.titler_model, llm=self.llm,
                                bus=self.bus, run_id=self.run_id,
                                challenge_id=self.challenge.id))
                        except Exception:
                            pass
        try:
            self.bus.add_sink(_help_sink)
            self._coord_sinks.append(_help_sink)  # L3: detach on finalize
        except Exception:
            pass

async def _emit_finalize_lifecycle_deltas(self, result: dict, reason: str) -> None:
    """J/刀2: mirror release_claims_for_finalize's DB transitions onto the bus so
    the deck stops showing finalized intents as live. The DB write already
    happened (and was recorded as an event row in shared_graph); this re-emits it
    as a BLACKBOARD_DELTA the client reducer folds (intent_state_changed)."""
    if not isinstance(result, dict):
        return
    closed = [str(x) for x in (result.get("closed_intents") or []) if x]
    resumed = [str(x) for x in (result.get("resumed_intents") or []) if x]
    if closed:
        await self._emit_bb_bus(
            "intent_state_changed", intent_id=",".join(closed),
            dispatch_state="closed", close_reason="closed_by_solve",
            stop_reason="solved")
    if resumed:
        await self._emit_bb_bus(
            "intent_state_changed", intent_id=",".join(resumed),
            dispatch_state="resume", stop_reason=reason)


@staticmethod
def _split_ids(value: Any) -> list[str]:
    return [x.strip() for x in str(value or "").split(",") if x.strip()]


def _graph_event_to_bb(self, ev: dict) -> list[tuple[str, dict]]:
    seq = int(ev.get("seq") or 0)
    kind = str(ev.get("kind") or "")
    actor = str(ev.get("actor") or "")
    p = dict(ev.get("payload") or {})
    if kind == "observation_added":
        fields = dict(p)
        fields.update({
            "observation_seq": seq,
            "artifact_id": ev.get("artifact_id") or p.get("artifact_id"),
            "confidence": ev.get("confidence", 0.0),
        })
        return [("observation_added", fields)]
    if kind == "fact_added":
        # Legacy candidate rows are migrated to observation_added during graph
        # open.  Replaying the old unverified event would recreate the retired
        # Candidate concept in the UI.
        if not bool(ev.get("verified")):
            return []
        return [("fact_added", {
            "fact": p.get("fact", ""),
            "source": p.get("source", ""),
            "source_solver": p.get("source_solver") or actor,
            "verified": bool(ev.get("verified")),
            "confidence": ev.get("confidence", 1.0),
            "verifier": p.get("verifier", ""),
            "witness": p.get("witness", ""),
            "artifact_id": ev.get("artifact_id"),
            "fact_seq": seq,
            "observation_seq": p.get("observation_seq"),
            "route_hash": p.get("route_hash", ""),
            "intent_id": p.get("intent_id", ""),
            "target_epoch": p.get("target_epoch", ""),
            "fact_identity_sha256": p.get("fact_identity_sha256", ""),
            "evidence_provenance": p.get("evidence_provenance") or {},
        })]
    if kind == "fact_observed":
        fields = dict(p)
        fields.update({
            "artifact_id": ev.get("artifact_id"),
            "confidence": ev.get("confidence", 1.0),
            "observation_seq": seq,
        })
        return [("fact_observed", fields)]
    if kind == "fact_promoted":
        fields = dict(p)
        fields.update({
            "verified": True,
            "artifact_id": ev.get("artifact_id"),
            "confidence": ev.get("confidence", 1.0),
            "promotion_seq": seq,
        })
        return [("fact_promoted", fields)]
    if kind == "dead_end":
        return [("dead_end", {
            "reason": p.get("reason", ""),
            "tested_scope": p.get("tested_scope", ""),
            "observed_result": p.get("observed_result", ""),
            "intent_id": p.get("intent_id", ""),
            "target_epoch": p.get("target_epoch", ""),
            "dead_end_seq": seq,
        })]
    if kind == "intent_proposed":
        fields = dict(p)
        fields["intent_id"] = p.get("intent_id", "")
        fields["goal"] = p.get("goal", "")
        fields["intent_seq"] = seq
        return [("intent_proposed", fields)]
    if kind == "intent_claimed":
        return [("intent_claimed", {
            "intent_id": p.get("intent_id", ""),
            "worker": actor,
            "intent_seq": seq,
        })]
    if kind == "intent_concluded":
        out = []
        for iid in self._split_ids(p.get("intent_id")):
            out.append(("intent_concluded", {
                "intent_id": iid,
                "worker": actor,
                "result": p.get("result", ""),
                "result_detail": p.get("result_detail", ""),
                "to_fact_seq": p.get("to_fact_seq"),
                "intent_seq": seq,
            }))
        return out
    if kind == "intent_state_changed":
        out = []
        raw_ids = p.get("intent_id")
        if not raw_ids and isinstance(p.get("intent_ids"), list):
            raw_ids = ",".join(str(item) for item in p["intent_ids"] if item)
        for iid in self._split_ids(raw_ids):
            fields = dict(p)
            fields["intent_id"] = iid
            fields["intent_seq"] = seq
            out.append(("intent_state_changed", fields))
        return out
    if kind in {
        "fact_challenged",
        "fact_revalidated",
        "fact_rejected",
        "fact_merged",
        "fact_superseded",
    }:
        fields = dict(p)
        fields["seq"] = seq
        return [(kind, fields)]
    if kind == "flag_found":
        fields = dict(p)
        fields["flag_seq"] = seq
        return [("flag_found", fields)]
    if kind == "flag_invalidated":
        fields = dict(p)
        fields["flag_seq"] = seq
        return [("flag_invalidated", fields)]
    if kind in {"poc_saved", "poc_claimed", "poc_concluded"}:
        fields = dict(p)
        fields["seq"] = seq
        if kind == "poc_saved":
            fields["artifact_id"] = ev.get("artifact_id") or p.get("artifact_id")
        return [(kind, fields)]
    if kind == "review_finding":
        fields = dict(p)
        fields["seq"] = seq
        if "kind" in fields:
            fields["finding_kind"] = fields.pop("kind")
        return [("review_finding", fields)]
    if kind in {"route_suppressed", "route_reopened", "branch_split",
                "branch_resolved", "coordinator_directive"}:
        fields = dict(p)
        fields["seq"] = seq
        return [(kind, fields)]
    if kind in {
        "capability_published", "capability_retired", "capability_gap_reported",
        "access_path_published", "access_path_state_changed",
        "runtime_resource_registered", "runtime_resource_state_changed",
        "value_receipt",
    }:
        fields = dict(p)
        fields["seq"] = seq
        return [(kind, fields)]
    return []


async def _drain_graph_to_bus(self, *, emit_bb) -> None:
    if self.shared_graph is None:
        return
    try:
        invalidated_flags = self.shared_graph.invalidated_flags()
        events = self.shared_graph.events_since(
            self._last_graph_event_seq,
            kinds=sorted(self._GRAPH_BRIDGE_KINDS),
        )
    except Exception:
        return
    for ev in events:
        seq = int(ev.get("seq") or 0)
        payload = dict(ev.get("payload") or {})
        if (
            str(ev.get("kind") or "") == "flag_found"
            and payload.get("flag") in invalidated_flags
        ):
            # 新 execution generation 会从图头恢复事件。判错 Flag 仍保留在
            # 审计历史里，但不得再次进入当前 Run 事件流、UI 或提交 Gate。
            self._last_graph_event_seq = max(self._last_graph_event_seq, seq)
            self._graph_bridge_failures.pop(seq, None)
            continue
        emissions = self._graph_event_to_bb(ev)
        try:
            for kind, fields in emissions:
                await emit_bb(
                    kind,
                    actor=str(ev.get("actor") or "coordinator"),
                    **fields,
                )
        except Exception:
            fails = self._graph_bridge_failures.get(seq, 0) + 1
            self._graph_bridge_failures[seq] = fails
            if fails >= 3:
                self._last_graph_event_seq = max(self._last_graph_event_seq, seq)
                self._graph_bridge_failures.pop(seq, None)
                continue
            return
        self._last_graph_event_seq = max(self._last_graph_event_seq, seq)
        self._graph_bridge_failures.pop(seq, None)


def _persist_runtime_diagnostics(self, *, reason: str, solved: bool) -> None:
    """Write a redacted finish receipt before scratch cleanup.

    Never stores argv, env, prompt, or secret material. Lengths and codes are
    enough to tell launch rejection from a target-side stop after cleanup.
    """
    worker_root = getattr(self, "worker_root", None)
    if worker_root is None:
        return
    try:
        run_dir = Path(worker_root).expanduser().resolve().parent
        payload = {
            "schema": "muteki.runtime-diagnostics.v1",
            "run_id": str(getattr(self, "run_id", "") or ""),
            "reason": str(reason or ""),
            "solved": bool(solved),
            "flags": len(list(getattr(self, "_found_flags", None) or [])),
            "expected_flags": int(self._expected_flags()) if hasattr(
                self, "_expected_flags") else 0,
            "failure_code": str(
                getattr(self, "_runtime_failure_code", "") or ""),
            "failure_phase": str(
                getattr(self, "_runtime_failure_phase", "") or ""),
            "failure_detail": str(
                getattr(self, "_runtime_failure_detail", "") or "")[:800],
        }
        path = run_dir / "runtime-diagnostics.json"
        path.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
    except Exception:
        return


async def _emit_run_finished(self, *, flag: "Optional[str]", solved: bool,
                             reason: str = "finished") -> None:
    """Emit the ONE run-level RUN_FINISHED for this swarm run. Sub-workers emit
    WORKER_FINISHED (worker-level), so this is the single signal that flips the
    deck/rail to 'finished'. Best-effort: a bus failure must not mask the
    outcome the caller is about to return.

    Payloads carry `flag` (first, back-compat), `flags` (all collected), and
    `expected_flags`."""
    self._persist_runtime_diagnostics(reason=reason, solved=solved)
    self._cleanup_finished_worker_dirs()
    if self.bus is None:
        return
    try:
        runtime_meta = self._runtime_metadata_for()
        payload = {"flag": flag, "flags": list(self._found_flags),
                   "expected_flags": self._expected_flags(),
                   "multi_flag": self._multi_flag(),
                   "solved": solved,
                   "reason": reason,
                   **runtime_meta}
        if reason == "runtime_failure":
            detail = str(getattr(self, "_runtime_failure_detail", "") or "")[:800]
            if detail:
                payload.update({
                    "detail": detail,
                    "failure_code": str(
                        getattr(self, "_runtime_failure_code", "")
                        or "runtime_failure"
                    ),
                    "failure_phase": str(
                        getattr(self, "_runtime_failure_phase", "")
                        or "runtime"
                    ),
                })
        await self.bus.emit(Event(
            event_type=EventType.RUN_FINISHED, run_id=self.run_id,
            challenge_id=self.challenge.id,
            payload=payload))
    except Exception:
        pass
