"""Lossless, read-only objects for resolving Protocol 2 command receipts.

``CanonicalReceipt`` intentionally commits hashes, not the complete command input,
event envelopes, outbox payloads, and projection mutations.  That compact receipt is
enough for the authority store, but it is not enough for a C6 compiler to resolve an
exact source field without reaching back into mutable/private database views.

This module defines the missing additive object/index contract.  A receipt object
contains every value committed by one command receipt, is itself sealed in
``ReceiptCAS``, and is resolved through a complete, contiguous index.  The resolver
is read-only.  It never writes canonical state and deliberately has no store,
admission, dispatch, progress, effect, or gate capability.
"""

from __future__ import annotations

import json
import re
from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from types import MappingProxyType
from typing import Any, Protocol, runtime_checkable

from muteki.epistemic.cas import CASIntegrityError, ReceiptCAS, SealedObject
from muteki.epistemic.contracts import (
    CANONICAL_SCHEMA_VERSION,
    CanonicalReceipt,
    EventEnvelopeV2,
    FrozenJSON,
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)


RECEIPT_OBJECT_SCHEMA_ID = "muteki.command-receipt-object.v1"
RECEIPT_INDEX_SCHEMA_ID = "muteki.command-receipt-object-index.v1"

_DIGEST_RE = re.compile(r"^[0-9a-f]{64}$")
_IDENTIFIER_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:-]{0,255}$")
_FIELD_PATH_RE = re.compile(
    r"^(?:command_payload|events\[(?:0|[1-9][0-9]*)\])"
    r"(?:\.[A-Za-z_][A-Za-z0-9_-]*|\[(?:0|[1-9][0-9]*)\])*$"
)
_EVENT_ROOT_RE = re.compile(r"^events\[(0|[1-9][0-9]*)\](?:\.|$)")

_RECEIPT_FIELDS = frozenset(
    {
        "command_id",
        "kind",
        "parent_digests",
        "payload",
        "receipt_id",
        "run_id",
        "schema_version",
    }
)
_RECEIPT_PAYLOAD_FIELDS = frozenset(
    {
        "command_payload_digest",
        "event_digests",
        "first_seq",
        "last_seq",
        "outbox",
        "projection_mutation_digest",
        "state_checksum",
    }
)
_EVENT_FIELDS = frozenset(
    {
        "actor",
        "command_id",
        "event_id",
        "kind",
        "occurred_at_ns",
        "ordinal",
        "parent_event_digest",
        "payload",
        "run_id",
        "schema_version",
    }
)
_OUTBOX_FIELDS = frozenset(
    {"ordinal", "outbox_id", "payload", "payload_digest", "topic"}
)
_MUTATION_FIELDS = frozenset({"kind", "payload"})
_OBJECT_FIELDS = frozenset(
    {
        "command_payload",
        "committed_at_ns",
        "events",
        "outbox",
        "projection_mutations",
        "receipt",
        "schema_id",
    }
)
_INDEX_ENTRY_FIELDS = frozenset(
    {
        "byte_count",
        "command_id",
        "diagnostic_receipt_digest",
        "first_seq",
        "last_seq",
        "object_digest",
        "receipt_digest",
        "run_id",
        "state",
    }
)
_INDEX_FIELDS = frozenset(
    {
        "complete_through_seq",
        "entries",
        "head_event_digest",
        "run_id",
        "schema_id",
    }
)


def _text(value: object, name: str, *, identifier: bool = False) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be an exact non-empty string")
    if identifier and not _IDENTIFIER_RE.fullmatch(value):
        raise ValueError(f"{name} must be a canonical identifier")
    return value


def _digest(value: object, name: str, *, allow_empty: bool = False) -> str:
    if allow_empty and value == "":
        return ""
    text = _text(value, name)
    if not _DIGEST_RE.fullmatch(text):
        raise ValueError(f"{name} must be an exact lowercase sha256")
    return text


def _integer(value: object, name: str, *, minimum: int = 0) -> int:
    if type(value) is not int or value < minimum:
        raise ValueError(f"{name} must be an exact integer >= {minimum}")
    return value


def _mapping(value: object, name: str) -> Mapping[str, FrozenJSON]:
    if type(value) is not dict and not isinstance(value, MappingProxyType):
        raise TypeError(f"{name} must be a built-in or frozen canonical mapping")
    frozen = freeze_json(value, path=f"$.{name}")
    if not isinstance(frozen, Mapping):
        raise TypeError(f"{name} must freeze to a mapping")
    return frozen


def _exact_fields(value: Mapping[str, Any], expected: frozenset[str], name: str) -> None:
    if set(value) != expected:
        raise ValueError(f"{name} has an incomplete or unversioned shape")


def _thaw(value: FrozenJSON) -> Any:
    if isinstance(value, Mapping):
        return {key: _thaw(child) for key, child in value.items()}
    if type(value) is tuple:
        return [_thaw(child) for child in value]
    return value


def _strict_json_loads(data: bytes) -> dict[str, Any]:
    def object_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    def reject_float(value: str) -> Any:
        raise ValueError(f"non-integer JSON number is forbidden: {value}")

    try:
        decoded = data.decode("utf-8")
        value = json.loads(
            decoded,
            object_pairs_hook=object_pairs,
            parse_float=reject_float,
            parse_constant=reject_float,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("receipt object is not strict UTF-8 JSON") from exc
    if type(value) is not dict:
        raise ValueError("receipt object root must be a built-in dict")
    if canonical_json_bytes(value) != data:
        raise ValueError("receipt object bytes are not canonical JSON")
    return value


@dataclass(frozen=True, slots=True)
class ReceiptOutboxObjectV1:
    ordinal: int
    outbox_id: str
    topic: str
    payload: Mapping[str, FrozenJSON]
    payload_digest: str

    def __post_init__(self) -> None:
        object.__setattr__(self, "ordinal", _integer(self.ordinal, "ordinal"))
        object.__setattr__(
            self, "outbox_id", _text(self.outbox_id, "outbox_id", identifier=True)
        )
        object.__setattr__(self, "topic", _text(self.topic, "topic", identifier=True))
        payload = _mapping(self.payload, "outbox.payload")
        object.__setattr__(self, "payload", payload)
        declared = _digest(self.payload_digest, "payload_digest")
        if declared != canonical_digest(payload):
            raise ValueError("outbox payload digest does not match its exact payload")
        object.__setattr__(self, "payload_digest", declared)

    def canonical_body(self) -> dict[str, Any]:
        return {
            "ordinal": self.ordinal,
            "outbox_id": self.outbox_id,
            "payload": self.payload,
            "payload_digest": self.payload_digest,
            "topic": self.topic,
        }

    def receipt_summary(self) -> dict[str, Any]:
        return {
            "ordinal": self.ordinal,
            "outbox_id": self.outbox_id,
            "payload_digest": self.payload_digest,
            "topic": self.topic,
        }


@dataclass(frozen=True, slots=True)
class ReceiptProjectionMutationV1:
    kind: str
    payload: Mapping[str, FrozenJSON]

    def __post_init__(self) -> None:
        object.__setattr__(self, "kind", _text(self.kind, "kind", identifier=True))
        object.__setattr__(
            self, "payload", _mapping(self.payload, "projection_mutation.payload")
        )

    def canonical_body(self) -> dict[str, Any]:
        return {"kind": self.kind, "payload": self.payload}


@dataclass(frozen=True, slots=True)
class CommandReceiptObjectV1:
    """The complete immutable material committed by one command receipt."""

    receipt: CanonicalReceipt
    command_payload: Mapping[str, FrozenJSON]
    events: tuple[EventEnvelopeV2, ...]
    outbox: tuple[ReceiptOutboxObjectV1, ...]
    projection_mutations: tuple[ReceiptProjectionMutationV1, ...]
    committed_at_ns: int
    schema_id: str = RECEIPT_OBJECT_SCHEMA_ID

    def __post_init__(self) -> None:
        if type(self.receipt) is not CanonicalReceipt:
            raise TypeError("receipt must be exactly CanonicalReceipt")
        if self.schema_id != RECEIPT_OBJECT_SCHEMA_ID:
            raise ValueError("unsupported receipt object schema")
        if self.receipt.kind != "COMMAND_COMMITTED":
            raise ValueError("receipt object requires a command commit receipt")
        if self.receipt.receipt_id != f"receipt:{self.receipt.command_id}":
            raise ValueError("receipt_id does not bind the exact command_id")
        if self.receipt.parent_digests:
            raise ValueError("receipt object v1 requires the current empty parent set")
        payload = dict(self.receipt.payload)
        _exact_fields(payload, _RECEIPT_PAYLOAD_FIELDS, "receipt payload")

        command_payload = _mapping(self.command_payload, "command_payload")
        object.__setattr__(self, "command_payload", command_payload)
        if _digest(
            payload["command_payload_digest"], "command_payload_digest"
        ) != canonical_digest(command_payload):
            raise ValueError("command payload does not match the receipt commitment")

        if type(self.events) is not tuple or not self.events or any(
            type(item) is not EventEnvelopeV2 for item in self.events
        ):
            raise TypeError("events must be a non-empty built-in tuple of envelopes")
        event_digests: list[str] = []
        previous_digest: str | None = None
        for ordinal, event in enumerate(self.events):
            if (
                event.run_id != self.receipt.run_id
                or event.command_id != self.receipt.command_id
                or type(event.ordinal) is not int
                or event.ordinal != ordinal
            ):
                raise ValueError("event identity does not bind the receipt command")
            _integer(event.occurred_at_ns, "event.occurred_at_ns")
            if previous_digest is not None and event.parent_event_digest != previous_digest:
                raise ValueError("event parent chain diverges inside the command")
            if event.parent_event_digest:
                _digest(event.parent_event_digest, "parent_event_digest")
            previous_digest = event.digest
            event_digests.append(event.digest)
        declared_events = payload["event_digests"]
        if type(declared_events) is not tuple or tuple(event_digests) != declared_events:
            raise ValueError("event envelopes do not match the receipt event set")

        first_seq = _integer(payload["first_seq"], "first_seq", minimum=1)
        last_seq = _integer(payload["last_seq"], "last_seq", minimum=1)
        if last_seq - first_seq + 1 != len(self.events):
            raise ValueError("receipt sequence boundary does not match event count")

        if type(self.outbox) is not tuple or any(
            type(item) is not ReceiptOutboxObjectV1 for item in self.outbox
        ):
            raise TypeError("outbox must be a built-in tuple of outbox objects")
        if tuple(item.ordinal for item in self.outbox) != tuple(range(len(self.outbox))):
            raise ValueError("outbox ordinals must be contiguous and canonical")
        declared_outbox = payload["outbox"]
        expected_outbox = tuple(item.receipt_summary() for item in self.outbox)
        if canonical_json_bytes(declared_outbox) != canonical_json_bytes(expected_outbox):
            raise ValueError("outbox objects do not match the receipt commitment")

        if type(self.projection_mutations) is not tuple or any(
            type(item) is not ReceiptProjectionMutationV1
            for item in self.projection_mutations
        ):
            raise TypeError("projection_mutations must be a built-in tuple")
        mutation_bodies = [item.canonical_body() for item in self.projection_mutations]
        if _digest(
            payload["projection_mutation_digest"], "projection_mutation_digest"
        ) != canonical_digest(mutation_bodies):
            raise ValueError("projection mutations do not match the receipt commitment")
        _digest(payload["state_checksum"], "state_checksum")

        committed = _integer(self.committed_at_ns, "committed_at_ns")
        if any(committed < event.occurred_at_ns for event in self.events):
            raise ValueError("command cannot commit before one of its events occurred")
        object.__setattr__(self, "committed_at_ns", committed)

    @property
    def first_seq(self) -> int:
        return int(self.receipt.payload["first_seq"])

    @property
    def last_seq(self) -> int:
        return int(self.receipt.payload["last_seq"])

    def canonical_body(self) -> dict[str, Any]:
        return {
            "command_payload": self.command_payload,
            "committed_at_ns": self.committed_at_ns,
            "events": [item.canonical_body() for item in self.events],
            "outbox": [item.canonical_body() for item in self.outbox],
            "projection_mutations": [
                item.canonical_body() for item in self.projection_mutations
            ],
            "receipt": self.receipt.canonical_body(),
            "schema_id": self.schema_id,
        }

    @property
    def bytes(self) -> bytes:
        return canonical_json_bytes(self.canonical_body())

    @property
    def object_digest(self) -> str:
        return canonical_digest(self.canonical_body())

    def seal(self, cas: ReceiptCAS) -> SealedObject:
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        sealed = cas.seal_bytes(self.bytes)
        if sealed.digest != self.object_digest:
            raise ValueError("CAS returned a digest different from canonical bytes")
        return sealed

    @classmethod
    def from_bytes(cls, data: bytes) -> CommandReceiptObjectV1:
        if type(data) is not bytes:
            raise TypeError("receipt object data must be exact bytes")
        raw = _strict_json_loads(data)
        _exact_fields(raw, _OBJECT_FIELDS, "receipt object")
        if raw["schema_id"] != RECEIPT_OBJECT_SCHEMA_ID:
            raise ValueError("unsupported receipt object schema")

        receipt_raw = raw["receipt"]
        if type(receipt_raw) is not dict:
            raise TypeError("receipt must be a built-in dict")
        _exact_fields(receipt_raw, _RECEIPT_FIELDS, "receipt")
        if type(receipt_raw["payload"]) is not dict:
            raise TypeError("receipt payload must be a built-in dict")
        if type(receipt_raw["parent_digests"]) is not list or any(
            type(item) is not str for item in receipt_raw["parent_digests"]
        ):
            raise TypeError("receipt parent_digests must be a string list")
        for name in ("receipt_id", "run_id", "command_id", "kind"):
            _text(receipt_raw[name], f"receipt.{name}")
        if (
            type(receipt_raw["schema_version"]) is not int
            or receipt_raw["schema_version"] != CANONICAL_SCHEMA_VERSION
        ):
            raise ValueError("receipt schema version is not exact")
        receipt = CanonicalReceipt(
            receipt_id=receipt_raw["receipt_id"],
            run_id=receipt_raw["run_id"],
            command_id=receipt_raw["command_id"],
            kind=receipt_raw["kind"],
            payload=receipt_raw["payload"],
            parent_digests=tuple(receipt_raw["parent_digests"]),
            schema_version=receipt_raw["schema_version"],
        )

        event_objects: list[EventEnvelopeV2] = []
        if type(raw["events"]) is not list:
            raise TypeError("events must be a built-in list")
        for index, event_raw in enumerate(raw["events"]):
            if type(event_raw) is not dict:
                raise TypeError("event must be a built-in dict")
            _exact_fields(event_raw, _EVENT_FIELDS, f"events[{index}]")
            for name in (
                "actor",
                "command_id",
                "event_id",
                "kind",
                "run_id",
            ):
                _text(event_raw[name], f"events[{index}].{name}")
            for name in ("ordinal", "occurred_at_ns"):
                _integer(event_raw[name], f"events[{index}].{name}")
            if type(event_raw["payload"]) is not dict:
                raise TypeError("event payload must be a built-in dict")
            _digest(
                event_raw["parent_event_digest"],
                f"events[{index}].parent_event_digest",
                allow_empty=True,
            )
            if (
                type(event_raw["schema_version"]) is not int
                or event_raw["schema_version"] != CANONICAL_SCHEMA_VERSION
            ):
                raise ValueError("event schema version is not exact")
            event_objects.append(
                EventEnvelopeV2(
                    event_id=event_raw["event_id"],
                    run_id=event_raw["run_id"],
                    command_id=event_raw["command_id"],
                    ordinal=event_raw["ordinal"],
                    kind=event_raw["kind"],
                    actor=event_raw["actor"],
                    occurred_at_ns=event_raw["occurred_at_ns"],
                    payload=event_raw["payload"],
                    parent_event_digest=event_raw["parent_event_digest"],
                    schema_version=event_raw["schema_version"],
                )
            )

        outbox_objects: list[ReceiptOutboxObjectV1] = []
        if type(raw["outbox"]) is not list:
            raise TypeError("outbox must be a built-in list")
        for index, outbox_raw in enumerate(raw["outbox"]):
            if type(outbox_raw) is not dict:
                raise TypeError("outbox item must be a built-in dict")
            _exact_fields(outbox_raw, _OUTBOX_FIELDS, f"outbox[{index}]")
            if type(outbox_raw["payload"]) is not dict:
                raise TypeError("outbox payload must be a built-in dict")
            outbox_objects.append(
                ReceiptOutboxObjectV1(
                    ordinal=outbox_raw["ordinal"],
                    outbox_id=outbox_raw["outbox_id"],
                    topic=outbox_raw["topic"],
                    payload=outbox_raw["payload"],
                    payload_digest=outbox_raw["payload_digest"],
                )
            )

        mutation_objects: list[ReceiptProjectionMutationV1] = []
        if type(raw["projection_mutations"]) is not list:
            raise TypeError("projection_mutations must be a built-in list")
        for index, mutation_raw in enumerate(raw["projection_mutations"]):
            if type(mutation_raw) is not dict:
                raise TypeError("projection mutation must be a built-in dict")
            _exact_fields(
                mutation_raw, _MUTATION_FIELDS, f"projection_mutations[{index}]"
            )
            if type(mutation_raw["payload"]) is not dict:
                raise TypeError("projection mutation payload must be a built-in dict")
            mutation_objects.append(
                ReceiptProjectionMutationV1(
                    kind=mutation_raw["kind"], payload=mutation_raw["payload"]
                )
            )

        if type(raw["command_payload"]) is not dict:
            raise TypeError("command_payload must be a built-in dict")
        return cls(
            receipt=receipt,
            command_payload=raw["command_payload"],
            events=tuple(event_objects),
            outbox=tuple(outbox_objects),
            projection_mutations=tuple(mutation_objects),
            committed_at_ns=raw["committed_at_ns"],
            schema_id=raw["schema_id"],
        )


class ReceiptObjectState(str, Enum):
    RESOLVED = "resolved"
    UNRESOLVED = "unresolved"
    UNKNOWN = "unknown"
    REBOUND = "rebound"


@dataclass(frozen=True, slots=True)
class ReceiptObjectIndexEntryV1:
    run_id: str
    command_id: str
    receipt_digest: str
    first_seq: int
    last_seq: int
    state: ReceiptObjectState
    object_digest: str = ""
    byte_count: int = 0
    diagnostic_receipt_digest: str = ""

    def __post_init__(self) -> None:
        object.__setattr__(self, "run_id", _text(self.run_id, "run_id", identifier=True))
        object.__setattr__(
            self, "command_id", _text(self.command_id, "command_id", identifier=True)
        )
        object.__setattr__(
            self, "receipt_digest", _digest(self.receipt_digest, "receipt_digest")
        )
        first = _integer(self.first_seq, "first_seq", minimum=1)
        last = _integer(self.last_seq, "last_seq", minimum=1)
        if last < first:
            raise ValueError("last_seq cannot precede first_seq")
        object.__setattr__(self, "first_seq", first)
        object.__setattr__(self, "last_seq", last)
        if type(self.state) is not ReceiptObjectState:
            raise TypeError("state must be ReceiptObjectState")
        byte_count = _integer(self.byte_count, "byte_count")
        object_digest = self.object_digest
        diagnostic_digest = self.diagnostic_receipt_digest
        if type(object_digest) is not str or type(diagnostic_digest) is not str:
            raise TypeError("object and diagnostic digests must be exact strings")
        if self.state is ReceiptObjectState.RESOLVED:
            object.__setattr__(
                self, "object_digest", _digest(self.object_digest, "object_digest")
            )
            object.__setattr__(
                self, "byte_count", _integer(byte_count, "byte_count", minimum=1)
            )
            if diagnostic_digest != "":
                raise ValueError("resolved entry cannot carry a failure diagnostic")
        else:
            if object_digest != "" or byte_count != 0:
                raise ValueError("non-resolved entry cannot claim a receipt object")
            object.__setattr__(
                self,
                "diagnostic_receipt_digest",
                _digest(
                    self.diagnostic_receipt_digest, "diagnostic_receipt_digest"
                ),
            )

    @classmethod
    def resolved(
        cls, receipt_object: CommandReceiptObjectV1, sealed: SealedObject
    ) -> ReceiptObjectIndexEntryV1:
        if type(receipt_object) is not CommandReceiptObjectV1:
            raise TypeError("receipt_object must be CommandReceiptObjectV1")
        if type(sealed) is not SealedObject:
            raise TypeError("sealed must be SealedObject")
        if (
            sealed.digest != receipt_object.object_digest
            or sealed.byte_count != len(receipt_object.bytes)
        ):
            raise ValueError("sealed object does not bind the receipt object")
        return cls(
            run_id=receipt_object.receipt.run_id,
            command_id=receipt_object.receipt.command_id,
            receipt_digest=receipt_object.receipt.digest,
            first_seq=receipt_object.first_seq,
            last_seq=receipt_object.last_seq,
            state=ReceiptObjectState.RESOLVED,
            object_digest=sealed.digest,
            byte_count=sealed.byte_count,
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "byte_count": self.byte_count,
            "command_id": self.command_id,
            "diagnostic_receipt_digest": self.diagnostic_receipt_digest,
            "first_seq": self.first_seq,
            "last_seq": self.last_seq,
            "object_digest": self.object_digest,
            "receipt_digest": self.receipt_digest,
            "run_id": self.run_id,
            "state": self.state.value,
        }


@dataclass(frozen=True, slots=True)
class CommandReceiptObjectIndexV1:
    """A contiguous receipt boundary index for one canonical run prefix."""

    run_id: str
    complete_through_seq: int
    head_event_digest: str
    entries: tuple[ReceiptObjectIndexEntryV1, ...]
    schema_id: str = RECEIPT_INDEX_SCHEMA_ID

    def __post_init__(self) -> None:
        object.__setattr__(self, "run_id", _text(self.run_id, "run_id", identifier=True))
        if self.schema_id != RECEIPT_INDEX_SCHEMA_ID:
            raise ValueError("unsupported receipt index schema")
        complete = _integer(self.complete_through_seq, "complete_through_seq")
        object.__setattr__(self, "complete_through_seq", complete)
        if type(self.head_event_digest) is not str:
            raise TypeError("head_event_digest must be an exact string")
        if type(self.entries) is not tuple or any(
            type(item) is not ReceiptObjectIndexEntryV1 for item in self.entries
        ):
            raise TypeError("entries must be a built-in tuple of index entries")
        if complete == 0:
            if self.entries or self.head_event_digest:
                raise ValueError("empty prefix cannot claim entries or a head digest")
            return
        object.__setattr__(
            self,
            "head_event_digest",
            _digest(self.head_event_digest, "head_event_digest"),
        )
        if not self.entries:
            raise ValueError("non-empty prefix requires index entries")
        expected_first = 1
        receipt_digests: set[str] = set()
        command_ids: set[str] = set()
        for entry in self.entries:
            if entry.run_id != self.run_id:
                raise ValueError("index entry belongs to a different run")
            if entry.first_seq != expected_first:
                raise ValueError("index entries must cover a contiguous command prefix")
            if entry.receipt_digest in receipt_digests or entry.command_id in command_ids:
                raise ValueError("index contains duplicate receipt or command identity")
            receipt_digests.add(entry.receipt_digest)
            command_ids.add(entry.command_id)
            expected_first = entry.last_seq + 1
        if self.entries[-1].last_seq != complete:
            raise ValueError("index entries do not reach complete_through_seq")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "complete_through_seq": self.complete_through_seq,
            "entries": [item.canonical_body() for item in self.entries],
            "head_event_digest": self.head_event_digest,
            "run_id": self.run_id,
            "schema_id": self.schema_id,
        }

    @property
    def bytes(self) -> bytes:
        return canonical_json_bytes(self.canonical_body())

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    def seal(self, cas: ReceiptCAS) -> SealedObject:
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        sealed = cas.seal_bytes(self.bytes)
        if sealed.digest != self.digest:
            raise ValueError("CAS returned a digest different from the index bytes")
        return sealed

    @classmethod
    def from_bytes(cls, data: bytes) -> CommandReceiptObjectIndexV1:
        if type(data) is not bytes:
            raise TypeError("index data must be exact bytes")
        raw = _strict_json_loads(data)
        _exact_fields(raw, _INDEX_FIELDS, "receipt index")
        if type(raw["entries"]) is not list:
            raise TypeError("index entries must be a built-in list")
        entries: list[ReceiptObjectIndexEntryV1] = []
        for index, item in enumerate(raw["entries"]):
            if type(item) is not dict:
                raise TypeError("index entry must be a built-in dict")
            _exact_fields(item, _INDEX_ENTRY_FIELDS, f"entries[{index}]")
            try:
                state = ReceiptObjectState(item["state"])
            except (TypeError, ValueError) as exc:
                raise ValueError("index entry has an unknown state") from exc
            entries.append(
                ReceiptObjectIndexEntryV1(
                    run_id=item["run_id"],
                    command_id=item["command_id"],
                    receipt_digest=item["receipt_digest"],
                    first_seq=item["first_seq"],
                    last_seq=item["last_seq"],
                    state=state,
                    object_digest=item["object_digest"],
                    byte_count=item["byte_count"],
                    diagnostic_receipt_digest=item["diagnostic_receipt_digest"],
                )
            )
        return cls(
            run_id=raw["run_id"],
            complete_through_seq=raw["complete_through_seq"],
            head_event_digest=raw["head_event_digest"],
            entries=tuple(entries),
            schema_id=raw["schema_id"],
        )


@dataclass(frozen=True, slots=True)
class ReceiptFieldPointerV1:
    """An exact field in one complete command receipt object."""

    run_id: str
    command_id: str
    receipt_digest: str
    object_digest: str
    field_path: str
    value_digest: str
    event_ordinal: int | None = None
    event_digest: str = ""

    def __post_init__(self) -> None:
        object.__setattr__(self, "run_id", _text(self.run_id, "run_id", identifier=True))
        object.__setattr__(
            self, "command_id", _text(self.command_id, "command_id", identifier=True)
        )
        for name in ("receipt_digest", "object_digest", "value_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if type(self.field_path) is not str or not _FIELD_PATH_RE.fullmatch(
            self.field_path
        ):
            raise ValueError("field_path is not an allowed canonical source path")
        match = _EVENT_ROOT_RE.match(self.field_path)
        if match:
            ordinal = int(match.group(1))
            if type(self.event_ordinal) is not int or self.event_ordinal != ordinal:
                raise ValueError("event field path requires its exact event ordinal")
            object.__setattr__(
                self, "event_digest", _digest(self.event_digest, "event_digest")
            )
        elif self.event_ordinal is not None or self.event_digest:
            raise ValueError("command field pointer cannot invent an event identity")
        elif type(self.event_digest) is not str:
            raise TypeError("event_digest must be an exact string")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "command_id": self.command_id,
            "event_digest": self.event_digest,
            "event_ordinal": self.event_ordinal,
            "field_path": self.field_path,
            "object_digest": self.object_digest,
            "receipt_digest": self.receipt_digest,
            "run_id": self.run_id,
            "value_digest": self.value_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


class ReceiptResolutionError(RuntimeError):
    """Base class for fail-closed receipt resolution."""


class UnresolvedReceiptError(ReceiptResolutionError):
    pass


class UnknownReceiptError(ReceiptResolutionError):
    pass


class ReboundReceiptError(ReceiptResolutionError):
    pass


class HindsightReceiptError(ReceiptResolutionError):
    pass


@dataclass(frozen=True, slots=True)
class VerifiedEventReferenceV1:
    seq: int
    event_digest: str
    receipt_digest: str
    kind: str
    payload_digest: str
    attempt_id: str = ""

    def __post_init__(self) -> None:
        object.__setattr__(self, "seq", _integer(self.seq, "seq", minimum=1))
        for name in ("event_digest", "receipt_digest", "payload_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        object.__setattr__(self, "kind", _text(self.kind, "kind", identifier=True))
        if type(self.attempt_id) is not str:
            raise TypeError("attempt_id must be an exact string")
        if self.attempt_id:
            object.__setattr__(
                self,
                "attempt_id",
                _text(self.attempt_id, "attempt_id", identifier=True),
            )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "attempt_id": self.attempt_id,
            "event_digest": self.event_digest,
            "kind": self.kind,
            "payload_digest": self.payload_digest,
            "receipt_digest": self.receipt_digest,
            "seq": self.seq,
        }


@dataclass(frozen=True, slots=True)
class VerifiedReceiptPrefixV1:
    run_id: str
    cutoff_seq: int
    head_event_digest: str
    receipt_digests: tuple[str, ...]
    events: tuple[VerifiedEventReferenceV1, ...]
    index_digest: str

    def __post_init__(self) -> None:
        object.__setattr__(
            self, "run_id", _text(self.run_id, "run_id", identifier=True)
        )
        cutoff = _integer(self.cutoff_seq, "cutoff_seq")
        object.__setattr__(self, "cutoff_seq", cutoff)
        if type(self.head_event_digest) is not str:
            raise TypeError("head_event_digest must be an exact string")
        if cutoff:
            object.__setattr__(
                self,
                "head_event_digest",
                _digest(self.head_event_digest, "head_event_digest"),
            )
        elif self.head_event_digest:
            raise ValueError("empty prefix cannot claim a head event digest")
        if type(self.receipt_digests) is not tuple:
            raise TypeError("receipt_digests must be a built-in tuple")
        for digest in self.receipt_digests:
            _digest(digest, "receipt_digest")
        if len(self.receipt_digests) != len(set(self.receipt_digests)):
            raise ValueError("receipt_digests contains duplicates")
        if type(self.events) is not tuple or any(
            type(item) is not VerifiedEventReferenceV1 for item in self.events
        ):
            raise TypeError("events must be a tuple of verified event references")
        if tuple(item.seq for item in self.events) != tuple(range(1, cutoff + 1)):
            raise ValueError("verified events must cover every sequence through cutoff")
        if cutoff and self.events[-1].event_digest != self.head_event_digest:
            raise ValueError("verified event inventory does not reach the prefix head")
        object.__setattr__(
            self, "index_digest", _digest(self.index_digest, "index_digest")
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "cutoff_seq": self.cutoff_seq,
            "events": [item.canonical_body() for item in self.events],
            "head_event_digest": self.head_event_digest,
            "index_digest": self.index_digest,
            "receipt_digests": list(self.receipt_digests),
            "run_id": self.run_id,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ResolvedReceiptFieldV1:
    pointer: ReceiptFieldPointerV1
    value: FrozenJSON
    command_first_seq: int
    command_last_seq: int
    event_kind: str = ""
    event_global_seq: int | None = None

    def __post_init__(self) -> None:
        if type(self.pointer) is not ReceiptFieldPointerV1:
            raise TypeError("pointer must be ReceiptFieldPointerV1")
        object.__setattr__(self, "value", freeze_json(self.value, path="$.value"))


@runtime_checkable
class ReceiptFieldResolverV1(Protocol):
    """Narrow read port consumed by production-disabled context compilers.

    A future store-backed implementation may satisfy this port directly.  Consumers
    must not need a SQLite connection, database path, CAS write handle, or authority
    object.
    """

    @property
    def index(self) -> CommandReceiptObjectIndexV1: ...

    def verify_complete_through(self, cutoff_seq: int) -> VerifiedReceiptPrefixV1: ...

    def resolve(
        self, pointer: ReceiptFieldPointerV1, *, cutoff_seq: int | None = None
    ) -> ResolvedReceiptFieldV1: ...


def _path_tokens(path: str) -> tuple[str | int, ...]:
    tokens: list[str | int] = []
    cursor = 0
    while cursor < len(path):
        if path[cursor] == ".":
            cursor += 1
            continue
        if path[cursor] == "[":
            end = path.index("]", cursor)
            tokens.append(int(path[cursor + 1 : end]))
            cursor = end + 1
            continue
        end = cursor
        while end < len(path) and path[end] not in ".[":
            end += 1
        tokens.append(path[cursor:end])
        cursor = end
    return tuple(tokens)


def _resolve_path(document: Any, path: str) -> FrozenJSON:
    value = document
    try:
        for token in _path_tokens(path):
            if type(token) is str:
                if not isinstance(value, Mapping) or token not in value:
                    raise KeyError(token)
                value = value[token]
            else:
                if type(value) not in (list, tuple) or token >= len(value):
                    raise KeyError(token)
                value = value[token]
    except (KeyError, TypeError) as exc:
        raise UnresolvedReceiptError("receipt field path does not resolve exactly") from exc
    return freeze_json(value, path=f"$.{path}")


class CanonicalCommandReceiptResolverV1:
    """Resolve exact fields only after verifying a lossless receipt prefix."""

    def __init__(
        self, *, index: CommandReceiptObjectIndexV1, cas: ReceiptCAS
    ) -> None:
        if type(index) is not CommandReceiptObjectIndexV1:
            raise TypeError("index must be CommandReceiptObjectIndexV1")
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        self._index = index
        self._cas = cas
        self._entries = {item.receipt_digest: item for item in index.entries}
        self._cache: dict[str, CommandReceiptObjectV1] = {}

    @property
    def index(self) -> CommandReceiptObjectIndexV1:
        return self._index

    def _entry(self, receipt_digest: str) -> ReceiptObjectIndexEntryV1:
        entry = self._entries.get(receipt_digest)
        if entry is None:
            raise UnresolvedReceiptError("receipt is absent from the complete index")
        if entry.state is ReceiptObjectState.UNRESOLVED:
            raise UnresolvedReceiptError("receipt object is explicitly unresolved")
        if entry.state is ReceiptObjectState.UNKNOWN:
            raise UnknownReceiptError("receipt object availability is UNKNOWN")
        if entry.state is ReceiptObjectState.REBOUND:
            raise ReboundReceiptError("receipt object has a recorded rebound failure")
        return entry

    def _load(self, entry: ReceiptObjectIndexEntryV1) -> CommandReceiptObjectV1:
        cached = self._cache.get(entry.object_digest)
        if cached is not None:
            return cached
        try:
            data = self._cas.read_verified(entry.object_digest)
        except (CASIntegrityError, ValueError) as exc:
            raise UnknownReceiptError("receipt object CAS bytes are unavailable") from exc
        if len(data) != entry.byte_count:
            raise ReboundReceiptError("receipt object byte count was rebound")
        try:
            receipt_object = CommandReceiptObjectV1.from_bytes(data)
        except (TypeError, ValueError) as exc:
            raise ReboundReceiptError("receipt object does not validate exactly") from exc
        exact = (
            receipt_object.object_digest == entry.object_digest,
            receipt_object.receipt.digest == entry.receipt_digest,
            receipt_object.receipt.run_id == entry.run_id,
            receipt_object.receipt.command_id == entry.command_id,
            receipt_object.first_seq == entry.first_seq,
            receipt_object.last_seq == entry.last_seq,
        )
        if not all(exact):
            raise ReboundReceiptError("receipt object identity differs from its index")
        self._cache[entry.object_digest] = receipt_object
        return receipt_object

    def verify_complete_through(self, cutoff_seq: int) -> VerifiedReceiptPrefixV1:
        cutoff = _integer(cutoff_seq, "cutoff_seq")
        if cutoff > self._index.complete_through_seq:
            raise UnresolvedReceiptError("cutoff exceeds the complete receipt index")
        selected: list[ReceiptObjectIndexEntryV1] = []
        if cutoff:
            for entry in self._index.entries:
                if entry.last_seq <= cutoff:
                    selected.append(entry)
                elif entry.first_seq <= cutoff < entry.last_seq:
                    raise HindsightReceiptError("cutoff splits one atomic command receipt")
                else:
                    break
            if not selected or selected[-1].last_seq != cutoff:
                raise UnresolvedReceiptError("cutoff is not a complete command boundary")

        previous_event_digest = ""
        receipt_digests: list[str] = []
        event_references: list[VerifiedEventReferenceV1] = []
        for entry in selected:
            resolved_entry = self._entry(entry.receipt_digest)
            receipt_object = self._load(resolved_entry)
            first_parent = receipt_object.events[0].parent_event_digest
            if first_parent != previous_event_digest:
                raise ReboundReceiptError("cross-command event chain is not contiguous")
            previous_event_digest = receipt_object.events[-1].digest
            receipt_digests.append(receipt_object.receipt.digest)
            for event in receipt_object.events:
                attempt_id = event.payload.get("attempt_id", "")
                if type(attempt_id) is not str:
                    attempt_id = ""
                event_references.append(
                    VerifiedEventReferenceV1(
                        seq=entry.first_seq + event.ordinal,
                        event_digest=event.digest,
                        receipt_digest=receipt_object.receipt.digest,
                        kind=event.kind,
                        payload_digest=canonical_digest(event.payload),
                        attempt_id=attempt_id,
                    )
                )

        if cutoff == self._index.complete_through_seq:
            if previous_event_digest != self._index.head_event_digest:
                raise ReboundReceiptError("index head digest differs from receipt objects")
        return VerifiedReceiptPrefixV1(
            run_id=self._index.run_id,
            cutoff_seq=cutoff,
            head_event_digest=previous_event_digest,
            receipt_digests=tuple(receipt_digests),
            events=tuple(event_references),
            index_digest=self._index.digest,
        )

    def pointer_for(
        self, receipt_digest: str, field_path: str, *, cutoff_seq: int | None = None
    ) -> ReceiptFieldPointerV1:
        digest = _digest(receipt_digest, "receipt_digest")
        entry = self._entry(digest)
        if cutoff_seq is not None:
            cutoff = _integer(cutoff_seq, "cutoff_seq")
            if entry.last_seq > cutoff:
                raise HindsightReceiptError("receipt is later than the decision cutoff")
        receipt_object = self._load(entry)
        document = {
            "command_payload": receipt_object.command_payload,
            "events": [item.canonical_body() for item in receipt_object.events],
        }
        value = _resolve_path(document, field_path)
        match = _EVENT_ROOT_RE.match(field_path)
        event_ordinal = int(match.group(1)) if match else None
        event_digest = (
            receipt_object.events[event_ordinal].digest
            if event_ordinal is not None
            else ""
        )
        return ReceiptFieldPointerV1(
            run_id=entry.run_id,
            command_id=entry.command_id,
            receipt_digest=entry.receipt_digest,
            object_digest=entry.object_digest,
            field_path=field_path,
            value_digest=canonical_digest(value),
            event_ordinal=event_ordinal,
            event_digest=event_digest,
        )

    def resolve(
        self, pointer: ReceiptFieldPointerV1, *, cutoff_seq: int | None = None
    ) -> ResolvedReceiptFieldV1:
        if type(pointer) is not ReceiptFieldPointerV1:
            raise TypeError("pointer must be ReceiptFieldPointerV1")
        if pointer.run_id != self._index.run_id:
            raise ReboundReceiptError("pointer was rebound to another run")
        entry = self._entry(pointer.receipt_digest)
        if (
            pointer.command_id != entry.command_id
            or pointer.object_digest != entry.object_digest
        ):
            raise ReboundReceiptError("pointer identity differs from the receipt index")
        if cutoff_seq is not None:
            cutoff = _integer(cutoff_seq, "cutoff_seq")
            if entry.last_seq > cutoff:
                raise HindsightReceiptError("receipt is later than the decision cutoff")
        receipt_object = self._load(entry)
        document = {
            "command_payload": receipt_object.command_payload,
            "events": [item.canonical_body() for item in receipt_object.events],
        }
        value = _resolve_path(document, pointer.field_path)
        if canonical_digest(value) != pointer.value_digest:
            raise ReboundReceiptError("field value differs from the sealed pointer")
        event_kind = ""
        event_global_seq: int | None = None
        if pointer.event_ordinal is not None:
            try:
                event = receipt_object.events[pointer.event_ordinal]
            except IndexError as exc:
                raise ReboundReceiptError("event ordinal exceeds the receipt") from exc
            if event.digest != pointer.event_digest:
                raise ReboundReceiptError("event identity differs from the pointer")
            event_kind = event.kind
            event_global_seq = entry.first_seq + pointer.event_ordinal
        return ResolvedReceiptFieldV1(
            pointer=pointer,
            value=value,
            command_first_seq=entry.first_seq,
            command_last_seq=entry.last_seq,
            event_kind=event_kind,
            event_global_seq=event_global_seq,
        )
