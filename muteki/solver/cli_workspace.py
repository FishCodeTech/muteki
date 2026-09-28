"""Workspace and path helpers for CliSolver. Moved from cli_solver.py."""
from __future__ import annotations

import asyncio
import hashlib
import os
import re
import shutil
from pathlib import Path, PurePosixPath
from typing import Optional

from muteki.solver.workspace import (
    ensure_workspace,
    link_input_into_worker,
    link_shared_into_worker,
    materialize_input,
    relative_symlink,
    stage_worker_toolbox,
    workspace_root_for_worker,
)
from muteki.solver.cli_protocol import (
    _REPO_BLACKBOARD_SCRIPT,
    _WORKER_PATH_PREFIX,
)

def _stable_worker_path(current: str) -> str:
    """Put system tool dirs before host shims without dropping the user's PATH."""
    parts: list[str] = []
    seen: set[str] = set()
    for item in [*_WORKER_PATH_PREFIX, *current.split(os.pathsep)]:
        if not item or item in seen:
            continue
        seen.add(item)
        parts.append(item)
    return os.pathsep.join(parts)


def _repo_blackboard_script() -> Optional[str]:
    """Absolute path to the IN-REPO blackboard skill if we're running from a source
    checkout, else None.

    A non-containerized worker invokes the skill purely as
    `python3 "$MUTEKI_BLACKBOARD_SCRIPT" <subcommand>` — so whatever path we hand it
    is the ONLY copy that runs. Historically that pointed at the DEPLOYED copy under
    ~/.claude or ~/.agents (installed once by scripts/install_blackboard_skill.sh),
    which silently rotted whenever the repo skill changed: run-75378 shipped workers a
    skill missing the entire G0-G4 + lifecycle landing (stale dedupe_key, no
    _retired_fact_seqs filter, no dispatch_state fence), half-defeating the run-75377
    echo-dedup fix. Pointing source runs straight at the repo copy removes that drift
    class entirely — there is no second copy to fall out of sync."""
    p = _REPO_BLACKBOARD_SCRIPT
    try:
        return str(p) if p.is_file() else None
    except OSError:
        return None


def _file_sha256(path: Path) -> Optional[str]:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def sync_deployed_blackboard_skills() -> list[dict]:
    """Compatibility shim retained for older callers.

    User-level installation is intentionally disabled.  Skill projection now
    happens in :func:`worker_skills.stage_blackboard_skill` for each Worker cwd.
    """
    return []


def _blackboard_script_path(self, cwd: str | None = None) -> str:
    # Container: baked into the worker image at /usr/local/bin/blackboard.py
    # (docker/worker/blackboard.py), kept fresh by image rebuilds.
    if self.container is not None:
        if getattr(self.challenge, "mode", "ctf") == "pentest" and cwd:
            staged = Path(cwd).resolve() / ".muteki" / "blackboard.py"
            mapper = getattr(self.container, "to_container_path", None)
            if staged.is_file() and callable(mapper):
                return str(mapper(str(staged)))
            raise FileNotFoundError("Pentest Blackboard tool was not staged")
        return "/usr/local/bin/blackboard.py"
    # Source checkout: run the repo copy DIRECTLY — no deployed copy to drift
    # out of sync (see _repo_blackboard_script). This is the common case for
    # `./run.sh` from a working tree.
    repo = _repo_blackboard_script()
    if repo is not None:
        return repo
    raise FileNotFoundError(
        "project muteki-blackboard script is unavailable; "
        "user-level Skill directories are not a Worker fallback")


def _spill_host_path(self, spill_path: str) -> "Optional[tuple[Path, Path]]":
    """Map one engine spill pointer to (active cwd, host candidate).

    The path is hostile metadata. Accept only paths beneath the exact active
    worker cwd; a sibling worker, graph DB, shared artifact, or arbitrary host
    file must never become this execution's output provenance.
    """
    cwd = self._current_workdir
    if cwd is None or not spill_path:
        return None
    raw = str(spill_path)
    if self.container is None:
        candidate = Path(raw) if Path(raw).is_absolute() else cwd / raw
        return cwd, candidate
    mapper = getattr(self.container, "to_container_cwd", None)
    if not callable(mapper):
        return None
    try:
        container_cwd = PurePosixPath(str(mapper(str(cwd))))
        reported = PurePosixPath(raw)
    except (TypeError, ValueError):
        return None
    candidate_container = (reported if reported.is_absolute()
                           else container_cwd / reported)
    try:
        rel = candidate_container.relative_to(container_cwd)
    except ValueError:
        return None
    if any(part in {"", ".", ".."} for part in rel.parts):
        return None
    return cwd, cwd.joinpath(*rel.parts)


def _stage_attachments(self, wd: Path) -> list[str]:
    """Materialize challenge attachments into the run workspace CAS, then link
    them into the worker cwd using their original basenames.

    The worker-facing compatibility contract remains `./<name>` so existing
    prompts and traces continue to work. The storage contract changes: bytes live
    once under `workspace/inputs/objects/<sha-prefix>/<sha>`, `inputs/by-name`
    points at the immutable object, and the cwd entry points at `inputs/by-name`.
    """
    wd = Path(wd).resolve()
    if getattr(self.challenge, "mode", "ctf") == "pentest":
        staged_board = wd / ".muteki" / "blackboard.py"
        staged_board.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(_REPO_BLACKBOARD_SCRIPT, staged_board)
        staged_board.chmod(0o444)
    if (getattr(self.challenge, "mode", "ctf") == "pentest"
            and getattr(self.driver, "name", "") == "pi"):
        extension = Path(__file__).with_name("pi_pentest_tool_timeout.ts")
        staged_extension = wd / ".muteki" / extension.name
        staged_extension.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(extension, staged_extension)
        staged_extension.chmod(0o444)
        flags = tuple(getattr(self.driver, "_CONTEXT_FLAGS", ()) or ())
        extension_arg = str(Path(".muteki") / extension.name)
        if extension_arg not in flags:
            self.driver._CONTEXT_FLAGS = (*flags, "--extension", extension_arg)
    root = workspace_root_for_worker(wd)
    ensure_workspace(root, runtime={
        "backend": "container" if getattr(self, "container", None) is not None else "local",
        "run_id": getattr(self, "run_id", getattr(self.challenge, "id", "")),
    })
    if (
        getattr(self.challenge, "mode", "ctf") == "ctf"
        and wd.parent.name == "workers"
    ):
        relative_symlink(wd / "shared", root / "shared" / "live")
    container = getattr(self, "container", None)
    self._toolbox_manifest = stage_worker_toolbox(
        wd,
        container=container is not None,
        image=getattr(container, "image", None) if container is not None else None,
    )
    instruction = str(
        getattr(getattr(self.challenge, "task_contract", None), "raw_instruction", "")
        or getattr(self.challenge, "description", "")
        or ""
    )
    task_path = wd / ".muteki_task.md"
    task_path.write_text(instruction, encoding="utf-8")
    task_path.chmod(0o444)
    self._task_file_name = task_path.name
    self._task_file_sha256 = hashlib.sha256(
        instruction.encode("utf-8", errors="replace")
    ).hexdigest()
    staged: list[str] = []
    for src in (self.challenge.attachments or []):
        logical_name = Path(src).name
        p = Path(src).resolve()
        if not p.exists():
            continue
        try:
            materialize_input(root, p, name=logical_name)
            link_input_into_worker(root, wd, logical_name)
            staged.append(logical_name)
        except (OSError, FileNotFoundError):
            continue
    self._link_existing_shared_artifacts(root, wd)
    self._link_inherited_pocs(root, wd)
    return staged


@staticmethod
def _ensure_shared_attachment(src: Path, shared_root: Path) -> bool:
    """Back-compat wrapper for old call sites: materialize as input CAS."""
    try:
        materialize_input(shared_root, src, name=src.name)
        return True
    except (OSError, FileNotFoundError):
        return False


@staticmethod
def _link_shared_attachment(dst: Path, target: str | Path) -> bool:
    """Make `dst` a relative symlink to a possibly multi-segment target."""
    try:
        target_path = Path(target)
        if not target_path.is_absolute():
            target_path = dst.parent / target_path
        relative_symlink(dst, target_path)
        return True
    except OSError:
        return False


@staticmethod
def _link_existing_shared_artifacts(root: Path, wd: Path) -> None:
    links = root / "shared" / "links"
    if not links.exists():
        return
    for link in links.iterdir():
        try:
            resolved = link.resolve()
        except OSError:
            continue
        try:
            link_shared_into_worker(root, wd, link.name, resolved.name)
        except OSError:
            continue


class RequiredPocUnavailableError(RuntimeError):
    code = "required_poc_unavailable"


def _link_inherited_pocs(self, root: Path, wd: Path) -> None:
    sg = getattr(self, "shared_graph", None)
    required = {
        str(item).strip() for item in (getattr(self, "required_pocs", None) or [])
        if str(item).strip()
    }
    if sg is None or not hasattr(sg, "pocs"):
        if required:
            raise RequiredPocUnavailableError("required PoC graph is unavailable")
        return
    if required:
        # A required PoC is an immutable CAS input. Give this Worker its own
        # copy instead of competing for the optional PoC claim lease.
        try:
            rows = {str(row.get("poc_id") or ""): row for row in sg.pocs()}
        except Exception as exc:
            raise RequiredPocUnavailableError(
                f"required PoC listing failed: {type(exc).__name__}: {exc}"
            ) from exc
        inherited: list[dict[str, str]] = []
        cas_root = (root / "shared" / "objects").resolve()
        for poc_id in sorted(required):
            if not re.fullmatch(r"poc-[0-9a-f]{12}", poc_id):
                raise RequiredPocUnavailableError(f"invalid required PoC ID: {poc_id}")
            row = rows.get(poc_id)
            if row is None or str(row.get("status") or "") not in {
                "available", "directional", "wip",
            }:
                raise RequiredPocUnavailableError(f"required PoC unavailable: {poc_id}")
            try:
                source = (root / str(row.get("path") or "")).resolve(strict=True)
                source.relative_to(cas_root)
                if not source.is_file():
                    raise ValueError("source is not a regular CAS file")
                expected = str(row.get("artifact_id") or "")
                if expected:
                    digest = hashlib.sha256()
                    with source.open("rb") as handle:
                        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                            digest.update(chunk)
                    if digest.hexdigest() != expected:
                        raise ValueError("CAS digest mismatch")
                name = Path(str(row.get("name") or source.name)).name
                target = wd / "inherited" / poc_id / name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, target)
                if expected:
                    copied_digest = hashlib.sha256()
                    with target.open("rb") as handle:
                        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                            copied_digest.update(chunk)
                    if copied_digest.hexdigest() != expected:
                        raise ValueError("materialized PoC digest mismatch")
                target.chmod(0o444)
            except (OSError, ValueError) as exc:
                raise RequiredPocUnavailableError(
                    f"required PoC materialization failed: {poc_id}: {exc}"
                ) from exc
            inherited.append({
                "poc_id": poc_id, "name": name,
                "entry_command": str(row.get("entry_command") or ""),
                "status": str(row.get("status") or ""),
                "note": str(row.get("note") or ""),
                "path": f"./inherited/{poc_id}/{name}",
            })
        self._inherited_pocs = inherited
        return
    try:
        rows = sg.pocs(inheritable_only=True)
    except Exception:
        return
    inherited: list[dict[str, str]] = []
    for p in rows:
        poc_id = str(p.get("poc_id") or "")
        if not poc_id:
            continue
        try:
            if hasattr(sg, "claim_poc") and not sg.claim_poc(worker=self.solver_id, poc_id=poc_id):
                continue
        except Exception:
            continue
        rel = str(p.get("path") or "")
        name = Path(str(p.get("name") or Path(rel).name)).name
        src = root / rel
        dst = wd / "inherited" / poc_id / name
        try:
            relative_symlink(dst, src)
        except OSError:
            continue
        self._claimed_pocs.add(poc_id)
        inherited.append({
            "poc_id": poc_id,
            "name": name,
            "entry_command": str(p.get("entry_command") or ""),
            "status": str(p.get("status") or ""),
            "note": str(p.get("note") or ""),
            "path": f"./inherited/{poc_id}/{name}",
        })
        if self.bus is not None:
            try:
                asyncio.get_running_loop().create_task(self._emit_bb(
                    "poc_claimed", poc_id=poc_id, worker=self.solver_id))
            except RuntimeError:
                pass
    self._inherited_pocs = inherited


def _workdir_path(self) -> "Optional[Path]":
    for raw in (getattr(self, "_current_workdir", None), getattr(self, "_workdir", None)):
        if not raw:
            continue
        try:
            return Path(raw).resolve()
        except OSError:
            continue
    return None


__all__ = [
    '_stable_worker_path',
    '_repo_blackboard_script',
    '_file_sha256',
    'sync_deployed_blackboard_skills',
    '_blackboard_script_path',
    '_spill_host_path',
    '_stage_attachments',
    '_ensure_shared_attachment',
    '_link_shared_attachment',
    '_link_existing_shared_artifacts',
    '_link_inherited_pocs',
    '_workdir_path',
]
