#!/usr/bin/env python3
"""muteki-blackboard — a worker's CLI to the shared solve graph (the blackboard).

A swarm worker (claude / codex) calls this to coordinate with its teammates
through the shared, append-only SQLite blackboard — NOT by talking to them
directly (stigmergy). The board holds:
  - facts      : confirmed, objective findings (with verified/candidate status)
  - dead-ends  : ruled-out directions (so nobody retries them)
  - intents    : declared exploration directions, claimable atomically

The DB path comes from $MUTEKI_BLACKBOARD_DB. It is provided only to the
post-solve respond path; ordinary role-scoped workers do not get a raw graph path.

An ordinary worker never WRITES the DB: commands publish structured requests
into $MUTEKI_BLACKBOARD_INGRESS_DIR and the owning host validates + applies them;
its initial graph context is already present in the prompt.

Ordinary Worker usage:
  blackboard.py context                        # complete role-scoped live view
  blackboard.py submit-fact "<title>" "<content>" # overwriteable draft
  blackboard.py commit-step                    # publish draft and finish Step
  blackboard.py submit-flag '<flag>'             # the only Flag submission API

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
_TARGET_EPOCH = os.environ.get("MUTEKI_TARGET_EPOCH", "1").strip() or "1"
_AUTONOMOUS_PROFILE = (
    os.environ.get("MUTEKI_BLACKBOARD_PROFILE", "").strip().casefold()
    == "autonomous"
)
_ROLE = os.environ.get("MUTEKI_BLACKBOARD_ROLE", "solve").strip().casefold() or "solve"
_CHALLENGE_MODE = (
    os.environ.get("MUTEKI_CHALLENGE_MODE", "ctf").strip().casefold() or "ctf"
)

_SOLVE_COMMANDS = (
    {"context", "read-artifact", "submit-fact", "commit-step", "mark-deadend", "request-input", "save-poc"}
    | ({"recent-evidence", "submit-report"} if _CHALLENGE_MODE == "pentest" else set())
    | ({"submit-flag"} if _CHALLENGE_MODE == "ctf" else set())
    if _CHALLENGE_MODE in {"ctf", "pentest"}
    else {"context", "write-fact", "mark-deadend", "request-input", "save-poc", "submit-flag"}
)

_ROLE_COMMANDS = {
    "solve": _SOLVE_COMMANDS,
    "verifier": (
        {"context", "read-artifact", "submit-fact", "commit-step", "mark-deadend", "save-poc"}
        | ({"recent-evidence", "submit-report"} if _CHALLENGE_MODE == "pentest" else set())
        if _CHALLENGE_MODE in {"ctf", "pentest"}
        else {"context", "write-fact", "mark-deadend", "save-poc"}
    ),
    "review": {
        "context", "review-finding", "challenge-fact", "merge-fact",
        "reject-fact", "revalidate-fact",
    },
    "respond": {"context"},
}
for _commands in (_ROLE_COMMANDS["solve"], _ROLE_COMMANDS["verifier"], _ROLE_COMMANDS["review"]):
    _commands.update({"mcp-tools", "mcp-schema", "mcp-call"})


def _db_path() -> str:
    p = os.environ.get("MUTEKI_BLACKBOARD_DB", "")
    if not p:
        print("ERROR: this Worker role has no raw blackboard DB capability",
              file=sys.stderr)
        sys.exit(2)
    return p


def _conn() -> sqlite3.Connection:
    # Post-solve respond receives read-only board access.
    path = os.path.abspath(_db_path())
    c = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=10)
    c.execute("PRAGMA query_only=ON")
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
        if f is None:
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
    _submit_request("submit_flag", {"flag": str(flag)})


def submit_fact(title: str, content: str, evidence: str = "") -> None:
    _submit_request("submit_fact", {
        "title": str(title),
        "content": str(content),
        "evidence_artifact_id": str(evidence or ""),
    })


def submit_report(path: str) -> None:
    try:
        with open(path, "r", encoding="utf-8") as handle:
            report = json.load(handle)
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        print(f"ERROR: cannot read report JSON file: {exc}", file=sys.stderr)
        sys.exit(2)
    if not isinstance(report, dict):
        print("ERROR: report file must contain one JSON object", file=sys.stderr)
        sys.exit(2)
    _submit_request("submit_report", {"report": report})


def commit_step() -> None:
    _submit_request("commit_step", {})


def _ingress_dir() -> str:
    return os.environ.get("MUTEKI_BLACKBOARD_INGRESS_DIR", "").strip()


def _write_request(operation: str, payload: dict) -> str:
    """Publish one structured request for the owning host to validate and apply."""
    request_dir = _ingress_dir()
    if not request_dir:
        print("ERROR: no Blackboard ingress ($MUTEKI_BLACKBOARD_INGRESS_DIR unset)",
              file=sys.stderr)
        sys.exit(2)
    os.makedirs(request_dir, mode=0o700, exist_ok=True)
    request_id = f"br-{uuid.uuid4().hex[:16]}"
    body = {
        "protocol": "muteki-blackboard-v2",
        "request_id": request_id,
        "operation": operation,
        "created_at": time.time(),
    }
    body.update(payload)
    fd, temporary = tempfile.mkstemp(
        prefix=f".{request_id}-", suffix=".tmp", dir=request_dir)
    final_path = os.path.join(
        request_dir, f"request-{operation}-{request_id}.json")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(body, handle, ensure_ascii=False, separators=(",", ":"))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, final_path)
    except BaseException:
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise
    return request_id


def _await_request_result(request_id: str, timeout_s: float = 30.0) -> dict | None:
    result_path = os.path.join(_ingress_dir(), f"result-{request_id}.json")
    deadline = time.time() + timeout_s
    while True:
        try:
            with open(result_path, "r", encoding="utf-8") as handle:
                result = json.load(handle)
            if isinstance(result, dict):
                return result
        except FileNotFoundError:
            pass
        except (OSError, ValueError):
            pass  # partially written — retry next tick
        if time.time() >= deadline:
            return None
        time.sleep(0.2)


def _request_timeout(timeout_s: float = 30.0) -> None:
    print(f"ERROR: host did not answer the Blackboard request within {timeout_s:g}s",
          file=sys.stderr)
    sys.exit(3)


def _submit_request(operation: str, payload: dict, *, timeout_s: float = 30.0) -> dict:
    request_id = _write_request(operation, payload)
    result = _await_request_result(request_id, timeout_s=timeout_s)
    if result is None:
        _request_timeout(timeout_s)
    message = str(result.get("message") or result.get("detail") or "")
    if not result.get("ok"):
        print(f"REJECTED{(': ' + message) if message else ''}", file=sys.stderr)
        sys.exit(2)
    print(message or "OK")
    return result


def _print_claim_verdict(result: dict | None) -> None:
    if result is None:
        _request_timeout()
    print("WON" if result.get("won") else "LOST")


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



def write_fact(text: str, verified: bool, witness: str = "", *,
               subject: str = "", predicate: str = "", object_value=None,
               scope: str = "", canonical_key: str = "",
               capability_key: str = "", capability_kind: str = "generic",
               capability_quality: str = "",
               capability_sharing: str = "run-shared") -> None:
    _submit_request("fact", {"text": text})
    return


def report_capability_gap(description: str, required: list[str],
                          consumers: list[str]) -> None:
    _submit_request("report_capability_gap", {
        "description": description, "required_capabilities": required,
        "consumers": consumers,
    })


def publish_capability(key: str, kind: str, quality: str, sharing: str,
                       evidence: list[int], metadata_json: str) -> None:
    metadata = json.loads(metadata_json) if metadata_json else {}
    _submit_request("publish_capability", {
        "capability_key": key, "kind": kind, "quality": quality,
        "sharing": sharing, "evidence_fact_seqs": evidence, "metadata": metadata,
    })


def launch_runtime_resource(name: str, command: str, allocate_port: bool,
                            cleanup_command: str = "") -> None:
    _submit_request("launch_runtime_resource", {
        "name": name, "command": command, "allocate_port": allocate_port,
        "cleanup_command": cleanup_command,
    })


def publish_access_path(resource_id: str, reach: list[str], operations: list[str],
                        quality: str, endpoint: str, use_env: list[str],
                        health_host: str, health_port: int,
                        dependency_facts: list[int], capabilities: list[str]) -> None:
    env = {}
    for item in use_env:
        key, sep, value = str(item).partition("=")
        if sep and key.strip():
            env[key.strip()] = value
    health = ({"kind": "tcp", "host": health_host, "port": health_port}
              if health_port > 0 else {})
    _submit_request("publish_access_path", {
        "runtime_resource_id": resource_id, "reach": reach,
        "operations": operations, "quality": quality, "endpoint": endpoint,
        "use_spec": {"env": env}, "health": health,
        "dependency_fact_seqs": dependency_facts,
        "capability_keys": capabilities,
    })


def mark_deadend(reason: str, tested_scope: str = "",
                 observed_result: str = "") -> None:
    _submit_request("dead_end", {
        "reason": reason,
        "tested_scope": tested_scope,
        "observed_result": observed_result,
    })
    return


def request_input(need: str) -> None:
    _submit_request("need_input", {"need": need})


def save_poc(path: str, entry_command: str, status: str, note: str) -> None:
    _submit_request("save_poc", {
        "path": path,
        "entry_command": entry_command,
        "status": status,
        "note": note,
    })


def propose_branch(goal: str, expected_observable: str,
                   stop_condition: str, coverage_key: str,
                   route_hash: str, lane_key: str = "",
                   risk_class: str = "", resource_key: str = "") -> None:
    _submit_request("branch_proposal", {
        "goal": goal,
        "expected_observable": expected_observable,
        "stop_condition": stop_condition,
        "coverage_key": coverage_key,
        "route_hash": route_hash,
        "lane_key": lane_key,
        "risk_class": risk_class,
        "resource_key": resource_key,
    })


def review_finding(kind: str, severity: str, summary: str,
                   recommended_actions: list[str]) -> None:
    _submit_request("review_finding", {
        "kind": kind,
        "severity": severity,
        "summary": summary,
        "recommended_actions": recommended_actions,
    })


def challenge_fact(fact_seq: int, reason: str, verification_goal: str) -> None:
    _submit_request("fact_challenge", {
        "fact_seq": fact_seq,
        "reason": reason,
        "verification_goal": verification_goal,
    })


def merge_fact(from_fact_seq: int, to_fact_seq: int, reason: str) -> None:
    _submit_request("fact_merge", {
        "from_fact_seq": from_fact_seq,
        "to_fact_seq": to_fact_seq,
        "reason": reason,
    })


def reject_fact(fact_seq: int, reason: str) -> None:
    _submit_request("fact_reject", {"fact_seq": fact_seq, "reason": reason})


def revalidate_fact(fact_seq: int, reason: str) -> None:
    _submit_request("fact_revalidation", {
        "fact_seq": fact_seq,
        "reason": reason,
    })


def refresh_context() -> None:
    request_id = _write_request("context", {})
    result = _await_request_result(request_id)
    if result is None:
        _request_timeout()
    if not result.get("ok"):
        print(str(result.get("detail") or "context unavailable"), file=sys.stderr)
        sys.exit(2)
    print(str(result.get("content") or "(no scoped context available)"))


def read_artifact(artifact_id: str) -> None:
    request_id = _write_request("read_artifact", {"artifact_id": artifact_id})
    result = _await_request_result(request_id)
    if result is None:
        _request_timeout()
    if not result.get("ok"):
        print(str(result.get("detail") or "artifact unavailable"), file=sys.stderr)
        sys.exit(2)
    sys.stdout.write(str(result.get("content") or ""))


def recent_evidence() -> None:
    request_id = _write_request("recent_evidence", {})
    result = _await_request_result(request_id)
    if result is None:
        _request_timeout()
    if not result.get("ok"):
        print(str(result.get("detail") or "evidence unavailable"), file=sys.stderr)
        sys.exit(2)
    sys.stdout.write(str(result.get("content") or "[]"))


def submission_lock(action: str, note: str) -> None:
    request_id = _write_request(
        "submission_lock", {"action": action, "note": note})
    result = _await_request_result(request_id)
    if result is None:
        _request_timeout()
    if not result.get("ok"):
        message = str(result.get("message") or result.get("detail") or "")
        print(f"REJECTED{(': ' + message) if message else ''}", file=sys.stderr)
        sys.exit(2)
    print("WON" if result.get("won") else "LOST")


def claim(intent_id: str) -> None:
    claim_id = _write_request("claim_intent", {"intent_id": intent_id})
    _print_claim_verdict(_await_request_result(claim_id))
    return


def _norm_activity_key(key: str) -> str:
    import re
    k = (key or "").strip().lower()
    k = re.sub(r"[\s/]+", ":", k)
    k = re.sub(r":+", ":", k).strip(":")
    return k


def claim_activity(key: str, lease_s: float = 600.0) -> None:
    """P4: claim a high-cost activity (e.g. 'nmap:8.130.96.176'). WON = go ahead;
    LOST = a teammate is already doing it, AVOID redoing."""
    claim_id = _write_request("claim_activity", {"key": key})
    _print_claim_verdict(_await_request_result(claim_id))
    return


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
    claim_id = _write_request("claim_resource", {
        "resource_key": resource_key, "scope": scope,
        "risk_class": risk_class})
    _print_claim_verdict(_await_request_result(claim_id))
    return


def release_resource(resource_key: str) -> None:
    """E: release a resource lock this worker holds (owner-fenced, best-effort)."""
    claim_id = _write_request("release_resource",
                            {"resource_key": resource_key})
    result = _await_request_result(claim_id)
    if result is None:
        _request_timeout()
    print("OK" if result.get("ok") else "LOST")
    return


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


def _read_guard(fn, *args) -> None:
    """Ordinary-mode board reads degrade gracefully: an unreadable DB prints a
    stderr note and empty output instead of crashing the worker's turn."""
    try:
        fn(*args)
    except sqlite3.OperationalError as exc:
        print(f"(blackboard read unavailable: {exc})", file=sys.stderr)


def main() -> None:
    def _reg(name: str):
        if name not in _ROLE_COMMANDS.get(_ROLE, {"context"}):
            return None
        return sub.add_parser(name)

    ap = argparse.ArgumentParser(prog="blackboard.py")
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
    p = _reg("submit-fact")
    if p is not None:
        p.add_argument("title")
        p.add_argument("content")
        if _CHALLENGE_MODE == "pentest":
            p.add_argument("--evidence", required=True,
                           help="artifact ID from recent-evidence supporting this Fact")
    p = _reg("submit-report")
    if p is not None:
        p.add_argument("json_file", help=(
            "JSON file containing one vulnerability report; evidence_note must include "
            "artifact_id from submit-fact --evidence, observed, and significance"
        ))
    _reg("commit-step")
    _reg("mcp-tools")
    p = _reg("mcp-schema")
    if p is not None:
        p.add_argument("name")
    p = _reg("mcp-call")
    if p is not None:
        p.add_argument("name")
        p.add_argument("json_file", help="JSON file containing tool arguments")
    p = _reg("mark-deadend")
    if p is not None:
        p.add_argument("reason")
        p.add_argument("--tested", default="")
        p.add_argument("--observed", default="")
    if not _AUTONOMOUS_PROFILE:
        p = _reg("request-input")
        if p is not None:
            p.add_argument("need")
    p = _reg("save-poc")
    if p is not None:
        p.add_argument("path")
        p.add_argument("--entry-command", default="")
        p.add_argument(
            "--status", choices=("available", "wip", "directional", "spent"),
            default="available")
        p.add_argument("--note", default="")
    p = _reg("propose-branch")
    if p is not None:
        p.add_argument("goal")
        p.add_argument("--expected-observable", required=True)
        p.add_argument("--stop-condition", required=True)
        p.add_argument("--coverage-key", default="")
        p.add_argument("--route-hash", default="")
        p.add_argument("--lane-key", default="")
        p.add_argument("--risk-class", default="")
        p.add_argument("--resource-key", default="")
    p = _reg("review-finding")
    if p is not None:
        p.add_argument("--kind", required=True)
        p.add_argument("--severity", default="info")
        p.add_argument("--summary", required=True)
        p.add_argument("--recommended-action", action="append", default=[])
    p = _reg("challenge-fact")
    if p is not None:
        p.add_argument("fact_seq", type=int)
        p.add_argument("--reason", required=True)
        p.add_argument("--verification-goal", required=True)
    p = _reg("merge-fact")
    if p is not None:
        p.add_argument("from_fact_seq", type=int)
        p.add_argument("to_fact_seq", type=int)
        p.add_argument("--reason", required=True)
    p = _reg("reject-fact")
    if p is not None:
        p.add_argument("fact_seq", type=int)
        p.add_argument("--reason", required=True)
    p = _reg("revalidate-fact")
    if p is not None:
        p.add_argument("fact_seq", type=int)
        p.add_argument("--reason", required=True)
    _reg("context")
    _reg("recent-evidence")
    p = _reg("read-artifact")
    if p is not None:
        p.add_argument("artifact_id")
    p = _reg("submission-lock")
    if p is not None:
        p.add_argument("action", choices=("acquire", "release"))
        p.add_argument("--note", default="")
    p = _reg("claim")
    if p is not None:
        p.add_argument("intent_id")
    p = _reg("claim-activity")
    if p is not None:
        p.add_argument("key")
    p = _reg("claim-resource")
    if p is not None:
        p.add_argument("resource_key")
        p.add_argument("--scope", default="activity")
        p.add_argument("--risk-class", default="")
    p = _reg("release-resource")
    if p is not None:
        p.add_argument("resource_key")
    p = _reg("report-capability-gap")
    if p is not None:
        p.add_argument("description")
        p.add_argument("--requires", action="append", default=[])
        p.add_argument("--consumer", action="append", default=[])
    p = _reg("publish-capability")
    if p is not None:
        p.add_argument("capability_key")
        p.add_argument("--kind", default="generic")
        p.add_argument("--quality", default="")
        p.add_argument("--sharing", default="run-shared")
        p.add_argument("--fact", action="append", type=int, default=[])
        p.add_argument("--metadata-json", default="{}")
    p = _reg("launch-runtime-resource")
    if p is not None:
        p.add_argument("name")
        p.add_argument("--command", required=True)
        p.add_argument("--allocate-port", action="store_true")
        p.add_argument("--cleanup-command", default="")
    p = _reg("publish-access-path")
    if p is not None:
        p.add_argument("runtime_resource_id")
        p.add_argument("--reach", action="append", default=[])
        p.add_argument("--operation", action="append", default=[])
        p.add_argument("--quality", default="multiplexed")
        p.add_argument("--endpoint", default="")
        p.add_argument("--use-env", action="append", default=[])
        p.add_argument("--health-host", default="127.0.0.1")
        p.add_argument("--health-port", type=int, default=0)
        p.add_argument("--fact", action="append", type=int, default=[])
        p.add_argument("--capability", action="append", default=[])
    _reg("read-resource-locks")
    args = ap.parse_args()

    if args.cmd == "read-facts":
        _read_guard(read_facts, args.verified_only)
    elif args.cmd == "read-review":
        _read_guard(read_review)
    elif args.cmd == "read-routes":
        _read_guard(read_routes)
    elif args.cmd == "read-branches":
        _read_guard(read_branches)
    elif args.cmd == "read-deadends":
        _read_guard(read_deadends)
    elif args.cmd == "read-flags":
        _read_guard(read_flags)
    elif args.cmd == "submit-flag":
        submit_flag(args.flag)
    elif args.cmd == "list-intents":
        _read_guard(list_intents)
    elif args.cmd == "write-fact":
        write_fact(args.text, False)
    elif args.cmd == "submit-fact":
        submit_fact(args.title, args.content, getattr(args, "evidence", ""))
    elif args.cmd == "submit-report":
        submit_report(args.json_file)
    elif args.cmd == "commit-step":
        commit_step()
    elif args.cmd == "mcp-tools":
        _submit_request("mcp_tools", {}, timeout_s=120)
    elif args.cmd == "mcp-schema":
        _submit_request("mcp_schema", {"name": args.name}, timeout_s=120)
    elif args.cmd == "mcp-call":
        with open(args.json_file, "r", encoding="utf-8") as handle:
            arguments = json.load(handle)
        if not isinstance(arguments, dict):
            print("ERROR: MCP arguments file must contain one JSON object", file=sys.stderr)
            sys.exit(2)
        _submit_request("mcp_call", {"name": args.name, "arguments": arguments}, timeout_s=120)
    elif args.cmd == "mark-deadend":
        mark_deadend(args.reason, args.tested, args.observed)
    elif args.cmd == "request-input":
        request_input(args.need)
    elif args.cmd == "save-poc":
        save_poc(args.path, args.entry_command, args.status, args.note)
    elif args.cmd == "propose-branch":
        propose_branch(
            args.goal, args.expected_observable,
            args.stop_condition, args.coverage_key, args.route_hash,
            args.lane_key, args.risk_class, args.resource_key)
    elif args.cmd == "review-finding":
        review_finding(
            args.kind, args.severity, args.summary, args.recommended_action)
    elif args.cmd == "challenge-fact":
        challenge_fact(args.fact_seq, args.reason, args.verification_goal)
    elif args.cmd == "merge-fact":
        merge_fact(args.from_fact_seq, args.to_fact_seq, args.reason)
    elif args.cmd == "reject-fact":
        reject_fact(args.fact_seq, args.reason)
    elif args.cmd == "revalidate-fact":
        revalidate_fact(args.fact_seq, args.reason)
    elif args.cmd == "context":
        refresh_context()
    elif args.cmd == "recent-evidence":
        recent_evidence()
    elif args.cmd == "read-artifact":
        read_artifact(args.artifact_id)
    elif args.cmd == "submission-lock":
        submission_lock(args.action, args.note)
    elif args.cmd == "claim":
        claim(args.intent_id)
    elif args.cmd == "claim-activity":
        claim_activity(args.key)
    elif args.cmd == "list-activities":
        _read_guard(list_activities)
    elif args.cmd == "claim-resource":
        claim_resource(args.resource_key, scope=args.scope, risk_class=args.risk_class)
    elif args.cmd == "release-resource":
        release_resource(args.resource_key)
    elif args.cmd == "report-capability-gap":
        report_capability_gap(args.description, args.requires, args.consumer)
    elif args.cmd == "publish-capability":
        publish_capability(args.capability_key, args.kind, args.quality,
                           args.sharing, args.fact, args.metadata_json)
    elif args.cmd == "launch-runtime-resource":
        launch_runtime_resource(
            args.name, args.command, args.allocate_port, args.cleanup_command)
    elif args.cmd == "publish-access-path":
        publish_access_path(
            args.runtime_resource_id, args.reach, args.operation, args.quality,
            args.endpoint, args.use_env, args.health_host, args.health_port,
            args.fact, args.capability,
        )
    elif args.cmd == "read-resource-locks":
        _read_guard(read_resource_locks)
    elif args.cmd == "read-directives":
        _read_guard(read_directives)
    elif args.cmd == "directive-status":
        _read_guard(directive_status, args.directive_id)


if __name__ == "__main__":
    main()
