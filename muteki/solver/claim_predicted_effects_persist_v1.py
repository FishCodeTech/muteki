"""Production-local claim-time predicted_effects persistence (R31/R32).

Emit-only JSONL store for ``PredictedEffectsV1`` (+ optional TQ/declaration
sidecar fields).  Default-off.  Does **not** import research, does **not**
mutate claim/dispatch order, and is intentionally unwired into production
coordinator / dispatch loops — callers opt in explicitly.

R32: research/shadow claim-site replay may opt in to append here (still
default-off; production dispatch unchanged).  Research offline replay reads
the same ordinary JSONL without a production→research dependency.
"""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
from typing import Final, Iterable, Mapping, Sequence

from muteki.models.predicted_effects_v1 import (
    AUTHORITY_EFFECT_NONE,
    DISPATCH_AUTHORITY as MODEL_DISPATCH_AUTHORITY,
    PredictedEffectsV1,
    parse_predicted_effects_v1,
    predicted_effects_from_parts_v1,
    serialize_predicted_effects_v1,
)


SCHEMA: Final = "muteki.solver.claim-predicted-effects-persist.v1"
RECORD_SCHEMA: Final = f"{SCHEMA}.record"
EXPERIMENT_ENABLED_DEFAULT: Final = False
PERSIST_ENABLED_DEFAULT: Final = False
PRODUCTION_ENABLED: Final = False
DISPATCH_AUTHORITY: Final = False
PROMOTION_AUTHORITY: Final = False
CAPABILITY_AUTHORITY: Final = False
RELEASE_AUTHORITY: Final = False
GATE_AUTHORITY: Final = False
# Research/shadow may opt in; production coordinator remains unwired.
WIRED_INTO_PRODUCTION_COORDINATOR: Final = False
WIRED_INTO_DISPATCH: Final = False
RESEARCH_SHADOW_OPT_IN_AVAILABLE: Final = True


class ClaimPredictedEffectsPersistDisabled(RuntimeError):
    """Raised unless claim-time persist is explicitly enabled."""


@dataclass(frozen=True, slots=True)
class ClaimPredictedEffectsRecordV1:
    """One claim-time (or propose-time) predicted-effects artifact."""

    run_id: str
    claim_seq: int
    intent_id: str
    predicted_effects: PredictedEffectsV1
    declares_effect_types: tuple[str, ...] = ()
    expected_artifacts: tuple[str, ...] = ()
    captured_at: str = "claim"
    source: str = "emit_only"

    def as_dict(self) -> dict[str, object]:
        body = serialize_predicted_effects_v1(self.predicted_effects)
        return {
            "schema": RECORD_SCHEMA,
            "run_id": self.run_id,
            "claim_seq": int(self.claim_seq),
            "intent_id": self.intent_id,
            "captured_at": self.captured_at,
            "source": self.source,
            "declares_effect_types": list(self.declares_effect_types),
            "expected_artifacts": list(self.expected_artifacts),
            "predicted_effects": body,
            "dispatch_authority": DISPATCH_AUTHORITY,
            "authority_effect": AUTHORITY_EFFECT_NONE,
        }


def persist_hook_status_v1() -> dict[str, object]:
    return {
        "schema": f"{SCHEMA}.status",
        "wired_into_production_coordinator": WIRED_INTO_PRODUCTION_COORDINATOR,
        "wired_into_dispatch": WIRED_INTO_DISPATCH,
        "research_shadow_opt_in_available": RESEARCH_SHADOW_OPT_IN_AVAILABLE,
        "persist_enabled_default": PERSIST_ENABLED_DEFAULT,
        "production_enabled": PRODUCTION_ENABLED,
        "dispatch_authority": DISPATCH_AUTHORITY,
        "model_dispatch_authority": MODEL_DISPATCH_AUTHORITY,
        "authority_effect": AUTHORITY_EFFECT_NONE,
        "allowed": (
            "append PredictedEffectsV1 JSONL at claim/propose when caller opts in",
            "research/shadow claim-site replay may opt in (default off)",
            "read back ordinary JSONL for research offline replay",
        ),
        "forbidden": (
            "import muteki.research from this module",
            "mutate claim/dispatch order from persisted predicted_effects",
            "promote predicted markers into verified evidence",
            "enable production dispatch or promotion from this hook",
        ),
    }


def build_claim_record_v1(
    *,
    run_id: str,
    claim_seq: int,
    intent_id: str,
    predicted_adds: Sequence[str] = (),
    predicted_removes: Sequence[str] = (),
    sources: Sequence[str] = (),
    mode: str = "claim_time",
    declares_effect_types: Sequence[str] = (),
    expected_artifacts: Sequence[str] = (),
    captured_at: str = "claim",
    source: str = "emit_only",
    enabled: bool = PERSIST_ENABLED_DEFAULT,
) -> ClaimPredictedEffectsRecordV1:
    if enabled is not True:
        raise ClaimPredictedEffectsPersistDisabled(
            "claim-time predicted_effects persist defaults off"
        )
    effects = predicted_effects_from_parts_v1(
        intent_id=intent_id,
        mode=mode,
        predicted_adds=predicted_adds,
        predicted_removes=predicted_removes,
        sources=sources,
    )
    return ClaimPredictedEffectsRecordV1(
        run_id=str(run_id or ""),
        claim_seq=int(claim_seq),
        intent_id=str(intent_id),
        predicted_effects=effects,
        declares_effect_types=_string_tuple(declares_effect_types),
        expected_artifacts=_string_tuple(expected_artifacts),
        captured_at=str(captured_at or "claim"),
        source=str(source or "emit_only"),
    )


def append_claim_record_v1(
    path: str | Path,
    record: ClaimPredictedEffectsRecordV1,
    *,
    enabled: bool = PERSIST_ENABLED_DEFAULT,
) -> Path:
    if enabled is not True:
        raise ClaimPredictedEffectsPersistDisabled(
            "claim-time predicted_effects persist defaults off"
        )
    destination = Path(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(record.as_dict(), sort_keys=True, ensure_ascii=False)
    with destination.open("a", encoding="utf-8") as handle:
        handle.write(line + "\n")
    return destination


def load_claim_records_v1(
    path: str | Path,
) -> tuple[ClaimPredictedEffectsRecordV1, ...]:
    """Tolerant JSONL load; skips blank / malformed lines."""

    source = Path(path)
    if not source.is_file():
        return ()
    out: list[ClaimPredictedEffectsRecordV1] = []
    for raw_line in source.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except (json.JSONDecodeError, TypeError):
            continue
        record = parse_claim_record_v1(payload)
        if record is not None:
            out.append(record)
    return tuple(out)


def parse_claim_record_v1(raw: object) -> ClaimPredictedEffectsRecordV1 | None:
    if not isinstance(raw, Mapping):
        return None
    effects_raw = raw.get("predicted_effects")
    if effects_raw is None:
        effects_raw = raw
    effects = parse_predicted_effects_v1(effects_raw)
    if effects is None:
        return None
    intent_id = str(raw.get("intent_id") or effects.intent_id or "").strip()
    if not intent_id:
        return None
    try:
        claim_seq = int(raw.get("claim_seq") or 0)
    except (TypeError, ValueError):
        claim_seq = 0
    return ClaimPredictedEffectsRecordV1(
        run_id=str(raw.get("run_id") or ""),
        claim_seq=claim_seq,
        intent_id=intent_id,
        predicted_effects=effects,
        declares_effect_types=_string_tuple(raw.get("declares_effect_types")),
        expected_artifacts=_string_tuple(raw.get("expected_artifacts")),
        captured_at=str(raw.get("captured_at") or "claim"),
        source=str(raw.get("source") or "emit_only"),
    )


def index_claim_records_by_intent_v1(
    records: Iterable[ClaimPredictedEffectsRecordV1],
) -> dict[str, ClaimPredictedEffectsRecordV1]:
    """Last write wins per intent_id (claim-time overwrite semantics)."""

    out: dict[str, ClaimPredictedEffectsRecordV1] = {}
    for record in records:
        out[record.intent_id] = record
    return out


def index_claim_records_by_seq_intent_v1(
    records: Iterable[ClaimPredictedEffectsRecordV1],
) -> dict[tuple[int, str], ClaimPredictedEffectsRecordV1]:
    return {(record.claim_seq, record.intent_id): record for record in records}


def _string_tuple(value: object) -> tuple[str, ...]:
    if value is None:
        return ()
    if isinstance(value, str):
        token = value.strip()
        return (token,) if token else ()
    if not isinstance(value, Sequence):
        return ()
    out: list[str] = []
    for item in value:
        token = str(item).strip()
        if token and token not in out:
            out.append(token)
    return tuple(out)


__all__ = [
    "CAPABILITY_AUTHORITY",
    "ClaimPredictedEffectsPersistDisabled",
    "ClaimPredictedEffectsRecordV1",
    "DISPATCH_AUTHORITY",
    "EXPERIMENT_ENABLED_DEFAULT",
    "GATE_AUTHORITY",
    "PERSIST_ENABLED_DEFAULT",
    "PRODUCTION_ENABLED",
    "PROMOTION_AUTHORITY",
    "RECORD_SCHEMA",
    "RELEASE_AUTHORITY",
    "RESEARCH_SHADOW_OPT_IN_AVAILABLE",
    "SCHEMA",
    "WIRED_INTO_DISPATCH",
    "WIRED_INTO_PRODUCTION_COORDINATOR",
    "append_claim_record_v1",
    "build_claim_record_v1",
    "index_claim_records_by_intent_v1",
    "index_claim_records_by_seq_intent_v1",
    "load_claim_records_v1",
    "parse_claim_record_v1",
    "persist_hook_status_v1",
]
