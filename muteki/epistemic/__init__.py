"""Epistemic kernel primitives.

Research substrate for muteki.eval/muteki.research; not part of the production
solver path.

This package is host-authority code.  It deliberately does not import the legacy
shared graph or expose an authority database path to workers.
"""

from .contracts import (
    CANONICAL_SCHEMA_VERSION,
    CanonicalReceipt,
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)

__all__ = [
    "CANONICAL_SCHEMA_VERSION",
    "CanonicalReceipt",
    "EventEnvelopeV2",
    "canonical_digest",
    "canonical_json_bytes",
    "freeze_json",
]
