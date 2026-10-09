"""Incremental, session-local copies of native CLI configuration.

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
