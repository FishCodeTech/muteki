from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import Any, Awaitable, Callable, Literal, Optional

from muteki.platform.contracts.base import new_id, utcnow
from muteki.platform.contracts.objects import ResourceLease

from .artifacts import SrcArtifactStore
from .store import SrcStore, SrcStoreError

RestorationState = Literal[
    "restored", "partially_restored", "not_restored", "unknown", "not_applicable",
]
RESTORATION_STATES = frozenset({
    "restored", "partially_restored", "not_restored", "unknown", "not_applicable",
})


class SrcObjectService:
    """Task-scoped business objects, experiments and fenced mutation leases."""

    def __init__(self, store: SrcStore, artifacts: SrcArtifactStore) -> None:
        self.store = store
        self.artifacts = artifacts

    @staticmethod
    def participant(task_id: str, context_id: str, actor: dict[str, Any]) -> str:
        thread_id = str(actor.get("thread_id") or "")
        run_id = str(actor.get("run_id") or "")
        worker_id = str(actor.get("worker_id") or "")
        if bool(thread_id) == bool(run_id):
            raise SrcStoreError(
                "task_binding_invalid",
                "participant requires exactly one of thread_id or run_id",
            )
        return f"{task_id}:{context_id}:{thread_id or run_id}:{worker_id}"

    @staticmethod
    def lease_key(
        task_id: str, context_id: str, resource_id: str, *,
        resource_kind: str = "src.business_object",
    ) -> str:
        if resource_kind == "src.business_object":
            return f"{task_id}:{resource_id}"
        return f"{task_id}:{context_id}:{resource_id}"

    def acquire_lease(
        self, task_id: str, context_id: str, resource_kind: str,
        resource_id: str, *, owner: str, ttl_seconds: int = 600,
    ) -> ResourceLease:
        if not owner.startswith(f"{task_id}:{context_id}:"):
            raise SrcStoreError("task_binding_invalid", "lease owner is outside this Task identity")
        if not resource_kind.startswith("src.") or not resource_id:
            raise SrcStoreError("task_binding_invalid", "invalid SRC lease resource")
        key = self.lease_key(task_id, context_id, resource_id, resource_kind=resource_kind)
        previous = self.store.platform.get(ResourceLease, resource_kind, key)
        now = utcnow()
        lease = ResourceLease(
            resource_kind=resource_kind, resource_key=key, owner=owner,
            scope=f"src:{task_id}", ttl_seconds=max(1, min(int(ttl_seconds), 86400)),
            fencing_token=(previous.fencing_token + 1) if previous else 1,
            acquired_at=now, expires_at=now + timedelta(seconds=max(1, min(int(ttl_seconds), 86400))),
            released_at=None,
        )
        try:
            return self.store.platform.acquire_lease(lease)
        except Exception as exc:
            if type(exc).__name__ == "StateConflictError":
                raise SrcStoreError(
                    "resource_busy", "resource is leased by another participant",
                    detail={"resource_kind": resource_kind, "resource_id": resource_id},
                ) from exc
            raise

    def assert_lease(
        self, task_id: str, context_id: str, resource_kind: str,
        resource_id: str, *, owner: str, fencing_token: int,
    ) -> ResourceLease:
        key = self.lease_key(task_id, context_id, resource_id, resource_kind=resource_kind)
        lease = self.store.platform.get(ResourceLease, resource_kind, key)
        if (
            lease is None or lease.scope != f"src:{task_id}"
            or lease.owner != owner or lease.fencing_token != fencing_token
            or lease.released_at is not None
            or (lease.expires_at is not None and lease.expires_at <= utcnow())
        ):
            raise SrcStoreError(
                "resource_busy", "resource lease is missing, expired or fenced",
                detail={"resource_kind": resource_kind, "resource_id": resource_id},
            )
        return lease

    def release_lease(
        self, task_id: str, context_id: str, resource_kind: str,
        resource_id: str, *, owner: str, fencing_token: int,
    ) -> ResourceLease:
        key = self.lease_key(task_id, context_id, resource_id, resource_kind=resource_kind)
        with self.store.platform.lock:
            self.assert_lease(
                task_id, context_id, resource_kind, resource_id,
                owner=owner, fencing_token=fencing_token,
            )
            return self.store.platform.release_lease(resource_kind, key)

    def register_object(
        self, task_id: str, context_id: str, *, object_type: str,
        object_id: str, owner: str, tenant: str = "",
        metadata: Optional[dict[str, Any]] = None,
        expected_revision: Optional[int] = None,
    ) -> dict[str, Any]:
        if not object_type or not object_id or not owner:
            raise SrcStoreError("task_binding_invalid", "object type, id and owner are required")
        natural = f"{context_id}:{object_type}:{object_id}"
        existing = self.store.by_natural_key(task_id, "test_object", natural)
        if existing and expected_revision is not None and expected_revision != existing.revision:
            raise SrcStoreError("version_conflict", "test object revision is stale")
        payload = {
            "context_id": context_id, "object_type": object_type,
            "object_id": object_id, "owner": owner, "tenant": tenant,
            "metadata": dict(metadata or {}),
        }
        record = self.store.put(
            task_id, "test_object", payload,
            record_id=existing.record_id if existing else new_id("sobject"),
            expected_revision=existing.revision if existing else None,
            natural_key=natural,
        )
        return record.model_dump(mode="json")

    def begin_experiment(
        self, task_id: str, context_id: str, *, object_type: str,
        object_id: str, owner: str, tenant: str = "",
        initial_state: dict[str, Any], expected_change: dict[str, Any],
        restoration: dict[str, Any], actor: dict[str, Any],
        expected_revision: Optional[int] = None,
        mutation: bool = True, ttl_seconds: int = 600,
        metadata: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        participant = self.participant(task_id, context_id, actor)
        resource_id = f"{object_type}:{object_id}"
        lease = self.acquire_lease(
            task_id, context_id, "src.business_object", resource_id,
            owner=participant, ttl_seconds=ttl_seconds,
        ) if mutation else None
        obj = self.register_object(
            task_id, context_id, object_type=object_type,
            object_id=object_id, owner=owner, tenant=tenant, metadata=metadata,
            expected_revision=expected_revision,
        )
        before = self.artifacts.put(
            task_id, self.store._json(initial_state).encode("utf-8"),
            media_type="application/json", name=f"{object_id}-before.json",
            source_run_id=str(actor.get("run_id") or "") or None,
        )
        experiment = {
            "context_id": context_id, "object_type": object_type,
            "object_id": object_id, "owner": owner, "tenant": tenant,
            "participant": participant, "actor": dict(actor),
            "before_artifact": before.model_dump(mode="json"),
            "initial_state": initial_state, "expected_change": expected_change,
            "restoration_plan": restoration,
            "lease": lease.model_dump(mode="json") if lease else None,
            "after_artifact": None, "after_state": None,
            "restoration": {"status": "not_applicable" if not mutation else "unknown"},
            "metadata": dict(metadata or {}),
        }
        record = self.store.put(
            task_id, "object_experiment", experiment,
            record_id=new_id("sexperiment"), parent_id=obj["record_id"],
        )
        self._append_state(task_id, record.record_id, "before", initial_state, [before])
        return {**record.model_dump(mode="json"), "fencing_token": lease.fencing_token if lease else 0}

    def _experiment(self, task_id: str, experiment_id: str) -> Any:
        return self.store.require(task_id, "object_experiment", experiment_id)

    def _assert_experiment_lease(
        self, task_id: str, experiment: Any, *, fencing_token: int,
    ) -> None:
        lease = experiment.payload.get("lease")
        if not lease:
            return
        self.assert_lease(
            task_id, experiment.payload["context_id"], "src.business_object",
            f"{experiment.payload['object_type']}:{experiment.payload['object_id']}",
            owner=lease["owner"], fencing_token=fencing_token,
        )

    def record_after(
        self, task_id: str, experiment_id: str, *,
        observed_state: dict[str, Any], fencing_token: int = 0,
        evidence_refs: Optional[list[dict[str, Any]]] = None,
    ) -> dict[str, Any]:
        experiment = self._experiment(task_id, experiment_id)
        self._assert_experiment_lease(task_id, experiment, fencing_token=fencing_token)
        artifact = self.artifacts.put(
            task_id, self.store._json(observed_state).encode("utf-8"),
            media_type="application/json", name=f"{experiment_id}-after.json",
            source_run_id=experiment.payload["actor"].get("run_id") or None,
        )
        payload = {
            **experiment.payload,
            "after_state": observed_state,
            "after_artifact": artifact.model_dump(mode="json"),
            "after_evidence_refs": list(evidence_refs or []),
        }
        record = self.store.put(
            task_id, "object_experiment", payload,
            record_id=experiment_id, expected_revision=experiment.revision,
            parent_id=experiment.parent_id,
        )
        self._append_state(task_id, experiment_id, "after", observed_state, [artifact])
        return record.model_dump(mode="json")

    def record_restoration(
        self, task_id: str, experiment_id: str, *, status: RestorationState,
        readback: Optional[dict[str, Any]], fencing_token: int = 0,
        residual_objects: Optional[list[dict[str, Any]]] = None,
        unrestored_fields: Optional[list[str]] = None,
        reason: str = "", evidence_refs: Optional[list[dict[str, Any]]] = None,
        release: bool = True,
    ) -> dict[str, Any]:
        if status not in RESTORATION_STATES:
            raise SrcStoreError("policy_denied", "unknown restoration state")
        if status not in {"not_applicable", "unknown"} and readback is None:
            raise SrcStoreError("artifact_missing", "restoration requires an actual state readback")
        if status in {"partially_restored", "not_restored", "unknown"} and not (
            reason or residual_objects or unrestored_fields
        ):
            raise SrcStoreError("artifact_missing", "incomplete restoration requires a reason or residual state")
        if status == "restored" and (residual_objects or unrestored_fields):
            raise SrcStoreError("version_conflict", "restored cannot retain residual objects or fields")
        experiment = self._experiment(task_id, experiment_id)
        self._assert_experiment_lease(task_id, experiment, fencing_token=fencing_token)
        artifact = self.artifacts.put(
            task_id, self.store._json(readback).encode("utf-8"),
            media_type="application/json", name=f"{experiment_id}-restore-readback.json",
            source_run_id=experiment.payload["actor"].get("run_id") or None,
        ) if readback is not None else None
        restoration = {
            "status": status, "readback": readback,
            "readback_artifact": artifact.model_dump(mode="json") if artifact else None,
            "residual_objects": list(residual_objects or []),
            "unrestored_fields": list(unrestored_fields or []),
            "reason": reason, "evidence_refs": list(evidence_refs or []),
            "recorded_at": utcnow().isoformat(),
        }
        record = self.store.put(
            task_id, "object_experiment", {**experiment.payload, "restoration": restoration},
            record_id=experiment_id, expected_revision=experiment.revision,
            parent_id=experiment.parent_id,
        )
        self._append_state(task_id, experiment_id, "restoration", {
            "status": status, "readback": readback,
            "residual_objects": restoration["residual_objects"],
            "unrestored_fields": restoration["unrestored_fields"],
        }, [artifact] if artifact else [])
        lease = experiment.payload.get("lease")
        if release and lease:
            self.release_lease(
                task_id, experiment.payload["context_id"], "src.business_object",
                f"{experiment.payload['object_type']}:{experiment.payload['object_id']}",
                owner=lease["owner"], fencing_token=fencing_token,
            )
        return record.model_dump(mode="json")

    async def restore_experiment(
        self, task_id: str, experiment_id: str, *,
        fencing_token: int,
        authorize: Callable[[dict[str, Any]], Awaitable[None]],
        restore_action: Callable[[dict[str, Any]], Awaitable[dict[str, Any]]],
    ) -> dict[str, Any]:
        experiment = self._experiment(task_id, experiment_id)
        self._assert_experiment_lease(task_id, experiment, fencing_token=fencing_token)
        snapshot = experiment.model_dump(mode="json")
        await authorize(snapshot)
        try:
            outcome = await restore_action(snapshot)
        except Exception as exc:
            self.record_restoration(
                task_id, experiment_id, status="unknown", readback=None,
                reason=f"{type(exc).__name__}: {exc}", fencing_token=fencing_token,
                release=False,
            )
            raise
        if not isinstance(outcome, dict) or "status" not in outcome:
            self.record_restoration(
                task_id, experiment_id, status="unknown",
                readback=outcome.get("readback") if isinstance(outcome, dict) else None,
                reason="restore action returned no verifiable status",
                fencing_token=fencing_token, release=False,
            )
            raise SrcStoreError("artifact_missing", "restore action returned no verifiable status")
        return self.record_restoration(
            task_id, experiment_id,
            status=outcome["status"], readback=outcome.get("readback"),
            fencing_token=fencing_token,
            residual_objects=outcome.get("residual_objects"),
            unrestored_fields=outcome.get("unrestored_fields"),
            reason=str(outcome.get("reason") or ""),
            evidence_refs=outcome.get("evidence_refs"),
        )

    def _append_state(
        self, task_id: str, experiment_id: str, phase: str,
        state: Any, artifacts: list[Any],
    ) -> None:
        self.store.put(
            task_id, "object_state_event", {
                "experiment_id": experiment_id, "phase": phase, "state": state,
                "artifact_refs": [item.model_dump(mode="json") for item in artifacts],
                "observed_at": utcnow().isoformat(),
            }, record_id=new_id("sobjectstate"), parent_id=experiment_id,
        )

    def history(self, task_id: str, experiment_id: str) -> dict[str, Any]:
        experiment = self._experiment(task_id, experiment_id)
        events = self.store.all(task_id, "object_state_event")
        return {
            "experiment": experiment.model_dump(mode="json"),
            "events": [row.model_dump(mode="json") for row in events if row.parent_id == experiment_id],
        }


__all__ = ["SrcObjectService", "RESTORATION_STATES", "RestorationState"]
