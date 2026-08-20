from __future__ import annotations

from pathlib import Path

import pytest

from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import IntegrityError
from muteki.runtime.admission import AdmissionRequest
from muteki.runtime.composition import HostRunFactory
from muteki.runtime.contracts import AttemptIdentity, EffectClass, LeaseIdentity
from muteki.runtime.controller import BootRecoveryCapability
from muteki.runtime.run_catalog import (
    LifecycleUnavailable, RunCatalog, ProvisionUnavailable,
)


class _Artifacts:
    def read_text(self, _aid):
        return ""


def _catalog(tmp_path):
    catalog = RunCatalog.create(root=tmp_path / "control")
    catalog.create_draft(draft_id="draft-1", policy={"protocol": 2}, occurred_at_ns=1)
    attachment = catalog.add_attachment(
        draft_id="draft-1", attachment_id="att-1", data=b"player file",
        occurred_at_ns=2)
    catalog.begin_provision(
        operation_id="provision-1", draft_id="draft-1", run_id="run-1",
        target_root=tmp_path / "runs" / "run-1",
        manifest_digest="a" * 64, owner_epoch=1, occurred_at_ns=3)
    return catalog, attachment


def test_draft_attachment_run_allocation_and_seal(tmp_path):
    catalog, attachment = _catalog(tmp_path)
    run = catalog.materialize(operation_id="provision-1", occurred_at_ns=4)
    assert run["state"] == "sealed"
    assert len(run["anchor_digest"]) == 64
    target = tmp_path / "runs" / "run-1" / "receipt-cas" / "sha256"
    assert target.exists()
    assert attachment["digest"]


def test_crash_after_target_commit_reconciles_same_run_id(tmp_path):
    catalog, _ = _catalog(tmp_path)

    def crash(point):
        if point == "after_target_commit":
            raise RuntimeError("crash window")

    with pytest.raises(RuntimeError, match="crash window"):
        catalog.materialize(operation_id="provision-1", occurred_at_ns=4,
                            fault_hook=crash)
    with pytest.raises(ProvisionUnavailable):
        catalog.sealed_run("run-1")
    run = catalog.reconcile(operation_id="provision-1", occurred_at_ns=5)
    assert run["run_id"] == "run-1" and run["state"] == "sealed"


def test_unknown_or_unsealed_run_cannot_open(tmp_path):
    catalog, _ = _catalog(tmp_path)
    factory = HostRunFactory(catalog=catalog, artifacts=_Artifacts())
    with pytest.raises((KeyError, ProvisionUnavailable)):
        factory.open(run_id="unknown", boot_capability=BootRecoveryCapability(1, 1, "x"),
                     occurred_at_ns=4)


def test_host_factory_boot_attests_then_explicit_start(tmp_path):
    catalog, _ = _catalog(tmp_path)
    catalog.materialize(operation_id="provision-1", occurred_at_ns=4)
    factory = HostRunFactory(catalog=catalog, artifacts=_Artifacts())
    context, ports = factory.open(
        run_id="run-1", boot_capability=BootRecoveryCapability(1, 1, "owner"),
        occurred_at_ns=5)
    assert context.run_id == "run-1"
    assert ports.store.state().execution_generation == 0
    scope, supervisor = factory.start_execution(
        ports=ports, idempotency_key="start-1", occurred_at_ns=6)
    assert scope.execution_generation == 1
    assert supervisor.active_count == 0


def test_catalog_projections_rebuild_from_events(tmp_path):
    catalog, _ = _catalog(tmp_path)
    catalog.materialize(operation_id="provision-1", occurred_at_ns=4)
    before = catalog._store.runtime_projection_digest()
    after = catalog._store.rebuild_runtime_projections()
    assert after == before
    assert catalog.sealed_run("run-1")["state"] == "sealed"


def test_archive_and_purge_saga_is_receipted_rebuildable_and_idempotent(tmp_path):
    catalog, _ = _catalog(tmp_path)
    catalog.materialize(operation_id="provision-1", occurred_at_ns=4)

    archived = catalog.request_archive(
        operation_id="archive-1", run_id="run-1", owner_epoch=1,
        occurred_at_ns=5)
    assert archived["state"] == "archived"
    assert len(archived["archive_receipt_digest"]) == 64
    assert catalog.run_view("run-1")["state"] == "archived"

    items = (
        {"locator": "display-log", "adapter": "file"},
        {"locator": "run-tree", "adapter": "tree"},
    )
    pending = catalog.begin_purge(
        operation_id="purge-1", run_id="run-1", owner_epoch=1,
        items=items, occurred_at_ns=8)
    assert pending["state"] == "purge_pending"
    assert len(pending["plan_receipt_digest"]) == 64
    for item in pending["items"]:
        catalog.record_purge_item_absent(
            operation_id="purge-1", ordinal=item["ordinal"],
            locator=item["locator"], adapter=item["adapter"],
            already_absent=False, occurred_at_ns=9 + item["ordinal"])
    purged = catalog.complete_purge(operation_id="purge-1", occurred_at_ns=20)
    assert purged["state"] == "purged"
    assert len(purged["absence_receipt_digest"]) == 64
    assert catalog.run_view("run-1")["state"] == "purged"

    # Same operation and same plan is byte-identical and causes no second event.
    head = catalog._store.state().head_seq
    assert catalog.begin_purge(
        operation_id="purge-1", run_id="run-1", owner_epoch=1,
        items=items, occurred_at_ns=99) == purged
    assert catalog.complete_purge(operation_id="purge-1", occurred_at_ns=100) == purged
    assert catalog._store.state().head_seq == head
    before = catalog._store.runtime_projection_digest()
    assert catalog._store.rebuild_runtime_projections() == before


def test_archive_settled_owner_check_is_atomic_in_run_store(tmp_path):
    catalog, _ = _catalog(tmp_path)
    catalog.materialize(operation_id="provision-1", occurred_at_ns=4)
    factory = HostRunFactory(catalog=catalog, artifacts=_Artifacts())
    _context, ports = factory.open(
        run_id="run-1", boot_capability=BootRecoveryCapability(1, 1, "owner"),
        occurred_at_ns=5)
    scope, _supervisor = factory.start_execution(
        ports=ports, idempotency_key="start", occurred_at_ns=6)
    ports.admission.create_branch(branch_id="root", max_attempts=1,
                                  occurred_at_ns=7)
    ports.admission.create_budget_account(
        account_id="run", limits={"attempts": 1, "tokens": 10},
        occurred_at_ns=8)
    attempt = AttemptIdentity(scope, "root", "attempt-1", 1)
    lease = LeaseIdentity(attempt, "lease-1", 1, 1)
    ports.admission.admit(AdmissionRequest(
        attempt=attempt, lease=lease, permit_id="permit-1", account_id="run",
        requested_budget={"attempts": 1, "tokens": 1},
        conflict_keys=("worker:one",), effect_class=EffectClass.OBSERVABLE,
        fingerprint=canonical_digest({"attempt": 1}), policy_digest="c" * 64,
        expires_at_ns=10_000), occurred_at_ns=9)
    ports.store.close()

    with pytest.raises(IntegrityError, match="unsettled"):
        catalog.request_archive(
            operation_id="archive-held", run_id="run-1", owner_epoch=1,
            occurred_at_ns=10)
    target = catalog.run_view("run-1")["target_root"]
    from muteki.epistemic.sqlite_store import EpistemicSQLiteStore
    reopened = EpistemicSQLiteStore.open(Path(target) / "epistemic-v2.db")
    try:
        assert reopened.event_rows(kind="RUN_ARCHIVE_REQUESTED") == ()
        assert reopened.state().run_execution.value == "running"
    finally:
        reopened.close()


def test_purge_plan_is_immutable(tmp_path):
    catalog, _ = _catalog(tmp_path)
    catalog.materialize(operation_id="provision-1", occurred_at_ns=4)
    catalog.request_archive(
        operation_id="archive-1", run_id="run-1", owner_epoch=1,
        occurred_at_ns=5)
    catalog.begin_purge(
        operation_id="purge-1", run_id="run-1", owner_epoch=1,
        items=({"locator": "one", "adapter": "file"},), occurred_at_ns=8)
    with pytest.raises(LifecycleUnavailable, match="immutable"):
        catalog.begin_purge(
            operation_id="purge-1", run_id="run-1", owner_epoch=1,
            items=({"locator": "two", "adapter": "file"},), occurred_at_ns=9)
