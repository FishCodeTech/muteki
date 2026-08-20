"""Neutral production-local predicted-effects schema (R29).

Serializable shape for optional intent annotations.  Intentionally lives
under ``muteki.models`` (not ``muteki.research``) so production code may
emit/persist the blob without importing research, and research offline
replay may consume the same ordinary JSON without a production→research
dependency.

Hard constraints:
* no ``muteki.research`` imports
* no dispatch / claim-order mutation
* emit-only consumers write artifacts; they do not rank
"""

from __future__ import annotations

from dataclasses import dataclass
import json
from typing import Any, Final, Mapping, Sequence


SCHEMA: Final = "muteki.models.predicted-effects.v1"
PRODUCTION_ENABLED: Final = True  # schema may exist in production artifacts
DISPATCH_AUTHORITY: Final = False
PROMOTION_AUTHORITY: Final = False
CAPABILITY_AUTHORITY: Final = False
RELEASE_AUTHORITY: Final = False
GATE_AUTHORITY: Final = False
AUTHORITY_EFFECT_NONE: Final = "NONE"


@dataclass(frozen=True, slots=True)
class PredictedEffectsV1:
    """Opaque predicted-effect annotation attached to an intent."""

    predicted_adds: tuple[str, ...] = ()
    predicted_removes: tuple[str, ...] = ()
    sources: tuple[str, ...] = ()
    mode: str = ""
    intent_id: str = ""

    def as_dict(self) -> dict[str, object]:
        return {
            "schema": SCHEMA,
            "intent_id": self.intent_id,
            "mode": self.mode,
            "predicted_adds": list(self.predicted_adds),
            "predicted_removes": list(self.predicted_removes),
            "sources": list(self.sources),
            "dispatch_authority": DISPATCH_AUTHORITY,
            "authority_effect": AUTHORITY_EFFECT_NONE,
        }


def serialize_predicted_effects_v1(effects: PredictedEffectsV1) -> dict[str, object]:
    """Return a JSON-ready dict (emit-only; no ranking side effects)."""

    return effects.as_dict()


def serialize_predicted_effects_json_v1(effects: PredictedEffectsV1) -> str:
    return json.dumps(
        serialize_predicted_effects_v1(effects),
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    )


def parse_predicted_effects_v1(raw: Any) -> PredictedEffectsV1 | None:
    """Tolerant parse of a predicted-effects payload (dict or JSON string)."""

    if isinstance(raw, str) and raw.strip():
        try:
            raw = json.loads(raw)
        except (json.JSONDecodeError, TypeError):
            return None
    if not isinstance(raw, Mapping):
        return None
    adds = _string_tuple(raw.get("predicted_adds"))
    removes = _string_tuple(raw.get("predicted_removes"))
    sources = _string_tuple(raw.get("sources"))
    if not adds and not removes and not sources:
        # Empty annotation is valid but useless; treat as absent.
        if not str(raw.get("intent_id") or "").strip() and not str(
            raw.get("mode") or ""
        ).strip():
            return None
    return PredictedEffectsV1(
        predicted_adds=adds,
        predicted_removes=removes,
        sources=sources,
        mode=str(raw.get("mode") or "").strip(),
        intent_id=str(raw.get("intent_id") or "").strip(),
    )


def predicted_effects_from_parts_v1(
    *,
    intent_id: str = "",
    mode: str = "",
    predicted_adds: Sequence[str] = (),
    predicted_removes: Sequence[str] = (),
    sources: Sequence[str] = (),
) -> PredictedEffectsV1:
    return PredictedEffectsV1(
        intent_id=str(intent_id or ""),
        mode=str(mode or ""),
        predicted_adds=_string_tuple(predicted_adds),
        predicted_removes=_string_tuple(predicted_removes),
        sources=_string_tuple(sources),
    )


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
    "AUTHORITY_EFFECT_NONE",
    "CAPABILITY_AUTHORITY",
    "DISPATCH_AUTHORITY",
    "GATE_AUTHORITY",
    "PRODUCTION_ENABLED",
    "PROMOTION_AUTHORITY",
    "PredictedEffectsV1",
    "RELEASE_AUTHORITY",
    "SCHEMA",
    "parse_predicted_effects_v1",
    "predicted_effects_from_parts_v1",
    "serialize_predicted_effects_json_v1",
    "serialize_predicted_effects_v1",
]
