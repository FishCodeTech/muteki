"""Tests for f11 agent teams — Gate-0 contracts (no cursor burn)."""

from __future__ import annotations

import json
import sqlite3
import types

from muteki.frameworks.f11_agent_teams.schema import (
    F11_FEATURE_KINDS,
    TEAMMATE_FORBIDDEN_CMDS,
    ensure_f11_schema,
)
from muteki.frameworks.f11_agent_teams.team import (
    claim_task,
    complete_task,
    counting_protocol,
    form_team,
    list_messages,
    send_message,
    teammate_cmd_allowed,
    write_assertion,
)


class _G:
    def __init__(self, conn):
        self._conn = conn
        self._lock = None
        self._kinds = []

    def _append(self, kind, actor, payload, dedupe_key=None):
        self._kinds.append(kind)
        self._conn.execute(
            "INSERT INTO events (ts, challenge_id, actor, kind, payload, verified, "
            "confidence) VALUES (0,'c',?,?,?,0,1)",
            (actor, kind, json.dumps(payload)),
        )
        self._conn.commit()
        return int(
            self._conn.execute("SELECT MAX(seq) FROM events").fetchone()[0] or 1
        )

    def verified_evidence(self):
        return [{"seq": 1, "text": "login sqli confirmed", "confidence": 1.0}]

    def events_since(self, after_seq, kinds=None):
        rows = self._conn.execute(
            "SELECT seq, kind, actor, payload FROM events WHERE seq>? ORDER BY seq",
            (int(after_seq or 0),),
        ).fetchall()
        out = []
        for seq, kind, actor, payload in rows:
            if kinds and kind not in kinds:
                continue
            out.append(
                {
                    "seq": int(seq),
                    "kind": kind,
                    "actor": actor,
                    "payload": json.loads(payload or "{}"),
                }
            )
        return out


def _graph(tmp_path):
    conn = sqlite3.connect(tmp_path / "g.db")
    conn.executescript(
        "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL, "
        "challenge_id TEXT, actor TEXT, kind TEXT, payload TEXT, verified INT, "
        "confidence REAL);"
    )
    ensure_f11_schema(conn)
    return _G(conn)


def test_swarm_bootstrap_emits_feature_kinds(tmp_path):
    from muteki.frameworks.f11_agent_teams import SwarmF11

    g = _graph(tmp_path)
    swarm = SwarmF11.__new__(SwarmF11)
    swarm.challenge = types.SimpleNamespace(id="2015q-for-flash", category="forensics")
    swarm.shared_graph = g
    swarm.run_id = "run-boot"
    swarm._f11_ready = False
    swarm._f11_bootstrapped = False
    swarm._f11_mirror_seq = 0
    swarm._f11_team_id = ""
    swarm._f11_members = []
    swarm._f11_tasks = []
    swarm._f11_token = {}
    swarm._f11_guidance = {}
    swarm._f11_intent_role = {}
    swarm._f11_intent_member = {}
    swarm._f11_intent_task = {}
    swarm._f11_role_rr = 0
    swarm._f11_msg_after = 0
    swarm._f11_lead_calls = 0
    swarm.framework_prepare_hook()
    kinds = set(g._kinds)
    assert "team_formed" in kinds
    assert any(k in kinds for k in F11_FEATURE_KINDS)
    assert SwarmF11.architecture_name == "f11-agent-teams"
    assert SwarmF11.f11_enabled is True
    guide = swarm.framework_worker_guidance_for_intent("missing")
    assert guide and "f11-agent-teams" in guide[0]


def test_gate0_counting_protocol_order_and_ack(tmp_path):
    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="gate0", roles=("recon", "exploit", "verify"))
    members = [m["name"] for m in formed["members"]]
    # Extend to 5 durable teammates for Gate-0 contract.
    members = members + ["recon-2", "exploit-2"]
    result = counting_protocol(
        g, team_id=formed["team_id"], members=members, rounds=30
    )
    assert result["stale_rejected"] is True
    assert result["seq_mono"] is True
    assert result["order_ok"] is True
    assert result["ack_rate"] == 1.0
    assert result["errors"] == []
    assert result["ok"] is True
    assert result["counted"] == 30 * 5


def test_message_contracts_hop_and_evidence(tmp_path):
    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="msg", roles=("recon", "exploit", "verify"))
    tid = formed["team_id"]
    bad_hop = send_message(
        g,
        team_id=tid,
        kind="direct",
        from_member="recon-1",
        to=["exploit-1"],
        body="relay",
        hop=3,
    )
    assert bad_hop["ok"] is False and bad_hop["error"] == "hop_exceeded"
    bad_ev = send_message(
        g,
        team_id=tid,
        kind="evidence",
        from_member="recon-1",
        to=["exploit-1"],
        body="claim without proof",
    )
    assert bad_ev["ok"] is False and bad_ev["error"] == "evidence_required"
    ok = send_message(
        g,
        team_id=tid,
        kind="evidence",
        from_member="recon-1",
        to=["exploit-1"],
        body="port 80 open",
        evidence_refs=[{"kind": "artifact", "ref": "ws/a.txt", "digest": "sha256:x"}],
        require_ack=True,
    )
    assert ok["ok"] is True and ok["seq"] >= 1
    # Channel rejects idle chat kinds.
    chat = send_message(
        g,
        team_id=tid,
        kind="channel",
        from_member="recon-1",
        to=["*"],
        body="lol",
        channel_trace_kind="chitchat",  # type: ignore[arg-type]
    )
    assert chat["ok"] is False
    ch = send_message(
        g,
        team_id=tid,
        kind="channel",
        from_member="recon-1",
        to=["*"],
        body="unexpected cookie",
        channel_trace_kind="surprise",
    )
    assert ch["ok"] is True
    msgs = list_messages(g, team_id=tid, after_seq=0)
    seqs = [m["seq"] for m in msgs]
    assert seqs == sorted(seqs)


def test_task_claim_persistence_and_assertion(tmp_path):
    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="tasks", roles=("recon", "exploit", "verify"))
    tid = formed["team_id"]
    task_id = formed["tasks"][1]
    # First claim may already be taken on tasks[0]; use tasks[1].
    c1 = claim_task(g, team_id=tid, task_id=task_id, owner="exploit-1")
    assert c1["ok"] is True
    c2 = claim_task(g, team_id=tid, task_id=task_id, owner="verify-1")
    assert c2["ok"] is False
    done = complete_task(
        g,
        team_id=tid,
        task_id=task_id,
        owner="exploit-1",
        evidence_refs=[{"kind": "artifact", "ref": "ws/x", "digest": "d"}],
    )
    assert done["ok"] is True
    bad_as = write_assertion(g, team_id=tid, text="no proof", evidence_refs=[])
    assert bad_as["ok"] is False
    ok_as = write_assertion(
        g,
        team_id=tid,
        text="exploit path viable",
        evidence_refs=[{"kind": "artifact", "ref": "ws/x", "digest": "d"}],
    )
    assert ok_as["ok"] is True
    assert "team_task_claimed" in g._kinds
    assert "team_assertion_written" in g._kinds


def test_gate0b_teammate_whitelist():
    assert teammate_cmd_allowed("msg-send") is True
    assert teammate_cmd_allowed("task-claim") is True
    assert teammate_cmd_allowed("token-wait") is True
    for cmd in TEAMMATE_FORBIDDEN_CMDS:
        assert teammate_cmd_allowed(cmd) is False
    # Non-teammate mode unrestricted for coordinator.
    assert teammate_cmd_allowed("read-facts", mode="lead") is True


# ---------------------------------------------------------------------------
# T05 lead budget gate / T07 cost gate / T12 health / T13 digest / swarm wiring
# ---------------------------------------------------------------------------

import asyncio
import os
import subprocess
import sys


def test_lead_call_gate_cap_and_cooldown(tmp_path):
    from muteki.frameworks.f11_agent_teams.team import lead_status, try_lead_call

    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="lead", roles=("recon", "exploit"))
    tid = formed["team_id"]
    assert formed["lead_calls_used"] == 0  # no fake call at team formation
    now = 1000.0
    ok = try_lead_call(g, team_id=tid, purpose="briefing", now=now)
    assert ok["ok"] is True and ok["calls_used"] == 1
    # cooldown: 45s default
    blocked = try_lead_call(g, team_id=tid, purpose="plan_verdict", now=now + 10)
    assert blocked["ok"] is False and blocked["error"] == "lead_cooldown"
    ok2 = try_lead_call(g, team_id=tid, purpose="plan_verdict", now=now + 46)
    assert ok2["ok"] is True and ok2["calls_used"] == 2
    st = lead_status(g, team_id=tid)
    assert st["calls_used"] == 2 and st["calls_cap"] == 12
    # burn to the cap
    t = now + 46
    for _ in range(10):
        t += 46
        assert try_lead_call(g, team_id=tid, purpose="contradiction", now=t)["ok"]
    t += 46
    cap = try_lead_call(g, team_id=tid, purpose="closing", now=t)
    assert cap["ok"] is False and cap["error"] == "lead_cap_exceeded"
    assert lead_status(g, team_id=tid)["calls_used"] == 12
    assert "team_lead_call" in g._kinds


def test_msg_cap_and_circuit_breaker_enforcement(tmp_path):
    from muteki.frameworks.f11_agent_teams.team import record_cost, update_budget

    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="cost", roles=("recon", "exploit"))
    tid = formed["team_id"]
    update_budget(g, team_id=tid, msg_cap=2)
    for i in range(2):
        r = send_message(
            g, team_id=tid, kind="direct", from_member="recon-1",
            to=["exploit-1"], body=f"m{i}",
        )
        assert r["ok"] is True
    # over cap: ordinary traffic rejected, evidence-class still passes
    blocked = send_message(
        g, team_id=tid, kind="direct", from_member="recon-1",
        to=["exploit-1"], body="m2",
    )
    assert blocked["ok"] is False and blocked["error"] == "msg_cap_exceeded"
    ev = send_message(
        g, team_id=tid, kind="evidence", from_member="recon-1",
        to=["exploit-1"], body="port 80 open",
        evidence_refs=[{"kind": "artifact", "ref": "ws/a", "digest": "d"}],
    )
    assert ev["ok"] is True
    # protocol frames never count against the cap
    hb = send_message(
        g, team_id=tid, kind="heartbeat", from_member="recon-1", to=["lead"],
    )
    assert hb["ok"] is True
    # circuit breaker: baseline 100, spend to 131 → +30% trips
    update_budget(g, team_id=tid, msg_cap=999, baseline_cost_usd=100.0)
    record_cost(g, team_id=tid, cost_usd=120.0)
    assert record_cost(g, team_id=tid, cost_usd=0.0)["circuit_open"] is False
    record_cost(g, team_id=tid, cost_usd=11.0)
    assert record_cost(g, team_id=tid, cost_usd=0.0)["circuit_open"] is True
    assert "team_circuit_open" in g._kinds
    frozen = send_message(
        g, team_id=tid, kind="direct", from_member="recon-1",
        to=["exploit-1"], body="frozen?",
    )
    assert frozen["ok"] is False and frozen["error"] == "circuit_open"
    # new tasks frozen too (§4.4)
    from muteki.frameworks.f11_agent_teams.team import create_task

    assert create_task(g, team_id=tid, goal="x", created_by="lead") == ""
    # evidence still allowed under the breaker
    ev2 = send_message(
        g, team_id=tid, kind="evidence", from_member="recon-1",
        to=["exploit-1"], body="still accepted",
        evidence_refs=[{"kind": "artifact", "ref": "ws/b", "digest": "d"}],
    )
    assert ev2["ok"] is True


def test_channel_per_member_cap(tmp_path):
    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="chancap", roles=("recon", "exploit"))
    tid = formed["team_id"]
    for i in range(12):
        r = send_message(
            g, team_id=tid, kind="channel", from_member="recon-1", to=["*"],
            body=f"surprise {i}", channel_trace_kind="surprise",
        )
        assert r["ok"] is True
    blocked = send_message(
        g, team_id=tid, kind="channel", from_member="recon-1", to=["*"],
        body="one too many", channel_trace_kind="surprise",
    )
    assert blocked["ok"] is False and blocked["error"] == "channel_cap_exceeded"
    # other member unaffected
    ok = send_message(
        g, team_id=tid, kind="channel", from_member="exploit-1", to=["*"],
        body="mine", channel_trace_kind="surprise",
    )
    assert ok["ok"] is True


def test_t12_health_stalled_dead_replace(tmp_path):
    from muteki.frameworks.f11_agent_teams.team import (
        check_health,
        claim_task,
        heartbeat,
        list_tasks,
        replace_member,
    )

    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="health", roles=("recon", "exploit"))
    tid = formed["team_id"]
    t0 = 5000.0
    assert heartbeat(g, team_id=tid, member="recon-1", now=t0)["ok"] is True
    assert heartbeat(g, team_id=tid, member="exploit-1", now=t0)["ok"] is True
    assert heartbeat(g, team_id=tid, member="nobody", now=t0)["ok"] is False
    # recon-1 claims a task then goes silent
    task_id = formed["tasks"][0]
    assert claim_task(g, team_id=tid, task_id=task_id, owner="recon-1")["ok"]
    heartbeat(g, team_id=tid, member="exploit-1", now=t0 + 40)
    h = check_health(g, team_id=tid, now=t0 + 40)
    assert h["stalled"] == ["recon-1"] and h["dead"] == []
    heartbeat(g, team_id=tid, member="exploit-1", now=t0 + 130)
    h = check_health(g, team_id=tid, now=t0 + 130)
    assert h["dead"] == ["recon-1"]
    assert h["released"]["recon-1"] == [task_id]
    assert "team_member_stalled" in g._kinds and "team_member_dead" in g._kinds
    # released task is claimable again
    tasks = {t["task_id"]: t for t in list_tasks(g, team_id=tid)}
    assert tasks[task_id]["status"] == "pending"
    # same-role replacement with rebuilt context
    repl = replace_member(g, team_id=tid, dead_member="recon-1", now=t0 + 131)
    assert repl["ok"] is True
    assert repl["member"]["name"] == "recon-2"
    assert repl["member"]["role"] == "recon"
    assert repl["member"]["replaces"] == "recon-1"
    assert "recon" in repl["context_brief"]
    assert "team_member_replaced" in g._kinds
    # cannot replace a live member
    bad = replace_member(g, team_id=tid, dead_member="exploit-1", now=t0 + 132)
    assert bad["ok"] is False and bad["error"] == "member_not_dead"


def test_t11_lease_timeout_sweep(tmp_path):
    from muteki.frameworks.f11_agent_teams.team import check_health, claim_task

    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="lease", roles=("recon", "exploit"))
    tid = formed["team_id"]
    task_id = formed["tasks"][1]
    assert claim_task(g, team_id=tid, task_id=task_id, owner="exploit-1")["ok"]
    # force the lease to look stale
    g._conn.execute(
        "UPDATE team_task SET lease_json=? WHERE task_id=?",
        (json.dumps({"owner": "exploit-1", "claimed_at": 1000.0}), task_id),
    )
    g._conn.commit()
    h = check_health(g, team_id=tid, now=1000.0 + 301.0)
    assert task_id in h["lease_expired"]


def test_digest_distiller_and_fallback(tmp_path):
    from muteki.frameworks.f11_agent_teams.team import (
        distill_channel_digest,
        latest_channel_digest,
    )

    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="digest", roles=("recon", "exploit"))
    tid = formed["team_id"]
    send_message(
        g, team_id=tid, kind="channel", from_member="recon-1", to=["*"],
        body="raw trace", channel_trace_kind="surprise",
    )
    # explicit grok-low text wins and is recorded as such
    dg = distill_channel_digest(
        g, team_id=tid, distilled_text="distilled by low model",
        by_model="grok-4.5-low",
    )
    assert dg["distilled"] == "distilled by low model"
    assert dg["by_model"] == "grok-4.5-low"
    # callable distiller path
    dg2 = distill_channel_digest(g, team_id=tid, distiller=lambda raw: "cb:" + raw[:10])
    assert dg2["distilled"].startswith("cb:") and dg2["by_model"] == "grok-4.5-low"
    # no LLM → deterministic fallback, honestly labelled
    dg3 = distill_channel_digest(g, team_id=tid)
    assert dg3["by_model"] == "deterministic"
    assert latest_channel_digest(g, team_id=tid)["by_model"] == "deterministic"


class _MockLLM:
    """Async stand-in for LLMClient.chat — no network."""

    def __init__(self):
        self.calls = []

    async def chat(self, *, model, messages, **kw):
        self.calls.append({"model": model, "messages": messages})
        user = messages[-1]["content"]
        if "Plan approval" in user:
            content = '{"verdict": "approve", "reason": "evidence looks solid"}'
        elif "Closing synthesis" in user:
            content = '{"summary": "all done", "lessons": ["l1"]}'
        else:
            content = (
                '{"summary": "brief", "tasks": [{"goal": "probe /api", '
                '"role": "recon", "declared_effects": [{"selector": "api.seen", '
                '"op": "eq", "value": true}]}]}'
            )
        return types.SimpleNamespace(content=content)


def _mk_swarm(tmp_path, llm=None, cid="2015q-for-flash"):
    from muteki.frameworks.f11_agent_teams import SwarmF11

    g = _graph(tmp_path)
    swarm = SwarmF11.__new__(SwarmF11)
    swarm.challenge = types.SimpleNamespace(id=cid, category="web", description="d")
    swarm.shared_graph = g
    swarm.run_id = "run-x"
    swarm.reason_model = "glm-5.2:cloud"
    swarm.llm = llm
    swarm.bus = None  # mirror emits become no-ops without an event bus
    swarm._f11_late_init()
    return swarm, g


def test_bootstrap_no_coordinator_claim_or_fake_lead_call(tmp_path):
    """Gap ①/②: tasks stay pending for teammates; no phantom lead call."""
    swarm, g = _mk_swarm(tmp_path)
    swarm.framework_prepare_hook()
    from muteki.frameworks.f11_agent_teams.team import lead_status, list_tasks

    tasks = list_tasks(g, team_id=swarm._f11_team_id)
    assert tasks and all(t["status"] == "pending" for t in tasks)
    assert lead_status(g, team_id=swarm._f11_team_id)["calls_used"] == 0
    # briefing was queued for the async lead pump
    assert any(p["purpose"] == "briefing" for p in swarm._f11_lead_pending)


def test_no_llm_lead_call_burns_no_budget(tmp_path):
    swarm, g = _mk_swarm(tmp_path, llm=None)
    swarm.framework_prepare_hook()
    asyncio.run(swarm._f11_pump_lead())
    from muteki.frameworks.f11_agent_teams.team import lead_status

    assert lead_status(g, team_id=swarm._f11_team_id)["calls_used"] == 0


def test_real_lead_briefing_and_plan_verdict(tmp_path):
    """Gap ②: briefing + plan approval go through self.llm (mocked glm)."""
    swarm, g = _mk_swarm(tmp_path, llm=_MockLLM())
    swarm.framework_prepare_hook()
    tid = swarm._f11_team_id
    g._conn.execute("UPDATE team_roster SET lead_cooldown_s=0 WHERE team_id=?", (tid,))
    g._conn.commit()
    # a teammate asks for plan approval
    send_message(
        g, team_id=tid, kind="plan_request", from_member="exploit-1",
        to=["lead"], body="plan: brute-force the admin token endpoint",
    )
    asyncio.run(swarm.framework_after_workers())
    from muteki.frameworks.f11_agent_teams.team import lead_status

    # briefing + verdict both consumed real (mocked) glm calls
    assert lead_status(g, team_id=tid)["calls_used"] == 2
    glm_calls = [c for c in swarm.llm.calls if c["model"] == "glm-5.2:cloud"]
    assert len(glm_calls) == 2  # briefing + plan verdict
    # the channel digest (first beat, >60s since epoch) used grok-low
    assert any(c["model"] == "grok-4.5-low" for c in swarm.llm.calls)
    # briefing created the refined task; verdict went back to the requester
    msgs = list_messages(g, team_id=tid, after_seq=0, limit=100)
    verdicts = [m for m in msgs if m["kind"] == "plan_verdict"]
    assert verdicts and verdicts[0]["to"] == ["exploit-1"]
    assert "approve" in verdicts[0]["body"]
    from muteki.frameworks.f11_agent_teams.team import list_tasks

    goals = [t["goal"] for t in list_tasks(g, team_id=tid)]
    assert any("probe /api" in goal for goal in goals)


def test_record_worker_outcome_no_coordinator_heartbeat_message(tmp_path):
    """Gap ①: coordinator updates liveness bookkeeping only — no heartbeat
    message is ever sent on a member's behalf."""
    swarm, g = _mk_swarm(tmp_path)
    swarm.framework_prepare_hook()
    swarm._f11_intent_member["i1"] = "recon-1"
    before = [
        m for m in list_messages(g, team_id=swarm._f11_team_id, after_seq=0, limit=100)
        if m["kind"] == "heartbeat"
    ]
    swarm.framework_record_worker_outcome(
        engine="cursor", intent={"intent_id": "i1"}, success=True, cost_usd=1.5
    )
    after = [
        m for m in list_messages(g, team_id=swarm._f11_team_id, after_seq=0, limit=100)
        if m["kind"] == "heartbeat"
    ]
    assert after == before  # zero heartbeat messages
    from muteki.frameworks.f11_agent_teams.team import get_budget

    assert get_budget(g, team_id=swarm._f11_team_id)["cost_usd"] == 1.5


def test_intent_tasks_not_preclaimed(tmp_path):
    swarm, g = _mk_swarm(tmp_path)
    swarm.framework_prepare_hook()
    swarm.framework_on_intents_proposed(
        [{"intent_id": "i9", "goal": "enumerate vhosts"}]
    )
    from muteki.frameworks.f11_agent_teams.team import list_tasks

    task = {t["task_id"]: t for t in list_tasks(g, team_id=swarm._f11_team_id)}[
        swarm._f11_intent_task["i9"]
    ]
    assert task["status"] == "pending" and not task["owner"]
    guidance = swarm.framework_worker_guidance_for_intent("i9")[0]
    assert "task-claim" in guidance and "--mode=teammate" in guidance


# ---------------------------------------------------------------------------
# Review-fix regressions: dormant outcome hook / finalize fallback / cascade
# ---------------------------------------------------------------------------


def test_after_workers_feeds_cost_ledger_from_intent_concluded(tmp_path):
    """Fix ①: framework_record_worker_outcome is never called by the swarm
    core, so the T07 ledger + +30% breaker are fed from intent_concluded
    events — idempotently, and only for this team's own intents."""
    from muteki.frameworks.f11_agent_teams.team import get_budget, update_budget

    swarm, g = _mk_swarm(tmp_path)
    swarm.framework_prepare_hook()
    tid = swarm._f11_team_id
    swarm._f11_intent_member["i1"] = "recon-1"
    update_budget(g, team_id=tid, baseline_cost_usd=100.0)
    g._append(
        "intent_concluded", "cli-cursor-1",
        {"intent_id": "i1", "result": "explored", "cost_usd": 140.0},
    )
    asyncio.run(swarm.framework_after_workers())
    budget = get_budget(g, team_id=tid)
    assert budget["cost_usd"] == 140.0
    assert budget["circuit_open"] is True  # 140 > 100 * 1.3 → breaker trips
    assert "team_circuit_open" in g._kinds
    # idempotent: further beats must not double-count the same event
    asyncio.run(swarm.framework_after_workers())
    assert get_budget(g, team_id=tid)["cost_usd"] == 140.0
    # an intent this team does not own never touches the team budget
    g._append(
        "intent_concluded", "cli-cursor-9",
        {"intent_id": "iX", "result": "explored", "cost_usd": 50.0},
    )
    # an owned intent with no cost in the event records nothing either
    swarm._f11_intent_member["i2"] = "exploit-1"
    g._append(
        "intent_concluded", "cli-cursor-2",
        {"intent_id": "i2", "result": "explored"},
    )
    asyncio.run(swarm.framework_after_workers())
    assert get_budget(g, team_id=tid)["cost_usd"] == 140.0


def test_finalize_fallback_when_all_tasks_terminal(tmp_path):
    """Fix ②: framework_finalize_hook has no core caller — after_workers must
    latch the closing synthesis itself, exactly once, when the list drains."""
    from muteki.frameworks.f11_agent_teams.team import lead_status

    swarm, g = _mk_swarm(tmp_path, llm=_MockLLM())
    swarm.framework_prepare_hook()
    tid = swarm._f11_team_id
    g._conn.execute("UPDATE team_roster SET lead_cooldown_s=0 WHERE team_id=?", (tid,))
    g._conn.execute("UPDATE team_task SET status='done' WHERE team_id=?", (tid,))
    g._conn.commit()
    asyncio.run(swarm.framework_after_workers())
    assert swarm._f11_finalize_triggered is True
    assert swarm._f11_finalized is True  # mock glm answered the closing call
    used = lead_status(g, team_id=tid)["calls_used"]
    assert used == 2  # briefing + closing, nothing else
    # once finalized, later beats never queue another closing
    asyncio.run(swarm.framework_after_workers())
    assert lead_status(g, team_id=tid)["calls_used"] == used


def test_finalize_fallback_when_flag_already_out(tmp_path):
    """Fix ②: a found flag triggers finalize even with tasks still open."""
    swarm, g = _mk_swarm(tmp_path, llm=_MockLLM())
    swarm.framework_prepare_hook()
    tid = swarm._f11_team_id
    g._conn.execute("UPDATE team_roster SET lead_cooldown_s=0 WHERE team_id=?", (tid,))
    g._conn.commit()
    g._append("flag_found", "cli-cursor-1", {"flag": "flag{pwned}"})
    asyncio.run(swarm.framework_after_workers())
    assert swarm._f11_finalize_triggered is True
    assert swarm._f11_finalized is True
    from muteki.frameworks.f11_agent_teams.team import list_tasks

    # tasks were still open — the flag alone latched the finalize
    assert any(
        t["status"] in ("pending", "claimed")
        for t in list_tasks(g, team_id=tid)
    )


def test_depends_on_cascade_invalidation(tmp_path):
    """Fix ③: completing (or failing to complete) a task invalidates its
    pending dependents, transitively (§2.2 depends_on, §4.4 链式收尾)."""
    from muteki.frameworks.f11_agent_teams.team import (
        claim_task,
        create_task,
        list_tasks,
    )

    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="cascade", roles=("recon", "exploit"))
    tid = formed["team_id"]
    parent = formed["tasks"][0]
    child = create_task(
        g, team_id=tid, goal="child step", created_by="recon-1",
        depends_on=[parent],
    )
    grand = create_task(
        g, team_id=tid, goal="grandchild step", created_by="recon-1",
        depends_on=[child],
    )
    unrelated = formed["tasks"][1]
    assert child and grand
    # failed completion (task never claimed → complete_lost) still cascades
    res = complete_task(
        g, team_id=tid, task_id=parent, owner="recon-1",
        evidence_refs=[{"kind": "artifact", "ref": "ws/x", "digest": "d"}],
    )
    assert res["ok"] is False and res["error"] == "complete_lost"
    assert set(res["invalidated"]) == {child, grand}
    tasks = {t["task_id"]: t for t in list_tasks(g, team_id=tid)}
    assert tasks[child]["status"] == "invalidated"
    assert tasks[grand]["status"] == "invalidated"  # transitive
    assert tasks[unrelated]["status"] == "pending"  # untouched
    # successful completion tears down its chain too
    child2 = create_task(
        g, team_id=tid, goal="chain step", created_by="exploit-1",
        depends_on=[unrelated],
    )
    assert claim_task(g, team_id=tid, task_id=unrelated, owner="exploit-1")["ok"]
    res2 = complete_task(
        g, team_id=tid, task_id=unrelated, owner="exploit-1",
        evidence_refs=[{"kind": "artifact", "ref": "ws/y", "digest": "d"}],
    )
    assert res2["ok"] is True and res2["invalidated"] == [child2]
    tasks = {t["task_id"]: t for t in list_tasks(g, team_id=tid)}
    assert tasks[unrelated]["status"] == "done"
    assert tasks[child2]["status"] == "invalidated"


# ---------------------------------------------------------------------------
# blackboard skill team subcommands (subprocess, stdlib-only CLI)
# ---------------------------------------------------------------------------

_SKILL = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "skills", "muteki-blackboard", "blackboard.py",
)


def _skill_db(tmp_path):
    g = _graph(tmp_path)
    formed = form_team(g, challenge_id="cli", roles=("recon", "exploit", "verify"))
    return str(tmp_path / "g.db"), formed


def _run_skill(db, *argv, member="recon-1", mode="teammate", timeout=30):
    env = dict(os.environ)
    env["MUTEKI_BLACKBOARD_DB"] = db
    env["MUTEKI_TEAM_MEMBER"] = member
    cmd = [sys.executable, _SKILL]
    if mode:
        cmd += ["--mode", mode]
    cmd += list(argv)
    return subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=timeout)


def test_skill_teammate_mode_whitelist(tmp_path):
    db, _ = _skill_db(tmp_path)
    # forbidden full-board reads are NOT registered in teammate mode
    for cmd in ("read-facts", "read-routes", "read-branches", "read-archive"):
        r = _run_skill(db, cmd)
        assert r.returncode != 0 and "invalid choice" in r.stderr
    # whitelisted commands exist
    r = _run_skill(db, "task-list")
    assert r.returncode == 0
    # without --mode=teammate everything is still there
    r = _run_skill(db, "read-facts", mode="")
    assert r.returncode == 0


def test_skill_msg_send_contracts(tmp_path):
    db, _ = _skill_db(tmp_path)
    # hop>2 rejected
    r = _run_skill(db, "msg-send", "--kind=direct", "--to=exploit-1",
                   "--body=x", "--hop=3")
    assert "REJECTED hop_exceeded" in r.stdout
    # evidence kind without evidence rejected
    r = _run_skill(db, "msg-send", "--kind=evidence", "--to=exploit-1",
                   "--body=claim")
    assert "REJECTED evidence_required" in r.stdout
    # ok with evidence
    r = _run_skill(db, "msg-send", "--kind=evidence", "--to=exploit-1",
                   "--body=port 80 open",
                   "--evidence=artifact:ws/a.txt:sha256:x",
                   "--require-ack")
    assert "OK seq=" in r.stdout
    # channel chitchat kind rejected; surprise ok
    r = _run_skill(db, "msg-send", "--kind=channel", "--body=lol",
                   "--channel-kind=chitchat")
    assert "REJECTED" in r.stdout
    r = _run_skill(db, "msg-send", "--kind=channel", "--body=odd cookie",
                   "--channel-kind=surprise")
    assert "OK seq=" in r.stdout
    # exploit-1 checks its mailbox and auto-acks the require_ack message
    r = _run_skill(db, "msg-check", member="exploit-1")
    assert "port 80 open" in r.stdout and "acked" in r.stdout


def test_skill_task_claim_done_and_heartbeat(tmp_path):
    db, formed = _skill_db(tmp_path)
    task_id = formed["tasks"][0]
    r = _run_skill(db, "task-claim", task_id, member="recon-1")
    assert r.stdout.strip() == "WON"
    r = _run_skill(db, "task-claim", task_id, member="exploit-1")
    assert "LOST" in r.stdout
    # done requires evidence
    r = _run_skill(db, "task-done", task_id, member="recon-1")
    assert "REJECTED evidence_required" in r.stdout
    r = _run_skill(db, "task-done", task_id,
                   "--evidence=artifact:ws/x:sha256:d", member="recon-1")
    assert r.stdout.strip() == "OK"
    r = _run_skill(db, "heartbeat", member="recon-1")
    assert r.stdout.strip() == "OK"
    r = _run_skill(db, "heartbeat", member="ghost-9")
    assert r.returncode == 2


def test_skill_assert_and_artifact(tmp_path):
    db, _ = _skill_db(tmp_path)
    r = _run_skill(db, "assert-write", "no proof here")
    assert "REJECTED evidence_required" in r.stdout
    art = tmp_path / "poc.txt"
    art.write_text("HTTP/1.1 200 OK\n")
    r = _run_skill(db, "artifact-put", str(art))
    ref = json.loads(r.stdout)
    assert ref["kind"] == "artifact" and ref["digest"].startswith("sha256:")
    r = _run_skill(db, "assert-write", "login sqli confirmed",
                   f"--evidence=artifact:{art}:{ref['digest']}")
    assert "OK assertion_id=" in r.stdout


def test_skill_token_wait_timeout(tmp_path):
    db, formed = _skill_db(tmp_path)
    tok = formed["token"]["token_id"]
    # recon-1 holds the bootstrap token; exploit-1 waits in vain (short timeout)
    r = _run_skill(db, "token-wait", f"--token-id={tok}", "--timeout=1",
                   member="exploit-1", timeout=15)
    assert "TIMEOUT" in r.stdout
    # the holder sees GRANTED with a fence credential
    holder = formed["token"]["holder"]
    r = _run_skill(db, "token-wait", f"--token-id={tok}", "--timeout=1",
                   member=holder, timeout=15)
    assert "GRANTED" in r.stdout and "fence=" in r.stdout
