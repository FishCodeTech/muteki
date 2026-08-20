"""SwarmF11 — agent teams swarm (named durable teammates + mailbox + task list)."""

from __future__ import annotations

import asyncio
import json
import re
import time
from typing import Any, Optional

from muteki.frameworks.f11_agent_teams.schema import (
    F11_FEATURE_KINDS,
    ensure_f11_schema_on_graph,
)
from muteki.frameworks.f11_agent_teams.team import (
    channel_messages,
    check_health,
    create_task,
    distill_channel_digest,
    form_team,
    heartbeat,
    lead_status,
    list_messages,
    list_tasks,
    pass_token,
    record_cost,
    replace_member,
    role_hat_guidance,
    send_message,
    try_lead_call,
    write_assertion,
)
from muteki.swarm.swarm import Swarm

LEAD_SYSTEM = (
    "You are the team lead of a CTF solver team (f11-agent-teams). You do NOT "
    "execute; you only make global rulings: team briefing, plan approval, "
    "contradiction arbitration, closing synthesis. Answer with a single JSON "
    "object. Be terse — evidence-backed claims only."
)
DIGEST_MODEL = "grok-4.5-low"
LEAD_MODEL_DEFAULT = "glm-5.2:cloud"
DIGEST_INTERVAL_S = 60.0
DIGEST_MSG_THRESHOLD = 15


def _parse_json_object(text: str) -> dict[str, Any] | None:
    """Tolerant JSON-object extraction from an LLM answer."""
    if not text:
        return None
    m = re.search(r"\{.*\}", text, flags=re.DOTALL)
    if not m:
        return None
    try:
        obj = json.loads(m.group(0))
    except Exception:
        return None
    return obj if isinstance(obj, dict) else None


class SwarmF11(Swarm):
    """Team-first cognition: lead (glm) + durable role-hatted teammates.

    All mechanisms are class-owned defaults. Does not read MUTEKI_* experimental
    env keys (runner clears them per cell).

    Coordinator role here is MECHANICAL only (design §4.1): roster/lease/fence
    bookkeeping, mailbox mirroring, health checks, budget accounting, and lead
    call proxying. It never claims tasks on a member's behalf and never sends
    heartbeat messages for members — teammates do both themselves through the
    blackboard skill in --mode=teammate.
    """

    architecture_name = "f11-agent-teams"
    framework_id = "f11"
    f11_enabled = True
    reason_declaration_mode = None

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self.f11_enabled = True
        self.reason_declaration_mode = None
        self._f11_late_init()

    def _f11_late_init(self) -> None:
        """Attribute defaults (also backstops tests that build via __new__)."""
        defaults: dict[str, Any] = {
            "_f11_ready": False,
            "_f11_bootstrapped": False,
            "_f11_mirror_seq": 0,
            "_f11_team_id": "",
            "_f11_members": [],
            "_f11_tasks": [],
            "_f11_token": {},
            "_f11_guidance": {},
            "_f11_intent_role": {},
            "_f11_intent_member": {},
            "_f11_intent_task": {},
            "_f11_role_rr": 0,
            "_f11_msg_after": 0,
            "_f11_lead_calls": 0,
            # T05 lead machinery
            "_f11_lead_pending": [],  # [{"purpose", "prompt", "msg_id"?}]
            "_f11_lead_handled": set(),  # msg_ids already ruled on (1 plan 1 verdict)
            "_f11_briefed": False,
            "_f11_finalized": False,
            "_f11_finalize_triggered": False,
            # T07 ledger feed from intent_concluded events (f05 pattern)
            "_f11_concluded_seq": 0,
            "_f11_cost_recorded": set(),
            # T13 digest trigger state
            "_f11_last_digest_at": 0.0,
            "_f11_digest_hi": 0,
        }
        for k, v in defaults.items():
            if not hasattr(self, k):
                setattr(self, k, v.copy() if isinstance(v, (dict, list, set)) else v)

    # -- schema / bootstrap -------------------------------------------------

    def _f11_ensure(self) -> None:
        self._f11_late_init()
        if self._f11_ready:
            return
        g = getattr(self, "shared_graph", None)
        if g is None:
            return
        if not ensure_f11_schema_on_graph(g):
            return
        self._f11_ready = True
        self._f11_bootstrap_seed()

    def _f11_bootstrap_seed(self) -> None:
        if self._f11_bootstrapped:
            return
        g = getattr(self, "shared_graph", None)
        if g is None:
            return
        cid = str(getattr(self.challenge, "id", "") or "unknown")
        run_id = str(getattr(self, "run_id", "") or cid)
        try:
            formed = form_team(
                g,
                challenge_id=cid,
                run_id=run_id,
                roles=("recon", "exploit", "verify"),
                lead_model=str(
                    getattr(self, "reason_model", "") or LEAD_MODEL_DEFAULT
                ),
                lead_calls_used=0,
            )
            self._f11_team_id = str(formed.get("team_id") or f"team-{cid}")
            self._f11_members = list(formed.get("members") or [])
            self._f11_tasks = list(formed.get("tasks") or [])
            self._f11_token = dict(formed.get("token") or {})
            self._f11_lead_calls = int(formed.get("lead_calls_used") or 0)
            # Seed role guidance for bootstrap intents.
            for m in self._f11_members:
                self._f11_guidance[str(m.get("name"))] = role_hat_guidance(
                    str(m.get("role") or ""),
                    member_name=str(m.get("name") or ""),
                    team_id=self._f11_team_id,
                )
            # Direct intro message + channel ambient post (T03/T13). Tasks stay
            # PENDING — teammates claim them themselves via the skill (T11);
            # the coordinator never claims on anyone's behalf.
            if self._f11_members:
                send_message(
                    g,
                    team_id=self._f11_team_id,
                    kind="direct",
                    from_member="lead",
                    to=[str(self._f11_members[0].get("name"))],
                    body="team formed; claim seed tasks via task-claim; "
                    "evidence-only assertions",
                    require_ack=False,
                )
                send_message(
                    g,
                    team_id=self._f11_team_id,
                    kind="channel",
                    from_member="lead",
                    to=["*"],
                    body="bootstrap: surface inventory open",
                    channel_trace_kind="surprise",
                )
            # T05: queue the team briefing for the next async lead pump.
            self._f11_queue_lead("briefing", self._f11_briefing_prompt())
        except Exception:
            pass
        self._f11_bootstrapped = True

    # -- T05 lead (glm) ------------------------------------------------------

    def _f11_lead_model(self) -> str:
        g = getattr(self, "shared_graph", None)
        if g is not None and self._f11_team_id:
            try:
                model = str(lead_status(g, team_id=self._f11_team_id).get("lead_model") or "")
                if model:
                    return model
            except Exception:
                pass
        return str(getattr(self, "reason_model", "") or LEAD_MODEL_DEFAULT)

    def _f11_briefing_prompt(self) -> str:
        ch = getattr(self, "challenge", None)
        desc = str(getattr(ch, "description", "") or getattr(ch, "goal", "") or "")
        seed = list(self._f11_tasks)
        return (
            "Team briefing for a CTF challenge.\n"
            f"challenge_id={getattr(ch, 'id', '')} category={getattr(ch, 'category', '')}\n"
            f"description: {desc[:1500]}\n"
            f"roster: {[m.get('name') for m in self._f11_members]} "
            f"seed_tasks={seed}\n"
            'Return JSON: {"summary": str, "tasks": [{"goal": str, "role": str, '
            '"declared_effects": [{"selector": str, "op": str, "value": ...}]}]} '
            "with at most 6 refined tasks."
        )

    def _f11_queue_lead(self, purpose: str, prompt: str, msg_id: str = "") -> None:
        if msg_id and msg_id in self._f11_lead_handled:
            return
        self._f11_lead_pending.append(
            {"purpose": purpose, "prompt": prompt, "msg_id": msg_id}
        )

    async def _f11_lead_call(self, purpose: str, prompt: str) -> str | None:
        """One real glm lead call behind the T05 budget/cooldown gate.

        Returns None (and burns NO budget) when no LLM is wired — deterministic
        fallbacks then apply and are labelled as such.
        """
        g = getattr(self, "shared_graph", None)
        llm = getattr(self, "llm", None)
        if g is None or not self._f11_team_id:
            return None
        if llm is None or not callable(getattr(llm, "chat", None)):
            return None
        gate = try_lead_call(g, team_id=self._f11_team_id, purpose=purpose)
        if not gate.get("ok"):
            return None
        self._f11_lead_calls = int(gate.get("calls_used") or self._f11_lead_calls)
        try:
            resp = await llm.chat(
                model=self._f11_lead_model(),
                messages=[
                    {"role": "system", "content": LEAD_SYSTEM},
                    {"role": "user", "content": prompt[:8000]},
                ],
                max_tokens=2000,
                stream=False,
                run_id=str(getattr(self, "run_id", "") or ""),
                challenge_id=str(getattr(self.challenge, "id", "") or ""),
                solver_id="f11-lead",
            )
        except Exception:
            return None
        content = str(getattr(resp, "content", "") or "").strip()
        return content or None

    async def _f11_pump_lead(self) -> None:
        """Discharge queued lead triggers (cooldown/cap enforced per call)."""
        while self._f11_lead_pending:
            item = self._f11_lead_pending.pop(0)
            purpose = str(item.get("purpose") or "")
            prompt = str(item.get("prompt") or "")
            msg_id = str(item.get("msg_id") or "")
            if msg_id:
                self._f11_lead_handled.add(msg_id)
            text = await self._f11_lead_call(purpose, prompt)
            if text is None:
                continue  # gate closed or no LLM — deterministic path stands
            if purpose == "briefing":
                self._f11_apply_briefing(text)
            elif purpose == "plan_verdict":
                self._f11_apply_plan_verdict(text, msg_id)
            elif purpose == "contradiction":
                self._f11_apply_contradiction_ruling(text, msg_id)
            elif purpose == "closing":
                self._f11_apply_closing(text)
            # "dead_letter" needs no state mutation beyond the audit event that
            # try_lead_call already emitted.

    def _f11_apply_briefing(self, text: str) -> None:
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        data = _parse_json_object(text) or {}
        summary = str(data.get("summary") or text)[:600]
        created: list[str] = []
        for t in list(data.get("tasks") or [])[:6]:
            if not isinstance(t, dict):
                continue
            goal = str(t.get("goal") or "")[:400]
            if not goal:
                continue
            effects = t.get("declared_effects")
            tid = create_task(
                g,
                team_id=self._f11_team_id,
                goal=goal,
                created_by="lead",
                declared_effects=effects if isinstance(effects, list) else [],
            )
            if tid:
                created.append(tid)
        self._f11_tasks.extend(created)
        send_message(
            g,
            team_id=self._f11_team_id,
            kind="broadcast",
            from_member="lead",
            to=["*"],
            body=f"briefing: {summary}",
        )
        self._f11_briefed = True

    def _f11_apply_plan_verdict(self, text: str, msg_id: str) -> None:
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        data = _parse_json_object(text) or {}
        verdict = str(data.get("verdict") or "review")[:40]
        reason = str(data.get("reason") or text)[:600]
        requester = ""
        try:
            req = [
                m
                for m in list_messages(g, team_id=self._f11_team_id, after_seq=0, limit=10000)
                if m.get("msg_id") == msg_id
            ]
            requester = str(req[0].get("from") or "") if req else ""
        except Exception:
            requester = ""
        send_message(
            g,
            team_id=self._f11_team_id,
            kind="plan_verdict",
            from_member="lead",
            to=[requester] if requester else ["*"],
            body=f"{verdict}: {reason}",
        )

    def _f11_apply_contradiction_ruling(self, text: str, msg_id: str) -> None:
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        data = _parse_json_object(text) or {}
        ruling = str(data.get("ruling") or text)[:600]
        # Source-disjoint ruling goes back to the whole team; assertion mutation
        # stays evidence-gated (the ruling itself is NOT evidence).
        send_message(
            g,
            team_id=self._f11_team_id,
            kind="broadcast",
            from_member="lead",
            to=["*"],
            body=f"contradiction ruling: {ruling}",
        )

    def _f11_apply_closing(self, text: str) -> None:
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        send_message(
            g,
            team_id=self._f11_team_id,
            kind="broadcast",
            from_member="lead",
            to=["*"],
            body=f"closing synthesis: {text[:600]}",
        )
        self._f11_finalized = True

    # -- mirroring -----------------------------------------------------------

    async def _f11_mirror_features(self) -> None:
        g = getattr(self, "shared_graph", None)
        emit = getattr(self, "_emit_coord_bb", None)
        if g is None or not callable(emit):
            return
        try:
            feature_events = g.events_since(
                self._f11_mirror_seq, kinds=list(F11_FEATURE_KINDS)
            )
        except Exception:
            feature_events = []
        for event in feature_events:
            payload = event.get("payload") or {}
            if not isinstance(payload, dict):
                payload = {}
            # Message events carry their own "kind" field in the payload — the
            # event kind wins the emit signature, so drop colliding keys.
            fields = {
                k: v for k, v in payload.items() if k not in ("kind", "graph_seq")
            }
            await emit(
                str(event.get("kind") or "team_formed"),
                graph_seq=int(event.get("seq") or 0),
                **fields,
            )
            self._f11_mirror_seq = max(
                self._f11_mirror_seq, int(event.get("seq") or 0)
            )

    # -- framework hooks -----------------------------------------------------

    def framework_prepare_hook(self) -> None:
        self._f11_ensure()
        # Prepare hook is sync; discharge the briefing on the running loop if
        # there is one, otherwise the next framework_after_workers beat picks it
        # up from _f11_lead_pending.
        if self._f11_lead_pending:
            try:
                loop = asyncio.get_running_loop()
            except RuntimeError:
                return
            loop.create_task(self._f11_pump_lead_guarded())

    async def _f11_pump_lead_guarded(self) -> None:
        try:
            await self._f11_pump_lead()
        except Exception:
            pass

    def framework_on_intents_proposed(self, proposed: list[dict]) -> None:
        self._f11_ensure()
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        roles = ["recon", "exploit", "verify"]
        for it in list(proposed or []):
            iid = str(it.get("intent_id") or "")
            goal = str(it.get("goal") or "")
            if not iid:
                continue
            role = roles[self._f11_role_rr % len(roles)]
            self._f11_role_rr += 1
            member = next(
                (m for m in self._f11_members
                 if m.get("role") == role and m.get("state") != "dead"),
                next(
                    (m for m in self._f11_members if m.get("state") != "dead"),
                    {"name": f"{role}-1"},
                ),
            )
            mname = str(member.get("name") or f"{role}-1")
            tid = create_task(
                g,
                team_id=self._f11_team_id,
                goal=goal or f"{role} work",
                created_by="lead",
                declared_effects=[
                    {"selector": f"intent.{iid}.progress", "op": "eq", "value": True}
                ],
            )
            if not tid:
                continue  # T07 circuit open — no new tasks
            self._f11_intent_role[iid] = role
            self._f11_intent_member[iid] = mname
            self._f11_intent_task[iid] = tid
            # The TASK STAYS PENDING: the teammate claims it atomically itself
            # via `blackboard.py --mode=teammate task-claim` (T11). The handoff
            # only NAMES the intended claimant.
            send_message(
                g,
                team_id=self._f11_team_id,
                kind="handoff",
                from_member="lead",
                to=[mname],
                body=f"claim task {tid} via task-claim: {goal[:200]}",
            )
            self._f11_guidance[iid] = (
                role_hat_guidance(role, member_name=mname, team_id=self._f11_team_id)
                + f"\nYou are teammate '{mname}'. Pending task={tid} — claim it: "
                f"`blackboard.py --mode=teammate task-claim {tid}`; send "
                "`blackboard.py --mode=teammate heartbeat` ~every 30s; finish with "
                f"`task-done {tid} --evidence kind:ref:digest`."
            )

    def framework_worker_guidance_for_intent(self, intent_id: str) -> list[str]:
        g = self._f11_guidance.get(str(intent_id) or "")
        if g:
            return [g]
        # Fall back to first member hat so bootstrap workers still see team mode.
        if self._f11_members:
            m = self._f11_members[0]
            return [
                role_hat_guidance(
                    str(m.get("role") or "recon"),
                    member_name=str(m.get("name") or "recon-1"),
                    team_id=self._f11_team_id or "team",
                )
            ]
        return []

    async def framework_after_workers(self, *_a: Any, **_k: Any) -> None:
        self._f11_ensure()
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        # T07 ledger: the base Swarm never calls framework_record_worker_outcome
        # (dormant hook), so cost + the +30% breaker learn from intent_concluded
        # events on the append-only log instead (f05/f10 pattern).
        try:
            self._f11_settle_costs_from_events()
        except Exception:
            pass
        # Mailbox scan: mirror progress and surface lead triggers (T05).
        try:
            msgs = list_messages(
                g, team_id=self._f11_team_id, after_seq=self._f11_msg_after, limit=40
            )
            if msgs:
                self._f11_msg_after = max(int(m["seq"]) for m in msgs)
        except Exception:
            msgs = []
        for m in msgs:
            kind = str(m.get("kind") or "")
            mid = str(m.get("msg_id") or "")
            if kind == "plan_request" and mid not in self._f11_lead_handled:
                self._f11_queue_lead(
                    "plan_verdict",
                    "Plan approval request from teammate "
                    f"{m.get('from')}: {str(m.get('body') or '')[:1500]}\n"
                    'Return JSON: {"verdict": "approve"|"reject", "reason": str}. '
                    "One plan, one verdict — resubmission needs new evidence.",
                    msg_id=mid,
                )
            elif kind == "contradiction" and mid not in self._f11_lead_handled:
                self._f11_queue_lead(
                    "contradiction",
                    "Contradiction surfaced by "
                    f"{m.get('from')}: {str(m.get('body') or '')[:1500]}\n"
                    "Rule source-disjoint on the evidence_refs, not on seniority. "
                    'Return JSON: {"ruling": str}.',
                    msg_id=mid,
                )
        # Promote verified evidence into assertions (T04).
        facts: list[str] = []
        try:
            for f in list(g.verified_evidence() or [])[-6:]:
                if isinstance(f, dict):
                    t = str(f.get("text") or f.get("fact") or "")
                    if t:
                        facts.append(t)
        except Exception:
            facts = []
        for text in facts[:3]:
            write_assertion(
                g,
                team_id=self._f11_team_id,
                text=text[:200],
                evidence_refs=[
                    {"kind": "event", "ref": "verified_evidence", "digest": "graph"}
                ],
                confidence=0.8,
            )
        if facts:
            send_message(
                g,
                team_id=self._f11_team_id,
                kind="channel",
                from_member=str(
                    (self._f11_members[0] if self._f11_members else {}).get(
                        "name", "recon-1"
                    )
                ),
                to=["*"],
                body=facts[0][:200],
                channel_trace_kind="evidence",
                evidence_refs=[
                    {"kind": "event", "ref": "verified_evidence", "digest": "graph"}
                ],
            )
        # T13 digest: grok-low distillation, triggered every 60s OR every 15
        # channel messages (whichever first, §2.3a). Pull-style: the digest,
        # never the raw channel text, is what teammates consume.
        try:
            new_chans = channel_messages(
                g, team_id=self._f11_team_id, after_seq=self._f11_digest_hi
            )
        except Exception:
            new_chans = []
        now = time.time()
        if new_chans and (
            len(new_chans) >= DIGEST_MSG_THRESHOLD
            or now - float(self._f11_last_digest_at or 0.0) >= DIGEST_INTERVAL_S
        ):
            await self._f11_distill(new_chans)
            self._f11_last_digest_at = now
            self._f11_digest_hi = max(int(c["seq"]) for c in new_chans)
        # T12: heartbeat-based health, lease sweep, same-role replacement.
        try:
            health = check_health(g, team_id=self._f11_team_id)
        except Exception:
            health = {}
        for dead_name in list((health or {}).get("dead") or []):
            try:
                repl = replace_member(g, team_id=self._f11_team_id, dead_member=dead_name)
            except Exception:
                repl = {}
            if repl.get("ok"):
                new_member = dict(repl.get("member") or {})
                self._f11_members.append(new_member)
                brief = str(repl.get("context_brief") or "")
                nname = str(new_member.get("name") or "")
                self._f11_guidance[nname] = (
                    role_hat_guidance(
                        str(new_member.get("role") or "recon"),
                        member_name=nname,
                        team_id=self._f11_team_id,
                    )
                    + f"\nYou replace {dead_name}. Rebuilt team context:\n{brief}"
                )
                send_message(
                    g,
                    team_id=self._f11_team_id,
                    kind="handoff",
                    from_member="lead",
                    to=[nname],
                    body=f"you replace {dead_name}; re-claim its released tasks",
                )
                self._f11_queue_lead(
                    "dead_letter",
                    f"teammate {dead_name} died (heartbeat timeout); replaced by "
                    f"{nname} (same role). Released tasks re-open for claim. "
                    'Return JSON: {"note": str} — any reprioritization?',
                )
        # Advance chain token among members when present (order primitive alive).
        tok = self._f11_token
        if tok.get("token_id") and len(self._f11_members) >= 2:
            names = [
                str(m.get("name"))
                for m in self._f11_members
                if m.get("state") != "dead"
            ]
            if len(names) >= 2:
                holder = str(tok.get("holder") or names[0])
                if holder not in names:
                    holder = names[0]
                try:
                    idx = names.index(holder)
                except ValueError:
                    idx = 0
                nxt = names[(idx + 1) % len(names)]
                passed = pass_token(
                    g,
                    team_id=self._f11_team_id,
                    token_id=str(tok["token_id"]),
                    from_holder=holder,
                    to_holder=nxt,
                    fence=int(tok.get("fence") or 1),
                )
                if passed.get("ok"):
                    self._f11_token = {
                        **tok,
                        "holder": nxt,
                        "fence": passed.get("fence"),
                    }
        # §4.4 finalize fallback: framework_finalize_hook has no caller in the
        # swarm core, so after_workers latches the closing synthesis itself —
        # exactly once — when every task is terminal OR a flag is already out.
        # Once latched, the closing stays queued until the lead gate lets it
        # through (budget/cooldown enforced per call by try_lead_call).
        if not self._f11_finalized:
            try:
                open_tasks = [
                    t
                    for t in list_tasks(g, team_id=self._f11_team_id, limit=100)
                    if t.get("status") in ("pending", "claimed")
                ]
            except Exception:
                open_tasks = []
            if not self._f11_finalize_triggered and (
                not open_tasks or self._f11_flag_seen(g)
            ):
                self._f11_finalize_triggered = True
            if self._f11_finalize_triggered and not any(
                p.get("purpose") == "closing" for p in self._f11_lead_pending
            ):
                self._f11_queue_lead("closing", self._f11_closing_prompt())
        await self._f11_pump_lead_guarded()
        await self._f11_mirror_features()

    def _f11_settle_costs_from_events(self) -> None:
        """T07 ledger feed from intent_concluded graph events (idempotent).

        The base Swarm never calls ``framework_record_worker_outcome`` (dormant
        hook upstream — same finding as f04/f05/f10), so the team cost ledger
        and the +30% circuit breaker learn from the append-only event log
        instead. Only intents owned by this team's members count, and only
        when the event carries a real cost — no fabricated numbers. The seq
        cursor plus the recorded-set keep replays from double-counting spend.
        """
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        try:
            events = g.events_since(
                int(getattr(self, "_f11_concluded_seq", 0)),
                kinds=["intent_concluded"],
            )
        except Exception:
            return
        for event in events or []:
            try:
                seq = int(event.get("seq") or 0)
            except (TypeError, ValueError):
                seq = 0
            self._f11_concluded_seq = max(self._f11_concluded_seq, seq)
            payload = event.get("payload") or {}
            if not isinstance(payload, dict):
                continue
            iid = str(payload.get("intent_id") or "")
            if not iid or "," in iid:  # bulk supersede marker
                continue
            if iid in self._f11_cost_recorded:
                continue
            if iid not in self._f11_intent_member:
                continue  # not this team's worker — no team budget impact
            raw_cost = payload.get("cost_usd", payload.get("cost"))
            try:
                cost = float(raw_cost)
            except (TypeError, ValueError):
                continue  # event carries no cost — nothing to ledger
            if cost <= 0:
                continue
            try:
                record_cost(g, team_id=self._f11_team_id, cost_usd=cost)
                self._f11_cost_recorded.add(iid)
            except Exception:
                pass

    def _f11_flag_seen(self, g: Any) -> bool:
        """A flag is already out (accepted by the gate) — finalize condition."""
        try:
            if list(getattr(g, "flags", None) or []):
                return True
        except Exception:
            pass
        try:
            return bool(g.events_since(0, kinds=["flag_found"]))
        except Exception:
            return False

    def _f11_closing_prompt(self) -> str:
        g = getattr(self, "shared_graph", None)
        tasks: list[dict[str, Any]] = []
        if g is not None and self._f11_team_id:
            try:
                tasks = list_tasks(g, team_id=self._f11_team_id, limit=50)
            except Exception:
                tasks = []
        done = [t for t in tasks if t.get("status") == "done"]
        return (
            "Closing synthesis. The task list is drained.\n"
            f"done tasks: {[t.get('goal') for t in done][:10]}\n"
            'Return JSON: {"summary": str, "lessons": [str]} (≤3 lessons).'
        )

    async def _f11_distill(self, chans: list[dict[str, Any]]) -> None:
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        llm = getattr(self, "llm", None)
        distilled: str | None = None
        if llm is not None and callable(getattr(llm, "chat", None)):
            raw = "\n".join(
                f"[{c['seq']}] {c['from']} {c['kind']}: {str(c.get('body') or '')[:200]}"
                for c in chans
            )
            try:
                resp = await llm.chat(
                    model=DIGEST_MODEL,
                    messages=[
                        {
                            "role": "system",
                            "content": (
                                "Distill CTF team channel traces into a ≤300-token "
                                "digest, segmented by thread. Keep speaker labels "
                                "and seq ranges. Flag assertion candidates with "
                                "their evidence refs. No coordination content."
                            ),
                        },
                        {"role": "user", "content": raw[:6000]},
                    ],
                    max_tokens=600,
                    stream=False,
                    run_id=str(getattr(self, "run_id", "") or ""),
                    challenge_id=str(getattr(self.challenge, "id", "") or ""),
                    solver_id="f11-digest",
                )
                distilled = str(getattr(resp, "content", "") or "").strip() or None
            except Exception:
                distilled = None
        distill_channel_digest(
            g,
            team_id=self._f11_team_id,
            distilled_text=distilled,
            by_model=DIGEST_MODEL if distilled else None,
        )

    def framework_record_worker_outcome(
        self,
        *,
        engine: str,
        intent: Optional[dict],
        success: bool,
        cost_usd: float | None = None,
    ) -> None:
        """Worker-outcome bookkeeping (T07 ledger + T12 liveness evidence).

        The coordinator NEVER posts heartbeat messages on a member's behalf —
        real heartbeats come from the teammate via the skill. Here we only
        update the roster liveness timestamp from observed worker activity and
        accumulate cost for the circuit breaker.
        """
        del success
        self._f11_ensure()
        g = getattr(self, "shared_graph", None)
        if g is None or not self._f11_team_id:
            return
        iid = str((intent or {}).get("intent_id") or "")
        mname = self._f11_intent_member.get(iid)
        if mname:
            try:
                heartbeat(g, team_id=self._f11_team_id, member=mname)
            except Exception:
                pass
        if cost_usd:
            try:
                record_cost(g, team_id=self._f11_team_id, cost_usd=float(cost_usd))
            except Exception:
                pass

    async def framework_finalize_hook(self) -> None:
        """Defensive finalize entry (not yet called by swarm core — the
        after_workers §4.4 fallback is the live trigger): one closing lead
        synthesis within budget, then the team is done."""
        self._f11_ensure()
        if self._f11_finalized or not self._f11_team_id:
            return
        self._f11_finalize_triggered = True
        self._f11_queue_lead("closing", self._f11_closing_prompt())
        await self._f11_pump_lead_guarded()


Swarm = SwarmF11
