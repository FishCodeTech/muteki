"""Shared, evidence-bearing, event-sourced solve graph.

A shared, evolving solve graph WITH a provenance gate on every fact: each fact
carries the evidence (and the event) that produced it, so the graph is not just
a scratchpad but an auditable record of what was actually proven.

Design (A+B+C+D):
- (D) Local direct SQLite file, ONE per challenge. No HTTP server: same-host
  sub-process workers open the same `.db`; WAL natively supports multi-process
  concurrent read/write. A `SharedGraph` Protocol keeps the backend swappable
  (a cross-container HTTP backend can be added later without touching callers).
- (A) One long-lived connection per instance + one-time PRAGMA, incl.
  `busy_timeout` (avoids lost writes: SQLITE_BUSY → auto-queue, not drop) +
  `synchronous=NORMAL` (safe & fast under WAL).
- (C) The source of truth is an append-only `events` table (INSERT only, never
  UPDATE/DELETE). `facts`/`intents` are MATERIALIZED views folded from events —
  droppable & rebuildable. Provenance is free (every fact's origin is its event);
  the analytics flywheel reads the raw event log; time-travel replay is possible.
- (B) Intent claiming is a single atomic UPDATE guarded by `changes()` — zero
  TOCTOU window (used once the reasoner dispatches intents).

Invariant: only an explicit model ``submit-flag`` declaration reaches
``flag_found``. The graph does not inspect the submitted value.
"""

from __future__ import annotations

import json
import sqlite3
import threading
import time
from pathlib import Path
from typing import Any, Optional, Protocol, runtime_checkable

from muteki.models.solve_graph import Challenge, SolveGraph


# Event-type vocabulary, lifecycle-state sets, and the stateless lane/fact helpers
# live in graph_defs (code-health G1). Re-exported here so the historical public
# surface (`from muteki.swarm.shared_graph import EV_*, canonicalize_lane, …`) is
# unchanged.
from muteki.swarm.graph_defs import (  # noqa: E402,F401
    EV_FACT_ADDED,
    EV_FACT_OBSERVED,
    EV_FACT_PROMOTED,
    EV_OBSERVATION_ADDED,
    EV_HYP_PROPOSED,
    EV_HYP_REFUTED,
    EV_DEAD_END,
    EV_INTENT_PROPOSED,
    EV_INTENT_CLAIMED,
    EV_INTENT_CONCLUDED,
    EV_FLAG_FOUND,
    EV_FLAG_INVALIDATED,
    EV_FLAG_SUBMISSION,
    EV_FLAG_SUBMISSION_DECISION,
    EV_FINDING_FOUND,
    EV_FINDING_INVALIDATED,
    EV_POC_SAVED,
    EV_POC_CLAIMED,
    EV_POC_CONCLUDED,
    EV_REVIEW_FINDING,
    EV_FACT_CHALLENGED,
    EV_FACT_REVALIDATED,
    EV_ROUTE_SUPPRESSED,
    EV_ROUTE_REOPENED,
    EV_BRANCH_SPLIT,
    EV_BRANCH_FACTS_BOUND,
    EV_BRANCH_RESOLVED,
    EV_COORDINATOR_DIRECTIVE,
    EV_REVIEW_PROPOSAL,
    EV_REVIEW_PROPOSAL_DECISION,
    EV_LANE_LOCKED,
    EV_LANE_RELEASED,
    EV_INTENT_LANE_DEFERRED,
    EV_FACT_REJECTED,
    EV_FACT_MERGED,
    EV_FACT_SUPERSEDED,
    EV_FACT_PINNED,
    EV_INTENT_STATE_CHANGED,
    EV_OPERATOR_DIRECTIVE,
    EV_OPERATOR_DIRECTIVE_STATUS,
    EV_CONTROL_STANDING_CLEAR_APPLIED,
    EV_HITL_CLASSIFIED,
    EV_RESOURCE_LOCKED,
    EV_RESOURCE_RELEASED,
    EV_GRAPH_COMPACTED,
    EV_WORKER_RESULT_COMMITTED,
    EV_CAPABILITY_PUBLISHED,
    EV_CAPABILITY_RETIRED,
    EV_CAPABILITY_GAP_REPORTED,
    EV_ACCESS_PATH_PUBLISHED,
    EV_ACCESS_PATH_STATE_CHANGED,
    EV_RUNTIME_RESOURCE_REGISTERED,
    EV_RUNTIME_RESOURCE_STATE_CHANGED,
    EV_VALUE_RECEIPT,
    SEMANTIC_GRAPH_KINDS,
    FACT_STATE_UNRESOLVED,
    FACT_STATE_CHALLENGED,
    FACT_STATE_REVALIDATED,
    FACT_STATE_REJECTED,
    FACT_STATE_MERGED,
    FACT_STATE_SUPERSEDED,
    _FACT_TERMINAL_STATES,
    _FACT_STATES,
    INTENT_DISPATCH_ACTIVE,
    INTENT_DISPATCH_RESUME,
    INTENT_DISPATCH_RETIRED,
    INTENT_DISPATCH_CLOSED,
    INTENT_DISPATCH_BLOCKED,
    _INTENT_DISPATCH_STATES,
    _SERVICE_DEFAULT_PORTS,
    _LANE_RISK_CLASSES,
    _FACT_ENGINE_PREFIX_RE,
    _normalize_fact_identity,
    _clean_lane_risk,
    _clean_lane_host,
    canonicalize_lane,
)


@runtime_checkable
class SharedGraph(Protocol):
    """Backend-swappable shared graph. Local = SQLite file; (future) cross-
    container = HTTP. Callers depend only on this surface."""

    def add_evidence(self, *, actor: str, source: str, fact: str,
                     artifact_id: Optional[str] = None, verified: bool = False,
                     confidence: float = 1.0, witness: Optional[str] = None,
                     verifier: str = "", route_hash: str = "",
                     intent_id: Optional[str] = None,
                     provenance: Optional[dict[str, Any]] = None,
                     subject: str = "", predicate: str = "",
                     object_value: Any = None, scope: str = "",
                     canonical_key: str = "") -> int: ...

    def add_dead_end(self, *, actor: str, reason: str, intent_id: str = "",
                     route_hash: str = "", coverage_key: str = "",
                     target_epoch: str = "", tested_scope: str = "",
                     observed_result: str = "") -> int: ...

    def flag_found(self, *, actor: str, flag: str,
                   artifact_id: Optional[str] = None,
                   intent_id: Optional[str] = None,
                   complete_intent: bool = False) -> int: ...

    def flag_submission(
        self, *, actor: str, submission_id: str, flag: str,
        intent_id: Optional[str] = None,
        protocol: str = "blackboard-api-v1",
    ) -> int: ...

    def flag_submission_decision(
        self, *, actor: str, submission_id: str, accepted: bool,
        code: str, detail: str = "",
    ) -> int: ...

    def resolve_flag_submission(
        self, *, actor: str, submission_id: str, flag: str,
        intent_id: Optional[str] = None,
        protocol: str = "blackboard-api-v1",
        accepted: bool, code: str, detail: str = "",
        ensure_submission: bool = True,
    ) -> dict[str, Any]: ...

    def finding_found(self, *, actor: str, finding: dict,
                      artifact_id: Optional[str] = None,
                      intent_id: Optional[str] = None) -> int: ...

    def finding_invalidated(self, *, actor: str, finding: dict | str) -> int: ...

    def propose_intent(self, *, actor: str, intent_id: str, goal: str,
                       payload: Optional[dict] = None,
                       from_fact_seqs: Optional[list[int]] = None) -> int: ...

    def start_intent(self, *, actor: str, worker: str, intent_id: str,
                     goal: str, worker_class: str = "code") -> bool: ...

    def claim_intent(self, *, worker: str, intent_id: str) -> bool: ...

    def query_legacy_candidates(self) -> list[dict]: ...

    def apply_legacy_lane_inferences(
        self, *, inferences: list[tuple[str, str, str]],
    ) -> None: ...

    def release_intent_claim(self, *, worker: str, intent_id: str,
                             reason: str = "") -> bool: ...

    def block_intent_context(self, *, actor: str, intent_id: str,
                             missing: list[str], reason: str = "") -> bool: ...

    def intent_claim_state(self, intent_id: str) -> dict[str, str]: ...

    def claimed_intent_successor(
        self, *, worker: str, intent_id: str,
    ) -> dict[str, Any]: ...

    def terminalize_intent_claim(
        self, *, worker: str, intent_id: str, reason: str = "",
    ) -> bool: ...

    def conclude_intent(self, *, actor: str, intent_id: str,
                        result: str = "",
                        to_fact_seq: Optional[int] = None,
                        result_detail: str = "") -> int: ...

    def save_poc(self, *, actor: str, poc_id: str, path: str,
                 entry_command: str, status: str = "available",
                 note: str = "", artifact_id: Optional[str] = None,
                 intent_id: Optional[str] = None, name: str = "") -> int: ...

    def claim_poc(self, *, worker: str, poc_id: str,
                  lease_s: float = 300.0) -> bool: ...

    def conclude_poc(self, *, actor: str, poc_id: str,
                     status: str = "spent", note: str = "") -> int: ...

    def commit_worker_result(self, *, actor: str, worker_id: str,
                             intent_id: Optional[str], target_epoch: str = "",
                             run_id: str = "", status: str,
                             checkpoint_id: str = "",
                             result_detail: str = "",
                             produced_new_info: bool = False,
                             stop_condition_result: str = "",
                             handoff_missing: bool = False,
                             observations: Optional[list[dict]] = None,
                             dead_ends: Optional[list[dict]] = None,
                             pocs: Optional[list[dict]] = None,
                             artifacts: Optional[list[str]] = None,
                             need_input: Optional[dict] = None,
                             conclude: bool = True,
                             successor_intent_id: str = "") -> dict: ...

    def supersede_open_intents(self, *, actor: str, match: str,
                               reason: str = "",
                               exclude_ids: Optional[list[str]] = None) -> list[str]: ...

    def supersede_open_intent_ids(self, *, actor: str,
                                  intent_ids: list[str],
                                  reason: str = "") -> list[str]: ...

    def reprioritize_open_intents(self, *, actor: str,
                                  priorities: dict[str, str]) -> list[str]: ...

    def add_review_finding(self, *, actor: str, kind: str, severity: str,
                           summary: str, evidence_seqs: Optional[list[int]] = None,
                           intent_ids: Optional[list[str]] = None,
                           route_hash: str = "", branch_id: str = "",
                           recommended_actions: Optional[list[str]] = None,
                           worker: str = "") -> int: ...

    def add_review_proposal(self, *, actor: str, marker: str,
                            payload: dict) -> int: ...

    def decide_review_proposal(self, *, actor: str, proposal_seq: int,
                               decision: str, reason: str = "",
                               applied_seq: Optional[int] = None) -> int: ...

    def challenge_fact(self, *, actor: str, fact_seq: int, reason: str,
                       verification_goal: str) -> dict: ...

    def revalidate_fact(self, *, actor: str, fact_seq: int, reason: str = "") -> int: ...

    def reject_fact(self, *, actor: str, fact_seq: int, reason: str = "") -> int: ...

    def merge_fact(self, *, actor: str, from_fact_seq: int, to_fact_seq: int,
                   reason: str = "") -> int: ...

    def supersede_fact(self, *, actor: str, fact_seq: int, reason: str = "",
                       by_fact_seq: Optional[int] = None) -> int: ...

    def review_fact(self, *, actor: str, fact_seq: int, action: str,
                    reason: str = "", verification_goal: str = "",
                    to_fact_seq: Optional[int] = None) -> dict: ...

    def active_candidates(self) -> list[dict]: ...

    def observations(self, *, limit: int = 200,
                     unadmitted_only: bool = False) -> list[dict]: ...

    def verified_evidence(self) -> list[dict]: ...

    def verified_fact_rows(self, *, limit: int = 200) -> list[dict]: ...

    def publish_capability(self, **kwargs: Any) -> int: ...

    def active_capabilities(self, target_epoch: str = "") -> list[dict[str, Any]]: ...

    def active_capability_keys(self, target_epoch: str = "") -> set[str]: ...

    def report_capability_gap(self, **kwargs: Any) -> Optional[dict[str, Any]]: ...

    def open_capability_gaps(self, target_epoch: str = "") -> list[dict[str, Any]]: ...

    def register_runtime_resource(self, **kwargs: Any) -> int: ...

    def publish_access_path(self, **kwargs: Any) -> int: ...

    def active_access_paths(self, *, target_epoch: str = "",
                            required_capabilities: Optional[list[str]] = None,
                            ) -> list[dict[str, Any]]: ...

    def refresh_access_path_health(self, **kwargs: Any) -> list[dict[str, Any]]: ...

    def stop_runtime_resources(self, *, actor: str = "coordinator",
                               target_epoch: str = "") -> int: ...

    def suppress_route(self, *, actor: str, route_hash: str, label: str = "",
                       reason: str = "", until: str = "new_evidence",
                       matching_intents: Optional[list[str]] = None) -> dict: ...

    def reopen_route(self, *, actor: str, route_hash: str, reason: str = "",
                     intent_goal: str = "") -> dict: ...

    def split_branch(self, *, actor: str, title: str,
                     branches: list[dict[str, Any]],
                     parent_id: str = "") -> dict: ...

    def bind_open_branch_facts(self, *, actor: str, source_intent: str,
                               fact_seqs: list[int]) -> dict: ...

    def resolve_branch(self, *, actor: str, branch_id: str, reason: str = "",
                       status: str = "resolved") -> dict: ...

    def add_coordinator_directive(self, *, actor: str, action: str,
                                  directive: str, priority: str = "normal",
                                  route_hash: str = "") -> int: ...

    def add_operator_directive(self, *, actor: str = "operator", action: str,
                               text: str, scope: str = "global",
                               standing: bool = False,
                               preempt_policy: str = "soft_rebind",
                               priority: Optional[int] = None,
                               source_command_id: str = "") -> dict: ...

    def update_directive_status(self, *, directive_id: str, status: str,
                                actor: str = "coordinator",
                                generated_fact_seq: Optional[int] = None,
                                generated_intent_id: Optional[str] = None,
                                bound_worker: Optional[str] = None,
                                conflicts: Optional[list[str]] = None) -> int: ...

    def operator_directives(self, *, active_only: bool = True) -> list[dict]: ...

    def expire_standing_directives(self, *, actor: str = "operator",
                                   text: str = "") -> list[str]: ...

    def apply_standing_clear(self, *, command_id: str,
                             actor: str = "operator", text: str = "",
                             cutoff_before: Optional[float] = None,
                             eligible_command_ids: Optional[list[str]] = None,
                             match_by_source_ids: bool = False) -> dict: ...

    def add_hitl_request(self, *, worker: str, need: str, need_kind: str,
                         classification_confidence: float = 1.0,
                         status: str = "classified",
                         request_id: Optional[str] = None,
                         directive_id: Optional[str] = None,
                         resource_lock_id: Optional[str] = None,
                         auto_action_seq: Optional[int] = None) -> dict: ...

    def lock_lane(self, *, actor: str, lane_key: str, risk_class: str,
                  owner_worker: str, owner_intent: str,
                  lease_s: float = 900.0) -> dict: ...

    def release_lane(self, *, actor: str, lane_key: str,
                     by_worker: str = "") -> dict: ...

    def defer_intent_for_lane(self, *, actor: str, intent_id: str,
                              lane_key: str, against_locked_seq: int = 0) -> int: ...

    def active_lanes(self) -> list[dict]: ...

    def request_resource_lock(self, *, actor: str, resource_key: str,
                              scope: str = "activity", risk_class: str = "",
                              owner_worker: str = "", owner_intent: str = "",
                              conflict_policy: str = "exclusive",
                              lease_s: float = 600.0, cooldown_s: float = 0.0) -> dict: ...

    def release_resource_lock(self, *, actor: str, resource_key: str = "",
                              lock_id: str = "", by_worker: str = "") -> dict: ...

    def active_resource_locks(self) -> list[dict]: ...

    def check_resource_conflicts(self, *, resource_key: str = "", lane_key: str = "",
                                 by_worker: str = "") -> dict: ...

    def is_lane_held_by_other(self, lane_key: str, by_worker: str) -> bool: ...

    def in_lane_cooldown(self, lane_key: str, worker: str) -> bool: ...

    def release_claims_for_finalize(self, *, reason: str) -> dict: ...

    def shift_active_leases(self, *, actor: str, delta_s: float,
                            scope_kind: str = "challenge", scope_id: str = "",
                            operation_id: str = "", reason: str = "freeze_resume",
                            observed_at: Optional[float] = None) -> dict: ...

    def suspend_active_leases(self, *, actor: str, suspension_id: str,
                              scope_kind: str = "challenge", scope_id: str = "",
                              guard_s: float = 3600.0, reason: str = "freeze",
                              observed_at: Optional[float] = None) -> dict: ...

    def resume_suspended_leases(self, *, actor: str, suspension_id: str,
                                resumed_at: Optional[float] = None,
                                duration_s: Optional[float] = None,
                                reason: str = "thaw") -> dict: ...

    def outstanding_lease_suspensions(self) -> list[str]: ...

    def recover_suspended_leases(self, *, actor: str = "control-recovery",
                                 resumed_at: Optional[float] = None) -> list[dict]: ...

    def compact_graph(self, *, actor: str = "coordinator",
                      trigger: str = "no_progress_time", summary: str = "") -> dict: ...

    def compact_epochs(self) -> list[dict]: ...

    def record_reason_context_compaction(
        self, *, actor: str, cutoff_seq: int, summary: str,
        tokens_before: int,
    ) -> dict: ...

    def revive_resume_intents(self, *, actor: str = "coordinator") -> list[str]: ...

    def prior_intent_count(self) -> int: ...

    def to_review_summary(self) -> str: ...

    def to_review_projection(self, *, fact_seqs: Optional[list[int]] = None,
                             since_seq: int = 0, directive: str = "",
                             limit_facts: int = 60) -> str: ...

    def suppressed_routes(self) -> list[dict]: ...

    def challenged_facts(self) -> list[dict]: ...

    def branches(self) -> list[dict]: ...

    def coordinator_directives(self) -> list[dict]: ...

    def snapshot(self) -> SolveGraph: ...

    def invalidated_flags(self) -> set[str]: ...

    def invalidated_findings(self) -> set[str]: ...

    def coverage_intent_rows(self) -> list[dict]: ...

    def events(self) -> list[dict]: ...
    def events_since(self, after_seq: int, kinds: Optional[list[str]] = None) -> list[dict]: ...

    def semantic_graph_watermark(self) -> int: ...

    def open_coverage_keys(self) -> list[str]: ...

    def active_coverage_keys(self) -> list[str]: ...

    def active_lane_intent_rows(self) -> list[dict]: ...

    def equivalent_step_keys(self) -> set[tuple]: ...

    def equivalent_lane_source_keys(self) -> set[tuple[str, tuple[int, ...]]]: ...

    def to_summary(self, max_evidence: int = 16,
                   max_dead_ends: Optional[int] = None) -> str: ...

    def to_reason_summary(
        self, standing_guidance: Optional[list[str]] = None, *,
        include_lineage: bool = False, compact_summary: str = "",
        compact_cutoff_seq: int = 0,
    ) -> str: ...

    def to_ctf_graph_yaml(self) -> str: ...

    def reason_compaction_cutoff(
        self, keep_recent_tokens: int, *, after_seq: int = 0,
    ) -> int: ...

    def dead_ends_for_context(self, *, intent_id: str = "", coverage_key: str = "",
                              route_hash: str = "", target_epoch: str = "",
                              limit: int = 10**9, epoch_wide: bool = False) -> list[dict]: ...

    def dead_ends_context_block(self, *, intent_id: str = "", coverage_key: str = "",
                                route_hash: str = "", target_epoch: str = "",
                                limit: int = 10**9, title: str = "",
                                epoch_wide: bool = False) -> str: ...

    def to_board_markdown(self) -> str: ...

    def open_goal_texts(self) -> list[str]: ...

    def dispatchable_goal_texts(self) -> list[str]: ...

    def open_route_hashes(self) -> list[str]: ...

    def barren_concluded_goal_texts(self) -> list[str]: ...

    def intent_source_facts(self, intent_id: str) -> list[dict]: ...

    def pin_facts(self, *, actor: str, fact_seqs: list[int],
                  reason: str = "") -> list[int]: ...

    def pinned_fact_seqs(self) -> list[int]: ...

    def fact_pin_context(self, limit: int = 240) -> str: ...

    def try_claim_activity(self, *, worker: str, key: str,
                           lease_s: float = 600.0) -> bool: ...

    def release_activity(self, *, worker: str, key: str) -> None: ...

    def active_activities(self) -> list[dict]: ...

    def canonical_credentials(self) -> list[dict]: ...


# The SQLite DDL lives in graph_schema (code-health G1). Aliased to the historical
# private name so the rest of this module is unchanged.
from muteki.swarm.graph_schema import SCHEMA as _SCHEMA  # noqa: E402

# SQLiteSharedGraph's methods are split into responsibility mixins (code-health G1);
# they are composed back into the class below, so behavior is unchanged.
from muteki.swarm.graph_facts import _FactsMixin  # noqa: E402
from muteki.swarm.graph_routes import _RoutesDirectivesMixin  # noqa: E402
from muteki.swarm.graph_locks import _LanesLocksMixin  # noqa: E402
from muteki.swarm.graph_intents import _IntentsPocsMixin  # noqa: E402
from muteki.swarm.graph_views import _QueriesViewsMixin  # noqa: E402
from muteki.swarm.graph_render import _RenderMixin  # noqa: E402
from muteki.swarm.graph_capabilities import _CapabilitiesMixin  # noqa: E402


class SQLiteSharedGraph(
    _FactsMixin,
    _RoutesDirectivesMixin,
    _LanesLocksMixin,
    _IntentsPocsMixin,
    _CapabilitiesMixin,
    _QueriesViewsMixin,
    _RenderMixin,
):
    """Local direct-SQLite implementation of SharedGraph (D)."""

    CANDIDATE_CAP_PER_SOURCE_ROUTE = 20
    # 刀7: route-LESS candidates (no route_hash) all land in one per-actor catch-all
    # bucket, so it gets a larger ceiling than a single route — but it is still
    # bounded, closing the old "route_hash IS NULL bypasses the cap entirely" leak
    # (run-75375's hottest candidate buckets were all route-less). Generous enough
    # that a productive worker emitting many distinct findings isn't starved.
    CANDIDATE_CAP_PER_SOURCE_NOROUTE = 60
    MAX_LANE_DEFERRALS = 5

    def __init__(self, db_path: str | Path, challenge: Challenge,
                 artifacts: Any = None) -> None:
        self.db_path = str(db_path)
        self.challenge = challenge
        self.artifacts = artifacts  # ArtifactStore, for the P-B gate
        Path(self.db_path).parent.mkdir(parents=True, exist_ok=True)
        # (A) one connection + one-time PRAGMA. check_same_thread=False so the
        # async solver tasks (same loop, possibly different threads) can share it;
        # we guard writes with a lock since sqlite3 module objects aren't
        # thread-safe for concurrent use on one connection.
        self._conn = sqlite3.connect(self.db_path, check_same_thread=False)
        self._lock = threading.Lock()
        cur = self._conn.cursor()
        cur.execute("PRAGMA journal_mode=WAL")
        cur.execute("PRAGMA busy_timeout=5000")      # fixes lost-write: auto-queue
        cur.execute("PRAGMA synchronous=NORMAL")     # safe + fast under WAL
        cur.execute("PRAGMA foreign_keys=ON")
        self._conn.executescript(_SCHEMA)
        self._conn.commit()
        try:
            self._conn.execute("ALTER TABLE intents ADD COLUMN to_fact_seq INTEGER")
            self._conn.commit()
        except sqlite3.OperationalError:
            pass
        # zh gist of the intent goal (deepseek-flash, written back once). Facts
        # carry their gist inside events.payload["summary"] instead (events is
        # append-only, so we patch the JSON in place — see record_fact_summary).
        try:
            self._conn.execute("ALTER TABLE intents ADD COLUMN summary TEXT")
            self._conn.commit()
        except sqlite3.OperationalError:
            pass
        for ddl in (
            "ALTER TABLE intents ADD COLUMN worker_class TEXT NOT NULL DEFAULT 'code'",
            "ALTER TABLE intents ADD COLUMN route_hash TEXT",
            "ALTER TABLE intents ADD COLUMN branch_id TEXT",
            "ALTER TABLE intents ADD COLUMN lane_key TEXT",
            "ALTER TABLE intents ADD COLUMN risk_class TEXT",
            "ALTER TABLE intents ADD COLUMN lane_deferrals INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE intents ADD COLUMN deferred_against_locked_seq INTEGER",
            "ALTER TABLE intents ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE intents ADD COLUMN requested_priority TEXT",
            "ALTER TABLE intents ADD COLUMN priority_reason TEXT",
            "ALTER TABLE intents ADD COLUMN value_claim_json TEXT",
            "ALTER TABLE intents ADD COLUMN novelty_key TEXT",
            "ALTER TABLE intents ADD COLUMN requires_capabilities_json TEXT",
            "ALTER TABLE intents ADD COLUMN required_pocs_json TEXT",
        ):
            try:
                self._conn.execute(ddl)
                self._conn.commit()
            except sqlite3.OperationalError:
                pass
        for ddl in (
            "ALTER TABLE runtime_resources ADD COLUMN cwd TEXT",
            "ALTER TABLE runtime_resources ADD COLUMN env_json TEXT",
            "ALTER TABLE runtime_resources ADD COLUMN cleanup_command TEXT",
        ):
            try:
                self._conn.execute(ddl)
                self._conn.commit()
            except sqlite3.OperationalError:
                pass
        try:
            self._conn.execute("ALTER TABLE lane_locks ADD COLUMN released_worker TEXT")
            self._conn.commit()
        except sqlite3.OperationalError:
            pass
        # A/J: dispatch_state lifecycle columns on intents (idempotent for old DBs).
        for ddl in (
            "ALTER TABLE intents ADD COLUMN dispatch_state TEXT NOT NULL DEFAULT 'active'",
            "ALTER TABLE intents ADD COLUMN close_reason TEXT",
            "ALTER TABLE intents ADD COLUMN stop_reason TEXT",
            "ALTER TABLE intents ADD COLUMN superseded_by_intent_id TEXT",
            "ALTER TABLE intents ADD COLUMN superseded_by_directive_id TEXT",
            "ALTER TABLE intents ADD COLUMN resource_key TEXT",
            "ALTER TABLE intents ADD COLUMN resource_lock_id TEXT",
            "ALTER TABLE intents ADD COLUMN compact_id TEXT",
            "ALTER TABLE intents ADD COLUMN directive_id TEXT",
            "ALTER TABLE intents ADD COLUMN result_detail TEXT",
        ):
            try:
                self._conn.execute(ddl)
                self._conn.commit()
            except sqlite3.OperationalError:
                pass
        if not self._column_exists("intents", "declares_json"):
            # Round-14 declaration seam: the proposer's typed expected effects
            # (JSON: effect_types/expected_artifacts/confidence).
            self._conn.execute("ALTER TABLE intents ADD COLUMN declares_json TEXT")
            self._conn.commit()
        for ddl in (
            "ALTER TABLE intents ADD COLUMN expected_observable TEXT",
            "ALTER TABLE intents ADD COLUMN stop_condition TEXT",
            "ALTER TABLE intents ADD COLUMN coverage_key TEXT",
        ):
            try:
                self._conn.execute(ddl)
                self._conn.commit()
            except sqlite3.OperationalError:
                pass
        for ddl in (
            "ALTER TABLE branches ADD COLUMN source_intent TEXT",
            "ALTER TABLE branches ADD COLUMN from_facts_json TEXT",
            "ALTER TABLE branches ADD COLUMN expected_observable TEXT",
            "ALTER TABLE branches ADD COLUMN stop_condition TEXT",
            "ALTER TABLE branches ADD COLUMN coverage_key TEXT",
            "ALTER TABLE branches ADD COLUMN route_hash TEXT",
            "ALTER TABLE branches ADD COLUMN lane_key TEXT",
            "ALTER TABLE branches ADD COLUMN risk_class TEXT",
            "ALTER TABLE branches ADD COLUMN resource_key TEXT",
        ):
            try:
                self._conn.execute(ddl)
                self._conn.commit()
            except sqlite3.OperationalError:
                pass
        try:
            self._conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_intents_dispatch "
                "ON intents(challenge_id, dispatch_state, status, priority, created_seq)"
            )
            self._conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_intents_novelty "
                "ON intents(challenge_id, novelty_key, dispatch_state, status)"
            )
            self._conn.commit()
        except sqlite3.OperationalError:
            pass
        # Old planners could label arbitrary queued work high.  On first open,
        # remove that inherited scheduling authority unless a structured claim
        # exists; operator-directed rows retain their explicit priority.
        self._conn.execute(
            "UPDATE intents SET requested_priority='high', priority=0, "
            "priority_reason='legacy_high_missing_value_claim' "
            "WHERE challenge_id=? AND priority>=50 "
            "AND COALESCE(value_claim_json,'')='' "
            "AND COALESCE(directive_id,'')='' "
            "AND worker_class IN ('code','shell_agent','review')",
            (self.challenge.id,),
        )
        self._conn.commit()
        self._last_observation_seq = 0
        self.migrate_legacy_candidates()

    def _table_exists(self, name: str) -> bool:
        with self._lock:
            row = self._conn.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                (name,),
            ).fetchone()
        return row is not None

    def _column_exists(self, table: str, column: str) -> bool:
        with self._lock:
            rows = self._conn.execute(f"PRAGMA table_info({table})").fetchall()
        return any(str(row[1]) == column for row in rows)

    # ── classmethod ctor ────────────────────────────────────────────────
    @classmethod
    def open(cls, *, db_path: str | Path, challenge: Challenge,
             artifacts: Any = None) -> "SQLiteSharedGraph":
        return cls(db_path, challenge, artifacts)

    @classmethod
    def open_readonly(cls, *, db_path: str | Path, challenge: Challenge) -> "SQLiteSharedGraph":
        """TRUE read-only open for observer paths (btw side-query, replay QA).

        Unlike `open()`, this NEVER: creates the parent dir, sets WAL, runs the
        schema/migration script, or commits. It opens the existing DB file in
        SQLite `mode=ro` + `query_only=ON` so even a buggy caller cannot write.

        Assumes the DB was already initialised by a prior `open()` (true for any
        run that has reached the coordination phase). If the file is missing or
        the schema is absent/stale, raises sqlite3.OperationalError — callers
        must catch and degrade to a minimal context, NOT attempt migration.

        WAL sidecar note: `mode=ro` reads an active WAL DB fine when `-wal`/`-shm`
        exist; if they are missing on a live run the read may fail or miss
        un-checkpointed rows. Callers should treat any OperationalError as
        "graph temporarily unreadable" and degrade, never as a reason to open RW.
        """
        p = str(db_path)
        uri = f"file:{p}?mode=ro"
        inst = cls.__new__(cls)
        inst.db_path = p
        inst.challenge = challenge
        inst.artifacts = None
        inst._lock = threading.Lock()
        # URI mode=ro: open the file read-only at the SQLite VFS layer.
        inst._conn = sqlite3.connect(uri, uri=True, check_same_thread=False)
        cur = inst._conn.cursor()
        # query_only blocks ANY write DDL/DML even if a caller tried; belt+suspenders
        # on top of mode=ro. These PRAGMAs are read-only-safe.
        cur.execute("PRAGMA query_only=ON")
        cur.execute("PRAGMA busy_timeout=3000")
        return inst

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    # ── append (C: INSERT only) ─────────────────────────────────────────
    def _append_locked(self, kind: str, actor: str, payload: dict, *,
                       artifact_id: Optional[str] = None, verified: bool = False,
                       confidence: float = 1.0,
                       dedupe_key: Optional[str] = None) -> int:
        """INSERT one event with ``self._lock`` already held and NO commit — the
        caller owns the transaction boundary (single-writer paths commit right
        away via ``_append``; ``commit_worker_result`` batches many writes into
        one commit). Same IntegrityError→-1 dedupe semantics as ``_append`` but
        without the rollback, so a dedupe collision inside a larger transaction
        does not discard the caller's earlier writes."""
        try:
            cur = self._conn.execute(
                "INSERT INTO events "
                "(ts, challenge_id, actor, kind, payload, artifact_id, "
                " verified, confidence, dedupe_key) "
                "VALUES (?,?,?,?,?,?,?,?,?)",
                (time.time(), self.challenge.id, actor, kind,
                 json.dumps(payload, default=str), artifact_id,
                 int(verified), float(confidence), dedupe_key),
            )
            return int(cur.lastrowid or 0)
        except sqlite3.IntegrityError:
            return -1

    def _append(self, kind: str, actor: str, payload: dict, *,
                artifact_id: Optional[str] = None, verified: bool = False,
                confidence: float = 1.0, dedupe_key: Optional[str] = None) -> int:
        with self._lock:
            seq = self._append_locked(
                kind, actor, payload, artifact_id=artifact_id,
                verified=verified, confidence=confidence, dedupe_key=dedupe_key)
            if seq < 0:
                # dedupe_key collision → same event already appended; no-op.
                self._conn.rollback()
            else:
                self._conn.commit()
            return seq

    def record_pentest_goal_completion(
        self, *, fact_seqs: list[int], reason: str,
    ) -> int:
        """Durably commit a Decide verdict only when its cited Facts are grounded."""
        contract = getattr(self.challenge, "pentest_contract", None)
        if contract is None:
            return -1
        from muteki.pentest.judgement import GOAL_COMPLETED, goal_evidence_valid
        cited = list(dict.fromkeys(int(seq) for seq in fact_seqs))
        if not goal_evidence_valid(self.events(), contract, cited):
            return -1
        return self._append(
            GOAL_COMPLETED, "coordinator",
            {"fact_seqs": cited, "reason": str(reason or "")},
            verified=True,
            dedupe_key=f"pentest-goal::{self.challenge.id}",
        )

    def record_pentest_report(
        self, *, payload: dict, error: bool = False,
    ) -> int:
        from muteki.pentest.judgement import REPORT_FAILED, REPORT_GENERATED
        kind = REPORT_FAILED if error else REPORT_GENERATED
        return self._append(
            kind, "coordinator", dict(payload),
            verified=not error,
        )


# ── GRAPH-01: GraphService adapter 接线（新增，不改变既有行为） ──────────────
def open_graph_service(*, db_path: str | Path, challenge: Challenge,
                       artifacts: Any = None) -> Any:
    """以 GraphService 接口（ctf.shared_graph.v1）打开本 Challenge 的 SharedGraph。

    薄 Adapter：append/snapshot/claim/lease 映射到 SQLiteSharedGraph 现有 API，
    历史数据库无迁移即可继续读写；现有 Protocol、SQLiteSharedGraph 与
    blackboard 行为完全不变。延迟导入避免 graphs → swarm 的循环依赖。
    """
    from muteki.graphs.ctf_shared_graph import CtfSharedGraphService
    return CtfSharedGraphService.open(
        db_path=db_path, challenge=challenge, artifacts=artifacts)
