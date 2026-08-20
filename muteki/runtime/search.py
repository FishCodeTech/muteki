"""Minimal typed search kernel on top of transactional SearchAdmission."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

from muteki.runtime.admission import AdmissionRequest, SearchAdmission
from muteki.runtime.contracts import AttemptPermit
from muteki.runtime.hypothesis import (
    H5RecommendationRequestV1,
    HypothesisSelector,
    SelectionPlan,
)
from muteki.runtime.progress import ProgressProjection
H5_RECOMMENDATION_GATE_VERSION = "muteki.runtime-h5-recommendation-gate.v2"


class FailureKind(str, Enum):
    HYPOTHESIS_REFUTED = "hypothesis_refuted"
    TOOL_FAILURE = "tool_failure"
    INFRA_FAILURE = "infra_failure"
    POLICY_BLOCKED = "policy_blocked"
    UNKNOWN = "unknown"


class RecoveryAction(str, Enum):
    CLOSE_BRANCH = "close_branch"
    RETRY_TOOL = "retry_tool"
    SWITCH_ENGINE = "switch_engine"
    PAUSE_INFRA = "pause_infra"
    REQUEST_INFORMATION = "request_information"
    HOLD_RECONCILIATION = "hold_reconciliation"


@dataclass(frozen=True, slots=True)
class LoopContract:
    max_attempts: int
    max_barren_attempts: int
    max_wall_ms: int

    def child(self, *, max_attempts: int, max_barren_attempts: int,
              max_wall_ms: int) -> "LoopContract":
        if (max_attempts > self.max_attempts
                or max_barren_attempts > self.max_barren_attempts
                or max_wall_ms > self.max_wall_ms):
            raise ValueError("child loop contract may only tighten its parent")
        return LoopContract(max_attempts, max_barren_attempts, max_wall_ms)


@dataclass(frozen=True, slots=True)
class ExecutionContract:
    task_shape: str
    success_predicate: str
    progress_predicate: str
    failure_semantics: str
    recovery_policy: str
    loop: LoopContract


@dataclass(frozen=True, slots=True)
class H5RecommendationGateV1:
    """Explicit, default-off gate for pure H5 recommendation generation.

    Enabling this gate only permits deterministic in-memory ranking.  It cannot
    admit an attempt, reserve budget, launch a worker, write progress, change an
    effect state, close a branch, or influence the hard acceptance gate.
    """

    enabled: bool = False
    version: str = H5_RECOMMENDATION_GATE_VERSION

    def __post_init__(self) -> None:
        if type(self.enabled) is not bool:
            raise TypeError("enabled must be an exact boolean")
        if type(self.version) is not str or self.version != H5_RECOMMENDATION_GATE_VERSION:
            raise ValueError("unsupported H5 recommendation gate version")


def recovery_for(failure: FailureKind) -> RecoveryAction:
    if failure is FailureKind.HYPOTHESIS_REFUTED:
        return RecoveryAction.CLOSE_BRANCH
    if failure is FailureKind.TOOL_FAILURE:
        return RecoveryAction.RETRY_TOOL
    if failure is FailureKind.INFRA_FAILURE:
        return RecoveryAction.PAUSE_INFRA
    if failure is FailureKind.POLICY_BLOCKED:
        return RecoveryAction.REQUEST_INFORMATION
    # UNKNOWN is an epistemic/effect hold, never an implicit extra sample.
    # Engine switching requires a later explicit, independently admitted action.
    return RecoveryAction.HOLD_RECONCILIATION


class SearchKernel:
    def __init__(self, *, admission: SearchAdmission,
                 progress: ProgressProjection | None = None,
                 h5_recommendation_gate: H5RecommendationGateV1 | None = None) -> None:
        self.admission = admission
        self.progress = progress or ProgressProjection()
        if h5_recommendation_gate is None:
            self._h5_recommendation_gate = H5RecommendationGateV1()
        elif type(h5_recommendation_gate) is H5RecommendationGateV1:
            self._h5_recommendation_gate = h5_recommendation_gate
        else:
            raise TypeError(
                "h5_recommendation_gate must be H5RecommendationGateV1 or None"
            )

    def admit(self, request: AdmissionRequest, *, occurred_at_ns: int) -> AttemptPermit:
        return self.admission.admit(request, occurred_at_ns=occurred_at_ns)

    def recommend_h5(
        self,
        request: H5RecommendationRequestV1,
    ) -> SelectionPlan | None:
        """Return a recommendation-only H5 plan when the explicit gate is on.

        The disabled path intentionally returns before inspecting ``request`` so
        ordinary S4 callers retain their baseline behavior.  The enabled path is
        a pure call into ``HypothesisSelector``: it does not call admission, touch
        this kernel's progress projection, record a receipt, or derive a retry.
        Until V7 supplies receipt-resolved independent checker authority, this
        runtime seam refuses proof/tombstone suppression inputs. Their opaque
        digest fields are valid research fixtures, not canonical evidence a
        runtime recommendation may trust.
        """

        if not self._h5_recommendation_gate.enabled:
            return None
        if type(request) is not H5RecommendationRequestV1:
            raise TypeError("request must be H5RecommendationRequestV1")
        if request.equivalence_proofs or request.tombstones:
            raise ValueError(
                "H5 runtime recommendation refuses unverified suppression inputs"
            )
        return HypothesisSelector.recommend(request)

    def failure_action(self, *, branch_id: str,
                       failure: FailureKind) -> RecoveryAction:
        # Infrastructure/tool/unknown failures never refute the hypothesis.
        return recovery_for(failure)
