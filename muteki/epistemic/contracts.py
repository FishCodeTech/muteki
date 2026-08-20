"""Immutable, version-pinned canonical contracts for Protocol 2.

The serializer intentionally accepts a smaller value language than ordinary JSON:
string keys, strings, integers, booleans, null, lists/tuples and mappings.  Floats
are rejected so hashes never depend on NaN handling, exponent spelling, or platform
rounding.  Domain contracts represent measurements as integer base units.
"""

from __future__ import annotations

import hashlib
import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from types import MappingProxyType
from typing import Any, TypeAlias


CANONICAL_SCHEMA_VERSION = 2
CanonicalScalar: TypeAlias = None | bool | int | str
FrozenJSON: TypeAlias = CanonicalScalar | tuple["FrozenJSON", ...] | Mapping[str, "FrozenJSON"]


class CanonicalValueError(ValueError):
    """A value cannot participate in Protocol 2 canonical hashing."""


def freeze_json(value: Any, *, path: str = "$") -> FrozenJSON:
    """Validate and recursively freeze the canonical JSON subset."""

    if value is None or isinstance(value, (bool, str)):
        return value
    if isinstance(value, int) and not isinstance(value, bool):
        return value
    if isinstance(value, float):
        raise CanonicalValueError(f"{path}: floats are not canonical; use integer base units")
    if isinstance(value, Mapping):
        frozen: dict[str, FrozenJSON] = {}
        for key, child in value.items():
            if not isinstance(key, str):
                raise CanonicalValueError(f"{path}: mapping keys must be strings")
            frozen[key] = freeze_json(child, path=f"{path}.{key}")
        return MappingProxyType(frozen)
    if isinstance(value, Sequence) and not isinstance(value, (str, bytes, bytearray)):
        return tuple(freeze_json(child, path=f"{path}[{index}]")
                     for index, child in enumerate(value))
    raise CanonicalValueError(f"{path}: unsupported canonical value {type(value).__name__}")


def _plain_json(value: FrozenJSON) -> Any:
    if isinstance(value, Mapping):
        return {key: _plain_json(value[key]) for key in sorted(value)}
    if isinstance(value, tuple):
        return [_plain_json(child) for child in value]
    return value


def canonical_json_bytes(value: Any) -> bytes:
    frozen = freeze_json(value)
    return json.dumps(
        _plain_json(frozen),
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")


def canonical_digest(value: Any) -> str:
    return hashlib.sha256(canonical_json_bytes(value)).hexdigest()


def _required_text(value: str, field_name: str) -> str:
    normalized = str(value or "").strip()
    if not normalized:
        raise ValueError(f"{field_name} is required")
    return normalized


@dataclass(frozen=True, slots=True)
class EventEnvelopeV2:
    event_id: str
    run_id: str
    command_id: str
    ordinal: int
    kind: str
    actor: str
    occurred_at_ns: int
    payload: Mapping[str, FrozenJSON] = field(default_factory=dict)
    parent_event_digest: str = ""
    schema_version: int = CANONICAL_SCHEMA_VERSION

    def __post_init__(self) -> None:
        for name in ("event_id", "run_id", "command_id", "kind", "actor"):
            object.__setattr__(self, name, _required_text(getattr(self, name), name))
        if self.schema_version != CANONICAL_SCHEMA_VERSION:
            raise ValueError("unsupported canonical schema version")
        if self.ordinal < 0 or self.occurred_at_ns < 0:
            raise ValueError("ordinal and occurred_at_ns must be non-negative")
        frozen = freeze_json(self.payload, path="$.payload")
        if not isinstance(frozen, Mapping):
            raise CanonicalValueError("payload must be a mapping")
        object.__setattr__(self, "payload", frozen)

    def canonical_body(self) -> dict[str, Any]:
        return {
            "actor": self.actor,
            "command_id": self.command_id,
            "event_id": self.event_id,
            "kind": self.kind,
            "occurred_at_ns": self.occurred_at_ns,
            "ordinal": self.ordinal,
            "parent_event_digest": self.parent_event_digest,
            "payload": self.payload,
            "run_id": self.run_id,
            "schema_version": self.schema_version,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class CanonicalReceipt:
    receipt_id: str
    run_id: str
    command_id: str
    kind: str
    payload: Mapping[str, FrozenJSON] = field(default_factory=dict)
    parent_digests: tuple[str, ...] = ()
    schema_version: int = CANONICAL_SCHEMA_VERSION

    def __post_init__(self) -> None:
        for name in ("receipt_id", "run_id", "command_id", "kind"):
            object.__setattr__(self, name, _required_text(getattr(self, name), name))
        if self.schema_version != CANONICAL_SCHEMA_VERSION:
            raise ValueError("unsupported canonical schema version")
        frozen = freeze_json(self.payload, path="$.payload")
        if not isinstance(frozen, Mapping):
            raise CanonicalValueError("payload must be a mapping")
        object.__setattr__(self, "payload", frozen)
        object.__setattr__(self, "parent_digests", tuple(self.parent_digests))

    def canonical_body(self) -> dict[str, Any]:
        return {
            "command_id": self.command_id,
            "kind": self.kind,
            "parent_digests": self.parent_digests,
            "payload": self.payload,
            "receipt_id": self.receipt_id,
            "run_id": self.run_id,
            "schema_version": self.schema_version,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())
