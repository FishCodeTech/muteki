"""Protocol 1 compatibility implementation of the narrow search-state seam."""

from __future__ import annotations

import time
from collections.abc import Mapping, Sequence
from typing import Any

from muteki.runtime.contracts import AttemptIdentity, AttemptPermit


class Protocol2AdmissionUnavailable(RuntimeError):
    pass


class V1SearchStatePort:
    """Freeze the legacy graph behind use-case methods while v1 runs drain."""

    def __init__(self, *, run_id: str, graph: Any) -> None:
        if not run_id:
            raise ValueError("run_id is required")
        self._run_id = run_id
        self._graph = graph

    def _check_run(self, run_id: str) -> None:
        if run_id != self._run_id:
            raise ValueError("search-state run mismatch")

    def query_legacy_candidates(self, *, run_id: str) -> Sequence[Mapping[str, Any]]:
        self._check_run(run_id)
        return self._graph.query_legacy_candidates(now=time.time())

    def apply_legacy_lane_inferences(
        self, *, run_id: str, inferences: list[tuple[str, str, str]],
    ) -> None:
        self._check_run(run_id)
        self._graph.apply_legacy_lane_inferences(inferences=inferences)

    def claim_legacy_occurrence(self, *, run_id: str, occurrence_id: str,
                                worker_id: str, lease_until_ns: int) -> bool:
        self._check_run(run_id)
        remaining_s = max(0.0, lease_until_ns / 1_000_000_000 - time.time())
        return bool(self._graph.claim_intent(
            worker=worker_id, intent_id=occurrence_id, lease_s=remaining_s))

    def admit_attempt(self, *, attempt: AttemptIdentity,
                      requested_budget: Mapping[str, int],
                      conflict_keys: Sequence[str]) -> AttemptPermit:
        raise Protocol2AdmissionUnavailable(
            "Protocol 1 compatibility state cannot mint Protocol 2 permits")
