from __future__ import annotations

import hashlib
import os
import sqlite3

import pytest

from muteki.epistemic.cas import CASIntegrityError, ReceiptCAS
from muteki.epistemic.folds import KernelHealth, RunExecution, SearchControlMode
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IdempotencyConflict,
    OutboxIntent,
    ProjectionMutation,
)


def _store(tmp_path):
    return EpistemicSQLiteStore.create(
        path=tmp_path / "control" / "run-1" / "epistemic-v2.db",
        run_id="run-1",
        manifest_digest="a" * 64,
    )


def test_command_event_projection_and_outbox_commit_atomically(tmp_path):
    store = _store(tmp_path)
    result = store.commit_command(
        command_id="C-boot",
        idempotency_key="boot-1",
        command_payload={"kind": "boot"},
        committed_at_ns=10,
        events=[
            CommandEvent("E-verify", "BOOT_VERIFYING", "host", 1),
            CommandEvent("E-ready", "BOOT_READY", "host", 2),
        ],
        outbox=[OutboxIntent("O-ready", "run.ready", {"run_id": "run-1"})],
    )
    state = store.state()
    assert result.first_seq == 1 and result.last_seq == 2
    assert state.kernel_health is KernelHealth.READY
    assert store.verify().checksum == state.checksum
    resolved = store.resolve_receipt(result.receipt_digest)
    assert resolved.command_id == "C-boot"
    assert resolved.digest == result.receipt_digest
    assert tuple(resolved.payload["event_digests"]) == tuple(
        row["event_digest"] for row in store.event_rows()
    )
    index = store.receipt_object_index()
    assert index.complete_through_seq == 2
    resolver = store.receipt_field_resolver()
    prefix = resolver.verify_complete_through(2)
    assert prefix.head_event_digest == store.state().head_event_digest
    pointer = resolver.pointer_for(
        result.receipt_digest, "events[1].kind", cutoff_seq=2
    )
    assert resolver.resolve(pointer, cutoff_seq=2).value == "BOOT_READY"
    store.close()


def test_idempotency_replays_original_and_conflicts_on_payload_change(tmp_path):
    store = _store(tmp_path)
    kwargs = dict(
        command_id="C-boot",
        idempotency_key="boot-1",
        command_payload={"kind": "boot"},
        committed_at_ns=10,
        events=[CommandEvent("E-ready", "BOOT_READY", "host", 1)],
    )
    first = store.commit_command(**kwargs)
    second = store.commit_command(**kwargs)
    assert second.idempotent is True
    assert second.receipt_digest == first.receipt_digest
    with pytest.raises(IdempotencyConflict):
        store.commit_command(**{**kwargs, "command_payload": {"kind": "other"}})


def test_fault_before_commit_rolls_back_event_projection_and_outbox(tmp_path):
    store = _store(tmp_path)

    def crash(point):
        if point == "before_commit":
            raise RuntimeError("synthetic crash")

    with pytest.raises(RuntimeError, match="synthetic crash"):
        store.commit_command(
            command_id="C-crash",
            idempotency_key="crash-1",
            command_payload={"kind": "crash"},
            committed_at_ns=10,
            events=[CommandEvent("E-crash", "BOOT_VERIFYING", "host", 1)],
            outbox=[OutboxIntent("O-crash", "never", {})],
            fault_hook=crash,
        )
    assert store.state().head_seq == 0
    assert store._conn.execute("SELECT COUNT(*) FROM events").fetchone()[0] == 0
    assert (
        store._conn.execute("SELECT COUNT(*) FROM immutable_outbox").fetchone()[0] == 0
    )
    assert (
        store._conn.execute("SELECT COUNT(*) FROM command_receipt_objects").fetchone()[
            0
        ]
        == 0
    )


def test_append_only_tables_reject_update_and_delete(tmp_path):
    store = _store(tmp_path)
    store.commit_command(
        command_id="C-boot",
        idempotency_key="boot-1",
        command_payload={},
        committed_at_ns=1,
        events=[CommandEvent("E-ready", "BOOT_READY", "host", 1)],
    )
    with pytest.raises(sqlite3.DatabaseError, match="append-only"):
        store._conn.execute("UPDATE events SET kind='X' WHERE seq=1")
    with pytest.raises(sqlite3.DatabaseError, match="append-only"):
        store._conn.execute("DELETE FROM commands WHERE command_id='C-boot'")
    with pytest.raises(sqlite3.DatabaseError, match="append-only"):
        store._conn.execute("UPDATE command_receipt_objects SET state='unknown'")
    with pytest.raises(sqlite3.DatabaseError, match="append-only"):
        store._conn.execute("DELETE FROM command_receipt_objects")


def test_projection_rebuild_is_bit_equivalent(tmp_path):
    store = _store(tmp_path)
    store.commit_command(
        command_id="C-boot",
        idempotency_key="boot-1",
        command_payload={},
        committed_at_ns=1,
        events=[CommandEvent("E-ready", "BOOT_READY", "host", 1)],
    )
    store.commit_command(
        command_id="C-start",
        idempotency_key="start-1",
        command_payload={},
        committed_at_ns=2,
        events=[
            CommandEvent(
                "E-start",
                "START_EXECUTION",
                "host",
                2,
                {"execution_generation": 1, "run_fence_epoch": 1},
            )
        ],
        projection_mutations=[
            ProjectionMutation(
                "execution_start_guard",
                {"execution_generation": 1, "run_fence_epoch": 1},
            )
        ],
        authority_capability=store._lifecycle_commit_capability,
    )
    before = store.state()
    store._conn.execute(
        "UPDATE state_projection SET state_json='{}',checksum='broken' WHERE singleton=1"
    )
    rebuilt = store.rebuild_projection()
    assert rebuilt.checksum == before.checksum
    assert rebuilt.run_execution is RunExecution.RUNNING
    assert rebuilt.search_mode is SearchControlMode.ACTIVE


@pytest.mark.parametrize(
    "generation,fence",
    [(True, 1), (1, "1"), (1.0, 1)],
)
def test_execution_epoch_fields_are_exact_integers(tmp_path, generation, fence):
    store = _store(tmp_path)
    store.commit_command(
        command_id="ready",
        idempotency_key="ready",
        command_payload={},
        committed_at_ns=1,
        events=[CommandEvent("event:ready", "BOOT_READY", "host", 1)],
    )
    with pytest.raises(ValueError, match="exact integers|floats are not canonical"):
        store.commit_command(
            command_id="start",
            idempotency_key="start",
            command_payload={},
            committed_at_ns=2,
            events=[
                CommandEvent(
                    "event:start",
                    "START_EXECUTION",
                    "host",
                    2,
                    {"execution_generation": generation, "run_fence_epoch": fence},
                )
            ],
            projection_mutations=[
                ProjectionMutation(
                    "execution_start_guard",
                    {"execution_generation": generation, "run_fence_epoch": fence},
                )
            ],
            authority_capability=store._lifecycle_commit_capability,
        )
    assert store.event_rows(kind="START_EXECUTION") == ()


def test_cas_copies_source_and_detects_corruption(tmp_path):
    cas = ReceiptCAS(tmp_path / "cas")
    source = tmp_path / "source.bin"
    source.write_bytes(b"original")
    sealed = cas.seal_file(source)
    source.write_bytes(b"mutated")
    assert cas.read_verified(sealed.digest) == b"original"
    target = cas._path(sealed.digest)
    target.chmod(0o600)
    target.write_bytes(b"corrupt")
    with pytest.raises(CASIntegrityError, match="digest mismatch|unsafe inode"):
        cas.read_verified(sealed.digest)


def test_cas_rejects_root_and_internal_directory_symlinks_without_chmod(
    tmp_path,
):
    victim = tmp_path / "victim"
    victim.mkdir(mode=0o755)
    original_mode = victim.stat().st_mode & 0o777

    root_link = tmp_path / "root-link"
    root_link.symlink_to(victim, target_is_directory=True)
    with pytest.raises(CASIntegrityError, match="CAS root"):
        ReceiptCAS(root_link)
    assert victim.stat().st_mode & 0o777 == original_mode

    for name in ("sha256", "staging"):
        root = tmp_path / f"cas-{name}"
        root.mkdir()
        (root / name).symlink_to(victim, target_is_directory=True)
        with pytest.raises(CASIntegrityError, match=name):
            ReceiptCAS(root)
        assert victim.stat().st_mode & 0o777 == original_mode


def test_cas_rejects_symlinked_digest_prefix_and_object(tmp_path):
    cas = ReceiptCAS(tmp_path / "cas")
    digest = hashlib.sha256(b"sealed").hexdigest()
    victim_dir = tmp_path / "prefix-victim"
    victim_dir.mkdir()
    prefix = cas.objects / digest[:2]
    prefix.symlink_to(victim_dir, target_is_directory=True)
    with pytest.raises(CASIntegrityError, match="digest prefix"):
        cas.seal_bytes(b"sealed")
    assert list(victim_dir.iterdir()) == []

    prefix.unlink()
    prefix.mkdir()
    victim_file = tmp_path / "object-victim"
    victim_file.write_bytes(b"unchanged")
    (prefix / digest[2:]).symlink_to(victim_file)
    with pytest.raises(CASIntegrityError, match="not trusted"):
        cas.seal_bytes(b"sealed")
    assert victim_file.read_bytes() == b"unchanged"


def test_cas_rejects_external_hardlink_alias_without_chmod(tmp_path):
    cas = ReceiptCAS(tmp_path / "cas")
    data = b"hardlink-alias"
    digest = hashlib.sha256(data).hexdigest()
    prefix = cas.objects / digest[:2]
    prefix.mkdir()
    outside = tmp_path / "outside-object"
    outside.write_bytes(data)
    outside.chmod(0o600)
    os.link(outside, prefix / digest[2:])

    with pytest.raises(CASIntegrityError, match="unsafe inode"):
        cas.seal_bytes(data)
    assert outside.read_bytes() == data
    assert outside.stat().st_mode & 0o777 == 0o600
    assert outside.stat().st_nlink == 2


def test_cas_publication_retains_no_hardlink_alias(tmp_path):
    cas = ReceiptCAS(tmp_path / "cas")
    sealed = cas.seal_bytes(b"single-link-object")
    inode = cas._path(sealed.digest).stat()
    assert inode.st_nlink == 1
    assert inode.st_mode & 0o777 == 0o400


def test_cas_rejects_symlink_source(tmp_path):
    cas = ReceiptCAS(tmp_path / "cas")
    source = tmp_path / "source"
    source.write_bytes(b"private")
    link = tmp_path / "source-link"
    link.symlink_to(source)
    with pytest.raises(ValueError, match="non-symlink"):
        cas.seal_file(link)
