"""f11 agent-teams core ops: roster, mailbox, tasks, tokens, assertions, channel."""

from __future__ import annotations

from contextlib import nullcontext
import hashlib
import json
import time
import uuid
from typing import Any

from muteki.frameworks.f11_agent_teams.schema import (
    TEAMMATE_ALLOWED_CMDS,
    TEAMMATE_FORBIDDEN_CMDS,
)

CHANNEL_KINDS = frozenset({"evidence", "dead_end", "surprise", "request_help"})
EVIDENCE_REQUIRED_KINDS = frozenset(
    {"evidence", "dead_end", "contradiction", "channel"}
)
# Protocol frames (§2.3/§4.3): never counted against msg_cap, always deliverable.
PROTOCOL_KINDS = frozenset({"ack", "heartbeat"})
# Past msg_cap / open circuit breaker only evidence-class traffic is accepted
# (§2.1 circuit_breaker, §4.4): direct evidence msgs + channel evidence traces.
CAP_EXEMPT_KINDS = frozenset({"evidence", "channel:evidence"})
LEAD_PURPOSES = frozenset(
    {"briefing", "plan_verdict", "contradiction", "dead_letter", "closing"}
)
MSG_KINDS = frozenset(
    {
        "direct",
        "broadcast",
        "handoff",
        "plan_request",
        "plan_verdict",
        "evidence",
        "dead_end",
        "contradiction",
        "heartbeat",
        "channel",
        "ack",
    }
)
DEFAULT_ROLES = ("recon", "exploit", "verify")


def _append(graph: Any, kind: str, payload: dict[str, Any]) -> int:
    fn = getattr(graph, "_append", None)
    if not callable(fn):
        return -1
    try:
        return int(fn(kind, "f11", payload))
    except Exception:
        return -1


def _conn_lock(graph: Any):
    conn = getattr(graph, "_conn", None)
    lock = getattr(graph, "_lock", None)
    return conn, (lock if lock is not None else nullcontext())


def goal_id_for(goal: str) -> str:
    return hashlib.sha256(goal.strip().encode("utf-8")).hexdigest()[:16]


def teammate_cmd_allowed(cmd: str, *, mode: str = "teammate") -> bool:
    """Gate-0b: teammate mode only registers whitelist; forbid full-board reads."""
    c = (cmd or "").strip().lstrip("-")
    if mode != "teammate":
        return True
    if c in TEAMMATE_FORBIDDEN_CMDS:
        return False
    return c in TEAMMATE_ALLOWED_CMDS


def form_team(
    graph: Any,
    *,
    challenge_id: str,
    run_id: str = "",
    roles: tuple[str, ...] = DEFAULT_ROLES,
    lead_model: str = "glm-5.2:cloud",
    lead_calls_used: int = 0,
) -> dict[str, Any]:
    """T01: create named team roster + initial task list + optional chain token.

    ``lead_calls_used`` reflects REAL glm calls already spent by the caller
    (T05 budget accounting lives in the roster row, not in hope).
    """
    conn, lock_cm = _conn_lock(graph)
    team_id = f"team-{challenge_id}"
    now = time.time()
    members = []
    for i, role in enumerate(roles):
        members.append(
            {
                "name": f"{role}-1",
                "role": role,
                "worker_id": "",
                "session_id": "",
                "state": "active",
                "last_heartbeat": now,
                "spawn_seq": i + 1,
            }
        )
    budget = {
        "wall_ms": 900000,
        "cost_cap_usd": 400,
        "msg_cap": 200,
        "circuit_breaker": "+30%",
        "cost_usd": 0.0,
        "baseline_cost_usd": None,
        "circuit_open": False,
        "channel_per_member_cap": 12,
    }
    if conn is not None:
        with lock_cm:
            conn.execute(
                "INSERT OR REPLACE INTO team_roster "
                "(team_id, challenge_id, lead_model, lead_calls_used, "
                " lead_calls_cap, lead_cooldown_s, members_json, budget_json, "
                " created_at, status) VALUES (?,?,?,?,12,45,?,?,?,'active')",
                (
                    team_id,
                    challenge_id,
                    lead_model,
                    int(lead_calls_used),
                    json.dumps(members, ensure_ascii=False),
                    json.dumps(budget),
                    now,
                ),
            )
            conn.execute(
                "INSERT OR IGNORE INTO team_seq (team_id, next_seq) VALUES (?,1)",
                (team_id,),
            )
            conn.commit()

    seed_goals = [
        ("recon", "inventory challenge surface and collect verified facts"),
        ("exploit", "pursue highest-leverage attack path with declared effects"),
        ("verify", "adversarially check claims; require tool evidence"),
    ]
    tasks = []
    for role, goal in seed_goals:
        if role not in roles:
            continue
        tid = create_task(
            graph,
            team_id=team_id,
            goal=goal,
            created_by="lead",
            declared_effects=[{"selector": f"{role}.progress", "op": "eq", "value": True}],
        )
        if tid:  # "" = rejected by an open circuit breaker (T07)
            tasks.append(tid)

    # Sequential protocol token for order-sensitive handoffs (Gate-0 substrate).
    token = grant_token(
        graph,
        team_id=team_id,
        protocol="chain-handoff-1",
        holder=members[0]["name"] if members else None,
    )

    payload = {
        "team_id": team_id,
        "challenge_id": challenge_id,
        "run_id": run_id,
        "members": [m["name"] for m in members],
        "roles": list(roles),
        "seed_tasks": tasks,
        "token_id": token.get("token_id") if token else None,
        "lead_model": lead_model,
    }
    _append(graph, "team_formed", payload)
    return {
        "team_id": team_id,
        "members": members,
        "tasks": tasks,
        "token": token,
        "lead_calls_used": int(lead_calls_used),
    }


def create_task(
    graph: Any,
    *,
    team_id: str,
    goal: str,
    created_by: str,
    declared_effects: list[dict[str, Any]] | None = None,
    depends_on: list[str] | None = None,
) -> str:
    """T04/T11 task-list write. Returns "" when the T07 circuit breaker is open
    (frozen teams accept no new tasks, §4.4)."""
    conn, lock_cm = _conn_lock(graph)
    task_id = f"task-{uuid.uuid4().hex[:10]}"
    now = time.time()
    gid = goal_id_for(goal)
    effects = json.dumps(declared_effects or [], ensure_ascii=False)
    deps = json.dumps(depends_on or [])
    if conn is not None:
        with lock_cm:
            # T07 circuit breaker (§4.4): frozen teams accept no new tasks.
            row = conn.execute(
                "SELECT budget_json FROM team_roster WHERE team_id=?", (team_id,)
            ).fetchone()
            budget = json.loads(row[0]) if row and row[0] else {}
            if budget.get("circuit_open"):
                conn.commit()
                return ""
            conn.execute(
                "INSERT INTO team_task "
                "(task_id, team_id, goal, goal_id, declared_effects, status, "
                " depends_on, evidence_refs, created_by, fence, created_at, "
                " updated_at) VALUES (?,?,?,?,?,'pending',?,'[]',?,0,?,?)",
                (task_id, team_id, goal[:2000], gid, effects, deps, created_by, now, now),
            )
            conn.commit()
    return task_id


def claim_task(
    graph: Any,
    *,
    team_id: str,
    task_id: str,
    owner: str,
    require_token: bool = False,
    token_id: str | None = None,
    token_fence: int | None = None,
) -> dict[str, Any]:
    """T11 atomic claim; optional turn-token gate for sequential tasks."""
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    now = time.time()
    with lock_cm:
        if require_token:
            if not token_id:
                return {"ok": False, "error": "token_required"}
            row = conn.execute(
                "SELECT holder, fence, status, lease_expires_at FROM team_turn_token "
                "WHERE token_id=? AND team_id=?",
                (token_id, team_id),
            ).fetchone()
            if not row:
                return {"ok": False, "error": "token_missing"}
            holder, fence, status, expires = row
            if status != "active" or holder != owner:
                return {"ok": False, "error": "token_not_held"}
            if token_fence is not None and int(fence) != int(token_fence):
                return {"ok": False, "error": "token_fence_mismatch"}
            if expires is not None and float(expires) < now:
                return {"ok": False, "error": "token_expired"}

        cur = conn.execute(
            "UPDATE team_task SET status='claimed', owner=?, fence=fence+1, "
            "lease_json=?, updated_at=? "
            "WHERE task_id=? AND team_id=? AND status='pending' "
            "AND (owner IS NULL OR owner='')",
            (
                owner,
                json.dumps({"owner": owner, "claimed_at": now}),
                now,
                task_id,
                team_id,
            ),
        )
        if cur.rowcount != 1:
            conn.commit()
            return {"ok": False, "error": "claim_lost"}
        fence_row = conn.execute(
            "SELECT fence FROM team_task WHERE task_id=?", (task_id,)
        ).fetchone()
        conn.commit()
    fence_v = int(fence_row[0]) if fence_row else 0
    _append(
        graph,
        "team_task_claimed",
        {"team_id": team_id, "task_id": task_id, "owner": owner, "fence": fence_v},
    )
    return {"ok": True, "task_id": task_id, "owner": owner, "fence": fence_v}


def _cascade_invalidate(
    conn: Any, team_id: str, terminal_ids: list[str]
) -> list[str]:
    """T11/§2.2 depends_on cascade: pending tasks whose prerequisite reached a
    terminal state are invalidated, transitively (链式收尾, §4.1.5/§4.4)."""
    roots = {str(t) for t in terminal_ids if t}
    rows = conn.execute(
        "SELECT task_id, depends_on FROM team_task WHERE team_id=? "
        "AND status='pending'",
        (team_id,),
    ).fetchall()
    deps = {
        str(r[0]): [str(d) for d in json.loads(r[1] or "[]")] for r in rows
    }
    dead = set(roots)
    changed = True
    while changed:
        changed = False
        for tid, ds in deps.items():
            if tid not in dead and any(d in dead for d in ds):
                dead.add(tid)
                changed = True
    invalidated = sorted(dead - roots)
    now = time.time()
    for tid in invalidated:
        conn.execute(
            "UPDATE team_task SET status='invalidated', updated_at=? "
            "WHERE task_id=? AND team_id=? AND status='pending'",
            (now, tid, team_id),
        )
    return invalidated


def complete_task(
    graph: Any,
    *,
    team_id: str,
    task_id: str,
    owner: str,
    evidence_refs: list[dict[str, Any]],
) -> dict[str, Any]:
    if not evidence_refs:
        return {"ok": False, "error": "evidence_required"}
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    now = time.time()
    with lock_cm:
        cur = conn.execute(
            "UPDATE team_task SET status='done', evidence_refs=?, updated_at=? "
            "WHERE task_id=? AND team_id=? AND status='claimed' AND owner=?",
            (
                json.dumps(evidence_refs, ensure_ascii=False),
                now,
                task_id,
                team_id,
                owner,
            ),
        )
        ok = cur.rowcount == 1
        # Chain teardown (§2.2/§4.4): whether the completion landed or was
        # lost, pending dependents of this task cascade to invalidated.
        invalidated = _cascade_invalidate(conn, team_id, [task_id])
        conn.commit()
    return {
        "ok": ok,
        "error": None if ok else "complete_lost",
        "invalidated": invalidated,
    }


def _next_seq(conn: Any, team_id: str) -> int:
    row = conn.execute(
        "SELECT next_seq FROM team_seq WHERE team_id=?", (team_id,)
    ).fetchone()
    if row is None:
        conn.execute(
            "INSERT INTO team_seq (team_id, next_seq) VALUES (?,2)", (team_id,)
        )
        return 1
    seq = int(row[0])
    conn.execute(
        "UPDATE team_seq SET next_seq=? WHERE team_id=?", (seq + 1, team_id)
    )
    return seq


def send_message(
    graph: Any,
    *,
    team_id: str,
    kind: str,
    from_member: str,
    to: list[str],
    body: str = "",
    verbatim: list[str] | None = None,
    evidence_refs: list[dict[str, Any]] | None = None,
    hop: int = 0,
    require_ack: bool = False,
    thread: str | None = None,
    channel_trace_kind: str | None = None,
    ack_of: str | None = None,
) -> dict[str, Any]:
    """T03 typed mailbox write with global team seq (Raft antidote)."""
    if hop > 2:
        return {"ok": False, "error": "hop_exceeded"}
    if kind == "channel":
        # Channel posts use kind=channel; trace subtype in channel_trace_kind.
        if channel_trace_kind and channel_trace_kind not in CHANNEL_KINDS:
            return {"ok": False, "error": "channel_kind_forbidden"}
        if not channel_trace_kind:
            return {"ok": False, "error": "channel_kind_required"}
        to = ["*"]
    elif kind not in MSG_KINDS:
        return {"ok": False, "error": "unknown_kind"}

    refs = evidence_refs or []
    if kind == "ack":
        pass
    elif kind == "channel" and channel_trace_kind in {"evidence", "dead_end"} and not refs:
        return {"ok": False, "error": "evidence_required"}
    elif kind in EVIDENCE_REQUIRED_KINDS and kind != "channel" and not refs:
        return {"ok": False, "error": "evidence_required"}

    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}

    # T07 cost gate (server-side enforcement, §2.1/§4.3/§4.4). Protocol frames
    # (ack/heartbeat) are exempt; past msg_cap or an open circuit breaker only
    # evidence-class traffic passes; channel posts are per-member capped.
    stored_kind = kind if kind != "channel" else f"channel:{channel_trace_kind}"
    if kind not in PROTOCOL_KINDS:
        with lock_cm:
            row = conn.execute(
                "SELECT budget_json FROM team_roster WHERE team_id=?", (team_id,)
            ).fetchone()
            budget = json.loads(row[0]) if row and row[0] else {}
            msg_cap = int(budget.get("msg_cap") or 200)
            used = int(
                conn.execute(
                    "SELECT COUNT(*) FROM team_message WHERE team_id=? "
                    "AND kind NOT IN ('ack','heartbeat')",
                    (team_id,),
                ).fetchone()[0]
            )
            over_cap = used >= msg_cap
            circuit_open = bool(budget.get("circuit_open"))
            if (over_cap or circuit_open) and stored_kind not in CAP_EXEMPT_KINDS:
                conn.commit()
                err = "circuit_open" if circuit_open else "msg_cap_exceeded"
                return {"ok": False, "error": err}
            if kind == "channel":
                per_cap = int(budget.get("channel_per_member_cap") or 12)
                mine = int(
                    conn.execute(
                        "SELECT COUNT(*) FROM team_message WHERE team_id=? "
                        "AND from_member=? AND kind LIKE 'channel:%'",
                        (team_id, from_member),
                    ).fetchone()[0]
                )
                if mine >= per_cap:
                    conn.commit()
                    return {"ok": False, "error": "channel_cap_exceeded"}

    # verbatim hard cap ~500 tokens ≈ 2000 chars per field
    verb = []
    for v in verbatim or []:
        s = str(v)
        if len(s) > 2000:
            # overflow → digest reference (T04)
            dig = hashlib.sha256(s.encode()).hexdigest()[:16]
            verb.append(f"[artifact-ref sha256:{dig} len={len(s)}]")
        else:
            verb.append(s)

    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    msg_id = f"m-{uuid.uuid4().hex[:12]}"
    now = time.time()
    with lock_cm:
        seq = _next_seq(conn, team_id)
        conn.execute(
            "INSERT INTO team_message "
            "(msg_id, team_id, seq, thread, kind, from_member, to_json, "
            " verbatim_json, body, evidence_refs, declared_effects, hop, "
            " require_ack, acked_by, ack_of, ttl_s, created_at) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,'[]',?,?,'[]',?,300,?)",
            (
                msg_id,
                team_id,
                seq,
                thread,
                kind if kind != "channel" else f"channel:{channel_trace_kind}",
                from_member,
                json.dumps(to),
                json.dumps(verb, ensure_ascii=False),
                body[:4000],
                json.dumps(refs, ensure_ascii=False),
                int(hop),
                1 if require_ack else 0,
                ack_of,
                now,
            ),
        )
        conn.commit()

    event_kind = "team_channel_posted" if kind == "channel" else "team_msg_sent"
    _append(
        graph,
        event_kind,
        {
            "team_id": team_id,
            "msg_id": msg_id,
            "seq": seq,
            "kind": kind,
            "channel_trace_kind": channel_trace_kind,
            "from": from_member,
            "to": to,
            "hop": hop,
        },
    )
    return {"ok": True, "msg_id": msg_id, "seq": seq}


def ack_message(
    graph: Any, *, team_id: str, msg_id: str, by_member: str
) -> dict[str, Any]:
    return send_message(
        graph,
        team_id=team_id,
        kind="ack",
        from_member=by_member,
        to=[],
        ack_of=msg_id,
    )


def list_messages(
    graph: Any, *, team_id: str, after_seq: int = 0, limit: int = 50
) -> list[dict[str, Any]]:
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return []
    with lock_cm:
        rows = conn.execute(
            "SELECT msg_id, seq, kind, from_member, to_json, body, hop, require_ack "
            "FROM team_message WHERE team_id=? AND seq>? ORDER BY seq ASC LIMIT ?",
            (team_id, after_seq, limit),
        ).fetchall()
    out = []
    for r in rows:
        out.append(
            {
                "msg_id": r[0],
                "seq": int(r[1]),
                "kind": r[2],
                "from": r[3],
                "to": json.loads(r[4] or "[]"),
                "body": r[5],
                "hop": int(r[6]),
                "require_ack": bool(r[7]),
            }
        )
    return out


def grant_token(
    graph: Any,
    *,
    team_id: str,
    protocol: str,
    holder: str | None,
    lease_s: float = 120.0,
) -> dict[str, Any]:
    """T09: issue turn token with monotonic fence."""
    conn, lock_cm = _conn_lock(graph)
    token_id = f"tt-{uuid.uuid4().hex[:8]}"
    now = time.time()
    fence = 1
    if conn is not None:
        with lock_cm:
            row = conn.execute(
                "SELECT MAX(fence) FROM team_turn_token WHERE team_id=? AND protocol=?",
                (team_id, protocol),
            ).fetchone()
            if row and row[0] is not None:
                fence = int(row[0]) + 1
            conn.execute(
                "INSERT INTO team_turn_token "
                "(token_id, team_id, protocol, holder, fence, lease_expires_at, "
                " on_expire, status) VALUES (?,?,?,?,?,?,'release+notify-lead','active')",
                (
                    token_id,
                    team_id,
                    protocol,
                    holder,
                    fence,
                    now + lease_s if holder else None,
                ),
            )
            conn.commit()
    payload = {
        "token_id": token_id,
        "team_id": team_id,
        "protocol": protocol,
        "holder": holder,
        "fence": fence,
    }
    _append(graph, "team_token_granted", payload)
    return payload


def pass_token(
    graph: Any,
    *,
    team_id: str,
    token_id: str,
    from_holder: str,
    to_holder: str,
    fence: int,
) -> dict[str, Any]:
    """Server-enforced token handoff (must hold current fence)."""
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    now = time.time()
    with lock_cm:
        row = conn.execute(
            "SELECT holder, fence, status FROM team_turn_token WHERE token_id=? AND team_id=?",
            (token_id, team_id),
        ).fetchone()
        if not row:
            return {"ok": False, "error": "missing"}
        holder, cur_fence, status = row
        if status != "active" or holder != from_holder or int(cur_fence) != int(fence):
            return {"ok": False, "error": "enforcement_reject"}
        new_fence = int(cur_fence) + 1
        conn.execute(
            "UPDATE team_turn_token SET holder=?, fence=?, lease_expires_at=? "
            "WHERE token_id=?",
            (to_holder, new_fence, now + 120.0, token_id),
        )
        conn.commit()
    _append(
        graph,
        "team_token_granted",
        {
            "token_id": token_id,
            "team_id": team_id,
            "holder": to_holder,
            "fence": new_fence,
            "passed_from": from_holder,
        },
    )
    return {"ok": True, "fence": new_fence, "holder": to_holder}


def write_assertion(
    graph: Any,
    *,
    team_id: str,
    text: str,
    evidence_refs: list[dict[str, Any]],
    confidence: float = 0.7,
    source_seqs: list[int] | None = None,
) -> dict[str, Any]:
    if not evidence_refs:
        return {"ok": False, "error": "evidence_required"}
    if len(text) > 800:
        text = text[:800]
    conn, lock_cm = _conn_lock(graph)
    aid = f"as-{uuid.uuid4().hex[:10]}"
    now = time.time()
    if conn is not None:
        with lock_cm:
            conn.execute(
                "INSERT INTO team_assertion "
                "(assertion_id, team_id, text, evidence_refs, confidence, status, "
                " source_seqs, created_at) VALUES (?,?,?,?,?,'active',?,?)",
                (
                    aid,
                    team_id,
                    text,
                    json.dumps(evidence_refs, ensure_ascii=False),
                    confidence,
                    json.dumps(source_seqs or []),
                    now,
                ),
            )
            conn.commit()
    _append(
        graph,
        "team_assertion_written",
        {"team_id": team_id, "assertion_id": aid, "text": text[:200]},
    )
    return {"ok": True, "assertion_id": aid}


def channel_messages(
    graph: Any, *, team_id: str, after_seq: int = 0, limit: int = 100
) -> list[dict[str, Any]]:
    """Channel posts (kind='channel:*') after ``after_seq``, oldest first."""
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return []
    with lock_cm:
        rows = conn.execute(
            "SELECT seq, kind, from_member, body, evidence_refs FROM team_message "
            "WHERE team_id=? AND kind LIKE 'channel:%' AND seq>? "
            "ORDER BY seq ASC LIMIT ?",
            (team_id, int(after_seq), int(limit)),
        ).fetchall()
    return [
        {
            "seq": int(r[0]),
            "kind": r[1],
            "from": r[2],
            "body": r[3] or "",
            "evidence_refs": json.loads(r[4] or "[]"),
        }
        for r in rows
    ]


def latest_channel_digest(graph: Any, *, team_id: str) -> dict[str, Any] | None:
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return None
    with lock_cm:
        row = conn.execute(
            "SELECT digest_id, span_lo, span_hi, distilled, generated_at, by_model "
            "FROM team_channel_digest WHERE team_id=? "
            "ORDER BY generated_at DESC LIMIT 1",
            (team_id,),
        ).fetchone()
    if not row:
        return None
    return {
        "digest_id": row[0],
        "span": [int(row[1]), int(row[2])],
        "distilled": row[3],
        "generated_at": float(row[4]),
        "by_model": row[5],
    }


def distill_channel_digest(
    graph: Any,
    *,
    team_id: str,
    distilled_text: str | None = None,
    distiller: Any = None,
    by_model: str | None = None,
) -> dict[str, Any] | None:
    """T13: pull-style digest (no full-text injection).

    Distillation source priority: explicit ``distilled_text`` (caller already ran
    grok-low) → ``distiller`` callable over the raw trace → deterministic join
    fallback. ``by_model`` is recorded for audit; it follows the source actually
    used, never claimed otherwise.
    """
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return None
    with lock_cm:
        rows = conn.execute(
            "SELECT seq, kind, from_member, body FROM team_message "
            "WHERE team_id=? AND kind LIKE 'channel:%' ORDER BY seq ASC",
            (team_id,),
        ).fetchall()
        if not rows:
            return None
        lo = int(rows[0][0])
        hi = int(rows[-1][0])
        raw_lines = [f"[{r[0]}] {r[2]} {r[1]}: {(r[3] or '')[:80]}" for r in rows[-15:]]
        raw = " | ".join(raw_lines)
        if distilled_text:
            distilled = str(distilled_text)[:1200]
            model = by_model or "grok-4.5-low"
        elif callable(distiller):
            try:
                distilled = str(distiller(raw))[:1200]
                model = by_model or "grok-4.5-low"
            except Exception:
                distilled = raw[:1200]
                model = "deterministic"
        else:
            distilled = raw[:1200]
            model = "deterministic"
        digest_id = f"dg-{uuid.uuid4().hex[:8]}"
        now = time.time()
        conn.execute(
            "INSERT INTO team_channel_digest "
            "(digest_id, team_id, span_lo, span_hi, distilled, assertion_candidates, "
            " role_relevance, generated_at, by_model) "
            "VALUES (?,?,?,?,?,'[]','{}',?,?)",
            (digest_id, team_id, lo, hi, distilled, now, model),
        )
        conn.commit()
    return {
        "digest_id": digest_id,
        "span": [lo, hi],
        "distilled": distilled,
        "by_model": model,
    }


def role_hat_guidance(role: str, *, member_name: str, team_id: str) -> str:
    """T02 prompt hats — functional heterogeneity on isomorphic cursor workers."""
    cli = (
        "Coordinate ONLY via `blackboard.py --mode=teammate` team subcommands: "
        "msg-check (mailbox+digest), msg-send, task-list, task-claim, task-done, "
        "assert-write, artifact-put, token-wait, heartbeat. "
        "Full-board reads (read-facts/read-routes/...) are not registered in "
        "teammate mode. Send `heartbeat` ~every 30s while working."
    )
    hats = {
        "recon": (
            f"[f11-agent-teams] You are {member_name} (recon) on {team_id}. "
            "Map surface; write verified facts with evidence. "
            "Post surprises to channel as --channel-kind=surprise. " + cli
        ),
        "exploit": (
            f"[f11-agent-teams] You are {member_name} (exploit) on {team_id}. "
            "Claim tasks with declared_effects; execute tool-backed probes. "
            "Handoff to verify with evidence_refs; hold turn token for chain steps. "
            + cli
        ),
        "verify": (
            f"[f11-agent-teams] You are {member_name} (verify) on {team_id}. "
            "Adversarial check: try to falsify teammate claims. "
            "Reject assertions without evidence_refs; emit contradiction when "
            "needed. " + cli
        ),
    }
    return hats.get(
        role,
        f"[f11-agent-teams] You are {member_name} on {team_id}. Follow task list + mailbox. "
        + cli,
    )


def _token_state(graph: Any, token_id: str) -> tuple[str | None, int]:
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return None, 0
    with lock_cm:
        row = conn.execute(
            "SELECT holder, fence FROM team_turn_token WHERE token_id=?",
            (token_id,),
        ).fetchone()
    if not row:
        return None, 0
    return str(row[0]), int(row[1])


def counting_protocol(
    graph: Any,
    *,
    team_id: str,
    members: list[str],
    rounds: int = 30,
) -> dict[str, Any]:
    """Gate-0: N teammates count strictly via turn token; assert zero reorder/loss."""
    if len(members) < 2:
        return {"ok": False, "error": "need≥2"}
    token = grant_token(
        graph, team_id=team_id, protocol="gate0-count", holder=members[0]
    )
    tid = token["token_id"]
    fence = int(token["fence"])
    holder = members[0]
    errors: list[str] = []
    acks = 0
    sends = 0
    total_steps = rounds * len(members)
    for expected in range(1, total_steps + 1):
        cur_holder, fence = _token_state(graph, tid)
        if cur_holder is None:
            errors.append("token_lost")
            break
        holder = cur_holder
        # Non-holder pass must fail.
        impostor = next(m for m in members if m != holder)
        bad = pass_token(
            graph,
            team_id=team_id,
            token_id=tid,
            from_holder=impostor,
            to_holder=holder,
            fence=fence,
        )
        if bad.get("ok"):
            errors.append(f"non_holder_pass_accepted:{impostor}")
            break
        idx = members.index(holder)
        nxt = members[(idx + 1) % len(members)]
        msg = send_message(
            graph,
            team_id=team_id,
            kind="direct",
            from_member=holder,
            to=[nxt],
            body=str(expected),
            require_ack=True,
            thread="gate0-count",
        )
        sends += 1
        if not msg.get("ok"):
            errors.append(f"send_fail:{msg}")
            break
        ack = ack_message(
            graph, team_id=team_id, msg_id=str(msg["msg_id"]), by_member=nxt
        )
        if ack.get("ok"):
            acks += 1
        else:
            errors.append("ack_fail")
            break
        passed = pass_token(
            graph,
            team_id=team_id,
            token_id=tid,
            from_holder=holder,
            to_holder=nxt,
            fence=fence,
        )
        if not passed.get("ok"):
            errors.append(f"pass_fail:{passed}")
            break
        fence = int(passed["fence"])
        holder = nxt

    msgs = list_messages(graph, team_id=team_id, after_seq=0, limit=10000)
    digit_msgs = [
        m
        for m in msgs
        if str(m.get("body") or "").isdigit() and m.get("kind") == "direct"
    ]
    bodies = [int(m["body"]) for m in digit_msgs]
    seqs = [int(m["seq"]) for m in digit_msgs]
    order_ok = bodies == list(range(1, len(bodies) + 1))
    seq_mono = seqs == sorted(seqs) and len(seqs) == len(set(seqs))
    stale = pass_token(
        graph,
        team_id=team_id,
        token_id=tid,
        from_holder=members[0],
        to_holder=members[1],
        fence=0,
    )
    if stale.get("ok"):
        errors.append("stale_fence_accepted")
    ack_rate = (acks / sends) if sends else 0.0
    return {
        "ok": bool(order_ok and seq_mono and not errors and ack_rate == 1.0),
        "order_ok": order_ok,
        "seq_mono": seq_mono,
        "counted": len(bodies),
        "errors": errors,
        "stale_rejected": not bool(stale.get("ok")),
        "ack_rate": ack_rate,
    }


# ---------------------------------------------------------------------------
# T05 lead budget gate (real glm calls, ≤12/题 + cooldown, server-enforced)
# ---------------------------------------------------------------------------


def _roster_row(conn: Any, team_id: str) -> tuple | None:
    return conn.execute(
        "SELECT lead_calls_used, lead_calls_cap, lead_cooldown_s, "
        "last_lead_call_at, members_json, budget_json, lead_model, status "
        "FROM team_roster WHERE team_id=?",
        (team_id,),
    ).fetchone()


def try_lead_call(
    graph: Any, *, team_id: str, purpose: str, now: float | None = None
) -> dict[str, Any]:
    """T05: atomic budget+cooldown gate for a lead (glm) call.

    Increments lead_calls_used inside the same single-writer transaction, so
    concurrent triggers cannot overrun the ≤12 cap. Cooldown (default 45s) is
    enforced against last_lead_call_at. Returns {"ok": False} without consuming
    budget when the cap is hit or the cooldown has not elapsed.
    """
    if purpose not in LEAD_PURPOSES:
        return {"ok": False, "error": "unknown_purpose"}
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    ts = time.time() if now is None else float(now)
    with lock_cm:
        row = _roster_row(conn, team_id)
        if not row:
            return {"ok": False, "error": "no_roster"}
        used, cap, cooldown, last_at = int(row[0]), int(row[1]), int(row[2]), row[3]
        if used >= cap:
            conn.commit()
            return {"ok": False, "error": "lead_cap_exceeded", "used": used, "cap": cap}
        if last_at is not None and ts - float(last_at) < cooldown:
            conn.commit()
            return {
                "ok": False,
                "error": "lead_cooldown",
                "retry_after_s": round(cooldown - (ts - float(last_at)), 1),
            }
        conn.execute(
            "UPDATE team_roster SET lead_calls_used=?, last_lead_call_at=? "
            "WHERE team_id=?",
            (used + 1, ts, team_id),
        )
        conn.commit()
    _append(
        graph,
        "team_lead_call",
        {"team_id": team_id, "purpose": purpose, "calls_used": used + 1},
    )
    return {"ok": True, "calls_used": used + 1, "cap": cap}


def lead_status(graph: Any, *, team_id: str) -> dict[str, Any]:
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {}
    with lock_cm:
        row = _roster_row(conn, team_id)
    if not row:
        return {}
    return {
        "calls_used": int(row[0]),
        "calls_cap": int(row[1]),
        "cooldown_s": int(row[2]),
        "last_lead_call_at": row[3],
        "lead_model": row[6],
        "status": row[7],
    }


# ---------------------------------------------------------------------------
# T07 cost ledger + +30% circuit breaker (§2.1/§4.4)
# ---------------------------------------------------------------------------


def get_budget(graph: Any, *, team_id: str) -> dict[str, Any]:
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {}
    with lock_cm:
        row = conn.execute(
            "SELECT budget_json FROM team_roster WHERE team_id=?", (team_id,)
        ).fetchone()
    return json.loads(row[0]) if row and row[0] else {}


def update_budget(graph: Any, *, team_id: str, **fields: Any) -> dict[str, Any]:
    """Merge fields into the roster budget_json (msg_cap, baseline_cost_usd, …)."""
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {}
    with lock_cm:
        row = conn.execute(
            "SELECT budget_json FROM team_roster WHERE team_id=?", (team_id,)
        ).fetchone()
        budget = json.loads(row[0]) if row and row[0] else {}
        budget.update(fields)
        conn.execute(
            "UPDATE team_roster SET budget_json=? WHERE team_id=?",
            (json.dumps(budget), team_id),
        )
        conn.commit()
    return budget


def record_cost(graph: Any, *, team_id: str, cost_usd: float) -> dict[str, Any]:
    """Accumulate teammate/lead spend; trip the +30% circuit breaker (§4.4).

    Breaker semantics: once a baseline for the old arm is known
    (``baseline_cost_usd``), added spend beyond baseline*1.3 freezes new
    messages/tasks — only evidence/flag-class traffic passes (enforced in
    send_message / create_task).
    """
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    tripped = False
    with lock_cm:
        row = conn.execute(
            "SELECT budget_json FROM team_roster WHERE team_id=?", (team_id,)
        ).fetchone()
        budget = json.loads(row[0]) if row and row[0] else {}
        budget["cost_usd"] = float(budget.get("cost_usd") or 0.0) + float(cost_usd or 0.0)
        baseline = budget.get("baseline_cost_usd")
        if (
            baseline
            and not budget.get("circuit_open")
            and budget["cost_usd"] > float(baseline) * 1.3
        ):
            budget["circuit_open"] = True
            tripped = True
        conn.execute(
            "UPDATE team_roster SET budget_json=? WHERE team_id=?",
            (json.dumps(budget), team_id),
        )
        conn.commit()
    if tripped:
        _append(
            graph,
            "team_circuit_open",
            {
                "team_id": team_id,
                "cost_usd": budget["cost_usd"],
                "baseline_cost_usd": baseline,
            },
        )
    return {"ok": True, "circuit_open": bool(budget.get("circuit_open")), "budget": budget}


# ---------------------------------------------------------------------------
# T12 teammate health: heartbeat / stalled / dead / same-role replacement
# ---------------------------------------------------------------------------


def _load_members(conn: Any, team_id: str) -> list[dict[str, Any]]:
    row = conn.execute(
        "SELECT members_json FROM team_roster WHERE team_id=?", (team_id,)
    ).fetchone()
    if not row or not row[0]:
        return []
    try:
        members = json.loads(row[0])
    except Exception:
        return []
    return members if isinstance(members, list) else []


def _store_members(conn: Any, team_id: str, members: list[dict[str, Any]]) -> None:
    conn.execute(
        "UPDATE team_roster SET members_json=? WHERE team_id=?",
        (json.dumps(members, ensure_ascii=False), team_id),
    )


def heartbeat(
    graph: Any,
    *,
    team_id: str,
    member: str,
    now: float | None = None,
    state: str = "active",
) -> dict[str, Any]:
    """T12: liveness write from the teammate itself (protocol frame, no msg_cap).

    This is a roster bookkeeping update, NOT a mailbox message — the coordinator
    never sends heartbeat messages on a member's behalf.
    """
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    ts = time.time() if now is None else float(now)
    with lock_cm:
        members = _load_members(conn, team_id)
        found = False
        for m in members:
            if m.get("name") == member:
                m["last_heartbeat"] = ts
                if m.get("state") in ("idle", "stalled"):
                    m["state"] = state
                found = True
                break
        if not found:
            conn.commit()
            return {"ok": False, "error": "unknown_member"}
        _store_members(conn, team_id, members)
        conn.commit()
    return {"ok": True, "member": member, "at": ts}


def release_member_tasks(
    graph: Any, *, team_id: str, member: str
) -> list[str]:
    """T11/T12: release tasks claimed by a dead member back to pending."""
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return []
    now = time.time()
    with lock_cm:
        rows = conn.execute(
            "SELECT task_id FROM team_task WHERE team_id=? AND owner=? "
            "AND status='claimed'",
            (team_id, member),
        ).fetchall()
        ids = [str(r[0]) for r in rows]
        for tid in ids:
            conn.execute(
                "UPDATE team_task SET status='pending', owner=NULL, lease_json=NULL, "
                "fence=fence+1, updated_at=? WHERE task_id=? AND team_id=?",
                (now, tid, team_id),
            )
        conn.commit()
    return ids


def check_health(
    graph: Any,
    *,
    team_id: str,
    now: float | None = None,
    stalled_after_s: float = 30.0,
    dead_after_s: float = 120.0,
    lease_s: float = 300.0,
) -> dict[str, Any]:
    """T12: classify members by heartbeat age; release tasks of the dead.

    active/idle → stalled (no heartbeat for stalled_after_s) → dead
    (dead_after_s). Dead members' claimed tasks are released for re-claim.
    Also sweeps T11 lease timeouts: tasks claimed longer than ``lease_s`` by
    any member fall back to pending.
    """
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    ts = time.time() if now is None else float(now)
    stalled: list[str] = []
    dead: list[str] = []
    released: dict[str, list[str]] = {}
    lease_expired: list[str] = []
    with lock_cm:
        members = _load_members(conn, team_id)
        changed = False
        for m in members:
            state = str(m.get("state") or "active")
            if state == "dead":
                continue
            last = float(m.get("last_heartbeat") or 0.0)
            age = ts - last
            name = str(m.get("name") or "")
            if age >= dead_after_s:
                m["state"] = "dead"
                dead.append(name)
                changed = True
            elif age >= stalled_after_s and state != "stalled":
                m["state"] = "stalled"
                stalled.append(name)
                changed = True
        if changed:
            _store_members(conn, team_id, members)
        # T11 lease timeout sweep (any owner, dead or alive).
        rows = conn.execute(
            "SELECT task_id, lease_json FROM team_task WHERE team_id=? "
            "AND status='claimed'",
            (team_id,),
        ).fetchall()
        for tid, lease_json in rows:
            try:
                claimed_at = float(json.loads(lease_json or "{}").get("claimed_at") or ts)
            except Exception:
                claimed_at = ts
            if ts - claimed_at >= lease_s:
                conn.execute(
                    "UPDATE team_task SET status='pending', owner=NULL, "
                    "lease_json=NULL, fence=fence+1, updated_at=? "
                    "WHERE task_id=? AND team_id=?",
                    (ts, tid, team_id),
                )
                lease_expired.append(str(tid))
        conn.commit()
    for name in stalled:
        _append(graph, "team_member_stalled", {"team_id": team_id, "member": name})
    for name in dead:
        released[name] = release_member_tasks(graph, team_id=team_id, member=name)
        _append(
            graph,
            "team_member_dead",
            {"team_id": team_id, "member": name, "released_tasks": released[name]},
        )
    return {
        "ok": True,
        "stalled": stalled,
        "dead": dead,
        "released": released,
        "lease_expired": lease_expired,
    }


def member_context_brief(
    graph: Any, *, team_id: str, role: str, limit: int = 10
) -> str:
    """Q_fail/§2.4: rebuild context for a replacement instance from team state —
    open tasks for the role + active assertions + latest channel digest.
    References only; no full-board dump.
    """
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return ""
    with lock_cm:
        tasks = conn.execute(
            "SELECT task_id, goal, status FROM team_task WHERE team_id=? "
            "AND status IN ('pending','claimed') ORDER BY created_at ASC LIMIT ?",
            (team_id, limit),
        ).fetchall()
        assertions = conn.execute(
            "SELECT text, evidence_refs FROM team_assertion WHERE team_id=? "
            "AND status='active' ORDER BY created_at DESC LIMIT ?",
            (team_id, limit),
        ).fetchall()
    lines = [f"[f11 context rebuild] role={role} team={team_id}"]
    if tasks:
        lines.append("open tasks:")
        for tid, goal, status in tasks:
            lines.append(f"- {tid} ({status}): {str(goal)[:160]}")
    if assertions:
        lines.append("active assertions (evidence-backed):")
        for text, refs in assertions:
            lines.append(f"- {str(text)[:160]} refs={str(refs)[:120]}")
    digest = latest_channel_digest(graph, team_id=team_id)
    if digest:
        lines.append(f"channel digest {digest['span']}: {digest['distilled'][:300]}")
    return "\n".join(lines)


def replace_member(
    graph: Any,
    *,
    team_id: str,
    dead_member: str,
    worker_id: str = "",
    session_id: str = "",
    now: float | None = None,
) -> dict[str, Any]:
    """T12: same-role replacement for a dead instance (Q_fail).

    The new instance gets the next index for its role (recon-2, …), inherits
    nothing in-process — context is rebuilt from team state (tasks + assertions
    + digest), which is returned as ``context_brief`` for the spawn guidance.
    """
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    ts = time.time() if now is None else float(now)
    with lock_cm:
        members = _load_members(conn, team_id)
        dead = next((m for m in members if m.get("name") == dead_member), None)
        if dead is None:
            conn.commit()
            return {"ok": False, "error": "unknown_member"}
        if dead.get("state") != "dead":
            conn.commit()
            return {"ok": False, "error": "member_not_dead"}
        role = str(dead.get("role") or "recon")
        idx = 1 + max(
            [int(str(m.get("name") or "").rsplit("-", 1)[-1])
             for m in members
             if str(m.get("name") or "").rsplit("-", 1)[-1].isdigit()]
            or [0]
        )
        new_name = f"{role}-{idx}"
        spawn_seq = max([int(m.get("spawn_seq") or 0) for m in members] or [0]) + 1
        new_member = {
            "name": new_name,
            "role": role,
            "worker_id": worker_id,
            "session_id": session_id,
            "state": "active",
            "last_heartbeat": ts,
            "spawn_seq": spawn_seq,
            "replaces": dead_member,
        }
        members.append(new_member)
        _store_members(conn, team_id, members)
        conn.commit()
    brief = member_context_brief(graph, team_id=team_id, role=role)
    _append(
        graph,
        "team_member_replaced",
        {
            "team_id": team_id,
            "dead": dead_member,
            "member": new_name,
            "role": role,
            "spawn_seq": spawn_seq,
        },
    )
    return {
        "ok": True,
        "member": new_member,
        "role": role,
        "context_brief": brief,
    }


def bind_member_worker(
    graph: Any,
    *,
    team_id: str,
    member: str,
    worker_id: str = "",
    session_id: str = "",
) -> dict[str, Any]:
    """T01: bind a durable teammate seat to its physical worker/session."""
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return {"ok": False, "error": "no_conn"}
    with lock_cm:
        members = _load_members(conn, team_id)
        for m in members:
            if m.get("name") == member:
                if worker_id:
                    m["worker_id"] = worker_id
                if session_id:
                    m["session_id"] = session_id
                _store_members(conn, team_id, members)
                conn.commit()
                return {"ok": True, "member": member}
        conn.commit()
    return {"ok": False, "error": "unknown_member"}


def list_tasks(
    graph: Any, *, team_id: str, status: str | None = None, limit: int = 50
) -> list[dict[str, Any]]:
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return []
    with lock_cm:
        if status:
            rows = conn.execute(
                "SELECT task_id, goal, status, owner, declared_effects, depends_on "
                "FROM team_task WHERE team_id=? AND status=? "
                "ORDER BY created_at ASC LIMIT ?",
                (team_id, status, limit),
            ).fetchall()
        else:
            rows = conn.execute(
                "SELECT task_id, goal, status, owner, declared_effects, depends_on "
                "FROM team_task WHERE team_id=? ORDER BY created_at ASC LIMIT ?",
                (team_id, limit),
            ).fetchall()
    return [
        {
            "task_id": r[0],
            "goal": r[1],
            "status": r[2],
            "owner": r[3],
            "declared_effects": json.loads(r[4] or "[]"),
            "depends_on": json.loads(r[5] or "[]"),
        }
        for r in rows
    ]


def token_status(graph: Any, *, team_id: str, token_id: str) -> dict[str, Any] | None:
    conn, lock_cm = _conn_lock(graph)
    if conn is None:
        return None
    with lock_cm:
        row = conn.execute(
            "SELECT holder, fence, lease_expires_at, status, protocol "
            "FROM team_turn_token WHERE token_id=? AND team_id=?",
            (token_id, team_id),
        ).fetchone()
    if not row:
        return None
    return {
        "holder": row[0],
        "fence": int(row[1]),
        "lease_expires_at": row[2],
        "status": row[3],
        "protocol": row[4],
    }
