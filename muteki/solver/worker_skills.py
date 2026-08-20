"""Worker-scoped skill projection.

Muteki must never install its coordination skill into an operator's user-level
agent configuration.  Every CLI already supports project-local skills, so each
Worker gets a private projection under its own cwd.  User skills remain visible
through the engine's normal user-level discovery and disappear from the Muteki
projection when the Worker workspace is removed.
"""

from __future__ import annotations

import os
import shutil
from pathlib import Path


_REPO_SKILL = (
    Path(__file__).resolve().parents[2] / "skills" / "muteki-blackboard"
)

# `.agents/skills` is the common Agent Skills location used by Codex, Pi and
# recent compatible CLIs.  Engine-specific locations keep discovery deterministic
# for tools that do not scan the common directory.
_PROJECT_SKILL_ROOTS: dict[str, tuple[str, ...]] = {
    "claude": (".claude/skills", ".agents/skills"),
    "codex": (".agents/skills", ".codex/skills"),
    "cursor": (".cursor/skills", ".agents/skills"),
    "pi": (".pi/skills", ".agents/skills"),
    "omp": (".omp/skills", ".agents/skills"),
    "kimi": (".kimi/skills", ".agents/skills"),
    "grok": (".grok/skills", ".agents/skills"),
    "opencode": (".opencode/skills", ".agents/skills"),
    "dsh": (".agents/skills",),
}


def project_skill_roots(engine: str) -> tuple[str, ...]:
    return _PROJECT_SKILL_ROOTS.get(
        str(engine or "").strip().lower(), (".agents/skills",)
    )


def stage_blackboard_skill(
    workdir: str | Path,
    *,
    engine: str,
    container: bool = False,
) -> list[str]:
    """Expose ``muteki-blackboard`` only inside one Worker's cwd.

    Local Workers receive symlinks to the repository copy.  Container Workers
    receive links to the immutable image copy because the host repository path is
    not mounted inside the Worker container.  A pre-existing non-symlink path is
    preserved; Muteki never overwrites project content supplied by the operator.
    """

    root = Path(workdir).resolve()
    target = Path("/opt/muteki/muteki-blackboard") if container else _REPO_SKILL
    if not container and not target.is_dir():
        raise FileNotFoundError(f"muteki-blackboard skill source missing: {target}")

    staged: list[str] = []
    for relative in project_skill_roots(engine):
        skills_root = root / relative
        dest = skills_root / "muteki-blackboard"
        skills_root.mkdir(parents=True, exist_ok=True)
        try:
            if dest.is_symlink():
                if os.readlink(dest) == str(target):
                    staged.append(str(dest))
                    continue
                dest.unlink()
            elif dest.exists():
                # A Worker attachment/project may intentionally provide a skill
                # with this name.  Keep it intact and rely on the explicit script
                # path for the protocol implementation.
                staged.append(str(dest))
                continue
            dest.symlink_to(target, target_is_directory=True)
        except OSError:
            # Filesystems without symlink support get a private physical copy for
            # local execution.  The container path is not readable on the host, so
            # that case must fail loudly instead of copying an unrelated source.
            if container:
                raise
            shutil.copytree(target, dest)
        staged.append(str(dest))
    return staged


def legacy_user_skill_paths(home: str | Path | None = None) -> tuple[Path, ...]:
    """Known user-level locations used by older Muteki releases."""

    base = Path(home).expanduser() if home is not None else Path.home()
    return (
        base / ".claude/skills/muteki-blackboard",
        base / ".agents/skills/muteki-blackboard",
        base / ".codex/skills/muteki-blackboard",
        base / ".cursor/skills-cursor/muteki-blackboard",
        base / ".cursor/skills/muteki-blackboard",
        base / ".pi/agent/skills/muteki-blackboard",
        base / ".omp/agent/skills/muteki-blackboard",
        base / ".kimi-code/skills/muteki-blackboard",
        base / ".grok/skills/muteki-blackboard",
    )
