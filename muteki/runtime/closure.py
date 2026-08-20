"""Read-only semantic resolution of the S4-E production closure.

The live session's ``S4E_CLOSURE_ATTESTED`` event is a declaration, not an
authority shortcut.  This module resolves every declared digest back to the
canonical event log, runtime projections, and verifier-owned CAS.  It never
writes state and fails closed on any ambiguity, UNKNOWN state, or split
lineage.
"""

from __future__ import annotations

import json
import sqlite3
from collections import defaultdict
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from muteki.epistemic.cas import CASIntegrityError, ReceiptCAS
from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.epistemic.sqlite_store import (
    EpistemicSQLiteStore,
    FlagAcceptedOutboxV1,
    IntegrityError,
)


class ClosureResolutionError(RuntimeError):
    """Canonical S4-E evidence does not prove one clean solved execution."""


@dataclass(frozen=True, slots=True)
class ResolvedS4EClosure:
    """Small, authority-safe result returned only after full resolution."""

    scope_digest: str
    closure_receipt_digest: str
    gate_receipt_digest: str
    policy_digest: str
    projection_digest: str
    attempt_count: int
    capture_count: int
    accepted_goal_units: int


_REQUIRED_RECEIPTS = frozenset(
    {
        "canonical_permit",
        "capture_manifest",
        "execution",
        "gate",
        "gate_input",
        "orphan_summary",
        "projection_rebuild",
        "s4e_closure",
        "s4e_schema",
        "usage_closure",
    }
)

_SCHEMA = {
    "name": "muteki-s4e-closure",
    "version": 1,
    "required_components": [
        "canonical_permit",
        "capture_manifest",
        "gate_input",
        "orphan_summary",
        "usage_closure",
    ],
}

_ADMISSION_FIELDS = frozenset(
    {
        "account_id",
        "attempt_digest",
        "attempt_id",
        "branch_id",
        "conflict_keys",
        "effect_class",
        "expires_at_ns",
        "fingerprint",
        "launch_ordinal",
        "lease_digest",
        "lease_epoch",
        "lease_id",
        "permit",
        "permit_digest",
        "permit_id",
        "policy_digest",
        "requested_budget",
        "reservation_ids",
        "scope_digest",
        "worker_generation",
    }
)

_PERMIT_FIELDS = frozenset(
    {
        "constraints",
        "effect_class",
        "expires_at_ns",
        "lease_digest",
        "permit_id",
        "policy_digest",
        "reservation_ids",
    }
)

_LAUNCH_FIELDS = frozenset(
    {
        "admission_event_digest",
        "attempt_digest",
        "attempt_id",
        "launch_ordinal",
        "lease_digest",
        "lease_id",
        "permit_digest",
        "permit_id",
        "reservation_ids",
        "scope_digest",
    }
)

_TERMINAL_FIELDS = frozenset(
    {
        "admission_event_digest",
        "attempt_digest",
        "attempt_id",
        "launch_event_digest",
        "lease_digest",
        "lease_id",
        "outcome",
        "permit_digest",
        "permit_id",
        "scope_digest",
    }
)

_CAPTURE_BODY_FIELDS = (
    "attempt_digest",
    "byte_count",
    "candidate_id",
    "capture_id",
    "flag_digest",
    "flag_format_digest",
    "lease_digest",
    "ordinal",
    "permit_digest",
    "policy_digest",
    "previous_manifest_digest",
    "raw_digest",
    "stream",
    "terminal",
)

_CAPTURE_FIELDS = frozenset((*_CAPTURE_BODY_FIELDS, "manifest_digest"))

_GATE_FIELDS = frozenset(
    {
        "accepted",
        "attempt_digest",
        "candidate_id",
        "capture_event_digest",
        "evaluation_id",
        "flag_digest",
        "flag_format_digest",
        "lease_digest",
        "manifest_digest",
        "permit_digest",
        "policy_digest",
        "raw_digest",
        "snapshot_digest",
    }
)


def _fail(message: str) -> None:
    raise ClosureResolutionError(message)


def _mapping(value: Any, name: str) -> dict[str, Any]:
    if not isinstance(value, Mapping):
        _fail(f"{name} must be a mapping")
    return dict(value)


def _exact_fields(value: Mapping[str, Any], fields: frozenset[str], name: str) -> None:
    if set(value) != fields:
        _fail(f"{name} has an unversioned or incomplete shape")


def _text(value: Any, name: str, *, allow_empty: bool = False) -> str:
    if (
        type(value) is not str
        or value != value.strip()
        or (not value and not allow_empty)
    ):
        _fail(f"{name} must be a canonical string")
    return value


def _sha256(value: Any, name: str, *, allow_empty: bool = False) -> str:
    text = _text(value, name, allow_empty=allow_empty)
    if allow_empty and not text:
        return text
    if len(text) != 64 or any(
        character not in "0123456789abcdef" for character in text
    ):
        _fail(f"{name} must be an exact lowercase sha256")
    return text


def _exact_int(value: Any, name: str, *, minimum: int = 0) -> int:
    if type(value) is not int or value < minimum:
        _fail(f"{name} must be an exact integer >= {minimum}")
    return value


def _exact_bool(value: Any, name: str) -> bool:
    if type(value) is not bool:
        _fail(f"{name} must be an exact boolean")
    return value


def _sequence(value: Any, name: str) -> list[Any]:
    if type(value) not in {list, tuple}:
        _fail(f"{name} must be a canonical sequence")
    return list(value)


def _canonical_strings(
    value: Any, name: str, *, allow_empty: bool = False
) -> list[str]:
    result = [
        _text(item, f"{name} item", allow_empty=allow_empty)
        for item in _sequence(value, name)
    ]
    if len(set(result)) != len(result):
        _fail(f"{name} must not contain duplicates")
    return result


def _nonnegative_int_map(value: Any, name: str) -> dict[str, int]:
    raw = _mapping(value, name)
    if not raw:
        _fail(f"{name} must not be empty")
    result: dict[str, int] = {}
    for axis, amount in raw.items():
        result[_text(axis, f"{name} axis")] = _exact_int(amount, f"{name}[{axis}]")
    return result


def _same_json(left: Any, right: Any) -> bool:
    try:
        return canonical_json_bytes(left) == canonical_json_bytes(right)
    except (TypeError, ValueError):
        return False


def _receipt_for(store: EpistemicSQLiteStore, row: Mapping[str, Any]) -> str:
    digest = _sha256(row.get("event_digest"), "canonical event digest")
    try:
        receipt_digest = _sha256(
            store.receipt_digest_for_event(digest), "canonical command receipt"
        )
        return store.resolve_receipt(receipt_digest).digest
    except (IntegrityError, KeyError) as exc:
        raise ClosureResolutionError(
            "canonical event has no complete committing command receipt"
        ) from exc


def _canonical_event_rows(store: EpistemicSQLiteStore) -> list[dict[str, Any]]:
    """Join the narrow public event view to immutable authority metadata."""

    public_rows = list(store.event_rows())
    metadata_rows = store._conn.execute(
        "SELECT e.seq,e.actor,e.command_id,e.ordinal,e.occurred_at_ns,"
        "c.event_count,c.first_seq,c.last_seq,c.payload_digest,c.receipt_digest,"
        "c.committed_at_ns,c.event_set_digest,c.outbox_set_digest "
        "FROM events e JOIN commands c "
        "ON c.command_id=e.command_id ORDER BY e.seq"
    ).fetchall()
    if len(public_rows) != len(metadata_rows):
        _fail("canonical event metadata cardinality diverges")
    result: list[dict[str, Any]] = []
    for public, metadata in zip(public_rows, metadata_rows, strict=True):
        if public["seq"] != metadata[0]:
            _fail("canonical event metadata sequence diverges")
        result.append(
            {
                **public,
                "_actor": metadata[1],
                "_command_id": metadata[2],
                "_ordinal": metadata[3],
                "_occurred_at_ns": metadata[4],
                "_command_event_count": metadata[5],
                "_command_first_seq": metadata[6],
                "_command_last_seq": metadata[7],
                "_command_payload_digest": metadata[8],
                "_command_receipt_digest": metadata[9],
                "_committed_at_ns": metadata[10],
                "_event_set_digest": metadata[11],
                "_outbox_set_digest": metadata[12],
            }
        )
    return result


def _require_authority_event(
    store: EpistemicSQLiteStore,
    row: Mapping[str, Any],
    *,
    actor: str,
    command_id: str,
    event_id: str,
    ordinal: int = 0,
    event_count: int = 1,
    first_seq: int | None = None,
    last_seq: int | None = None,
    command_payload: Mapping[str, Any] | None = None,
) -> None:
    """Pin authority identity and the immutable command receipt boundary."""

    expected_first = row["seq"] if first_seq is None else first_seq
    expected_last = row["seq"] if last_seq is None else last_seq
    exact = (
        row.get("_actor") == actor,
        row.get("_command_id") == command_id,
        row.get("event_id") == event_id,
        type(row.get("_ordinal")) is int and row.get("_ordinal") == ordinal,
        type(row.get("_command_event_count")) is int
        and row.get("_command_event_count") == event_count,
        row.get("_command_first_seq") == expected_first,
        row.get("_command_last_seq") == expected_last,
    )
    if not all(exact):
        _fail(f"{row.get('kind', 'event')} has false authority provenance")
    occurred_at_ns = _exact_int(
        row.get("_occurred_at_ns"), f"{row.get('kind')} occurred_at_ns"
    )
    committed_at_ns = _exact_int(
        row.get("_committed_at_ns"), f"{row.get('kind')} committed_at_ns"
    )
    if committed_at_ns < occurred_at_ns:
        _fail(f"{row.get('kind')} was committed before its occurrence")
    command_receipt = _sha256(
        row.get("_command_receipt_digest"), "authority command receipt"
    )
    if command_receipt != _receipt_for(store, row):
        _fail(f"{row.get('kind')} command receipt binding diverges")
    event_digests = [
        str(item[0])
        for item in store._conn.execute(
            "SELECT event_digest FROM events WHERE command_id=? ORDER BY ordinal",
            (command_id,),
        ).fetchall()
    ]
    if _sha256(
        row.get("_event_set_digest"), "command event-set digest"
    ) != canonical_digest(event_digests):
        _fail(f"{row.get('kind')} command event set is false")
    outbox_rows = [
        {
            "ordinal": int(item[0]),
            "outbox_id": str(item[1]),
            "payload_digest": str(item[4]),
            "topic": str(item[2]),
        }
        for item in store._conn.execute(
            "SELECT ordinal,outbox_id,topic,payload_json,payload_digest "
            "FROM immutable_outbox WHERE command_id=? ORDER BY ordinal",
            (command_id,),
        ).fetchall()
    ]
    if _sha256(
        row.get("_outbox_set_digest"), "command outbox-set digest"
    ) != canonical_digest(outbox_rows):
        _fail(f"{row.get('kind')} command outbox set is false")
    if command_payload is not None:
        payload_digest = _sha256(
            row.get("_command_payload_digest"), "authority command payload digest"
        )
        if payload_digest != canonical_digest(command_payload):
            _fail(f"{row.get('kind')} command payload digest is false")


def _one(rows: Sequence[dict[str, Any]], name: str) -> dict[str, Any]:
    if len(rows) != 1:
        _fail(f"{name} must resolve exactly once")
    return rows[0]


def _rows_by_kind(rows: Sequence[dict[str, Any]], kind: str) -> list[dict[str, Any]]:
    return [row for row in rows if row["kind"] == kind]


def _validate_receipt_chain(receipt_chain: Mapping[str, str]) -> dict[str, str]:
    chain = _mapping(receipt_chain, "receipt_chain")
    missing = sorted(_REQUIRED_RECEIPTS - set(chain))
    if missing:
        _fail("receipt_chain is missing: " + ", ".join(missing))
    validated: dict[str, str] = {}
    for name, digest in chain.items():
        key = _text(name, "receipt name")
        validated[key] = _sha256(digest, f"receipt_chain[{key}]")
    return validated


def _validate_closure_declaration(
    store: EpistemicSQLiteStore,
    rows: Sequence[dict[str, Any]],
    chain: Mapping[str, str],
) -> tuple[dict[str, Any], dict[str, Any]]:
    row = _one(_rows_by_kind(rows, "S4E_CLOSURE_ATTESTED"), "S4-E closure")
    if row["seq"] != rows[-1]["seq"]:
        _fail("S4-E closure must be the canonical tail event")
    if _receipt_for(store, row) != chain["s4e_closure"]:
        _fail("S4-E closure receipt does not commit the closure event")
    payload = _mapping(row["payload"], "S4-E closure payload")
    generation = store.state().execution_generation
    _require_authority_event(
        store,
        row,
        actor="protocol2-live-session",
        command_id=f"S4E_CLOSURE_ATTESTED:{generation}",
        event_id=f"event:S4E_CLOSURE_ATTESTED:{generation}",
        command_payload=payload,
    )
    _exact_fields(
        payload,
        frozenset(
            {
                "all_clean",
                "components",
                "invariants",
                "scope_digest",
                "schema",
                "solved",
            }
        ),
        "S4-E closure payload",
    )
    if _exact_bool(payload["solved"], "closure solved") is not True:
        _fail("production closure must be solved")
    if _exact_bool(payload["all_clean"], "closure all_clean") is not True:
        _fail("production closure must be all_clean")
    scope_digest = _sha256(payload["scope_digest"], "closure scope_digest")
    schema = _mapping(payload["schema"], "closure schema")
    if not _same_json(schema, _SCHEMA):
        _fail("closure schema is not the pinned S4-E v1 schema")
    invariants = _mapping(payload["invariants"], "closure invariants")
    expected_invariants = {
        "capture_pairs": True,
        "effects_close": True,
        "gate_closes": True,
        "orphan_free": True,
        "usage_closes": True,
    }
    if not _same_json(invariants, expected_invariants):
        _fail("closure invariant declaration is not exactly all-clean")
    components = _mapping(payload["components"], "closure components")
    if set(components) != {
        "canonical_permit",
        "capture_manifest",
        "gate_input",
        "orphan_summary",
        "s4e_schema",
        "usage_closure",
    }:
        _fail("closure components do not match the pinned schema")
    for name, digest in components.items():
        _sha256(digest, f"closure component {name}")
        if chain[name] != digest:
            _fail(f"receipt chain diverges at component {name}")
    if components["s4e_schema"] != canonical_digest(_SCHEMA):
        _fail("S4-E schema component digest is false")
    return row, {**payload, "scope_digest": scope_digest}


def _validate_admissions(
    store: EpistemicSQLiteStore,
    rows: Sequence[dict[str, Any]],
    scope_digest: str,
) -> list[dict[str, Any]]:
    admissions = _rows_by_kind(rows, "ATTEMPT_ADMITTED")
    if not admissions:
        _fail("solved closure has no admitted attempts")
    launches = _rows_by_kind(rows, "WORKER_LAUNCH_PREPARED")
    terminals = _rows_by_kind(rows, "WORKER_TERMINAL")
    if _rows_by_kind(rows, "WORKER_UNKNOWN"):
        _fail("WORKER_UNKNOWN cannot enter a clean closure")
    contexts: list[dict[str, Any]] = []
    unique: dict[str, set[str]] = defaultdict(set)
    for admission in admissions:
        payload = _mapping(admission["payload"], "admission payload")
        _exact_fields(payload, _ADMISSION_FIELDS, "admission payload")
        attempt_id = _text(payload["attempt_id"], "attempt_id")
        permit_id = _text(payload["permit_id"], "permit_id")
        branch_id = _text(payload["branch_id"], "branch_id")
        lease_id = _text(payload["lease_id"], "lease_id")
        account_id = _text(payload["account_id"], "account_id")
        fingerprint = _text(payload["fingerprint"], "fingerprint")
        unique["attempt"].add(attempt_id)
        unique["permit"].add(permit_id)
        unique["lease"].add(lease_id)
        unique["fingerprint"].add(fingerprint)
        if payload["scope_digest"] != scope_digest:
            _fail("admission belongs to a different execution scope")
        launch_ordinal = _exact_int(
            payload["launch_ordinal"], "launch_ordinal", minimum=1
        )
        lease_epoch = _exact_int(payload["lease_epoch"], "lease_epoch", minimum=1)
        worker_generation = _exact_int(
            payload["worker_generation"], "worker_generation", minimum=1
        )
        _exact_int(payload["expires_at_ns"], "expires_at_ns")
        expires_at_ns = payload["expires_at_ns"]
        attempt_digest = _sha256(payload["attempt_digest"], "attempt_digest")
        expected_attempt = canonical_digest(
            {
                "attempt_id": attempt_id,
                "branch_id": branch_id,
                "launch_ordinal": launch_ordinal,
                "scope_digest": scope_digest,
            }
        )
        if attempt_digest != expected_attempt:
            _fail("admission attempt digest is false")
        lease_digest = _sha256(payload["lease_digest"], "lease_digest")
        expected_lease = canonical_digest(
            {
                "attempt_digest": attempt_digest,
                "lease_epoch": lease_epoch,
                "lease_id": lease_id,
                "worker_generation": worker_generation,
            }
        )
        if lease_digest != expected_lease:
            _fail("admission lease digest is false")
        policy_digest = _sha256(payload["policy_digest"], "policy_digest")
        reservations = _canonical_strings(payload["reservation_ids"], "reservation_ids")
        if not reservations:
            _fail("admission has no reservation identities")
        ancestry = list(store.budget_ancestry(account_id))
        expected_reservations = [f"{permit_id}:{ancestor}" for ancestor in ancestry]
        if not ancestry or reservations != expected_reservations:
            _fail("admission reservations do not match canonical budget ancestry")
        requested_budget = _nonnegative_int_map(
            payload["requested_budget"], "requested_budget"
        )
        conflicts = _canonical_strings(payload["conflict_keys"], "conflict_keys")
        effect_class = _text(payload["effect_class"], "effect_class")
        if effect_class not in {"pure", "idempotent", "observable", "non_idempotent"}:
            _fail("admission effect class is unknown")
        permit = _mapping(payload["permit"], "permit body")
        _exact_fields(permit, _PERMIT_FIELDS, "permit body")
        expected_constraints = {
            "account_id": account_id,
            "conflict_keys": conflicts,
            "fingerprint": fingerprint,
            "requested_budget": requested_budget,
        }
        expected_permit = {
            "constraints": expected_constraints,
            "effect_class": effect_class,
            "expires_at_ns": payload["expires_at_ns"],
            "lease_digest": lease_digest,
            "permit_id": permit_id,
            "policy_digest": policy_digest,
            "reservation_ids": reservations,
        }
        if not _same_json(permit, expected_permit):
            _fail("admission permit body is internally split")
        permit_digest = _sha256(payload["permit_digest"], "permit_digest")
        if canonical_digest(permit) != permit_digest:
            _fail("admission permit digest is false")

        permit_launches = [
            row for row in launches if row["payload"].get("permit_id") == permit_id
        ]
        launch = _one(permit_launches, f"launch for {permit_id}")
        launch_payload = _mapping(launch["payload"], "launch payload")
        _exact_fields(launch_payload, _LAUNCH_FIELDS, "launch payload")
        expected_launch = {
            "admission_event_digest": admission["event_digest"],
            "attempt_digest": attempt_digest,
            "attempt_id": attempt_id,
            "launch_ordinal": launch_ordinal,
            "lease_digest": lease_digest,
            "lease_id": lease_id,
            "permit_digest": permit_digest,
            "permit_id": permit_id,
            "reservation_ids": reservations,
            "scope_digest": scope_digest,
        }
        if not _same_json(launch_payload, expected_launch):
            _fail("worker launch does not exactly descend from admission")
        _require_authority_event(
            store,
            admission,
            actor="search-admission",
            command_id=f"attempt:admit:{attempt_id}",
            event_id=f"event:attempt:admit:{attempt_id}",
            command_payload=payload,
        )
        _require_authority_event(
            store,
            launch,
            actor="run-supervisor",
            command_id=f"launch:{permit_id}",
            event_id=f"event:launch:{permit_id}",
            command_payload=launch_payload,
        )
        if launch["seq"] <= admission["seq"]:
            _fail("worker launch precedes its admission")
        if not (
            admission["_occurred_at_ns"] <= launch["_occurred_at_ns"] < expires_at_ns
        ):
            _fail("worker launch is outside the admitted permit lifetime")

        permit_terminals = [
            row for row in terminals if row["payload"].get("permit_id") == permit_id
        ]
        terminal = _one(permit_terminals, f"terminal for {permit_id}")
        terminal_payload = _mapping(terminal["payload"], "worker terminal payload")
        _exact_fields(terminal_payload, _TERMINAL_FIELDS, "worker terminal payload")
        expected_terminal = {
            "admission_event_digest": admission["event_digest"],
            "attempt_digest": attempt_digest,
            "attempt_id": attempt_id,
            "launch_event_digest": launch["event_digest"],
            "lease_digest": lease_digest,
            "lease_id": lease_id,
            "outcome": "observed",
            "permit_digest": permit_digest,
            "permit_id": permit_id,
            "scope_digest": scope_digest,
        }
        if not _same_json(terminal_payload, expected_terminal):
            _fail("worker terminal lineage is malformed")
        _require_authority_event(
            store,
            terminal,
            actor="run-supervisor",
            command_id=f"launch-terminal:{permit_id}",
            event_id=f"event:launch-terminal:{permit_id}",
            command_payload=terminal_payload,
        )
        if terminal["seq"] <= launch["seq"]:
            _fail("worker terminal precedes its launch")

        projection = store._conn.execute(
            "SELECT branch_id,permit_id,scope_digest,lease_id,lease_epoch,"
            "worker_generation,fingerprint,effect_class,state "
            "FROM runtime_attempts WHERE attempt_id=?",
            (attempt_id,),
        ).fetchone()
        expected_projection = (
            branch_id,
            permit_id,
            scope_digest,
            lease_id,
            lease_epoch,
            worker_generation,
            fingerprint,
            effect_class,
            "terminal",
        )
        if projection is None or tuple(projection) != expected_projection:
            _fail("terminal attempt projection diverges from canonical lineage")
        reservation_rows = store._conn.execute(
            "SELECT reservation_id,account_id,dimensions_json,state "
            "FROM budget_reservations WHERE attempt_id=? ORDER BY reservation_id",
            (attempt_id,),
        ).fetchall()
        if len(reservation_rows) != len(reservations):
            _fail("reservation projection cardinality diverges")
        if {str(row[0]) for row in reservation_rows} != set(reservations):
            _fail("reservation projection identities diverge")
        account_by_reservation = dict(zip(reservations, ancestry, strict=True))
        for reservation_row in reservation_rows:
            if reservation_row[1] != account_by_reservation[str(reservation_row[0])]:
                _fail("reservation projection account lineage diverges")
            if json.loads(reservation_row[2]) != requested_budget:
                _fail("reservation projection dimensions diverge")
            if reservation_row[3] != "settled":
                _fail("reservation projection is not terminal-settled")
        contexts.append(
            {
                "admission": admission,
                "attempt_digest": attempt_digest,
                "attempt_id": attempt_id,
                "budget": requested_budget,
                "conflicts": conflicts,
                "effect_class": effect_class,
                "expires_at_ns": expires_at_ns,
                "launch": launch,
                "lease_digest": lease_digest,
                "permit_digest": permit_digest,
                "permit_id": permit_id,
                "policy_digest": policy_digest,
                "reservations": reservations,
                "terminal": terminal,
            }
        )
    for identity, values in unique.items():
        if len(values) != len(admissions):
            _fail(f"{identity} identity is reused across admissions")
    if len(launches) != len(admissions) or len(terminals) != len(admissions):
        _fail("unowned launch or terminal event exists")
    if any(context["effect_class"] != "observable" for context in contexts):
        _fail("S4-E live canary requires the coarse observable worker effect class")
    if len({context["policy_digest"] for context in contexts}) != 1:
        _fail("attempts do not share one admitted live-session policy")
    if len({canonical_digest(context["budget"]) for context in contexts}) != 1:
        _fail("attempts do not share one admitted per-attempt budget")
    if (
        len({context["admission"]["payload"]["account_id"] for context in contexts})
        != 1
    ):
        _fail("attempts do not share one live-session budget account")
    return contexts


def _validate_supporting_authority(
    store: EpistemicSQLiteStore,
    rows: Sequence[dict[str, Any]],
    contexts: Sequence[dict[str, Any]],
) -> None:
    """Pin the boot, execution, branch, and budget roots that underwrite permits."""

    state = store.state()
    verifying = _one(_rows_by_kind(rows, "BOOT_VERIFYING"), "BOOT_VERIFYING")
    verifying_payload = _mapping(verifying["payload"], "BOOT_VERIFYING payload")
    _exact_fields(
        verifying_payload,
        frozenset({"boot_epoch", "writer_epoch"}),
        "BOOT_VERIFYING payload",
    )
    boot_epoch = _exact_int(verifying_payload["boot_epoch"], "boot_epoch", minimum=1)
    _exact_int(verifying_payload["writer_epoch"], "writer_epoch", minimum=1)
    _require_authority_event(
        store,
        verifying,
        actor="host-run-factory",
        command_id=f"BOOT_VERIFYING:{boot_epoch}",
        event_id=f"event:BOOT_VERIFYING:{boot_epoch}",
        command_payload=verifying_payload,
    )
    ready = _one(_rows_by_kind(rows, "BOOT_READY"), "BOOT_READY")
    ready_payload = _mapping(ready["payload"], "BOOT_READY payload")
    _exact_fields(
        ready_payload, frozenset({"attestation_digest"}), "BOOT_READY payload"
    )
    _sha256(ready_payload["attestation_digest"], "boot attestation digest")
    _require_authority_event(
        store,
        ready,
        actor="host-run-factory",
        command_id=f"BOOT_READY:{boot_epoch}",
        event_id=f"event:BOOT_READY:{boot_epoch}",
        command_payload=ready_payload,
    )
    start = _one(_rows_by_kind(rows, "START_EXECUTION"), "START_EXECUTION")
    start_payload = _mapping(start["payload"], "START_EXECUTION payload")
    expected_start = {
        "execution_generation": state.execution_generation,
        "run_fence_epoch": state.run_fence_epoch,
    }
    if not _same_json(start_payload, expected_start):
        _fail("START_EXECUTION does not establish the closed execution scope")
    generation = state.execution_generation
    _require_authority_event(
        store,
        start,
        actor="host-run-factory",
        command_id=f"START_EXECUTION:{generation}",
        event_id=f"event:START_EXECUTION:{generation}",
        command_payload=start_payload,
    )
    if not (verifying["seq"] < ready["seq"] < start["seq"]):
        _fail("boot and execution authority order is invalid")

    branch_creates = _rows_by_kind(rows, "BRANCH_CREATED")
    if not branch_creates:
        _fail("attempts have no canonical branch authority")
    branches: dict[str, dict[str, Any]] = {}
    for event in branch_creates:
        payload = _mapping(event["payload"], "BRANCH_CREATED payload")
        _exact_fields(
            payload,
            frozenset({"branch_id", "depends_on", "max_attempts"}),
            "BRANCH_CREATED payload",
        )
        branch_id = _text(payload["branch_id"], "branch_id")
        if branch_id in branches:
            _fail("branch authority is duplicated")
        depends_on = _canonical_strings(payload["depends_on"], "branch dependencies")
        max_attempts = _exact_int(payload["max_attempts"], "max_attempts", minimum=1)
        canonical_payload = {
            "branch_id": branch_id,
            "depends_on": depends_on,
            "max_attempts": max_attempts,
        }
        _require_authority_event(
            store,
            event,
            actor="search-admission",
            command_id=f"branch:create:{branch_id}",
            event_id=f"event:branch:create:{branch_id}",
            command_payload=canonical_payload,
        )
        branches[branch_id] = {
            "depends_on": depends_on,
            "max_attempts": max_attempts,
            "state": "open",
        }
    changes = _rows_by_kind(rows, "BRANCH_STATE_CHANGED")
    for event in changes:
        payload = _mapping(event["payload"], "BRANCH_STATE_CHANGED payload")
        _exact_fields(
            payload,
            frozenset({"branch_id", "from", "revision", "to"}),
            "BRANCH_STATE_CHANGED payload",
        )
        branch_id = _text(payload["branch_id"], "branch state branch_id")
        revision = _exact_int(payload["revision"], "branch revision", minimum=1)
        before = _text(payload["from"], "branch from state")
        after = _text(payload["to"], "branch to state")
        branch = branches.get(branch_id)
        if branch is None or branch["state"] != before:
            _fail("branch state authority does not compare-and-set")
        _require_authority_event(
            store,
            event,
            actor="search-admission",
            command_id=f"branch:state:{branch_id}:{revision}",
            event_id=f"event:branch:state:{branch_id}:{revision}",
            command_payload={
                "branch_id": branch_id,
                "expected_state": before,
                "new_state": after,
                "revision": revision,
            },
        )
        branch["state"] = after
    attempts_by_branch: dict[str, int] = defaultdict(int)
    for context in contexts:
        branch_id = context["admission"]["payload"]["branch_id"]
        if branch_id not in branches:
            _fail("admission references an unauthorized branch")
        attempts_by_branch[branch_id] += 1
    projection_branches = store._conn.execute(
        "SELECT branch_id,state,depends_on_json,max_attempts,attempt_count "
        "FROM runtime_branches ORDER BY branch_id"
    ).fetchall()
    if {str(row[0]) for row in projection_branches} != set(branches):
        _fail("branch projection contains unauthorized roots")
    for (
        branch_id,
        branch_state,
        dependencies_json,
        max_attempts,
        count,
    ) in projection_branches:
        branch = branches[str(branch_id)]
        if (
            branch_state != branch["state"]
            or json.loads(dependencies_json) != branch["depends_on"]
            or max_attempts != branch["max_attempts"]
            or count != attempts_by_branch[str(branch_id)]
            or count > max_attempts
        ):
            _fail("branch projection or hard attempt bound diverges")

    account_events = _rows_by_kind(rows, "BUDGET_ACCOUNT_CREATED")
    if not account_events:
        _fail("attempts have no canonical budget authority")
    accounts: dict[str, dict[str, Any]] = {}
    for event in account_events:
        payload = _mapping(event["payload"], "BUDGET_ACCOUNT_CREATED payload")
        _exact_fields(
            payload,
            frozenset({"account_id", "limits", "parent_id"}),
            "BUDGET_ACCOUNT_CREATED payload",
        )
        account_id = _text(payload["account_id"], "budget account_id")
        if account_id in accounts:
            _fail("budget account authority is duplicated")
        parent_id = _text(payload["parent_id"], "budget parent_id", allow_empty=True)
        limits = _nonnegative_int_map(payload["limits"], "budget limits")
        canonical_payload = {
            "account_id": account_id,
            "limits": limits,
            "parent_id": parent_id,
        }
        _require_authority_event(
            store,
            event,
            actor="search-admission",
            command_id=f"budget:create:{account_id}",
            event_id=f"event:budget:create:{account_id}",
            command_payload=canonical_payload,
        )
        accounts[account_id] = {"limits": limits, "parent_id": parent_id}
    for account_id, account in accounts.items():
        parent_id = account["parent_id"]
        if parent_id and parent_id not in accounts:
            _fail(f"budget account {account_id} has an unauthorized parent")
    for context in contexts:
        account_id = context["admission"]["payload"]["account_id"]
        if account_id not in accounts:
            _fail("admission references an unauthorized budget account")
    projection_accounts = store._conn.execute(
        "SELECT account_id,parent_id,limits_json FROM budget_accounts ORDER BY account_id"
    ).fetchall()
    if {str(row[0]) for row in projection_accounts} != set(accounts):
        _fail("budget projection contains unauthorized roots")
    for account_id, parent_id, limits_json in projection_accounts:
        account = accounts[str(account_id)]
        if (
            str(parent_id or "") != account["parent_id"]
            or json.loads(limits_json) != account["limits"]
        ):
            _fail("budget account projection diverges from authority event")


def _validate_usage(
    store: EpistemicSQLiteStore,
    rows: Sequence[dict[str, Any]],
    contexts: Sequence[dict[str, Any]],
) -> list[dict[str, Any]]:
    if _rows_by_kind(rows, "BUDGET_USAGE_UNKNOWN"):
        _fail("UNKNOWN usage cannot enter a clean closure")
    if _rows_by_kind(rows, "BUDGET_PESSIMISTICALLY_SETTLED"):
        _fail("unobserved pessimistic usage cannot enter a clean closure")
    settled = _rows_by_kind(rows, "BUDGET_SETTLED")
    for context in contexts:
        attempt_rows = [
            row
            for row in settled
            if row["payload"].get("attempt_id") == context["attempt_id"]
        ]
        row = _one(attempt_rows, f"budget settlement for {context['attempt_id']}")
        payload = _mapping(row["payload"], "budget settlement payload")
        _exact_fields(
            payload,
            frozenset(
                {
                    "actual_usage",
                    "attempt_id",
                    "reservation_ids",
                    "settlement_revision",
                    "usage_report",
                    "usage_report_digest",
                }
            ),
            "budget settlement payload",
        )
        if (
            _exact_int(payload["settlement_revision"], "settlement_revision", minimum=1)
            != 1
        ):
            _fail("live S4-E settlement must use first revision")
        _require_authority_event(
            store,
            row,
            actor="search-admission",
            command_id=f"budget:settle:{context['attempt_id']}:1",
            event_id=f"event:budget:settle:{context['attempt_id']}:1",
            command_payload=payload,
        )
        if (
            _sequence(payload["reservation_ids"], "settlement reservation_ids")
            != context["reservations"]
        ):
            _fail("settlement does not bind the admitted reservations")
        report = _mapping(payload["usage_report"], "usage report")
        _exact_fields(report, frozenset({"measurements"}), "usage report")
        measurements = _sequence(report["measurements"], "usage measurements")
        if len(measurements) != len(context["budget"]):
            _fail("usage report does not cover every reserved axis")
        axes: list[str] = []
        charged: dict[str, int] = {}
        for measurement_value in measurements:
            measurement = _mapping(measurement_value, "usage measurement")
            _exact_fields(
                measurement,
                frozenset({"axis", "observed_so_far", "reserved_ceiling", "status"}),
                "usage measurement",
            )
            axis = _text(measurement["axis"], "usage axis")
            observed = _exact_int(measurement["observed_so_far"], "observed usage")
            ceiling = _exact_int(measurement["reserved_ceiling"], "usage ceiling")
            status = _text(measurement["status"], "usage status")
            if status not in {"observed", "partial"}:
                _fail("clean closure usage may not be UNKNOWN")
            if context["budget"].get(axis) != ceiling:
                _fail("usage measurement does not bind its reservation ceiling")
            if observed > ceiling:
                _fail("observed usage exceeds its admitted hard ceiling")
            axes.append(axis)
            charged[axis] = observed if status == "observed" else max(observed, ceiling)
        if axes != sorted(axes) or len(set(axes)) != len(axes):
            _fail("usage measurements are not canonical and unique")
        if set(axes) != set(context["budget"]):
            _fail("usage measurements do not exactly cover reserved axes")
        if canonical_digest(report) != _sha256(
            payload["usage_report_digest"], "usage_report_digest"
        ):
            _fail("usage report digest is false")
        if _nonnegative_int_map(payload["actual_usage"], "actual_usage") != charged:
            _fail("settled charge does not match tagged usage")
        if not (context["launch"]["seq"] < row["seq"] < context["terminal"]["seq"]):
            _fail("budget settlement is outside worker lifecycle")
        context["settlement"] = row
    if len(settled) != len(contexts):
        _fail("unowned budget settlement exists")
    # The projection was checked per attempt; this guards aggregate owner leakage.
    owners = store.lifecycle_owner_summary()
    if owners != {"attempts": 0, "reservations": 0, "effects": 0}:
        _fail("runtime lifecycle owners remain after closure")
    accounts = store._conn.execute(
        "SELECT account_id,limits_json,settled_json,held_json,debt "
        "FROM budget_accounts ORDER BY account_id"
    ).fetchall()
    for account_id, limits_json, settled_json, held_json, debt in accounts:
        limits = _nonnegative_int_map(json.loads(limits_json), "budget limits")
        charged = _nonnegative_int_map(json.loads(settled_json), "budget settled")
        held = _nonnegative_int_map(json.loads(held_json), "budget held")
        if (
            type(debt) is not int
            or debt != 0
            or set(limits) != set(charged)
            or set(limits) != set(held)
            or any(held[axis] != 0 for axis in held)
            or any(charged[axis] > limits[axis] for axis in limits)
        ):
            _fail(f"budget account {account_id} is not cleanly accounted")
    return settled


def _validate_effects(
    store: EpistemicSQLiteStore,
    rows: Sequence[dict[str, Any]],
    contexts: Sequence[dict[str, Any]],
) -> None:
    forbidden = {
        "EFFECT_UNKNOWN",
        "EFFECT_CONFIRMED_NOT_APPLIED",
        "EFFECT_RETRY_PREPARED",
    }
    if any(row["kind"] in forbidden for row in rows):
        _fail("non-observed effect state cannot enter a clean closure")
    prepared = _rows_by_kind(rows, "EFFECT_PREPARED")
    dispatched = _rows_by_kind(rows, "EFFECT_DISPATCH_MAY_HAVE_STARTED")
    observed = _rows_by_kind(rows, "EFFECT_OBSERVED")
    for context in contexts:
        operation_id = "worker-effect:" + context["permit_id"]
        prep = _one(
            [
                row
                for row in prepared
                if row["payload"].get("operation_id") == operation_id
            ],
            f"effect prepare for {operation_id}",
        )
        dispatch = _one(
            [
                row
                for row in dispatched
                if row["payload"].get("operation_id") == operation_id
            ],
            f"effect dispatch for {operation_id}",
        )
        observation = _one(
            [
                row
                for row in observed
                if row["payload"].get("operation_id") == operation_id
            ],
            f"effect observation for {operation_id}",
        )
        prep_payload = _mapping(prep["payload"], "effect prepare payload")
        expected_prep = {
            "attempt_id": context["attempt_id"],
            "conflict_keys": context["conflicts"],
            "effect_class": context["effect_class"],
            "operation_id": operation_id,
        }
        if set(prep_payload) != set(expected_prep) or not _same_json(
            prep_payload, expected_prep
        ):
            _fail("effect prepare does not bind the admitted operation")
        expected_dispatch = {
            "expected_state": "prepared",
            "new_state": "dispatch_may_have_started",
            "operation_id": operation_id,
            "revision": 1,
        }
        expected_observed = {
            "expected_state": "dispatch_may_have_started",
            "new_state": "observed",
            "operation_id": operation_id,
            "revision": 2,
        }
        if not _same_json(dispatch["payload"], expected_dispatch):
            _fail("effect dispatch transition is malformed")
        if not _same_json(observation["payload"], expected_observed):
            _fail("effect observation transition is malformed")
        _require_authority_event(
            store,
            prep,
            actor="effect-ledger",
            command_id=f"effect:prepare:{operation_id}",
            event_id=f"event:effect:prepare:{operation_id}",
            command_payload=prep_payload,
        )
        _require_authority_event(
            store,
            dispatch,
            actor="effect-ledger",
            command_id=f"effect:transition:{operation_id}:1",
            event_id=f"event:effect:transition:{operation_id}:1",
            command_payload=expected_dispatch,
        )
        _require_authority_event(
            store,
            observation,
            actor="effect-ledger",
            command_id=f"effect:transition:{operation_id}:2",
            event_id=f"event:effect:transition:{operation_id}:2",
            command_payload=expected_observed,
        )
        if not (
            context["launch"]["seq"]
            < prep["seq"]
            < dispatch["seq"]
            < observation["seq"]
            < context["settlement"]["seq"]
            < context["terminal"]["seq"]
        ):
            _fail("effect state machine is outside worker lifecycle")
        projection = store._conn.execute(
            "SELECT attempt_id,effect_class,conflict_keys_json,state,current_ordinal "
            "FROM effect_operations WHERE operation_id=?",
            (operation_id,),
        ).fetchone()
        expected_projection = (
            context["attempt_id"],
            context["effect_class"],
            canonical_json_bytes(context["conflicts"]).decode(),
            "observed",
            1,
        )
        if projection is None or tuple(projection) != expected_projection:
            _fail("effect operation projection is not observed and closed")
        attempts = store._conn.execute(
            "SELECT ordinal,state FROM effect_attempts WHERE operation_id=? ORDER BY ordinal",
            (operation_id,),
        ).fetchall()
        if attempts != [(1, "observed")]:
            _fail("effect attempt projection is not one observed attempt")
        context["effect_dispatch"] = dispatch
        context["effect_observed"] = observation
    if not (len(prepared) == len(dispatched) == len(observed) == len(contexts)):
        _fail("unowned or incomplete effect operation exists")


def _validate_captures(
    store: EpistemicSQLiteStore,
    cas: ReceiptCAS,
    rows: Sequence[dict[str, Any]],
    contexts: Sequence[dict[str, Any]],
) -> tuple[list[dict[str, Any]], dict[str, dict[str, Any]]]:
    captures = _rows_by_kind(rows, "CAPTURE_CHUNK_SEALED")
    manifests = _rows_by_kind(rows, "CAPTURE_MANIFEST_ADVANCED")
    if len(captures) != len(manifests):
        _fail("capture and manifest event cardinality differs")
    by_manifest: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for manifest in manifests:
        digest = _sha256(manifest["payload"].get("manifest_digest"), "manifest digest")
        by_manifest[digest].append(manifest)
    context_by_permit = {context["permit_digest"]: context for context in contexts}
    previous_by_permit: dict[str, str] = defaultdict(str)
    ordinal_by_permit: dict[str, int] = defaultdict(int)
    terminal_by_permit: dict[str, bool] = defaultdict(bool)
    capture_by_event: dict[str, dict[str, Any]] = {}
    capture_ids: set[str] = set()
    for capture in captures:
        payload = _mapping(capture["payload"], "capture payload")
        _exact_fields(payload, _CAPTURE_FIELDS, "capture payload")
        permit_digest = _sha256(payload["permit_digest"], "capture permit_digest")
        context = context_by_permit.get(permit_digest)
        if context is None:
            _fail("capture has no admitted permit")
        if terminal_by_permit[permit_digest]:
            _fail("capture exists after a terminal manifest")
        capture_id = _text(payload["capture_id"], "capture_id")
        if capture_id in capture_ids:
            _fail("capture_id was consumed more than once")
        capture_ids.add(capture_id)
        ordinal = _exact_int(payload["ordinal"], "capture ordinal")
        if ordinal != ordinal_by_permit[permit_digest]:
            _fail("capture ordinal chain is discontinuous")
        previous = _sha256(
            payload["previous_manifest_digest"],
            "previous_manifest_digest",
            allow_empty=True,
        )
        if previous != previous_by_permit[permit_digest]:
            _fail("capture hash chain is discontinuous")
        if payload["attempt_digest"] != context["attempt_digest"]:
            _fail("capture attempt binding diverges")
        if payload["lease_digest"] != context["lease_digest"]:
            _fail("capture lease binding diverges")
        stream = _text(payload["stream"], "capture stream")
        if stream not in {"stdout", "stderr", "tool_result", "gate_input"}:
            _fail("capture stream is unsupported")
        terminal = _exact_bool(payload["terminal"], "capture terminal")
        candidate_id = _text(
            payload["candidate_id"], "capture candidate_id", allow_empty=True
        )
        flag_digest = _sha256(
            payload["flag_digest"], "capture flag_digest", allow_empty=True
        )
        flag_format_digest = _sha256(
            payload["flag_format_digest"],
            "capture flag_format_digest",
            allow_empty=True,
        )
        policy_digest = _sha256(
            payload["policy_digest"], "capture policy_digest", allow_empty=True
        )
        if stream == "gate_input":
            if (
                not candidate_id
                or not flag_digest
                or not flag_format_digest
                or policy_digest != context["policy_digest"]
                or terminal
            ):
                _fail("gate input capture metadata is incomplete or rebound")
        elif any((candidate_id, flag_digest, flag_format_digest, policy_digest)):
            _fail("ordinary capture carries unauthorized gate metadata")
        raw_digest = _sha256(payload["raw_digest"], "capture raw_digest")
        byte_count = _exact_int(payload["byte_count"], "capture byte_count")
        body = {name: payload[name] for name in _CAPTURE_BODY_FIELDS}
        manifest_digest = _sha256(payload["manifest_digest"], "capture manifest_digest")
        if canonical_digest(body) != manifest_digest:
            _fail("capture manifest digest is false")
        manifest = _one(by_manifest.get(manifest_digest, []), "capture manifest pair")
        if manifest["payload"] != capture["payload"]:
            _fail("capture chunk and manifest payloads differ")
        if manifest["seq"] != capture["seq"] + 1:
            _fail("capture chunk and manifest are not an atomic ordered pair")
        capture_actor = capture.get("_actor")
        if (
            capture_actor
            not in {"capture-port", "cognitive-runtime-output-port-v1"}
            or manifest.get("_actor") != capture_actor
        ):
            _fail("capture authority actor is unsupported or split")
        command_id = f"capture:{permit_digest}:{ordinal}"
        _require_authority_event(
            store,
            capture,
            actor=capture_actor,
            command_id=command_id,
            event_id=f"event:{command_id}:chunk",
            ordinal=0,
            event_count=2,
            first_seq=capture["seq"],
            last_seq=manifest["seq"],
            command_payload=payload,
        )
        _require_authority_event(
            store,
            manifest,
            actor=capture_actor,
            command_id=command_id,
            event_id=f"event:{command_id}:manifest",
            ordinal=1,
            event_count=2,
            first_seq=capture["seq"],
            last_seq=manifest["seq"],
            command_payload=payload,
        )
        if capture["_occurred_at_ns"] >= context["expires_at_ns"]:
            _fail("capture is outside the admitted permit lifetime")
        if not (
            context["effect_dispatch"]["seq"]
            < capture["seq"]
            < manifest["seq"]
            < context["effect_observed"]["seq"]
        ):
            _fail("capture is outside the dispatched effect interval")
        try:
            raw = cas.read_verified(raw_digest)
        except (CASIntegrityError, ValueError) as exc:
            raise ClosureResolutionError(
                "capture CAS object failed verification"
            ) from exc
        if len(raw) != byte_count:
            _fail("capture byte count differs from CAS readback")
        event_digest = _sha256(capture["event_digest"], "capture event digest")
        capture_by_event[event_digest] = {**payload, "raw": raw, "row": capture}
        previous_by_permit[permit_digest] = manifest_digest
        ordinal_by_permit[permit_digest] += 1
        terminal_by_permit[permit_digest] = terminal
    if any(len(value) != 1 for value in by_manifest.values()):
        _fail("capture manifest is ambiguous")
    return captures, capture_by_event


def _validate_gate_and_goal(
    store: EpistemicSQLiteStore,
    cas: ReceiptCAS,
    rows: Sequence[dict[str, Any]],
    contexts: Sequence[dict[str, Any]],
    capture_by_event: Mapping[str, dict[str, Any]],
    chain: Mapping[str, str],
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    accepted = _rows_by_kind(rows, "FLAG_ACCEPTED")
    if not accepted:
        _fail("solved closure has no accepted gate event")
    if len(accepted) != 1:
        _fail(
            "S4-E schema v1 proves only an exact single-goal canary; "
            "multi-goal completeness is not canonical"
        )
    if _rows_by_kind(rows, "FLAG_REJECTED"):
        _fail("a rejected hardcoded gate decision cannot attest gate equivalence")
    context_by_permit = {context["permit_digest"]: context for context in contexts}
    consumed_captures: set[str] = set()
    accepted_bindings: list[tuple[str, str]] = []
    for gate in accepted:
        payload = _mapping(gate["payload"], "accepted gate payload")
        _exact_fields(payload, _GATE_FIELDS, "accepted gate payload")
        if _exact_bool(payload["accepted"], "gate accepted") is not True:
            _fail("FLAG_ACCEPTED payload is not accepted")
        capture_digest = _sha256(
            payload["capture_event_digest"], "gate capture_event_digest"
        )
        if capture_digest in consumed_captures:
            _fail("one gate input capture was accepted more than once")
        consumed_captures.add(capture_digest)
        capture = capture_by_event.get(capture_digest)
        if capture is None or capture["stream"] != "gate_input":
            _fail("accepted gate does not resolve to a gate-input capture")
        permit_digest = _sha256(payload["permit_digest"], "gate permit_digest")
        context = context_by_permit.get(permit_digest)
        if context is None:
            _fail("accepted gate has no admitted permit")
        candidate_id = _text(payload["candidate_id"], "gate candidate_id")
        flag_digest = _sha256(payload["flag_digest"], "gate flag_digest")
        expected = {
            "attempt_digest": context["attempt_digest"],
            "candidate_id": capture["candidate_id"],
            "flag_digest": capture["flag_digest"],
            "flag_format_digest": capture["flag_format_digest"],
            "lease_digest": context["lease_digest"],
            "manifest_digest": capture["manifest_digest"],
            "policy_digest": capture["policy_digest"],
            "raw_digest": capture["raw_digest"],
        }
        if any(payload.get(name) != value for name, value in expected.items()):
            _fail("accepted gate is rebound from its capture lineage")
        evaluation_id = canonical_digest(
            {
                "candidate_id": candidate_id,
                "flag_digest": flag_digest,
                "manifest_digest": capture["manifest_digest"],
                "permit_digest": permit_digest,
                "version": 1,
            }
        )
        if payload["evaluation_id"] != evaluation_id:
            _fail("gate evaluation identity is false")
        decoded = capture["raw"].decode("utf-8", errors="replace")
        snapshot = {
            "artifact_policy": "inline-capture-only-v1",
            "attempt_digest": context["attempt_digest"],
            "candidate_id": candidate_id,
            "capture_event_digest": capture_digest,
            "decoded_gate_input_digest": canonical_digest(decoded),
            "decoder": "utf-8-errors-replace-v1",
            "flag_digest": flag_digest,
            "lease_digest": context["lease_digest"],
            "manifest_digest": capture["manifest_digest"],
            "permit_digest": permit_digest,
            "policy_digest": capture["policy_digest"],
            "raw_capture_digest": capture["raw_digest"],
            "raw_capture_size": capture["byte_count"],
        }
        if payload["snapshot_digest"] != canonical_digest(snapshot):
            _fail("gate snapshot digest is false")
        _require_authority_event(
            store,
            gate,
            actor="hardcoded-gate",
            command_id=f"gate:{evaluation_id}",
            event_id=f"event:gate:{evaluation_id}",
            command_payload={**snapshot, "accepted": True},
        )
        _require_gate_outbox(
            store,
            cas,
            command_id=f"gate:{evaluation_id}",
            attempt_digest=context["attempt_digest"],
            candidate_id=candidate_id,
            evaluation_id=evaluation_id,
            flag_digest=flag_digest,
            snapshot_digest=payload["snapshot_digest"],
        )
        if gate["_occurred_at_ns"] >= context["expires_at_ns"]:
            _fail("gate decision is outside the admitted permit lifetime")
        if not (
            capture["row"]["seq"] < gate["seq"] < context["effect_observed"]["seq"]
        ):
            _fail("gate acceptance is outside the observed effect interval")
        accepted_bindings.append((flag_digest, _receipt_for(store, gate)))

    gate_input_captures = {
        event_digest
        for event_digest, capture in capture_by_event.items()
        if capture["stream"] == "gate_input"
    }
    if consumed_captures != gate_input_captures:
        _fail("every gate-input capture must be decided and consumed exactly once")

    goal = _one(_rows_by_kind(rows, "GOAL_COMPLETED"), "GOAL_COMPLETED")
    goal_payload = _mapping(goal["payload"], "GOAL_COMPLETED payload")
    _exact_fields(goal_payload, frozenset({"gate_receipts"}), "GOAL_COMPLETED payload")
    raw_bindings = _sequence(goal_payload["gate_receipts"], "goal gate_receipts")
    goal_bindings: list[tuple[str, str]] = []
    for raw_binding in raw_bindings:
        binding = _sequence(raw_binding, "goal gate receipt binding")
        if len(binding) != 2:
            _fail("goal gate receipt binding must contain flag and receipt digests")
        goal_bindings.append(
            (
                _sha256(binding[0], "goal flag digest"),
                _sha256(binding[1], "goal gate receipt"),
            )
        )
    if not goal_bindings or goal_bindings != sorted(goal_bindings):
        _fail("goal gate receipt bindings are empty or non-canonical")
    if len(set(goal_bindings)) != len(goal_bindings):
        _fail("goal gate receipt binding was consumed more than once")
    if len({flag_digest for flag_digest, _receipt in goal_bindings}) != len(
        goal_bindings
    ) or len({receipt for _flag_digest, receipt in goal_bindings}) != len(
        goal_bindings
    ):
        _fail("goal flag or gate receipt identity was consumed more than once")
    if sorted(accepted_bindings) != goal_bindings:
        _fail("GOAL_COMPLETED does not consume every accepted gate exactly once")
    if any(context["terminal"]["seq"] >= goal["seq"] for context in contexts):
        _fail("GOAL_COMPLETED precedes worker terminalization")
    generation = store.state().execution_generation
    _require_authority_event(
        store,
        goal,
        actor="protocol2-live-session",
        command_id=f"GOAL_COMPLETED:{generation}",
        event_id=f"event:GOAL_COMPLETED:{generation}",
        command_payload=goal_payload,
    )
    if _receipt_for(store, goal) != chain["gate"]:
        _fail("gate receipt does not commit GOAL_COMPLETED")
    return accepted, goal


def _require_gate_outbox(
    store: EpistemicSQLiteStore,
    cas: ReceiptCAS,
    *,
    command_id: str,
    attempt_digest: str,
    candidate_id: str,
    evaluation_id: str,
    flag_digest: str,
    snapshot_digest: str,
) -> None:
    rows = store._conn.execute(
        "SELECT ordinal,outbox_id,topic,payload_json,payload_digest "
        "FROM immutable_outbox WHERE command_id=? ORDER BY ordinal",
        (command_id,),
    ).fetchall()
    if len(rows) != 1:
        _fail("accepted gate must emit exactly one immutable outbox intent")
    ordinal, outbox_id, topic, payload_json, payload_digest = rows[0]
    try:
        raw_payload = json.loads(payload_json)
        accepted_outbox = FlagAcceptedOutboxV1.from_payload(raw_payload)
    except (TypeError, ValueError, json.JSONDecodeError) as exc:
        raise ClosureResolutionError(
            "accepted gate outbox authority binding is malformed"
        ) from exc
    if (
        type(ordinal) is not int
        or ordinal != 0
        or outbox_id != f"outbox:flag:{evaluation_id}"
        or topic != "flag.accepted"
        or payload_digest != canonical_digest(raw_payload)
        or accepted_outbox.attempt_digest != attempt_digest
        or accepted_outbox.candidate_id != candidate_id
        or accepted_outbox.evaluation_id != evaluation_id
        or accepted_outbox.flag_digest != flag_digest
        or accepted_outbox.snapshot_digest != snapshot_digest
    ):
        _fail("accepted gate outbox authority binding is malformed")
    try:
        flag_bytes = cas.read_verified(accepted_outbox.flag_object_digest)
        decoded_flag = flag_bytes.decode(
            accepted_outbox.flag_encoding, errors="strict"
        )
    except (CASIntegrityError, OSError, UnicodeError, ValueError) as exc:
        raise ClosureResolutionError(
            "accepted gate flag object failed verification"
        ) from exc
    if (
        len(flag_bytes) != accepted_outbox.flag_byte_count
        or canonical_digest(decoded_flag) != flag_digest
    ):
        _fail("accepted gate flag object is rebound or malformed")


def _validate_drain_and_rebuild(
    store: EpistemicSQLiteStore,
    rows: Sequence[dict[str, Any]],
    scope_digest: str,
    goal: Mapping[str, Any],
    closure: Mapping[str, Any],
    chain: Mapping[str, str],
) -> str:
    drain = _one(
        _rows_by_kind(rows, "EXECUTION_SCOPE_DRAINED"), "EXECUTION_SCOPE_DRAINED"
    )
    drain_payload = _mapping(drain["payload"], "execution drain payload")
    if drain_payload != {"scope_digest": scope_digest}:
        _fail("execution drain does not bind the closed scope")
    if _receipt_for(store, drain) != chain["execution"]:
        _fail("execution receipt does not commit the drain event")
    generation = store.state().execution_generation
    _require_authority_event(
        store,
        drain,
        actor="protocol2-live-session",
        command_id=f"EXECUTION_SCOPE_DRAINED:{generation}",
        event_id=f"event:EXECUTION_SCOPE_DRAINED:{generation}",
        command_payload=drain_payload,
    )
    rebuild = _one(
        _rows_by_kind(rows, "PROJECTION_REBUILD_VERIFIED"),
        "PROJECTION_REBUILD_VERIFIED",
    )
    payload = _mapping(rebuild["payload"], "projection rebuild payload")
    _exact_fields(
        payload,
        frozenset({"after", "before", "equivalent", "scope_digest"}),
        "projection rebuild payload",
    )
    before = _sha256(payload["before"], "projection before digest")
    after = _sha256(payload["after"], "projection after digest")
    if (
        _exact_bool(payload["equivalent"], "projection equivalent") is not True
        or before != after
        or payload["scope_digest"] != scope_digest
    ):
        _fail("projection rebuild is not equivalent for the closed scope")
    if _receipt_for(store, rebuild) != chain["projection_rebuild"]:
        _fail("projection receipt does not commit the rebuild event")
    _require_authority_event(
        store,
        rebuild,
        actor="protocol2-live-session",
        command_id=f"PROJECTION_REBUILD_VERIFIED:{generation}",
        event_id=f"event:PROJECTION_REBUILD_VERIFIED:{generation}",
        command_payload=payload,
    )
    if store.runtime_projection_digest() != after:
        _fail("current runtime projection differs from verified rebuild")
    replayed = _shadow_rebuild_digest(store)
    if replayed != after:
        _fail("independent runtime projection replay diverges")
    if not (goal["seq"] < drain["seq"] < rebuild["seq"] < closure["seq"]):
        _fail("goal, drain, rebuild, and closure order is invalid")
    state = store.state()
    if state.run_execution.value != "stopped" or state.search_mode.value != "paused":
        _fail("execution scope is not durably stopped and paused")
    return after


def _shadow_rebuild_digest(store: EpistemicSQLiteStore) -> str:
    """Replay disposable projections in memory without mutating the source store."""

    connection = sqlite3.connect(":memory:", isolation_level=None)
    try:
        store._conn.backup(connection)
        shadow = EpistemicSQLiteStore(Path(":memory:"), connection)
        return shadow.rebuild_runtime_projections()
    finally:
        connection.close()


def _recompute_components(
    rows: Sequence[dict[str, Any]],
    admissions: Sequence[dict[str, Any]],
    contexts: Sequence[dict[str, Any]],
    settled: Sequence[dict[str, Any]],
    captures: Sequence[dict[str, Any]],
    accepted: Sequence[dict[str, Any]],
) -> tuple[dict[str, str], dict[str, Any]]:
    launches = _rows_by_kind(rows, "WORKER_LAUNCH_PREPARED")
    manifests = _rows_by_kind(rows, "CAPTURE_MANIFEST_ADVANCED")
    permit_body = {
        "admission_event_digests": [row["event_digest"] for row in admissions],
        "launch_event_digests": [row["event_digest"] for row in launches],
    }
    capture_body = {
        "capture_event_digests": [row["event_digest"] for row in captures],
        "manifest_event_digests": [row["event_digest"] for row in manifests],
        "paired": True,
    }
    accepted_capture_digests = {
        row["payload"]["capture_event_digest"] for row in accepted
    }
    gate_input_body = {
        "accepted_event_digests": [row["event_digest"] for row in accepted],
        "capture_event_digests": sorted(accepted_capture_digests),
        "resolves": True,
    }
    usage_body = {
        "settled_event_digests": [row["event_digest"] for row in settled],
        "unknown_event_digests": [],
        "complete": True,
    }
    orphan_body = {
        "ambiguous_attempt_ids": [],
        "orphaned_permit_ids": [],
        "worker_unknown": False,
        "complete": True,
    }
    components = {
        "canonical_permit": canonical_digest(permit_body),
        "capture_manifest": canonical_digest(capture_body),
        "gate_input": canonical_digest(gate_input_body),
        "orphan_summary": canonical_digest(orphan_body),
        "s4e_schema": canonical_digest(_SCHEMA),
        "usage_closure": canonical_digest(usage_body),
    }
    return components, {
        "capture_pairs": True,
        "effects_close": True,
        "gate_closes": True,
        "orphan_free": True,
        "usage_closes": True,
    }


def resolve_s4e_closure(
    *,
    store: EpistemicSQLiteStore,
    cas: ReceiptCAS,
    receipt_chain: Mapping[str, str],
) -> ResolvedS4EClosure:
    """Resolve one production S4-E receipt chain without modifying authority state."""

    if not isinstance(store, EpistemicSQLiteStore):
        raise ClosureResolutionError("store must be EpistemicSQLiteStore")
    if not isinstance(cas, ReceiptCAS):
        raise ClosureResolutionError("cas must be ReceiptCAS")
    try:
        chain = _validate_receipt_chain(receipt_chain)
        with store.stable_read_snapshot():
            store.verify()
            rows = _canonical_event_rows(store)
            if not rows:
                _fail("canonical event log is empty")
            for row in rows:
                _exact_int(row.get("seq"), "canonical event sequence", minimum=1)
                _sha256(row.get("event_digest"), "canonical event digest")
            closure, declaration = _validate_closure_declaration(store, rows, chain)
            scope_digest = declaration["scope_digest"]
            state = store.state()
            canonical_scope = canonical_digest(
                {
                    "execution_generation": state.execution_generation,
                    "run_fence_epoch": state.run_fence_epoch,
                    "run_id": state.run_id,
                }
            )
            if scope_digest != canonical_scope:
                _fail("closure scope is not the canonical execution scope")
            contexts = _validate_admissions(store, rows, scope_digest)
            _validate_supporting_authority(store, rows, contexts)
            settled = _validate_usage(store, rows, contexts)
            _validate_effects(store, rows, contexts)
            captures, capture_by_event = _validate_captures(store, cas, rows, contexts)
            accepted, goal = _validate_gate_and_goal(
                store, cas, rows, contexts, capture_by_event, chain
            )
            projection_digest = _validate_drain_and_rebuild(
                store, rows, scope_digest, goal, closure, chain
            )
            admissions = _rows_by_kind(rows, "ATTEMPT_ADMITTED")
            components, invariants = _recompute_components(
                rows, admissions, contexts, settled, captures, accepted
            )
            if not _same_json(declaration["components"], components):
                _fail("declared S4-E components differ from semantic recomputation")
            if not _same_json(declaration["invariants"], invariants):
                _fail("declared S4-E invariants differ from semantic recomputation")
            for name, digest in components.items():
                if chain[name] != digest:
                    _fail(f"receipt chain fails semantic component {name}")
            return ResolvedS4EClosure(
                scope_digest=scope_digest,
                closure_receipt_digest=chain["s4e_closure"],
                gate_receipt_digest=chain["gate"],
                policy_digest=contexts[0]["policy_digest"],
                projection_digest=projection_digest,
                attempt_count=len(contexts),
                capture_count=len(captures),
                accepted_goal_units=len(accepted),
            )
    except ClosureResolutionError:
        raise
    except (
        CASIntegrityError,
        IntegrityError,
        KeyError,
        OSError,
        sqlite3.DatabaseError,
        TypeError,
        ValueError,
    ) as exc:
        raise ClosureResolutionError("S4-E semantic resolution failed closed") from exc
