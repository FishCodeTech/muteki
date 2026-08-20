#!/usr/bin/env python3
"""muteki-blackboard — a worker's CLI to the shared solve graph (the blackboard).

A swarm worker (claude / codex) calls this to coordinate with its teammates
through the shared, append-only SQLite blackboard — NOT by talking to them
directly (stigmergy). The board holds:
  - facts      : confirmed, objective findings (with verified/candidate status)
  - dead-ends  : ruled-out directions (so nobody retries them)
  - intents    : declared exploration directions, claimable atomically

The DB path comes from $MUTEKI_BLACKBOARD_DB (the coordinator sets it per worker).

Usage:
  blackboard.py read-facts [--verified-only]   # what teammates confirmed
  blackboard.py read-review                    # review-arbiter challenges/directives
  blackboard.py read-routes                    # suppressed/reopened routes
  blackboard.py read-branches                  # branch hypotheses to split/verify
  blackboard.py read-deadends                  # paths already ruled out — AVOID
  blackboard.py read-flags                     # flags already found (multi-flag) — don't re-hunt
  blackboard.py list-intents                   # open directions you can claim
  blackboard.py write-fact "<text>" [--verified]
  blackboard.py mark-deadend "<reason>"
  blackboard.py submit-flag '<flag>'             # the only Flag submission API
  blackboard.py claim <intent_id>              # atomic; prints WON or LOST

This script is intentionally dependency-free (stdlib sqlite3 only) so it runs in
any worker container without setup.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sqlite3
import sys
import tempfile
import time
import uuid

_ACTOR = os.environ.get("MUTEKI_WORKER_ID", "worker")
_INTENT_ID = os.environ.get("MUTEKI_INTENT_ID", "").strip()


def _db_path() -> str:
    p = os.environ.get("MUTEKI_BLACKBOARD_DB", "")
    if not p:
        # fallback: a path file dropped in cwd by the coordinator
        for cand in (".muteki_blackboard", "shared_graph.db"):
            if os.path.isfile(cand):
                return cand
        print("ERROR: no blackboard DB ($MUTEKI_BLACKBOARD_DB unset and no "
              "shared_graph.db in cwd)", file=sys.stderr)
        sys.exit(2)
    return p


def _conn() -> sqlite3.Connection:
    c = sqlite3.connect(_db_path(), timeout=10)
    c.execute("PRAGMA busy_timeout=5000")
    return c


def _has_column(c: sqlite3.Connection, table: str, col: str) -> bool:
    try:
        cols = {row[1] for row in c.execute(f"PRAGMA table_info({table})").fetchall()}
    except Exception:
        return False
    return col in cols


def _has_table(c: sqlite3.Connection, table: str) -> bool:
    try:
        row = c.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (table,)
        ).fetchone()
    except Exception:
        return False
    return row is not None


def _retired_fact_seqs(c: sqlite3.Connection) -> set:
    """fact_seqs in a terminal lifecycle state (rejected/merged/superseded) — these
    must NOT be shown to workers as evidence. Empty on an old DB without fact_states."""
    if not _has_table(c, "fact_states"):
        return set()
    try:
        rows = c.execute(
            "SELECT fact_seq FROM fact_states "
            "WHERE state IN ('rejected','merged','superseded') OR retired_seq IS NOT NULL"
        ).fetchall()
    except Exception:
        return set()
    return {int(r[0]) for r in rows}


def _challenge_id(c: sqlite3.Connection) -> str:
    # Pick the first NON-EMPTY challenge_id. Some events are written with an empty
    # challenge_id, and a bare `LIMIT 1` could grab one of those — then claim's
    # `WHERE challenge_id=?` matched nothing and always returned LOST even for an
    # open intent. Fall back to the intents table (those rows reliably carry the run
    # id), then to "" as a last resort.
    row = c.execute(
        "SELECT challenge_id FROM events "
        "WHERE challenge_id IS NOT NULL AND challenge_id != '' LIMIT 1"
    ).fetchone()
    if row and row[0]:
        return row[0]
    row = c.execute(
        "SELECT challenge_id FROM intents "
        "WHERE challenge_id IS NOT NULL AND challenge_id != '' LIMIT 1"
    ).fetchone()
    return row[0] if row and row[0] else ""


def read_facts(verified_only: bool) -> None:
    c = _conn()
    retired = _retired_fact_seqs(c)
    q = ("SELECT seq, payload, verified, confidence FROM events "
         "WHERE kind='fact_added' ORDER BY seq")
    out = []
    for seq, payload, verified, conf in c.execute(q).fetchall():
        if int(seq) in retired:
            continue  # rejected/merged/superseded by review — not evidence
        if verified_only and not verified:
            continue
        d = json.loads(payload)
        out.append({"fact": d.get("fact", ""), "source": d.get("source", ""),
                    "verified": bool(verified), "confidence": conf})
    if not out:
        print("(no facts on the board yet)")
        return
    for f in out:
        tag = "VERIFIED" if f["verified"] else f"candidate({f['confidence']:.1f})"
        print(f"[{tag}] ({f['source']}) {f['fact']}")


def read_flags() -> None:
    """Flags teammates have already recovered. On a MULTI-FLAG challenge, read
    this before submitting so you don't re-hunt one a teammate already found —
    go after the ones NOT listed here."""
    c = _conn()
    rows = c.execute(
        "SELECT payload, kind FROM events "
        "WHERE kind IN ('flag_found','flag_invalidated') ORDER BY seq").fetchall()
    found: list[str] = []
    for payload, kind in rows:
        f = (json.loads(payload) or {}).get("flag")
        if not f:
            continue
        if kind == "flag_found" and f not in found:
            found.append(f)
        elif kind == "flag_invalidated" and f in found:
            found.remove(f)  # a false positive was retracted
    if not found:
        print("(no flags recovered yet — you may be the first)")
        return
    print("# Flags already recovered by the team — do NOT re-submit these:")
    for f in found:
        print(f"- {f}")


def submit_flag(flag: str) -> None:
    """Submit one Flag candidate to the owning Worker for provenance validation.

    This command never writes ``flag_found`` or the shared SQLite database. The
    CliSolver that owns ``_ACTOR`` imports the atomic request through the host DB
    owner, validates it against command output captured before this API call, then
    appends a decision and publishes ``flag_found`` only when the gate accepts it.
    """

    value = str(flag or "").strip()
    if not value or len(value) > 1024 or any(ord(ch) < 32 for ch in value):
        print("ERROR: flag must be one non-empty line (maximum 1024 characters)",
              file=sys.stderr)
        sys.exit(2)
    submission_id = f"fs-{uuid.uuid4().hex[:16]}"
    request_dir = os.environ.get("MUTEKI_FLAG_SUBMISSION_DIR", "").strip()
    if not request_dir:
        print("ERROR: the owning Worker did not provide a Flag submission ingress",
              file=sys.stderr)
        sys.exit(2)
    os.makedirs(request_dir, mode=0o700, exist_ok=True)
    request = {
        "submission_id": submission_id,
        "flag": value,
        "intent_id": _INTENT_ID,
        "actor": _ACTOR,
        "protocol": "blackboard-api-v1",
        "created_at": time.time(),
    }
    # Container and host must not write the same SQLite WAL.  Publish one complete
    # request through an atomic rename; the owning host CliSolver is the only DB
    # writer and the only component allowed to validate provenance.
    fd, temporary = tempfile.mkstemp(
        prefix=f".{submission_id}-", suffix=".tmp", dir=request_dir)
    final_path = os.path.join(request_dir, f"{submission_id}.json")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(request, handle, ensure_ascii=False, separators=(",", ":"))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, final_path)
    except BaseException:
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise
    # Do not echo the candidate. Tool output from the submission call must never
    # become evidence for its own payload.
    print(f"SUBMITTED {submission_id}; awaiting provenance validation")


def read_deadends() -> None:
    c = _conn()
    rows = c.execute(
        "SELECT payload FROM events WHERE kind='dead_end' ORDER BY seq").fetchall()
    if not rows:
        print("(no dead-ends recorded — nothing ruled out yet)")
        return
    print("# Dead-ends — directions already ruled out, DO NOT retry these:")
    for (payload,) in rows:
        d = json.loads(payload)
        print(f"- {d.get('reason', '')}")


def _table_exists(c: sqlite3.Connection, table: str) -> bool:
    row = c.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        (table,),
    ).fetchone()
    return bool(row)


def _event_payload_by_seq(c: sqlite3.Connection, seq: int) -> dict:
    row = c.execute("SELECT payload FROM events WHERE seq=?", (int(seq),)).fetchone()
    if not row:
        return {}
    try:
        return json.loads(row[0]) or {}
    except Exception:
        return {}


def read_routes() -> None:
    c = _conn()
    if not _table_exists(c, "routes"):
        print("(this board has no route review table yet)")
        return
    rows = c.execute(
        "SELECT route_hash, label, status, reason, until_policy "
        "FROM routes ORDER BY COALESCE(suppressed_seq, reopened_seq, 0), route_hash"
    ).fetchall()
    if not rows:
        print("(no reviewed routes)")
        return
    print("# Reviewed routes")
    for route_hash, label, status, reason, until_policy in rows:
        tag = "SUPPRESSED" if status == "suppressed" else "OPEN"
        extra = f" until={until_policy}" if until_policy else ""
        print(f"[{tag}] {route_hash} ({label or route_hash}){extra}: {reason or ''}")


def read_branches() -> None:
    c = _conn()
    if not _table_exists(c, "branches"):
        print("(this board has no branch review table yet)")
        return
    rows = c.execute(
        "SELECT branch_id, parent_id, title, assumption, prove_or_disprove, status "
        "FROM branches ORDER BY created_seq, branch_id"
    ).fetchall()
    if not rows:
        print("(no branch hypotheses)")
        return
    print("# Review branches — prove/disprove separately")
    for branch_id, parent_id, title, assumption, pod, status in rows:
        parent = f" parent={parent_id}" if parent_id else ""
        print(f"- [{status or 'open'}] {branch_id}{parent}: {title or assumption}")
        if assumption:
            print(f"  assumption: {assumption}")
        if pod:
            print(f"  prove/disprove: {pod}")


def read_review() -> None:
    c = _conn()
    print("# Review-Arbiter state")

    rows = c.execute(
        "SELECT seq, actor, payload FROM events "
        "WHERE kind='review_finding' ORDER BY seq DESC LIMIT 12"
    ).fetchall()
    if rows:
        print("\n## Findings")
        for seq, actor, payload in reversed(rows):
            d = json.loads(payload)
            sev = d.get("severity", "info")
            kind = d.get("kind", "finding")
            route = f" route={d.get('route_hash')}" if d.get("route_hash") else ""
            print(f"- #{seq} [{sev}/{kind}] {actor}:{route} {d.get('summary', '')}")

    challenged: list[tuple] = []
    if _table_exists(c, "fact_reviews"):
        challenged = c.execute(
            "SELECT fact_seq, status, reason, verification_intent_id "
            "FROM fact_reviews WHERE status='challenged' ORDER BY challenged_seq"
        ).fetchall()
    if challenged:
        print("\n## Challenged facts — do NOT rely on these until verified")
        for fact_seq, status, reason, verification_intent_id in challenged:
            fact = _event_payload_by_seq(c, int(fact_seq)).get("fact", "")
            print(f"- fact #{fact_seq}: {fact}")
            print(f"  reason: {reason or ''}")
            if verification_intent_id:
                print(f"  verify intent: {verification_intent_id}")

    dirs = c.execute(
        "SELECT seq, actor, payload FROM events "
        "WHERE kind='coordinator_directive' ORDER BY seq DESC LIMIT 8"
    ).fetchall()
    if dirs:
        print("\n## Coordinator directives")
        for seq, actor, payload in reversed(dirs):
            d = json.loads(payload)
            print(f"- #{seq} {actor} {d.get('action', 'note')}: {d.get('directive', '')}")

    print("\n## Routes")
    read_routes()
    print("\n## Branches")
    read_branches()


def list_intents() -> None:
    c = _conn()
    cols = {row[1] for row in c.execute("PRAGMA table_info(intents)").fetchall()}
    select_cols = ["intent_id", "goal"]
    for optional in ("worker_class", "route_hash", "branch_id"):
        select_cols.append(optional if optional in cols else "''")
    # only dispatch_state='active' intents are claimable; resume/retired/closed are
    # held back (the column is absent on old DBs → no filter, same as before).
    where = "status='open'"
    if "dispatch_state" in cols:
        where += " AND dispatch_state='active'"
    rows = c.execute(
        "SELECT " + ",".join(select_cols) +
        f" FROM intents WHERE {where} ORDER BY created_seq"
    ).fetchall()
    if not rows:
        print("(no open intents)")
        return
    print("# Open intents you can claim:")
    for iid, goal, worker_class, route_hash, branch_id in rows:
        meta = []
        if worker_class:
            meta.append(f"class={worker_class}")
        if route_hash:
            meta.append(f"route={route_hash}")
        if branch_id:
            meta.append(f"branch={branch_id}")
        suffix = f" [{' '.join(meta)}]" if meta else ""
        print(f"- {iid}: {goal}{suffix}")



def write_fact(text: str, verified: bool) -> None:
    c = _conn()
    cid = _challenge_id(c)
    payload_obj = {"source": _ACTOR, "fact": text, "source_solver": _ACTOR,
                   "witness": None, "verifier": _ACTOR if verified else ""}
    if _INTENT_ID:
        payload_obj["intent_id"] = _INTENT_ID
    payload = json.dumps(payload_obj)
    # dedupe on fact IDENTITY, matching SQLiteSharedGraph.add_evidence exactly so a
    # bare skill fact and its "[engine] <text>" VERIFIED_FACT marker echo collide on
    # one key (strip a leading "[engine] " tag, fold whitespace, lowercase; artifact
    # is provenance, not identity). Keep this in lockstep with _normalize_fact_identity.
    _norm = re.sub(r"^\[[a-z0-9 _.-]{1,40}\]\s*", "", text, flags=re.IGNORECASE)
    _norm = " ".join(_norm.split()).lower()
    dk = f"fact::{_ACTOR}::{_norm}"
    try:
        cur = c.execute(
            "INSERT INTO events (ts, challenge_id, actor, kind, payload, "
            "artifact_id, verified, confidence, dedupe_key) "
            "VALUES (?,?,?,?,?,?,?,?,?)",
            (time.time(), cid, _ACTOR, "fact_added", payload, None,
             int(verified), 1.0 if verified else 0.4, dk))
        fact_seq = int(cur.lastrowid or 0)
        if _INTENT_ID and fact_seq > 0 and _has_table(c, "intent_products"):
            c.execute(
                "INSERT OR IGNORE INTO intent_products (intent_id, fact_seq) VALUES (?,?)",
                (_INTENT_ID, fact_seq))
        c.commit()
        print(f"OK wrote {'verified' if verified else 'candidate'} fact")
    except sqlite3.IntegrityError:
        print("OK (duplicate fact, already on board)")


def mark_deadend(reason: str) -> None:
    c = _conn()
    cid = _challenge_id(c)
    payload = json.dumps({"reason": reason})
    try:
        c.execute(
            "INSERT INTO events (ts, challenge_id, actor, kind, payload, "
            "verified, confidence, dedupe_key) VALUES (?,?,?,?,?,?,?,?)",
            (time.time(), cid, _ACTOR, "dead_end", payload, 0, 1.0,
             f"deadend::{reason}"))
        c.commit()
        print("OK marked dead-end")
    except sqlite3.IntegrityError:
        print("OK (dead-end already recorded)")


def claim(intent_id: str) -> None:
    c = _conn()
    cid = _challenge_id(c)
    now = time.time()
    # a resume/retired/closed intent is NOT claimable even while status='open'
    # (the column is absent on old DBs → no extra fence, same as before).
    active_fence = " AND dispatch_state='active'" if _has_column(c, "intents", "dispatch_state") else ""
    cur = c.execute(
        "UPDATE intents SET worker=?, status='claimed', lease_until=? "
        "WHERE intent_id=? AND challenge_id=?" + active_fence +
        "  AND (status='open' OR (status='claimed' AND lease_until < ?))",
        (_ACTOR, now + 300.0, intent_id, cid, now))
    c.commit()
    if cur.rowcount == 1:
        c.execute(
            "INSERT INTO events (ts, challenge_id, actor, kind, payload, "
            "verified, confidence) VALUES (?,?,?,?,?,?,?)",
            (now, cid, _ACTOR, "intent_claimed",
             json.dumps({"intent_id": intent_id}), 0, 1.0))
        c.commit()
        print("WON")
    else:
        print("LOST")


def _norm_activity_key(key: str) -> str:
    import re
    k = (key or "").strip().lower()
    k = re.sub(r"[\s/]+", ":", k)
    k = re.sub(r":+", ":", k).strip(":")
    return k


def claim_activity(key: str, lease_s: float = 600.0) -> None:
    """P4: claim a high-cost activity (e.g. 'nmap:8.130.96.176'). WON = go ahead;
    LOST = a teammate is already doing it, AVOID redoing."""
    c = _conn()
    cid = _challenge_id(c)
    nkey = _norm_activity_key(key)
    now = time.time()
    if not nkey:
        print("WON")
        return
    # the table may not exist on an old DB — create-if-missing, best-effort.
    c.execute(
        "CREATE TABLE IF NOT EXISTS activity_locks ("
        "activity_key TEXT PRIMARY KEY, challenge_id TEXT NOT NULL, "
        "worker TEXT NOT NULL, lease_until REAL NOT NULL, claimed_ts REAL NOT NULL)")
    cur = c.execute(
        "INSERT INTO activity_locks "
        "(activity_key, challenge_id, worker, lease_until, claimed_ts) "
        "VALUES (?,?,?,?,?) "
        "ON CONFLICT(activity_key) DO UPDATE SET "
        "  worker=excluded.worker, lease_until=excluded.lease_until, "
        "  claimed_ts=excluded.claimed_ts "
        "WHERE activity_locks.lease_until < ?",
        (nkey, cid, _ACTOR, now + lease_s, now, now))
    c.commit()
    print("WON" if cur.rowcount == 1 else "LOST")


def list_activities() -> None:
    """P4: in-progress activities (lease not expired) a teammate is doing now."""
    c = _conn()
    cid = _challenge_id(c)
    now = time.time()
    try:
        rows = c.execute(
            "SELECT activity_key, worker FROM activity_locks "
            "WHERE challenge_id=? AND lease_until > ? ORDER BY claimed_ts",
            (cid, now)).fetchall()
    except Exception:
        rows = []
    if not rows:
        print("(no activities in progress)")
        return
    for key, worker in rows:
        print(f"{key}  [{worker}]")


def _normalize_resource_key(key: str) -> str:
    import re
    raw = (key or "").strip().lower()
    raw = re.sub(r"\s+", "", raw)
    raw = re.sub(r"[^a-z0-9_:@.*/-]+", "-", raw).strip("-")
    return raw[:180]


def claim_resource(resource_key: str, scope: str = "activity",
                   risk_class: str = "", lease_s: float = 600.0) -> None:
    """E: claim a shared RESOURCE (exclusive site/account/listener). WON = exclusive
    access granted; LOST = a teammate holds it — do not run conflicting work."""
    c = _conn()
    cid = _challenge_id(c)
    rkey = _normalize_resource_key(resource_key)
    now = time.time()
    if not rkey:
        print("WON")
        return
    c.execute(
        "CREATE TABLE IF NOT EXISTS resource_locks ("
        "lock_id TEXT PRIMARY KEY, challenge_id TEXT NOT NULL, resource_key TEXT NOT NULL, "
        "scope TEXT NOT NULL, risk_class TEXT, status TEXT NOT NULL DEFAULT 'requested', "
        "owner_worker TEXT, owner_intent TEXT, lease_until REAL, created_seq INTEGER, "
        "released_seq INTEGER, conflict_policy TEXT NOT NULL DEFAULT 'exclusive', "
        "cooldown_s REAL NOT NULL DEFAULT 0)")
    lock_id = f"rl-{rkey}"
    # take over only if free, owned by us, or the existing lease expired (self-heal).
    cur = c.execute(
        "INSERT INTO resource_locks "
        "(lock_id, challenge_id, resource_key, scope, risk_class, status, owner_worker, lease_until) "
        "VALUES (?,?,?,?,?,'active',?,?) "
        "ON CONFLICT(lock_id) DO UPDATE SET "
        "  status='active', owner_worker=excluded.owner_worker, "
        "  scope=excluded.scope, risk_class=excluded.risk_class, lease_until=excluded.lease_until "
        "WHERE resource_locks.owner_worker=excluded.owner_worker "
        "   OR resource_locks.lease_until IS NULL OR resource_locks.lease_until < ?",
        (lock_id, cid, rkey, scope or "activity", risk_class or None, _ACTOR,
         now + lease_s, now))
    c.commit()
    if cur.rowcount == 1:
        c.execute(
            "INSERT INTO events (ts, challenge_id, actor, kind, payload, verified, confidence) "
            "VALUES (?,?,?,?,?,?,?)",
            (now, cid, _ACTOR, "resource_locked",
             json.dumps({"resource_key": rkey, "scope": scope, "lock_id": lock_id}), 0, 1.0))
        c.commit()
        print("WON")
    else:
        print("LOST")


def release_resource(resource_key: str) -> None:
    """E: release a resource lock this worker holds (owner-fenced, best-effort)."""
    c = _conn()
    cid = _challenge_id(c)
    rkey = _normalize_resource_key(resource_key)
    now = time.time()
    if not _has_table(c, "resource_locks") or not rkey:
        print("OK")
        return
    cur = c.execute(
        "UPDATE resource_locks SET status='released', owner_worker=NULL, lease_until=NULL "
        "WHERE challenge_id=? AND resource_key=? AND owner_worker=?",
        (cid, rkey, _ACTOR))
    c.commit()
    if cur.rowcount >= 1:
        c.execute(
            "INSERT INTO events (ts, challenge_id, actor, kind, payload, verified, confidence) "
            "VALUES (?,?,?,?,?,?,?)",
            (now, cid, _ACTOR, "resource_released",
             json.dumps({"resource_key": rkey}), 0, 1.0))
        c.commit()
    print("OK")


def read_resource_locks() -> None:
    """E: active resource locks a teammate holds now (avoid conflicting work)."""
    c = _conn()
    cid = _challenge_id(c)
    now = time.time()
    if not _has_table(c, "resource_locks"):
        print("(no resource locks)")
        return
    rows = c.execute(
        "SELECT resource_key, scope, risk_class, owner_worker FROM resource_locks "
        "WHERE challenge_id=? AND status='active' AND owner_worker IS NOT NULL "
        "AND (lease_until IS NULL OR lease_until > ?) ORDER BY created_seq",
        (cid, now)).fetchall()
    if not rows:
        print("(no resource locks held)")
        return
    print("# Resource locks held by teammates (do NOT duplicate):")
    for rkey, scope, risk, owner in rows:
        risk_s = f" risk={risk}" if risk else ""
        print(f"- {rkey} (scope={scope}{risk_s}) [{owner}]")


def read_directives() -> None:
    """B: operator directives the swarm must respect (highest priority guidance)."""
    c = _conn()
    cid = _challenge_id(c)
    if not _has_table(c, "operator_directives"):
        print("(no operator directives)")
        return
    rows = c.execute(
        "SELECT directive_id, action, text, status, priority FROM operator_directives "
        "WHERE challenge_id=? AND status NOT IN ('superseded','expired','rejected') "
        "ORDER BY priority DESC, received_seq",
        (cid,)).fetchall()
    if not rows:
        print("(no active operator directives)")
        return
    print("# Operator directives (must respect — guidance, not evidence):")
    for did, action, text, status, priority in rows:
        print(f"- [{action}/{status}] {text}  (id={did})")


def directive_status(directive_id: str) -> None:
    """B: delivery status of one operator directive."""
    c = _conn()
    cid = _challenge_id(c)
    if not _has_table(c, "operator_directives"):
        print("(unknown)")
        return
    row = c.execute(
        "SELECT action, text, status, bound_worker FROM operator_directives "
        "WHERE challenge_id=? AND directive_id=?",
        (cid, directive_id)).fetchone()
    if not row:
        print("(unknown directive)")
        return
    action, text, status, bound = row
    bound_s = f" bound={bound}" if bound else ""
    print(f"{directive_id}: {action} status={status}{bound_s} :: {text}")


def read_declarations() -> None:
    """f01: show declaration_targets for current intent (or all). Lazy if tables absent."""
    c = _conn()
    if not _has_table(c, "declaration_targets"):
        print("(no declaration_targets — framework not active)")
        return
    intent = _INTENT_ID
    if intent:
        rows = c.execute(
            "SELECT intent_id, target_id, predicate, polarity, receipt_class, "
            "receipt_key, effect_types, confidence FROM declaration_targets "
            "WHERE intent_id=? ORDER BY target_id",
            (intent,),
        ).fetchall()
    else:
        rows = c.execute(
            "SELECT intent_id, target_id, predicate, polarity, receipt_class, "
            "receipt_key, effect_types, confidence FROM declaration_targets "
            "ORDER BY intent_id, target_id LIMIT 80"
        ).fetchall()
    if not rows:
        print("(no declarations)")
        return
    for r in rows:
        print(
            f"{r[0]} target={r[1]} pred={r[2]} pol={r[3]} "
            f"receipt={r[4]}:{r[5]} effects={r[6]} conf={r[7]}"
        )


def read_model() -> None:
    """f02: print current challenge_models row (lazy if table absent)."""
    c = _conn()
    if not _has_table(c, "challenge_models"):
        print("(no challenge_models — framework not active)")
        return
    row = c.execute(
        "SELECT model_id, version, parent_version, domain_json FROM challenge_models "
        "WHERE is_current=1 ORDER BY version DESC LIMIT 1"
    ).fetchone()
    if not row:
        print("(no current model)")
        return
    print(f"model_id={row[0]} version={row[1]} parent={row[2]}")
    print(row[3])


def write_prediction(text: str) -> None:
    """f02: worker writes a prediction note for its intent (append-only event)."""
    c = _conn()
    if not _has_table(c, "events"):
        print("ERROR: no events table", file=sys.stderr)
        sys.exit(2)
    payload = json.dumps(
        {
            "text": text,
            "intent_id": _INTENT_ID or "",
            "source": "write-prediction",
        },
        ensure_ascii=False,
    )
    try:
        cid = _challenge_id(c)
    except Exception:
        cid = ""
    c.execute(
        "INSERT INTO events (ts, challenge_id, actor, kind, payload, verified, "
        "confidence) VALUES (?,?,?,?,?,?,?)",
        (time.time(), cid, _ACTOR, "prediction_written", payload, 0, 1.0),
    )
    if _has_table(c, "experiment_predictions") and _INTENT_ID:
        try:
            c.execute(
                "UPDATE experiment_predictions SET predicted_observation=?, "
                "status='open' WHERE intent_id=?",
                (text[:2000], _INTENT_ID),
            )
        except Exception:
            pass
    c.commit()
    print("OK")


def write_observation(text: str) -> None:
    """f02: worker writes a structured observation note (append-only event)."""
    c = _conn()
    if not _has_table(c, "events"):
        print("ERROR: no events table", file=sys.stderr)
        sys.exit(2)
    payload = json.dumps(
        {
            "text": text,
            "intent_id": _INTENT_ID or "",
            "source": "write-observation",
        },
        ensure_ascii=False,
    )
    try:
        cid = _challenge_id(c)
    except Exception:
        cid = ""
    c.execute(
        "INSERT INTO events (ts, challenge_id, actor, kind, payload, verified, "
        "confidence) VALUES (?,?,?,?,?,?,?)",
        (time.time(), cid, _ACTOR, "observation_written", payload, 0, 1.0),
    )
    c.commit()
    print("OK")


def read_tree() -> None:
    """f03: show open tree nodes/edges (lazy if tables absent)."""
    c = _conn()
    if not _has_table(c, "solution_tree_nodes"):
        print("(no solution_tree_nodes — framework not active)")
        return
    nodes = c.execute(
        "SELECT node_id, depth, status, q_value, visit_count FROM solution_tree_nodes "
        "ORDER BY depth, created_at LIMIT 40"
    ).fetchall()
    if not nodes:
        print("(empty tree)")
    else:
        for n in nodes:
            print(f"node {n[0]} depth={n[1]} status={n[2]} q={n[3]} visits={n[4]}")
    if _has_table(c, "solution_tree_edges"):
        edges = c.execute(
            "SELECT edge_id, parent_node_id, intent_id, action_kind, status, "
            "novelty_forced FROM solution_tree_edges ORDER BY created_at LIMIT 40"
        ).fetchall()
        for e in edges:
            print(
                f"edge {e[0]} parent={e[1]} intent={e[2]} kind={e[3]} "
                f"status={e[4]} novelty={e[5]}"
            )


def write_checkpoint(checkpoint_id: str) -> None:
    """f03: worker claims a checkpoint id was reached (append-only event)."""
    c = _conn()
    if not _has_table(c, "events"):
        print("ERROR: no events table", file=sys.stderr)
        sys.exit(2)
    payload = json.dumps(
        {
            "checkpoint_id": checkpoint_id,
            "intent_id": _INTENT_ID or "",
            "source": "write-checkpoint",
        },
        ensure_ascii=False,
    )
    try:
        cid = _challenge_id(c)
    except Exception:
        cid = ""
    c.execute(
        "INSERT INTO events (ts, challenge_id, actor, kind, payload, verified, "
        "confidence) VALUES (?,?,?,?,?,?,?)",
        (time.time(), cid, _ACTOR, "tree_checkpoint_hit", payload, 0, 1.0),
    )
    c.commit()
    print("OK")


def read_archive() -> None:
    """f04: show niche coverage + top archive entries (lazy if tables absent)."""
    c = _conn()
    if not _has_table(c, "qd_niche_coverage"):
        print("(no qd_niche_coverage — framework not active)")
        return
    rows = c.execute(
        "SELECT niche_key, hit_count, active_entries, best_quality, deficit_score "
        "FROM qd_niche_coverage ORDER BY deficit_score DESC, hit_count DESC LIMIT 40"
    ).fetchall()
    if not rows:
        print("(empty coverage)")
    else:
        for r in rows:
            print(
                f"niche {r[0]} hits={r[1]} active={r[2]} best_q={r[3]} deficit={r[4]}"
            )
    if _has_table(c, "qd_archive_entry"):
        ents = c.execute(
            "SELECT entry_id, niche_key, quality_score, status FROM qd_archive_entry "
            "ORDER BY updated_at DESC LIMIT 20"
        ).fetchall()
        for e in ents:
            print(f"entry {e[0]} niche={e[1]} q={e[2]} status={e[3]}")


def market_list() -> None:
    """f05: list open/cleared auctions (lazy if tables absent)."""
    c = _conn()
    if not _has_table(c, "market_intent_auction"):
        print("(no market_intent_auction — framework not active)")
        return
    rows = c.execute(
        "SELECT auction_id, intent_id, behavior_descriptor, status, "
        "cleared_engine, reserve_price, voi_estimate "
        "FROM market_intent_auction ORDER BY created_at DESC LIMIT 40"
    ).fetchall()
    if not rows:
        print("(empty market)")
        return
    for r in rows:
        print(
            f"auc {r[0]} intent={r[1]} bd={r[2]} status={r[3]} "
            f"winner={r[4]} reserve={r[5]} voi={r[6]}"
        )


def read_cases() -> None:
    """f06: list adaptation plan + top case records (lazy if tables absent)."""
    c = _conn()
    if not _has_table(c, "case_bank_record") and not _has_table(
        c, "case_bank_adaptation"
    ):
        print("(no case_bank_record — framework not active)")
        return
    if _has_table(c, "case_bank_adaptation"):
        plans = c.execute(
            "SELECT plan_id, confidence, source_case_ids FROM case_bank_adaptation "
            "ORDER BY created_at DESC LIMIT 5"
        ).fetchall()
        if not plans:
            print("(no adaptation plans)")
        for p in plans:
            print(f"plan {p[0]} conf={p[1]} cases={p[2]}")
    if _has_table(c, "case_bank_record"):
        rows = c.execute(
            "SELECT case_id, category, validation_status, utility_score, flag_hash "
            "FROM case_bank_record ORDER BY utility_score DESC LIMIT 20"
        ).fetchall()
        for r in rows:
            # Never print flag plaintext — only hash prefix if present.
            fh = (r[4] or "")[:12]
            print(
                f"case {r[0]} cat={r[1]} status={r[2]} util={r[3]} flag_hash={fh or '-'}"
            )


def read_slow_tree() -> None:
    c = _conn()
    if not _has_table(c, "slow_tree"):
        print("(no slow_tree — framework not active)")
        return
    for r in c.execute(
        "SELECT node_id, status, leaf_kind, surprise_score, intent_id "
        "FROM slow_tree ORDER BY created_at LIMIT 40"
    ):
        print(f"node {r[0]} status={r[1]} kind={r[2]} surprise={r[3]} intent={r[4]}")


def read_lineages() -> None:
    c = _conn()
    if not _has_table(c, "evo_lineages"):
        print("(no evo_lineages — framework not active)")
        return
    for r in c.execute(
        "SELECT lineage_id, generation, fitness_mu, status, verified_count "
        "FROM evo_lineages ORDER BY fitness_mu DESC LIMIT 40"
    ):
        print(
            f"lin {r[0]} gen={r[1]} fit={r[2]} status={r[3]} verified={r[4]}"
        )


def read_epistemic() -> None:
    c = _conn()
    if not _has_table(c, "epistemic_nodes"):
        print("(no epistemic_nodes — framework not active)")
        return
    for r in c.execute(
        "SELECT node_seq, node_kind, state, confidence, substr(content,1,80) "
        "FROM epistemic_nodes ORDER BY node_seq DESC LIMIT 40"
    ):
        print(f"n{r[0]} {r[1]} [{r[2]}] conf={r[3]} {r[4]}")


def read_edge_shells() -> None:
    c = _conn()
    if not _has_table(c, "edge_worker_budget"):
        print("(no edge_worker_budget — framework not active)")
        return
    for r in c.execute(
        "SELECT shell_id, intent_id, turns_used, turn_limit, killed "
        "FROM edge_worker_budget ORDER BY updated_at DESC LIMIT 40"
    ):
        print(
            f"shell {r[0]} intent={r[1]} turns={r[2]}/{r[3]} killed={r[4]}"
        )


def write_outcome(text: str, state: str = "satisfied", note: str = "") -> None:
    """f01: append a zero-authority worker observation for one declared target."""
    c = _conn()
    if not _has_table(c, "declaration_targets"):
        print("(no declaration_targets — framework not active)")
        return
    if not _INTENT_ID:
        print("ERROR: MUTEKI_INTENT_ID is required for write-outcome", file=sys.stderr)
        return
    rows = c.execute(
        "SELECT target_id, predicate, receipt_class, receipt_key "
        "FROM declaration_targets WHERE intent_id=? ORDER BY target_id",
        (_INTENT_ID,),
    ).fetchall()
    selected = next((row for row in rows if str(row[0]) == text), None)
    if selected is None and len(rows) == 1:
        selected = rows[0]
        note = note or text
    if selected is None:
        print("ERROR: target_id is not declared for this intent", file=sys.stderr)
        return
    target_id, predicate, receipt_class, receipt_key = selected
    payload = json.dumps(
        {
            "intent_id": _INTENT_ID,
            "target_id": target_id,
            "predicate": predicate,
            "receipt_class": receipt_class,
            "receipt_key": receipt_key,
            "observed_state": state,
            "note": note,
            "source": "write-outcome",
        },
        ensure_ascii=False,
    )
    try:
        cid = _challenge_id(c)
    except Exception:
        cid = ""
    c.execute(
        "INSERT INTO events (ts, challenge_id, actor, kind, payload, verified, "
        "confidence) VALUES (?,?,?,?,?,?,?)",
        (time.time(), cid, _ACTOR, "declaration_outcome", payload, 0, 1.0),
    )
    c.commit()
    print("OK")


# ---------------------------------------------------------------------------
# f11 agent-teams: teammate-mode team subcommands (T03/T04/T09/T11/T12).
# These talk to the team_* tables in the same SQLite DB. In --mode=teammate
# ONLY these subcommands (plus provenance writes) are registered — full-board
# reads are not (Gate-0b, enforced at CLI level, not by prompt discipline).
# ---------------------------------------------------------------------------

_TEAM_MEMBER = os.environ.get("MUTEKI_TEAM_MEMBER", "").strip() or _ACTOR

# Keep in lockstep with muteki/frameworks/f11_agent_teams/schema.py (this
# script is stdlib-only and runs inside worker containers, so no import).
TEAMMATE_ALLOWED_CMDS = frozenset({
    "msg-send", "msg-check", "task-list", "task-claim", "task-done",
    "assert-write", "artifact-put", "token-wait", "heartbeat",
    "write-fact", "mark-deadend", "claim", "claim-resource", "release-resource",
    "list-intents", "read-flags", "read-resource-locks", "read-deadends",
    "read-review",
    "submit-flag",
})
_TEAM_CHANNEL_KINDS = frozenset({"evidence", "dead_end", "surprise", "request_help"})
_TEAM_MSG_KINDS = frozenset({
    "direct", "broadcast", "handoff", "plan_request", "plan_verdict",
    "evidence", "dead_end", "contradiction", "heartbeat", "channel", "ack",
})
_TEAM_PROTOCOL_KINDS = frozenset({"ack", "heartbeat"})
_TEAM_EVIDENCE_REQUIRED = frozenset({"evidence", "dead_end", "contradiction"})
_TEAM_CAP_EXEMPT = frozenset({"evidence", "channel:evidence"})


def _team(c: sqlite3.Connection):
    """(team_id, budget, members) for this challenge's team, or None."""
    if not _has_table(c, "team_roster"):
        return None
    tid = os.environ.get("MUTEKI_TEAM_ID", "").strip()
    if tid:
        row = c.execute(
            "SELECT team_id, budget_json, members_json FROM team_roster "
            "WHERE team_id=?", (tid,)).fetchone()
    else:
        row = c.execute(
            "SELECT team_id, budget_json, members_json FROM team_roster "
            "ORDER BY created_at DESC LIMIT 1").fetchone()
    if not row:
        return None
    try:
        budget = json.loads(row[1] or "{}")
    except Exception:
        budget = {}
    try:
        members = json.loads(row[2] or "[]")
    except Exception:
        members = []
    return (row[0], budget, members)


def _team_next_seq(c: sqlite3.Connection, team_id: str) -> int:
    row = c.execute(
        "SELECT next_seq FROM team_seq WHERE team_id=?", (team_id,)).fetchone()
    if row is None:
        c.execute("INSERT INTO team_seq (team_id, next_seq) VALUES (?,2)", (team_id,))
        return 1
    seq = int(row[0])
    c.execute("UPDATE team_seq SET next_seq=? WHERE team_id=?", (seq + 1, team_id))
    return seq


def _team_event(c: sqlite3.Connection, kind: str, payload: dict) -> None:
    """Mirror team ops into the events ledger (audit/receipt), best-effort."""
    try:
        cid = _challenge_id(c)
    except Exception:
        cid = ""
    try:
        c.execute(
            "INSERT INTO events (ts, challenge_id, actor, kind, payload, "
            "verified, confidence) VALUES (?,?,?,?,?,?,?)",
            (time.time(), cid, _TEAM_MEMBER, kind,
             json.dumps(payload, ensure_ascii=False), 0, 1.0))
    except Exception:
        pass


def _parse_evidence_refs(items) -> list:
    """--evidence kind:ref:digest (repeatable) → evidence_refs list."""
    refs = []
    for it in items or []:
        parts = str(it).split(":", 2)
        if len(parts) == 3:
            refs.append({"kind": parts[0], "ref": parts[1], "digest": parts[2]})
        elif len(parts) == 2:
            refs.append({"kind": parts[0], "ref": parts[1], "digest": ""})
        else:
            refs.append({"kind": "artifact", "ref": str(it), "digest": ""})
    return refs


def msg_send(kind, to, body, verbatim, evidence, hop, require_ack, thread,
             channel_kind, ack_of) -> None:
    c = _conn()
    t = _team(c)
    if t is None:
        print("ERROR: no team roster (f11 not active)", file=sys.stderr)
        sys.exit(2)
    team_id, budget, _members = t
    if hop > 2:
        print("REJECTED hop_exceeded (hop>2)")
        return
    if kind == "channel":
        if not channel_kind:
            print("REJECTED channel_kind_required "
                  "(evidence|dead_end|surprise|request_help)")
            return
        if channel_kind not in _TEAM_CHANNEL_KINDS:
            print("REJECTED channel_kind_forbidden")
            return
        to = ["*"]
    elif kind not in _TEAM_MSG_KINDS:
        print(f"REJECTED unknown_kind {kind}")
        return
    refs = _parse_evidence_refs(evidence)
    stored_kind = kind if kind != "channel" else f"channel:{channel_kind}"
    if kind == "ack":
        if not ack_of:
            print("REJECTED ack_requires_ack_of")
            return
    elif kind == "channel" and channel_kind in ("evidence", "dead_end") and not refs:
        print("REJECTED evidence_required")
        return
    elif kind in _TEAM_EVIDENCE_REQUIRED and not refs:
        print("REJECTED evidence_required")
        return
    # verbatim hard cap ~500 tokens ≈ 2000 chars per field (T04)
    verb = []
    for v in verbatim or []:
        s = str(v)
        if len(s) > 2000:
            dig = hashlib.sha256(s.encode()).hexdigest()[:16]
            verb.append(f"[artifact-ref sha256:{dig} len={len(s)}]")
        else:
            verb.append(s)
    now = time.time()
    c.execute("BEGIN IMMEDIATE")
    try:
        # T07 cost gate (mirrors team.py send_message enforcement).
        if kind not in _TEAM_PROTOCOL_KINDS:
            msg_cap = int(budget.get("msg_cap") or 200)
            used = int(c.execute(
                "SELECT COUNT(*) FROM team_message WHERE team_id=? "
                "AND kind NOT IN ('ack','heartbeat')", (team_id,)).fetchone()[0])
            over_cap = used >= msg_cap
            circuit_open = bool(budget.get("circuit_open"))
            if (over_cap or circuit_open) and stored_kind not in _TEAM_CAP_EXEMPT:
                c.execute("ROLLBACK")
                print("REJECTED " + ("circuit_open" if circuit_open
                                     else "msg_cap_exceeded"))
                return
            if kind == "channel":
                per_cap = int(budget.get("channel_per_member_cap") or 12)
                mine = int(c.execute(
                    "SELECT COUNT(*) FROM team_message WHERE team_id=? "
                    "AND from_member=? AND kind LIKE 'channel:%'",
                    (team_id, _TEAM_MEMBER)).fetchone()[0])
                if mine >= per_cap:
                    c.execute("ROLLBACK")
                    print("REJECTED channel_cap_exceeded")
                    return
        seq = _team_next_seq(c, team_id)
        msg_id = f"m-{uuid.uuid4().hex[:12]}"
        c.execute(
            "INSERT INTO team_message "
            "(msg_id, team_id, seq, thread, kind, from_member, to_json, "
            " verbatim_json, body, evidence_refs, declared_effects, hop, "
            " require_ack, acked_by, ack_of, ttl_s, created_at) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,'[]',?,?,'[]',?,300,?)",
            (msg_id, team_id, seq, thread, stored_kind, _TEAM_MEMBER,
             json.dumps(to or []), json.dumps(verb, ensure_ascii=False),
             (body or "")[:4000], json.dumps(refs, ensure_ascii=False),
             int(hop), 1 if require_ack else 0, ack_of, now))
        if kind == "ack" and ack_of:
            row = c.execute(
                "SELECT acked_by FROM team_message WHERE msg_id=? AND team_id=?",
                (ack_of, team_id)).fetchone()
            if row:
                try:
                    acked = json.loads(row[0] or "[]")
                except Exception:
                    acked = []
                if _TEAM_MEMBER not in acked:
                    acked.append(_TEAM_MEMBER)
                c.execute(
                    "UPDATE team_message SET acked_by=? WHERE msg_id=? AND team_id=?",
                    (json.dumps(acked), ack_of, team_id))
        _team_event(c, "team_channel_posted" if kind == "channel" else "team_msg_sent",
                    {"team_id": team_id, "msg_id": msg_id, "seq": seq,
                     "kind": kind, "from": _TEAM_MEMBER, "to": to or []})
        c.execute("COMMIT")
    except Exception:
        c.execute("ROLLBACK")
        raise
    print(f"OK seq={seq} msg_id={msg_id}")


def msg_check(after_seq: int, digest: bool) -> None:
    c = _conn()
    t = _team(c)
    if t is None:
        print("(no team roster — f11 not active)")
        return
    team_id, _budget, _members = t
    if digest:
        if _has_table(c, "team_channel_digest"):
            row = c.execute(
                "SELECT digest_id, span_lo, span_hi, distilled, by_model "
                "FROM team_channel_digest WHERE team_id=? "
                "ORDER BY generated_at DESC LIMIT 1", (team_id,)).fetchone()
            if row:
                print(f"# digest {row[0]} span={row[1]}..{row[2]} by={row[4]}")
                print(row[3])
                return
        print("(no channel digest yet)")
        return
    rows = c.execute(
        "SELECT msg_id, seq, kind, from_member, to_json, body, require_ack "
        "FROM team_message WHERE team_id=? AND seq>? ORDER BY seq ASC LIMIT 50",
        (team_id, int(after_seq))).fetchall()
    shown = 0
    for msg_id, seq, kind, frm, to_json, body, req_ack in rows:
        try:
            to = json.loads(to_json or "[]")
        except Exception:
            to = []
        addressed = (_TEAM_MEMBER in to) or ("*" in to) or kind.startswith("channel:")
        if not addressed or frm == _TEAM_MEMBER:
            continue
        print(f"[{seq}] {kind} {frm}: {str(body or '')[:200]}")
        shown += 1
        if req_ack:
            # explicit delivery confirmation (§2.3) — protocol frame, free.
            sub = _conn()
            st = _team(sub)
            if st:
                seq2 = _team_next_seq(sub, team_id)
                ack_id = f"m-{uuid.uuid4().hex[:12]}"
                sub.execute(
                    "INSERT INTO team_message "
                    "(msg_id, team_id, seq, thread, kind, from_member, to_json, "
                    " verbatim_json, body, evidence_refs, declared_effects, hop, "
                    " require_ack, acked_by, ack_of, ttl_s, created_at) "
                    "VALUES (?,?,?,NULL,'ack',?,'[]','[]','','[]','[]',0,0,'[]',?,300,?)",
                    (ack_id, team_id, seq2, _TEAM_MEMBER, msg_id, time.time()))
                try:
                    acked = json.loads(sub.execute(
                        "SELECT acked_by FROM team_message WHERE msg_id=?",
                        (msg_id,)).fetchone()[0] or "[]")
                except Exception:
                    acked = []
                if _TEAM_MEMBER not in acked:
                    acked.append(_TEAM_MEMBER)
                sub.execute("UPDATE team_message SET acked_by=? WHERE msg_id=?",
                            (json.dumps(acked), msg_id))
                sub.commit()
            print(f"    (acked {msg_id})")
    if not shown:
        print("(no new messages)")


def task_list(status: str = "") -> None:
    c = _conn()
    t = _team(c)
    if t is None:
        print("(no team roster — f11 not active)")
        return
    team_id, _budget, _members = t
    if status:
        rows = c.execute(
            "SELECT task_id, goal, status, owner, declared_effects FROM team_task "
            "WHERE team_id=? AND status=? ORDER BY created_at ASC LIMIT 50",
            (team_id, status)).fetchall()
    else:
        rows = c.execute(
            "SELECT task_id, goal, status, owner, declared_effects FROM team_task "
            "WHERE team_id=? ORDER BY created_at ASC LIMIT 50", (team_id,)).fetchall()
    if not rows:
        print("(no tasks)")
        return
    for tid, goal, st, owner, effects in rows:
        print(f"- {tid} [{st}] owner={owner or '-'}: {str(goal)[:140]}")
        try:
            eff = json.loads(effects or "[]")
        except Exception:
            eff = []
        if eff:
            print(f"    declared_effects={json.dumps(eff)[:160]}")


def task_claim(task_id: str, token_id: str = "", token_fence: int = -1) -> None:
    c = _conn()
    t = _team(c)
    if t is None:
        print("LOST (no team roster)")
        return
    team_id, _budget, _members = t
    now = time.time()
    c.execute("BEGIN IMMEDIATE")
    try:
        if token_id:
            row = c.execute(
                "SELECT holder, fence, status, lease_expires_at FROM team_turn_token "
                "WHERE token_id=? AND team_id=?", (token_id, team_id)).fetchone()
            if not row or row[2] != "active" or row[0] != _TEAM_MEMBER:
                c.execute("ROLLBACK")
                print("LOST (token_not_held)")
                return
            if token_fence >= 0 and int(row[1]) != int(token_fence):
                c.execute("ROLLBACK")
                print("LOST (token_fence_mismatch)")
                return
            if row[3] is not None and float(row[3]) < now:
                c.execute("ROLLBACK")
                print("LOST (token_expired)")
                return
        cur = c.execute(
            "UPDATE team_task SET status='claimed', owner=?, fence=fence+1, "
            "lease_json=?, updated_at=? "
            "WHERE task_id=? AND team_id=? AND status='pending' "
            "AND (owner IS NULL OR owner='')",
            (_TEAM_MEMBER, json.dumps({"owner": _TEAM_MEMBER, "claimed_at": now}),
             now, task_id, team_id))
        if cur.rowcount != 1:
            c.execute("ROLLBACK")
            print("LOST")
            return
        _team_event(c, "team_task_claimed",
                    {"team_id": team_id, "task_id": task_id, "owner": _TEAM_MEMBER})
        c.execute("COMMIT")
    except Exception:
        c.execute("ROLLBACK")
        raise
    print("WON")


def task_done(task_id: str, evidence) -> None:
    refs = _parse_evidence_refs(evidence)
    if not refs:
        print("REJECTED evidence_required (done needs --evidence kind:ref:digest)")
        return
    c = _conn()
    t = _team(c)
    if t is None:
        print("ERROR: no team roster (f11 not active)", file=sys.stderr)
        sys.exit(2)
    team_id, _budget, _members = t
    cur = c.execute(
        "UPDATE team_task SET status='done', evidence_refs=?, updated_at=? "
        "WHERE task_id=? AND team_id=? AND status='claimed' AND owner=?",
        (json.dumps(refs, ensure_ascii=False), time.time(), task_id, team_id,
         _TEAM_MEMBER))
    c.commit()
    print("OK" if cur.rowcount == 1 else "LOST (not claimed by you)")


def assert_write(text: str, evidence, confidence: float) -> None:
    refs = _parse_evidence_refs(evidence)
    if not refs:
        print("REJECTED evidence_required (assertions need --evidence)")
        return
    c = _conn()
    t = _team(c)
    if t is None:
        print("ERROR: no team roster (f11 not active)", file=sys.stderr)
        sys.exit(2)
    team_id, _budget, _members = t
    aid = f"as-{uuid.uuid4().hex[:10]}"
    c.execute(
        "INSERT INTO team_assertion "
        "(assertion_id, team_id, text, evidence_refs, confidence, status, "
        " source_seqs, created_at) VALUES (?,?,?,?,?,'active','[]',?)",
        (aid, team_id, text[:800], json.dumps(refs, ensure_ascii=False),
         float(confidence), time.time()))
    _team_event(c, "team_assertion_written",
                {"team_id": team_id, "assertion_id": aid, "text": text[:200]})
    c.commit()
    print(f"OK assertion_id={aid}")


def artifact_put(path: str) -> None:
    """Register a workspace artifact; prints the evidence_ref JSON to cite."""
    if not os.path.isfile(path):
        print(f"ERROR: no such file {path}", file=sys.stderr)
        sys.exit(2)
    with open(path, "rb") as fh:
        digest = hashlib.sha256(fh.read()).hexdigest()
    print(json.dumps({"kind": "artifact", "ref": path,
                      "digest": f"sha256:{digest}"}))


def token_wait(token_id: str, protocol: str, timeout: float) -> None:
    """Block until this member holds the turn token (T09); prints the fence
    credential to attach to token-gated writes."""
    c = _conn()
    t = _team(c)
    if t is None:
        print("TIMEOUT (no team roster)")
        return
    team_id, _budget, _members = t
    deadline = time.time() + max(0.5, float(timeout))
    while time.time() < deadline:
        if token_id:
            row = c.execute(
                "SELECT token_id, holder, fence, status FROM team_turn_token "
                "WHERE token_id=? AND team_id=?", (token_id, team_id)).fetchone()
        else:
            row = c.execute(
                "SELECT token_id, holder, fence, status FROM team_turn_token "
                "WHERE team_id=? AND protocol=? AND status='active' "
                "ORDER BY fence DESC LIMIT 1", (team_id, protocol)).fetchone()
        if row and row[3] == "active" and row[1] == _TEAM_MEMBER:
            print(f"GRANTED token_id={row[0]} fence={row[2]}")
            return
        time.sleep(0.5)
    print("TIMEOUT")


def heartbeat() -> None:
    """T12 liveness write (protocol frame; no msg_cap, no mailbox message)."""
    c = _conn()
    t = _team(c)
    if t is None:
        print("ERROR: no team roster (f11 not active)", file=sys.stderr)
        sys.exit(2)
    team_id, _budget, members = t
    now = time.time()
    found = False
    for m in members:
        if m.get("name") == _TEAM_MEMBER:
            m["last_heartbeat"] = now
            if m.get("state") in ("idle", "stalled"):
                m["state"] = "active"
            found = True
            break
    if not found:
        print(f"ERROR: {_TEAM_MEMBER} not on roster", file=sys.stderr)
        sys.exit(2)
    c.execute("UPDATE team_roster SET members_json=? WHERE team_id=?",
              (json.dumps(members, ensure_ascii=False), team_id))
    c.commit()
    print("OK")


def main() -> None:
    # Gate-0b: --mode=teammate registers ONLY the teammate whitelist — the
    # full-board read subcommands are not merely rejected, they do not exist.
    teammate_mode = "--mode=teammate" in sys.argv or any(
        a == "--mode" and i + 1 < len(sys.argv) and sys.argv[i + 1] == "teammate"
        for i, a in enumerate(sys.argv)
    )

    def _reg(name: str):
        if teammate_mode and name not in TEAMMATE_ALLOWED_CMDS:
            return None  # not registered in teammate mode
        return sub.add_parser(name)

    ap = argparse.ArgumentParser(prog="blackboard.py")
    ap.add_argument("--mode", default="", choices=("", "teammate", "lead"))
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = _reg("read-facts")
    if p is not None:
        p.add_argument("--verified-only", action="store_true")
    _reg("read-review")
    _reg("read-routes")
    _reg("read-branches")
    _reg("read-deadends")
    _reg("read-flags")
    p = _reg("submit-flag")
    if p is not None:
        p.add_argument("flag")
    _reg("list-intents")
    p = _reg("write-fact")
    if p is not None:
        p.add_argument("text")
        p.add_argument("--verified", action="store_true")
    p = _reg("mark-deadend")
    if p is not None:
        p.add_argument("reason")
    p = _reg("claim")
    if p is not None:
        p.add_argument("intent_id")
    if not teammate_mode:
        p = sub.add_parser("claim-activity")
        p.add_argument("key")
        sub.add_parser("list-activities")
    p = _reg("claim-resource")
    if p is not None:
        p.add_argument("resource_key")
        p.add_argument("--scope", default="activity")
        p.add_argument("--risk-class", default="")
    p = _reg("release-resource")
    if p is not None:
        p.add_argument("resource_key")
    _reg("read-resource-locks")
    if not teammate_mode:
        sub.add_parser("read-directives")
        p = sub.add_parser("directive-status")
        p.add_argument("directive_id")
        # f01 declared-effects (lazy: tables may be absent on production Swarm)
        sub.add_parser("read-declarations")
        p = sub.add_parser("write-outcome")
        p.add_argument("text", help="declared target_id (or note when only one target exists)")
        p.add_argument(
            "--state",
            choices=("satisfied", "unsatisfied", "indeterminate"),
            default="satisfied",
        )
        p.add_argument("--note", default="")
        # f02 world-model (lazy: tables may be absent)
        sub.add_parser("read-model")
        p = sub.add_parser("write-prediction")
        p.add_argument("text")
        p = sub.add_parser("write-observation")
        p.add_argument("text")
        # f03 solution-tree (lazy)
        sub.add_parser("read-tree")
        p = sub.add_parser("write-checkpoint")
        p.add_argument("checkpoint_id")
        # f04 qd-archive (lazy)
        sub.add_parser("read-archive")
        # f05 market (lazy)
        sub.add_parser("market-list")
        # f06 case-bank (lazy)
        sub.add_parser("read-cases")
        # f07–f10 lazy readouts
        sub.add_parser("read-slow-tree")
        sub.add_parser("read-lineages")
        sub.add_parser("read-epistemic")
        sub.add_parser("read-edge-shells")
    # f11 agent-teams (teammate whitelist; always registered outside teammate
    # mode too, so lead/coordinator tooling can inspect)
    p = sub.add_parser("msg-send")
    p.add_argument("--kind", default="direct")
    p.add_argument("--to", default="", help="comma-separated members, or *")
    p.add_argument("--body", default="")
    p.add_argument("--verbatim", action="append", default=[])
    p.add_argument("--evidence", action="append", default=[],
                   help="kind:ref:digest (repeatable)")
    p.add_argument("--hop", type=int, default=0)
    p.add_argument("--require-ack", action="store_true")
    p.add_argument("--thread", default=None)
    p.add_argument("--channel-kind", default=None)
    p.add_argument("--ack-of", default=None)
    p = sub.add_parser("msg-check")
    p.add_argument("--after-seq", type=int, default=0)
    p.add_argument("--digest", action="store_true",
                   help="pull the latest channel digest instead of raw messages")
    p = sub.add_parser("task-list")
    p.add_argument("--status", default="")
    p = sub.add_parser("task-claim")
    p.add_argument("task_id")
    p.add_argument("--token-id", default="")
    p.add_argument("--token-fence", type=int, default=-1)
    p = sub.add_parser("task-done")
    p.add_argument("task_id")
    p.add_argument("--evidence", action="append", default=[])
    p = sub.add_parser("assert-write")
    p.add_argument("text")
    p.add_argument("--evidence", action="append", default=[])
    p.add_argument("--confidence", type=float, default=0.7)
    p = sub.add_parser("artifact-put")
    p.add_argument("path")
    p = sub.add_parser("token-wait")
    p.add_argument("--token-id", default="")
    p.add_argument("--protocol", default="")
    p.add_argument("--timeout", type=float, default=60.0)
    sub.add_parser("heartbeat")
    args = ap.parse_args()

    if args.cmd == "read-facts":
        read_facts(args.verified_only)
    elif args.cmd == "read-review":
        read_review()
    elif args.cmd == "read-routes":
        read_routes()
    elif args.cmd == "read-branches":
        read_branches()
    elif args.cmd == "read-deadends":
        read_deadends()
    elif args.cmd == "read-flags":
        read_flags()
    elif args.cmd == "submit-flag":
        submit_flag(args.flag)
    elif args.cmd == "list-intents":
        list_intents()
    elif args.cmd == "write-fact":
        write_fact(args.text, args.verified)
    elif args.cmd == "mark-deadend":
        mark_deadend(args.reason)
    elif args.cmd == "claim":
        claim(args.intent_id)
    elif args.cmd == "claim-activity":
        claim_activity(args.key)
    elif args.cmd == "list-activities":
        list_activities()
    elif args.cmd == "claim-resource":
        claim_resource(args.resource_key, scope=args.scope, risk_class=args.risk_class)
    elif args.cmd == "release-resource":
        release_resource(args.resource_key)
    elif args.cmd == "read-resource-locks":
        read_resource_locks()
    elif args.cmd == "read-directives":
        read_directives()
    elif args.cmd == "directive-status":
        directive_status(args.directive_id)
    elif args.cmd == "read-declarations":
        read_declarations()
    elif args.cmd == "write-outcome":
        write_outcome(args.text, state=args.state, note=args.note)
    elif args.cmd == "read-model":
        read_model()
    elif args.cmd == "write-prediction":
        write_prediction(args.text)
    elif args.cmd == "write-observation":
        write_observation(args.text)
    elif args.cmd == "read-tree":
        read_tree()
    elif args.cmd == "write-checkpoint":
        write_checkpoint(args.checkpoint_id)
    elif args.cmd == "read-archive":
        read_archive()
    elif args.cmd == "market-list":
        market_list()
    elif args.cmd == "read-cases":
        read_cases()
    elif args.cmd == "read-slow-tree":
        read_slow_tree()
    elif args.cmd == "read-lineages":
        read_lineages()
    elif args.cmd == "read-epistemic":
        read_epistemic()
    elif args.cmd == "read-edge-shells":
        read_edge_shells()
    # f11 agent-teams team subcommands
    elif args.cmd == "msg-send":
        msg_send(args.kind, [t for t in str(args.to).split(",") if t],
                 args.body, args.verbatim, args.evidence, args.hop,
                 args.require_ack, args.thread, args.channel_kind, args.ack_of)
    elif args.cmd == "msg-check":
        msg_check(args.after_seq, args.digest)
    elif args.cmd == "task-list":
        task_list(args.status)
    elif args.cmd == "task-claim":
        task_claim(args.task_id, token_id=args.token_id,
                   token_fence=args.token_fence)
    elif args.cmd == "task-done":
        task_done(args.task_id, args.evidence)
    elif args.cmd == "assert-write":
        assert_write(args.text, args.evidence, args.confidence)
    elif args.cmd == "artifact-put":
        artifact_put(args.path)
    elif args.cmd == "token-wait":
        token_wait(args.token_id, args.protocol, args.timeout)
    elif args.cmd == "heartbeat":
        heartbeat()


if __name__ == "__main__":
    main()
