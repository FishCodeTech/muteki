"""Canonical filesystem layout for Web runs and coordinator-owned state."""

from __future__ import annotations

from dataclasses import dataclass
import os
from pathlib import Path

from muteki.core.path_ids import encode_run_id


def _absolute(path: str | Path) -> Path:
    return Path(os.path.abspath(Path(path).expanduser()))


def _default_state_root(sessions_root: Path) -> Path:
    if sessions_root.name == "sessions":
        return sessions_root.parent / "state"
    return sessions_root.parent / f"{sessions_root.name}-state"


def _contains(path: Path, root: Path) -> bool:
    try:
        path.resolve().relative_to(root.resolve())
        return True
    except ValueError:
        return False


@dataclass(frozen=True)
class StorageLayout:
    """One Run workspace tree plus a separate coordinator state tree."""

    sessions_root: Path
    state_root: Path
    events_root: Path
    control_root: Path
    graph_root: Path
    runtime_root: Path

    @classmethod
    def resolve(
        cls,
        *,
        sessions_root: str | Path | None = None,
        state_root: str | Path | None = None,
        control_root: str | Path | None = None,
    ) -> "StorageLayout":
        sessions = _absolute(
            sessions_root or os.environ.get("MUTEKI_SESSIONS_ROOT") or "sessions"
        )
        state = _absolute(
            state_root
            or os.environ.get("MUTEKI_STATE_ROOT")
            or _default_state_root(sessions)
        )
        control = _absolute(
            control_root
            or os.environ.get("MUTEKI_COORDINATOR_CONTROL_ROOT")
            or state / "control"
        )
        graph = _absolute(
            os.environ.get("MUTEKI_COORDINATOR_GRAPH_ROOT")
            or control / "graphs"
        )
        layout = cls(
            sessions_root=sessions,
            state_root=state,
            events_root=state,
            control_root=control,
            graph_root=graph,
            runtime_root=state / "runtime",
        )
        layout.validate()
        environment_root = os.environ.get("MUTEKI_ENVIRONMENT_ROOT")
        if environment_root:
            root = _absolute(environment_root)
            for name, directory in (("state", state), ("sessions", sessions), ("control", control), ("graph", graph)):
                if directory == root or not _contains(directory, root):
                    raise ValueError(f"{name} must be inside the selected desktop environment")
        return layout

    def validate(self) -> None:
        if _contains(self.state_root, self.sessions_root) or _contains(
            self.sessions_root, self.state_root
        ):
            raise ValueError("state root and sessions root must be separate trees")
        for name, path in (
            ("control root", self.control_root),
            ("graph root", self.graph_root),
            ("runtime root", self.runtime_root),
        ):
            if _contains(path, self.sessions_root):
                raise ValueError(
                    f"{name} cannot be inside the worker-visible sessions tree"
                )

    def prepare(self) -> None:
        for path in (
            self.sessions_root,
            self.state_root,
            self.events_root,
            self.control_root,
            self.graph_root,
            self.runtime_root,
        ):
            path.mkdir(mode=0o700, parents=True, exist_ok=True)
        for path in (self.state_root, self.events_root, self.control_root,
                     self.graph_root, self.runtime_root):
            try:
                os.chmod(path, 0o700)
            except OSError:
                pass
        secret_root = self.state_root / "_secrets"
        if secret_root.exists():
            try:
                os.chmod(secret_root, 0o700)
            except OSError:
                pass

    def run_root(self, run_id: str) -> Path:
        return self.sessions_root / encode_run_id(run_id)

    def workspace(self, run_id: str) -> Path:
        return self.run_root(run_id) / "workspace"

    def shared_worker_mount(self) -> Path:
        """Only explicitly shared Run workspaces live below this mount."""
        return self.sessions_root / ".muteki-shared-worker-mount"

    def shared_workspace(self, run_id: str) -> Path:
        return self.shared_worker_mount() / encode_run_id(run_id)

    def uploads(self, run_id: str) -> Path:
        return self.run_root(run_id) / "uploads"

    def event_log(self, run_id: str) -> Path:
        return self.events_root / f"{encode_run_id(run_id)}.jsonl"

    def run_control(self, run_id: str) -> Path:
        return self.control_root / encode_run_id(run_id)

    def run_graph(self, run_id: str) -> Path:
        return self.graph_root / encode_run_id(run_id)

    def account_projection(self, owner_id: str) -> Path:
        return self.runtime_root / "account-projections" / encode_run_id(owner_id)


__all__ = ["StorageLayout"]
