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
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from importlib.metadata import PackageNotFoundError, version as package_version
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


def _app_version() -> str:
    try:
        return package_version("project-muteki")
    except PackageNotFoundError:
        return ""


def _git_commit() -> str:
    repo = Path(__file__).resolve().parents[2]
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=repo,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=2,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return ""
    if result.returncode != 0:
        return ""
    return (result.stdout or "").strip()


def worker_image_identity(image: str) -> dict[str, str]:
    identity = {"name": image, "id": "", "digest": ""}
    try:
        result = subprocess.run(
            [
                "docker", "image", "inspect", image,
                "--format", "{{.Id}}\n{{index .RepoDigests 0}}",
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=8,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return identity
    if result.returncode != 0:
        return identity
    lines = [line.strip() for line in (result.stdout or "").splitlines() if line.strip()]
    if lines:
        identity["id"] = lines[0]
    if len(lines) > 1 and lines[1] not in {"<no value>", "<nil>"}:
        identity["digest"] = lines[1]
    return identity


def run_identity() -> dict[str, str]:
    return {
        "app_version": _app_version(),
        "git_commit": _git_commit(),
        "generated_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    }


def load_manifest_runtime(root: str | Path) -> dict[str, Any]:
    path = Path(root) / "manifest.json"
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    runtime = data.get("runtime") if isinstance(data, dict) else None
    return dict(runtime) if isinstance(runtime, dict) else {}


def merge_manifest_runtime(
    existing: dict[str, Any],
    update: dict[str, Any] | None,
) -> dict[str, Any]:
    merged = dict(existing)
    if update:
        merged.update(update)
    return merged


def ensure_workspace(
    root: str | Path,
    *,
    runtime: dict[str, Any] | None = None,
    include_graph: bool = False,
) -> Path:
    root = Path(root)
    directories = [
        "inputs/by-name",
        "inputs/objects",
        "shared/objects",
        "shared/links",
        "shared/live",
        "workers",
        "homes",
        "tmp",
        "logs",
        "final",
    ]
    if include_graph:
        directories.append("graph")
    for rel in directories:
        (root / rel).mkdir(parents=True, exist_ok=True)
    index = root / "shared" / "index.jsonl"
    index.touch(exist_ok=True)
    write_manifest(root, runtime=runtime)
    return root


# Candidate toolbox paths for container images (full Kali + Ubuntu slim).
# Projection must probe the chosen image: slim lacks most of these.
_CONTAINER_DIR_CANDIDATES: dict[str, Path] = {
    "target-binaries": Path("/usr/share/chisel-common-binaries"),
    "wordlists": Path("/usr/share/seclists"),
    "pocs": Path("/home/kali/pocs"),
    "knowledge": Path("/home/kali/knowledges"),
    "nuclei-templates": Path("/home/kali/.local/nuclei-templates"),
}
_CONTAINER_BIN_CANDIDATES: dict[str, Path] = {
    "chisel": Path("/usr/bin/chisel"),
    "proxychains4": Path("/usr/bin/proxychains4"),
    "tmux": Path("/usr/bin/tmux"),
}

# Cache immutable image identity + exact probe paths; failed probes are retriable.
_image_toolbox_path_cache: dict[tuple[str, tuple[str, ...]], frozenset[str]] = {}


def clear_toolbox_probe_cache() -> None:
    _image_toolbox_path_cache.clear()


def container_toolbox_candidates() -> dict[str, Path]:
    return {**_CONTAINER_DIR_CANDIDATES, **_CONTAINER_BIN_CANDIDATES}


def probe_image_paths(image: str, paths: Iterable[str | Path], *, use_cache: bool = True) -> set[str]:
    image = str(image or "").strip()
    wanted = tuple(sorted({str(Path(p)) for p in paths if str(p).strip()}))
    if not image or not wanted:
        return set()
    name = f"muteki-toolbox-probe-{os.getpid()}-{time.time_ns()}"
    try:
        identity = subprocess.run(["docker", "image", "inspect", "--format", "{{.Id}}", image],
                                  capture_output=True, text=True, timeout=20, check=False)
        if identity.returncode != 0 or not identity.stdout.strip():
            return set()
        image_id = identity.stdout.strip()
        key = (image_id, wanted)
        if use_cache and key in _image_toolbox_path_cache:
            return set(_image_toolbox_path_cache[key])
        result = subprocess.run([
            "docker", "run", "--rm", "--name", name, "--pull", "never",
            "--network", "none", "--read-only", "--user", "kali", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges=true", "--entrypoint", "/bin/sh", image_id,
            "-c", 'for p do if [ -e "$p" ]; then printf "%s\\n" "$p"; fi; done; exit 0',
            "probe", *wanted,
        ], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=120, check=False)
    except subprocess.TimeoutExpired:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True, timeout=20, check=False)
        return set()
    except OSError:
        return set()
    if result.returncode != 0:
        return set()
    present = frozenset(line for line in result.stdout.splitlines() if line in wanted)
    if use_cache:
        _image_toolbox_path_cache[key] = present
    return set(present)


def stage_worker_toolbox(
    worker_dir: str | Path,
    *,
    container: bool = False,
    image: str | None = None,
    present_paths: Iterable[str | Path] | None = None,
) -> dict[str, Any]:
    """Project backend tool paths at the stable ``./toolbox`` path.

    Only link targets that exist in the selected backend (local tree, or the
    chosen container image). Slim images must not receive dangling full-only
    links. Does not restore the retired TSec ``toolbox/MANIFEST.json``.
    """
    wd = Path(worker_dir).resolve()
    toolbox = wd / "toolbox"
    if toolbox.is_symlink():
        raise ValueError("toolbox root must not be a symlink")
    toolbox.mkdir(parents=True, exist_ok=True)
    repo_root = Path(__file__).resolve().parents[2]
    local_root = Path(
        os.environ.get("MUTEKI_LOCAL_WORKER_ROOT")
        or os.environ.get("MUTEKI_MAC_WORKER_ROOT")
        or repo_root / "ctf-tools"
    ).resolve()

    dir_mappings: dict[str, Path]
    bin_mappings: dict[str, Path] = {}

    if container:
        dir_mappings = dict(_CONTAINER_DIR_CANDIDATES)
        bin_mappings = dict(_CONTAINER_BIN_CANDIDATES)
        candidates = [str(p) for p in (*dir_mappings.values(), *bin_mappings.values())]
        if present_paths is not None:
            available = {str(Path(p)) for p in present_paths}
        else:
            probe_image = (image or os.environ.get("MUTEKI_WORKER_IMAGE") or "").strip()
            available = (
                probe_image_paths(probe_image, candidates) if probe_image else set()
            )
    else:
        dir_mappings = {
            "bin": local_root / "bin",
            "target-binaries": local_root / "tools" / "target-binaries",
            "wordlists": local_root / "wordlists",
            "pocs": local_root / "pocs",
            "knowledge": local_root / "knowledges",
            "nuclei-templates": local_root / "nuclei-templates",
        }
        if present_paths is not None:
            available = {str(Path(p)) for p in present_paths}
        else:
            available = {str(target) for target in dir_mappings.values() if target.exists()}

    kept_dirs: set[str] = set()
    kept_bins: set[str] = set()

    for logical, target in dir_mappings.items():
        if str(target) not in available:
            continue
        if (toolbox / logical).exists() and not (toolbox / logical).is_symlink():
            continue
        _replace_symlink(toolbox / logical, target)
        kept_dirs.add(logical)


    if bin_mappings:
        bin_dir = toolbox / "bin"
        if bin_dir.is_symlink():
            bin_dir.unlink()
        bin_dir.mkdir(exist_ok=True)
        for name, target in bin_mappings.items():
            if str(target) not in available:
                continue
            if (bin_dir / name).exists() and not (bin_dir / name).is_symlink():
                continue
            _replace_symlink(bin_dir / name, target)
            kept_bins.add(name)


    # Remove stale projections left from a previous (e.g. full-image) staging.
    known_dir_names = set(dir_mappings) | set(_CONTAINER_DIR_CANDIDATES) | {
        "bin", "target-binaries", "wordlists", "pocs", "knowledge", "nuclei-templates",
    }
    for logical in known_dir_names:
        if logical in kept_dirs:
            continue
        link = toolbox / logical
        try:
            if link.is_symlink():
                link.unlink()
        except OSError:
            pass

    bin_dir = toolbox / "bin"
    if bin_dir.is_dir() and not bin_dir.is_symlink():
        known_bin_names = set(_CONTAINER_BIN_CANDIDATES)
        for child in list(bin_dir.iterdir()):
            if child.name in kept_bins:
                continue
            if child.name not in known_bin_names:
                continue
            try:
                if child.is_symlink():
                    child.unlink()
            except OSError:
                pass
        try:
            if bin_dir.is_dir() and not any(bin_dir.iterdir()) and not kept_bins:
                bin_dir.rmdir()
        except OSError:
            pass

    # The curated inventory is retired: do not regenerate or inject it. A
    # same-named file may belong to the operator; without an ownership marker
    # there is no safe basis for deleting it during ordinary restaging.
    return {}  # keep the retired TSec inventory out of Worker prompts



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
    topology = {
        "inputs": "inputs",
        "shared": "shared",
        "workers": "workers",
        "homes": "homes",
        "tmp": "tmp",
        "logs": "logs",
        "final": "final",
    }
    if (root / "graph").is_dir():
        topology["graph"] = "graph"
    manifest = {
        "version": 1,
        "topology": topology,
        "inputs": inputs,
        "runtime": merge_manifest_runtime(load_manifest_runtime(root), runtime),
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
    sibling workspace directories such as shared, final, or CAS objects.
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
