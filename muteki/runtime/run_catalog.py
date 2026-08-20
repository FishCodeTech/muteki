"""Draft -> provision operation -> immutable sealed RunId lifecycle."""

from __future__ import annotations

from collections.abc import Callable, Mapping
from pathlib import Path

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import (
    CommandEvent, EpistemicSQLiteStore, IntegrityError, ProjectionMutation,
)


class ProvisionUnavailable(RuntimeError):
    pass


class LifecycleUnavailable(RuntimeError):
    pass


class RunCatalog:
    def __init__(self, *, store: EpistemicSQLiteStore,
                 staging_cas: ReceiptCAS) -> None:
        self._store = store
        self._staging_cas = staging_cas

    @classmethod
    def create(cls, *, root: Path) -> "RunCatalog":
        root = Path(root)
        store = EpistemicSQLiteStore.create(
            path=root / "catalog-v2.db", run_id="__run_catalog__",
            manifest_digest=canonical_digest({"kind": "run-catalog", "version": 2}),
            durability_tier="D1_HOST",
        )
        return cls(store=store, staging_cas=ReceiptCAS(root / "draft-cas"))

    @classmethod
    def open_or_create(cls, *, root: Path) -> "RunCatalog":
        root = Path(root)
        path = root / "catalog-v2.db"
        if not path.exists():
            return cls.create(root=root)
        return cls(
            store=EpistemicSQLiteStore.open(path),
            staging_cas=ReceiptCAS(root / "draft-cas"),
        )

    def create_draft(self, *, draft_id: str, policy: Mapping,
                     occurred_at_ns: int) -> str:
        payload = {"draft_id": draft_id, "policy": policy}
        result = self._store.commit_command(
            command_id=f"draft:create:{draft_id}",
            idempotency_key=f"draft:create:{draft_id}", command_payload=payload,
            events=[CommandEvent(
                f"event:draft:create:{draft_id}", "DRAFT_CREATED",
                "run-catalog", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("draft_create", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def add_attachment(self, *, draft_id: str, attachment_id: str, data: bytes,
                       occurred_at_ns: int) -> dict:
        sealed = self._staging_cas.seal_bytes(data)
        payload = {"attachment_id": attachment_id, "byte_count": sealed.byte_count,
                   "digest": sealed.digest, "draft_id": draft_id}
        result = self._store.commit_command(
            command_id=f"draft:attachment:{attachment_id}",
            idempotency_key=f"draft:attachment:{attachment_id}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:draft:attachment:{attachment_id}", "DRAFT_ATTACHMENT_SEALED",
                "run-catalog", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("draft_attachment", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return {**payload, "receipt_digest": result.receipt_digest}

    def begin_provision(self, *, operation_id: str, draft_id: str, run_id: str,
                        target_root: Path, manifest_digest: str, owner_epoch: int,
                        occurred_at_ns: int) -> str:
        payload = {"draft_id": draft_id, "manifest_digest": manifest_digest,
                   "operation_id": operation_id, "owner_epoch": owner_epoch,
                   "run_id": run_id, "target_root": str(Path(target_root).resolve())}
        result = self._store.commit_command(
            command_id=f"provision:begin:{operation_id}",
            idempotency_key=f"provision:begin:{operation_id}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:provision:begin:{operation_id}", "RUN_ID_ALLOCATED",
                "run-catalog", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("provision_begin", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def materialize(self, *, operation_id: str, occurred_at_ns: int,
                    fault_hook: Callable[[str], None] | None = None) -> dict:
        status = self._store.provision_status(operation_id)
        if status["state"] == "sealed":
            return self._store.catalog_run(status["run_id"])
        if status["state"] not in {"run_allocated", "run_materialized"}:
            raise ProvisionUnavailable(f"cannot materialize from {status['state']}")
        root = Path(status["target_root"])
        root.mkdir(parents=True, exist_ok=True, mode=0o700)
        target_cas = ReceiptCAS(root / "receipt-cas")
        attachment_digests: list[str] = []
        for attachment in self._store.draft_attachments(status["draft_id"]):
            sealed = target_cas.seal_bytes(
                self._staging_cas.read_verified(attachment["digest"]))
            if sealed.digest != attachment["digest"]:
                raise IntegrityError("target CAS digest differs from frozen draft")
            attachment_digests.append(sealed.digest)

        db_path = root / "epistemic-v2.db"
        if db_path.exists():
            run_store = EpistemicSQLiteStore.open(db_path)
            anchor = run_store.run_anchor()
            if (anchor["run_id"] != status["run_id"]
                    or anchor["manifest_digest"] != status["manifest_digest"]):
                raise IntegrityError("materialized run anchor mismatch")
        else:
            run_store = EpistemicSQLiteStore.create(
                path=db_path, run_id=status["run_id"],
                manifest_digest=status["manifest_digest"], durability_tier="D1_HOST")
        create = run_store.commit_command(
            command_id="CREATE_RUN", idempotency_key="CREATE_RUN",
            command_payload={"attachment_digests": attachment_digests,
                             "manifest_digest": status["manifest_digest"]},
            events=[CommandEvent(
                "event:CREATE_RUN", "RUN_CREATED", "provisioning-authority",
                occurred_at_ns,
                {"attachment_digests": attachment_digests,
                 "manifest_digest": status["manifest_digest"]})],
            committed_at_ns=occurred_at_ns,
        )
        run_store.verify()
        anchor_digest = canonical_digest({
            "attachment_digests": attachment_digests,
            "create_receipt_digest": create.receipt_digest,
            "manifest_digest": status["manifest_digest"],
            "run_id": status["run_id"],
            "state_checksum": run_store.state().checksum,
        })
        run_store.close()
        if fault_hook:
            fault_hook("after_target_commit")
        if status["state"] == "run_allocated":
            payload = {"operation_id": operation_id,
                       "owner_epoch": status["owner_epoch"]}
            self._store.commit_command(
                command_id=f"provision:materialized:{operation_id}",
                idempotency_key=f"provision:materialized:{operation_id}",
                command_payload=payload,
                events=[CommandEvent(
                    f"event:provision:materialized:{operation_id}",
                    "RUN_MATERIALIZED", "run-catalog", occurred_at_ns, payload)],
                projection_mutations=[ProjectionMutation(
                    "provision_materialized", payload)],
                committed_at_ns=occurred_at_ns,
            )
        if fault_hook:
            fault_hook("after_materialized")
        payload = {"anchor_digest": anchor_digest, "operation_id": operation_id,
                   "owner_epoch": status["owner_epoch"], "run_id": status["run_id"]}
        self._store.commit_command(
            command_id=f"provision:sealed:{operation_id}",
            idempotency_key=f"provision:sealed:{operation_id}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:provision:sealed:{operation_id}", "RUN_SEALED",
                "run-catalog", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("provision_sealed", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return self._store.catalog_run(status["run_id"])

    def reconcile(self, *, operation_id: str, occurred_at_ns: int) -> dict:
        return self.materialize(operation_id=operation_id,
                                occurred_at_ns=occurred_at_ns)

    def sealed_run(self, run_id: str) -> dict:
        run = self._store.catalog_run(run_id)
        if run["state"] != "sealed":
            raise ProvisionUnavailable("run is not sealed")
        status = self._store.provision_status(run["operation_id"])
        return {**run, "target_root": status["target_root"]}

    def has_run(self, run_id: str) -> bool:
        try:
            self._store.catalog_run(run_id)
            return True
        except KeyError:
            return False

    def list_run_ids(self) -> tuple[str, ...]:
        """Enumerate the local catalog inventory in stable RunId order."""

        with self._store.stable_read_snapshot():
            rows = self._store._conn.execute(
                "SELECT run_id FROM catalog_runs ORDER BY run_id"
            ).fetchall()
        return tuple(str(row[0]) for row in rows)

    def run_view(self, run_id: str) -> dict:
        run = self._store.catalog_run(run_id)
        provision = self._store.provision_status(run["operation_id"])
        return {**run, "target_root": provision["target_root"]}

    def request_archive(self, *, operation_id: str, run_id: str,
                        owner_epoch: int, occurred_at_ns: int) -> dict:
        """Drain and archive one run; retry rolls forward the same operation."""
        try:
            existing = self._store.archive_status(operation_id)
        except KeyError:
            existing = None
        if existing is not None:
            if existing["run_id"] != run_id or existing["owner_epoch"] != owner_epoch:
                raise LifecycleUnavailable("archive operation identity conflict")
            if existing["state"] == "archived":
                return existing
        else:
            payload = {
                "operation_id": operation_id,
                "owner_epoch": int(owner_epoch),
                "requested_at_ns": int(occurred_at_ns),
                "run_id": run_id,
            }
            self._store.commit_command(
                command_id=f"archive:request:{operation_id}",
                idempotency_key=f"archive:request:{operation_id}",
                command_payload=payload,
                events=[CommandEvent(
                    f"event:archive:request:{operation_id}",
                    "CATALOG_ARCHIVE_REQUESTED", "run-catalog",
                    occurred_at_ns, payload)],
                projection_mutations=[ProjectionMutation("archive_begin", payload)],
                committed_at_ns=occurred_at_ns,
            )

        run = self.run_view(run_id)
        target = Path(run["target_root"]) / "epistemic-v2.db"
        if not target.is_file():
            raise LifecycleUnavailable("archive target store is unavailable")
        run_store = EpistemicSQLiteStore.open(target)
        try:
            state = run_store.state()
            rows = run_store.event_rows()
            sealed_closure = (
                bool(rows)
                and rows[-1]["kind"] == "S4E_CLOSURE_ATTESTED"
                and state.run_execution.value == "stopped"
                and state.search_mode.value == "paused"
            )
            if sealed_closure:
                run_receipt_digest = run_store.resolve_receipt_for_event(
                    rows[-1]["event_digest"]
                ).digest
            elif state.run_execution.value != "archived":
                request = run_store.commit_command(
                    command_id=f"RUN_ARCHIVE_REQUESTED:{operation_id}",
                    idempotency_key=f"RUN_ARCHIVE_REQUESTED:{operation_id}",
                    command_payload={"operation_id": operation_id,
                                     "owner_epoch": int(owner_epoch)},
                    events=[CommandEvent(
                        f"event:RUN_ARCHIVE_REQUESTED:{operation_id}",
                        "RUN_ARCHIVE_REQUESTED", "lifecycle-authority",
                        occurred_at_ns,
                        {"operation_id": operation_id,
                         "owner_epoch": int(owner_epoch)})],
                    projection_mutations=[ProjectionMutation(
                        "archive_assert_settled", {"operation_id": operation_id})],
                    committed_at_ns=occurred_at_ns,
                )
                archived = run_store.commit_command(
                    command_id=f"RUN_ARCHIVED:{operation_id}",
                    idempotency_key=f"RUN_ARCHIVED:{operation_id}",
                    command_payload={"operation_id": operation_id,
                                     "request_receipt_digest": request.receipt_digest},
                    events=[CommandEvent(
                        f"event:RUN_ARCHIVED:{operation_id}", "RUN_ARCHIVED",
                        "lifecycle-authority", occurred_at_ns + 1,
                        {"operation_id": operation_id,
                         "request_receipt_digest": request.receipt_digest})],
                    committed_at_ns=occurred_at_ns + 1,
                )
                run_receipt_digest = archived.receipt_digest
            else:
                archived_rows = run_store.event_rows(kind="RUN_ARCHIVED")
                if not archived_rows:
                    raise LifecycleUnavailable("archived state lacks canonical receipt")
                run_receipt_digest = run_store.resolve_receipt_for_event(
                    archived_rows[-1]["event_digest"]
                ).digest
            owner_summary = run_store.lifecycle_owner_summary()
            if any(owner_summary.values()):
                raise LifecycleUnavailable("archive owner settlement is incomplete")
            archive_receipt_digest = canonical_digest({
                "operation_id": operation_id,
                "owner_epoch": int(owner_epoch),
                "run_id": run_id,
                "run_receipt_digest": run_receipt_digest,
                "state_checksum": run_store.verify().checksum,
            })
        finally:
            run_store.close()

        payload = {
            "archive_receipt_digest": archive_receipt_digest,
            "operation_id": operation_id,
            "owner_epoch": int(owner_epoch),
            "run_id": run_id,
            "run_receipt_digest": run_receipt_digest,
        }
        self._store.commit_command(
            command_id=f"archive:complete:{operation_id}",
            idempotency_key=f"archive:complete:{operation_id}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:archive:complete:{operation_id}",
                "CATALOG_RUN_ARCHIVED", "run-catalog",
                occurred_at_ns + 2, payload)],
            projection_mutations=[ProjectionMutation("archive_complete", payload)],
            committed_at_ns=occurred_at_ns + 2,
        )
        return self._store.archive_status(operation_id)

    def begin_purge(self, *, operation_id: str, run_id: str, owner_epoch: int,
                    items: tuple[Mapping, ...], occurred_at_ns: int) -> dict:
        """Seal an external purge plan before any destructive adapter is called."""
        try:
            existing = self._store.purge_status(operation_id)
        except KeyError:
            existing = None
        if existing is not None:
            if existing["run_id"] != run_id or existing["owner_epoch"] != owner_epoch:
                raise LifecycleUnavailable("purge operation identity conflict")
            expected = tuple({"adapter": str(item["adapter"]),
                              "locator": str(item["locator"])} for item in items)
            if existing["plan_digest"] != canonical_digest({
                    "items": expected, "operation_id": operation_id,
                    "owner_epoch": int(owner_epoch), "run_id": run_id}):
                raise LifecycleUnavailable("purge plan is immutable")
            return existing

        run = self.run_view(run_id)
        if run["state"] != "archived":
            raise LifecycleUnavailable("purge requires an archived run")
        target = Path(run["target_root"]) / "epistemic-v2.db"
        if not target.is_file():
            raise LifecycleUnavailable("purge target store is unavailable before plan seal")
        run_store = EpistemicSQLiteStore.open(target)
        try:
            state = run_store.state()
            rows = run_store.event_rows()
            sealed_closure = (
                bool(rows)
                and rows[-1]["kind"] == "S4E_CLOSURE_ATTESTED"
                and state.run_execution.value == "stopped"
                and state.search_mode.value == "paused"
            )
            if state.run_execution.value != "archived" and not sealed_closure:
                raise LifecycleUnavailable("run store is not canonically archived")
            if any(run_store.lifecycle_owner_summary().values()):
                raise LifecycleUnavailable("purge owner settlement is incomplete")
        finally:
            run_store.close()

        normalized = tuple({"adapter": str(item["adapter"]),
                            "locator": str(item["locator"])} for item in items)
        plan = {"items": normalized, "operation_id": operation_id,
                "owner_epoch": int(owner_epoch), "run_id": run_id}
        plan_digest = canonical_digest(plan)
        plan_receipt_digest = canonical_digest({
            "kind": "PurgePlanReceipt", "plan_digest": plan_digest,
            "catalog_anchor": self._store.run_anchor(),
        })
        payload = {**plan, "plan_digest": plan_digest,
                   "plan_receipt_digest": plan_receipt_digest,
                   "requested_at_ns": int(occurred_at_ns)}
        self._store.commit_command(
            command_id=f"purge:plan:{operation_id}",
            idempotency_key=f"purge:plan:{operation_id}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:purge:plan:{operation_id}", "PURGE_PLAN_SEALED",
                "run-catalog", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("purge_begin", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return self._store.purge_status(operation_id)

    def record_purge_item_absent(
        self, *, operation_id: str, ordinal: int, locator: str, adapter: str,
        already_absent: bool, occurred_at_ns: int,
    ) -> dict:
        status = self._store.purge_status(operation_id)
        item = next((row for row in status["items"]
                     if row["ordinal"] == int(ordinal)), None)
        if item is None or item["locator"] != locator or item["adapter"] != adapter:
            raise LifecycleUnavailable("purge item is outside the sealed plan")
        if item["state"] == "absent":
            return status
        action_receipt_digest = canonical_digest({
            "adapter": adapter, "already_absent": bool(already_absent),
            "locator": locator, "operation_id": operation_id,
            "ordinal": int(ordinal), "result": "delete_returned",
        })
        absence_receipt_digest = canonical_digest({
            "action_receipt_digest": action_receipt_digest,
            "adapter": adapter, "locator": locator,
            "operation_id": operation_id, "ordinal": int(ordinal),
            "readback": "absent",
        })
        payload = {
            "action_receipt_digest": action_receipt_digest,
            "adapter": adapter,
            "absence_receipt_digest": absence_receipt_digest,
            "locator": locator,
            "operation_id": operation_id,
            "ordinal": int(ordinal),
        }
        self._store.commit_command(
            command_id=f"purge:item:{operation_id}:{ordinal}:absent",
            idempotency_key=f"purge:item:{operation_id}:{ordinal}:absent",
            command_payload=payload,
            events=[CommandEvent(
                f"event:purge:item:{operation_id}:{ordinal}:absent",
                "PURGE_ITEM_ABSENT", "run-catalog", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("purge_item_absent", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return self._store.purge_status(operation_id)

    def record_purge_item_unknown(
        self, *, operation_id: str, ordinal: int, locator: str, adapter: str,
        error_class: str, occurred_at_ns: int,
    ) -> dict:
        action_receipt_digest = canonical_digest({
            "adapter": adapter, "error_class": str(error_class),
            "locator": locator, "operation_id": operation_id,
            "ordinal": int(ordinal), "result": "unknown",
        })
        payload = {"action_receipt_digest": action_receipt_digest,
                   "adapter": adapter, "locator": locator,
                   "operation_id": operation_id, "ordinal": int(ordinal)}
        self._store.commit_command(
            command_id=f"purge:item:{operation_id}:{ordinal}:unknown",
            idempotency_key=f"purge:item:{operation_id}:{ordinal}:unknown",
            command_payload=payload,
            events=[CommandEvent(
                f"event:purge:item:{operation_id}:{ordinal}:unknown",
                "PURGE_ITEM_UNKNOWN", "run-catalog", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("purge_item_unknown", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return self._store.purge_status(operation_id)

    def complete_purge(self, *, operation_id: str,
                       occurred_at_ns: int) -> dict:
        status = self._store.purge_status(operation_id)
        if status["state"] == "purged":
            return status
        if any(item["state"] != "absent" for item in status["items"]):
            raise LifecycleUnavailable("purge has incomplete absence readback")
        absence_receipt_digest = canonical_digest({
            "item_absence_receipts": tuple(
                item["absence_receipt_digest"] for item in status["items"]),
            "operation_id": operation_id,
            "plan_digest": status["plan_digest"],
            "run_id": status["run_id"],
        })
        payload = {
            "absence_receipt_digest": absence_receipt_digest,
            "operation_id": operation_id,
            "owner_epoch": status["owner_epoch"],
            "plan_digest": status["plan_digest"],
            "purged_at_ns": int(occurred_at_ns),
            "run_id": status["run_id"],
        }
        self._store.commit_command(
            command_id=f"purge:complete:{operation_id}",
            idempotency_key=f"purge:complete:{operation_id}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:purge:complete:{operation_id}", "PURGE_COMPLETED",
                "run-catalog", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("purge_complete", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return self._store.purge_status(operation_id)
