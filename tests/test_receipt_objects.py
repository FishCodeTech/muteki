from __future__ import annotations

from dataclasses import replace

import pytest

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import (
    CanonicalReceipt,
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
)
from muteki.epistemic.receipt_objects import (
    CanonicalCommandReceiptResolverV1,
    CommandReceiptObjectIndexV1,
    CommandReceiptObjectV1,
    HindsightReceiptError,
    ReceiptObjectIndexEntryV1,
    ReceiptObjectState,
    ReceiptProjectionMutationV1,
    ReboundReceiptError,
    UnknownReceiptError,
    UnresolvedReceiptError,
)


def _digest(label: str) -> str:
    return canonical_digest({"label": label})


def _receipt_object(
    *,
    command_id: str,
    first_seq: int,
    parent_event_digest: str,
    event_kind: str,
    event_payload: dict[str, object],
    mutation_payload: dict[str, object] | None = None,
) -> CommandReceiptObjectV1:
    command_payload = {"request": command_id}
    event = EventEnvelopeV2(
        event_id=f"event:{command_id}",
        run_id="run-receipts",
        command_id=command_id,
        ordinal=0,
        kind=event_kind,
        actor="host-authority",
        occurred_at_ns=first_seq * 10,
        payload=event_payload,
        parent_event_digest=parent_event_digest,
    )
    mutations = (
        ReceiptProjectionMutationV1(
            kind="record_test", payload=mutation_payload or {"command": command_id}
        ),
    )
    receipt = CanonicalReceipt(
        receipt_id=f"receipt:{command_id}",
        run_id="run-receipts",
        command_id=command_id,
        kind="COMMAND_COMMITTED",
        payload={
            "command_payload_digest": canonical_digest(command_payload),
            "event_digests": [event.digest],
            "first_seq": first_seq,
            "last_seq": first_seq,
            "outbox": [],
            "projection_mutation_digest": canonical_digest(
                [item.canonical_body() for item in mutations]
            ),
            "state_checksum": _digest(f"state:{command_id}"),
        },
    )
    return CommandReceiptObjectV1(
        receipt=receipt,
        command_payload=command_payload,
        events=(event,),
        outbox=(),
        projection_mutations=mutations,
        committed_at_ns=first_seq * 10 + 1,
    )


def _resolver(tmp_path):
    cas = ReceiptCAS(tmp_path / "cas")
    first = _receipt_object(
        command_id="decision-1",
        first_seq=1,
        parent_event_digest="",
        event_kind="DECISION_NEED_REGISTERED",
        event_payload={
            "question": "Which observation distinguishes the alternatives?",
            "actions": ["inspect_receipt", "run_probe"],
        },
    )
    second = _receipt_object(
        command_id="observation-1",
        first_seq=2,
        parent_event_digest=first.events[-1].digest,
        event_kind="OBSERVATION_RECORDED",
        event_payload={
            "observation": "A stable causal difference was captured.",
            "confidence_bp": 9000,
        },
    )
    first_sealed = first.seal(cas)
    second_sealed = second.seal(cas)
    index = CommandReceiptObjectIndexV1(
        run_id="run-receipts",
        complete_through_seq=2,
        head_event_digest=second.events[-1].digest,
        entries=(
            ReceiptObjectIndexEntryV1.resolved(first, first_sealed),
            ReceiptObjectIndexEntryV1.resolved(second, second_sealed),
        ),
    )
    return CanonicalCommandReceiptResolverV1(index=index, cas=cas), first, second


def test_complete_receipt_objects_roundtrip_and_resolve_exact_field(tmp_path):
    resolver, first, second = _resolver(tmp_path)

    decoded = CommandReceiptObjectV1.from_bytes(second.bytes)
    assert decoded.bytes == second.bytes
    assert decoded.object_digest == second.object_digest

    prefix = resolver.verify_complete_through(2)
    assert prefix.head_event_digest == second.events[-1].digest
    assert prefix.receipt_digests == (
        first.receipt.digest,
        second.receipt.digest,
    )

    pointer = resolver.pointer_for(
        second.receipt.digest,
        "events[0].payload.observation",
        cutoff_seq=2,
    )
    resolved = resolver.resolve(pointer, cutoff_seq=2)
    assert resolved.value == "A stable causal difference was captured."
    assert resolved.event_kind == "OBSERVATION_RECORDED"
    assert resolved.event_global_seq == 2
    assert resolved.pointer.event_digest == second.events[0].digest


def test_receipt_object_parser_rejects_noncanonical_or_incomplete_bytes(tmp_path):
    _, _, second = _resolver(tmp_path)
    pretty = canonical_json_bytes(second.canonical_body()).decode().replace(
        ":", ": ", 1
    )
    with pytest.raises(ValueError, match="not canonical JSON"):
        CommandReceiptObjectV1.from_bytes(pretty.encode())

    body = second.canonical_body()
    body.pop("projection_mutations")
    with pytest.raises(ValueError, match="incomplete or unversioned"):
        CommandReceiptObjectV1.from_bytes(canonical_json_bytes(body))


def test_resolver_rejects_hindsight_and_rebound_pointers(tmp_path):
    resolver, first, second = _resolver(tmp_path)
    later = resolver.pointer_for(
        second.receipt.digest, "events[0].payload.observation"
    )
    with pytest.raises(HindsightReceiptError, match="later than"):
        resolver.resolve(later, cutoff_seq=1)

    earlier = resolver.pointer_for(
        first.receipt.digest, "events[0].payload.question", cutoff_seq=1
    )
    with pytest.raises(ReboundReceiptError, match="field value"):
        resolver.resolve(replace(earlier, value_digest=_digest("fabricated")))
    with pytest.raises(ReboundReceiptError, match="another run"):
        resolver.resolve(replace(earlier, run_id="run-rebound"))


@pytest.mark.parametrize(
    ("state", "error"),
    (
        (ReceiptObjectState.UNRESOLVED, UnresolvedReceiptError),
        (ReceiptObjectState.UNKNOWN, UnknownReceiptError),
        (ReceiptObjectState.REBOUND, ReboundReceiptError),
    ),
)
def test_nonresolved_index_state_fails_closed(tmp_path, state, error):
    cas = ReceiptCAS(tmp_path / "cas")
    entry = ReceiptObjectIndexEntryV1(
        run_id="run-receipts",
        command_id="missing-1",
        receipt_digest=_digest("missing-receipt"),
        first_seq=1,
        last_seq=1,
        state=state,
        diagnostic_receipt_digest=_digest(f"diagnostic:{state.value}"),
    )
    index = CommandReceiptObjectIndexV1(
        run_id="run-receipts",
        complete_through_seq=1,
        head_event_digest=_digest("declared-head"),
        entries=(entry,),
    )
    resolver = CanonicalCommandReceiptResolverV1(index=index, cas=cas)
    with pytest.raises(error):
        resolver.verify_complete_through(1)


def test_missing_cas_object_is_unknown_not_an_empty_receipt(tmp_path):
    resolver, _, _ = _resolver(tmp_path)
    entry = resolver.index.entries[0]
    missing = replace(entry, object_digest=_digest("absent-object"))
    index = CommandReceiptObjectIndexV1(
        run_id=resolver.index.run_id,
        complete_through_seq=2,
        head_event_digest=resolver.index.head_event_digest,
        entries=(missing, resolver.index.entries[1]),
    )
    broken = CanonicalCommandReceiptResolverV1(
        index=index, cas=ReceiptCAS(tmp_path / "cas")
    )
    with pytest.raises(UnknownReceiptError, match="unavailable"):
        broken.verify_complete_through(2)


def test_index_requires_a_contiguous_command_prefix(tmp_path):
    resolver, _, _ = _resolver(tmp_path)
    with pytest.raises(ValueError, match="contiguous command prefix"):
        CommandReceiptObjectIndexV1(
            run_id="run-receipts",
            complete_through_seq=2,
            head_event_digest=resolver.index.head_event_digest,
            entries=(replace(resolver.index.entries[1], first_seq=2),),
        )


def test_index_manifest_roundtrip_is_byte_identical(tmp_path):
    resolver, _, _ = _resolver(tmp_path)
    decoded = CommandReceiptObjectIndexV1.from_bytes(resolver.index.bytes)
    assert decoded.bytes == resolver.index.bytes
    assert decoded.digest == resolver.index.digest
