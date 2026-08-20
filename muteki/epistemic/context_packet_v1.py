"""Neutral production-disabled C6 ContextPacket v1 contracts.

The compiler consumes only exact fields resolved through the narrow canonical
receipt port.  It verifies a complete command boundary before reading any field,
keeps every lossless source pointer in the manifest, and emits a disposable typed
lossy view sealed in ``ReceiptCAS``.  It has no write, dispatch, progress, budget,
effect, gate, provider, filesystem-workspace, or UI capability.

This module is an additive research slice.  It does not claim that context packets
improve terminal outcomes.  Exact terminal and complete-accounting receipts may be
syntax-checked here, but the assessment remains ``INSUFFICIENT_EVIDENCE`` until an
evaluator-owned assignment authority and sample-derived paired reducer exist.
"""

from __future__ import annotations

import re
from collections import defaultdict
from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from types import MappingProxyType
from typing import Any

from muteki.epistemic.cas import ReceiptCAS, SealedObject
from muteki.epistemic.contracts import (
    FrozenJSON,
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)
from muteki.epistemic.receipt_objects import (
    ReceiptFieldPointerV1,
    ReceiptFieldResolverV1,
    ResolvedReceiptFieldV1,
    VerifiedReceiptPrefixV1,
)


CONTEXT_PACKET_SCHEMA_ID = "muteki.context-packet.v1"
CONTEXT_PACKET_COMPILER_VERSION = "muteki.context-packet.compiler.v1"
CONTEXT_POLICY_ID = "muteki.context-policy.receipt-only.v1"
OMISSION_POLICY_ID = "muteki.context-omission.explicit.v1"
REDACTION_POLICY_ID = "muteki.context-redaction.c6.v1"
NOT_AVAILABLE_H5 = "NOT_AVAILABLE(stage=H5)"
INSUFFICIENT_EVIDENCE = "INSUFFICIENT_EVIDENCE"
PRODUCTION_ENABLED = False
ACCEPTED_SET_CHANGE = False
ACCEPTED_SET_UNCHANGED = True

_DIGEST_RE = re.compile(r"^[0-9a-f]{64}$")
_IDENTIFIER_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:-]{0,255}$")
_WHITESPACE_RE = re.compile(r"\s+")
_BRACED_TOKEN_RE = re.compile(r"(?i)\b[A-Za-z][A-Za-z0-9_-]{1,40}\{[^{}\r\n]{1,256}\}")
_MIXED_BARE_TOKEN_RE = re.compile(
    r"(?<![A-Za-z0-9])(?=[A-Za-z0-9_-]{12,128}(?![A-Za-z0-9]))"
    r"(?=[A-Za-z0-9_-]*[A-Za-z])(?=[A-Za-z0-9_-]*[0-9])"
    r"[A-Za-z0-9_-]{12,128}"
)
_PATH_WORD_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_-]*")

_MAX_TEXT_CHARS = 2_048
_MAX_LIST_ITEMS = 32
_MAX_LIST_ITEM_CHARS = 512

_FORBIDDEN_RETROSPECTIVE_EVENT_KINDS = frozenset(
    {
        "ADJUDICATED_LABEL",
        "BLIND_ADJUDICATION_RECORDED",
        "EVALUATION_LABEL_REVEALED",
        "REFERENCE_SOLUTION_RECORDED",
        "SEALED_FINAL_TEST_RESULT",
    }
)
_HINDSIGHT_FIELD_NAMES = frozenset(
    {
        "accepted",
        "answer",
        "gold",
        "ground_truth",
        "label",
        "outcome",
        "reward",
        "score",
        "solution",
        "solved",
        "success",
        "verdict",
    }
)
_PRIOR_ATTEMPT_OUTCOME_FIELDS = frozenset({"outcome", "solved", "success", "verdict"})
_PRIOR_ATTEMPT_EVENT_KINDS = frozenset(
    {"BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN", "WORKER_TERMINAL", "WORKER_UNKNOWN"}
)
_TERMINAL_OUTCOME_EVENT_KINDS = frozenset(
    {"GOAL_COMPLETED", "S4E_CLOSURE_ATTESTED", "WORKER_TERMINAL"}
)
_TARGET_TERMINAL_EVENT_KINDS = frozenset(
    {
        "BUDGET_SETTLED",
        "BUDGET_USAGE_UNKNOWN",
        "WORKER_TERMINAL",
        "WORKER_UNKNOWN",
    }
)
_CANONICAL_OBSERVATION_EVENT_KINDS = frozenset(
    {
        "BUDGET_SETTLED",
        "BUDGET_USAGE_UNKNOWN",
        "CAPTURE_CHUNK_SEALED",
        "CAPTURE_MANIFEST_ADVANCED",
        "EFFECT_OBSERVED",
        "WORKER_TERMINAL",
        "WORKER_UNKNOWN",
    }
)
_PROPOSAL_EVENT_KINDS = frozenset(
    {
        "DISCRIMINATING_EXPERIMENT_PROPOSED",
        "HYPOTHESIS_PROPOSED",
        "WORKFLOW_MUTATION_PROPOSED",
    }
)
_REQUIRED_FIELD_EVENT_KINDS: Mapping[str, frozenset[str]] = MappingProxyType(
    {
        "acceptance_boundary": frozenset(
            {
                "CONTEXT_DECISION_REGISTERED",
                "DECISION_NEED_REGISTERED",
                "RUNTIME_CONTEXT_DECISION_REGISTERED",
            }
        ),
        "decision_need": frozenset(
            {
                "CONTEXT_DECISION_REGISTERED",
                "DECISION_NEED_REGISTERED",
                "RUNTIME_CONTEXT_DECISION_REGISTERED",
            }
        ),
        "effect_ambiguity": frozenset(
            {
                "CONTEXT_DECISION_REGISTERED",
                "DECISION_NEED_REGISTERED",
                "EFFECT_AMBIGUITY_SNAPSHOT",
                "RUNTIME_CONTEXT_DECISION_REGISTERED",
            }
        ),
        "non_negotiable_policy": frozenset(
            {
                "CONTEXT_DECISION_REGISTERED",
                "DECISION_NEED_REGISTERED",
                "RUNTIME_CONTEXT_DECISION_REGISTERED",
            }
        ),
        "objective": frozenset(
            {
                "CONTEXT_DECISION_REGISTERED",
                "DECISION_NEED_REGISTERED",
                "RUNTIME_CONTEXT_DECISION_REGISTERED",
            }
        ),
        "preallocated_attempt_id": frozenset(
            {
                "ATTEMPT_PREALLOCATED",
                "CONTEXT_DECISION_REGISTERED",
                "DECISION_NEED_REGISTERED",
                "RUNTIME_CONTEXT_DECISION_REGISTERED",
            }
        ),
        "remaining_budget": frozenset(
            {
                "BUDGET_SNAPSHOT_RECORDED",
                "CONTEXT_DECISION_REGISTERED",
                "DECISION_NEED_REGISTERED",
                "RUNTIME_CONTEXT_DECISION_REGISTERED",
            }
        ),
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


def _bool(value: object, name: str) -> bool:
    if type(value) is not bool:
        raise TypeError(f"{name} must be an exact boolean")
    return value


def _string_tuple(
    value: object, name: str, *, allow_empty: bool = True
) -> tuple[str, ...]:
    if type(value) is not tuple:
        raise TypeError(f"{name} must be a built-in tuple")
    result = tuple(_text(item, f"{name} item") for item in value)
    if not allow_empty and not result:
        raise ValueError(f"{name} must not be empty")
    if len(set(result)) != len(result):
        raise ValueError(f"{name} must not contain duplicates")
    return result


def _redact_text(value: str, *, maximum: int) -> tuple[str, bool, int]:
    normalized = _WHITESPACE_RE.sub(" ", _text(value, "source text")).strip()
    normalized = _BRACED_TOKEN_RE.sub("[REDACTED_TOKEN]", normalized)
    normalized = _MIXED_BARE_TOKEN_RE.sub("[REDACTED_TOKEN]", normalized)
    if len(normalized) <= maximum:
        return normalized, False, 0
    omitted = len(normalized) - maximum
    return normalized[:maximum] + "…", True, omitted


class ContextSection(str, Enum):
    OBJECTIVE_POLICY = "objective_policy"
    EPOCH_CAPABILITIES = "epoch_capabilities"
    VERIFIED_EVIDENCE = "verified_evidence"
    CONTRADICTIONS = "contradictions"
    HYPOTHESIS_BOUNDARY = "hypothesis_boundary"
    ATTEMPT_HISTORY = "attempt_history"
    EFFECT_BUDGET = "effect_budget"
    ARTIFACT_HANDLES = "artifact_handles"
    OMISSIONS = "omissions"


SECTION_ORDER = (
    ContextSection.OBJECTIVE_POLICY,
    ContextSection.EPOCH_CAPABILITIES,
    ContextSection.VERIFIED_EVIDENCE,
    ContextSection.CONTRADICTIONS,
    ContextSection.HYPOTHESIS_BOUNDARY,
    ContextSection.ATTEMPT_HISTORY,
    ContextSection.EFFECT_BUDGET,
    ContextSection.ARTIFACT_HANDLES,
    ContextSection.OMISSIONS,
)


class SourceTrustLabel(str, Enum):
    CANONICAL_CONTRACT = "canonical_contract"
    CANONICAL_OBSERVATION = "canonical_observation"
    DETERMINISTIC_VERIFIED = "deterministic_verified"
    GATE_VERIFIED = "gate_verified"
    TAINTED_LEAD = "tainted_lead"
    PROPOSAL_ONLY = "proposal_only"


class LossyViewKind(str, Enum):
    TEXT = "text"
    STRING_LIST = "string_list"
    NONNEGATIVE_INT = "nonnegative_int"
    NONNEGATIVE_INT_MAP = "nonnegative_int_map"
    BOOLEAN = "boolean"
    DIGEST = "digest"
    OPAQUE_REFERENCE = "opaque_reference"


class OmissionReason(str, Enum):
    IRRELEVANT_TO_DECISION = "irrelevant_to_decision"
    QUARANTINED = "quarantined"
    REDACTION_POLICY = "redaction_policy"
    SIZE_LIMIT = "size_limit"
    STAGE_UNAVAILABLE = "stage_unavailable"


@dataclass(frozen=True, slots=True)
class PacketFieldBindingV1:
    field_id: str
    section: ContextSection
    trust: SourceTrustLabel
    view_kind: LossyViewKind
    source: ReceiptFieldPointerV1
    critical: bool = False

    def __post_init__(self) -> None:
        object.__setattr__(
            self, "field_id", _text(self.field_id, "field_id", identifier=True)
        )
        if (
            type(self.section) is not ContextSection
            or self.section is ContextSection.OMISSIONS
        ):
            raise ValueError("field binding requires a non-omission context section")
        if type(self.trust) is not SourceTrustLabel:
            raise TypeError("trust must be SourceTrustLabel")
        if type(self.view_kind) is not LossyViewKind:
            raise TypeError("view_kind must be LossyViewKind")
        if type(self.source) is not ReceiptFieldPointerV1:
            raise TypeError("source must be ReceiptFieldPointerV1")
        object.__setattr__(self, "critical", _bool(self.critical, "critical"))
        if self.trust is SourceTrustLabel.TAINTED_LEAD and (
            self.view_kind is not LossyViewKind.OPAQUE_REFERENCE
        ):
            raise ValueError(
                "tainted leads may enter a packet only as opaque references"
            )
        if self.trust is SourceTrustLabel.PROPOSAL_ONLY and (
            self.section is not ContextSection.HYPOTHESIS_BOUNDARY
        ):
            raise ValueError("proposal-only material belongs in hypothesis_boundary")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "critical": self.critical,
            "field_id": self.field_id,
            "section": self.section.value,
            "source": self.source.canonical_body(),
            "trust": self.trust.value,
            "view_kind": self.view_kind.value,
        }


@dataclass(frozen=True, slots=True)
class OmissionDeclarationV1:
    omission_id: str
    reason: OmissionReason
    source_pointers: tuple[ReceiptFieldPointerV1, ...] = ()
    stage_marker: str = ""

    def __post_init__(self) -> None:
        object.__setattr__(
            self, "omission_id", _text(self.omission_id, "omission_id", identifier=True)
        )
        if type(self.reason) is not OmissionReason:
            raise TypeError("reason must be OmissionReason")
        if type(self.source_pointers) is not tuple or any(
            type(item) is not ReceiptFieldPointerV1 for item in self.source_pointers
        ):
            raise TypeError(
                "source_pointers must be a built-in tuple of exact pointers"
            )
        if type(self.stage_marker) is not str:
            raise TypeError("stage_marker must be an exact string")
        pointer_digests = [item.digest for item in self.source_pointers]
        if len(pointer_digests) != len(set(pointer_digests)):
            raise ValueError("omission source pointers contain duplicates")
        if self.reason is OmissionReason.STAGE_UNAVAILABLE:
            if self.source_pointers:
                raise ValueError(
                    "stage-unavailable omission cannot invent source pointers"
                )
            object.__setattr__(
                self, "stage_marker", _text(self.stage_marker, "stage_marker")
            )
        elif not self.source_pointers or self.stage_marker:
            raise ValueError("source omission requires pointers and no stage marker")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "omission_id": self.omission_id,
            "reason": self.reason.value,
            "source_pointers": [item.canonical_body() for item in self.source_pointers],
            "stage_marker": self.stage_marker,
        }


_REQUIRED_BINDINGS: Mapping[str, tuple[ContextSection, LossyViewKind]] = (
    MappingProxyType(
        {
            "acceptance_boundary": (
                ContextSection.OBJECTIVE_POLICY,
                LossyViewKind.TEXT,
            ),
            "decision_need": (
                ContextSection.HYPOTHESIS_BOUNDARY,
                LossyViewKind.TEXT,
            ),
            "effect_ambiguity": (
                ContextSection.EFFECT_BUDGET,
                LossyViewKind.STRING_LIST,
            ),
            "non_negotiable_policy": (
                ContextSection.OBJECTIVE_POLICY,
                LossyViewKind.STRING_LIST,
            ),
            "objective": (ContextSection.OBJECTIVE_POLICY, LossyViewKind.TEXT),
            "preallocated_attempt_id": (
                ContextSection.EPOCH_CAPABILITIES,
                LossyViewKind.TEXT,
            ),
            "remaining_budget": (
                ContextSection.EFFECT_BUDGET,
                LossyViewKind.NONNEGATIVE_INT_MAP,
            ),
        }
    )
)


@dataclass(frozen=True, slots=True)
class ContextPacketBuildRequestV1:
    run_id: str
    decision_id: str
    decision_epoch_id: str
    target_attempt_id: str
    cutoff_seq: int
    cutoff_head_event_digest: str
    expected_index_digest: str
    expected_prefix_digest: str
    bindings: tuple[PacketFieldBindingV1, ...]
    omissions: tuple[OmissionDeclarationV1, ...]
    compiler_version: str = CONTEXT_PACKET_COMPILER_VERSION
    context_policy_id: str = CONTEXT_POLICY_ID
    omission_policy_id: str = OMISSION_POLICY_ID
    redaction_policy_id: str = REDACTION_POLICY_ID

    def __post_init__(self) -> None:
        for name in ("run_id", "decision_id", "decision_epoch_id", "target_attempt_id"):
            object.__setattr__(
                self, name, _text(getattr(self, name), name, identifier=True)
            )
        cutoff = _integer(self.cutoff_seq, "cutoff_seq", minimum=1)
        object.__setattr__(self, "cutoff_seq", cutoff)
        for name in (
            "cutoff_head_event_digest",
            "expected_index_digest",
            "expected_prefix_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if type(self.bindings) is not tuple or any(
            type(item) is not PacketFieldBindingV1 for item in self.bindings
        ):
            raise TypeError("bindings must be a built-in tuple of field bindings")
        if type(self.omissions) is not tuple or any(
            type(item) is not OmissionDeclarationV1 for item in self.omissions
        ):
            raise TypeError("omissions must be a built-in tuple")
        field_ids = [item.field_id for item in self.bindings]
        pointers = [item.source.digest for item in self.bindings]
        omission_ids = [item.omission_id for item in self.omissions]
        omitted_pointers = [
            pointer.digest
            for item in self.omissions
            for pointer in item.source_pointers
        ]
        if len(field_ids) != len(set(field_ids)):
            raise ValueError("packet field IDs must be unique")
        if len(pointers) != len(set(pointers)):
            raise ValueError(
                "one source pointer cannot be duplicated across packet fields"
            )
        if len(omission_ids) != len(set(omission_ids)):
            raise ValueError("omission IDs must be unique")
        if len(omitted_pointers) != len(set(omitted_pointers)):
            raise ValueError("one source pointer cannot be omitted more than once")
        if set(pointers) & set(omitted_pointers):
            raise ValueError("one source pointer cannot be both included and omitted")
        stage_omissions = [
            item
            for item in self.omissions
            if item.reason is OmissionReason.STAGE_UNAVAILABLE
            and item.stage_marker == NOT_AVAILABLE_H5
        ]
        if len(stage_omissions) != 1:
            raise ValueError("pre-H5 packet requires exactly one explicit H5 omission")
        for field_id, (section, view_kind) in _REQUIRED_BINDINGS.items():
            matches = [item for item in self.bindings if item.field_id == field_id]
            if len(matches) != 1:
                raise ValueError(f"packet requires exactly one {field_id} binding")
            binding = matches[0]
            if (
                binding.section is not section
                or binding.view_kind is not view_kind
                or binding.trust is not SourceTrustLabel.CANONICAL_CONTRACT
                or not binding.critical
            ):
                raise ValueError(f"required binding {field_id} has an unsafe type")
        if self.compiler_version != CONTEXT_PACKET_COMPILER_VERSION:
            raise ValueError("unsupported compiler version")
        if self.context_policy_id != CONTEXT_POLICY_ID:
            raise ValueError("unsupported context policy")
        if self.omission_policy_id != OMISSION_POLICY_ID:
            raise ValueError("unsupported omission policy")
        if self.redaction_policy_id != REDACTION_POLICY_ID:
            raise ValueError("unsupported redaction policy")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "bindings": [item.canonical_body() for item in self.bindings],
            "compiler_version": self.compiler_version,
            "context_policy_id": self.context_policy_id,
            "cutoff_head_event_digest": self.cutoff_head_event_digest,
            "cutoff_seq": self.cutoff_seq,
            "decision_epoch_id": self.decision_epoch_id,
            "decision_id": self.decision_id,
            "expected_index_digest": self.expected_index_digest,
            "expected_prefix_digest": self.expected_prefix_digest,
            "omission_policy_id": self.omission_policy_id,
            "omissions": [item.canonical_body() for item in self.omissions],
            "redaction_policy_id": self.redaction_policy_id,
            "run_id": self.run_id,
            "target_attempt_id": self.target_attempt_id,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ContextPacketAtomV1:
    field_id: str
    section: ContextSection
    trust: SourceTrustLabel
    view_kind: LossyViewKind
    source: ReceiptFieldPointerV1
    source_event_kind: str
    value: FrozenJSON
    truncated: bool
    omitted_units: int

    def __post_init__(self) -> None:
        object.__setattr__(
            self, "field_id", _text(self.field_id, "field_id", identifier=True)
        )
        if (
            type(self.section) is not ContextSection
            or self.section is ContextSection.OMISSIONS
        ):
            raise ValueError("atom requires a non-omission section")
        if type(self.trust) is not SourceTrustLabel:
            raise TypeError("atom trust must be SourceTrustLabel")
        if type(self.view_kind) is not LossyViewKind:
            raise TypeError("atom view_kind must be LossyViewKind")
        if type(self.source) is not ReceiptFieldPointerV1:
            raise TypeError("atom source must be an exact receipt pointer")
        if type(self.source_event_kind) is not str:
            raise TypeError("source_event_kind must be an exact string")
        if self.source_event_kind:
            object.__setattr__(
                self,
                "source_event_kind",
                _text(self.source_event_kind, "source_event_kind", identifier=True),
            )
        object.__setattr__(self, "value", freeze_json(self.value, path="$.atom.value"))
        object.__setattr__(self, "truncated", _bool(self.truncated, "truncated"))
        object.__setattr__(
            self, "omitted_units", _integer(self.omitted_units, "omitted_units")
        )
        if self.truncated != (self.omitted_units > 0):
            raise ValueError("truncation marker and omitted unit count disagree")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "field_id": self.field_id,
            "omitted_units": self.omitted_units,
            "section": self.section.value,
            "source": self.source.canonical_body(),
            "source_event_kind": self.source_event_kind,
            "trust": self.trust.value,
            "truncated": self.truncated,
            "value": self.value,
            "view_kind": self.view_kind.value,
        }


@dataclass(frozen=True, slots=True)
class ContextPacketSectionV1:
    section: ContextSection
    items: tuple[ContextPacketAtomV1, ...] = ()
    omissions: tuple[OmissionDeclarationV1, ...] = ()
    stage_marker: str = ""

    def __post_init__(self) -> None:
        if type(self.section) is not ContextSection:
            raise TypeError("section must be ContextSection")
        if type(self.items) is not tuple or any(
            type(item) is not ContextPacketAtomV1 for item in self.items
        ):
            raise TypeError("items must be a built-in tuple of atoms")
        if type(self.omissions) is not tuple or any(
            type(item) is not OmissionDeclarationV1 for item in self.omissions
        ):
            raise TypeError("omissions must be a built-in tuple")
        if any(item.section is not self.section for item in self.items):
            raise ValueError("section contains an atom for another section")
        if type(self.stage_marker) is not str:
            raise TypeError("stage_marker must be an exact string")
        if self.section is ContextSection.OMISSIONS:
            if self.items or not self.omissions:
                raise ValueError("omission section requires declarations and no atoms")
        elif self.omissions:
            raise ValueError("only the omission section may carry declarations")
        if self.section is ContextSection.HYPOTHESIS_BOUNDARY:
            if self.stage_marker != NOT_AVAILABLE_H5:
                raise ValueError("pre-H5 packet requires an explicit H5 marker")
        elif self.stage_marker:
            raise ValueError("only hypothesis_boundary may carry a stage marker")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "items": [item.canonical_body() for item in self.items],
            "omissions": [item.canonical_body() for item in self.omissions],
            "section": self.section.value,
            "stage_marker": self.stage_marker,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ContextPacketManifestV1:
    run_id: str
    decision_id: str
    decision_epoch_id: str
    target_attempt_id: str
    cutoff_seq: int
    cutoff_head_event_digest: str
    verified_prefix_digest: str
    receipt_index_digest: str
    build_request_digest: str
    section_digests: tuple[str, ...]
    included_sources: tuple[ReceiptFieldPointerV1, ...]
    omitted_sources: tuple[ReceiptFieldPointerV1, ...]
    compiler_version: str = CONTEXT_PACKET_COMPILER_VERSION
    context_policy_id: str = CONTEXT_POLICY_ID
    omission_policy_id: str = OMISSION_POLICY_ID
    redaction_policy_id: str = REDACTION_POLICY_ID
    schema_id: str = CONTEXT_PACKET_SCHEMA_ID
    accepted_set_change: bool = False

    def __post_init__(self) -> None:
        for name in ("run_id", "decision_id", "decision_epoch_id", "target_attempt_id"):
            object.__setattr__(
                self, name, _text(getattr(self, name), name, identifier=True)
            )
        object.__setattr__(
            self, "cutoff_seq", _integer(self.cutoff_seq, "cutoff_seq", minimum=1)
        )
        for name in (
            "cutoff_head_event_digest",
            "verified_prefix_digest",
            "receipt_index_digest",
            "build_request_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if type(self.section_digests) is not tuple or len(self.section_digests) != len(
            SECTION_ORDER
        ):
            raise ValueError("manifest requires one digest for every fixed section")
        for digest in self.section_digests:
            _digest(digest, "section_digest")
        for name in ("included_sources", "omitted_sources"):
            value = getattr(self, name)
            if type(value) is not tuple or any(
                type(item) is not ReceiptFieldPointerV1 for item in value
            ):
                raise TypeError(f"{name} must be a tuple of exact receipt pointers")
        included = [item.digest for item in self.included_sources]
        omitted = [item.digest for item in self.omitted_sources]
        if (
            len(included) != len(set(included))
            or len(omitted) != len(set(omitted))
            or set(included) & set(omitted)
        ):
            raise ValueError("manifest source sets are duplicated or overlap")
        if self.compiler_version != CONTEXT_PACKET_COMPILER_VERSION:
            raise ValueError("unsupported compiler version")
        if self.context_policy_id != CONTEXT_POLICY_ID:
            raise ValueError("unsupported context policy")
        if self.omission_policy_id != OMISSION_POLICY_ID:
            raise ValueError("unsupported omission policy")
        if self.redaction_policy_id != REDACTION_POLICY_ID:
            raise ValueError("unsupported redaction policy")
        if self.schema_id != CONTEXT_PACKET_SCHEMA_ID:
            raise ValueError("unsupported context packet schema")
        if self.accepted_set_change is not ACCEPTED_SET_CHANGE:
            raise ValueError("ContextPacket cannot change the gate accepted set")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": self.accepted_set_change,
            "build_request_digest": self.build_request_digest,
            "compiler_version": self.compiler_version,
            "context_policy_id": self.context_policy_id,
            "cutoff_head_event_digest": self.cutoff_head_event_digest,
            "cutoff_seq": self.cutoff_seq,
            "decision_epoch_id": self.decision_epoch_id,
            "decision_id": self.decision_id,
            "included_sources": [
                item.canonical_body() for item in self.included_sources
            ],
            "omission_policy_id": self.omission_policy_id,
            "omitted_sources": [item.canonical_body() for item in self.omitted_sources],
            "receipt_index_digest": self.receipt_index_digest,
            "redaction_policy_id": self.redaction_policy_id,
            "run_id": self.run_id,
            "schema_id": self.schema_id,
            "section_digests": list(self.section_digests),
            "target_attempt_id": self.target_attempt_id,
            "verified_prefix_digest": self.verified_prefix_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ContextPacketViewV1:
    manifest_digest: str
    sections: tuple[ContextPacketSectionV1, ...]

    def __post_init__(self) -> None:
        object.__setattr__(
            self, "manifest_digest", _digest(self.manifest_digest, "manifest_digest")
        )
        if type(self.sections) is not tuple or any(
            type(item) is not ContextPacketSectionV1 for item in self.sections
        ):
            raise TypeError("sections must be a built-in tuple")
        if tuple(item.section for item in self.sections) != SECTION_ORDER:
            raise ValueError("context packet sections must use the fixed order")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "manifest_digest": self.manifest_digest,
            "sections": [item.canonical_body() for item in self.sections],
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class SealedContextPacketV1:
    manifest: ContextPacketManifestV1
    view: ContextPacketViewV1
    sealed: SealedObject

    def __post_init__(self) -> None:
        if type(self.manifest) is not ContextPacketManifestV1:
            raise TypeError("manifest must be ContextPacketManifestV1")
        if type(self.view) is not ContextPacketViewV1:
            raise TypeError("view must be ContextPacketViewV1")
        if type(self.sealed) is not SealedObject:
            raise TypeError("sealed must be SealedObject")
        if self.view.manifest_digest != self.manifest.digest:
            raise ValueError("lossy view is rebound to another manifest")
        section_digests = tuple(item.digest for item in self.view.sections)
        if section_digests != self.manifest.section_digests:
            raise ValueError("manifest section digests differ from the lossy view")
        if self.sealed.digest != self.digest or self.sealed.byte_count != len(
            self.bytes
        ):
            raise ValueError("sealed object does not bind the complete context packet")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "manifest": self.manifest.canonical_body(),
            "view": self.view.canonical_body(),
        }

    @property
    def bytes(self) -> bytes:
        return canonical_json_bytes(self.canonical_body())

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def _validate_source_semantics(
    binding: PacketFieldBindingV1, resolved: ResolvedReceiptFieldV1
) -> None:
    if resolved.event_kind in _FORBIDDEN_RETROSPECTIVE_EVENT_KINDS:
        raise ValueError("retrospective adjudication cannot enter a prospective packet")
    path_words = {
        item.casefold() for item in _PATH_WORD_RE.findall(binding.source.field_path)
    }
    hindsight = path_words & _HINDSIGHT_FIELD_NAMES
    if hindsight:
        prior_attempt_ok = (
            binding.section is ContextSection.ATTEMPT_HISTORY
            and resolved.event_kind in _PRIOR_ATTEMPT_EVENT_KINDS
            and hindsight <= _PRIOR_ATTEMPT_OUTCOME_FIELDS
        )
        opaque_gate_ok = (
            binding.section is ContextSection.VERIFIED_EVIDENCE
            and resolved.event_kind in {"FLAG_ACCEPTED", "FLAG_REJECTED"}
            and binding.view_kind is LossyViewKind.OPAQUE_REFERENCE
        )
        if not prior_attempt_ok and not opaque_gate_ok:
            raise ValueError(
                "hindsight-labelled source field is forbidden prospectively"
            )
    if binding.trust is SourceTrustLabel.GATE_VERIFIED:
        if (
            resolved.event_kind not in {"FLAG_ACCEPTED", "FLAG_REJECTED"}
            or binding.view_kind is not LossyViewKind.OPAQUE_REFERENCE
        ):
            raise ValueError("gate evidence must remain an opaque exact gate reference")
    allowed_contract_kinds = _REQUIRED_FIELD_EVENT_KINDS.get(binding.field_id)
    if (
        binding.trust is SourceTrustLabel.CANONICAL_CONTRACT
        and allowed_contract_kinds is None
    ):
        raise ValueError(
            "canonical contract trust requires a registered field authority"
        )
    if allowed_contract_kinds is not None and (
        resolved.event_kind not in allowed_contract_kinds
    ):
        raise ValueError(
            "required context contract has an incompatible event authority"
        )
    if (
        binding.trust is SourceTrustLabel.CANONICAL_OBSERVATION
        and resolved.event_kind not in _CANONICAL_OBSERVATION_EVENT_KINDS
    ):
        raise ValueError("canonical observation trust lacks an observation authority")
    if binding.trust is SourceTrustLabel.DETERMINISTIC_VERIFIED:
        raise ValueError(
            "deterministic verification trust is unavailable until an "
            "evaluator-owned verification authority is receipt-resolved"
        )
    if (
        binding.trust is SourceTrustLabel.PROPOSAL_ONLY
        and resolved.event_kind not in _PROPOSAL_EVENT_KINDS
    ):
        raise ValueError("proposal-only trust lacks a proposal event authority")
    if binding.section is ContextSection.VERIFIED_EVIDENCE and binding.trust not in {
        SourceTrustLabel.DETERMINISTIC_VERIFIED,
        SourceTrustLabel.GATE_VERIFIED,
        SourceTrustLabel.TAINTED_LEAD,
    }:
        raise ValueError("verified_evidence section has an incompatible trust label")


def _lossy_value(
    binding: PacketFieldBindingV1, resolved: ResolvedReceiptFieldV1
) -> tuple[FrozenJSON, bool, int]:
    value = resolved.value
    if binding.view_kind is LossyViewKind.OPAQUE_REFERENCE:
        opaque = {
            "source_pointer_digest": binding.source.digest,
            "source_value_digest": binding.source.value_digest,
        }
        return freeze_json(opaque), False, 0
    if binding.view_kind is LossyViewKind.TEXT:
        if type(value) is not str:
            raise TypeError(f"{binding.field_id} must resolve to text")
        text, truncated, omitted = _redact_text(value, maximum=_MAX_TEXT_CHARS)
        return text, truncated, omitted
    if binding.view_kind is LossyViewKind.STRING_LIST:
        if type(value) is not tuple or any(type(item) is not str for item in value):
            raise TypeError(f"{binding.field_id} must resolve to a string list")
        visible = value[:_MAX_LIST_ITEMS]
        rendered: list[str] = []
        omitted_units = max(0, len(value) - len(visible))
        for item in visible:
            text, truncated, omitted = _redact_text(item, maximum=_MAX_LIST_ITEM_CHARS)
            rendered.append(text)
            omitted_units += omitted
        return tuple(rendered), omitted_units > 0, omitted_units
    if binding.view_kind is LossyViewKind.NONNEGATIVE_INT:
        if type(value) is not int or value < 0:
            raise TypeError(f"{binding.field_id} must be a non-negative integer")
        return value, False, 0
    if binding.view_kind is LossyViewKind.NONNEGATIVE_INT_MAP:
        if not isinstance(value, Mapping) or not value:
            raise TypeError(f"{binding.field_id} must be a non-empty integer map")
        result: dict[str, int] = {}
        for key, amount in value.items():
            canonical_key = _text(key, f"{binding.field_id} key", identifier=True)
            if type(amount) is not int or amount < 0:
                raise TypeError(
                    f"{binding.field_id}[{canonical_key}] must be non-negative"
                )
            result[canonical_key] = amount
        return freeze_json(result), False, 0
    if binding.view_kind is LossyViewKind.BOOLEAN:
        if type(value) is not bool:
            raise TypeError(f"{binding.field_id} must be an exact boolean")
        return value, False, 0
    if binding.view_kind is LossyViewKind.DIGEST:
        return _digest(value, binding.field_id), False, 0
    raise TypeError("unsupported lossy view kind")


class ContextPacketCompilerV1:
    """Deterministic receipt-only compiler for the production-disabled C6 slice."""

    def compile(
        self,
        request: ContextPacketBuildRequestV1,
        *,
        resolver: ReceiptFieldResolverV1,
        cas: ReceiptCAS,
    ) -> SealedContextPacketV1:
        if type(request) is not ContextPacketBuildRequestV1:
            raise TypeError("request must be ContextPacketBuildRequestV1")
        if not isinstance(resolver, ReceiptFieldResolverV1):
            raise TypeError(
                "resolver must satisfy the narrow ReceiptFieldResolverV1 port"
            )
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")

        prefix = resolver.verify_complete_through(request.cutoff_seq)
        self._validate_prefix(request, prefix, resolver)

        by_section: dict[ContextSection, list[ContextPacketAtomV1]] = defaultdict(list)
        included_sources: list[ReceiptFieldPointerV1] = []
        for binding in request.bindings:
            resolved = resolver.resolve(binding.source, cutoff_seq=request.cutoff_seq)
            _validate_source_semantics(binding, resolved)
            if (
                binding.field_id == "preallocated_attempt_id"
                and resolved.value != request.target_attempt_id
            ):
                raise ValueError("target attempt was rebound after preallocation")
            value, truncated, omitted_units = _lossy_value(binding, resolved)
            by_section[binding.section].append(
                ContextPacketAtomV1(
                    field_id=binding.field_id,
                    section=binding.section,
                    trust=binding.trust,
                    view_kind=binding.view_kind,
                    source=binding.source,
                    source_event_kind=resolved.event_kind,
                    value=value,
                    truncated=truncated,
                    omitted_units=omitted_units,
                )
            )
            included_sources.append(binding.source)

        omitted_sources: list[ReceiptFieldPointerV1] = []
        for omission in request.omissions:
            for pointer in omission.source_pointers:
                resolved = resolver.resolve(pointer, cutoff_seq=request.cutoff_seq)
                path_words = {
                    item.casefold()
                    for item in _PATH_WORD_RE.findall(pointer.field_path)
                }
                if (
                    resolved.event_kind in _FORBIDDEN_RETROSPECTIVE_EVENT_KINDS
                    or path_words & _HINDSIGHT_FIELD_NAMES
                ):
                    raise ValueError(
                        "hindsight cannot be smuggled through omission metadata"
                    )
                omitted_sources.append(pointer)

        post_resolution_prefix = resolver.verify_complete_through(request.cutoff_seq)
        self._validate_prefix(request, post_resolution_prefix, resolver)
        if post_resolution_prefix.digest != prefix.digest:
            raise ValueError("receipt prefix changed while the packet was compiled")

        sections: list[ContextPacketSectionV1] = []
        for section in SECTION_ORDER:
            if section is ContextSection.OMISSIONS:
                sections.append(
                    ContextPacketSectionV1(
                        section=section,
                        omissions=request.omissions,
                    )
                )
                continue
            items = tuple(sorted(by_section[section], key=lambda item: item.field_id))
            sections.append(
                ContextPacketSectionV1(
                    section=section,
                    items=items,
                    stage_marker=(
                        NOT_AVAILABLE_H5
                        if section is ContextSection.HYPOTHESIS_BOUNDARY
                        else ""
                    ),
                )
            )

        section_tuple = tuple(sections)
        manifest = ContextPacketManifestV1(
            run_id=request.run_id,
            decision_id=request.decision_id,
            decision_epoch_id=request.decision_epoch_id,
            target_attempt_id=request.target_attempt_id,
            cutoff_seq=request.cutoff_seq,
            cutoff_head_event_digest=request.cutoff_head_event_digest,
            verified_prefix_digest=prefix.digest,
            receipt_index_digest=prefix.index_digest,
            build_request_digest=request.digest,
            section_digests=tuple(item.digest for item in section_tuple),
            included_sources=tuple(included_sources),
            omitted_sources=tuple(omitted_sources),
        )
        view = ContextPacketViewV1(
            manifest_digest=manifest.digest,
            sections=section_tuple,
        )
        body = {
            "manifest": manifest.canonical_body(),
            "view": view.canonical_body(),
        }
        sealed = cas.seal_bytes(canonical_json_bytes(body))
        return SealedContextPacketV1(manifest=manifest, view=view, sealed=sealed)

    @staticmethod
    def _validate_prefix(
        request: ContextPacketBuildRequestV1,
        prefix: VerifiedReceiptPrefixV1,
        resolver: ReceiptFieldResolverV1,
    ) -> None:
        exact = (
            prefix.run_id == request.run_id,
            prefix.cutoff_seq == request.cutoff_seq,
            prefix.head_event_digest == request.cutoff_head_event_digest,
            prefix.index_digest == request.expected_index_digest,
            prefix.digest == request.expected_prefix_digest,
            resolver.index.run_id == request.run_id,
            resolver.index.digest == request.expected_index_digest,
        )
        if not all(exact):
            raise ValueError("receipt prefix was rebound after the build request froze")
        if any(
            event.attempt_id == request.target_attempt_id
            and event.kind in _TARGET_TERMINAL_EVENT_KINDS
            for event in prefix.events
        ):
            raise ValueError("target attempt already has hindsight terminal evidence")


class C6TerminalEvidenceStatus(str, Enum):
    INSUFFICIENT_EVIDENCE = INSUFFICIENT_EVIDENCE


@dataclass(frozen=True, slots=True)
class C6TerminalOutcomeRecordV1:
    fixture_id: str
    arm_id: str
    attempt_id: str
    packet_digest: str
    assignment: ReceiptFieldPointerV1
    outcome: ReceiptFieldPointerV1
    complete_accounting: ReceiptFieldPointerV1

    def __post_init__(self) -> None:
        for name in ("fixture_id", "arm_id", "attempt_id"):
            object.__setattr__(
                self, name, _text(getattr(self, name), name, identifier=True)
            )
        object.__setattr__(
            self, "packet_digest", _digest(self.packet_digest, "packet_digest")
        )
        if (
            type(self.assignment) is not ReceiptFieldPointerV1
            or type(self.outcome) is not ReceiptFieldPointerV1
            or type(self.complete_accounting) is not ReceiptFieldPointerV1
        ):
            raise TypeError("terminal record requires exact receipt pointers")
        if (
            len(
                {
                    self.assignment.run_id,
                    self.outcome.run_id,
                    self.complete_accounting.run_id,
                }
            )
            != 1
        ):
            raise ValueError(
                "terminal assignment, outcome, and accounting belong to different runs"
            )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "arm_id": self.arm_id,
            "assignment": self.assignment.canonical_body(),
            "attempt_id": self.attempt_id,
            "complete_accounting": self.complete_accounting.canonical_body(),
            "fixture_id": self.fixture_id,
            "outcome": self.outcome.canonical_body(),
            "packet_digest": self.packet_digest,
        }


@dataclass(frozen=True, slots=True)
class C6TerminalEvidenceAssessmentV1:
    status: C6TerminalEvidenceStatus
    record_count: int
    record_set_digest: str
    terminal_receipt_digests: tuple[str, ...]
    architecture_selection_supported: bool = False
    benefit_claim_supported: bool = False
    accepted_set_change: bool = False

    def __post_init__(self) -> None:
        if type(self.status) is not C6TerminalEvidenceStatus:
            raise TypeError("status must be C6TerminalEvidenceStatus")
        count = _integer(self.record_count, "record_count")
        object.__setattr__(self, "record_count", count)
        object.__setattr__(
            self,
            "record_set_digest",
            _digest(self.record_set_digest, "record_set_digest"),
        )
        if type(self.terminal_receipt_digests) is not tuple:
            raise TypeError("terminal_receipt_digests must be a built-in tuple")
        for digest in self.terminal_receipt_digests:
            _digest(digest, "terminal_receipt_digest")
        if len(self.terminal_receipt_digests) != len(
            set(self.terminal_receipt_digests)
        ):
            raise ValueError("terminal receipt digests contain duplicates")
        flags = (
            self.architecture_selection_supported,
            self.benefit_claim_supported,
            self.accepted_set_change,
        )
        if any(type(item) is not bool for item in flags) or any(flags):
            raise ValueError(
                "terminal presence alone cannot claim selection, benefit, or gate change"
            )
        if self.status is not C6TerminalEvidenceStatus.INSUFFICIENT_EVIDENCE:
            raise ValueError(
                "C6 cannot leave INSUFFICIENT_EVIDENCE without evaluator authority"
            )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": self.accepted_set_change,
            "architecture_selection_supported": self.architecture_selection_supported,
            "benefit_claim_supported": self.benefit_claim_supported,
            "record_count": self.record_count,
            "record_set_digest": self.record_set_digest,
            "status": self.status.value,
            "terminal_receipt_digests": list(self.terminal_receipt_digests),
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def assess_c6_terminal_evidence(
    records: tuple[C6TerminalOutcomeRecordV1, ...],
    *,
    resolver: ReceiptFieldResolverV1,
) -> C6TerminalEvidenceAssessmentV1:
    """Validate terminal receipt presence without making an architecture claim."""

    if type(records) is not tuple or any(
        type(item) is not C6TerminalOutcomeRecordV1 for item in records
    ):
        raise TypeError("records must be a built-in tuple of terminal records")
    if not isinstance(resolver, ReceiptFieldResolverV1):
        raise TypeError("resolver must satisfy ReceiptFieldResolverV1")
    record_bodies = [item.canonical_body() for item in records]
    if not records:
        return C6TerminalEvidenceAssessmentV1(
            status=C6TerminalEvidenceStatus.INSUFFICIENT_EVIDENCE,
            record_count=0,
            record_set_digest=canonical_digest(record_bodies),
            terminal_receipt_digests=(),
        )

    resolver.verify_complete_through(resolver.index.complete_through_seq)
    identities: set[tuple[str, str]] = set()
    receipt_digests: set[str] = set()
    for record in records:
        identity = (record.fixture_id, record.arm_id)
        if identity in identities:
            raise ValueError("terminal study has duplicate fixture/arm identity")
        identities.add(identity)
        assignment = resolver.resolve(record.assignment)
        outcome = resolver.resolve(record.outcome)
        accounting = resolver.resolve(record.complete_accounting)
        if assignment.event_kind != "C6_TERMINAL_TRIAL_ASSIGNED":
            raise ValueError("terminal record lacks a canonical trial assignment")
        if outcome.event_kind not in _TERMINAL_OUTCOME_EVENT_KINDS:
            raise ValueError("outcome pointer is not a canonical terminal event")
        if accounting.event_kind != "BUDGET_SETTLED":
            raise ValueError("terminal record lacks complete-accounted BUDGET_SETTLED")
        if not isinstance(assignment.value, Mapping) or set(assignment.value) != {
            "arm_id",
            "attempt_id",
            "fixture_id",
            "packet_digest",
        }:
            raise ValueError("terminal trial assignment payload is malformed")
        expected_assignment = {
            "arm_id": record.arm_id,
            "attempt_id": record.attempt_id,
            "fixture_id": record.fixture_id,
            "packet_digest": record.packet_digest,
        }
        if canonical_json_bytes(assignment.value) != canonical_json_bytes(
            expected_assignment
        ):
            raise ValueError("terminal trial assignment was rebound")
        if not isinstance(outcome.value, Mapping):
            raise ValueError(
                "terminal outcome pointer must resolve to its event payload"
            )
        if outcome.value.get("attempt_id") != record.attempt_id or outcome.value.get(
            "outcome"
        ) in {None, "unknown"}:
            raise ValueError("terminal outcome attempt is missing, UNKNOWN, or rebound")
        if not isinstance(accounting.value, Mapping):
            raise ValueError(
                "complete accounting pointer must resolve to its event payload"
            )
        if accounting.value.get("attempt_id") != record.attempt_id:
            raise ValueError("terminal accounting attempt was rebound")
        actual_usage = accounting.value.get("actual_usage")
        if (
            not isinstance(actual_usage, Mapping)
            or not actual_usage
            or any(
                type(amount) is not int or amount < 0
                for amount in actual_usage.values()
            )
        ):
            raise ValueError("complete accounting map is malformed")
        receipt_digests.add(record.assignment.receipt_digest)
        receipt_digests.add(record.outcome.receipt_digest)
        receipt_digests.add(record.complete_accounting.receipt_digest)

    return C6TerminalEvidenceAssessmentV1(
        status=C6TerminalEvidenceStatus.INSUFFICIENT_EVIDENCE,
        record_count=len(records),
        record_set_digest=canonical_digest(record_bodies),
        terminal_receipt_digests=tuple(sorted(receipt_digests)),
    )
