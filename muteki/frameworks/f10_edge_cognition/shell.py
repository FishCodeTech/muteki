"""Edge worker shell protocol helpers (central remains a light arbiter).

Two sides, one file:

- **Central side** (`start_shell`, `guidance_from_envelope`, `ingest_edge_events`):
  registers per-worker budget ceilings, hands the worker an IntentEnvelope
  (design §2.4), and folds worker-appended checkpoint/sub-intent EVENTS into the
  strategic ledger.  The central side NEVER invents worker cognition — no
  ghost-written plan queues, no fabricated stuck verdicts, no hardcoded token
  counts.  Everything in the ledger traces to a real worker-reported event.

- **Worker side** (`emit_shell_checkpoint`, `emit_sub_intent`): append-only
  EdgeMessage events (design §2.3) onto the shared graph's event log.  The
  worker's local belief state lives in `<workspace>/.shell/state.json`
  (state.py); the bus only carries verified checkpoints.
"""

from __future__ import annotations

from contextlib import nullcontext
import json
import time
import uuid
from typing import Any, Optional

from muteki.frameworks.f10_edge_cognition.state import (
    PLAN_EVERY,
    STUCK_LIMIT,
    TOKEN_LIMIT,
    TURN_LIMIT,
    goal_id_for,
    render_envelope_guidance,
)

EDGE_CHECKPOINT_KINDS = (
    "edge_shell_checkpoint",
    "edge_shell_stuck",
    "edge_sub_intent",
)


def _append(graph: Any, kind: str, payload: dict[str, Any], *,
            actor: str = "f10") -> int:
    fn = getattr(graph, "_append", None)
    if not callable(fn):
        return -1
    try:
        return int(fn(kind, actor, payload))
    except Exception:
        return -1


def _conn_lock(graph: Any) -> tuple[Any, Any]:
    return getattr(graph, "_conn", None), getattr(graph, "_lock", None)


def ensure_run_budget(graph: Any, *, run_id: str, token_budget: int = 400000) -> None:
    conn, lock = _conn_lock(graph)
    if conn is None:
        return
    with lock if lock is not None else nullcontext():
        conn.execute(
            "INSERT OR IGNORE INTO edge_run_budget "
            "(run_id, token_budget, tokens_spent, status) VALUES (?,?,0,'open')",
            (run_id, token_budget),
        )
        conn.commit()


def start_shell(
    graph: Any,
    *,
    intent_id: str,
    goal: str,
    category: str,
    token_limit: int = TOKEN_LIMIT,
    turn_limit: int = TURN_LIMIT,
    predicted_effects: Optional[list] = None,
    success_criteria: Optional[list] = None,
    from_facts: Optional[list] = None,
    lane_key: str = "",
    risk_class: str = "",
    priority: float = 0.6,
    source: str = "prepare",
) -> dict[str, Any]:
    """Register an intent as an edge worker shell and return its IntentEnvelope.

    Central writes only what central OWNS: the budget ceiling row, the intent
    queue row, the lifecycle spawn record.  The plan queue, working memory and
    stuck counter are the worker's — they are born empty in the worker's local
    state.json, not here.
    """
    shell_id = f"shell-{uuid.uuid4().hex[:10]}"
    conn, lock = _conn_lock(graph)
    now = time.time()
    effects = [dict(e) for e in (predicted_effects or []) if isinstance(e, dict)]
    criteria = [str(c) for c in (success_criteria or []) if c] or [
        "verified fact from REAL tool output",
    ]
    if conn is not None:
        with lock if lock is not None else nullcontext():
            conn.execute(
                "INSERT OR REPLACE INTO edge_worker_budget "
                "(shell_id, intent_id, token_limit, tokens_used, turn_limit, "
                " turns_used, killed, updated_at) VALUES (?,?,?,0,?,0,0,?)",
                (shell_id, intent_id, token_limit, turn_limit, now),
            )
            conn.execute(
                "INSERT OR REPLACE INTO edge_intent_queue "
                "(intent_id, goal, predicted_effects, priority, source, created_at) "
                "VALUES (?,?,?,?,?,?)",
                (
                    intent_id,
                    goal[:2000],
                    json.dumps(effects, ensure_ascii=False),
                    float(priority),
                    str(source or "prepare"),
                    now,
                ),
            )
            conn.execute(
                "INSERT INTO edge_worker_lifecycle "
                "(shell_id, event, ts, intent_id, payload) VALUES (?,?,?,?,?)",
                (
                    shell_id,
                    "spawn",
                    now,
                    intent_id,
                    json.dumps({"goal": goal[:200]}, ensure_ascii=False),
                ),
            )
            conn.commit()
    # Make the intent dispatchable by the ordinary coordinator: the edge queue is
    # f10's own ledger, but workers are spawned off the shared graph's intents
    # table.  propose_intent dedupes on intent_id, so seeding an intent Reason
    # already proposed is a no-op.
    propose = getattr(graph, "propose_intent", None)
    if callable(propose):
        try:
            propose(
                actor="f10-edge",
                intent_id=intent_id,
                goal=goal,
                payload={
                    "worker_class": "shell_agent",
                    "source": f"edge_{source}",
                    "lane_key": lane_key,
                    "risk_class": risk_class,
                    "priority": priority,
                },
                from_fact_seqs=[int(s) for s in (from_facts or [])
                                if str(s).isdigit()],
            )
        except Exception:
            pass
    envelope = {
        "intent_id": intent_id,
        "shell_id": shell_id,
        "goal": goal,
        "goal_id": goal_id_for(goal),
        "predicted_effects": effects,
        "success_criteria": criteria,
        "from_facts": list(from_facts or []),
        "lane_key": lane_key,
        "risk_class": risk_class,
        "category": category,
        "budget": {"token_limit": int(token_limit), "turn_limit": int(turn_limit)},
        "profile": {"plan_every": PLAN_EVERY, "stuck_limit": STUCK_LIMIT},
    }
    _append(
        graph,
        "edge_shell_started",
        {"shell_id": shell_id, "intent_id": intent_id, "turn_limit": turn_limit},
    )
    _append(
        graph,
        "edge_intent_spawned",
        {"intent_id": intent_id, "shell_id": shell_id, "source": source},
    )
    return envelope


def guidance_from_envelope(envelope: dict[str, Any]) -> list[str]:
    """Worker-facing guidance for one envelope: line 1 is the machine-readable
    marker CliSolver parses to enter the shell loop; the rest is the human
    protocol contract (also embedded in the worker's turn-1 prompt, so this
    survives standing-guidance truncation)."""
    budget = envelope.get("budget") or {}
    profile = envelope.get("profile") or {}
    lines = [
        render_envelope_guidance(envelope),
        "[f10-edge-shell] You are a multi-step mini-agent, not a one-shot "
        "executor: Observe→Plan→Execute→Verify→Reflect→Checkpoint, at most "
        f"{budget.get('turn_limit', TURN_LIMIT)} turns.",
        "[f10-edge-shell] Re-plan only every "
        f"{profile.get('plan_every', PLAN_EVERY)} turns (PLAN_STEP= lines); on "
        "other turns execute the next queued step against its predefined verifier.",
        "[f10-edge-shell] Reject soliloquizing: every observation must come from "
        "real tool/shell output; a VERIFIED_FACT= needs a real witness.",
        "[f10-edge-shell] Discover a genuinely different direction worth a "
        "separate worker? Emit SUB_INTENT=<goal> and the central arbiter queues it.",
        "[f10-edge-shell] On "
        f"{profile.get('stuck_limit', STUCK_LIMIT)} consecutive barren turns "
        "(no new fact/dead-end/artifact): mark DEADEND=, checkpoint, stop.",
    ]
    return lines


# ── worker side: append-only EdgeMessage events (design §2.3) ────────────────


def emit_shell_checkpoint(
    graph: Any,
    *,
    shell_id: str,
    intent_id: str,
    checkpoint: dict[str, Any],
    budget_signal: dict[str, Any],
    stuck: bool = False,
) -> int:
    """Append one worker checkpoint event. The worker reports REAL counts only;
    ledger bookkeeping happens centrally in ingest_edge_events."""
    payload = {
        "kind": "shell_checkpoint",
        "actor": str(shell_id),
        "intent_id": str(intent_id),
        "shell_id": str(shell_id),
        "checkpoint": dict(checkpoint or {}),
        "budget_signal": dict(budget_signal or {}),
    }
    kind = "edge_shell_stuck" if stuck else "edge_shell_checkpoint"
    return _append(graph, kind, payload, actor=str(shell_id) or "f10")


def emit_sub_intent(
    graph: Any,
    *,
    shell_id: str,
    intent_id: str,
    goal: str,
    parent_intent_id: str = "",
    priority: float = 0.7,
) -> int:
    """Worker declares a sub-intent (design §4.1b): appended to the bus; the
    central arbiter persists it into edge_intent_queue + the shared intents
    table during ingestion."""
    payload = {
        "kind": "sub_intent",
        "actor": str(shell_id),
        "shell_id": str(shell_id),
        "intent_id": str(intent_id),
        "parent_intent_id": str(parent_intent_id or ""),
        "goal": str(goal or "")[:2000],
        "priority": float(priority),
    }
    return _append(graph, "edge_sub_intent", payload, actor=str(shell_id) or "f10")


# ── central side: ingest worker events into the strategic ledger ─────────────


def _ingest_one_checkpoint(
    graph: Any,
    payload: dict[str, Any],
    *,
    stuck: bool,
    tripped: list[str],
) -> None:
    """Fold one worker-reported checkpoint into the ledger. Absolute counters
    come from the worker's budget_signal; budget trips kill the shell here."""
    conn, lock = _conn_lock(graph)
    shell_id = str(payload.get("shell_id") or payload.get("actor") or "")
    intent_id = str(payload.get("intent_id") or "")
    if not shell_id:
        return
    signal = payload.get("budget_signal") or {}
    if not isinstance(signal, dict):
        signal = {}
    checkpoint = payload.get("checkpoint") or {}
    if not isinstance(checkpoint, dict):
        checkpoint = {}
    now = time.time()
    tokens_used = int(signal.get("tokens_used") or 0)
    turns_used = int(signal.get("turns_used") or 0)
    if conn is not None:
        with lock if lock is not None else nullcontext():
            row = conn.execute(
                "SELECT token_limit, turn_limit, killed FROM edge_worker_budget "
                "WHERE shell_id=?",
                (shell_id,),
            ).fetchone()
            if row is None:
                # Self-heal a shell the central never registered (e.g. a worker
                # resumed from a local checkpoint after a central restart): adopt
                # the envelope limits the worker carried in its budget signal.
                conn.execute(
                    "INSERT OR IGNORE INTO edge_worker_budget "
                    "(shell_id, intent_id, token_limit, tokens_used, turn_limit, "
                    " turns_used, killed, updated_at) VALUES (?,?,?,0,?,0,0,?)",
                    (
                        shell_id,
                        intent_id,
                        int(signal.get("token_limit") or TOKEN_LIMIT),
                        int(signal.get("turn_limit") or TURN_LIMIT),
                        now,
                    ),
                )
                row = conn.execute(
                    "SELECT token_limit, turn_limit, killed FROM "
                    "edge_worker_budget WHERE shell_id=?",
                    (shell_id,),
                ).fetchone()
            conn.execute(
                "UPDATE edge_worker_budget SET tokens_used=?, turns_used=?, "
                "updated_at=? WHERE shell_id=?",
                (tokens_used, turns_used, now, shell_id),
            )
            conn.execute(
                "INSERT INTO edge_worker_lifecycle "
                "(shell_id, event, ts, intent_id, payload) VALUES (?,?,?,?,?)",
                (
                    shell_id,
                    "stuck" if stuck else "checkpoint",
                    now,
                    intent_id,
                    json.dumps(
                        {
                            "turn": checkpoint.get("turn"),
                            "new_facts": len(checkpoint.get("new_facts") or []),
                            "new_deadends": len(
                                checkpoint.get("new_deadends") or []),
                            "tokens_used": tokens_used,
                            "turns_used": turns_used,
                        },
                        ensure_ascii=False,
                    ),
                ),
            )
            # run-level burn: recompute from per-worker absolutes (idempotent).
            conn.execute(
                "UPDATE edge_run_budget SET tokens_spent="
                "(SELECT COALESCE(SUM(tokens_used),0) FROM edge_worker_budget)"
            )
            trip = False
            if row is not None and not int(row[2] or 0):
                token_limit, turn_limit = int(row[0]), int(row[1])
                if tokens_used >= token_limit or turns_used >= turn_limit:
                    conn.execute(
                        "UPDATE edge_worker_budget SET killed=1 WHERE shell_id=?",
                        (shell_id,),
                    )
                    trip = True
            conn.commit()
    else:
        trip = False
    if trip:
        tripped.append(shell_id)
        _append(
            graph,
            "edge_budget_trip",
            {"shell_id": shell_id, "intent_id": intent_id,
             "tokens_used": tokens_used, "turns_used": turns_used},
        )
        _append(
            graph,
            "edge_shell_killed",
            {"shell_id": shell_id, "intent_id": intent_id, "reason": "budget"},
        )


def _ingest_one_sub_intent(graph: Any, payload: dict[str, Any]) -> None:
    """Persist a worker-declared sub-intent into the edge queue and (when the
    graph supports it) the shared intents table so the coordinator dispatches
    it like any other open intent."""
    conn, lock = _conn_lock(graph)
    intent_id = str(payload.get("intent_id") or "")
    goal = str(payload.get("goal") or "")
    if not intent_id or not goal:
        return
    now = time.time()
    if conn is not None:
        with lock if lock is not None else nullcontext():
            conn.execute(
                "INSERT OR REPLACE INTO edge_intent_queue "
                "(intent_id, goal, predicted_effects, priority, source, created_at) "
                "VALUES (?,?,'[]',?,'worker',?)",
                (intent_id, goal[:2000], float(payload.get("priority") or 0.7), now),
            )
            conn.commit()
    propose = getattr(graph, "propose_intent", None)
    if callable(propose):
        try:
            propose(
                actor=str(payload.get("shell_id") or "f10-edge"),
                intent_id=intent_id,
                goal=goal,
                payload={
                    "worker_class": "shell_agent",
                    "source": "edge_sub_intent",
                    "priority": float(payload.get("priority") or 0.7),
                },
            )
        except Exception:
            pass


def ingest_edge_events(graph: Any, *, since_seq: int = 0) -> dict[str, Any]:
    """Central ingestion pass: fold every worker-appended edge event after
    ``since_seq`` into the strategic ledger. Returns the new cursor plus the
    shells whose budgets tripped (the caller cancels their live workers)."""
    report: dict[str, Any] = {
        "since_seq": int(since_seq or 0),
        "checkpoints": 0,
        "stuck": 0,
        "sub_intents": 0,
        "tripped": [],
    }
    events_since = getattr(graph, "events_since", None)
    if not callable(events_since):
        return report
    try:
        events = events_since(since_seq, kinds=list(EDGE_CHECKPOINT_KINDS))
    except Exception:
        return report
    for event in events or []:
        try:
            seq = int(event.get("seq") or 0)
        except Exception:
            seq = 0
        report["since_seq"] = max(report["since_seq"], seq)
        kind = str(event.get("kind") or "")
        payload = event.get("payload") or {}
        if not isinstance(payload, dict):
            continue
        if kind == "edge_sub_intent":
            _ingest_one_sub_intent(graph, payload)
            report["sub_intents"] += 1
        elif kind in ("edge_shell_checkpoint", "edge_shell_stuck"):
            _ingest_one_checkpoint(
                graph, payload,
                stuck=(kind == "edge_shell_stuck"),
                tripped=report["tripped"],
            )
            if kind == "edge_shell_stuck":
                report["stuck"] += 1
            else:
                report["checkpoints"] += 1
    return report


# ── capability profile (unchanged mechanics; real outcomes only) ─────────────


def record_capability(
    graph: Any, *, engine: str, category: str, success: bool
) -> None:
    conn, lock = _conn_lock(graph)
    if conn is None or not engine:
        return
    now = time.time()
    with lock if lock is not None else nullcontext():
        row = conn.execute(
            "SELECT win_rate, sample_count FROM edge_capability_profile "
            "WHERE engine=? AND category=?",
            (engine, category),
        ).fetchone()
        if row is None:
            conn.execute(
                "INSERT INTO edge_capability_profile "
                "(engine, category, win_rate, tokens_per_flag, sample_count, "
                " updated_at) VALUES (?,?,?,?,1,?)",
                (engine, category, 1.0 if success else 0.0, None, now),
            )
        else:
            n = int(row[1]) + 1
            wr = (float(row[0]) * (n - 1) + (1.0 if success else 0.0)) / n
            conn.execute(
                "UPDATE edge_capability_profile SET win_rate=?, sample_count=?, "
                "updated_at=? WHERE engine=? AND category=?",
                (wr, n, now, engine, category),
            )
        conn.commit()


def pick_by_capability(
    graph: Any,
    available: list[str],
    *,
    category: str,
    running: list[str] | None = None,
) -> str | None:
    if not available:
        return None
    conn, _ = _conn_lock(graph)
    running = list(running or [])
    best = None
    best_score = -1.0
    for eng in available:
        score = 0.5
        if conn is not None:
            row = conn.execute(
                "SELECT win_rate, sample_count FROM edge_capability_profile "
                "WHERE engine=? AND category=?",
                (eng, category),
            ).fetchone()
            if row and int(row[1]) > 0:
                score = float(row[0])
        if eng in running:
            score -= 0.05
        if score > best_score:
            best_score = score
            best = eng
    return best
