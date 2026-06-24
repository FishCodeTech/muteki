"""Run workspace materialization: immutable inputs, shared CAS, and manifest.

The workspace protocol is intentionally local-filesystem only.  Both host-local
workers and container workers see the same layout under ``sessions/<run>/workspace``:
inputs are content-addressed, shared artifacts are content-addressed, and worker
directories only contain relative symlinks into those stable locations.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import tempfile
import time
from pathlib import Path
from typing import Any, Iterable


def workspace_root_for_worker(wd: str | Path) -> Path:
    """Return the run workspace root for a worker cwd.

    Normal web runs use ``workspace/workers/<worker-id>``.  Unit tests and older
    callers may pass an arbitrary cwd; in that case the cwd's parent becomes a
    lightweight workspace root so local execution still uses the CAS protocol.
    """
    p = Path(wd).resolve()
    if p.parent.name == "workers":
        return p.parent.parent
    return p.parent


def ensure_workspace(root: str | Path, *, runtime: dict[str, Any] | None = None) -> Path:
    root = Path(root)
    for rel in (
        "inputs/by-name",
        "inputs/objects",
        "shared/objects",
        "shared/links",
        "graph",
        "workers",
        "homes",
        "tmp",
        "logs",
        "final",
    ):
        (root / rel).mkdir(parents=True, exist_ok=True)
    index = root / "shared" / "index.jsonl"
    index.touch(exist_ok=True)
    write_manifest(root, runtime=runtime)
    return root


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _sha256_dir(path: Path) -> str:
    h = hashlib.sha256()
    for item in sorted(p for p in path.rglob("*") if p.is_file()):
        rel = item.relative_to(path).as_posix().encode()
        h.update(rel + b"\0")
        with item.open("rb") as f:
            for chunk in iter(lambda: f.read(1024 * 1024), b""):
                h.update(chunk)
    return h.hexdigest()


def sha256_path(path: str | Path) -> str:
    p = Path(path)
    return _sha256_dir(p) if p.is_dir() else _sha256_file(p)


def object_path(root: str | Path, area: str, sha256: str) -> Path:
    if area not in {"inputs", "shared"}:
        raise ValueError(f"unknown CAS area: {area}")
    return Path(root) / area / "objects" / sha256[:2] / sha256[2:4] / sha256


def _atomic_materialize(src: Path, dst: Path) -> None:
    if dst.exists():
        return
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp_parent = dst.parent
    tmp = tmp_parent / f".{dst.name}.staging.{os.getpid()}.{time.time_ns()}"
    try:
        if src.is_dir():
            shutil.copytree(src, tmp)
        else:
            try:
                os.link(src, tmp)
            except OSError:
                shutil.copy2(src, tmp)
        os.replace(tmp, dst)
    except FileExistsError:
        pass
    finally:
        if tmp.exists():
            if tmp.is_dir():
                shutil.rmtree(tmp, ignore_errors=True)
            else:
                try:
                    tmp.unlink()
                except OSError:
                    pass


def _replace_symlink(link: Path, target: Path) -> None:
    link.parent.mkdir(parents=True, exist_ok=True)
    try:
        link.unlink()
    except FileNotFoundError:
        pass
    link.symlink_to(target)


def relative_symlink(link: str | Path, target: str | Path) -> None:
    link = Path(link)
    target = Path(target)
    rel = os.path.relpath(target, start=link.parent)
    _replace_symlink(link, Path(rel))


def materialize_input(root: str | Path, src: str | Path, *, name: str | None = None) -> dict[str, Any]:
    root = ensure_workspace(root)
    srcp = Path(src).resolve()
    if not srcp.exists():
        raise FileNotFoundError(srcp)
    digest = sha256_path(srcp)
    obj = object_path(root, "inputs", digest)
    _atomic_materialize(srcp, obj)
    clean_name = Path(name or srcp.name).name
    by_name = root / "inputs" / "by-name" / clean_name
    relative_symlink(by_name, obj)
    write_manifest(root)
    return {
        "name": clean_name,
        "sha256": digest,
        "object": obj,
        "by_name": by_name,
        "kind": "directory" if srcp.is_dir() else "file",
    }


def materialize_shared_artifact(
    root: str | Path,
    src: str | Path,
    *,
    name: str | None = None,
    kind: str = "derived",
    status: str = "available",
    metadata: dict[str, Any] | None = None,
) -> dict[str, Any]:
    root = ensure_workspace(root)
    srcp = Path(src).resolve()
    if not srcp.exists():
        raise FileNotFoundError(srcp)
    digest = sha256_path(srcp)
    obj = object_path(root, "shared", digest)
    _atomic_materialize(srcp, obj)
    clean_name = Path(name or srcp.name).name
    link = root / "shared" / "links" / clean_name
    relative_symlink(link, obj)
    row = {
        "ts": time.time(),
        "kind": kind,
        "status": status,
        "name": clean_name,
        "sha256": digest,
        "path": obj.relative_to(root).as_posix(),
        **(metadata or {}),
    }
    # index.jsonl is a rebuildable materialized view; callers should treat
    # shared_graph events as truth once artifact events exist.
    with (root / "shared" / "index.jsonl").open("a", encoding="utf-8") as f:
        f.write(json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n")
    write_manifest(root)
    return {**row, "object": obj, "link": link}


def link_input_into_worker(root: str | Path, wd: str | Path, name: str) -> Path:
    root = ensure_workspace(root)
    dst = Path(wd) / Path(name).name
    src = root / "inputs" / "by-name" / Path(name).name
    relative_symlink(dst, src)
    return dst


def link_shared_into_worker(root: str | Path, wd: str | Path, name: str, sha256: str) -> Path:
    root = ensure_workspace(root)
    dst = Path(wd) / "shared" / Path(name).name
    src = object_path(root, "shared", sha256)
    relative_symlink(dst, src)
    return dst


def write_manifest(root: str | Path, *, runtime: dict[str, Any] | None = None) -> Path:
    root = Path(root)
    root.mkdir(parents=True, exist_ok=True)
    inputs: list[dict[str, Any]] = []
    by_name = root / "inputs" / "by-name"
    if by_name.exists():
        for link in sorted(by_name.iterdir(), key=lambda p: p.name):
            try:
                resolved = link.resolve()
                sha = resolved.name
            except OSError:
                sha = ""
            inputs.append({
                "name": link.name,
                "sha256": sha,
                "path": link.relative_to(root).as_posix(),
                "object": f"inputs/objects/{sha[:2]}/{sha[2:4]}/{sha}" if sha else "",
            })
    manifest = {
        "version": 1,
        "topology": {
            "inputs": "inputs",
            "shared": "shared",
            "graph": "graph",
            "workers": "workers",
            "homes": "homes",
            "tmp": "tmp",
            "logs": "logs",
            "final": "final",
        },
        "inputs": inputs,
        "runtime": runtime or {},
        "artifact_truth": "shared_graph.events",
        "shared_index": "shared/index.jsonl (rebuildable materialized view)",
    }
    path = root / "manifest.json"
    fd, tmp_name = tempfile.mkstemp(prefix=".manifest.", suffix=".json", dir=str(root))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(manifest, f, ensure_ascii=False, indent=2, sort_keys=True)
        os.replace(tmp_name, path)
    finally:
        try:
            os.unlink(tmp_name)
        except FileNotFoundError:
            pass
    return path


def cleanup_worker_scratch(worker_root: str | Path, *, keep: Iterable[str] = ()) -> list[Path]:
    """Remove finished/failed worker scratch directories under ``workers/``.

    Callers can keep winner/current worker ids.  The function never touches
    sibling workspace directories such as shared, graph, final, or CAS objects.
    """
    root = Path(worker_root)
    keep_set = set(keep)
    removed: list[Path] = []
    if not root.exists():
        return removed
    for child in root.iterdir():
        if not child.is_dir() or child.name.startswith("_") or child.name in keep_set:
            continue
        shutil.rmtree(child, ignore_errors=True)
        removed.append(child)
    return removed
