"""Explicit native snapshots, application assets, and legacy history migration.

The index owns directory identity; source revisions never create new homes.
Only imported files belong to the synchronizer. Native history and files written
by the CLI are deliberately outside its deletion contract.
"""
from __future__ import annotations

from contextlib import closing, contextmanager
from functools import lru_cache
from hashlib import sha256
import json
import os
from pathlib import Path
import sqlite3
import tempfile
from typing import Callable, Iterator


class NativeEnvironmentError(ValueError):
    code = "chat_plugin.environment_invalid"


@lru_cache(maxsize=65536)
def _file_digest(path: str, signature: tuple[int, ...]) -> str:
    with open(path, "rb") as stream:
        digest = sha256()
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
        return digest.hexdigest()


def file_digest(path: Path) -> str:
    st = path.stat()
    return _file_digest(str(path), (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns))


def source_files(path: Path, relative: Path = Path(), ancestors: frozenset[Path] = frozenset()) -> Iterator[tuple[Path, Path]]:
    if not path.exists():
        return
    real = path.resolve()
    if real in ancestors:
        raise NativeEnvironmentError(f"Native configuration contains a symlink cycle: {path}")
    if path.is_dir():
        for child in sorted(path.iterdir()):
            # Factory rewrites sync_stamp.json; Claude plugin caches maintain
            # .in_use/<pid> leases. Neither changes the imported capabilities.
            if child.name not in {".git", "__pycache__", "logs", ".DS_Store", "sync_stamp.json", ".in_use"}:
                yield from source_files(child, relative / child.name, ancestors | {real})
    elif path.is_file():
        yield relative, path
    else:
        raise NativeEnvironmentError(f"Native configuration is not a regular file: {path}")


def content_revision(
    paths: list[tuple[str, Path]], *,
    json_exclude: dict[str, frozenset[str]] | None = None,
) -> str:
    digest = sha256()
    for label, path in paths:
        for relative, source in source_files(path):
            excluded = (json_exclude or {}).get(label)
            if excluded and source == path:
                try:
                    value = json.loads(source.read_text())
                except (OSError, ValueError) as exc:
                    raise NativeEnvironmentError(f"Cannot read native configuration {source}: {exc}") from exc
                if not isinstance(value, dict):
                    raise NativeEnvironmentError(f"Native configuration must be a JSON object: {source}")
                fingerprint = sha256(json.dumps(
                    {key: value[key] for key in value if key not in excluded},
                    sort_keys=True, separators=(",", ":"),
                ).encode()).hexdigest()
            else:
                fingerprint = file_digest(source)
            digest.update(json.dumps([label, relative.as_posix(), fingerprint]).encode())
    return digest.hexdigest()[:16]


def _target(root: Path, relative: str) -> Path:
    path = root / relative
    if Path(relative).is_absolute() or ".." in Path(relative).parts:
        raise NativeEnvironmentError("Invalid native environment manifest path")
    cursor = path
    while cursor != root:
        if cursor.is_symlink():
            raise NativeEnvironmentError(f"Native environment destination is a symlink: {cursor}")
        cursor = cursor.parent
    return path


@contextmanager
def environment_home(base: Path, engine: str, identity: str, legacy_revision: str | None = None) -> Iterator[Path]:
    """Serialize creation/refresh across service instances and processes."""
    sessions = base / "sessions"
    if sessions.is_symlink():
        raise NativeEnvironmentError("Native sessions directory is a symlink")
    sessions.mkdir(parents=True, exist_ok=True)
    key = sha256(f"v4:{engine}:{identity}".encode()).hexdigest()[:24]
    index = base / "environments.sqlite3"
    with closing(sqlite3.connect(index, timeout=60)) as conn, conn:
        index.chmod(0o600)
        conn.execute("CREATE TABLE IF NOT EXISTS homes (identity TEXT PRIMARY KEY, directory TEXT NOT NULL, engine TEXT NOT NULL, owner TEXT NOT NULL)")
        conn.execute("BEGIN IMMEDIATE")
        row = conn.execute("SELECT directory FROM homes WHERE identity=?", (key,)).fetchone()
        directory = row[0] if row else key
        if row is None and legacy_revision:
            legacy = sha256(f"v3:{engine}:{identity}:{legacy_revision}".encode()).hexdigest()[:24]
            if (sessions / legacy / ".ready").is_file():
                directory = legacy  # Keep absolute native resume paths valid.
        if len(directory) != 24 or any(c not in "0123456789abcdef" for c in directory):
            raise NativeEnvironmentError("Invalid native environment index")
        root = _target(sessions, directory)
        root.mkdir(exist_ok=True, mode=0o700)
        root.chmod(0o700)
        conn.execute("INSERT OR IGNORE INTO homes VALUES (?,?,?,?)", (key, directory, engine, identity))
        yield root


def synchronize(
    root: Path, imports: list[tuple[Path, Path]], copy_file: Callable,
    replacements: tuple[tuple[str, str], ...] = (),
) -> bool:
    """Refresh changed imports; preserve runtime mutations until source changes.

    Interrupted imports remain retryable: the manifest is committed only after
    all copies. Atomic file replacement also prevents writes through hard links.
    """
    manifest = _target(root, ".imports.json")
    previous = json.loads(manifest.read_text()) if manifest.exists() else {}
    current = {}
    changed = False
    expected = {}
    for source_root, destination in imports:
        for relative, source in source_files(source_root):
            expected[(destination / relative).as_posix()] = source
    for name, source in expected.items():
        target = _target(root, name)
        digest = file_digest(source)
        old = previous.get(name, {})
        if old.get("source") == digest and target.is_file():
            current[name] = old
            continue
        target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd, temporary = tempfile.mkstemp(prefix=".import-", dir=target.parent)
        os.close(fd)
        tmp = Path(temporary)
        try:
            tmp.unlink()  # clonefile requires a destination that does not exist.
            copy_file(source, tmp)
            if file_digest(tmp) != digest:
                raise NativeEnvironmentError(f"Native configuration changed during import: {source}")
            if target.suffix in {".json", ".toml", ".yaml", ".yml", ".jsonc"} and tmp.stat().st_size < 2_000_000:
                try:
                    original = tmp.read_text()
                except UnicodeError:
                    original = None
                if original is not None:
                    value = original
                    for before, after in replacements:
                        value = value.replace(before, after)
                    if value != original:
                        tmp.write_text(value)
            tmp.chmod(source.stat().st_mode & 0o700 | 0o600)
            output = file_digest(tmp)
            os.replace(tmp, target)
            current[name] = {"source": digest, "output": output}
            changed = True
        finally:
            tmp.unlink(missing_ok=True)
    for name, old in previous.items():
        if name in current:
            continue
        target = _target(root, name)
        if target.is_file() and file_digest(target) == old["output"]:
            target.unlink()
            changed = True
    fd, temporary = tempfile.mkstemp(prefix=".imports-", dir=root)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(current, stream)
        os.replace(temporary, manifest)
    finally:
        Path(temporary).unlink(missing_ok=True)
    return changed


def evict_imports(root: Path, prefixes: tuple[str, ...]) -> int:
    """Release unmodified materialized assets, never native history or edits.

    Keep the manifest: synchronize notices missing files and restores them on
    next use. A caller must first close the native session successfully.
    """
    manifest = _target(root, ".imports.json")
    if not manifest.exists():
        return 0
    removed = 0
    for name, metadata in json.loads(manifest.read_text()).items():
        if not any(name == prefix or name.startswith(prefix + "/") for prefix in prefixes):
            continue
        path = _target(root, name)
        if path.is_file() and file_digest(path) == metadata["output"]:
            removed += path.stat().st_size
            path.unlink()
    return removed


def migrate_history(source: Path, destination: Path, copy_file: Callable) -> None:
    """Import legacy native transcripts only, retaining the complete old home.

    Never import credentials, configuration, plugins or SQLite/WAL files. The
    native client owns indexing. Previously imported transcripts may be updated
    by that client, so a migration receipt prevents replaying stale copies.
    """
    if not source.is_dir() or source.resolve() == destination.resolve():
        return
    receipt = source.parent.parent / ".history-migration.json"
    previous = json.loads(receipt.read_text()) if receipt.exists() else {}
    identity = str(destination.resolve())
    migrated = previous.get(identity, {})
    current = dict(migrated)
    for directory in ("sessions", "archived_sessions", "projects"):
        for relative, original in source_files(source / directory):
            name = (Path(directory) / relative).as_posix()
            # History directories occasionally contain databases; raw database
            # copying is never safe while a native client may own the target.
            if original.suffix in {".sqlite", ".db"} or original.name.endswith(("-wal", "-shm")):
                continue
            digest = file_digest(original)
            if migrated.get(name) == digest:
                continue
            target = _target(destination, name)
            if target.exists():
                if not target.is_file() or file_digest(target) != digest:
                    raise NativeEnvironmentError(f"Native history migration conflict: {target}; legacy history retained at {original}")
            else:
                target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                fd, temporary = tempfile.mkstemp(prefix=".history-", dir=target.parent)
                os.close(fd)
                tmp = Path(temporary)
                try:
                    tmp.unlink()
                    copy_file(original, tmp)
                    if file_digest(tmp) != digest:
                        raise NativeEnvironmentError(f"Native history changed during migration: {original}")
                    tmp.chmod(0o600)
                    # Publish without replacing a concurrently created native file.
                    try:
                        os.link(tmp, target)
                    except FileExistsError:
                        if not target.is_file() or file_digest(target) != digest:
                            raise NativeEnvironmentError(f"Native history migration conflict: {target}")
                finally:
                    tmp.unlink(missing_ok=True)
            current[name] = digest
    if current == migrated:
        return
    previous[identity] = current
    fd, temporary = tempfile.mkstemp(prefix=".history-receipt-", dir=receipt.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(previous, stream)
        os.replace(temporary, receipt)
    finally:
        Path(temporary).unlink(missing_ok=True)


def migrate_cursor_history(source: Path, destination: Path, session_id: str) -> str:
    """Move one recorded Cursor chat into native storage through SQLite backup."""
    import re
    import shutil
    if not re.fullmatch(r"[a-zA-Z0-9-]+", session_id or ""):
        raise NativeEnvironmentError("Invalid recorded Cursor session id")
    chats = source / "chats"
    matches = list(chats.glob("*/" + session_id)) if chats.is_dir() else []
    if not matches:
        return ""
    if len(matches) != 1:
        raise NativeEnvironmentError("Legacy Cursor chat directory is ambiguous")
    original = matches[0]
    if original.is_symlink() or not original.resolve().is_relative_to(source.resolve()):
        raise NativeEnvironmentError("Legacy Cursor chat directory escapes its home")
    unknown = {p.name for p in original.iterdir()} - {"store.db", "store.db-wal", "store.db-shm", "meta.json"}
    if unknown or not (original / "store.db").is_file():
        raise NativeEnvironmentError("Legacy Cursor chat layout is unsupported")
    for path in original.iterdir():
        if path.is_symlink():
            raise NativeEnvironmentError("Legacy Cursor chat contains a symlink")
    relative = original.relative_to(source)
    target = _target(destination, relative.as_posix())
    if original.resolve() == target.resolve():
        return str(target)
    def database_digest(path: Path) -> str:
        digest = sha256()
        with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as conn:
            conn.execute("BEGIN")
            for statement in conn.iterdump():
                digest.update(statement.encode())
                digest.update(b"\n")
        return digest.hexdigest()
    metadata = original / "meta.json"
    fingerprint = {"database": database_digest(original / "store.db"),
                   "metadata": file_digest(metadata) if metadata.is_file() else ""}
    receipt = source.parent.parent / ".cursor-history-migration.json"
    previous = json.loads(receipt.read_text()) if receipt.exists() else {}
    key = str(target.resolve())
    if previous.get(key) == fingerprint and target.is_dir():
        return str(target)
    if target.exists():
        if (not target.is_dir() or not (target / "store.db").is_file()
                or database_digest(target / "store.db") != fingerprint["database"]
                or (file_digest(target / "meta.json") if (target / "meta.json").is_file() else "") != fingerprint["metadata"]):
            raise NativeEnvironmentError(f"Cursor native history conflict at {target}; legacy chat retained at {original}")
    else:
        target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        staging = Path(tempfile.mkdtemp(prefix=".cursor-history-", dir=target.parent))
        try:
            backed_up = staging / "store.db"
            with closing(sqlite3.connect((original / "store.db").as_uri() + "?mode=ro", uri=True)) as native:
                with closing(sqlite3.connect(backed_up)) as output:
                    native.backup(output)
            backed_up.chmod(0o600)
            if database_digest(backed_up) != fingerprint["database"]:
                raise NativeEnvironmentError("Cursor history changed during migration")
            if metadata.is_file():
                shutil.copy2(metadata, staging / "meta.json")
                (staging / "meta.json").chmod(0o600)
                if file_digest(staging / "meta.json") != fingerprint["metadata"]:
                    raise NativeEnvironmentError("Cursor history metadata changed during migration")
            # An exclusive directory claim prevents replacing a concurrent native
            # chat. A failed publish retains the legacy chat and exposes failure.
            target.mkdir(mode=0o700)
            published = []
            try:
                for name in ("store.db", "meta.json"):
                    file = staging / name
                    if not file.is_file():
                        continue
                    os.link(file, target / file.name)
                    published.append((target / file.name, file.stat().st_ino))
            except Exception:
                # Roll back only files still owned by this failed publication.
                for file, inode in published:
                    if file.is_file() and file.stat().st_ino == inode:
                        file.unlink()
                if target.is_dir() and not any(target.iterdir()):
                    target.rmdir()
                raise
        finally:
            shutil.rmtree(staging)
    previous[key] = fingerprint
    fd, temporary = tempfile.mkstemp(prefix=".cursor-receipt-", dir=receipt.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(previous, stream)
        os.replace(temporary, receipt)
    finally:
        Path(temporary).unlink(missing_ok=True)
    return str(target)
