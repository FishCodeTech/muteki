"""Linux ownership primitives used by the standalone process guardian.

T3 ACP uses a delegated cgroup when available and otherwise an observed
pid/birth ledger. The latter cannot capture an unobserved double fork.
This module has only standard-library dependencies.
"""
from __future__ import annotations

from dataclasses import dataclass
import os
from pathlib import Path, PurePosixPath
import signal
import uuid


@dataclass(frozen=True)
class Identity:
    pid: int
    parent: int
    birth: str
    uid: int
    state: str


def identities() -> dict[int, Identity]:
    result = {}
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            stat = (path / "stat").read_text()
            fields = stat[stat.rindex(")") + 2:].split()
            pid = int(path.name)
            result[pid] = Identity(pid, int(fields[1]), fields[19], path.stat().st_uid, fields[0])
        except (FileNotFoundError, ProcessLookupError):
            continue
    return result


class DescendantLedger:
    def __init__(self, root: int) -> None:
        self.root = root
        self.known: dict[int, Identity] = {}
        self.observe()

    def observe(self) -> dict[int, Identity]:
        snapshot = identities()
        root = snapshot.get(self.root)
        if not self.known and root is not None:
            self.known[self.root] = root
        live = {pid: current for pid, old in self.known.items()
                if (current := snapshot.get(pid)) is not None and current.birth == old.birth and current.uid == old.uid}
        # Discover by a currently verified parent, never by a recycled pid.
        while True:
            children = {pid: item for pid, item in snapshot.items()
                        if pid not in live and item.parent in live and item.uid == os.getuid()}
            if not children:
                break
            live.update(children)
            self.known.update(children)
        return {pid: item for pid, item in live.items() if pid != self.root and item.state != "Z"}

    def signal(self, sig: int) -> None:
        for pid, item in self.observe().items():
            # As in T3's reduced-guarantee POSIX path, recheck birth directly
            # before signaling. This is not a kernel-atomic pidfd guarantee.
            current = identities().get(pid)
            if pid > 1 and current is not None and current.birth == item.birth and current.uid == item.uid:
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    continue


def _unified_path(body: str) -> str | None:
    return next((line[3:] for line in body.splitlines() if line.startswith("0::")), None)


def _safe(path: str) -> bool:
    return path.startswith("/") and ".." not in PurePosixPath(path).parts


def _mount_path(path: str) -> str:
    for encoded, decoded in (("\\040", " "), ("\\011", "\t"), ("\\012", "\n"), ("\\134", "\\")):
        path = path.replace(encoded, decoded)
    return path


@dataclass(frozen=True)
class CgroupLease:
    path: Path

    @classmethod
    def create(cls) -> CgroupLease | None:
        """Only use an already delegated writable cgroup; never change ACLs."""
        try:
            relative = _unified_path(Path("/proc/self/cgroup").read_text())
            if relative is None or not _safe(relative):
                return None
            for line in Path("/proc/self/mountinfo").read_text().splitlines():
                left, separator, right = line.partition(" - ")
                if not separator or not right.startswith("cgroup2 "):
                    continue
                fields = left.split()
                root, mount = _mount_path(fields[3]), _mount_path(fields[4])
                if not _safe(root) or not _safe(mount):
                    continue
                if root != "/" and relative != root and not relative.startswith(root + "/"):
                    continue
                suffix = relative if root == "/" else relative[len(root):]
                parent = Path(mount) / suffix.lstrip("/")
                child = parent / f"muteki-acp-{os.getpid()}-{uuid.uuid4().hex}"
                try:
                    child.mkdir(mode=0o700)
                    if (child / "cgroup.type").read_text().strip() != "domain":
                        raise OSError("process.cgroup.not_domain")
                    if not all(os.access(child / name, mode) for name, mode in (
                            ("cgroup.procs", os.W_OK), ("cgroup.kill", os.W_OK), ("cgroup.events", os.R_OK))):
                        raise OSError("process.cgroup.not_delegated")
                    return cls(child)
                except OSError:
                    if child.exists():
                        child.rmdir()  # It has never contained an engine.
        except OSError:
            return None
        return None

    def join(self) -> None:
        (self.path / "cgroup.procs").write_text(str(os.getpid()) + "\n")

    def populated(self) -> bool:
        values = dict(line.split() for line in (self.path / "cgroup.events").read_text().splitlines())
        if values.get("populated") not in {"0", "1"}:
            raise OSError("process.cgroup.events_invalid")
        return values["populated"] == "1"

    def kill(self) -> None:
        (self.path / "cgroup.kill").write_text("1\n")

    def remove(self) -> None:
        def remove_children(path: Path) -> None:
            for child in path.iterdir():
                if child.is_dir():
                    remove_children(child)
                    child.rmdir()
        remove_children(self.path)
        self.path.rmdir()
