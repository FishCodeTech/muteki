from __future__ import annotations

import hashlib
import os
import uuid
from pathlib import Path

from .contracts import ArtifactRef
from .store import SrcStoreError


class SrcArtifactStore:
    def __init__(self, root: Path) -> None:
        self.root = Path(root).resolve()
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)

    def task_root(self, task_id: str) -> Path:
        if not task_id or any(char not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-" for char in task_id):
            raise SrcStoreError("task_binding_invalid", "invalid Task id for artifact storage")
        path = self.root / task_id / "cas"
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
        if path.is_symlink() or not path.resolve().is_relative_to(self.root):
            raise SrcStoreError("artifact_missing", "artifact root escapes the SRC store")
        return path

    def put(
        self, task_id: str, data: bytes, *, media_type: str = "application/octet-stream",
        name: str = "", kind: str = "src.evidence", source_run_id: str | None = None,
        source_event_seq: int | None = None, complete: str = "complete",
        original: bool = True,
    ) -> ArtifactRef:
        if not isinstance(data, bytes):
            raise TypeError("artifact content must be bytes")
        digest = hashlib.sha256(data).hexdigest()
        root = self.task_root(task_id)
        path = root / digest
        if not path.exists():
            temporary = root / f".{digest}.{uuid.uuid4().hex}.tmp"
            try:
                fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                with os.fdopen(fd, "wb") as handle:
                    handle.write(data)
                    handle.flush()
                    os.fsync(handle.fileno())
                os.replace(temporary, path)
            finally:
                temporary.unlink(missing_ok=True)
        if path.is_symlink() or not path.is_file():
            raise SrcStoreError("artifact_missing", "artifact is not a regular file")
        return ArtifactRef(
            artifact_id=digest[:16], sha256=digest, media_type=media_type,
            size=len(data), task_id=task_id, source_run_id=source_run_id,
            source_event_seq=source_event_seq, name=name or kind, complete=complete,
            original=original,
        )

    def path(self, ref: ArtifactRef) -> Path:
        if len(ref.sha256) != 64 or any(char not in "0123456789abcdef" for char in ref.sha256):
            raise SrcStoreError("artifact_missing", "invalid artifact digest")
        path = self.task_root(ref.task_id) / ref.sha256
        if path.is_symlink() or not path.is_file():
            raise SrcStoreError("artifact_missing", "artifact bytes are unavailable", detail={"sha256": ref.sha256})
        return path

    def read(self, ref: ArtifactRef, *, expected_task_id: str) -> bytes:
        if ref.task_id != expected_task_id:
            raise SrcStoreError("task_binding_invalid", "artifact belongs to another Task")
        path = self.path(ref)
        data = path.read_bytes()
        if len(data) != ref.size or hashlib.sha256(data).hexdigest() != ref.sha256:
            raise SrcStoreError("artifact_hash_mismatch", "artifact bytes do not match the frozen reference")
        return data

    def pin(self, ref: ArtifactRef, *, target_task_id: str) -> ArtifactRef:
        data = self.read(ref, expected_task_id=ref.task_id)
        return self.put(
            target_task_id, data, media_type=ref.media_type, name=ref.name,
            source_run_id=ref.source_run_id, source_event_seq=ref.source_event_seq,
            complete=ref.complete,
        )

    def destroy(self, task_id: str, sha256: str) -> None:
        from .contracts import ArtifactRef
        ref = ArtifactRef(artifact_id=sha256[:16], sha256=sha256, task_id=task_id)
        path = self.path(ref)
        path.unlink()

    def destroy_root(self, task_id: str) -> None:
        import shutil
        root = self.task_root(task_id)
        if root.is_symlink() or not root.resolve().is_relative_to(self.root):
            raise SrcStoreError("artifact_missing", "artifact root escapes the SRC store")
        if root.exists():
            shutil.rmtree(root)


__all__ = ["SrcArtifactStore"]
