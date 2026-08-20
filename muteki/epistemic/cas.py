"""Verifier-owned SHA-256 receipt CAS with no-follow durable writes.

Every directory and object used by the CAS is opened relative to an already
verified directory descriptor.  In particular, ``root``, ``sha256``,
``staging`` and the two-character digest prefix may never be symbolic links.
This prevents a local sibling process from redirecting a verifier write or
``chmod`` outside the verifier-owned tree.
"""

from __future__ import annotations

import errno
import hashlib
import os
import secrets
import stat
from dataclasses import dataclass
from pathlib import Path


class CASIntegrityError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class SealedObject:
    digest: str
    byte_count: int


_DIRECTORY_FLAGS = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
_NOFOLLOW = getattr(os, "O_NOFOLLOW", 0)


def _open_directory_at(
    parent_fd: int,
    name: str,
    *,
    create: bool,
    label: str,
) -> int:
    """Open one plain child directory without following a symlink."""

    if not name or name in {".", ".."} or "/" in name:
        raise CASIntegrityError(f"{label} has an invalid path component")
    if create:
        try:
            os.mkdir(name, mode=0o700, dir_fd=parent_fd)
        except FileExistsError:
            pass
        except OSError as exc:
            raise CASIntegrityError(f"{label} cannot be created safely") from exc
    try:
        descriptor = os.open(
            name,
            _DIRECTORY_FLAGS | _NOFOLLOW,
            dir_fd=parent_fd,
        )
    except OSError as exc:
        raise CASIntegrityError(f"{label} is not a trusted directory") from exc
    if not stat.S_ISDIR(os.fstat(descriptor).st_mode):
        os.close(descriptor)
        raise CASIntegrityError(f"{label} is not a directory")
    return descriptor


def _open_directory_path_nofollow(path: Path, *, create: bool, label: str) -> int:
    """Traverse an absolute path component-by-component without symlinks."""

    absolute = path.absolute()
    parts = absolute.parts
    if not absolute.is_absolute() or not parts or parts[0] != absolute.anchor:
        raise CASIntegrityError(f"{label} must be absolute")
    descriptor = os.open(absolute.anchor, _DIRECTORY_FLAGS | _NOFOLLOW)
    try:
        for index, component in enumerate(parts[1:]):
            child = _open_directory_at(
                descriptor,
                component,
                create=create,
                label=f"{label} component",
            )
            os.close(descriptor)
            descriptor = child
            # Missing parents are allowed only while constructing a new CAS.
            # Existing components are still opened with O_NOFOLLOW.
            if not create and index < len(parts[1:]) - 1:
                continue
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def _read_regular_fd(descriptor: int) -> tuple[bytes, str, int]:
    mode = os.fstat(descriptor).st_mode
    if not stat.S_ISREG(mode):
        raise CASIntegrityError("CAS object is not a regular file")
    os.lseek(descriptor, 0, os.SEEK_SET)
    digest = hashlib.sha256()
    chunks: list[bytes] = []
    byte_count = 0
    while True:
        chunk = os.read(descriptor, 1024 * 1024)
        if not chunk:
            break
        chunks.append(chunk)
        digest.update(chunk)
        byte_count += len(chunk)
    return b"".join(chunks), digest.hexdigest(), byte_count


def _write_all(descriptor: int, data: bytes) -> None:
    offset = 0
    while offset < len(data):
        written = os.write(descriptor, data[offset:])
        if written <= 0:
            raise CASIntegrityError("CAS write stalled")
        offset += written


class ReceiptCAS:
    def __init__(self, root: Path) -> None:
        self.root = Path(root).absolute()
        self.objects = self.root / "sha256"
        self.staging = self.root / "staging"
        root_fd = _open_directory_path_nofollow(
            self.root, create=True, label="CAS root"
        )
        try:
            os.fchmod(root_fd, 0o700)
            objects_fd = _open_directory_at(
                root_fd, "sha256", create=True, label="CAS sha256 directory"
            )
            staging_fd = _open_directory_at(
                root_fd, "staging", create=True, label="CAS staging directory"
            )
            try:
                os.fchmod(objects_fd, 0o700)
                os.fchmod(staging_fd, 0o700)
            finally:
                os.close(objects_fd)
                os.close(staging_fd)
        finally:
            os.close(root_fd)

    def _root_fd(self) -> int:
        return _open_directory_path_nofollow(self.root, create=False, label="CAS root")

    def _path(self, digest: str) -> Path:
        self._validate_digest(digest)
        return self.objects / digest[:2] / digest[2:]

    @staticmethod
    def _validate_digest(digest: str) -> None:
        if (
            type(digest) is not str
            or len(digest) != 64
            or any(c not in "0123456789abcdef" for c in digest)
        ):
            raise ValueError("invalid sha256 digest")

    @staticmethod
    def _digest_file(path: Path) -> tuple[str, int]:
        flags = os.O_RDONLY | _NOFOLLOW
        try:
            descriptor = os.open(path, flags)
        except OSError as exc:
            raise CASIntegrityError("CAS file cannot be opened safely") from exc
        try:
            _, digest, size = _read_regular_fd(descriptor)
            return digest, size
        finally:
            os.close(descriptor)

    @staticmethod
    def _new_temp(staging_fd: int) -> tuple[int, str]:
        for _ in range(128):
            name = f"seal-{secrets.token_hex(16)}"
            try:
                descriptor = os.open(
                    name,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | _NOFOLLOW,
                    0o600,
                    dir_fd=staging_fd,
                )
                return descriptor, name
            except OSError as exc:
                if exc.errno == errno.EEXIST:
                    continue
                raise CASIntegrityError("CAS staging file cannot be created") from exc
        raise CASIntegrityError("CAS staging name allocation exhausted")

    def _seal_from_writer(self, writer: object) -> SealedObject:
        root_fd = self._root_fd()
        objects_fd: int | None = None
        staging_fd: int | None = None
        prefix_fd: int | None = None
        temp_name: str | None = None
        target_name: str | None = None
        incomplete_target = False
        try:
            objects_fd = _open_directory_at(
                root_fd, "sha256", create=False, label="CAS sha256 directory"
            )
            staging_fd = _open_directory_at(
                root_fd, "staging", create=False, label="CAS staging directory"
            )
            temp_fd, temp_name = self._new_temp(staging_fd)
            digest_state = hashlib.sha256()
            size = 0
            try:
                if isinstance(writer, bytes):
                    _write_all(temp_fd, writer)
                    digest_state.update(writer)
                    size = len(writer)
                else:
                    source_fd = int(writer)
                    while True:
                        chunk = os.read(source_fd, 1024 * 1024)
                        if not chunk:
                            break
                        _write_all(temp_fd, chunk)
                        digest_state.update(chunk)
                        size += len(chunk)
                os.fsync(temp_fd)
            finally:
                os.close(temp_fd)
            digest = digest_state.hexdigest()
            prefix_fd = _open_directory_at(
                objects_fd,
                digest[:2],
                create=True,
                label="CAS digest prefix",
            )
            target_name = digest[2:]
            try:
                target_fd = os.open(
                    target_name,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | _NOFOLLOW,
                    0o400,
                    dir_fd=prefix_fd,
                )
                created_new = True
                incomplete_target = True
            except FileExistsError:
                created_new = False
            except OSError as exc:
                raise CASIntegrityError(
                    "CAS object cannot be created atomically"
                ) from exc
            if not created_new:
                try:
                    existing_fd = os.open(
                        target_name,
                        os.O_RDONLY | _NOFOLLOW,
                        dir_fd=prefix_fd,
                    )
                except OSError as exc:
                    raise CASIntegrityError(
                        "existing CAS object is not trusted"
                    ) from exc
                try:
                    existing_stat = os.fstat(existing_fd)
                    if (
                        not stat.S_ISREG(existing_stat.st_mode)
                        or existing_stat.st_nlink != 1
                        or existing_stat.st_uid != os.geteuid()
                        or stat.S_IMODE(existing_stat.st_mode) != 0o400
                    ):
                        raise CASIntegrityError(
                            "existing CAS object has an unsafe inode identity"
                        )
                    _, existing_digest, existing_size = _read_regular_fd(existing_fd)
                finally:
                    os.close(existing_fd)
                if existing_digest != digest or existing_size != size:
                    raise CASIntegrityError("existing CAS object failed readback")
                os.unlink(temp_name, dir_fd=staging_fd)
                temp_name = None
            else:
                try:
                    source_fd = os.open(
                        temp_name,
                        os.O_RDONLY | _NOFOLLOW,
                        dir_fd=staging_fd,
                    )
                    try:
                        while True:
                            chunk = os.read(source_fd, 1024 * 1024)
                            if not chunk:
                                break
                            _write_all(target_fd, chunk)
                    finally:
                        os.close(source_fd)
                    os.fsync(target_fd)
                    target_stat = os.fstat(target_fd)
                    if (
                        not stat.S_ISREG(target_stat.st_mode)
                        or target_stat.st_nlink != 1
                        or target_stat.st_uid != os.geteuid()
                        or stat.S_IMODE(target_stat.st_mode) != 0o400
                    ):
                        raise CASIntegrityError(
                            "new CAS object has an unsafe inode identity"
                        )
                finally:
                    os.close(target_fd)
                incomplete_target = False
                os.fsync(prefix_fd)
                os.unlink(temp_name, dir_fd=staging_fd)
                temp_name = None
            if self.read_verified(digest) != self._read_by_digest_fd(
                prefix_fd, target_name, digest
            ):
                raise CASIntegrityError("CAS seal readback diverged")
            return SealedObject(digest, size)
        finally:
            if incomplete_target and prefix_fd is not None and target_name is not None:
                try:
                    os.unlink(target_name, dir_fd=prefix_fd)
                except FileNotFoundError:
                    pass
            if temp_name is not None and staging_fd is not None:
                try:
                    os.unlink(temp_name, dir_fd=staging_fd)
                except FileNotFoundError:
                    pass
            for descriptor in (prefix_fd, staging_fd, objects_fd, root_fd):
                if descriptor is not None:
                    os.close(descriptor)

    @staticmethod
    def _read_by_digest_fd(prefix_fd: int, name: str, digest: str) -> bytes:
        try:
            descriptor = os.open(name, os.O_RDONLY | _NOFOLLOW, dir_fd=prefix_fd)
        except OSError as exc:
            raise CASIntegrityError("CAS object is missing or not regular") from exc
        try:
            inode = os.fstat(descriptor)
            if (
                not stat.S_ISREG(inode.st_mode)
                or inode.st_nlink != 1
                or inode.st_uid != os.geteuid()
                or stat.S_IMODE(inode.st_mode) != 0o400
            ):
                raise CASIntegrityError("CAS object has an unsafe inode identity")
            data, measured, _ = _read_regular_fd(descriptor)
        finally:
            os.close(descriptor)
        if measured != digest:
            raise CASIntegrityError("CAS digest mismatch")
        return data

    def seal_file(self, source: Path) -> SealedObject:
        flags = os.O_RDONLY | _NOFOLLOW
        try:
            source_fd = os.open(Path(source), flags)
        except OSError as exc:
            raise ValueError("CAS source must be a regular non-symlink file") from exc
        try:
            if not stat.S_ISREG(os.fstat(source_fd).st_mode):
                raise ValueError("CAS source must be a regular non-symlink file")
            return self._seal_from_writer(source_fd)
        finally:
            os.close(source_fd)

    def seal_bytes(self, data: bytes) -> SealedObject:
        if type(data) is not bytes:
            raise TypeError("CAS data must be bytes")
        return self._seal_from_writer(data)

    def read_verified(self, digest: str) -> bytes:
        self._validate_digest(digest)
        root_fd = self._root_fd()
        objects_fd: int | None = None
        prefix_fd: int | None = None
        try:
            objects_fd = _open_directory_at(
                root_fd, "sha256", create=False, label="CAS sha256 directory"
            )
            prefix_fd = _open_directory_at(
                objects_fd,
                digest[:2],
                create=False,
                label="CAS digest prefix",
            )
            return self._read_by_digest_fd(prefix_fd, digest[2:], digest)
        finally:
            for descriptor in (prefix_fd, objects_fd, root_fd):
                if descriptor is not None:
                    os.close(descriptor)
