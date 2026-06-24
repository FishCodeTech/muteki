"""The muteki-blackboard skill's CLI (blackboard.py) round-trips against a real
SQLiteSharedGraph DB: read facts/dead-ends, write fact, mark dead-end, claim intent.

This is what a swarm worker (claude/codex) actually runs inside its container to
coordinate through the shared board (stigmergy). We drive it as a subprocess with
MUTEKI_BLACKBOARD_DB pointed at a freshly-built graph, exactly like a worker.
"""
from __future__ import annotations

import os
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

from muteki.models.solve_graph import Challenge
from muteki.solver.cli_solver import CliSolver
from muteki.swarm.shared_graph import SQLiteSharedGraph, _normalize_fact_identity

_SKILL = Path(__file__).resolve().parents[1] / "skills" / "muteki-blackboard" / "blackboard.py"


def _board(tmp_path):
    ch = Challenge(id="c1", name="t", category="web")
    return SQLiteSharedGraph.open(db_path=tmp_path / "shared_graph.db", challenge=ch)


def _run(db, *args, worker="cli-claude", intent_id=""):
    env = {**os.environ, "MUTEKI_BLACKBOARD_DB": str(db), "MUTEKI_WORKER_ID": worker}
    if intent_id:
        env["MUTEKI_INTENT_ID"] = intent_id
    r = subprocess.run([sys.executable, str(_SKILL), *args],
                       capture_output=True, text=True, env=env, timeout=30)
    assert r.returncode == 0, f"blackboard.py {args} failed: {r.stderr}"
    return r.stdout


def test_skill_file_exists():
    assert _SKILL.exists(), "blackboard.py skill script missing"
    skill_md = _SKILL.parent / "SKILL.md"
    assert skill_md.exists()
    text = skill_md.read_text()
    assert "muteki-blackboard" in text and "read-deadends" in text
    assert "read-review" in text


def test_read_empty_board(tmp_path):
    g = _board(tmp_path)
    db = g.db_path
    g.close()
    assert "no facts" in _run(db, "read-facts").lower()
    assert "no dead-ends" in _run(db, "read-deadends").lower()


def test_write_fact_then_read(tmp_path):
    g = _board(tmp_path)
    db = g.db_path
    g.close()
    _run(db, "write-fact", "admin:admin works on /login", "--verified")
    out = _run(db, "read-facts")
    assert "admin:admin works on /login" in out
    assert "VERIFIED" in out
    # verified-only filter still shows it
    assert "admin:admin" in _run(db, "read-facts", "--verified-only")


def test_candidate_fact_excluded_by_verified_only(tmp_path):
    g = _board(tmp_path)
    db = g.db_path
    g.close()
    _run(db, "write-fact", "maybe an IDOR on /api/user")  # no --verified
    assert "maybe an IDOR" in _run(db, "read-facts")
    assert "maybe an IDOR" not in _run(db, "read-facts", "--verified-only")


def test_mark_and_read_deadend(tmp_path):
    g = _board(tmp_path)
    db = g.db_path
    g.close()
    _run(db, "mark-deadend", "no SQLi on /search — parameterized")
    out = _run(db, "read-deadends")
    assert "no SQLi on /search" in out
    assert "DO NOT retry" in out


def test_claim_intent_won_then_lost(tmp_path):
    g = _board(tmp_path)
    db = g.db_path
    # propose an intent via the graph API (the coordinator does this)
    g.propose_intent(actor="reason", intent_id="I1", goal="try default creds")
    g.close()
    # first worker wins
    assert "WON" in _run(db, "claim", "I1", worker="cli-claude")
    # second worker loses (already claimed, lease valid)
    assert "LOST" in _run(db, "claim", "I1", worker="cli-codex")


def test_list_intents(tmp_path):
    g = _board(tmp_path)
    db = g.db_path
    g.propose_intent(
        actor="reason", intent_id="I7", goal="decode the JWT",
        payload={"worker_class": "verifier", "route_hash": "web:jwt"},
    )
    g.close()
    out = _run(db, "list-intents")
    assert "I7" in out and "decode the JWT" in out
    assert "class=verifier" in out and "route=web:jwt" in out


def test_read_review_state(tmp_path):
    g = _board(tmp_path)
    db = g.db_path
    fseq = g.add_evidence(actor="cli-a", source="curl", fact="JWT uses HS256",
                          verified=True, artifact_id="a1")
    g.add_review_finding(actor="reviewer", kind="route_loop", severity="blocker",
                         summary="login SQLi repeated", route_hash="web:login:sqli")
    g.challenge_fact(actor="reviewer", fact_seq=fseq, reason="no raw JWT header",
                     verification_goal="Decode a real JWT header.")
    g.suppress_route(actor="reviewer", route_hash="web:login:sqli",
                     label="login SQLi", reason="three repeated dead ends")
    g.split_branch(actor="reviewer", title="CRM branch", branches=[
        {"id": "crm-public", "assumption": "public CRM reachable",
         "prove_or_disprove": "curl /admin from current pivot"},
    ])
    g.add_coordinator_directive(actor="reviewer", action="rebootstrap",
                                directive="Stop repeating login SQLi.")
    g.close()

    out = _run(db, "read-review")
    assert "login SQLi repeated" in out
    assert "Challenged facts" in out and "JWT uses HS256" in out
    assert "web:login:sqli" in out
    assert "CRM branch" in out
    assert "Stop repeating login SQLi" in out

    assert "SUPPRESSED" in _run(db, "read-routes")
    assert "crm-public" in _run(db, "read-branches")


def test_claim_succeeds_despite_empty_challenge_id_event(tmp_path):
    """run-7349 regression: the DB had an event with an EMPTY challenge_id, and
    _challenge_id used `SELECT challenge_id FROM events LIMIT 1` — which could grab
    that empty row, making claim's `WHERE challenge_id=?` match nothing → an open
    intent always returned LOST (so no worker could pick up Reason's intents). The
    fix skips empty challenge_ids; claim must WON here."""
    import sqlite3
    g = _board(tmp_path)
    db = g.db_path
    g.close()
    # The empty-challenge_id event must come FIRST (lowest rowid) so a naive
    # `SELECT challenge_id FROM events LIMIT 1` returns "" — that's the exact shape
    # of run-7349's DB. Insert it before the real intent.
    conn = sqlite3.connect(db)
    conn.execute(
        "INSERT INTO events (ts, challenge_id, actor, kind, payload, verified, "
        "confidence) VALUES (0, '', 'coordinator', 'note', '{}', 0, 1.0)")
    conn.commit()
    conn.close()
    # sanity: the naive query the old code used would indeed return "" here
    conn = sqlite3.connect(db)
    naive = conn.execute("SELECT challenge_id FROM events LIMIT 1").fetchone()[0]
    conn.close()
    assert naive == "", "test must reproduce the empty-challenge_id-first condition"
    # now propose the intent (challenge_id = c1) and claim it via the skill
    g = SQLiteSharedGraph.open(db_path=db,
                               challenge=Challenge(id="c1", name="t", category="web"))
    g.propose_intent(actor="reason", intent_id="I1", goal="enumerate redis")
    g.close()
    assert "WON" in _run(db, "claim", "I1", worker="cli-claude")


def test_fact_written_by_skill_is_visible_to_graph(tmp_path):
    """A fact the worker writes via the skill must be readable through the graph
    API (so Reason's to_summary sees it)."""
    g = _board(tmp_path)
    db = g.db_path
    g.close()
    _run(db, "write-fact", "service is nginx 1.18.0", "--verified")
    # reopen the graph and check the materialized view
    ch = Challenge(id="c1", name="t", category="web")
    g2 = SQLiteSharedGraph.open(db_path=db, challenge=ch)
    summary = g2.to_summary()
    g2.close()
    assert "nginx 1.18.0" in summary


def test_skill_write_fact_links_product_to_current_intent(tmp_path):
    g = _board(tmp_path)
    db = g.db_path
    g.propose_intent(actor="reason", intent_id="I-skill", goal="enumerate /admin")
    g.claim_intent(worker="cli-claude", intent_id="I-skill")
    g.close()

    _run(db, "write-fact", "admin panel exists", "--verified", intent_id="I-skill")

    g2 = SQLiteSharedGraph.open(db_path=db, challenge=Challenge(id="c1", name="t", category="web"))
    products = g2.intent_products("I-skill")
    g2.close()
    assert len(products) == 1


# ── dedupe_key parity: skill ↔ shared_graph._normalize_fact_identity ──────────
#
# The write-time fact dedupe lives in TWO places that MUST stay in lockstep: the
# coordinator's SQLiteSharedGraph.add_evidence (keyed off _normalize_fact_identity)
# and the standalone skill's write_fact. run-75378 showed what happens when they
# diverge — a deployed skill still using the old `fact::{actor}::None::{text}` key
# echo-collided NOTHING, so the same fact appended once as a bare skill write and
# again as its "[engine] <text>" VERIFIED_FACT marker, half-defeating the
# run-75377 echo-dedup fix. These tests run the ACTUAL skill against a real DB and
# assert the persisted dedupe_key equals what _normalize_fact_identity would
# produce, across the echo-dedup battery (engine prefix, whitespace, case).

def _dedupe_key_for(db, text, *, worker="cli-claude", script=None):
    """Write `text` via the skill and return the dedupe_key it persisted."""
    skill = str(script) if script is not None else None
    if skill is None:
        _run(db, "write-fact", text, worker=worker)
    else:
        env = {**os.environ, "MUTEKI_BLACKBOARD_DB": str(db), "MUTEKI_WORKER_ID": worker}
        r = subprocess.run([sys.executable, skill, "write-fact", text],
                           capture_output=True, text=True, env=env, timeout=30)
        assert r.returncode == 0, f"{skill} write-fact failed: {r.stderr}"
    con = sqlite3.connect(str(db))
    try:
        row = con.execute(
            "SELECT dedupe_key FROM events WHERE kind='fact_added' "
            "AND actor=? ORDER BY seq DESC LIMIT 1", (worker,)).fetchone()
    finally:
        con.close()
    assert row is not None, "skill did not persist a fact_added event"
    return row[0]


@pytest.mark.parametrize("text", [
    "service is nginx 1.18.0",
    "[claude] service is nginx 1.18.0",            # engine prefix stripped
    "[Codex] service is nginx 1.18.0",             # case-insensitive prefix
    "service   is\tnginx\n1.18.0",                 # whitespace folded
    "SERVICE is NGINX 1.18.0",                      # lowercased
    "admin:admin works on /login",
    "flag candidate: FLAG{not_a_real_flag}",
])
def test_skill_dedupe_key_matches_normalize_fact_identity(tmp_path, text):
    """The repo skill's persisted dedupe_key == coordinator's identity key for the
    same actor+text. This is the regression-catcher for run-75378 drift."""
    g = _board(tmp_path)
    db = g.db_path
    g.close()
    got = _dedupe_key_for(db, text, worker="cli-claude")
    expected = f"fact::cli-claude::{_normalize_fact_identity(text)}"
    assert got == expected


def test_skill_echo_dedupe_collides_engine_prefixed_marker(tmp_path):
    """A bare skill fact and its "[engine] <text>" VERIFIED_FACT echo must collide on
    ONE dedupe_key (the exact run-75377/75378 echo the normalized key exists to kill).
    Same actor + same identity ⇒ second write is a no-op."""
    g = _board(tmp_path)
    db = g.db_path
    g.close()
    k1 = _dedupe_key_for(db, "service is nginx 1.18.0", worker="cli-claude")
    k2 = _dedupe_key_for(db, "[claude] service is nginx 1.18.0", worker="cli-claude")
    assert k1 == k2
    # and the board carries exactly one such fact, not two
    con = sqlite3.connect(str(db))
    try:
        n = con.execute(
            "SELECT COUNT(*) FROM events WHERE kind='fact_added' AND actor='cli-claude'"
        ).fetchone()[0]
    finally:
        con.close()
    assert n == 1


def test_resolved_blackboard_script_dedupe_matches_repo(tmp_path):
    """Whatever _blackboard_script_path() RESOLVES to for a non-containerized run must
    itself produce a dedupe_key matching _normalize_fact_identity — i.e. the path the
    swarm actually hands workers is never a drifted copy. (Source runs resolve to the
    repo skill; this guards the resolution wiring + the resolved file's logic.)"""
    ch = Challenge(id="resolve", name="t", category="web")
    resolved = CliSolver(None, ch, engine="claude")._blackboard_script_path()
    assert resolved != "/usr/local/bin/blackboard.py"  # not the container path here
    assert Path(resolved).is_file()

    g = _board(tmp_path)
    db = g.db_path
    g.close()
    text = "[claude]   ADMIN panel   at /admin"
    got = _dedupe_key_for(db, text, worker="cli-codex", script=resolved)
    expected = f"fact::cli-codex::{_normalize_fact_identity(text)}"
    assert got == expected


# ── safety-net: sync_deployed_blackboard_skills reconciles deployed copies ────

def test_sync_deployed_blackboard_skills_resyncs_stale_and_missing(tmp_path, monkeypatch):
    """The launch-time safety net overwrites a stale/missing deployed copy from the
    repo source and leaves a fresh one alone — closing the run-75378 drift gap for the
    auto-discovered user-scope copies."""
    from muteki.solver import cli_solver

    claude = tmp_path / ".claude" / "skills" / "muteki-blackboard" / "blackboard.py"
    agents = tmp_path / ".agents" / "skills" / "muteki-blackboard" / "blackboard.py"
    monkeypatch.setattr(cli_solver, "_DEPLOYED_BLACKBOARD_SCRIPTS",
                        (str(claude), str(agents)))
    src = Path(cli_solver._repo_blackboard_script())

    # First run: both missing → both synced from repo (and SKILL.md moves too).
    rows = cli_solver.sync_deployed_blackboard_skills()
    assert {r["status"] for r in rows} == {"synced"}
    assert claude.read_bytes() == src.read_bytes()
    assert agents.read_bytes() == src.read_bytes()
    assert (claude.parent / "SKILL.md").is_file()

    # Second run: identical → no action.
    rows = cli_solver.sync_deployed_blackboard_skills()
    assert {r["status"] for r in rows} == {"ok"}

    # Drift ONE copy → only it is re-synced; the fresh one is left untouched.
    claude.write_text("# drifted out of sync\n")
    rows = cli_solver.sync_deployed_blackboard_skills()
    by_path = {r["path"]: r["status"] for r in rows}
    assert by_path[str(claude)] == "synced"
    assert by_path[str(agents)] == "ok"
    assert claude.read_bytes() == src.read_bytes()  # restored


def test_sync_deployed_blackboard_skills_no_source_is_noop(monkeypatch):
    """An installed deployment (no repo skill adjacent to the package) reports
    'no-source' and touches nothing — the deployed copy IS the source of truth there."""
    from muteki.solver import cli_solver

    monkeypatch.setattr(cli_solver, "_repo_blackboard_script", lambda: None)
    rows = cli_solver.sync_deployed_blackboard_skills()
    assert rows and all(r["status"] == "no-source" for r in rows)
