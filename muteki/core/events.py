"""Typed event schema — the single contract between the agent core and any frontend.

AG-UI-style ordered, typed event stream. Web and TUI are dumb subscribers; they
never call the core directly, they only subscribe to these events and post HITL.

Appendix A of the design doc, fleshed out with typed payload constructors so
producers don't hand-build dicts (which drift and break the frontend).
"""

from __future__ import annotations

import time
from enum import Enum
from typing import Any, Optional

from pydantic import BaseModel, Field


class EventType(str, Enum):
    RUN_STARTED = "run.started"
    RUN_TITLED = "run.titled"  # auto-generated conversation title landed (rail label)
    RUN_FINISHED = "run.finished"
    RUN_REOPENED = "run.reopened"  # a terminal run was re-opened (continue solving
    #   or flag marked false-positive) — rail flips solved/finished→running
    WORKER_STATUS = "worker.status"  # a solver worker came online/offline
    WORKER_FINISHED = "worker.finished"  # ONE swarm sub-worker ended (worker-level,
    #   NOT the run). In coordinator mode many workers come and go while the run keeps
    #   going (re-bootstrap until solved/stopped), so a worker ending must NOT mark the
    #   whole run finished — only the coordinator's own exit emits a run-level
    #   RUN_FINISHED. A single-solver run (mock / race / standby) is its own run, so it
    #   still emits RUN_FINISHED directly.
    TEXT_MESSAGE_DELTA = "text.delta"
    REASONING_DELTA = "reasoning.delta"
    TOOL_CALL_START = "tool.start"
    TOOL_CALL_ARGS = "tool.args"
    TOOL_CALL_RESULT = "tool.result"
    TERMINAL_OUTPUT = "terminal.output"  # sandbox PTY byte stream
    CONTEXT_STATE = "context.state"  # context "fuel gauge"
    SOLVE_GRAPH_DELTA = "solvegraph.delta"  # evidence / hypothesis / dead-end change
    INSIGHT_BUS_EVENT = "insight.event"  # Fact / DeadEnd / FlagFound
    SHARED_GRAPH_DELTA = "sharedgraph.delta"  # P-A/P-B: verified/candidate evidence on the shared graph
    REASON_INTENT = "reason.intent"  # P-C: planner proposed a typed intent (or goal_met)
    BLACKBOARD_DELTA = "blackboard.delta"  # shared knowledge blackboard lifecycle:
    #   intent proposed/claimed/concluded (who claimed what, done?), fact, dead-end,
    #   flag — the full collaboration layer of the SQLiteSharedGraph, surfaced so the
    #   deck can render the blackboard canvas (claim animations + provenance + timeline)
    NODE_SUMMARIZED = "node.summarized"  # a deepseek-flash one-line zh gist for a
    #   fact/intent node landed (graph + blackboard swap the truncated raw text for
    #   the gist; the raw stays in a <details> on the card)
    COST_UPDATE = "cost.update"
    STALLED = "guard.stalled"
    GUIDANCE_INJECTED = "coordinator.guidance"
    HITL_REQUEST = "hitl.request"  # agent asks a human to decide
    HITL_RESPONSE = "hitl.response"  # human issues a command / interrupt
    HITL_TRANSLATED = "hitl.translated"  # a zh translation of a worker's hand-raise
    GRAPH_COMPACTED = "graph.compacted"  # H: a long-run graph compaction epoch landed
    WORKER_LIFECYCLE = "worker.lifecycle"  # I: granular worker lifecycle (spawned/
    #   phase_changed/stalled/exited) — finer than WORKER_STATUS online/offline


class Event(BaseModel):
    event_type: EventType
    seq: int = 0  # monotonically increasing, used for Last-Event-ID resume
    ts: float = 0.0  # epoch seconds
    run_id: str
    challenge_id: Optional[str] = None
    solver_id: Optional[str] = None
    payload: dict[str, Any] = Field(default_factory=dict)

    def model_post_init(self, __ctx: Any) -> None:
        if not self.ts:
            object.__setattr__(self, "ts", time.time())

    # --- SSE serialization helpers (Last-Event-ID continuity) ---
    def to_sse(self) -> str:
        """Render as a Server-Sent-Events frame. `id:` carries seq for resume."""
        data = self.model_dump_json()
        return f"id: {self.seq}\nevent: {self.event_type.value}\ndata: {data}\n\n"


# ---------------------------------------------------------------------------
# Typed payload constructors.
# Producers call these; they return a `payload` dict with a stable shape.
# ---------------------------------------------------------------------------


def context_state_payload(
    zones: list[dict[str, Any]],
    total: int,
    limit: int,
    compacted: Optional[list[dict[str, Any]]] = None,
) -> dict[str, Any]:
    """Context fuel-gauge. zones: [{"label": "system", "tokens": 1200}, ...]"""
    return {
        "zones": zones,
        "total": total,
        "limit": limit,
        "compacted": compacted or [],
    }


def tool_result_payload(
    tool: str,
    result: Any,
    artifact_id: Optional[str] = None,
    truncated: bool = False,
) -> dict[str, Any]:
    return {
        "tool": tool,
        "result": result,
        "artifact_id": artifact_id,
        "truncated": truncated,
    }


def insight_payload(kind: str, **fields: Any) -> dict[str, Any]:
    """kind in {FactDiscovered, DeadEndMarked, FlagFound, ...}; fields are kind-specific."""
    return {"kind": kind, **fields}


def cost_payload(scope: str, usd: float, tokens: int, **extra: Any) -> dict[str, Any]:
    return {"scope": scope, "usd": round(usd, 6), "tokens": tokens, **extra}


def worker_status_payload(
    online: bool, *, status: str = "", reason: str = "", engine: str = "",
    session: str = "", runtime: Optional[dict[str, Any]] = None,
    worker_role: str = "",
) -> dict[str, Any]:
    """Live worker presence for the deck's interpreter cluster.

    `reason` is intentionally small and UI-oriented: started | finished |
    solved | timeout | cancelled | error. It is observational telemetry only;
    it never participates in fact or flag verification.

    `session` is the worker's live CLI session id (claude `-r <id>` / codex
    `exec resume <id>`), shown on the lane so an operator can manually attach to a
    running worker. Empty until the engine assigns one.
    """
    payload = {"online": online, "status": status, "reason": reason,
               "engine": engine, "session": session}
    if worker_role:
        payload["worker_role"] = worker_role
    if runtime:
        payload["runtime"] = runtime
    return payload


def hitl_response_payload(target: str, action: str, **fields: Any) -> dict[str, Any]:
    """target like 'global' | 'challenge:c1' | 'solver:opus-max'; action like 'hint'|'pause'."""
    return {"target": target, "action": action, **fields}


def hitl_request_payload(worker: str, need: str, *, kind: str = "need_input",
                         **fields: Any) -> dict[str, Any]:
    """A worker RAISES ITS HAND: it needs something only the operator can supply
    (`kind="need_input"`: a VPS/credential/tool) or the environment is unusable
    (`kind="env_down"`: target unreachable/expired). `need` is the concrete ask.
    The deck renders this so the operator knows what to provide; the coordinator
    pauses re-spawning that direction until an operator command arrives."""
    return {"worker": worker, "need": need, "kind": kind, **fields}


def solve_graph_delta_payload(kind: str, **fields: Any) -> dict[str, Any]:
    """kind in {evidence_added, hypothesis_added, hypothesis_status, dead_end, flag}."""
    return {"kind": kind, **fields}


def shared_graph_delta_payload(
    fact: str, *, verified: bool, confidence: float, actor: str = "",
    artifact_id: Optional[str] = None, verifier: str = "",
    fact_seq: int = -1,
) -> dict[str, Any]:
    """P-A/P-B: an evidence row entered the shared graph, with its gate verdict.

    fact_seq is the shared_graph event seq (so the UI can address this fact by a
    stable id and collapse it onto the same graph node the blackboard delta made)."""
    return {"fact": fact, "verified": verified, "confidence": confidence,
            "actor": actor, "artifact_id": artifact_id, "verifier": verifier,
            "fact_seq": fact_seq}


def reason_intent_payload(
    goal_met: bool, intents: list[dict[str, Any]], audit: list[str],
) -> dict[str, Any]:
    """P-C: the planner's output — typed intents + evidence-audit notes."""
    return {"goal_met": goal_met, "intents": intents, "audit": audit}


def node_summarized_payload(
    summary: str, *, node_kind: str,
    fact_seq: int = -1, intent_id: str = "",
) -> dict[str, Any]:
    """A short zh gist for one graph/blackboard node (fact or intent) is ready.

    node_kind is "fact" or "intent". The node is addressed by `fact_seq`
    (for facts, matching the `fact:{seq}` graph id) or `intent_id` (for intents,
    matching `intent:{id}`). The frontend swaps the node's displayed label for
    `summary` while keeping the raw text in a disclosure."""
    return {"summary": summary, "node_kind": node_kind,
            "fact_seq": fact_seq, "intent_id": intent_id}


def hitl_translated_payload(worker: str, need: str, need_zh: str) -> dict[str, Any]:
    """A zh translation of a worker's hand-raise (NEED_INPUT) is ready. The deck
    matches it to the pending HITL_REQUEST by (worker, need) and shows `need_zh`
    on the card, keeping the raw `need` available. Async/eventual — the card renders
    the raw text first and swaps to zh when this arrives."""
    return {"worker": worker, "need": need, "need_zh": need_zh}


def worker_lifecycle_payload(
    phase: str, *, intent_id: str = "", tokens_spent: int = 0,
    paused: bool = False, **extra: Any,
) -> dict[str, Any]:
    """I: a granular worker-lifecycle transition (finer than WORKER_STATUS).

    `phase` is one of: spawned | phase_changed | stalled | exited (and the running
    phase label for phase_changed, e.g. bootstrap/explore/review). `intent_id` ties
    the worker to the intent it is executing; `tokens_spent` is its running total."""
    return {"phase": phase, "intent_id": intent_id,
            "tokens_spent": int(tokens_spent), "paused": bool(paused), **extra}


def blackboard_delta_payload(kind: str, *, actor: str = "", **fields: Any) -> dict[str, Any]:
    """One change on the shared knowledge blackboard. `kind` is one of:
      intent_proposed  {intent_id, goal, worker_class}
      intent_claimed   {intent_id, worker}            — a solver took the task
      intent_concluded {intent_id, worker, result}    — and finished it
      fact_added       {fact, verified, confidence, verifier, artifact_id}
      dead_end         {reason}
      flag_found       {flag}
    `actor` is the solver that caused it. The deck folds these into the blackboard
    canvas (claim state per intent, provenance per fact, an event timeline)."""
    return {"kind": kind, "actor": actor, **fields}
