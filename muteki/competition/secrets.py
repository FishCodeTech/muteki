"""比赛平台凭据的协调器私有存储（任务书 10.2 / 设计 13.2，COMP-02）。

现有 ``muteki.control.secrets.SecretStore`` 的契约限定为单个 Run，不能承担
跨 Run 的平台凭据；本模块复用其机制（原子写入、0700/0600、opaque
reference），但使用独立命名空间与作用域：

- 根目录：``control_root/competition/secrets/``（0700）。
- 布局：``<connection_id>/<key>.secret``，每连接一个 0700 子目录。
- 引用形态：``secret://platform/<connection_id>/<key>``。
- 与 Run-local SecretStore 的 ``secret://<uuid>`` 引用前缀不同，解析器互不
  接受对方的引用，保证平台凭据与 Worker Provider 凭据不同命名空间。

competition.db、命令、事件、投影和日志只保存 ``secret://platform/...``
引用；``resolve`` 是唯一返回真实值的操作，只能在传输层注入边界短暂使用。
与 Run-local store 的 no-overwrite 不同，平台凭据支持同键轮换（Token 过期
重新授权），轮换仍走「临时 inode + fsync + os.replace」的原子路径。
"""

from __future__ import annotations

import os
import json
import re
import secrets
import stat
import tempfile
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path

_REFERENCE_PREFIX = "secret://platform/"
_SECRET_SUFFIX = ".secret"
_METADATA_SUFFIX = ".meta.json"
#: connection_id 形如 ``pconn-<hex>``；key 为凭据条目名（token / password …）。
#: 允许集排除分隔符与点，路径逃逸在文件系统边界再显式校验一次。
_SEGMENT_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,127}\Z")


class PlatformSecretError(RuntimeError):
    """平台凭据存储的基础错误；消息只含引用，绝不含 secret 值。"""


class InvalidPlatformSecretReference(PlatformSecretError, ValueError):
    """输入不是规范的 ``secret://platform/<connection_id>/<key>`` 引用。"""


class PlatformSecretNotFound(PlatformSecretError, KeyError):
    """引用形态合法，但存储中不存在对应条目。"""


@dataclass(frozen=True, slots=True)
class PlatformSecretMetadata:
    """可安全进入 API 响应、日志与事件的元数据（不含值）。"""

    reference: str
    connection_id: str
    key: str
    created_at: str = ""
    updated_at: str = ""
    expires_at: str = ""
    rotation: int = 1
    status: str = "active"


class PlatformSecretStore:
    """协调器私有的平台凭据存储。

    ``root`` 必须是协调器私有目录（``control_root/competition/secrets/``），
    不得位于任何 Run workspace 或 Worker 可见挂载点之下。调用方可以持久化
    ``put`` 返回的引用；``resolve`` 返回的值只允许瞬时使用。
    """

    def __init__(self, root: str | os.PathLike[str]) -> None:
        self._root = Path(root)
        self._prepare_dir(self._root)

    @property
    def root(self) -> Path:
        """存储根目录（安全元数据，非 secret 值）。"""
        return self._root

    # ------------------------------------------------------------------
    # 公开操作
    # ------------------------------------------------------------------

    def put(
        self,
        connection_id: str,
        key: str,
        value: str,
        *,
        expires_at: str = "",
    ) -> str:
        """原子写入凭据并返回 opaque 引用。

        同一引用再次写入属于原位更新，引用保持不变。需要让旧引用失效时应
        调用 :meth:`rotate`，避免把更新与轮换混为同一语义。
        """
        self._validate_segment(connection_id)
        self._validate_segment(key)
        if not isinstance(value, str):
            raise TypeError("secret value must be text")
        directory = self._connection_dir(connection_id)
        target = directory / f"{key}{_SECRET_SUFFIX}"
        existing = self._read_metadata(connection_id, key)
        now = datetime.now(timezone.utc).isoformat()
        self._atomic_write(target, value.encode("utf-8"))
        self._write_metadata(PlatformSecretMetadata(
            reference=self._reference(connection_id, key),
            connection_id=connection_id,
            key=key,
            created_at=existing.created_at if existing else now,
            updated_at=now,
            expires_at=str(expires_at or (existing.expires_at if existing else "")),
            rotation=existing.rotation if existing else 1,
            status="active",
        ))
        return self._reference(connection_id, key)

    def rotate(
        self,
        reference: str,
        value: str,
        *,
        key: str = "",
        expires_at: str = "",
        revoke_old: bool = True,
    ) -> PlatformSecretMetadata:
        """生成新引用并删除旧引用；成功返回新引用的安全元数据。"""
        connection_id, old_key, _ = self._parse(reference)
        previous = self.get(reference)
        base = str(key or old_key).split("-r", 1)[0]
        next_key = f"{base}-r{secrets.token_hex(6)}"
        new_ref = self.put(
            connection_id, next_key, value, expires_at=expires_at)
        current = self.get(new_ref)
        self._write_metadata(PlatformSecretMetadata(
            **{
                **asdict(current),
                "created_at": previous.created_at or current.created_at,
                "rotation": max(1, previous.rotation + 1),
            }
        ))
        if revoke_old:
            self.delete(reference)
        return self.get(new_ref)

    def resolve(self, reference: str) -> str:
        """在传输注入边界物化 secret 值（唯一返回值操作）。

        返回值只允许短暂使用（如构造请求头），禁止写入 competition.db、
        命令、事件、投影或日志。
        """
        connection_id, key, path = self._parse(reference)
        self._assert_regular_secret(path, reference)
        raw = path.read_bytes()
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise PlatformSecretError(
                f"secret payload is not valid UTF-8: {reference}"
            ) from exc

    def get(self, reference: str) -> PlatformSecretMetadata:
        """返回引用的安全元数据；不存在时抛 ``PlatformSecretNotFound``。"""
        connection_id, key, path = self._parse(reference)
        self._assert_regular_secret(path, reference)
        self._enforce_file_mode(path)
        metadata = self._read_metadata(connection_id, key)
        if metadata is not None:
            return metadata
        return PlatformSecretMetadata(
            reference=self._reference(connection_id, key),
            connection_id=connection_id,
            key=key,
        )

    def list(self, connection_id: str | None = None) -> tuple[PlatformSecretMetadata, ...]:
        """列出安全元数据（不读取任何 secret 值）。"""
        records: list[PlatformSecretMetadata] = []
        if connection_id is not None:
            self._validate_segment(connection_id)
            directories = [self._root / connection_id]
        else:
            directories = [
                entry
                for entry in self._root.iterdir()
                if entry.is_dir() and not entry.is_symlink()
                and _SEGMENT_RE.fullmatch(entry.name)
            ]
        for directory in directories:
            if not directory.is_dir():
                continue
            for entry in directory.iterdir():
                if not entry.name.endswith(_SECRET_SUFFIX):
                    continue
                key = entry.name[: -len(_SECRET_SUFFIX)]
                if not _SEGMENT_RE.fullmatch(key):
                    continue
                try:
                    self._assert_regular_secret(
                        entry, self._reference(directory.name, key)
                    )
                    self._enforce_file_mode(entry)
                except PlatformSecretError:
                    # 不跟踪、不暴露异常条目（符号链接等）。
                    continue
                records.append(self.get(self._reference(directory.name, key)))
        return tuple(sorted(records, key=lambda item: item.reference))

    def delete(self, reference: str) -> PlatformSecretMetadata:
        """删除凭据，返回被删引用的安全元数据。"""
        connection_id, key, path = self._parse(reference)
        self._assert_regular_secret(path, reference)
        try:
            path.unlink()
        except FileNotFoundError as exc:
            raise PlatformSecretNotFound(f"secret not found: {reference}") from exc
        self._fsync_directory(path.parent)
        metadata = self._read_metadata(connection_id, key) or PlatformSecretMetadata(
            reference=self._reference(connection_id, key),
            connection_id=connection_id,
            key=key,
        )
        try:
            self._metadata_path(connection_id, key).unlink()
        except FileNotFoundError:
            pass
        return metadata

    def delete_connection(self, connection_id: str) -> int:
        """删除某连接的全部凭据（连接删除/重建时调用），返回删除条数。"""
        self._validate_segment(connection_id)
        directory = self._root / connection_id
        if not directory.is_dir() or directory.is_symlink():
            return 0
        removed = 0
        for entry in directory.iterdir():
            try:
                if entry.is_symlink() or not entry.is_file():
                    continue
                entry.unlink()
                if entry.name.endswith(_SECRET_SUFFIX):
                    removed += 1
            except FileNotFoundError:
                continue
        try:
            directory.rmdir()
        except OSError:
            # 目录内残留非常规条目：保守保留，不递归删除。
            pass
        self._fsync_directory(self._root)
        return removed

    # ------------------------------------------------------------------
    # 引用解析与校验
    # ------------------------------------------------------------------

    def _parse(self, reference: str) -> tuple[str, str, Path]:
        if not isinstance(reference, str) or not reference.startswith(_REFERENCE_PREFIX):
            raise InvalidPlatformSecretReference("invalid platform secret reference")
        body = reference[len(_REFERENCE_PREFIX):]
        parts = body.split("/")
        if len(parts) != 2:
            raise InvalidPlatformSecretReference("invalid platform secret reference")
        connection_id, key = parts
        self._validate_segment(connection_id)
        self._validate_segment(key)
        canonical = self._reference(connection_id, key)
        if reference != canonical:
            raise InvalidPlatformSecretReference("invalid platform secret reference")
        path = self._root / connection_id / f"{key}{_SECRET_SUFFIX}"
        if path.parent.parent != self._root:
            raise InvalidPlatformSecretReference("secret reference escapes its store")
        return connection_id, key, path

    @staticmethod
    def _validate_segment(segment: object) -> str:
        if not isinstance(segment, str) or not _SEGMENT_RE.fullmatch(segment):
            raise InvalidPlatformSecretReference("invalid platform secret segment")
        return segment

    @staticmethod
    def _reference(connection_id: str, key: str) -> str:
        return f"{_REFERENCE_PREFIX}{connection_id}/{key}"

    # ------------------------------------------------------------------
    # 文件系统细节（机制复用 muteki.control.secrets.SecretStore）
    # ------------------------------------------------------------------

    def _connection_dir(self, connection_id: str) -> Path:
        directory = self._root / connection_id
        self._prepare_dir(directory)
        return directory

    def _metadata_path(self, connection_id: str, key: str) -> Path:
        return self._root / connection_id / f"{key}{_METADATA_SUFFIX}"

    def _read_metadata(
        self, connection_id: str, key: str
    ) -> PlatformSecretMetadata | None:
        path = self._metadata_path(connection_id, key)
        if not path.is_file() or path.is_symlink():
            return None
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            return PlatformSecretMetadata(**data)
        except (OSError, ValueError, TypeError):
            return None

    def _write_metadata(self, metadata: PlatformSecretMetadata) -> None:
        path = self._metadata_path(metadata.connection_id, metadata.key)
        payload = json.dumps(
            asdict(metadata), ensure_ascii=False, sort_keys=True,
        ).encode("utf-8")
        self._atomic_write(path, payload)

    @staticmethod
    def _prepare_dir(directory: Path) -> None:
        try:
            current = directory.lstat()
        except FileNotFoundError:
            directory.mkdir(mode=0o700, parents=True, exist_ok=False)
            current = directory.lstat()
        if not stat.S_ISDIR(current.st_mode) or stat.S_ISLNK(current.st_mode):
            raise PlatformSecretError("secret store path must be a real directory")
        os.chmod(directory, 0o700)

    def _atomic_write(self, target: Path, payload: bytes) -> None:
        fd, temp_name = tempfile.mkstemp(prefix=".secret-tmp-", dir=target.parent)
        temp = Path(temp_name)
        try:
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "wb", closefd=True) as handle:
                fd = -1
                handle.write(payload)
                handle.flush()
                os.fsync(handle.fileno())
            # rename 原子替换，支持同键凭据轮换（Token 重新授权）。
            os.replace(temp, target)
            os.chmod(target, 0o600)
            self._fsync_directory(target.parent)
        except OSError as exc:
            raise PlatformSecretError("failed to persist secret") from exc
        finally:
            if fd >= 0:
                os.close(fd)
            try:
                temp.unlink()
            except OSError:
                pass

    @staticmethod
    def _assert_regular_secret(path: Path, reference: str) -> os.stat_result:
        try:
            info = path.lstat()
        except FileNotFoundError as exc:
            raise PlatformSecretNotFound(f"secret not found: {reference}") from exc
        if not stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode):
            raise PlatformSecretError(f"unsafe secret entry: {reference}")
        return info

    @staticmethod
    def _enforce_file_mode(path: Path) -> None:
        try:
            os.chmod(path, 0o600, follow_symlinks=False)
        except (OSError, NotImplementedError) as exc:
            raise PlatformSecretError("failed to secure secret file") from exc

    @staticmethod
    def _fsync_directory(directory: Path) -> None:
        flags = os.O_RDONLY
        if hasattr(os, "O_DIRECTORY"):
            flags |= os.O_DIRECTORY
        try:
            fd = os.open(directory, flags)
        except OSError:
            return
        try:
            os.fsync(fd)
        except OSError:
            # 部分文件系统不支持目录 fsync；文件内容已在发布前 fsync。
            pass
        finally:
            os.close(fd)


__all__ = [
    "InvalidPlatformSecretReference",
    "PlatformSecretError",
    "PlatformSecretMetadata",
    "PlatformSecretNotFound",
    "PlatformSecretStore",
]
