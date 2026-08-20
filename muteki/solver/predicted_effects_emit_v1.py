"""Production-local emit-only hook for predicted_effects (R29).

Default-off.  Serializes a neutral ``PredictedEffectsV1`` blob that may be
persisted on intent rows / event payloads.  Does **not** import research,
does **not** call the champion ranker, and does **not** mutate claim or
dispatch order.

Wiring status: intentionally unwired into coordinator / dispatch loops.
Callers that want to persist must opt in explicitly; isolation tests
forbid production→research imports and keep dispatch authority false.
"""

from __future__ import annotations

from typing import Final, Sequence

from muteki.models.predicted_effects_v1 import (
    AUTHORITY_EFFECT_NONE,
    DISPATCH_AUTHORITY as MODEL_DISPATCH_AUTHORITY,
    PredictedEffectsV1,
    predicted_effects_from_parts_v1,
    serialize_predicted_effects_json_v1,
    serialize_predicted_effects_v1,
)


SCHEMA: Final = "muteki.solver.predicted-effects-emit.v1"
EXPERIMENT_ENABLED_DEFAULT: Final = False
EMIT_ENABLED_DEFAULT: Final = False
PRODUCTION_ENABLED: Final = False  # hook defaults closed even though schema exists
DISPATCH_AUTHORITY: Final = False
PROMOTION_AUTHORITY: Final = False
CAPABILITY_AUTHORITY: Final = False
RELEASE_AUTHORITY: Final = False
GATE_AUTHORITY: Final = False


class PredictedEffectsEmitDisabled(RuntimeError):
    """Raised unless emit is explicitly enabled."""


def emit_hook_status_v1() -> dict[str, object]:
    return {
        "schema": f"{SCHEMA}.status",
        "wired_into_production_coordinator": False,
        "wired_into_dispatch": False,
        "emit_enabled_default": EMIT_ENABLED_DEFAULT,
        "production_enabled": PRODUCTION_ENABLED,
        "dispatch_authority": DISPATCH_AUTHORITY,
        "model_dispatch_authority": MODEL_DISPATCH_AUTHORITY,
        "authority_effect": AUTHORITY_EFFECT_NONE,
        "allowed": (
            "serialize PredictedEffectsV1 to intents/events JSON when caller opts in",
        ),
        "forbidden": (
            "import muteki.research from this module",
            "mutate claim/dispatch order from predicted_effects",
            "promote predicted markers into verified evidence",
        ),
    }


def build_predicted_effects_payload_v1(
    *,
    intent_id: str,
    predicted_adds: Sequence[str] = (),
    predicted_removes: Sequence[str] = (),
    sources: Sequence[str] = (),
    mode: str = "emit_only",
    enabled: bool = EMIT_ENABLED_DEFAULT,
) -> dict[str, object]:
    """Build a serializable predicted-effects dict (emit-only)."""

    if enabled is not True:
        raise PredictedEffectsEmitDisabled("predicted_effects emit defaults off")
    effects = predicted_effects_from_parts_v1(
        intent_id=intent_id,
        mode=mode,
        predicted_adds=predicted_adds,
        predicted_removes=predicted_removes,
        sources=sources,
    )
    payload = serialize_predicted_effects_v1(effects)
    payload["emit_schema"] = SCHEMA
    payload["dispatch_authority"] = DISPATCH_AUTHORITY
    return payload


def build_predicted_effects_json_v1(
    *,
    intent_id: str,
    predicted_adds: Sequence[str] = (),
    predicted_removes: Sequence[str] = (),
    sources: Sequence[str] = (),
    mode: str = "emit_only",
    enabled: bool = EMIT_ENABLED_DEFAULT,
) -> str:
    if enabled is not True:
        raise PredictedEffectsEmitDisabled("predicted_effects emit defaults off")
    effects = predicted_effects_from_parts_v1(
        intent_id=intent_id,
        mode=mode,
        predicted_adds=predicted_adds,
        predicted_removes=predicted_removes,
        sources=sources,
    )
    return serialize_predicted_effects_json_v1(effects)


__all__ = [
    "CAPABILITY_AUTHORITY",
    "DISPATCH_AUTHORITY",
    "EMIT_ENABLED_DEFAULT",
    "EXPERIMENT_ENABLED_DEFAULT",
    "GATE_AUTHORITY",
    "PRODUCTION_ENABLED",
    "PROMOTION_AUTHORITY",
    "PredictedEffectsEmitDisabled",
    "PredictedEffectsV1",
    "RELEASE_AUTHORITY",
    "SCHEMA",
    "build_predicted_effects_json_v1",
    "build_predicted_effects_payload_v1",
    "emit_hook_status_v1",
]
