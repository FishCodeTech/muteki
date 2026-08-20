"""Protocol 2 epistemic kernel primitives.

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
