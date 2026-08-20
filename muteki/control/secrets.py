"""Run-local storage for operator supplied secrets.

The control journal and event stream must only persist opaque ``secret://``
references.  This module is the deliberately narrow boundary where the
referenced value can be materialised for a worker process.

Secret values are stored as individual files in a caller-provided, run-local
directory.  The directory is forced to mode ``0700`` and files to ``0600``.
Creation is atomic and no-overwrite: a fully written, fsynced temporary inode
is linked into place only after its contents are complete.
"""

from __future__ import annotations

import os
import re
import stat
import tempfile
import uuid
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from pathlib import Path


_REFERENCE_PREFIX = "secret://"
_SECRET_ID_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{2,127}\Z")
_SECRET_SUFFIX = ".secret"
_MAX_ID_ATTEMPTS = 32


class SecretStoreError(RuntimeError):
    """Base error for the run-local secret store.

    Error messages contain references at most, never secret values.
    """


class InvalidSecretReference(SecretStoreError, ValueError):
    """Raised when an input is not a canonical opaque secret reference."""


class SecretNotFound(SecretStoreError, KeyError):
    """Raised when a well-formed secret reference is absent from this run."""


@dataclass(frozen=True, slots=True)
class SecretMetadata:
    """Safe metadata suitable for API responses, journals, and logs."""

    reference: str
    secret_id: str


class SecretStore:
    """An isolated file-backed secret store for exactly one run.

    ``root`` must be a coordinator-private directory dedicated to one run. It
    must never sit below a worker workspace or another path mounted into a worker
    runtime. Callers may persist the references returned by :meth:`put`; they
    must not persist values returned by :meth:`resolve`.

    ``id_factory`` exists for deterministic tests.  Production callers should
    leave it unset so ids are generated from UUID4 randomness.
    """

    def __init__(
        self,
        root: str | os.PathLike[str],
        *,
        id_factory: Callable[[], str] | None = None,
    ) -> None:
        self._root = Path(root)
        self._id_factory = id_factory or (lambda: uuid.uuid4().hex)
        self._prepare_root()

    @property
    def root(self) -> Path:
        """Return the storage directory, never a secret value."""

        return self._root

    def put(self, value: str) -> str:
        """Atomically store ``value`` and return an opaque reference.

        A new id is always allocated.  Existing secrets are never overwritten,
        including in the astronomically unlikely event of an id collision.
        """

        if not isinstance(value, str):
            raise TypeError("secret value must be text")

        encoded = value.encode("utf-8")
        for _ in range(_MAX_ID_ATTEMPTS):
            secret_id = self._validate_secret_id(self._id_factory())
            target = self._path_for_id(secret_id)
            try:
                self._atomic_create(target, encoded)
            except FileExistsError:
                continue
            return self._reference(secret_id)
        raise SecretStoreError("unable to allocate a unique secret reference")

    def get(self, reference: str) -> SecretMetadata:
        """Return safe metadata for an existing reference."""

        secret_id, path = self._parse(reference)
        self._assert_regular_secret(path, reference)
        self._enforce_file_mode(path)
        return SecretMetadata(reference=self._reference(secret_id), secret_id=secret_id)

    def list(self) -> tuple[SecretMetadata, ...]:
        """Return sorted safe metadata without reading any secret values."""

        records: list[SecretMetadata] = []
        for entry in self._iter_secret_entries():
            secret_id = entry.name[: -len(_SECRET_SUFFIX)]
            try:
                secret_id = self._validate_secret_id(secret_id)
                self._assert_regular_secret(entry, self._reference(secret_id))
                self._enforce_file_mode(entry)
            except SecretStoreError:
                # Never follow or expose unexpected directory entries.
                continue
            records.append(
                SecretMetadata(
                    reference=self._reference(secret_id),
                    secret_id=secret_id,
                )
            )
        return tuple(sorted(records, key=lambda item: item.secret_id))

    def delete(self, reference: str) -> SecretMetadata:
        """Delete a secret and return only the deleted reference metadata."""

        secret_id, path = self._parse(reference)
        self._assert_regular_secret(path, reference)
        try:
            path.unlink()
        except FileNotFoundError as exc:
            raise SecretNotFound(f"secret not found: {reference}") from exc
        self._fsync_directory()
        return SecretMetadata(reference=self._reference(secret_id), secret_id=secret_id)

    def resolve(self, reference: str) -> str:
        """Materialise a secret at the worker-injection boundary.

        This is the sole value-returning operation.  Its return value must be
        used transiently (for example to populate a child-process environment)
        and must never be written into the control journal or event stream.
        """

        _, path = self._parse(reference)
        fd = self._open_regular_secret(path, reference)
        try:
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "rb", closefd=True) as handle:
                fd = -1
                raw = handle.read()
        finally:
            if fd >= 0:
                os.close(fd)
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise SecretStoreError(f"secret payload is not valid UTF-8: {reference}") from exc

    def _prepare_root(self) -> None:
        try:
            current = self._root.lstat()
        except FileNotFoundError:
            self._root.mkdir(mode=0o700, parents=True, exist_ok=False)
            current = self._root.lstat()
        if not stat.S_ISDIR(current.st_mode) or stat.S_ISLNK(current.st_mode):
            raise SecretStoreError("secret store root must be a real directory")
        os.chmod(self._root, 0o700)
        self._cleanup_stale_temps()

    def _cleanup_stale_temps(self) -> None:
        # A process crash before publication may leave a 0600 temporary inode.
        # Remove only our regular temporary files and never follow symlinks.
        try:
            entries = tuple(self._root.iterdir())
        except FileNotFoundError as exc:
            raise SecretStoreError("secret store root disappeared") from exc
        for entry in entries:
            if not entry.name.startswith(".secret-tmp-"):
                continue
            try:
                mode = entry.lstat().st_mode
                if stat.S_ISREG(mode):
                    entry.unlink()
            except FileNotFoundError:
                continue

    def _atomic_create(self, target: Path, payload: bytes) -> None:
        fd, temp_name = tempfile.mkstemp(prefix=".secret-tmp-", dir=self._root)
        temp = Path(temp_name)
        try:
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "wb", closefd=True) as handle:
                fd = -1
                handle.write(payload)
                handle.flush()
                os.fsync(handle.fileno())

            # Hard-link publication is atomic and refuses to overwrite.  The
            # temp file and target reside in the same run-local directory.
            os.link(temp, target, follow_symlinks=False)
            try:
                temp.unlink()
            except OSError:
                # Publication already succeeded.  A 0600 stale temp is safer
                # than reporting failure for a secret that now exists; it is
                # retried below and cleaned on the next store initialisation.
                pass
            self._fsync_directory()
        except FileExistsError:
            raise
        except OSError as exc:
            raise SecretStoreError("failed to persist secret") from exc
        finally:
            if fd >= 0:
                os.close(fd)
            try:
                temp.unlink()
            except OSError:
                pass

    def _parse(self, reference: str) -> tuple[str, Path]:
        if not isinstance(reference, str) or not reference.startswith(_REFERENCE_PREFIX):
            raise InvalidSecretReference("invalid secret reference")
        secret_id = reference[len(_REFERENCE_PREFIX) :]
        secret_id = self._validate_secret_id(secret_id)
        canonical = self._reference(secret_id)
        if reference != canonical:
            raise InvalidSecretReference("invalid secret reference")
        return secret_id, self._path_for_id(secret_id)

    @staticmethod
    def _validate_secret_id(secret_id: object) -> str:
        if not isinstance(secret_id, str) or not _SECRET_ID_RE.fullmatch(secret_id):
            raise InvalidSecretReference("invalid secret identifier")
        return secret_id

    def _path_for_id(self, secret_id: str) -> Path:
        # The allowlist above excludes separators and dots; this explicit parent
        # check keeps that security property visible at the filesystem boundary.
        path = self._root / f"{secret_id}{_SECRET_SUFFIX}"
        if path.parent != self._root:
            raise InvalidSecretReference("secret reference escapes its run store")
        return path

    @staticmethod
    def _reference(secret_id: str) -> str:
        return f"{_REFERENCE_PREFIX}{secret_id}"

    def _iter_secret_entries(self) -> Iterator[Path]:
        try:
            yield from (
                entry
                for entry in self._root.iterdir()
                if entry.name.endswith(_SECRET_SUFFIX)
            )
        except FileNotFoundError as exc:
            raise SecretStoreError("secret store root disappeared") from exc

    @staticmethod
    def _assert_regular_secret(path: Path, reference: str) -> os.stat_result:
        try:
            info = path.lstat()
        except FileNotFoundError as exc:
            raise SecretNotFound(f"secret not found: {reference}") from exc
        if not stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode):
            raise SecretStoreError(f"unsafe secret entry: {reference}")
        return info

    @staticmethod
    def _enforce_file_mode(path: Path) -> None:
        try:
            os.chmod(path, 0o600, follow_symlinks=False)
        except (OSError, NotImplementedError) as exc:
            raise SecretStoreError("failed to secure secret file") from exc

    def _open_regular_secret(self, path: Path, reference: str) -> int:
        before = self._assert_regular_secret(path, reference)
        flags = os.O_RDONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            fd = os.open(path, flags)
        except FileNotFoundError as exc:
            raise SecretNotFound(f"secret not found: {reference}") from exc
        except OSError as exc:
            raise SecretStoreError(f"unable to open secret: {reference}") from exc
        after = os.fstat(fd)
        if (
            not stat.S_ISREG(after.st_mode)
            or before.st_dev != after.st_dev
            or before.st_ino != after.st_ino
        ):
            os.close(fd)
            raise SecretStoreError(f"unsafe secret entry: {reference}")
        return fd

    def _fsync_directory(self) -> None:
        flags = os.O_RDONLY
        if hasattr(os, "O_DIRECTORY"):
            flags |= os.O_DIRECTORY
        try:
            fd = os.open(self._root, flags)
        except OSError:
            return
        try:
            os.fsync(fd)
        except OSError:
            # Some filesystems do not support directory fsync.  File contents
            # were already fsynced before atomic publication.
            pass
        finally:
            os.close(fd)


__all__ = [
    "InvalidSecretReference",
    "SecretMetadata",
    "SecretNotFound",
    "SecretStore",
    "SecretStoreError",
]
