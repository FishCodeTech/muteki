from __future__ import annotations

import hashlib
import json
import sqlite3
from datetime import datetime
from typing import Any, Iterable, Optional

from muteki.platform.contracts.base import new_id, utcnow
from muteki.platform.store import PlatformStore

from .contracts import SrcRecord


class SrcStoreError(RuntimeError):
    def __init__(self, code: str, message: str, *, detail: Optional[dict[str, Any]] = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.detail = detail or {}


class SrcStore:
    def __init__(self, platform: PlatformStore) -> None:
        self.platform = platform

    @staticmethod
    def _json(value: Any) -> str:
        return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), default=str)

    @staticmethod
    def digest(value: Any) -> str:
        return hashlib.sha256(SrcStore._json(value).encode("utf-8")).hexdigest()

    @staticmethod
    def _decode(row: sqlite3.Row) -> SrcRecord:
        payload = json.loads(row["payload"])
        return SrcRecord(
            record_id=row["record_id"], kind=row["kind"], task_id=row["task_id"],
            revision=int(row["revision"]), parent_id=row["parent_id"],
            natural_key=row["natural_key"], generation=int(row["generation"]),
            created_at=datetime.fromisoformat(row["created_at"]),
            updated_at=datetime.fromisoformat(row["updated_at"]), payload=payload,
        )

    def put(
        self, task_id: str, kind: str, payload: dict[str, Any], *,
        record_id: str = "", expected_revision: Optional[int] = None,
        parent_id: str = "", natural_key: str = "", generation: int = 0,
        created_at: Optional[datetime] = None,
    ) -> SrcRecord:
        if not task_id or not kind:
            raise ValueError("task_id and kind are required")
        record_id = record_id or new_id(kind.replace("_", "-"))
        now = utcnow()
        with self.platform.transaction():
            row = self.platform.conn.execute(
                "SELECT task_id, revision, created_at FROM src_records WHERE kind=? AND record_id=?",
                (kind, record_id),
            ).fetchone()
            if row is not None and row["task_id"] != task_id:
                raise SrcStoreError("task_binding_invalid", "record belongs to another Task")
            if expected_revision is not None and (row is None or int(row["revision"]) != expected_revision):
                raise SrcStoreError(
                    "version_conflict", "record revision does not match",
                    detail={"expected": expected_revision, "actual": int(row["revision"]) if row else 0},
                )
            revision = int(row["revision"]) + 1 if row is not None else 1
            created = row["created_at"] if row is not None else (created_at or now).isoformat()
            values = (
                record_id, task_id, kind, revision, parent_id, natural_key,
                int(generation), created, now.isoformat(), self._json(payload),
            )
            try:
                self.platform.conn.execute(
                    "INSERT INTO src_records (record_id,task_id,kind,revision,parent_id,natural_key,generation,created_at,updated_at,payload) "
                    "VALUES (?,?,?,?,?,?,?,?,?,?) "
                    "ON CONFLICT(kind,record_id) DO UPDATE SET revision=excluded.revision,"
                    "parent_id=excluded.parent_id,natural_key=excluded.natural_key,"
                    "generation=excluded.generation,updated_at=excluded.updated_at,payload=excluded.payload",
                    values,
                )
            except sqlite3.IntegrityError as exc:
                raise SrcStoreError("version_conflict", "record natural key already exists") from exc
        return SrcRecord(
            record_id=record_id, kind=kind, task_id=task_id, revision=revision,
            parent_id=parent_id, natural_key=natural_key, generation=generation,
            created_at=datetime.fromisoformat(created), updated_at=now, payload=payload,
        )

    def get(self, task_id: str, kind: str, record_id: str, *, revision: Optional[int] = None) -> Optional[SrcRecord]:
        with self.platform.lock:
            row = self.platform.conn.execute(
                "SELECT * FROM src_records WHERE task_id=? AND kind=? AND record_id=?",
                (task_id, kind, record_id),
            ).fetchone()
        if row is None:
            return None
        record = self._decode(row)
        if revision is not None and record.revision != revision:
            raise SrcStoreError("version_conflict", "record revision does not match requested version")
        return record

    def require(self, task_id: str, kind: str, record_id: str, *, revision: Optional[int] = None) -> SrcRecord:
        record = self.get(task_id, kind, record_id, revision=revision)
        if record is None:
            raise SrcStoreError("artifact_missing", f"{kind}/{record_id} is unavailable")
        return record

    def by_natural_key(self, task_id: str, kind: str, natural_key: str) -> Optional[SrcRecord]:
        with self.platform.lock:
            row = self.platform.conn.execute(
                "SELECT * FROM src_records WHERE task_id=? AND kind=? AND natural_key=?",
                (task_id, kind, natural_key),
            ).fetchone()
        return self._decode(row) if row is not None else None

    def list(
        self, task_id: str, kind: str = "", *, parent_id: Optional[str] = None,
        after_rowid: int = 0, limit: int = 100, reverse: bool = False,
        excluded_kinds: Optional[set[str]] = None,
        max_rowid: Optional[int] = None,
    ) -> list[SrcRecord]:
        clauses = ["task_id=?"]
        params: list[Any] = [task_id]
        if kind:
            clauses.append("kind=?")
            params.append(kind)
        if parent_id is not None:
            clauses.append("parent_id=?")
            params.append(parent_id)
        if excluded_kinds:
            ordered_excluded = sorted(excluded_kinds)
            clauses.append("kind NOT IN (" + ",".join("?" for _ in ordered_excluded) + ")")
            params.extend(ordered_excluded)
        if max_rowid is not None and max_rowid > 0:
            clauses.append("rowid<=?")
            params.append(int(max_rowid))
        if after_rowid:
            clauses.append("rowid>?" if not reverse else "rowid<?")
            params.append(after_rowid)
        order = "DESC" if reverse else "ASC"
        params.append(max(1, min(int(limit), 1001)))
        sql = "SELECT * FROM src_records WHERE " + " AND ".join(clauses) + f" ORDER BY rowid {order} LIMIT ?"
        with self.platform.lock:
            rows = list(self.platform.conn.execute(sql, params).fetchall())
        return [self._decode(row) for row in rows]

    def rowid(self, record: SrcRecord) -> int:
        with self.platform.lock:
            row = self.platform.conn.execute(
                "SELECT rowid FROM src_records WHERE kind=? AND record_id=?",
                (record.kind, record.record_id),
            ).fetchone()
        if row is None:
            raise SrcStoreError("artifact_missing", "record no longer exists")
        return int(row[0])

    def all(self, task_id: str, kind: str = "") -> list[SrcRecord]:
        rows: list[SrcRecord] = []
        cursor = 0
        while True:
            page = self.list(task_id, kind, after_rowid=cursor, limit=500)
            if not page:
                return rows
            rows.extend(page)
            cursor = self.rowid(page[-1])

    def idempotent(
        self, task_id: str, operation: str, key: str, request: Any,
    ) -> Optional[dict[str, Any]]:
        with self.platform.lock:
            row = self.platform.conn.execute(
                "SELECT request_hash,response FROM src_idempotency WHERE task_id=? AND operation=? AND idempotency_key=?",
                (task_id, operation, key),
            ).fetchone()
        if row is None:
            return None
        if row["request_hash"] != self.digest(request):
            raise SrcStoreError("version_conflict", "idempotency key was used for different input")
        return json.loads(row["response"])

    def remember(
        self, task_id: str, operation: str, key: str, request: Any, response: Any,
    ) -> dict[str, Any]:
        if not key:
            raise ValueError("idempotency key is required")
        with self.platform.transaction():
            self.platform.conn.execute(
                "INSERT INTO src_idempotency (task_id,operation,idempotency_key,request_hash,response,created_at) VALUES (?,?,?,?,?,?)",
                (task_id, operation, key, self.digest(request), self._json(response), utcnow().isoformat()),
            )
        return response

    def source_watermark(self, task_id: str, source_id: str) -> dict[str, Any]:
        with self.platform.lock:
            row = self.platform.conn.execute(
                "SELECT * FROM src_sources WHERE task_id=? AND source_id=?", (task_id, source_id),
            ).fetchone()
        if row is None:
            return {"source_id": source_id, "next_seq": 0, "generation": 0, "state": "pending", "error": ""}
        return {key: row[key] for key in row.keys() if key != "task_id"}

    def advance_source(
        self, task_id: str, source_id: str, *, expected_generation: int,
        next_seq: int, extractor_version: str, state: str = "ready", error: str = "",
    ) -> None:
        with self.platform.transaction():
            row = self.platform.conn.execute(
                "SELECT generation,next_seq FROM src_sources WHERE task_id=? AND source_id=?",
                (task_id, source_id),
            ).fetchone()
            if row is not None and int(row["generation"]) > expected_generation:
                raise SrcStoreError("authorization_stale", "source generation is stale")
            if row is not None and next_seq < int(row["next_seq"]):
                raise SrcStoreError("version_conflict", "source watermark cannot move backwards")
            self.platform.conn.execute(
                "INSERT INTO src_sources (task_id,source_id,generation,next_seq,extractor_version,state,error,updated_at) "
                "VALUES (?,?,?,?,?,?,?,?) ON CONFLICT(task_id,source_id) DO UPDATE SET "
                "generation=excluded.generation,next_seq=excluded.next_seq,extractor_version=excluded.extractor_version,"
                "state=excluded.state,error=excluded.error,updated_at=excluded.updated_at",
                (task_id, source_id, expected_generation, next_seq, extractor_version, state, error, utcnow().isoformat()),
            )

    def watermarks(self, task_id: str) -> list[dict[str, Any]]:
        with self.platform.lock:
            rows = list(self.platform.conn.execute(
                "SELECT * FROM src_sources WHERE task_id=? ORDER BY source_id", (task_id,),
            ).fetchall())
        return [{key: row[key] for key in row.keys() if key != "task_id"} for row in rows]


__all__ = ["SrcStore", "SrcStoreError"]
