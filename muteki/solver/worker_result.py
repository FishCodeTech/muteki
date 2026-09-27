"""Structured worker result: published incrementally, concluded atomically.

Fact claims pass through the provenance gate and enter SharedGraph immediately;
unbound claims remain candidates.  The WorkerResult retains the same claims for
its checkpoint/end-of-life receipt, commits bounded dead ends and PoCs, and
concludes the intent LAST.  Graph deduplication makes the repeated fact claims
idempotent.  The crash path commits the same shape with
``handoff_missing=True`` so the evidence trail survives a worker that died
before handing off.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional


@dataclass(frozen=True, slots=True)
class ObservationClaim:
    """One observed fact. ``provenance`` is the host-checkable tool-event binding
    (``ToolEvidenceRecord.provenance``); the graph's evidence gate re-validates it
    at commit, so a missing/invalid binding lands as a candidate."""

    text: str
    claim_verified: bool = False
    provenance: Optional[dict] = None
    subject: str = ""
    predicate: str = ""
    object_value: Any = None
    scope: str = ""
    canonical_key: str = ""
    witness: str = ""


@dataclass(frozen=True, slots=True)
class DeadEndClaim:
    """One bounded test the worker ruled out; bound to its intent+epoch."""

    reason: str
    tested_scope: str = ""
    observed_result: str = ""
    route_hash: str = ""
    coverage_key: str = ""


@dataclass(frozen=True, slots=True)
class PocClaim:
    """One PoC already materialized to the shared artifact store (CAS write stays
    immediate); only the graph row is deferred to the commit."""

    poc_id: str
    path: str
    entry_command: str
    status: str = "available"
    note: str = ""
    artifact_id: Optional[str] = None
    name: str = ""


@dataclass(frozen=True, slots=True)
class NeedInputClaim:
    """An operator input the worker raised live (HITL); recorded here only so the
    commit's audit event can summarize it — it is NOT re-written to the graph."""

    need: str
    need_kind: str = ""


@dataclass(frozen=True, slots=True)
class WorkerResult:
    """End-of-life rollup of everything one worker established this run."""

    status: str
    result_detail: str = ""
    produced_new_info: bool = False
    stop_condition_result: str = ""
    handoff_missing: bool = False
    observations: list[ObservationClaim] = field(default_factory=list)
    dead_ends: list[DeadEndClaim] = field(default_factory=list)
    pocs: list[PocClaim] = field(default_factory=list)
    artifact_ids: list[str] = field(default_factory=list)
    need_input: Optional[NeedInputClaim] = None

    def to_commit_kwargs(self, *, actor: str, worker_id: str, intent_id: str,
                         target_epoch: str, run_id: str) -> dict[str, Any]:
        """Map onto ``SharedGraph.commit_worker_result`` kwargs (``conclude`` is
        left to the call site — a worker without an intent must not conclude)."""
        return {
            "actor": actor,
            "worker_id": worker_id,
            "intent_id": intent_id,
            "target_epoch": target_epoch,
            "run_id": run_id,
            "status": self.status,
            "result_detail": self.result_detail,
            "produced_new_info": self.produced_new_info,
            "stop_condition_result": self.stop_condition_result,
            "handoff_missing": self.handoff_missing,
            "observations": [
                {
                    "text": o.text,
                    "claim_verified": o.claim_verified,
                    "provenance": (dict(o.provenance)
                                   if o.provenance is not None else None),
                    "subject": o.subject,
                    "predicate": o.predicate,
                    "object": o.object_value,
                    "scope": o.scope,
                    "canonical_key": o.canonical_key,
                    "witness": o.witness,
                }
                for o in self.observations
            ],
            "dead_ends": [
                {
                    "reason": d.reason,
                    "tested_scope": d.tested_scope,
                    "observed_result": d.observed_result,
                    "route_hash": d.route_hash,
                    "coverage_key": d.coverage_key,
                }
                for d in self.dead_ends
            ],
            "pocs": [
                {
                    "poc_id": p.poc_id,
                    "path": p.path,
                    "entry_command": p.entry_command,
                    "status": p.status,
                    "note": p.note,
                    "artifact_id": p.artifact_id,
                    "name": p.name,
                }
                for p in self.pocs
            ],
            "artifacts": [str(a) for a in self.artifact_ids if str(a)],
            "need_input": (
                {"need": self.need_input.need,
                 "need_kind": self.need_input.need_kind}
                if self.need_input is not None else None),
        }

    @classmethod
    def salvage(cls, *, status: str, result_detail: str = "",
                produced_new_info: bool = False,
                stop_condition_result: str = "",
                observations: "Optional[list[ObservationClaim]]" = None,
                dead_ends: "Optional[list[DeadEndClaim]]" = None,
                pocs: "Optional[list[PocClaim]]" = None,
                artifact_ids: "Optional[list[str]]" = None,
                need_input: Optional[NeedInputClaim] = None) -> "WorkerResult":
        """Crash-path result: the worker died before handing off, so the commit
        carries whatever artifact refs were passed in under handoff_missing."""
        return cls(
            status=status,
            result_detail=result_detail,
            produced_new_info=produced_new_info,
            stop_condition_result=stop_condition_result,
            handoff_missing=True,
            observations=list(observations or []),
            dead_ends=list(dead_ends or []),
            pocs=list(pocs or []),
            artifact_ids=[str(a) for a in (artifact_ids or []) if str(a)],
            need_input=need_input,
        )
