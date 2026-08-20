"""Rendering paths: reason/board/review summaries and their block helpers.

Split out of ``shared_graph.py`` (code-health G1) as a mixin of
``SQLiteSharedGraph``. Every method body is byte-for-byte the original; the mixin
is combined back into ``SQLiteSharedGraph`` so behavior and the public surface are
unchanged. Instance state (``self._conn``, ``self._lock``, ``self.challenge``,
``self._append``, the class-level caps, the ``normalize_*`` helpers, …) is
resolved through the composed class at runtime.
"""

from __future__ import annotations

import json  # noqa: F401
import hashlib  # noqa: F401
import re  # noqa: F401
import time  # noqa: F401
from difflib import SequenceMatcher  # noqa: F401
from pathlib import Path  # noqa: F401
from typing import Any, Optional  # noqa: F401

from muteki.models.solve_graph import Challenge, Evidence, SolveGraph  # noqa: F401
from muteki.solver.result_codes import is_genuine_giveup  # noqa: F401
from muteki.swarm.graph_defs import (  # noqa: F401
    EV_FACT_ADDED, EV_HYP_PROPOSED, EV_HYP_REFUTED, EV_DEAD_END,
    EV_INTENT_PROPOSED, EV_INTENT_CLAIMED, EV_INTENT_CONCLUDED, EV_FLAG_FOUND,
    EV_FLAG_INVALIDATED, EV_POC_SAVED, EV_POC_CLAIMED, EV_POC_CONCLUDED,
    EV_REVIEW_FINDING, EV_FACT_CHALLENGED, EV_FACT_REVALIDATED,
    EV_ROUTE_SUPPRESSED, EV_ROUTE_REOPENED, EV_BRANCH_SPLIT, EV_BRANCH_RESOLVED,
    EV_COORDINATOR_DIRECTIVE, EV_REVIEW_PROPOSAL, EV_REVIEW_PROPOSAL_DECISION,
    EV_LANE_LOCKED, EV_LANE_RELEASED, EV_INTENT_LANE_DEFERRED, EV_FACT_REJECTED,
    EV_FACT_MERGED, EV_FACT_SUPERSEDED, EV_FACT_PINNED, EV_INTENT_STATE_CHANGED,
    EV_OPERATOR_DIRECTIVE, EV_OPERATOR_DIRECTIVE_STATUS, EV_HITL_CLASSIFIED,
    EV_RESOURCE_LOCKED, EV_RESOURCE_RELEASED, EV_GRAPH_COMPACTED,
    FACT_STATE_UNRESOLVED, FACT_STATE_CHALLENGED, FACT_STATE_REVALIDATED,
    FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED,
    _FACT_TERMINAL_STATES, _FACT_STATES,
    INTENT_DISPATCH_ACTIVE, INTENT_DISPATCH_RESUME, INTENT_DISPATCH_RETIRED,
    INTENT_DISPATCH_CLOSED, _INTENT_DISPATCH_STATES,
    _SERVICE_DEFAULT_PORTS, _LANE_RISK_CLASSES, _FACT_ENGINE_PREFIX_RE,
    _normalize_fact_identity, _clean_lane_risk, _clean_lane_host, canonicalize_lane,
)


class _RenderMixin:
    def _attempted_intents_block(self, limit: int = 40) -> str:
        """Render CONCLUDED intents with each one's conclusion text, so the Reason
        planner sees what was already tried AND what came of it (run-11190: the
        planner kept re-proposing paraphrases of concluded directions because the
        summary never showed them). The result comes from the EV_INTENT_CONCLUDED
        event the row's result_seq points at; superseded/no-result rows render a
        placeholder. Most recent `limit` shown (oldest→newest); earlier ones are
        collapsed into a count line — a goal is one line, so 40 stays cheap."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.goal, e.payload, i.worker_class, i.route_hash, i.branch_id, "
                "i.result_detail FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.status='done' ORDER BY i.created_seq",
            ).fetchall()
        if not rows:
            return ""
        omitted = max(0, len(rows) - limit)
        lines = ["\n## Already attempted (concluded intents — do NOT re-propose; "
                 "build on their results)"]
        if omitted:
            lines.append(f"  (… {omitted} earlier attempted intents omitted)")
        for goal, payload, worker_class, route_hash, branch_id, row_detail in rows[-limit:]:
            result = ""
            detail = str(row_detail or "")
            if payload:
                try:
                    p = json.loads(payload) or {}
                    result = str(p.get("result", ""))
                    detail = detail or str(p.get("result_detail", ""))
                except (json.JSONDecodeError, TypeError):
                    result = ""
            tail = result.strip()[:80] if result.strip() else "(superseded / no result recorded)"
            if detail.strip():
                tail = f"{tail}: {detail.strip()[:220]}"
            meta = []
            if worker_class and worker_class != "code":
                meta.append(str(worker_class))
            if route_hash:
                meta.append(f"route={route_hash}")
            if branch_id:
                meta.append(f"branch={branch_id}")
            suffix = f" ({', '.join(meta)})" if meta else ""
            lines.append(f"- {str(goal)[:160]}{suffix} → {tail}")
        return "\n".join(lines)

    def challenged_facts(self) -> list[dict]:
        texts = self._fact_text_by_seq()
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, status, reason, verification_intent_id "
                "FROM fact_reviews WHERE challenge_id=? AND status='challenged' "
                "ORDER BY challenged_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"fact_seq": int(r[0]), "status": r[1], "reason": r[2] or "",
             "verification_intent_id": r[3] or "", "fact": texts.get(int(r[0]), "")}
            for r in rows
        ]

    def revalidated_facts(self) -> list[dict]:
        texts = self._fact_text_by_seq()
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, status, reason FROM fact_reviews "
                "WHERE challenge_id=? AND status='revalidated' ORDER BY revalidated_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"fact_seq": int(r[0]), "status": r[1], "reason": r[2] or "",
             "fact": texts.get(int(r[0]), "")}
            for r in rows
        ]

    def retired_facts(self, *, states: Optional[tuple[str, ...]] = None) -> list[dict]:
        """Facts in a terminal lifecycle state (rejected/merged/superseded) — for the
        review/audit board (kept visible but de-verified)."""
        if not self._table_exists("fact_states"):
            return []
        want = states or (FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED)
        texts = self._fact_text_by_seq(include_retired=True)
        q = ",".join("?" for _ in want)
        with self._lock:
            rows = self._conn.execute(
                f"SELECT fact_seq, state, reason, merged_seq FROM fact_states "
                f"WHERE challenge_id=? AND state IN ({q}) ORDER BY updated_seq",
                (self.challenge.id, *want),
            ).fetchall()
        merges: dict[int, int] = {}
        if self._table_exists("fact_merges"):
            with self._lock:
                mrows = self._conn.execute(
                    "SELECT from_fact_seq, to_fact_seq FROM fact_merges WHERE challenge_id=?",
                    (self.challenge.id,),
                ).fetchall()
            merges = {int(m[0]): int(m[1]) for m in mrows}
        return [
            {"fact_seq": int(r[0]), "state": r[1], "reason": r[2] or "",
             "fact": texts.get(int(r[0]), ""),
             "merged_into": merges.get(int(r[0]))}
            for r in rows
        ]

    def suppressed_routes(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT route_hash, label, reason, until_policy, suppressed_seq "
                "FROM routes WHERE challenge_id=? AND status='suppressed' "
                "ORDER BY suppressed_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"route_hash": r[0], "label": r[1], "reason": r[2] or "",
             "until": r[3] or "new_evidence", "suppressed_seq": r[4]}
            for r in rows
        ]

    def is_route_suppressed(self, route_hash: str) -> bool:
        route = self.normalize_route_hash(route_hash)
        with self._lock:
            row = self._conn.execute(
                "SELECT status FROM routes WHERE challenge_id=? AND route_hash=?",
                (self.challenge.id, route),
            ).fetchone()
        return bool(row and row[0] == "suppressed")

    def branches(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT branch_id, parent_id, title, assumption, prove_or_disprove, status "
                "FROM branches WHERE challenge_id=? ORDER BY created_seq, branch_id",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"branch_id": r[0], "parent_id": r[1] or "", "title": r[2] or "",
             "assumption": r[3] or "", "prove_or_disprove": r[4] or "",
             "status": r[5] or "open"}
            for r in rows
        ]

    def coordinator_directives(self) -> list[dict]:
        out: list[dict] = []
        for e in self.events():
            if e.get("kind") == EV_COORDINATOR_DIRECTIVE:
                p = dict(e.get("payload") or {})
                p["seq"] = e.get("seq")
                p["actor"] = e.get("actor")
                out.append(p)
        return out

    def latest_unconsumed_directive_seq(self, *, after_seq: int = 0,
                                        action: str = "") -> Optional[dict]:
        directives = [
            d for d in self.coordinator_directives()
            if int(d.get("seq") or 0) > int(after_seq or 0)
            and (not action or d.get("action") == action)
        ]
        return directives[-1] if directives else None

    def genuine_failures_for_route(self, route_hash: str) -> int:
        route = self.normalize_route_hash(route_hash)
        with self._lock:
            rows = self._conn.execute(
                "SELECT e.payload FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.challenge_id=? AND i.route_hash=? AND i.status='done'",
                (self.challenge.id, route),
            ).fetchall()
        count = 0
        for (payload,) in rows:
            result = ""
            if payload:
                try:
                    result = str((json.loads(payload) or {}).get("result", "")).lower()
                except (json.JSONDecodeError, TypeError):
                    result = ""
            if not result:
                continue
            if any(skip in result for skip in (
                "timeout", "timed out", "cancelled", "canceled", "steered",
                "oom", "killed", "route_suppressed", "superseded",
                "lane_deferred", "lane_blocked", "closed_by_solve",
            )):
                continue
            if any(tok in result for tok in (
                "dead", "failed", "no flag", "no verified flag", "gave up",
                "exhausted", "not exploitable",
            )):
                count += 1
        return count

    def _review_state_block(self) -> str:
        parts: list[str] = []
        challenged = self.challenged_facts()
        if challenged:
            parts.append("\n## Challenged facts (DO NOT treat as verified until revalidated)")
            for f in challenged[-30:]:
                parts.append(
                    f"- [#{f['fact_seq']}] {f['fact'][:160]} :: {f['reason'][:160]} "
                    f"(verify via {f['verification_intent_id']})")
        revalidated = self.revalidated_facts()
        if revalidated:
            parts.append("\n## Revalidated facts")
            for f in revalidated[-20:]:
                parts.append(f"- [#{f['fact_seq']}] {f['fact'][:160]} :: {f['reason'][:160]}")
        retired = self.retired_facts()
        if retired:
            parts.append("\n## Retired facts (rejected/merged/superseded — do NOT use as evidence)")
            for f in retired[-30:]:
                tag = f['state']
                if f['state'] == FACT_STATE_MERGED and f.get('merged_into'):
                    tag = f"merged→#{f['merged_into']}"
                parts.append(f"- [#{f['fact_seq']}] ({tag}) {f['fact'][:160]} :: {f['reason'][:160]}")
        suppressed = self.suppressed_routes()
        if suppressed:
            parts.append("\n## Suppressed routes (ordinary workers must not retry)")
            for r in suppressed[-30:]:
                parts.append(
                    f"- {r['route_hash']} ({r['label']}): {r['reason'][:180]} "
                    f"until={r['until']}")
        branches = self.branches()
        if branches:
            parts.append("\n## Open branches (do not mix incompatible assumptions)")
            for b in branches[-30:]:
                parts.append(
                    f"- {b['branch_id']} [{b['status']}]: {b['assumption'][:180]} "
                    f"→ {b['prove_or_disprove'][:180]}")
        directives = self.coordinator_directives()
        if directives:
            parts.append("\n## Review directives")
            for d in directives[-20:]:
                parts.append(
                    f"- #{d.get('seq')} {d.get('action')}[{d.get('priority','normal')}]: "
                    f"{str(d.get('directive',''))[:220]}")
        return "\n".join(parts)

    def _poc_block(self, *, limit: int = 30) -> str:
        rows = self.pocs(inheritable_only=False)
        if not rows:
            return ""
        visible = [p for p in rows if p.get("status") != "quarantined"]
        if not visible:
            return "\n## Shared PoCs\n- all saved PoCs are quarantined; do not inherit them"
        # Only the PoCs the linker actually mounts get the "./inherited/<poc_id>/"
        # promise (#10). A 'wip' PoC under a live lease, or a 'spent' one, is NOT
        # linked into any worker cwd, so advertising that path for it points at a
        # folder that doesn't exist. Split: inheritable (path promised) vs historical
        # (metadata only, no path). The inheritable set is exactly pocs(inheritable
        # _only=True) so the board and the linker never disagree.
        inheritable_ids = {p["poc_id"] for p in self.pocs(inheritable_only=True)}

        def _render(items: list[dict]) -> list[str]:
            out: list[str] = []
            for p in items:
                iid = f" intent={p['intent_id']}" if p.get("intent_id") else ""
                note = f" — {str(p.get('note') or '')[:100]}" if p.get("note") else ""
                out.append(f"- {p['poc_id']} ({p['status']}){iid}: "
                           f"{p['entry_command']}{note}")
            return out

        inheritable = [p for p in visible if p["poc_id"] in inheritable_ids]
        historical = [p for p in visible if p["poc_id"] not in inheritable_ids]
        lines: list[str] = []
        if inheritable:
            lines.append("\n## Inheritable PoCs (run/copy under ./inherited/<poc_id>/)")
            omitted = max(0, len(inheritable) - limit)
            if omitted:
                lines.append(f"  (... {omitted} older inheritable PoCs omitted)")
            lines.extend(_render(inheritable[-limit:]))
        if historical:
            # in-use (wip, currently leased) or spent — listed for context, but NOT
            # mounted; don't tell a worker to run them from ./inherited/.
            lines.append("\n## Historical PoCs (in-use or spent; metadata only, not mounted)")
            omitted = max(0, len(historical) - limit)
            if omitted:
                lines.append(f"  (... {omitted} older historical PoCs omitted)")
            lines.extend(_render(historical[-limit:]))
        return "\n".join(lines)

    def _standing_guidance_block(self, standing_guidance: Optional[list[str]]) -> str:
        items = [
            str(x).strip()
            for x in (standing_guidance or [])
            if str(x).strip()
        ]
        if not items:
            return ""
        lines = ["\n## Operator standing guidance (highest priority; guidance, not evidence)"]
        for item in items[-12:]:
            # Round-7: fruitless-interrupt / chain-completion packets carry
            # REQUIRED/FORBIDDEN replan constraints; keep them intact.
            limit = 1400 if (
                item.startswith("[fruitless-interrupt")
                or item.startswith("[chain-completion")
            ) else 500
            lines.append(f"- {item[:limit]}")
        return "\n".join(lines)

    def _operator_directives_block(self) -> str:
        """B: active operator directives the planner MUST prioritize (highest
        priority; guidance, not proven evidence)."""
        directives = self.operator_directives(active_only=True)
        if not directives:
            return ""
        lines = ["\n## Operator directives (MUST prioritize — guidance, not evidence)"]
        for d in directives[:12]:
            text = str(d.get("text") or "")
            # Round-7 fruitless-interrupt MUST constraint needs full wording.
            limit = 900 if text.startswith("[fruitless-interrupt") else 400
            lines.append(f"- [{d['action']}/{d['status']}] {text[:limit]}")
        return "\n".join(lines)

    def _forbidden_zones_block(self) -> str:
        """D/E: the exclusive lanes + held resource locks the planner must route
        AROUND (don't propose intents that collide with an active lock)."""
        parts: list[str] = []
        lanes = self.active_lanes()
        locks = self.active_resource_locks()
        if not lanes and not locks:
            return ""
        parts.append("\n## Forbidden zones (locked — do NOT propose conflicting work)")
        for lane in lanes[:20]:
            parts.append(f"- lane {lane['lane_key']} [{lane['owner_worker']}]")
        for rl in locks[:20]:
            parts.append(f"- resource {rl['resource_key']} (scope={rl['scope']}) "
                         f"[{rl['owner_worker']}]")
        return "\n".join(parts)

    def to_reason_summary(self, standing_guidance: Optional[list[str]] = None) -> str:
        """The PLANNER's board view: the uncapped [#seq]-labelled summary (all
        facts AND all dead-ends — the P1.5 un-blinding lifted only the evidence
        cap; dead-ends stayed clipped to the last 8, so long-run planners forgot
        old dead directions) plus the two intent sections the snapshot can't
        carry: in-flight (open/claimed) and attempted-with-results. REASON_SYSTEM
        references both section titles in its no-re-proposal rule.

        Phase 4: now also carries active operator directives (B, must-prioritize)
        and forbidden zones (D/E, locked lanes/resources to route around). Retired
        facts (rejected/merged/superseded) are already dropped by snapshot(); only
        dispatch_state='active' intents appear in the open-intents block."""
        relevant = self._reason_relevant_fact_seqs()
        parts = [self._summary_for_fact_seqs(relevant, max_dead_ends=10**9),
                 self._captured_flags_block(),   # defect-9: already-solved directions
                 self._captured_findings_block(),
                 self._standing_guidance_block(standing_guidance),
                 self._operator_directives_block(),
                 self._forbidden_zones_block(),
                 self._review_state_block(),
                 self._active_intent_lineage_block(),
                 self._open_intents_block(limit=24),
                 self._poc_block(),
                 self._attempted_intents_block()]
        return "\n".join(p for p in parts if p and p.strip())

    def _captured_flags_block(self) -> str:
        """defect-9: the flags the run already holds. Surfaced to the planner so it
        does NOT re-propose intents aiming at an already-captured flag (the ezrop-ROP
        re-do: a worker re-running a direction that already yielded flag1). Empty when
        no flags yet — single/zero-flag runs are byte-identical."""
        flags = self.snapshot().flags
        if not flags:
            return ""
        lines = "\n".join(f"  - {f}" for f in flags)
        return ("\n## Flags already captured (do NOT propose any intent to re-recover "
                "these — those directions are DONE):\n"
                f"{lines}\n")

    def _captured_findings_block(self) -> str:
        findings = self.snapshot().findings
        if not findings:
            return ""
        lines = []
        for f in findings:
            lines.append(
                f"  - {f.get('finding_class','')} {f.get('resource_id','')} "
                f"({f.get('identity_a','')} / {f.get('identity_b','')})"
            )
        body = "\n".join(lines)
        return (
            "\n## Gated findings already accepted (do NOT re-propose these; "
            "acceptance is the evidence predicate, not a verbal claim):\n"
            f"{body}\n"
        )

    def _credential_block(self, creds: "Optional[list[dict]]" = None) -> str:
        """The canonical credential / unlock-chain section (also used standalone as
        the inline prompt digest). Empty string when no creds qualify."""
        creds = self.canonical_credentials() if creds is None else creds
        if not creds:
            return ""
        chain = " → ".join(f"{c['entity']}:{c['value']}" for c in creds)
        return ("\n## Recovered credentials / unlock chain "
                "(heuristically derived — verify before trusting)\n"
                f"{chain}\n")

    def _brief_block(self) -> str:
        """The FULL, untruncated challenge brief for the board file. SolveGraph's
        to_summary caps the description at 300 chars — but the brief is exactly where
        target/connection blocks live (e.g. an `SSH Access` host/port/creds section,
        run-10070), so capping it forces workers to dig the target out of session
        files. The file has no budget, so carry the whole thing here.

        Target/attachments come from the prompt builder (not the graph), so we render
        what the SolveGraph snapshot has: the challenge description verbatim."""
        c = self.challenge
        desc = (getattr(c, "description", "") or "").strip()
        if not desc:
            return ""
        return ("\n## Challenge brief (full — read for target/connection details)\n"
                f"{desc}\n")

    def to_board_markdown(self) -> str:
        """The FULL board rendered for the workdir file (no truncation): the
        canonical credential chain on top, the FULL challenge brief (target/SSH
        block lives here), then the untruncated [#seq]-labelled fact summary, then
        open intents. The credential section is also the inline prompt digest
        (rendered alone via _credential_block).

        Uses to_summary(max_evidence=10**9, max_dead_ends=10**9) so stage-1 [-16]
        is defeated, ALL dead-ends are shown (a worker re-walking a long-ruled-out
        path is the same waste the planner suffers — see to_reason_summary), and
        the [#seq] labels (the stable fact ids that Reason cites via `from`)
        are preserved; the caller drops the stage-2 [:2000] clip by using this
        method instead of the inline path."""
        creds = self.canonical_credentials()
        parts = [self._credential_block(creds), self._brief_block(),
                 self.to_summary(max_evidence=10**9, max_dead_ends=10**9),
                 self._review_state_block(),
                 self._open_intents_block(),
                 self._poc_block(),
                 # P4: in-progress activities a teammate is doing right now (avoid
                 # redoing a nmap/brute already underway).
                 self._activity_locks_block(),
                 self._lane_locks_block(),
                 self._resource_locks_block(),
                 # P1-A: also show CONCLUDED directions (+ results) to WORKERS, not
                 # just the Reason planner. Without this the board was asymmetric
                 # (to_reason_summary had it, to_board_markdown didn't), so a new
                 # worker re-walked directions already attempted-and-concluded — the
                 # "重走老路" report. A goal is one line; the file has no budget.
                 self._attempted_intents_block()]
        return "\n".join(p for p in parts if p and p.strip())

    def to_review_summary(self) -> str:
        """Review-Arbiter's full audit view. It intentionally includes more than
        Reason's compact planner view: raw event tails, all fact classes, route
        state, branch state, intent lifecycle, PoCs, flags, and operator/review
        directives. It is still derived from append-only events/materialized views."""
        parts = [
            "# Review-Arbiter audit board",
            self._brief_block(),
            self.to_summary(max_evidence=10**9, max_dead_ends=10**9),
            self._captured_flags_block(),
            self._captured_findings_block(),
            self._review_state_block(),
            self._open_intents_block(),
            self._poc_block(limit=80),
            self._activity_locks_block(),
            self._lane_locks_block(),
            self._resource_locks_block(),
            self._attempted_intents_block(limit=120),
            "\n## Recent raw events",
        ]
        for e in self.events()[-80:]:
            payload = e.get("payload") or {}
            preview = json.dumps(payload, ensure_ascii=False, default=str)[:500]
            parts.append(
                f"- #{e.get('seq')} {e.get('kind')} actor={e.get('actor')} "
                f"verified={e.get('verified')} {preview}")
        return "\n".join(p for p in parts if p and str(p).strip())
