"""P-C: Reason phase — planning + evidence audit. ScriptedLLM, no API key."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass

import pytest

from muteki.models.solve_graph import Challenge, SolveGraph
from muteki.solver.reason import (
    Intent, build_reason_prompt, parse_reason_reply, run_reason, dispatch_intents,
)
from muteki.swarm.shared_graph import SQLiteSharedGraph


@dataclass
class _Resp:
    content: str
    reasoning: str = ""


class ScriptedLLM:
    """Returns a fixed reply for chat()."""
    def __init__(self, reply: str):
        self.reply = reply
        self.calls = 0

    async def chat(self, **kw):
        self.calls += 1
        return _Resp(content=self.reply)


def test_parse_clean_json():
    reply = """```json
    {"goal_met": false,
     "intents": [
       {"id":"I1","goal":"crack repeating xor with crib drag","worker_class":"code","depends_on":[],"rationale":"high ascii entropy"},
       {"id":"I2","goal":"check for known plaintext header","worker_class":"code"}
     ],
     "audit": []}
    ```"""
    r = parse_reason_reply(reply)
    assert r.goal_met is False
    assert len(r.intents) == 2
    assert r.intents[0].goal.startswith("crack repeating xor")
    assert r.intents[0].worker_class == "code"


def test_parse_caps_intents():
    reply = '{"intents":[' + ",".join(
        f'{{"id":"I{i}","goal":"g{i}"}}' for i in range(10)) + ']}'
    r = parse_reason_reply(reply, max_intents=3)
    assert len(r.intents) == 3


def test_parse_garbage_is_safe():
    r = parse_reason_reply("the model rambled with no json")
    assert r.intents == []
    assert r.goal_met is False


def test_shell_agent_only_when_declared():
    reply = '{"intents":[{"id":"I1","goal":"deep RE long chain","worker_class":"shell_agent"}]}'
    r = parse_reason_reply(reply)
    assert r.intents[0].worker_class == "shell_agent"
    # invalid worker_class falls back to code
    reply2 = '{"intents":[{"id":"I1","goal":"x","worker_class":"wizard"}]}'
    assert parse_reason_reply(reply2).intents[0].worker_class == "code"


def test_parse_accepts_review_worker_class_and_route_metadata():
    reply = (
        '{"intents":[{"id":"I-review","goal":"audit repeated login SQLi",'
        '"worker_class":"review","route_hash":"web:login:sqli",'
        '"branch_id":"branch-auth","from":[3],"rationale":"loop detected"}]}'
    )
    r = parse_reason_reply(reply)
    assert r.intents[0].worker_class == "review"
    assert r.intents[0].route_hash == "web:login:sqli"
    assert r.intents[0].branch_id == "branch-auth"


def test_parse_accepts_semantic_duplicate_metadata():
    reply = (
        '{"intents":[{"id":"I2","goal":"Enumerate admin endpoints",'
        '"dup_of":"I1-old","reopen_because":"new /admin leak"}]}'
    )
    r = parse_reason_reply(reply)
    assert r.semantic_dedupe_available is True
    assert r.intents[0].dup_of == "I1-old"
    assert r.intents[0].reopen_because == "new /admin leak"


def test_parse_accepts_model_selected_pinned_facts():
    reply = (
        '{"goal_met":false,"pinned_facts":[3,"4",-1,"bad",3],'
        '"intents":[{"id":"I1","goal":"continue with retained credential"}]}'
    )
    r = parse_reason_reply(reply)
    assert r.pinned_facts == [3, 4]


def test_parse_preserves_lane_metadata():
    reply = (
        '{"intents":[{"id":"I-lane","goal":"serialize SMB exploit",'
        '"lane_key":"destructive:tcp:445@172.22.11.45",'
        '"risk_class":"destructive"}]}'
    )
    r = parse_reason_reply(reply)
    assert r.intents[0].lane_key == "destructive:tcp:445@172.22.11.45"
    assert r.intents[0].to_payload()["risk_class"] == "destructive"


def test_audit_flags_unverified_fact():
    reply = ('{"goal_met":false,'
             '"intents":[{"id":"I1","goal":"VERIFY the claimed key before using it"}],'
             '"audit":["the key is 0x1337 — unverified, no artifact"]}')
    r = parse_reason_reply(reply)
    assert r.audit_notes
    assert "0x1337" in r.audit_notes[0]


def test_prompt_includes_summary_and_candidate_section():
    g = SolveGraph(challenge=Challenge(id="t", name="t", category="crypto"))
    g.add_evidence(source="x", fact="ciphertext loaded", verified=True,
                   verifier="substring_in_artifact")
    g.add_evidence(source="x", fact="maybe rsa", verified=False, confidence=0.4,
                   verifier="artifact_readable_unlocated")
    summary = g.to_summary()
    msgs = build_reason_prompt(summary)
    assert "Candidates / needs verification" in msgs[1]["content"]
    assert "[UNVERIFIED]" in msgs[1]["content"]


def test_run_reason_end_to_end():
    llm = ScriptedLLM('{"goal_met":false,"intents":[{"id":"I1","goal":"do the thing"}]}')
    r = asyncio.run(run_reason(llm=llm, model="flash", graph_summary="# c"))
    assert llm.calls == 1
    assert r.intents[0].goal == "do the thing"


def test_run_reason_prompt_includes_fact_index_for_model_pinning():
    class CapturingLLM(ScriptedLLM):
        async def chat(self, **kw):
            self.messages = kw["messages"]
            return await super().chat(**kw)

    llm = CapturingLLM('{"goal_met":false,"pinned_facts":[9],"intents":[]}')
    r = asyncio.run(run_reason(
        llm=llm, model="flash", graph_summary="# c",
        fact_index="[#9] verified :: 后台口令是 admin / 猎人二号",
    ))
    user_msg = llm.messages[1]["content"]
    assert "Fact retention index" in user_msg
    assert "后台口令" in user_msg
    assert r.pinned_facts == [9]


def test_dispatch_intents_to_shared_graph(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="crypto"))
    from muteki.solver.reason import ReasonResult
    res = ReasonResult(goal_met=False,
                       intents=[Intent("I1", "crack xor"), Intent("I2", "find header")],
                       audit_notes=[])
    proposed = dispatch_intents(g, res)
    # dispatch_intents returns the list of intents actually proposed (so the caller
    # can emit blackboard intent_proposed events), not a bare count. Each id is now
    # suffixed with a goal-hash so cross-round I1/I2 reuse doesn't collide.
    ids = [p["intent_id"] for p in proposed]
    assert len(ids) == 2 and ids[0].startswith("I1-") and ids[1].startswith("I2-")
    # the intents are claimable under their (suffixed) ids
    assert g.claim_intent(worker="w1", intent_id=ids[0]) is True
    with g._lock:
        row = g._conn.execute(
            "SELECT worker_class FROM intents WHERE intent_id=?", (ids[0],)
        ).fetchone()
    assert row[0] == "code"
    g.close()


def test_dispatch_intents_unique_id_survives_round_reuse(tmp_path):
    """The retry-storm root cause: Reason re-labels its intents I1..I4 every round,
    and propose_intent dedupes on intent_id. Before the goal-hash suffix, round 2's
    I1 collided with round 1's and was dropped (0 proposed → Explore starves). Now a
    DIFFERENT goal under the same raw id is a distinct intent; the SAME goal is still
    correctly deduped."""
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    # round 1: I1 = "probe /login"
    r1 = dispatch_intents(g, ReasonResult(
        goal_met=False, intents=[Intent("I1", "probe /login")], audit_notes=[]))
    # round 2: model reuses id "I1" but for a genuinely different direction
    r2 = dispatch_intents(g, ReasonResult(
        goal_met=False, intents=[Intent("I1", "fuzz /export for SSRF")], audit_notes=[]))
    # round 3: same raw id AND same goal as round 1 → must dedupe (no duplicate work)
    r3 = dispatch_intents(g, ReasonResult(
        goal_met=False, intents=[Intent("I1", "probe /login")], audit_notes=[]))
    assert len(r1) == 1 and len(r2) == 1, "different goals under reused I1 must BOTH propose"
    assert r1[0]["intent_id"] != r2[0]["intent_id"], "distinct goals → distinct ids"
    assert len(r3) == 0, "identical goal re-proposed must be deduped"
    g.close()


def test_dispatch_drops_near_duplicate_of_open_intent(tmp_path):
    """A reworded paraphrase of a goal already OPEN on the board must NOT be
    proposed as a new intent — the goal-hash id only catches byte-identical goals,
    so 'Submit the L1 flag to the dashboard' would otherwise re-propose as new
    against an open 'Request the operator to submit L1 flag to dashboard'."""
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    r1 = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "Submit the L1 flag to the dashboard")],
        audit_notes=[]))
    assert len(r1) == 1
    # a filler-word rewording of the SAME direction → dropped
    r2 = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "Submit L1 flag to dashboard now")],
        audit_notes=[]))
    assert len(r2) == 0, "near-duplicate of an open intent must be dropped"
    # a genuinely different direction still gets through
    r3 = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "Brute-force the SSH password for ghost3")],
        audit_notes=[]))
    assert len(r3) == 1, "a distinct direction must still propose"
    g.close()


def test_dispatch_drops_llm_marked_duplicate(tmp_path):
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    g.propose_intent(actor="reason", intent_id="I-old",
                     goal="enumerate admin routes")

    out = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "list the administrator endpoints", dup_of="I-old")],
        audit_notes=[],
        semantic_dedupe_available=True,
    ))

    assert out == []
    g.close()


def test_dispatch_semantic_mode_does_not_apply_character_fallback(tmp_path):
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    g.propose_intent(actor="reason", intent_id="I-old",
                     goal="Submit the L1 flag to the dashboard")

    out = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "Submit L1 flag to dashboard now")],
        audit_notes=[],
        semantic_dedupe_available=True,
    ))

    assert len(out) == 1
    assert out[0]["goal"] == "Submit L1 flag to dashboard now"
    g.close()


def test_dispatch_fallback_duplicate_only_when_semantic_unavailable(tmp_path):
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    g.propose_intent(actor="reason", intent_id="I-old",
                     goal="Submit the L1 flag to the dashboard")

    out = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "Submit L1 flag to dashboard now")],
        audit_notes=[],
    ))

    assert out == []
    g.close()


def test_dispatch_starvation_valve_allows_one_when_wall_empty(tmp_path):
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))

    out = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "rerun login recon", dup_of="I-gone")],
        audit_notes=[],
        semantic_dedupe_available=True,
    ))

    assert len(out) == 1
    assert out[0]["goal"] == "rerun login recon"
    g.close()


def test_dispatch_starvation_valve_uses_dispatchable_not_claimed_goals(tmp_path):
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    g.propose_intent(actor="reason", intent_id="I-held",
                     goal="enumerate login injection")
    assert g.claim_intent(worker="cli-held", intent_id="I-held", lease_s=3600) is True

    out = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "enumerate login injection more carefully",
                        dup_of="I-held")],
        audit_notes=[],
        semantic_dedupe_available=True,
    ))

    assert len(out) == 1
    assert out[0]["goal"] == "enumerate login injection more carefully"
    g.close()


def test_dispatch_route_dedupes_plain_intents_but_exempts_review(tmp_path):
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    g.propose_intent(
        actor="reason", intent_id="I-open", goal="try login SQL injection",
        payload={"worker_class": "code", "route_hash": "web:login:sqli"})

    out = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[
            Intent("I1", "try SQL injection on the login form",
                   route_hash="WEB LOGIN SQLi"),
            Intent("I2", "review the repeated login SQLi loop",
                   worker_class="review", route_hash="WEB LOGIN SQLi"),
        ],
        audit_notes=[],
        semantic_dedupe_available=True,
    ))

    assert [x["worker_class"] for x in out] == ["review"]
    assert out[0]["goal"].startswith("review")
    g.close()


def test_dispatch_dedupes_two_wordings_within_one_batch(tmp_path):
    """Reason sometimes emits two wordings of one direction in a SINGLE round; the
    second must drop against the first proposed this batch (not just the board)."""
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    out = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "Download and analyze the admin JS bundle"),
                 Intent("I2", "Download and analyze admin JS bundle now")],
        audit_notes=[]))
    assert len(out) == 1, "two wordings of one direction in a batch → one proposed"
    g.close()


def test_dispatch_allows_distinct_directions_not_overpruned(tmp_path):
    """Guard against the run-7349 failure shape: the near-duplicate filter must NOT
    be so aggressive it collapses genuinely distinct directions (which would starve
    Explore). Four clearly-different goals must ALL propose."""
    from muteki.solver.reason import ReasonResult
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="t", name="t", category="web"))
    out = dispatch_intents(g, ReasonResult(
        goal_met=False,
        intents=[Intent("I1", "Probe /api/v3/admin for an IDOR"),
                 Intent("I2", "Decode the JWT and forge an admin token"),
                 Intent("I3", "Fetch HAR files from har.telos-health.local"),
                 Intent("I4", "Query the Wayback Machine for archived JS")],
        audit_notes=[]))
    assert len(out) == 4, "distinct directions must not be over-pruned"
    g.close()


# ── verdict state machine (complete / course_correct / explore) ──

def test_verdict_explicit_complete():
    r = parse_reason_reply(
        '{"verdict":"complete","complete_why":"flag printed in real stdout",'
        '"intents":[]}')
    assert r.verdict == "complete"
    assert r.goal_met is True          # complete implies goal_met for old callers
    assert "real stdout" in r.complete_why


def test_verdict_course_correct_carries_drift():
    r = parse_reason_reply(
        '{"verdict":"course_correct","drift":"stuck probing /login; flag is on /home",'
        '"intents":[{"id":"I1","goal":"follow the redirect to /home"}]}')
    assert r.verdict == "course_correct"
    assert "home" in r.drift
    assert r.intents[0].goal.startswith("follow the redirect")


def test_verdict_defaults_explore():
    # no verdict field, goal not met → explore (back-compat with old schema)
    r = parse_reason_reply('{"goal_met":false,"intents":[{"id":"I1","goal":"recon"}]}')
    assert r.verdict == "explore"
    assert r.goal_met is False


def test_verdict_derived_from_goal_met():
    # old schema: goal_met:true with no verdict → complete
    r = parse_reason_reply('{"goal_met":true,"intents":[]}')
    assert r.verdict == "complete"


def test_verdict_invalid_falls_back():
    r = parse_reason_reply('{"verdict":"banana","goal_met":false,"intents":[]}')
    assert r.verdict == "explore"  # unknown verdict → derived, not crashed
