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
  blackboard.py claim <intent_id>              # atomic; prints WON or LOST

This script is intentionally dependency-free (stdlib sqlite3 only) so it runs in
any worker container without setup.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sqlite3
import sys
import time

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


def main() -> None:
    ap = argparse.ArgumentParser(prog="blackboard.py")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("read-facts")
    p.add_argument("--verified-only", action="store_true")
    sub.add_parser("read-review")
    sub.add_parser("read-routes")
    sub.add_parser("read-branches")
    sub.add_parser("read-deadends")
    sub.add_parser("read-flags")
    sub.add_parser("list-intents")
    p = sub.add_parser("write-fact")
    p.add_argument("text")
    p.add_argument("--verified", action="store_true")
    p = sub.add_parser("mark-deadend")
    p.add_argument("reason")
    p = sub.add_parser("claim")
    p.add_argument("intent_id")
    p = sub.add_parser("claim-activity")
    p.add_argument("key")
    sub.add_parser("list-activities")
    p = sub.add_parser("claim-resource")
    p.add_argument("resource_key")
    p.add_argument("--scope", default="activity")
    p.add_argument("--risk-class", default="")
    p = sub.add_parser("release-resource")
    p.add_argument("resource_key")
    sub.add_parser("read-resource-locks")
    sub.add_parser("read-directives")
    p = sub.add_parser("directive-status")
    p.add_argument("directive_id")
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


if __name__ == "__main__":
    main()
