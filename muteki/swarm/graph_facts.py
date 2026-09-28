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
    EV_FACT_ADDED, EV_FACT_OBSERVED, EV_FACT_PROMOTED, EV_OBSERVATION_ADDED,
    EV_HYP_PROPOSED, EV_HYP_REFUTED, EV_DEAD_END,
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
    REVIEW_FACT_MARKERS,
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
    def ctf_artifact_digest(self, artifact_id: str) -> Optional[str]:
        """Authorize a tool artifact only when this challenge cites its digest."""
        if getattr(self.challenge, "mode", "ctf") not in {"ctf", "pentest"} or not re.fullmatch(
            r"[0-9a-f]{12}", artifact_id
        ):
            return None
        with self._lock:
            rows = self._conn.execute(
                "SELECT provenance_json FROM observations WHERE challenge_id=? "
                "ORDER BY observation_seq DESC",
                (self.challenge.id,),
            ).fetchall()
        for (raw_provenance,) in rows:
            provenance = json.loads(raw_provenance or "{}")
            for ref in provenance.get("artifact_refs") or []:
                if not isinstance(ref, dict):
                    continue
                if ref.get("artifact_id") == artifact_id:
                    digest = str(ref.get("sha256") or "")
                    return digest if re.fullmatch(r"[0-9a-f]{64}", digest) else None
        return None

    def migrate_legacy_candidates(self) -> int:
        """Convert pre-v2 unverified Fact rows into non-semantic Observations.

        The original event remains append-only audit history; ``fact_states``
        removes it from every active Fact view.
        """
        migrated = 0
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq,ts,actor,payload,artifact_id,confidence FROM events "
                "WHERE challenge_id=? AND kind=? AND verified=0 ORDER BY seq",
                (self.challenge.id, EV_FACT_ADDED),
            ).fetchall()
            for legacy_seq, ts, actor, raw_payload, artifact_id, confidence in rows:
                try:
                    payload = json.loads(raw_payload or "{}")
                except (TypeError, ValueError, json.JSONDecodeError):
                    payload = {}
                text = str(payload.get("fact") or "").strip()
                if not text:
                    continue
                dedupe = f"legacy-observation::{self.challenge.id}::{int(legacy_seq)}"
                obs_payload = {
                    "source": payload.get("source") or "legacy-fact",
                    "text": text,
                    "source_solver": actor,
                    "intent_id": payload.get("intent_id") or "",
                    "target_epoch": payload.get("target_epoch") or "legacy",
                    "witness": payload.get("witness"),
                    "artifact_id": artifact_id,
                    "claimed_verified": False,
                    "admitted": False,
                    "canonical_key": payload.get("canonical_key")
                    or _normalize_fact_identity(text),
                    "legacy_fact_seq": int(legacy_seq),
                    "evidence_provenance": payload.get("evidence_provenance") or {},
                }
                obs_seq = self._append_locked(
                    EV_OBSERVATION_ADDED, str(actor or "migration"), obs_payload,
                    artifact_id=artifact_id, verified=False,
                    confidence=float(confidence or 0.4), dedupe_key=dedupe,
                )
                if obs_seq < 0:
                    found = self._conn.execute(
                        "SELECT seq FROM events WHERE dedupe_key=?", (dedupe,)
                    ).fetchone()
                    obs_seq = int(found[0]) if found else 0
                if obs_seq > 0:
                    self._conn.execute(
                        "INSERT OR IGNORE INTO observations (observation_seq,challenge_id,actor,"
                        "intent_id,target_epoch,text,witness,artifact_id,provenance_json,canonical_key,"
                        "admitted_fact_seq,created_at) VALUES (?,?,?,?,?,?,?,?,?,?,NULL,?)",
                        (obs_seq, self.challenge.id, str(actor or "migration"),
                         str(payload.get("intent_id") or ""),
                         str(payload.get("target_epoch") or "legacy"), text,
                         payload.get("witness"), artifact_id,
                         json.dumps(payload.get("evidence_provenance") or {}, default=str),
                         obs_payload["canonical_key"], float(ts or time.time())),
                    )
                self._upsert_fact_state(
                    int(legacy_seq), FACT_STATE_SUPERSEDED,
                    reason="legacy candidate migrated to Observation",
                    verified_effective=0, updated_seq=obs_seq or None,
                )
                migrated += 1
            self._conn.commit()
        return migrated

    def _equivalent_fact_rows(
        self, fact_identity: str, target_epoch: str,
    ) -> list[dict[str, Any]]:
        """Return the active canonical Fact for one normalized epoch identity."""
        with self._lock:
            return self._equivalent_fact_rows_locked(fact_identity, target_epoch)

    def _equivalent_fact_rows_locked(
        self, fact_identity: str, target_epoch: str,
    ) -> list[dict[str, Any]]:
        """``_equivalent_fact_rows`` with ``self._lock`` already held."""
        rows = self._conn.execute(
            "SELECT e.seq, e.verified, e.payload, fs.state, "
            "fs.verified_effective FROM events e "
            "LEFT JOIN fact_states fs ON fs.fact_seq=e.seq "
            "WHERE e.challenge_id=? AND e.kind=? ORDER BY e.seq",
            (self.challenge.id, EV_FACT_ADDED),
        ).fetchall()
        matches: list[dict[str, Any]] = []
        for seq, original_verified, raw_payload, state, effective_verified in rows:
            try:
                payload = json.loads(str(raw_payload or "{}"))
            except (TypeError, ValueError, json.JSONDecodeError):
                continue
            stored_identity = str(payload.get("canonical_key") or "").strip().casefold()
            if not stored_identity:
                stored_identity = _normalize_fact_identity(str(payload.get("fact") or ""))
            if stored_identity != fact_identity:
                continue
            row_provenance = payload.get("evidence_provenance")
            row_epoch = str(
                payload.get("target_epoch")
                or (
                    row_provenance.get("target_epoch")
                    if isinstance(row_provenance, dict) else ""
                )
                or "legacy"
            )
            if row_epoch != target_epoch:
                continue
            lifecycle = str(state or FACT_STATE_UNRESOLVED)
            if lifecycle in _FACT_TERMINAL_STATES or lifecycle == FACT_STATE_CHALLENGED:
                continue
            verified_now = bool(original_verified)
            if effective_verified is not None:
                verified_now = bool(effective_verified)
            if not verified_now:
                continue
            matches.append({
                "seq": int(seq),
                "verified": verified_now,
                "payload": payload,
            })
        return matches

    def _valid_fact_provenance(
        self, *, actor: str, intent_id: str, artifact_id: str,
        provenance: dict[str, Any],
    ) -> bool:
        """Host-side validation for every binding required by Fact promotion."""
        # tool_call_id / committed_at are pass-through audit fields: carried into
        # the stored evidence_provenance but deliberately NOT gate inputs — the
        # verified binding stays worker/intent/epoch/artifact-digest/event-seq.
        if not provenance or not artifact_id or self.artifacts is None:
            return False
        if str(provenance.get("worker_id") or "") != str(actor or ""):
            return False
        if str(provenance.get("intent_id") or "") != str(intent_id or ""):
            return False
        target_epoch = str(provenance.get("target_epoch") or "")
        if not target_epoch:
            return False
        if str(provenance.get("artifact_id") or "") != artifact_id:
            return False
        digest = str(provenance.get("artifact_sha256") or "").lower()
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            return False
        try:
            event_seq = int(provenance.get("tool_event_seq") or 0)
            event_at = float(provenance.get("tool_event_ts") or 0.0)
            observed_at = float(provenance.get("observed_at") or 0.0)
            promoted_at = float(provenance.get("promoted_at") or 0.0)
        except (TypeError, ValueError):
            return False
        if (event_seq <= 0 or event_at <= 0 or observed_at <= 0
                or event_at + 1.0 < observed_at or promoted_at < event_at):
            return False
        if promoted_at > time.time() + 5.0:
            return False
        event_id = str(provenance.get("tool_event_id") or "")
        if not event_id or event_id.rsplit(":", 1)[-1] != str(event_seq):
            return False
        try:
            persisted_digest = self.artifacts.sha256(artifact_id)
        except Exception:
            return False
        return bool(persisted_digest and persisted_digest == digest)

    def _append_fact_observation_locked(
        self, *, fact_seq: int, actor: str, source: str, fact: str,
        artifact_id: Optional[str], verified: bool, confidence: float,
        witness: Optional[str], verifier: str, intent_id: str,
        target_epoch: str, provenance: dict[str, Any], observation_seq: int = 0,
    ) -> int:
        """Append the fold-observation with ``self._lock`` held and no commit
        (transactional reuse from ``_add_evidence_locked``)."""
        payload = {
            "fact_seq": int(fact_seq),
            "source": source,
            "fact": fact,
            "source_solver": actor,
            "intent_id": intent_id,
            "target_epoch": target_epoch,
            "witness": witness,
            "verifier": verifier,
            "verified": bool(verified),
            "evidence_provenance": provenance,
            "observation_seq": int(observation_seq or 0),
        }
        observation_identity = "\x1f".join((
            str(fact_seq), str(actor), str(intent_id),
            str(provenance.get("tool_event_seq") or 0),
            str(provenance.get("artifact_sha256") or ""),
            str(int(bool(verified))),
        ))
        observation_key = hashlib.sha256(
            observation_identity.encode("utf-8", errors="replace")
        ).hexdigest()
        seq = self._append_locked(
            EV_FACT_OBSERVED, actor, payload,
            artifact_id=artifact_id, verified=verified,
            confidence=confidence,
            dedupe_key=f"fact-observed::{self.challenge.id}::{observation_key}",
        )
        if observation_seq > 0:
            self._conn.execute(
                "INSERT OR IGNORE INTO fact_evidence (fact_seq,observation_seq,challenge_id) "
                "VALUES (?,?,?)",
                (int(fact_seq), int(observation_seq), self.challenge.id),
            )
            self._conn.execute(
                "UPDATE observations SET admitted_fact_seq=? WHERE observation_seq=? "
                "AND challenge_id=?",
                (int(fact_seq), int(observation_seq), self.challenge.id),
            )
        return seq

    def _promote_canonical_fact_locked(
        self, *, fact_seq: int, actor: str, source: str, fact: str,
        artifact_id: str, confidence: float, witness: Optional[str],
        verifier: str, intent_id: str, target_epoch: str,
        provenance: dict[str, Any],
    ) -> int:
        """Promote a canonical Fact with ``self._lock`` held and no commit
        (transactional reuse from ``_add_evidence_locked``)."""
        payload = {
            "fact_seq": int(fact_seq),
            "source": source,
            "fact": fact,
            "source_solver": actor,
            "intent_id": intent_id,
            "target_epoch": target_epoch,
            "witness": witness,
            "verifier": verifier,
            "evidence_provenance": provenance,
        }
        seq = self._append_locked(
            EV_FACT_PROMOTED, actor, payload,
            artifact_id=artifact_id, verified=True,
            confidence=confidence,
            dedupe_key=(
                f"fact-promoted::{self.challenge.id}::{fact_seq}::{target_epoch}"
            ),
        )
        update_seq = seq if seq > 0 else None
        review = self._conn.execute(
            "SELECT status FROM fact_reviews WHERE challenge_id=? AND fact_seq=?",
            (self.challenge.id, int(fact_seq)),
        ).fetchone()
        if review is not None and str(review[0] or "") == "challenged":
            self._conn.execute(
                "UPDATE fact_reviews SET status='revalidated', "
                "revalidated_seq=COALESCE(?, revalidated_seq), reason=? "
                "WHERE challenge_id=? AND fact_seq=?",
                (update_seq, "promoted by bound tool evidence",
                 self.challenge.id, int(fact_seq)),
            )
        lifecycle_state = (
            FACT_STATE_REVALIDATED
            if review is not None and str(review[0] or "") == "challenged"
            else FACT_STATE_UNRESOLVED
        )
        self._upsert_fact_state(
            int(fact_seq),
            state=lifecycle_state,
            verified_effective=1,
            confidence_effective=float(confidence),
            revalidated_seq=update_seq,
            reason="promoted by bound tool evidence",
            updated_seq=update_seq,
        )
        return seq

    def add_evidence(self, *, actor: str, source: str, fact: str,
                     artifact_id: Optional[str] = None, verified: bool = False,
                     confidence: float = 1.0, witness: Optional[str] = None,
                     verifier: str = "", route_hash: str = "",
                     intent_id: Optional[str] = None,
                     provenance: Optional[dict[str, Any]] = None,
                     subject: str = "", predicate: str = "",
                     object_value: Any = None, scope: str = "",
                     canonical_key: str = "") -> int:
        """Store every claim as an Observation and return an admitted Fact id.

        ``0`` means the Observation was retained but evidence admission failed.
        This is intentionally different from ``-1`` (invalid/rejected request).
        """
        with self._lock:
            try:
                product_seq = self._add_evidence_locked(
                    actor=actor, source=source, fact=fact,
                    artifact_id=artifact_id, verified=verified,
                    confidence=confidence, witness=witness, verifier=verifier,
                    route_hash=route_hash, intent_id=intent_id,
                    provenance=provenance,
                    subject=subject, predicate=predicate,
                    object_value=object_value, scope=scope,
                    canonical_key=canonical_key,
                )
            except BaseException:
                self._conn.rollback()
                raise
            self._conn.commit()
            return product_seq

    @staticmethod
    def _independent_verifier(*, actor: str, verified: bool, verifier: str,
                              rows: list[dict[str, Any]]) -> str:
        """A verified re-observation by a worker other than the canonical
        producer is an independent check; name that worker as the verifier when
        the caller did not, so the board can attribute the verification."""
        if verifier or not verified or not rows:
            return verifier
        canonical = next((row for row in rows if row["verified"]), rows[0])
        producer = str((canonical.get("payload") or {}).get("source_solver") or "")
        if producer and producer != actor:
            return actor
        return verifier

    def _add_evidence_locked(self, *, actor: str, source: str, fact: str,
                             artifact_id: Optional[str] = None,
                             verified: bool = False, confidence: float = 1.0,
                             witness: Optional[str] = None, verifier: str = "",
                             route_hash: str = "", intent_id: Optional[str] = None,
                             provenance: Optional[dict[str, Any]] = None,
                             subject: str = "", predicate: str = "",
                             object_value: Any = None, scope: str = "",
                             canonical_key: str = "") -> int:
        """Observation/Fact admission with one caller-owned transaction."""
        iid = (intent_id or "").strip()
        evidence_provenance = dict(provenance or {})
        target_epoch = str(evidence_provenance.get("target_epoch") or scope or "legacy")
        if (getattr(self.challenge, "mode", "ctf") == "ctf"
                or (getattr(self.challenge, "mode", "ctf") == "pentest"
                    and actor == "origin" and source == "origin")):
            fact = str(fact or "")
            if not fact.strip():
                return -1
            observation_identity = hashlib.sha256(
                "\x1f".join((actor, iid, target_epoch, fact,
                            str(bool(verified)))).encode("utf-8")
            ).hexdigest()
            observation_seq = self._append_locked(
                EV_OBSERVATION_ADDED,
                actor,
                {"source": source, "text": fact, "source_solver": actor,
                 "intent_id": iid, "target_epoch": target_epoch,
                 "claimed_verified": bool(verified),
                 "evidence_provenance": evidence_provenance},
                artifact_id=artifact_id,
                verified=bool(verified),
                dedupe_key=f"ctf-observation::{self.challenge.id}::{observation_identity}",
            )
            if observation_seq < 0:
                row = self._conn.execute(
                    "SELECT seq FROM events WHERE challenge_id=? AND dedupe_key=?",
                    (self.challenge.id,
                     f"ctf-observation::{self.challenge.id}::{observation_identity}"),
                ).fetchone()
                observation_seq = int(row[0]) if row else 0
            if observation_seq <= 0:
                raise RuntimeError("CTF Observation append failed")
            self._conn.execute(
                "INSERT OR IGNORE INTO observations "
                "(observation_seq,challenge_id,actor,intent_id,target_epoch,"
                "text,witness,artifact_id,provenance_json,canonical_key,"
                "admitted_fact_seq,created_at) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,NULL,?)",
                (observation_seq, self.challenge.id, actor, iid, target_epoch,
                 fact, witness, artifact_id,
                 json.dumps(evidence_provenance, ensure_ascii=False, default=str),
                 observation_identity, time.time()),
            )
            self._last_observation_seq = observation_seq
            if not verified:
                return 0
            payload = {
                "source": source,
                "fact": fact,
                "source_solver": actor,
                "intent_id": iid,
                "observation_seq": observation_seq,
                "evidence_provenance": evidence_provenance,
            }
            identity = hashlib.sha256(
                "\x1f".join((actor, iid, fact)).encode(
                    "utf-8", errors="replace"
                )
            ).hexdigest()
            dedupe_key = f"ctf-fact::{self.challenge.id}::{identity}"
            product_seq = self._append_locked(
                EV_FACT_ADDED,
                actor,
                payload,
                artifact_id=artifact_id,
                verified=True,
                confidence=float(confidence or 1.0),
                dedupe_key=dedupe_key,
            )
            if product_seq < 0:
                row = self._conn.execute(
                    "SELECT seq FROM events WHERE challenge_id=? AND dedupe_key=?",
                    (self.challenge.id, dedupe_key),
                ).fetchone()
                product_seq = int(row[0]) if row else -1
            if product_seq > 0 and iid:
                self._conn.execute(
                    "INSERT OR IGNORE INTO intent_products "
                    "(intent_id, fact_seq) VALUES (?,?)",
                    (iid, product_seq),
                )
            if product_seq > 0:
                self._conn.execute(
                    "UPDATE observations SET admitted_fact_seq=? "
                    "WHERE challenge_id=? AND observation_seq=?",
                    (product_seq, self.challenge.id, observation_seq),
                )
                self._conn.execute(
                    "INSERT OR IGNORE INTO fact_evidence "
                    "(fact_seq,observation_seq,challenge_id) VALUES (?,?,?)",
                    (product_seq, observation_seq, self.challenge.id),
                )
            return product_seq

        route = self.normalize_route_hash(route_hash) if route_hash else ""
        claimed_verified = bool(verified)
        if (verified
                and getattr(self.challenge, "mode", "ctf") != "ctf"
                and not self._valid_fact_provenance(
            actor=actor,
            intent_id=iid,
            artifact_id=str(artifact_id or ""),
            provenance=evidence_provenance,
        )):
            # Every promotion must carry one host-checkable tool-event binding.
            verified = False
            confidence = min(float(confidence or 0.4), 0.4)

        clean_subject = " ".join(str(subject or "").split())[:1000]
        clean_predicate = " ".join(str(predicate or "").split()).casefold()[:160]
        clean_scope = " ".join(str(scope or target_epoch).split())[:300]
        supplied_key = " ".join(str(canonical_key or "").split()).casefold()[:1000]
        if supplied_key:
            fact_identity = supplied_key
        elif clean_subject and clean_predicate:
            fact_identity = "structured:" + hashlib.sha256(
                json.dumps(
                    [clean_subject.casefold(), clean_predicate, object_value, clean_scope],
                    ensure_ascii=False, sort_keys=True, default=str,
                ).encode("utf-8", errors="replace")
            ).hexdigest()
        else:
            fact_identity = _normalize_fact_identity(fact)
        identity_digest = hashlib.sha256(
            f"{target_epoch}\x1f{fact_identity}".encode(
                "utf-8", errors="replace")
        ).hexdigest()

        observation_payload = {
            "source": source, "text": fact, "source_solver": actor,
            "intent_id": iid, "target_epoch": target_epoch, "witness": witness,
            "artifact_id": artifact_id, "claimed_verified": claimed_verified,
            "admitted": bool(verified), "canonical_key": fact_identity,
            "evidence_provenance": evidence_provenance,
        }
        observation_identity = "\x1f".join((
            actor, iid, target_epoch,
            str(evidence_provenance.get("tool_event_seq") or 0),
            str(evidence_provenance.get("artifact_sha256") or ""),
            identity_digest,
        ))
        observation_key = hashlib.sha256(
            observation_identity.encode("utf-8", errors="replace")
        ).hexdigest()
        observation_seq = self._append_locked(
            EV_OBSERVATION_ADDED, actor, observation_payload,
            artifact_id=artifact_id, verified=verified, confidence=confidence,
            dedupe_key=f"observation::{self.challenge.id}::{observation_key}",
        )
        if observation_seq < 0:
            row = self._conn.execute(
                "SELECT seq FROM events WHERE challenge_id=? AND dedupe_key=?",
                (self.challenge.id, f"observation::{self.challenge.id}::{observation_key}"),
            ).fetchone()
            observation_seq = int(row[0]) if row else 0
        if observation_seq > 0:
            self._conn.execute(
                "INSERT OR IGNORE INTO observations (observation_seq,challenge_id,actor,intent_id,"
                "target_epoch,text,witness,artifact_id,provenance_json,canonical_key,"
                "admitted_fact_seq,created_at) VALUES (?,?,?,?,?,?,?,?,?,?,NULL,?)",
                (observation_seq, self.challenge.id, actor, iid, target_epoch, fact,
                 witness, artifact_id, json.dumps(evidence_provenance, default=str),
                 fact_identity, time.time()),
            )
        self._last_observation_seq = observation_seq
        if not verified:
            return 0

        existing = self._equivalent_fact_rows_locked(fact_identity, target_epoch)
        verifier = self._independent_verifier(
            actor=actor, verified=True, verifier=verifier, rows=existing)
        payload = {"source": source, "fact": fact, "source_solver": actor,
                   "witness": witness, "verifier": verifier,
                   "target_epoch": target_epoch, "canonical_key": fact_identity,
                   "subject": clean_subject, "predicate": clean_predicate,
                   "object": object_value, "scope": clean_scope}
        if route:
            payload["route_hash"] = route
        if iid:
            payload["intent_id"] = iid
        payload["claimed_verified"] = claimed_verified
        if evidence_provenance:
            payload["evidence_provenance"] = evidence_provenance
        # Dedupe on fact identity across the challenge, normalized to collapse the
        # skill/marker double-write: strip the "[engine]" tag, fold whitespace, drop
        # case; actor and artifact_id are provenance, not identity. This also keeps
        # two Workers from adding separate rows for the same observation.
        payload["fact_identity_sha256"] = identity_digest
        dk = f"fact-v2::{self.challenge.id}::{identity_digest}"
        prior_identity = self._conn.execute(
            "SELECT e.seq,COALESCE(fs.state,?) FROM events e "
            "LEFT JOIN fact_states fs ON fs.fact_seq=e.seq "
            "WHERE e.challenge_id=? AND e.dedupe_key=? LIMIT 1",
            (FACT_STATE_UNRESOLVED, self.challenge.id, dk),
        ).fetchone()
        existing_verified = next(
            (row for row in existing if row["verified"]), None)
        product_seq = 0
        if existing_verified is not None:
            product_seq = int(existing_verified["seq"])
            self._append_fact_observation_locked(
                fact_seq=product_seq, actor=actor, source=source, fact=fact,
                artifact_id=artifact_id, verified=True,
                confidence=confidence, witness=witness, verifier=verifier,
                intent_id=iid, target_epoch=target_epoch,
                provenance=evidence_provenance, observation_seq=observation_seq,
            )
        elif prior_identity is not None and str(prior_identity[1]) == FACT_STATE_CHALLENGED:
            # A fresh, bound observation can resolve a challenged Fact while
            # retaining its stable fact_seq.  This is the explicit review
            # lifecycle; normal observations never use fact_promoted.
            product_seq = int(prior_identity[0])
            self._append_fact_observation_locked(
                fact_seq=product_seq, actor=actor, source=source, fact=fact,
                artifact_id=artifact_id, verified=True, confidence=confidence,
                witness=witness, verifier=verifier, intent_id=iid,
                target_epoch=target_epoch, provenance=evidence_provenance,
                observation_seq=observation_seq,
            )
            revalidated_payload = {
                "fact_seq": product_seq, "status": "revalidated",
                "reason": "fresh bound tool evidence",
                "revalidated_by": actor, "observation_seq": observation_seq,
                "evidence_provenance": evidence_provenance,
            }
            revalidated_seq = self._append_locked(
                EV_FACT_REVALIDATED, actor, revalidated_payload,
                artifact_id=artifact_id, verified=True, confidence=confidence,
                dedupe_key=(
                    f"fact-revalidated-evidence::{self.challenge.id}::"
                    f"{product_seq}::{observation_seq}"
                ),
            )
            self._conn.execute(
                "UPDATE fact_reviews SET status='revalidated',revalidated_seq=?,reason=? "
                "WHERE challenge_id=? AND fact_seq=?",
                (revalidated_seq if revalidated_seq > 0 else None,
                 "fresh bound tool evidence", self.challenge.id, product_seq),
            )
            self._upsert_fact_state(
                product_seq, FACT_STATE_REVALIDATED,
                reason="fresh bound tool evidence", verified_effective=1,
                confidence_effective=float(confidence),
                revalidated_seq=revalidated_seq if revalidated_seq > 0 else None,
                updated_seq=revalidated_seq if revalidated_seq > 0 else None,
            )
        else:
            if prior_identity is not None:
                # A rejected/merged/superseded historical Fact remains in the
                # audit log.  Fresh admitted evidence starts a new canonical
                # version instead of being blocked by the old dedupe row.
                dk = f"{dk}::reassert::{observation_seq}"
            seq = self._append_locked(
                EV_FACT_ADDED, actor, payload,
                artifact_id=artifact_id, verified=True,
                confidence=confidence, dedupe_key=dk,
            )
            if seq > 0:
                product_seq = seq
                self._upsert_fact_state(
                    product_seq, state=FACT_STATE_UNRESOLVED,
                    verified_effective=1, confidence_effective=float(confidence),
                    reason="admitted by bound tool evidence", updated_seq=seq,
                )
            else:
                refreshed = self._equivalent_fact_rows_locked(
                    fact_identity, target_epoch)
                verifier = self._independent_verifier(
                    actor=actor, verified=True, verifier=verifier,
                    rows=refreshed)
                preferred = next(
                    (row for row in refreshed if row["verified"]), None)
                if preferred is not None:
                    product_seq = int(preferred["seq"])
            if product_seq > 0:
                self._append_fact_observation_locked(
                    fact_seq=product_seq, actor=actor, source=source, fact=fact,
                    artifact_id=artifact_id, verified=True, confidence=confidence,
                    witness=witness, verifier=verifier, intent_id=iid,
                    target_epoch=target_epoch, provenance=evidence_provenance,
                    observation_seq=observation_seq,
                )
        # A FACT_CHALLENGE verifier may phrase its evidence differently from the
        # original candidate, so actor/text dedupe cannot close that lifecycle.
        # Any evidence-backed fact produced by the challenge's dedicated intent
        # replaces the old candidate.  A verifier that finds no verified result
        # leaves the challenge open.
        if product_seq > 0 and iid:
            challenged = self._conn.execute(
                "SELECT fact_seq FROM fact_reviews "
                "WHERE challenge_id=? AND status='challenged' "
                "AND verification_intent_id=? ORDER BY fact_seq LIMIT 1",
                (self.challenge.id, iid),
            ).fetchone()
            if challenged is not None:
                challenged_seq = int(challenged[0])
                if challenged_seq != product_seq:
                    self._supersede_fact_locked(
                        actor=actor,
                        fact_seq=challenged_seq,
                        reason="verifier evidence supersedes challenged fact",
                        by_fact_seq=product_seq,
                    )
        if product_seq > 0 and iid:
            self._conn.execute(
                "INSERT OR IGNORE INTO intent_products "
                "(intent_id, fact_seq) VALUES (?,?)",
                (iid, product_seq),
            )
        return product_seq

    def add_dead_end(self, *, actor: str, reason: str, intent_id: str = "",
                     route_hash: str = "", coverage_key: str = "",
                     target_epoch: str = "", tested_scope: str = "",
                     observed_result: str = "") -> int:
        with self._lock:
            try:
                seq = self._add_dead_end_locked(
                    actor=actor, reason=reason, intent_id=intent_id,
                    route_hash=route_hash, coverage_key=coverage_key,
                    target_epoch=target_epoch, tested_scope=tested_scope,
                    observed_result=observed_result,
                )
            except BaseException:
                self._conn.rollback()
                raise
            self._conn.commit()
            return seq

    def _add_dead_end_locked(self, *, actor: str, reason: str,
                             intent_id: str = "", route_hash: str = "",
                             coverage_key: str = "",
                             target_epoch: str = "", tested_scope: str = "",
                             observed_result: str = "") -> int:
        """``add_dead_end`` body with ``self._lock`` held and no commit. The
        payload always carries the intent/route/coverage/epoch binding (empty
        strings when unscoped) so consumers can scope the dead end by it."""
        iid = str(intent_id or "").strip()
        if getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}:
            reason = str(reason or "").strip()
            tested = str(tested_scope or "").strip()
            observed = str(observed_result or "").strip()
            if not reason or not tested or not observed:
                return -1
            digest = hashlib.sha256(
                "\x1f".join((actor, iid, reason, tested, observed)).encode(
                    "utf-8", errors="replace"
                )
            ).hexdigest()
            return self._append_locked(
                EV_DEAD_END,
                actor,
                {"reason": reason, "tested_scope": tested,
                 "observed_result": observed, "intent_id": iid,
                 "target_epoch": str(target_epoch or "").strip()},
                dedupe_key=f"ctf-deadend::{self.challenge.id}::{digest}",
            )
        route = (
            self.normalize_route_hash(str(route_hash))
            if str(route_hash or "").strip() else ""
        )
        coverage = " ".join(str(coverage_key or "").split()).casefold()
        epoch = str(target_epoch or "").strip()
        tested = str(tested_scope or "").strip()
        observed = str(observed_result or "").strip()
        if self._has_near_duplicate_dead_end_locked(
            reason, intent_id=iid, route_hash=route,
            coverage_key=coverage, target_epoch=epoch,
        ):
            return -1
        payload = {
            "reason": reason,
            "tested_scope": tested,
            "observed_result": observed,
            "intent_id": iid,
            "route_hash": route,
            "coverage_key": coverage,
            "target_epoch": epoch,
        }
        identity = json.dumps(
            [iid, route, coverage, epoch, self._norm_dead_end_text(reason)],
            ensure_ascii=False, separators=(",", ":"),
        )
        digest = hashlib.sha256(identity.encode("utf-8")).hexdigest()
        return self._append_locked(EV_DEAD_END, actor, payload,
                                   dedupe_key=f"deadend::{digest}")

    @staticmethod
    def _norm_dead_end_text(text: str) -> str:
        s = (text or "").strip().casefold()
        s = re.sub(r"\bthree\b", "3", s)
        s = re.sub(r"\btwo\b", "2", s)
        s = re.sub(r"\bone\b", "1", s)
        # Python's Unicode-aware ``\w`` retains Chinese and other non-Latin
        # evidence text.  ASCII-only normalization made every Chinese reason an
        # empty identity and collapsed unrelated dead ends in the same scope.
        s = re.sub(r"[\W_]+", " ", s)
        return re.sub(r"\s+", " ", s).strip()

    def _has_near_duplicate_dead_end(
        self, reason: str, *, intent_id: str = "", route_hash: str = "",
        coverage_key: str = "", target_epoch: str = "",
        threshold: float = 0.92,
    ) -> bool:
        with self._lock:
            return self._has_near_duplicate_dead_end_locked(
                reason, intent_id=intent_id, route_hash=route_hash,
                coverage_key=coverage_key, target_epoch=target_epoch,
                threshold=threshold)

    def _has_near_duplicate_dead_end_locked(
        self, reason: str, *, intent_id: str = "", route_hash: str = "",
        coverage_key: str = "", target_epoch: str = "",
        threshold: float = 0.92,
    ) -> bool:
        """``_has_near_duplicate_dead_end`` with ``self._lock`` already held."""
        target = self._norm_dead_end_text(reason)
        if not target:
            return False
        iid = str(intent_id or "").strip()
        route = (
            self.normalize_route_hash(str(route_hash))
            if str(route_hash or "").strip() else ""
        )
        coverage = " ".join(str(coverage_key or "").split()).casefold()
        epoch = str(target_epoch or "").strip()
        target_nums = set(re.findall(r"\b\d+\b", target))
        rows = self._conn.execute(
            "SELECT json_extract(payload,'$.reason') FROM events "
            "WHERE challenge_id=? AND kind=? "
            "AND COALESCE(json_extract(payload,'$.intent_id'),'')=? "
            "AND COALESCE(json_extract(payload,'$.route_hash'),'')=? "
            "AND COALESCE(json_extract(payload,'$.coverage_key'),'')=? "
            "AND COALESCE(json_extract(payload,'$.target_epoch'),'')=? "
            "ORDER BY seq DESC LIMIT 200",
            (self.challenge.id, EV_DEAD_END, iid, route, coverage, epoch),
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
                   intent_id: Optional[str] = None,
                   complete_intent: bool = False) -> int:
        """Persist a distinct Flag and, when complete, close its Intent atomically."""
        payload = {"flag": flag}
        if intent_id:
            payload["intent_id"] = intent_id
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                seq = self._append_locked(
                    EV_FLAG_FOUND,
                    actor,
                    payload,
                    artifact_id=artifact_id,
                    verified=True,
                    dedupe_key=f"flag::{flag}",
                )
                if seq < 0:
                    self._conn.rollback()
                    return seq
                if complete_intent and intent_id:
                    self._conclude_intent_locked(
                        actor=actor,
                        intent_id=str(intent_id),
                        result="solved",
                        result_detail=(
                            "Configured Flag count reached by explicit submission."
                        ),
                    )
                self._conn.commit()
                return seq
            except BaseException:
                self._conn.rollback()
                raise

    def flag_submission(
        self, *, actor: str, submission_id: str, flag: str,
        intent_id: Optional[str] = None,
        protocol: str = "blackboard-api-v1",
    ) -> int:
        """Record one unverified Worker API request through the host DB owner."""
        payload = {
            "submission_id": str(submission_id),
            "flag": str(flag),
            "intent_id": str(intent_id or ""),
            "protocol": str(protocol or "blackboard-api-v1"),
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

    def resolve_flag_submission(
        self, *, actor: str, submission_id: str, flag: str,
        intent_id: Optional[str] = None,
        protocol: str = "blackboard-api-v1",
        accepted: bool, code: str, detail: str = "",
        ensure_submission: bool = True,
    ) -> dict[str, Any]:
        """Commit one Flag submission's authoritative terminal state atomically.

        Submission IDs are idempotency keys, not mutable request handles.  A retry
        must therefore return the original decision rather than recomputing a new
        verdict against different evidence.  When the decision is accepted, its
        durable ``flag_found`` projection is written in the same SQLite transaction;
        a process crash cannot leave an accepted decision stranded without a Flag.

        The returned dictionary contains the canonical decision already stored in
        the graph.  Callers must project *that* decision to the event bus instead of
        trusting their tentative verdict.
        """
        sid = str(submission_id or "")
        candidate = str(flag or "")
        owner = str(actor or "")
        intent = str(intent_id or "")
        wire_protocol = str(protocol or "blackboard-api-v1")
        submit_key = f"flag-submission::{sid}"
        decision_key = f"flag-submission-decision::{sid}"
        flag_key = f"flag::{candidate}"

        def _payload(row: Any) -> dict[str, Any]:
            try:
                value = json.loads(str(row or "{}"))
            except (TypeError, ValueError, json.JSONDecodeError):
                value = {}
            return value if isinstance(value, dict) else {}

        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                submission_row = self._conn.execute(
                    "SELECT seq, actor, payload FROM events WHERE dedupe_key=?",
                    (submit_key,),
                ).fetchone()
                submission_seq = 0
                if submission_row is not None:
                    submission_seq = int(submission_row[0] or 0)
                    prior_actor = str(submission_row[1] or "")
                    prior = _payload(submission_row[2])
                    prior_flag = str(prior.get("flag") or "")
                    prior_protocol = str(
                        prior.get("protocol") or "blackboard-api-v1")
                    if (prior_actor != owner or prior_flag != candidate
                            or prior_protocol != wire_protocol):
                        self._conn.commit()
                        return {
                            "submission_seq": submission_seq,
                            "decision_seq": 0,
                            "flag_found_seq": 0,
                            "accepted": False,
                            "code": "submission_conflict",
                            "detail": (
                                "submission id is already bound to a different "
                                "candidate or owner"
                            ),
                            "created": False,
                        }
                elif not ensure_submission:
                    self._conn.commit()
                    return {
                        "submission_seq": 0,
                        "decision_seq": 0,
                        "flag_found_seq": 0,
                        "accepted": False,
                        "code": "submission_missing",
                        "detail": "submission was not durably recorded",
                        "created": False,
                    }
                else:
                    payload = {
                        "submission_id": sid,
                        "flag": candidate,
                        "intent_id": intent,
                        "protocol": wire_protocol,
                    }
                    cur = self._conn.execute(
                        "INSERT INTO events "
                        "(ts, challenge_id, actor, kind, payload, artifact_id, "
                        " verified, confidence, dedupe_key) "
                        "VALUES (?,?,?,?,?,?,?,?,?)",
                        (time.time(), self.challenge.id, owner,
                         EV_FLAG_SUBMISSION, json.dumps(payload, default=str),
                         None, 0, 1.0, submit_key),
                    )
                    submission_seq = int(cur.lastrowid or 0)

                decision_row = self._conn.execute(
                    "SELECT seq, payload FROM events WHERE dedupe_key=?",
                    (decision_key,),
                ).fetchone()
                created = False
                if decision_row is None:
                    canonical_accepted = bool(accepted)
                    canonical_code = str(code or "rejected")
                    canonical_detail = str(detail or "")[:240]
                    decision_payload = {
                        "submission_id": sid,
                        "accepted": canonical_accepted,
                        "code": canonical_code,
                        "detail": canonical_detail,
                    }
                    cur = self._conn.execute(
                        "INSERT INTO events "
                        "(ts, challenge_id, actor, kind, payload, artifact_id, "
                        " verified, confidence, dedupe_key) "
                        "VALUES (?,?,?,?,?,?,?,?,?)",
                        (time.time(), self.challenge.id, owner,
                         EV_FLAG_SUBMISSION_DECISION,
                         json.dumps(decision_payload, default=str), None,
                         int(canonical_accepted), 1.0, decision_key),
                    )
                    decision_seq = int(cur.lastrowid or 0)
                    created = True
                else:
                    decision_seq = int(decision_row[0] or 0)
                    prior = _payload(decision_row[1])
                    canonical_accepted = bool(prior.get("accepted"))
                    canonical_code = str(prior.get("code") or "rejected")
                    canonical_detail = str(prior.get("detail") or "")[:240]

                flag_found_seq = 0
                if canonical_accepted:
                    flag_row = self._conn.execute(
                        "SELECT seq FROM events WHERE dedupe_key=?", (flag_key,)
                    ).fetchone()
                    if flag_row is not None:
                        flag_found_seq = int(flag_row[0] or 0)
                    else:
                        flag_payload = {"flag": candidate}
                        if intent:
                            flag_payload["intent_id"] = intent
                        cur = self._conn.execute(
                            "INSERT INTO events "
                            "(ts, challenge_id, actor, kind, payload, artifact_id, "
                            " verified, confidence, dedupe_key) "
                            "VALUES (?,?,?,?,?,?,?,?,?)",
                            (time.time(), self.challenge.id, owner, EV_FLAG_FOUND,
                             json.dumps(flag_payload, default=str), None, 1, 1.0,
                             flag_key),
                        )
                        flag_found_seq = int(cur.lastrowid or 0)
                self._conn.commit()
                return {
                    "submission_seq": submission_seq,
                    "decision_seq": decision_seq,
                    "flag_found_seq": flag_found_seq,
                    "accepted": canonical_accepted,
                    "code": canonical_code,
                    "detail": canonical_detail,
                    "created": created,
                }
            except Exception:
                self._conn.rollback()
                raise

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
                           recommended_actions: Optional[list[str]] = None,
                           worker: str = "") -> int:
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
            # The review worker that raised the finding (actor stays the coordinator
            # that applied it).
            "worker": (worker or "").strip(),
        }
        return self._append(EV_REVIEW_FINDING, actor, payload,
                            dedupe_key=f"review::{payload['kind']}::{payload['summary']}::{route}")

    def add_review_proposal(self, *, actor: str, marker: str, payload: dict) -> int:
        marker = (marker or "").strip().upper()
        if marker not in REVIEW_FACT_MARKERS:
            raise ValueError(f"marker is outside Review authority: {marker}")
        clean_payload = dict(payload or {})
        route_hash = str(clean_payload.get("route_hash") or "").strip()
        if route_hash:
            clean_payload["route_hash"] = self.normalize_route_hash(route_hash)
        confidence = clean_payload.get("confidence", 1.0)
        try:
            confidence = float(confidence)
        except (TypeError, ValueError):
            confidence = 1.0
        clean_payload["confidence"] = max(0.0, min(1.0, confidence))
        payload_out = {
            "marker": marker,
            "tier": "tier1",
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
            payload={"worker_class": "verifier",
                     "rationale": f"Review challenged fact #{fact_seq}: {reason}",
                     "expected_observable": (
                         f"Fresh tool output that confirms or contradicts fact #{fact_seq}"
                     ),
                     "stop_condition": (
                         "Stop after the bounded reproduction attempt and record "
                         "verified evidence or a dead end"
                     )},
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
        with self._lock:
            try:
                seq = self._supersede_fact_locked(
                    actor=actor, fact_seq=fact_seq, reason=reason,
                    by_fact_seq=by_fact_seq,
                )
            except BaseException:
                self._conn.rollback()
                raise
            self._conn.commit()
            return seq

    def _supersede_fact_locked(self, *, actor: str, fact_seq: int,
                               reason: str = "",
                               by_fact_seq: Optional[int] = None) -> int:
        """``supersede_fact`` body with ``self._lock`` held and no commit
        (transactional reuse from ``_add_evidence_locked``)."""
        fact_seq = int(fact_seq)
        payload = {"fact_seq": fact_seq, "status": FACT_STATE_SUPERSEDED,
                   "reason": (reason or "").strip()[:1000], "superseded_by": actor}
        if by_fact_seq is not None:
            payload["by_fact_seq"] = int(by_fact_seq)
        seq = self._append_locked(EV_FACT_SUPERSEDED, actor, payload,
                                  dedupe_key=f"fact-superseded::{fact_seq}::{payload['reason']}")
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
        """Legacy compatibility surface; candidates no longer exist as Facts."""
        return []

    def observations(self, *, limit: int = 200, unadmitted_only: bool = False) -> list[dict]:
        """Bounded raw evidence for audit and explicit verifier retrieval."""
        where = "challenge_id=?"
        args: list[Any] = [self.challenge.id]
        if unadmitted_only:
            where += " AND admitted_fact_seq IS NULL"
        with self._lock:
            rows = self._conn.execute(
                "SELECT observation_seq,actor,intent_id,target_epoch,text,witness,artifact_id,"
                "canonical_key,admitted_fact_seq,created_at FROM observations "
                f"WHERE {where} ORDER BY observation_seq DESC LIMIT ?",
                (*args, int(limit)),
            ).fetchall()
        return [{"observation_seq": int(r[0]), "actor": r[1], "intent_id": r[2] or "",
                 "target_epoch": r[3], "text": r[4], "witness": r[5],
                 "artifact_id": r[6], "canonical_key": r[7],
                 "admitted_fact_seq": (int(r[8]) if r[8] is not None else None),
                 "created_at": float(r[9])} for r in reversed(rows)]

    def verified_evidence(self) -> list[dict]:
        """Facts that are verified (origin or revalidated) AND not retired."""
        texts = self._fact_text_by_seq()
        states = (
            {}
            if getattr(self.challenge, "mode", "ctf") == "ctf"
            else self._fact_state_map()
        )
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
            if (st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES
                    or st.get("state") == FACT_STATE_CHALLENGED):
                continue
            eff = st.get("verified_effective")
            is_verified = bool(verified) if eff is None else eff
            if is_verified:
                out.append({"fact_seq": seq, "fact": texts.get(seq, "")})
        return out

    def verified_fact_rows(self, *, limit: int = 200) -> list[dict]:
        """Active verified facts with seq/text/route_hash, newest first, bounded.

        Deterministic input for host-side scans over the verified set (the
        semantic-duplicate review trigger buckets by route_hash). Unlike
        verified_evidence() this carries the route bucket; shape kept separate so
        the long-standing verified_evidence() contract is untouched."""
        states = (
            {}
            if getattr(self.challenge, "mode", "ctf") == "ctf"
            else self._fact_state_map()
        )
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, verified, "
                "json_extract(payload,'$.fact'), "
                "json_extract(payload,'$.route_hash') "
                "FROM events WHERE challenge_id=? AND kind=? ORDER BY seq DESC "
                "LIMIT ?",
                (self.challenge.id, EV_FACT_ADDED, int(limit)),
            ).fetchall()
        out: list[dict] = []
        for seq, verified, fact, route in rows:
            seq = int(seq)
            st = states.get(seq, {})
            state = st.get("state", FACT_STATE_UNRESOLVED)
            if st.get("retired") or state in _FACT_TERMINAL_STATES:
                continue
            eff = st.get("verified_effective")
            is_verified = bool(verified) if eff is None else eff
            if state == FACT_STATE_CHALLENGED:
                is_verified = False
            if not is_verified:
                continue
            out.append({"fact_seq": seq, "fact": str(fact or ""),
                        "route_hash": str(route or "")})
        return out
