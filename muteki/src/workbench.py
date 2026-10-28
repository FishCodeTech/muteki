from __future__ import annotations

import json
from typing import Any

from .store import SrcStore, SrcStoreError

WORKBENCH_RECORD_KIND = "workbench_slice"
WORKBENCH_SCHEMA = 1
WORKBENCH_MAX_BYTES = 16 * 1024 * 1024
WORKBENCH_SLICES = frozenset({
    "projects", "sources", "clues", "tasks", "dossier", "delivery",
    "governance", "threads", "chat", "imports", "records",
})


def _json_bytes(value: Any) -> int:
    return len(json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), default=str).encode("utf-8"))


class SrcWorkbenchService:
    """Operator-side persistence for the workbench UI state, one record per slice."""

    def __init__(self, store: SrcStore) -> None:
        self.store = store

    @staticmethod
    def _record_id(task_id: str, slice: str) -> str:
        return f"{task_id}:{slice}"

    @staticmethod
    def _validate_slice(slice: Any) -> str:
        name = str(slice or "").strip()
        if name not in WORKBENCH_SLICES:
            raise SrcStoreError("workbench_slice_invalid", f"unsupported workbench slice {name!r}")
        return name

    def save_slice(
        self, task_id: str, slice: str, data: Any, base_revision: int = 0,
        client_id: str = "", actor_id: str = "",
    ) -> dict[str, Any]:
        name = self._validate_slice(slice)
        if not isinstance(data, dict):
            raise SrcStoreError("workbench_slice_invalid", "workbench slice data must be a JSON object")
        base = int(base_revision or 0)
        if base < 0:
            raise SrcStoreError("workbench_slice_invalid", "base_revision cannot be negative")
        payload = {
            "slice": name, "schema": WORKBENCH_SCHEMA, "data": data,
            "saved_by": str(actor_id or ""), "client_id": str(client_id or ""),
        }
        if _json_bytes(payload) > WORKBENCH_MAX_BYTES:
            raise SrcStoreError(
                "workbench_slice_too_large", "workbench slice payload exceeds 16 MiB",
                detail={"limit": WORKBENCH_MAX_BYTES, "actual": _json_bytes(payload)},
            )
        record_id = self._record_id(task_id, name)
        if base == 0:
            existing = self.store.get(task_id, WORKBENCH_RECORD_KIND, record_id)
            if existing is not None:
                raise SrcStoreError(
                    "version_conflict", "workbench slice already exists",
                    detail={"expected": 0, "actual": existing.revision},
                )
            record = self.store.put(
                task_id, WORKBENCH_RECORD_KIND, payload,
                record_id=record_id, natural_key=name,
            )
        else:
            record = self.store.put(
                task_id, WORKBENCH_RECORD_KIND, payload,
                record_id=record_id, expected_revision=base, natural_key=name,
            )
        return {
            "slice": name, "revision": record.revision,
            "updated_at": record.updated_at.isoformat(),
        }

    def snapshot(self, task_id: str) -> dict[str, Any]:
        slices: dict[str, Any] = {}
        for record in self.store.list(task_id, WORKBENCH_RECORD_KIND, limit=500):
            slices[record.natural_key] = {
                "revision": record.revision,
                "updated_at": record.updated_at.isoformat(),
                "client_id": str(record.payload.get("client_id") or ""),
                "data": record.payload.get("data"),
            }
        return {"slices": slices}

    def slice_get(self, task_id: str, slice: str) -> dict[str, Any]:
        name = self._validate_slice(slice)
        record = self.store.get(task_id, WORKBENCH_RECORD_KIND, self._record_id(task_id, name))
        if record is None:
            raise SrcStoreError("artifact_missing", f"workbench slice {name!r} is unavailable")
        return {
            "slice": name, "revision": record.revision,
            "updated_at": record.updated_at.isoformat(),
            "client_id": str(record.payload.get("client_id") or ""),
            "data": record.payload.get("data"),
        }


__all__ = ["SrcWorkbenchService", "WORKBENCH_RECORD_KIND", "WORKBENCH_SLICES"]
