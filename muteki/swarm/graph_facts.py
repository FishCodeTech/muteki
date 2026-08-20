"""Facts, evidence, dead-ends, flags, review findings, and fact lifecycle.

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
    EV_FLAG_FOUND, EV_FLAG_INVALIDATED, EV_FLAG_SUBMISSION,
    EV_FLAG_SUBMISSION_DECISION,
    EV_FINDING_FOUND, EV_FINDING_INVALIDATED,
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


class _FactsMixin:
    def add_evidence(self, *, actor: str, source: str, fact: str,
                     artifact_id: Optional[str] = None, verified: bool = False,
                     confidence: float = 1.0, witness: Optional[str] = None,
                     verifier: str = "", route_hash: str = "",
                     intent_id: Optional[str] = None) -> int:
        route = self.normalize_route_hash(route_hash) if route_hash else ""
        if not verified:
            if route:
                with self._lock:
                    row = self._conn.execute(
                        "SELECT COUNT(*) FROM events "
                        "WHERE challenge_id=? AND kind=? AND actor=? AND verified=0 "
                        "AND json_extract(payload,'$.route_hash')=?",
                        (self.challenge.id, EV_FACT_ADDED, actor, route),
                    ).fetchone()
                if int(row[0] if row else 0) >= self.CANDIDATE_CAP_PER_SOURCE_ROUTE:
                    return -1
            else:
                # 刀7: route-less candidates used to skip the cap entirely. Bound the
                # per-actor catch-all bucket (route_hash absent/NULL) too.
                with self._lock:
                    row = self._conn.execute(
                        "SELECT COUNT(*) FROM events "
                        "WHERE challenge_id=? AND kind=? AND actor=? AND verified=0 "
                        "AND (json_extract(payload,'$.route_hash') IS NULL "
                        "     OR json_extract(payload,'$.route_hash')='')",
                        (self.challenge.id, EV_FACT_ADDED, actor),
                    ).fetchone()
                if int(row[0] if row else 0) >= self.CANDIDATE_CAP_PER_SOURCE_NOROUTE:
                    return -1
        payload = {"source": source, "fact": fact, "source_solver": actor,
                   "witness": witness, "verifier": verifier}
        if route:
            payload["route_hash"] = route
        iid = (intent_id or "").strip()
        if iid:
            payload["intent_id"] = iid
        # dedupe on fact IDENTITY (who-said-what), normalized to collapse the
        # skill/marker double-write: strip the "[engine]" tag, fold whitespace, drop
        # case; artifact_id is NOT part of identity. So a worker's bare verified skill
        # fact and its prefixed VERIFIED_FACT marker echo land on ONE key.
        fact_identity = _normalize_fact_identity(fact)
        dk = f"fact::{actor}::{fact_identity}"
        dedupe_key = dk
        superseded_candidate_seq: int | None = None
        if verified:
            with self._lock:
                row = self._conn.execute(
                    "SELECT seq, verified FROM events WHERE challenge_id=? AND kind=? "
                    "AND dedupe_key=? ORDER BY seq LIMIT 1",
                    (self.challenge.id, EV_FACT_ADDED, dk),
                ).fetchone()
            if row and not int(row[1] or 0):
                superseded_candidate_seq = int(row[0])
                dedupe_key = f"fact-verified::{actor}::{fact_identity}"
        seq = self._append(EV_FACT_ADDED, actor, payload,
                           artifact_id=artifact_id, verified=verified,
                           confidence=confidence, dedupe_key=dedupe_key)
        if seq <= 0:
            # Collided with an existing fact of the same identity. The echo is
            # dropped; verified-after-candidate is represented by a separate
            # fact-verified event, never by mutating the original event row.
            pass
        elif superseded_candidate_seq is not None:
            self.supersede_fact(
                actor=actor,
                fact_seq=superseded_candidate_seq,
                reason="verified duplicate supersedes unverified candidate",
                by_fact_seq=seq,
            )
        product_seq = seq
        if product_seq <= 0 and iid:
            with self._lock:
                row = self._conn.execute(
                    "SELECT seq FROM events WHERE challenge_id=? AND kind=? "
                    "AND dedupe_key IN (?,?) ORDER BY seq DESC LIMIT 1",
                    (
                        self.challenge.id,
                        EV_FACT_ADDED,
                        dedupe_key,
                        f"fact-verified::{actor}::{fact_identity}",
                    ),
                ).fetchone()
            product_seq = int(row[0]) if row else -1
        if product_seq > 0 and iid:
            with self._lock:
                self._conn.execute(
                    "INSERT OR IGNORE INTO intent_products "
                    "(intent_id, fact_seq) VALUES (?,?)",
                    (iid, product_seq),
                )
                self._conn.commit()
        return seq

    def add_dead_end(self, *, actor: str, reason: str) -> int:
        if self._has_near_duplicate_dead_end(reason):
            return -1
        return self._append(EV_DEAD_END, actor, {"reason": reason},
                            dedupe_key=f"deadend::{reason}")

    @staticmethod
    def _norm_dead_end_text(text: str) -> str:
        s = (text or "").strip().lower()
        s = re.sub(r"\bthree\b", "3", s)
        s = re.sub(r"\btwo\b", "2", s)
        s = re.sub(r"\bone\b", "1", s)
        s = re.sub(r"[^a-z0-9]+", " ", s)
        return re.sub(r"\s+", " ", s).strip()

    def _has_near_duplicate_dead_end(self, reason: str, *, threshold: float = 0.92) -> bool:
        target = self._norm_dead_end_text(reason)
        if not target:
            return False
        target_nums = set(re.findall(r"\b\d+\b", target))
        with self._lock:
            rows = self._conn.execute(
                "SELECT json_extract(payload,'$.reason') FROM events "
                "WHERE challenge_id=? AND kind=? ORDER BY seq DESC LIMIT 200",
                (self.challenge.id, EV_DEAD_END),
            ).fetchall()
        for (old_reason,) in rows:
            old = self._norm_dead_end_text(str(old_reason or ""))
            if not old:
                continue
            old_nums = set(re.findall(r"\b\d+\b", old))
            if target_nums != old_nums:
                continue
            if SequenceMatcher(None, target, old).ratio() >= threshold:
                return True
        return False

    def flag_found(self, *, actor: str, flag: str,
                   artifact_id: Optional[str] = None,
                   intent_id: Optional[str] = None) -> int:
        payload = {"flag": flag}
        if intent_id:
            payload["intent_id"] = intent_id
        return self._append(EV_FLAG_FOUND, actor, payload,
                            artifact_id=artifact_id, verified=True,
                            dedupe_key=f"flag::{flag}")

    def flag_submission(
        self, *, actor: str, submission_id: str, flag: str,
        intent_id: Optional[str] = None,
    ) -> int:
        """Record one unverified Worker API request through the host DB owner."""
        payload = {
            "submission_id": str(submission_id),
            "flag": str(flag),
            "intent_id": str(intent_id or ""),
            "protocol": "blackboard-api-v1",
        }
        return self._append(
            EV_FLAG_SUBMISSION,
            actor,
            payload,
            verified=False,
            dedupe_key=f"flag-submission::{submission_id}",
        )

    def flag_submission_decision(
        self, *, actor: str, submission_id: str, accepted: bool,
        code: str, detail: str = "",
    ) -> int:
        """Record the authority decision for one Blackboard API submission."""
        return self._append(
            EV_FLAG_SUBMISSION_DECISION,
            actor,
            {
                "submission_id": str(submission_id),
                "accepted": bool(accepted),
                "code": str(code),
                "detail": str(detail)[:240],
            },
            verified=bool(accepted),
            dedupe_key=f"flag-submission-decision::{submission_id}",
        )

    def finding_found(self, *, actor: str, finding: dict,
                      artifact_id: Optional[str] = None,
                      intent_id: Optional[str] = None) -> int:
        payload = dict(finding or {})
        if intent_id:
            payload["intent_id"] = intent_id
        key = SolveGraph._finding_identity(payload)
        return self._append(EV_FINDING_FOUND, actor, payload,
                            artifact_id=artifact_id, verified=True,
                            dedupe_key=f"finding::{key}")

    def finding_invalidated(self, *, actor: str, finding: dict | str) -> int:
        key = finding if isinstance(finding, str) else SolveGraph._finding_identity(finding or {})
        return self._append(EV_FINDING_INVALIDATED, actor, {"finding_key": key},
                            dedupe_key=f"findinginvalid::{key}")

    # ── review-arbiter events/state ────────────────────────────────────
    _ROUTE_STOPWORDS = {
        "the", "a", "an", "to", "of", "for", "on", "in", "at", "and",
        "or", "with", "via", "try", "test", "probe", "inspect", "attack",
        "exploit", "route", "path", "endpoint", "issue",
    }
    _ROUTE_ALIAS = (
        (re.compile(r"\bsql\s+injection\b|\bunion\s+(?:select\s+)?(?:payload|sqli)\b", re.I), "sqli"),
        (re.compile(r"\bcross\s+site\s+scripting\b|\bxss\b", re.I), "xss"),
        (re.compile(r"\bserver\s+side\s+request\s+forgery\b|\bssrf\b", re.I), "ssrf"),
        (re.compile(r"\bserver\s+side\s+template\s+injection\b|\bssti\b", re.I), "ssti"),
        (re.compile(r"\bpath\s+traversal\b|\bdirectory\s+traversal\b", re.I), "traversal"),
        (re.compile(r"\bfile\s+upload\b|\bupload\b", re.I), "upload"),
        (re.compile(r"\bjson\s+web\s+token\b|\bjwts?\b", re.I), "jwt"),
        (re.compile(r"\bcommand\s+injection\b|\bcmdi\b", re.I), "cmdi"),
    )

    @classmethod
    def normalize_route_hash(cls, route_hash: str, *, label: str = "") -> str:
        raw = (route_hash or label or "").strip().lower()
        for rx, repl in cls._ROUTE_ALIAS:
            raw = rx.sub(repl, raw)
        parts = [
            p for p in re.findall(r"[a-z0-9]+", raw)
            if p and p not in cls._ROUTE_STOPWORDS
        ]
        if not parts:
            h = hashlib.sha1(raw.encode("utf-8", "ignore")).hexdigest()[:10]
            return f"route:{h}"
        return ":".join(parts[:6])

    @staticmethod
    def normalize_lane_key(lane_key: str) -> str:
        raw = (lane_key or "").strip().lower()
        raw = re.sub(r"\s+", "", raw)
        raw = raw.replace("://", ":")
        raw = re.sub(r"[^a-z0-9_:@.*-]+", "-", raw).strip("-")
        if not raw:
            return ""
        m = re.match(r"^(?P<risk>[a-z0-9_]+):(?P<proto>[a-z0-9_]+):(?P<port>[0-9*]+)@(?P<host>.+)$", raw)
        if not m:
            return raw[:180]
        risk = _clean_lane_risk(m.group("risk"))
        proto = m.group("proto") or "tcp"
        port = m.group("port") or "*"
        host = m.group("host").strip("[]")
        return f"{risk}:{proto}:{port}@{host}"[:180]

    @staticmethod
    def _safe_review_severity(value: str) -> str:
        v = (value or "info").strip().lower()
        return v if v in {"info", "warn", "blocker"} else "warn"

    @staticmethod
    def review_finding_identity(kind: str, summary: str, route_hash: str = "") -> str:
        seed = (
            f"{(kind or 'no_action').strip()}:"
            f"{(summary or '').strip()[:1000]}:"
            f"{(route_hash or '').strip()}"
        )
        return f"rvw-{hashlib.sha1(seed.encode()).hexdigest()[:10]}"

    def add_review_finding(self, *, actor: str, kind: str, severity: str,
                           summary: str, evidence_seqs: Optional[list[int]] = None,
                           intent_ids: Optional[list[str]] = None,
                           route_hash: str = "", branch_id: str = "",
                           recommended_actions: Optional[list[str]] = None) -> int:
        route = self.normalize_route_hash(route_hash) if route_hash else ""
        payload = {
            "finding_id": self.review_finding_identity(kind, summary, route),
            "kind": (kind or "no_action").strip() or "no_action",
            "severity": self._safe_review_severity(severity),
            "summary": (summary or "").strip()[:1000],
            "evidence_seqs": [int(x) for x in (evidence_seqs or []) if isinstance(x, int)],
            "intent_ids": [str(x) for x in (intent_ids or []) if x],
            "route_hash": route,
            "branch_id": (branch_id or "").strip(),
            "recommended_actions": [str(x) for x in (recommended_actions or []) if x],
        }
        return self._append(EV_REVIEW_FINDING, actor, payload,
                            dedupe_key=f"review::{payload['kind']}::{payload['summary']}::{route}")

    @staticmethod
    def _review_proposal_tier(marker: str) -> str:
        m = (marker or "").strip().upper()
        if m in {"ROUTE_SUPPRESS", "COORDINATOR_DIRECTIVE", "LANE_LOCK", "LANE_UNLOCK"}:
            return "tier2"
        return "tier1"

    def add_review_proposal(self, *, actor: str, marker: str, payload: dict,
                            tier: str = "tier1") -> int:
        marker = (marker or "").strip().upper()
        clean_payload = dict(payload or {})
        route_hash = str(clean_payload.get("route_hash") or "").strip()
        if route_hash:
            clean_payload["route_hash"] = self.normalize_route_hash(route_hash)
        lane_key = str(clean_payload.get("lane_key") or "").strip()
        if lane_key:
            clean_payload["lane_key"] = self.normalize_lane_key(lane_key)
        confidence = clean_payload.get("confidence", 1.0)
        try:
            confidence = float(confidence)
        except (TypeError, ValueError):
            confidence = 1.0
        clean_payload["confidence"] = max(0.0, min(1.0, confidence))
        clean_tier = tier if tier in {"tier1", "tier2"} else self._review_proposal_tier(marker)
        payload_out = {
            "marker": marker,
            "tier": clean_tier,
            "payload": clean_payload,
            "status": "pending",
        }
        fp = json.dumps(clean_payload, sort_keys=True, ensure_ascii=False, default=str)
        return self._append(EV_REVIEW_PROPOSAL, actor, payload_out,
                            dedupe_key=f"review-proposal::{marker}::{hashlib.sha1(fp.encode()).hexdigest()}")

    def decide_review_proposal(self, *, actor: str, proposal_seq: int,
                               decision: str, reason: str = "",
                               applied_seq: Optional[int] = None) -> int:
        clean_decision = (decision or "deferred").strip().lower()
        if clean_decision not in {"accepted", "deferred", "rejected"}:
            clean_decision = "deferred"
        payload = {
            "proposal_seq": int(proposal_seq),
            "decision": clean_decision,
            "reason": (reason or "").strip()[:1000],
        }
        if applied_seq is not None:
            payload["applied_seq"] = int(applied_seq)
        return self._append(
            EV_REVIEW_PROPOSAL_DECISION, actor, payload,
            dedupe_key=f"review-proposal-decision::{proposal_seq}::{clean_decision}",
        )

    def challenge_fact(self, *, actor: str, fact_seq: int, reason: str,
                       verification_goal: str) -> dict:
        fact_seq = int(fact_seq)
        goal = (verification_goal or f"Verify fact #{fact_seq}: {reason}").strip()
        h = hashlib.sha1(f"{fact_seq}:{goal}".encode("utf-8", "ignore")).hexdigest()[:8]
        intent_id = f"I-verify-{fact_seq}-{h}"
        payload = {
            "fact_seq": fact_seq,
            "status": "challenged",
            "reason": (reason or "").strip()[:1000],
            "challenged_by": actor,
            "verification_intent_id": intent_id,
        }
        seq = self._append(EV_FACT_CHALLENGED, actor, payload,
                           dedupe_key=f"fact-challenged::{fact_seq}::{payload['reason']}")
        with self._lock:
            self._conn.execute(
                "INSERT INTO fact_reviews "
                "(fact_seq, challenge_id, status, challenged_seq, reason, verification_intent_id) "
                "VALUES (?,?,?,?,?,?) "
                "ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='challenged', challenged_seq=excluded.challenged_seq, "
                " reason=excluded.reason, verification_intent_id=excluded.verification_intent_id",
                (fact_seq, self.challenge.id, "challenged",
                 seq if seq > 0 else None, payload["reason"], intent_id),
            )
            self._upsert_fact_state(
                fact_seq, FACT_STATE_CHALLENGED,
                reason=payload["reason"], challenged_seq=seq if seq > 0 else None,
                verification_intent_id=intent_id,
                verified_effective=0, confidence_effective=0.4, updated_seq=seq)
            self._conn.commit()
        self.propose_intent(
            actor=actor, intent_id=intent_id, goal=goal,
            payload={"worker_class": "verifier", "depends_on": [str(fact_seq)],
                     "rationale": f"Review challenged fact #{fact_seq}: {reason}"},
            from_fact_seqs=[fact_seq],
        )
        return {"fact_seq": fact_seq, "verification_intent_id": intent_id,
                "seq": seq, "reason": payload["reason"]}

    def revalidate_fact(self, *, actor: str, fact_seq: int, reason: str = "") -> int:
        fact_seq = int(fact_seq)
        payload = {
            "fact_seq": fact_seq,
            "status": "revalidated",
            "reason": (reason or "").strip()[:1000],
            "revalidated_by": actor,
        }
        seq = self._append(EV_FACT_REVALIDATED, actor, payload,
                           dedupe_key=f"fact-revalidated::{fact_seq}::{payload['reason']}")
        # revalidate effectively restores the fact's verified verdict (defect-4: the
        # legacy path wrote status but the snapshot still leaned on events.verified).
        orig_verified, orig_conf = self._fact_origin_verdict(fact_seq)
        with self._lock:
            self._conn.execute(
                "INSERT INTO fact_reviews "
                "(fact_seq, challenge_id, status, revalidated_seq, reason) "
                "VALUES (?,?,?,?,?) "
                "ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='revalidated', revalidated_seq=excluded.revalidated_seq, "
                " reason=excluded.reason",
                (fact_seq, self.challenge.id, "revalidated",
                 seq if seq > 0 else None, payload["reason"]),
            )
            self._upsert_fact_state(
                fact_seq, FACT_STATE_REVALIDATED, reason=payload["reason"],
                revalidated_seq=seq if seq > 0 else None,
                verified_effective=1 if orig_verified else 0,
                confidence_effective=orig_conf, updated_seq=seq)
            self._conn.commit()
        return seq

    # ── A: fact lifecycle (reject / merge / supersede) ──────────────────
    def _fact_origin_verdict(self, fact_seq: int) -> tuple[bool, float]:
        """The fact's original verified/confidence from the append-only event."""
        with self._lock:
            row = self._conn.execute(
                "SELECT verified, confidence FROM events WHERE seq=? AND kind=?",
                (int(fact_seq), EV_FACT_ADDED),
            ).fetchone()
        if not row:
            return (False, 0.0)
        return (bool(row[0]), float(row[1] if row[1] is not None else 1.0))

    def _upsert_fact_state(self, fact_seq: int, state: str, *,
                           reason: str = "", verified_effective: Optional[int] = None,
                           confidence_effective: Optional[float] = None,
                           challenged_seq: Optional[int] = None,
                           revalidated_seq: Optional[int] = None,
                           rejected_seq: Optional[int] = None,
                           merged_seq: Optional[int] = None,
                           superseded_seq: Optional[int] = None,
                           retired_seq: Optional[int] = None,
                           verification_intent_id: Optional[str] = None,
                           updated_seq: Optional[int] = None) -> None:
        """Write the current lifecycle state for a fact. Caller holds self._lock.

        Only non-None transition seqs / effective verdicts overwrite existing
        columns (COALESCE), so a later reject doesn't blank an earlier challenge's
        challenged_seq. `state`, `reason`, and effective verdicts always win."""
        state = state if state in _FACT_STATES else FACT_STATE_UNRESOLVED
        self._conn.execute(
            "INSERT INTO fact_states "
            "(fact_seq, challenge_id, state, verified_effective, confidence_effective, "
            " reason, challenged_seq, revalidated_seq, rejected_seq, merged_seq, "
            " superseded_seq, retired_seq, verification_intent_id, updated_seq) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?) "
            "ON CONFLICT(fact_seq) DO UPDATE SET "
            " state=excluded.state, reason=excluded.reason, "
            " verified_effective=COALESCE(excluded.verified_effective, fact_states.verified_effective), "
            " confidence_effective=COALESCE(excluded.confidence_effective, fact_states.confidence_effective), "
            " challenged_seq=COALESCE(excluded.challenged_seq, fact_states.challenged_seq), "
            " revalidated_seq=COALESCE(excluded.revalidated_seq, fact_states.revalidated_seq), "
            " rejected_seq=COALESCE(excluded.rejected_seq, fact_states.rejected_seq), "
            " merged_seq=COALESCE(excluded.merged_seq, fact_states.merged_seq), "
            " superseded_seq=COALESCE(excluded.superseded_seq, fact_states.superseded_seq), "
            " retired_seq=COALESCE(excluded.retired_seq, fact_states.retired_seq), "
            " verification_intent_id=COALESCE(excluded.verification_intent_id, fact_states.verification_intent_id), "
            " updated_seq=COALESCE(excluded.updated_seq, fact_states.updated_seq)",
            (int(fact_seq), self.challenge.id, state, verified_effective,
             confidence_effective, reason or None, challenged_seq, revalidated_seq,
             rejected_seq, merged_seq, superseded_seq, retired_seq,
             verification_intent_id, updated_seq),
        )

    def reject_fact(self, *, actor: str, fact_seq: int, reason: str = "") -> int:
        """Mark a fact REJECTED — review proved it false. It is retired from the
        active candidate set and excluded from snapshots / Reason summaries, but the
        originating event stays (audit trail)."""
        fact_seq = int(fact_seq)
        payload = {"fact_seq": fact_seq, "status": FACT_STATE_REJECTED,
                   "reason": (reason or "").strip()[:1000], "rejected_by": actor}
        seq = self._append(EV_FACT_REJECTED, actor, payload,
                           dedupe_key=f"fact-rejected::{fact_seq}::{payload['reason']}")
        with self._lock:
            self._conn.execute(
                "INSERT INTO fact_reviews (fact_seq, challenge_id, status, reason) "
                "VALUES (?,?,?,?) ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='rejected', reason=excluded.reason",
                (fact_seq, self.challenge.id, FACT_STATE_REJECTED, payload["reason"]),
            )
            self._upsert_fact_state(
                fact_seq, FACT_STATE_REJECTED, reason=payload["reason"],
                rejected_seq=seq if seq > 0 else None, retired_seq=seq if seq > 0 else None,
                verified_effective=0, confidence_effective=0.0, updated_seq=seq)
            self._conn.commit()
        return seq

    def merge_fact(self, *, actor: str, from_fact_seq: int, to_fact_seq: int,
                   reason: str = "") -> int:
        """Fold `from_fact_seq` into `to_fact_seq` — they describe the same finding.
        The from-fact is retired (merged) and the merge edge recorded."""
        from_seq, to_seq = int(from_fact_seq), int(to_fact_seq)
        if from_seq == to_seq:
            return -1
        payload = {"from_fact_seq": from_seq, "to_fact_seq": to_seq,
                   "status": FACT_STATE_MERGED, "reason": (reason or "").strip()[:1000],
                   "merged_by": actor}
        seq = self._append(EV_FACT_MERGED, actor, payload,
                           dedupe_key=f"fact-merged::{from_seq}::{to_seq}")
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO fact_merges "
                "(from_fact_seq, to_fact_seq, challenge_id, merge_seq, reason) "
                "VALUES (?,?,?,?,?)",
                (from_seq, to_seq, self.challenge.id, seq if seq > 0 else 0,
                 payload["reason"]),
            )
            self._conn.execute(
                "INSERT INTO fact_reviews (fact_seq, challenge_id, status, reason) "
                "VALUES (?,?,?,?) ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='merged', reason=excluded.reason",
                (from_seq, self.challenge.id, FACT_STATE_MERGED, payload["reason"]),
            )
            self._upsert_fact_state(
                from_seq, FACT_STATE_MERGED, reason=payload["reason"],
                merged_seq=seq if seq > 0 else None, retired_seq=seq if seq > 0 else None,
                verified_effective=0, updated_seq=seq)
            self._conn.commit()
        return seq

    def supersede_fact(self, *, actor: str, fact_seq: int, reason: str = "",
                       by_fact_seq: Optional[int] = None) -> int:
        """Mark a fact SUPERSEDED — a newer fact replaces it. Retired from the
        active set; kept for audit."""
        fact_seq = int(fact_seq)
        payload = {"fact_seq": fact_seq, "status": FACT_STATE_SUPERSEDED,
                   "reason": (reason or "").strip()[:1000], "superseded_by": actor}
        if by_fact_seq is not None:
            payload["by_fact_seq"] = int(by_fact_seq)
        seq = self._append(EV_FACT_SUPERSEDED, actor, payload,
                           dedupe_key=f"fact-superseded::{fact_seq}::{payload['reason']}")
        with self._lock:
            self._conn.execute(
                "INSERT INTO fact_reviews (fact_seq, challenge_id, status, reason) "
                "VALUES (?,?,?,?) ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='superseded', reason=excluded.reason",
                (fact_seq, self.challenge.id, FACT_STATE_SUPERSEDED, payload["reason"]),
            )
            self._upsert_fact_state(
                fact_seq, FACT_STATE_SUPERSEDED, reason=payload["reason"],
                superseded_seq=seq if seq > 0 else None, retired_seq=seq if seq > 0 else None,
                verified_effective=0, updated_seq=seq)
            self._conn.commit()
        return seq

    def review_fact(self, *, actor: str, fact_seq: int, action: str,
                    reason: str = "", verification_goal: str = "",
                    to_fact_seq: Optional[int] = None) -> dict:
        """Unified fact review dispatcher (challenge/revalidate/reject/merge/supersede).
        Returns {action, fact_seq, seq}."""
        act = (action or "").strip().lower()
        if act in ("challenge", "challenged"):
            res = self.challenge_fact(actor=actor, fact_seq=fact_seq, reason=reason,
                                      verification_goal=verification_goal)
            return {"action": "challenge", "fact_seq": int(fact_seq),
                    "seq": int(res.get("seq") or 0)}
        if act in ("revalidate", "revalidated"):
            seq = self.revalidate_fact(actor=actor, fact_seq=fact_seq, reason=reason)
            return {"action": "revalidate", "fact_seq": int(fact_seq), "seq": seq}
        if act in ("reject", "rejected"):
            seq = self.reject_fact(actor=actor, fact_seq=fact_seq, reason=reason)
            return {"action": "reject", "fact_seq": int(fact_seq), "seq": seq}
        if act in ("merge", "merged"):
            seq = self.merge_fact(actor=actor, from_fact_seq=fact_seq,
                                  to_fact_seq=int(to_fact_seq or 0), reason=reason)
            return {"action": "merge", "fact_seq": int(fact_seq), "seq": seq}
        if act in ("supersede", "superseded"):
            seq = self.supersede_fact(actor=actor, fact_seq=fact_seq, reason=reason,
                                      by_fact_seq=to_fact_seq)
            return {"action": "supersede", "fact_seq": int(fact_seq), "seq": seq}
        return {"action": act, "fact_seq": int(fact_seq), "seq": -1}

    def _fact_state_map(self) -> dict[int, dict]:
        """fact_seq → {state, verified_effective, confidence_effective, retired}."""
        if not self._table_exists("fact_states"):
            return {}
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, state, verified_effective, confidence_effective, "
                "retired_seq FROM fact_states WHERE challenge_id=?",
                (self.challenge.id,),
            ).fetchall()
        return {
            int(r[0]): {
                "state": str(r[1] or FACT_STATE_UNRESOLVED),
                "verified_effective": (None if r[2] is None else bool(r[2])),
                "confidence_effective": (None if r[3] is None else float(r[3])),
                "retired": r[4] is not None,
            }
            for r in rows
        }

    def active_candidates(self) -> list[dict]:
        """Active (non-retired) UNRESOLVED/CHALLENGED candidate facts — the set the
        planner should still weigh. Excludes verified, rejected, merged, superseded."""
        texts = self._fact_text_by_seq()
        states = self._fact_state_map()
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, verified FROM events WHERE challenge_id=? AND kind=? "
                "ORDER BY seq",
                (self.challenge.id, EV_FACT_ADDED),
            ).fetchall()
        out: list[dict] = []
        for seq, verified in rows:
            seq = int(seq)
            st = states.get(seq, {})
            state = st.get("state", FACT_STATE_UNRESOLVED)
            if st.get("retired") or state in _FACT_TERMINAL_STATES:
                continue
            eff = st.get("verified_effective")
            is_verified = bool(verified) if eff is None else eff
            if is_verified and state != FACT_STATE_CHALLENGED:
                continue
            out.append({"fact_seq": seq, "fact": texts.get(seq, ""), "state": state})
        return out

    def verified_evidence(self) -> list[dict]:
        """Facts that are verified (origin or revalidated) AND not retired."""
        texts = self._fact_text_by_seq()
        states = self._fact_state_map()
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, verified FROM events WHERE challenge_id=? AND kind=? "
                "ORDER BY seq",
                (self.challenge.id, EV_FACT_ADDED),
            ).fetchall()
        out: list[dict] = []
        for seq, verified in rows:
            seq = int(seq)
            st = states.get(seq, {})
            if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                continue
            eff = st.get("verified_effective")
            is_verified = bool(verified) if eff is None else eff
            if is_verified:
                out.append({"fact_seq": seq, "fact": texts.get(seq, "")})
        return out
