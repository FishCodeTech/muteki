"""Read paths: snapshots, summaries, and rendered board/reason views.

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
    EV_INTENT_PROPOSED, EV_INTENT_CLAIMED, EV_INTENT_CONCLUDED,
    EV_FLAG_FOUND, EV_FLAG_INVALIDATED,     EV_FINDING_FOUND, EV_FINDING_INVALIDATED,
    EV_REPORT_SUBMITTED, EV_REPORT_REJECTED, EV_REPORT_REPRO_DECISION,
    EV_REPORT_VALUE_DECISION, EV_REPORT_ACCEPTED,
    EV_POC_SAVED, EV_POC_CLAIMED, EV_POC_CONCLUDED,
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


class _QueriesViewsMixin:
    def invalidated_flags(self) -> set[str]:
        """Every flag the operator marked false (an EV_FLAG_INVALIDATED event).

        snapshot().flags already excludes these; this accessor lets the coordinator
        DROP a stale in-memory flag during reconciliation so a blacklisted flag can
        never count toward expected_flags (BUG③ cross-check). Cheap, read-only."""
        out: set[str] = set()
        with self._lock:
            rows = self._conn.execute(
                "SELECT payload FROM events WHERE challenge_id=? AND kind=?",
                (self.challenge.id, EV_FLAG_INVALIDATED),
            ).fetchall()
        for (payload,) in rows:
            try:
                bad = (json.loads(payload) or {}).get("flag")
            except Exception:
                bad = None
            if bad:
                out.add(bad)
        return out

    def invalidated_findings(self) -> set[str]:
        out: set[str] = set()
        with self._lock:
            rows = self._conn.execute(
                "SELECT payload FROM events WHERE challenge_id=? AND kind=?",
                (self.challenge.id, EV_FINDING_INVALIDATED),
            ).fetchall()
        for (payload,) in rows:
            try:
                bad = (json.loads(payload) or {}).get("finding_key")
            except Exception:
                bad = None
            if bad:
                out.add(str(bad))
        return out

    def events(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, ts, actor, kind, payload, artifact_id, verified, "
                "confidence FROM events ORDER BY seq"
            ).fetchall()
        out = []
        for seq, ts, actor, kind, payload, aid, verified, conf in rows:
            out.append({"seq": seq, "ts": ts, "actor": actor, "kind": kind,
                        "payload": json.loads(payload), "artifact_id": aid,
                        "verified": bool(verified), "confidence": conf})
        return out

    def recent_events(self, limit: int = 40) -> list[dict]:
        """Last `limit` events, oldest-first, filtered to this challenge.

        Unlike `events()[-limit:]` this is bounded at the SQL layer (no full-table
        scan) and scopes to challenge_id so a shared sessions DB stays correct.
        Used by the read-only btw observer to build a recent timeline snapshot.
        """
        if limit <= 0:
            return []
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, ts, actor, kind, payload, artifact_id, verified, "
                "confidence FROM events WHERE challenge_id=? "
                "ORDER BY seq DESC LIMIT ?",
                (self.challenge.id, int(limit)),
            ).fetchall()
        out = []
        for seq, ts, actor, kind, payload, aid, verified, conf in reversed(rows):
            try:
                p = json.loads(payload)
            except Exception:
                p = {}
            out.append({"seq": seq, "ts": ts, "actor": actor, "kind": kind,
                        "payload": p, "artifact_id": aid,
                        "verified": bool(verified), "confidence": conf})
        return out

    def events_since(self, after_seq: int, kinds: Optional[list[str]] = None) -> list[dict]:
        after = int(after_seq or 0)
        params: list[Any] = [after]
        kind_list = [str(k) for k in (kinds or []) if str(k)]
        where = "WHERE seq > ?"
        if kind_list:
            where += " AND kind IN (" + ",".join("?" for _ in kind_list) + ")"
            params.extend(kind_list)
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, ts, actor, kind, payload, artifact_id, verified, "
                f"confidence FROM events {where} ORDER BY seq",
                tuple(params),
            ).fetchall()
        out = []
        for seq, ts, actor, kind, payload, aid, verified, conf in rows:
            try:
                parsed = json.loads(payload)
            except (json.JSONDecodeError, TypeError):
                parsed = {}
            out.append({"seq": seq, "ts": ts, "actor": actor, "kind": kind,
                        "payload": parsed, "artifact_id": aid,
                        "verified": bool(verified), "confidence": conf})
        return out

    def intent_products(self, intent_id: str) -> list[int]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq FROM intent_products WHERE intent_id=? ORDER BY fact_seq",
                (intent_id,),
            ).fetchall()
        return [int(r[0]) for r in rows]

    def snapshot(self) -> SolveGraph:
        """Materialize (C) the event log into a read-only SolveGraph view.

        Facts in a TERMINAL lifecycle state (rejected/merged/superseded) are
        dropped — they failed review and must not pollute the planner/worker view
        or downstream writeups. challenged stays (shown, but de-verified)."""
        g = SolveGraph(challenge=self.challenge)
        fact_reviews = self._fact_review_map()
        fact_states = self._fact_state_map()
        for e in self.events():
            p = e["payload"]
            if e["kind"] == EV_FACT_ADDED:
                seq = int(e["seq"])
                st = fact_states.get(seq, {})
                if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                    continue  # A: rejected/merged/superseded facts leave the view
                status = fact_reviews.get(seq)
                verified = bool(e["verified"])
                confidence = e["confidence"]
                if status == "challenged":
                    verified = False
                    confidence = min(float(confidence or 0.4), 0.4)
                elif status == "revalidated":
                    eff = st.get("verified_effective")
                    verified = bool(e["verified"]) if eff is None else eff
                g.add_evidence(
                    source=p.get("source", ""), fact=p.get("fact", ""),
                    artifact_id=e["artifact_id"],
                    verified=verified, confidence=confidence,
                    source_solver=p.get("source_solver", ""),
                    witness=p.get("witness"), verifier=p.get("verifier", ""),
                )
            elif e["kind"] == EV_DEAD_END:
                g.mark_dead_end(p.get("reason", ""))
            elif e["kind"] == EV_FLAG_FOUND:
                # multi-flag: ACCUMULATE (was a last-wins overwrite that lost every
                # flag but the last). add_flag dedups + keeps flag==flags[0].
                g.add_flag(p.get("flag"))
            elif e["kind"] == EV_FLAG_INVALIDATED:
                # a false-positive flag was marked by the operator — drop just that
                # one from the set (preserving any other collected flags) AND record
                # it as permanently rejected. reject_flag is UNCONDITIONAL on the
                # rejected set: a flag invalidated here stays rejected even if its
                # EV_FLAG_FOUND has not yet replayed, or is re-emitted later by a
                # reopened worker (add_flag refuses anything in rejected_flags). This
                # closes the run-75379 invalidate→reopen→re-find→re-accept loop at the
                # durable layer — survivable across worker respawn.
                g.reject_flag(p.get("flag"))
            elif e["kind"] == EV_FINDING_FOUND:
                g.add_finding(p)
            elif e["kind"] == EV_FINDING_INVALIDATED:
                g.reject_finding(p.get("finding_key") or p)
            elif e["kind"] == EV_REPORT_ACCEPTED:
                g.add_vuln_report(p)
        return g

    def to_summary(self, max_evidence: int = 16,
                   max_dead_ends: Optional[int] = None) -> str:
        """Like SolveGraph.to_summary but with [seq] labels on each fact so the
        Reason model can reference specific facts by number in its `from` field."""
        base = self.snapshot().to_summary(max_evidence=max_evidence,
                                          max_dead_ends=max_dead_ends)
        seq_map = self._fact_seq_map()
        if not seq_map:
            return base
        for fact_text, seq in seq_map.items():
            short = fact_text[:80]
            old_marker = f") {short}"
            new_marker = f") [#{seq}] {short}"
            base = base.replace(old_marker, new_marker, 1)
        return base

    def _fact_seq_map(self) -> dict[str, int]:
        """Map fact text → event seq for the most recent facts."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, json_extract(payload, '$.fact') "
                "FROM events WHERE kind=? ORDER BY seq",
                (EV_FACT_ADDED,),
            ).fetchall()
        return {text: seq for seq, text in rows if text}

    def _fact_text_by_seq(self, *, include_retired: bool = False) -> dict[int, str]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, json_extract(payload, '$.fact') "
                "FROM events WHERE kind=? ORDER BY seq",
                (EV_FACT_ADDED,),
            ).fetchall()
        active = None if include_retired else self._active_fact_seq_set()
        return {
            int(seq): str(text)
            for seq, text in rows
            if text and (active is None or int(seq) in active)
        }

    def _fact_review_map(self) -> dict[int, str]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, status FROM fact_reviews WHERE challenge_id=?",
                (self.challenge.id,),
            ).fetchall()
        return {int(seq): str(status) for seq, status in rows}

    def _active_fact_seq_set(self) -> set[int]:
        """Fact seqs still usable as graph evidence.

        Terminal lifecycle states (rejected/merged/superseded) are audit history only:
        they must not participate in graph reachability, lineage display, or worker
        neighborhood prompts. Challenged facts remain active candidates, but their
        effective verified status is downgraded elsewhere.
        """
        states = self._fact_state_map()
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq FROM events WHERE challenge_id=? AND kind=?",
                (self.challenge.id, EV_FACT_ADDED),
            ).fetchall()
        out: set[int] = set()
        for (seq_raw,) in rows:
            seq = int(seq_raw)
            st = states.get(seq, {})
            if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                continue
            out.add(seq)
        return out

    def fact_seqs_for_texts(self, texts: list[str]) -> list[int]:
        """Resolve fact description strings to their event seq numbers."""
        m = self._fact_seq_map()
        return [m[t] for t in texts if t in m]

    def per_flag_evidence_chains(self) -> dict[str, list[str]]:
        """G: per-flag evidence chains for multi-flag writeups. For each captured
        flag, build the ordered VERIFIED-fact trail that led to it:

          1. INTENT-LINKED (preferred): the flag_found event carries intent_id →
             use that intent's source facts (intent_sources) + produced fact
             (to_fact_seq), in seq order. This is the precise per-flag path.
          2. TEMPORAL FALLBACK (no intent_id): every verified fact with seq < the
             flag's seq (the breadcrumb trail up to that flag's discovery).

        Single-flag runs return {flag: chain} too — the caller can fall back to the
        flat evidence_chain when only one flag exists (byte-identical behavior)."""
        # collect flag_found events (flag, seq, intent_id) and flag invalidations
        flag_events: list[tuple[str, int, str]] = []
        invalidated: set[str] = set()
        for e in self.events():
            if e["kind"] == EV_FLAG_FOUND:
                p = e["payload"] or {}
                fl = p.get("flag")
                if fl:
                    flag_events.append((fl, int(e["seq"]), str(p.get("intent_id") or "")))
            elif e["kind"] == EV_FLAG_INVALIDATED:
                bad = (e["payload"] or {}).get("flag")
                if bad:
                    invalidated.add(bad)
        if not flag_events:
            return {}
        texts = self._fact_text_by_seq()
        states = self._fact_state_map()

        def _live_verified(seq: int) -> bool:
            st = states.get(seq, {})
            if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                return False
            return True

        # verified fact seqs in order (origin verified OR revalidated, not retired)
        verified_seqs = [d["fact_seq"] for d in self.verified_evidence()]
        out: dict[str, list[str]] = {}
        for flag, fseq, intent_id in flag_events:
            if flag in invalidated:
                continue
            chain_seqs: list[int] = []
            if intent_id:
                # intent-linked: source facts + produced fact for this flag's intent
                with self._lock:
                    src_rows = self._conn.execute(
                        "SELECT fact_seq FROM intent_sources WHERE intent_id=? ORDER BY fact_seq",
                        (intent_id,),
                    ).fetchall()
                    to_row = self._conn.execute(
                        "SELECT to_fact_seq FROM intents WHERE intent_id=? AND challenge_id=?",
                        (intent_id, self.challenge.id),
                    ).fetchone()
                for (s,) in src_rows:
                    if s is not None and _live_verified(int(s)):
                        chain_seqs.append(int(s))
                if to_row and to_row[0] is not None and _live_verified(int(to_row[0])):
                    chain_seqs.append(int(to_row[0]))
            if not chain_seqs:
                # temporal fallback: verified facts discovered before this flag
                chain_seqs = [s for s in verified_seqs if s <= fseq]
            # de-dup preserve order, resolve to text
            seen: set[int] = set()
            chain: list[str] = []
            for s in chain_seqs:
                if s in seen:
                    continue
                seen.add(s)
                t = texts.get(s)
                if t:
                    chain.append(t)
            out[flag] = chain[:12]
        return out

    # ── P2A: canonical credential / unlock chain (read-side, text-derived) ────
    # A long unlock-chain challenge (run-10067: 22-level SSH ladder) buries the
    # reusable passwords in 90+ free-text facts; truncation then drops them and
    # workers re-walk from ghost0. We surface the chain as a first-class section
    # derived from the fact TEXT (where the password literally appears in a
    # verified fact) — NOT from the stored from/to edges, which are untrustworthy
    # (`from` = a truncation-blinded planner's self-report; `to` = the worker's
    # closing stdout-tail summary). See DESIGN_board_file_handoff §9.
    #
    # HARD false-positive guard (DESIGN §4-P2A): a free-text board mixes "password
    # is X" with "tried X but it FAILED". Promoting a failed guess into a section
    # labelled "reuse, do NOT re-derive" is STRICTLY WORSE than truncation. So we
    # promote ONLY verified facts that carry an explicit success cue and lack a
    # failure cue, and we label the section "verify before trusting".
    _CRED_SUCCESS_CUE = re.compile(
        r"(unlock|logg?ed in|logs? in|whoami|authenticat|succe|login (?:to|succeed)|"
        r"password (?:for|is|works|valid)|cred(?:ential)?s? (?:for|is)|"
        r"pass(?:word)? (?:works|valid))", re.I)
    _CRED_FAILURE_CUE = re.compile(
        r"(fail|denied|wrong|incorrect|invalid|rejected|tried but|does ?n'?t "
        r"work|decoy|red herring|not (?:a )?(?:valid|the right) )", re.I)
    # The ENTITY being unlocked: a level/user-ish token (ghost3, level4, bandit5,
    # user1, root, admin). Anchored to known CTF-ladder prefixes OR a bare
    # well-known account, to avoid lifting random words.
    _CRED_ENTITY = re.compile(
        r"\b((?:ghost|level|bandit|krypton|natas|user|stage|node|flag|box)\s?\d{1,3}"
        r"|root|admin|administrator)\b", re.I)
    # The VALUE: the credential token introduced by a password keyword
    # ("password X", "with X", "is X", zh 密码 X) OR an explicit entity:value pair.
    _CRED_VALUE_KW = re.compile(
        r"(?:password|passwd|pass|cred(?:ential)?s?|secret|key|密码|凭据)\s*"
        r"(?:for\s+\S+\s+)?(?:is|=|:|of|was|为|->|→)?\s*"
        r"[`'\"]?([A-Za-z0-9_+/=.\-]{6,64})[`'\"]?", re.I)
    # A bare credential-shaped token: a MIXED-CASE alphanumeric ≥8 chars (looks like
    # a real password, not an English word / hex blob / decimal). Used only as a
    # last resort when a fact has a success cue + an entity but no keyword-introduced
    # value — and only if EXACTLY ONE such token exists (ambiguity → emit nothing).
    _CRED_BARE = re.compile(r"\b([A-Za-z0-9_+/=.\-]{8,64})\b")
    _CRED_PAIR = re.compile(
        r"\b((?:ghost|level|bandit|krypton|natas|user|stage)\s?\d{1,3})\s*[:=]\s*"
        r"[`'\"]?([A-Za-z0-9_+/=.\-]{6,64})[`'\"]?", re.I)
    # tokens that look credential-shaped but are noise we must never emit as a value
    _CRED_VALUE_STOP = {"password", "passwd", "secret", "succeeds", "succeeded",
                        "returns", "returned", "whoami", "authenticates", "unlocked",
                        "credential", "credentials", "logged", "login"}
    # SSH/config option assignments (PubkeyAuthentication=no, StrictHostKeyChecking=yes,
    # IdentitiesOnly=yes) look like entity:value pairs but are flags, not passwords —
    # observed as a false-positive `ghost0:Authentication=no` row in run-10070.
    _CRED_VALUE_REJECT = re.compile(
        r"(?:^|=)(?:no|yes)$|authentication|hostkey|identit|knownhosts|stricthost|"
        r"pubkey|forwarding|batchmode|connecttimeout", re.I)

    # The credential belongs to the entity it UNLOCKS, not the one whose home it was
    # found in. "X authenticates as ghost3" / "is the ghost3 password" / "unlocks
    # ghost3" / "认证 ghost3" → the TARGET entity is ghost3, even if the fact opens
    # with "ghost2 hidden lead contains X". Prefer this target over a leading entity.
    _CRED_TARGET = re.compile(
        r"(?:authenticat\w*|logs? in|logg?ed in|unlock\w*|is the|为|认证|登录)\s+"
        r"(?:as\s+|password\s+(?:for|of)\s+|to\s+)?"
        r"((?:ghost|level|bandit|krypton|natas|user|stage)\s?\d{1,3})\b", re.I)

    @staticmethod
    def _norm_entity(ent: str) -> str:
        """ghost 3 / Ghost3 / GHOST3 → ghost3 (canonical dedup key)."""
        return re.sub(r"\s+", "", ent).lower()

    def canonical_credentials(self) -> list[dict]:
        """Deduped recovered-credential rows derived from VERIFIED fact text, in
        unlock order. Each row: {entity, value, seq}. Newest verified fact per
        entity wins. Returns [] when nothing qualifies (graceful — the raw facts
        are still rendered in the verified-facts section).

        Extraction is conservative by design (DESIGN §4-P2A false-positive guard):
        a row is emitted ONLY from a verified fact that (a) carries a success cue,
        (b) lacks a failure cue, AND (c) yields BOTH a level/user entity and a
        password-shaped value via a keyword/pair pattern. A miss is fine — the raw
        fact still appears in the verified-facts section; a wrong row would mislead
        workers told to reuse it, so we prefer to emit nothing when unsure."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, json_extract(payload,'$.fact'), verified "
                "FROM events WHERE kind=? ORDER BY seq",
                (EV_FACT_ADDED,),
            ).fetchall()
        by_entity: dict[str, dict] = {}
        for seq, fact, verified in rows:
            if not verified or not fact:
                continue                              # guard 1: verified only
            if self._CRED_FAILURE_CUE.search(fact):
                continue                              # guard 3: skip explicit failures
            if not self._CRED_SUCCESS_CUE.search(fact):
                continue                              # guard 2: require a success cue
            ent_m = self._CRED_ENTITY.search(fact)
            if not ent_m:
                continue                              # need a concrete entity
            # the entity the credential UNLOCKS (authenticates-as / is-the-X-password)
            # wins over a leading "found in X's home" entity — fixes mis-attributing
            # a cred to the box it was discovered on rather than the box it opens.
            tgt_m = self._CRED_TARGET.search(fact)
            entity = tgt_m.group(1) if tgt_m else ent_m.group(1)
            # value: prefer an explicit entity:value pair, else a keyword-introduced token
            value = None
            pair = self._CRED_PAIR.search(fact)
            if pair and not tgt_m:
                entity, value = pair.group(1), pair.group(2)
            elif pair:
                value = pair.group(2)                 # keep the target entity
            else:
                for cand in self._CRED_VALUE_KW.findall(fact):
                    if cand.lower() not in self._CRED_VALUE_STOP and not cand.isdigit():
                        value = cand
                        break
            if not value:
                # last resort: a value-first / no-keyword fact ("X authenticates as
                # ghostN"). Accept ONLY a strong, UNAMBIGUOUS password-shaped token:
                # mixed letters+digits, ≥8 chars, and EXACTLY ONE such token in the
                # fact (more than one → can't tell which is the cred → emit nothing).
                strong = [t for t in self._CRED_BARE.findall(fact)
                          if t.lower() not in self._CRED_VALUE_STOP
                          and self._norm_entity(t) != self._norm_entity(entity)
                          and re.search(r"[A-Za-z]", t) and re.search(r"\d", t)
                          and not re.fullmatch(r"[0-9a-f]{8,}", t.lower())]  # not pure hex
                uniq = list(dict.fromkeys(strong))
                if len(uniq) == 1:
                    value = uniq[0]
            if not value or value.lower() in self._CRED_VALUE_STOP:
                continue
            if self._CRED_VALUE_REJECT.search(value):
                continue                              # SSH/config flag, not a password
            key = self._norm_entity(entity)
            by_entity[key] = {"entity": self._norm_entity(entity), "value": value,
                              "seq": seq}
        # order by the seq each entity was (last) confirmed at → unlock order
        return sorted(by_entity.values(), key=lambda r: r["seq"])

    def _open_intents_block(self, limit: int = 24) -> str:
        """Render open/claimed intents (not in the SolveGraph snapshot — they live
        only in the intents table). Empty string when none."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT goal, status, worker, worker_class, route_hash, branch_id, "
                "priority, lane_key, risk_class FROM intents "
                "WHERE status IN ('open','claimed') AND dispatch_state='active' "
                "ORDER BY priority DESC, created_seq",
            ).fetchall()
        if not rows:
            return ""
        omitted = max(0, len(rows) - limit)
        rows = rows[-limit:]
        lines = ["\n## Open intents (directions in flight)"]
        if omitted:
            lines.append(f"  (... {omitted} earlier open intents omitted)")
        for goal, status, worker, worker_class, route_hash, branch_id, priority, lane_key, risk_class in rows:
            who = f" [{worker}]" if worker else ""
            meta = []
            if worker_class and worker_class != "code":
                meta.append(str(worker_class))
            if route_hash:
                meta.append(f"route={route_hash}")
            if branch_id:
                meta.append(f"branch={branch_id}")
            if lane_key:
                meta.append(f"lane={lane_key}")
            if risk_class:
                meta.append(f"risk={risk_class}")
            if int(priority or 0):
                meta.append(f"priority={int(priority or 0)}")
            suffix = f" ({', '.join(meta)})" if meta else ""
            lines.append(f"- ({status}){who} {str(goal)[:160]}{suffix}")
        return "\n".join(lines)

    def _intent_sources_map(self, intent_ids: Optional[set[str]] = None, *,
                            include_retired: bool = False) -> dict[str, list[int]]:
        params: list[Any] = []
        where = ""
        if intent_ids:
            where = "WHERE intent_id IN (" + ",".join("?" for _ in intent_ids) + ")"
            params.extend(sorted(intent_ids))
        with self._lock:
            rows = self._conn.execute(
                f"SELECT intent_id, fact_seq FROM intent_sources {where} ORDER BY fact_seq",
                tuple(params),
            ).fetchall()
        active = None if include_retired else self._active_fact_seq_set()
        out: dict[str, list[int]] = {}
        for iid, seq in rows:
            fact_seq = int(seq)
            if active is not None and fact_seq not in active:
                continue
            out.setdefault(str(iid), []).append(fact_seq)
        return out

    def _intent_products_map(self, intent_ids: Optional[set[str]] = None, *,
                             include_retired: bool = False) -> dict[str, list[int]]:
        params: list[Any] = []
        where = ""
        if intent_ids:
            where = "WHERE intent_id IN (" + ",".join("?" for _ in intent_ids) + ")"
            params.extend(sorted(intent_ids))
        with self._lock:
            rows = self._conn.execute(
                f"SELECT intent_id, fact_seq FROM intent_products {where} ORDER BY fact_seq",
                tuple(params),
            ).fetchall()
        active = None if include_retired else self._active_fact_seq_set()
        out: dict[str, list[int]] = {}
        for iid, seq in rows:
            fact_seq = int(seq)
            if active is not None and fact_seq not in active:
                continue
            out.setdefault(str(iid), []).append(fact_seq)
        return out

    def _active_intent_rows(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT intent_id, goal, status, worker, worker_class, route_hash, branch_id "
                "FROM intents WHERE status IN ('open','claimed') AND dispatch_state='active' "
                "ORDER BY priority DESC, created_seq",
            ).fetchall()
        return [
            {"intent_id": r[0], "goal": r[1], "status": r[2], "worker": r[3] or "",
             "worker_class": r[4] or "code", "route_hash": r[5] or "", "branch_id": r[6] or ""}
            for r in rows
        ]

    def _giveup_product_fact_seqs(self) -> set[int]:
        giveup = self._intent_giveup_map()
        if not giveup:
            return set()
        products = self._intent_products_map(include_retired=True)
        out: set[int] = set()
        for iid, seqs in products.items():
            if giveup.get(iid, False):
                out.update(seqs)
        return out

    def _active_fact_seqs_by_verified(self, *, verified: bool,
                                      limit: Optional[int] = None,
                                      exclude_giveup_products: bool = False) -> list[int]:
        states = self._fact_state_map()
        blocked = self._giveup_product_fact_seqs() if exclude_giveup_products else set()
        sql = "SELECT seq, verified FROM events WHERE challenge_id=? AND kind=? ORDER BY seq DESC"
        params: list[Any] = [self.challenge.id, EV_FACT_ADDED]
        if limit is not None:
            sql += " LIMIT ?"
            params.append(int(limit))
        with self._lock:
            rows = self._conn.execute(sql, tuple(params)).fetchall()
        out: list[int] = []
        for seq_raw, raw_verified in rows:
            seq = int(seq_raw)
            if seq in blocked:
                continue
            st = states.get(seq, {})
            state = st.get("state", FACT_STATE_UNRESOLVED)
            if st.get("retired") or state in _FACT_TERMINAL_STATES:
                continue
            eff = st.get("verified_effective")
            is_verified = bool(raw_verified) if eff is None else bool(eff)
            if state == FACT_STATE_CHALLENGED:
                is_verified = False
            if is_verified == bool(verified):
                out.append(seq)
        return list(reversed(out))

    def _latest_verified_fact_seqs(self, limit: Optional[int] = None, *,
                                   exclude_giveup_products: bool = False) -> list[int]:
        return self._active_fact_seqs_by_verified(
            verified=True, limit=limit,
            exclude_giveup_products=exclude_giveup_products)

    def _latest_candidate_fact_seqs(self, limit: Optional[int] = None, *,
                                    exclude_giveup_products: bool = False) -> list[int]:
        return self._active_fact_seqs_by_verified(
            verified=False, limit=limit,
            exclude_giveup_products=exclude_giveup_products)

    def pin_facts(self, *, actor: str, fact_seqs: list[int],
                  reason: str = "") -> list[int]:
        active = self._active_fact_seq_set()
        clean: list[int] = []
        seen: set[int] = set()
        for raw in fact_seqs or []:
            try:
                seq = int(raw)
            except (TypeError, ValueError):
                continue
            if seq <= 0 or seq in seen or seq not in active:
                continue
            seen.add(seq)
            clean.append(seq)
        if not clean:
            return []
        pinned: list[int] = []
        detail = (reason or "").strip()[:500]
        for seq in clean:
            ev_seq = self._append(
                EV_FACT_PINNED,
                actor,
                {"fact_seq": seq, "reason": detail},
                dedupe_key=f"fact-pinned::{self.challenge.id}::{seq}",
            )
            with self._lock:
                self._conn.execute(
                    "INSERT OR IGNORE INTO fact_pins "
                    "(fact_seq, challenge_id, actor, reason, pinned_seq) "
                    "VALUES (?,?,?,?,?)",
                    (seq, self.challenge.id, actor, detail,
                     ev_seq if ev_seq > 0 else 0),
                )
                self._conn.commit()
            pinned.append(seq)
        return pinned

    def pinned_fact_seqs(self, *, exclude_giveup_products: bool = False) -> list[int]:
        if not self._table_exists("fact_pins"):
            return []
        active = self._active_fact_seq_set()
        blocked = self._giveup_product_fact_seqs() if exclude_giveup_products else set()
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq FROM fact_pins WHERE challenge_id=? ORDER BY pinned_seq",
                (self.challenge.id,),
            ).fetchall()
        out: list[int] = []
        for (raw_seq,) in rows:
            seq = int(raw_seq)
            if seq in active and seq not in blocked:
                out.append(seq)
        return out

    def fact_pin_context(self, limit: int = 240) -> str:
        active = self._active_fact_seq_set() - self._giveup_product_fact_seqs()
        if not active:
            return ""
        states = self._fact_state_map()
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, json_extract(payload,'$.source'), "
                "json_extract(payload,'$.fact'), verified, confidence "
                "FROM events WHERE challenge_id=? AND kind=? ORDER BY seq DESC LIMIT ?",
                (self.challenge.id, EV_FACT_ADDED, int(limit)),
            ).fetchall()
        lines: list[str] = []
        for seq_raw, source, fact, raw_verified, confidence in reversed(rows):
            seq = int(seq_raw)
            if seq not in active or not fact:
                continue
            st = states.get(seq, {})
            eff = st.get("verified_effective")
            is_verified = bool(raw_verified) if eff is None else bool(eff)
            if st.get("state") == FACT_STATE_CHALLENGED:
                is_verified = False
            verdict = "verified" if is_verified else "candidate"
            lines.append(
                f"- [#{seq}] {verdict} ({source or 'unknown'}, "
                f"confidence={float(confidence or 0):.2f}) {str(fact)[:220]}"
            )
        if not lines:
            return ""
        return "## Fact retention index (model decides pinned_facts)\n" + "\n".join(lines)

    def _intent_giveup_map(self) -> dict[str, bool]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT intent_id, close_reason FROM intents WHERE challenge_id=?",
                (self.challenge.id,),
            ).fetchall()
        return {str(iid): is_genuine_giveup(str(reason or "")) for iid, reason in rows}

    def _reason_relevant_fact_seqs(self) -> set[int]:
        active = self._active_intent_rows()
        active_ids = {str(i["intent_id"]) for i in active}
        sources = self._intent_sources_map()
        products = self._intent_products_map()
        giveup = self._intent_giveup_map()
        producer_by_fact: dict[int, set[str]] = {}
        for iid, seqs in products.items():
            for seq in seqs:
                producer_by_fact.setdefault(seq, set()).add(iid)
        facts: set[int] = set()
        seen_intents: set[str] = set()
        stack = list(active_ids)
        while stack:
            iid = stack.pop()
            if iid in seen_intents:
                continue
            seen_intents.add(iid)
            for seq in sources.get(iid, []):
                facts.add(seq)
                for producer in producer_by_fact.get(seq, set()) - seen_intents:
                    if not giveup.get(producer, False):
                        stack.append(producer)
            for seq in products.get(iid, []):
                facts.add(seq)
        facts.update(self._latest_verified_fact_seqs(
            limit=8, exclude_giveup_products=True))
        facts.update(self.pinned_fact_seqs(exclude_giveup_products=True))
        facts.update(self._latest_candidate_fact_seqs(
            limit=16, exclude_giveup_products=True))
        return facts

    def _summary_for_fact_seqs(self, fact_seqs: set[int],
                               max_dead_ends: Optional[int] = None) -> str:
        c = self.challenge
        lines = [f"# Challenge: {c.name} [{c.category}] ({c.points} pts)"]
        fact_states = self._fact_state_map()
        want = sorted(fact_seqs)
        if want:
            q = ",".join("?" for _ in want)
            with self._lock:
                rows = self._conn.execute(
                    "SELECT seq, json_extract(payload,'$.source'), "
                    "json_extract(payload,'$.fact'), verified, confidence "
                    f"FROM events WHERE kind=? AND seq IN ({q}) ORDER BY seq",
                    (EV_FACT_ADDED, *want),
                ).fetchall()
            verified_lines: list[str] = []
            candidate_lines: list[str] = []
            for seq, source, fact, verified, confidence in rows:
                st = fact_states.get(int(seq), {})
                if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                    continue
                line = f"- ({source or 'unknown'}) [#{int(seq)}] {str(fact)[:240]}"
                if bool(verified):
                    verified_lines.append(line)
                else:
                    candidate_lines.append(f"{line} [UNVERIFIED] confidence={float(confidence or 0):.2f}")
            if verified_lines:
                lines.append("\n## Confirmed evidence")
                lines.extend(verified_lines)
            if candidate_lines:
                lines.append("\n## Candidates / needs verification")
                lines.extend(candidate_lines)
        with self._lock:
            rows = self._conn.execute(
                "SELECT json_extract(payload,'$.reason') FROM events "
                "WHERE kind=? ORDER BY seq",
                (EV_DEAD_END,),
            ).fetchall()
        reasons = [str(r[0]) for r in rows if r[0]]
        if max_dead_ends is not None:
            reasons = reasons[-int(max_dead_ends):]
        if reasons:
            lines.append("\n## Dead ends")
            lines.extend(f"- {r}" for r in reasons)
        return "\n".join(lines)

    def _active_intent_lineage_block(self, limit: int = 24) -> str:
        active = self._active_intent_rows()[-limit:]
        if not active:
            return ""
        texts = self._fact_text_by_seq()
        ids = {str(i["intent_id"]) for i in active}
        sources = self._intent_sources_map(ids)
        products = self._intent_products_map(ids)
        lines = ["\n## Active intent lineage"]
        for row in active:
            iid = str(row["intent_id"])
            src = sources.get(iid, [])
            prod = products.get(iid, [])
            src_txt = ", ".join(f"#{seq} {texts.get(seq, '')[:80]}" for seq in src) or "no source facts"
            prod_txt = ", ".join(f"#{seq}" for seq in prod) or "no products yet"
            lines.append(
                f"- {iid} ({row['status']}): {str(row['goal'])[:140]} <= {src_txt}; "
                f"products: {prod_txt}")
        return "\n".join(lines)

    def intent_neighborhood_block(self, intent_id: str, sibling_limit: int = 8) -> str:
        iid = (intent_id or "").strip()
        if not iid:
            return ""
        texts = self._fact_text_by_seq()
        sources = self._intent_sources_map({iid}).get(iid, [])
        if not sources:
            return ""
        source_set = set(sources)
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.intent_id, i.goal, i.status FROM intents i "
                "JOIN intent_sources s ON s.intent_id=i.intent_id "
                "WHERE s.fact_seq IN (" + ",".join("?" for _ in source_set) + ") "
                "AND i.intent_id<>? AND i.status IN ('open','claimed') "
                "AND i.dispatch_state='active' ORDER BY i.created_seq LIMIT ?",
                (*sorted(source_set), iid, int(sibling_limit)),
            ).fetchall()
        lines = ["\n## Intent graph neighborhood"]
        lines.append("Source facts:")
        for seq in sources[:12]:
            lines.append(f"- [#{seq}] {texts.get(seq, '')[:240]}")
        if rows:
            lines.append("Sibling intents sharing those facts:")
            for sid, goal, status in rows:
                lines.append(f"- {sid} ({status}): {str(goal)[:180]}")
        return "\n".join(lines)

    def open_goal_texts(self) -> list[str]:
        """Goal texts of every open/claimed intent — the dedup reference set for
        dispatch_intents' near-duplicate filter (reason.py). Claimed included:
        a direction someone is actively working must not be re-proposed either."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT goal FROM intents WHERE status IN ('open','claimed') "
                "AND dispatch_state='active' ORDER BY created_seq",
            ).fetchall()
        return [str(r[0]) for r in rows if r[0]]

    def dispatchable_goal_texts(self) -> list[str]:
        """Goal texts that can be claimed right now.

        This intentionally differs from open_goal_texts(): a live claimed intent is
        active for dedupe, but it is not dispatchable until its lease expires. Reason's
        starvation valve needs this narrower view to avoid treating a stale live claim
        as available work.
        """
        now = time.time()
        with self._lock:
            rows = self._conn.execute(
                "SELECT goal FROM intents WHERE dispatch_state='active' "
                "AND (status='open' OR (status='claimed' AND lease_until IS NOT NULL "
                "AND lease_until < ?)) ORDER BY priority DESC, created_seq",
                (now,),
            ).fetchall()
        return [str(r[0]) for r in rows if r[0]]

    def open_route_hashes(self) -> list[str]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT DISTINCT route_hash FROM intents "
                "WHERE status IN ('open','claimed') AND dispatch_state='active' "
                "AND route_hash IS NOT NULL AND route_hash<>'' "
                "AND worker_class NOT IN ('verifier','review') "
                "ORDER BY route_hash",
            ).fetchall()
        return [str(r[0]) for r in rows if r[0]]

    def coverage_intent_rows(self) -> list[dict]:
        """All non-review intents with status/dispatch/result for pentest P2 coverage."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.intent_id, i.goal, i.route_hash, i.status, i.dispatch_state, "
                "i.worker_class, e.payload FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.challenge_id=? AND i.worker_class NOT IN ('verifier','review') "
                "ORDER BY i.created_seq",
                (self.challenge.id,),
            ).fetchall()
        out: list[dict] = []
        for iid, goal, route, status, dispatch, wc, payload in rows:
            result = ""
            if payload:
                try:
                    result = str((json.loads(payload) or {}).get("result") or "")
                except (json.JSONDecodeError, TypeError):
                    result = ""
            out.append({
                "intent_id": iid,
                "goal": goal or "",
                "route_hash": route or "",
                "status": status or "",
                "dispatch_state": dispatch or "",
                "worker_class": wc or "",
                "result": result,
            })
        return out

    def barren_concluded_goal_texts(self) -> list[str]:
        """P1 escape-valve dedup set: goals of CONCLUDED intents that yielded NOTHING
        — result is a barren 'explored'/dead-end/no-flag AND no fact was attached
        (result_seq → to_fact_seq is NULL). These are safe to suppress re-proposal of
        (a tried-and-empty direction). Intents that DID produce a fact (to_fact_seq
        set) are EXCLUDED, so re-proposing a productive direction under new evidence
        stays a planner judgment call — avoiding the run-7349 starvation where a
        blanket concluded-dedup proposed 0 intents and Explore starved."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.goal, e.payload FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.status='done' AND i.to_fact_seq IS NULL "
                "AND NOT EXISTS ("
                "  SELECT 1 FROM pocs p WHERE p.intent_id=i.intent_id "
                "  AND p.status IN ('available','wip','directional')"
                ") "
                "ORDER BY i.created_seq",
            ).fetchall()
        out: list[str] = []
        for goal, payload in rows:
            if not goal:
                continue
            result = ""
            if payload:
                try:
                    result = str((json.loads(payload) or {}).get("result", "")).lower()
                except (json.JSONDecodeError, TypeError):
                    result = ""
            # only barren outcomes — never 'solved' (that has a flag) or anything
            # that produced evidence. 'explored'/'dead_end'/'no verified flag'/''.
            if "solved" in result:
                continue
            out.append(str(goal))
        return out
