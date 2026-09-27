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
import subprocess
from dataclasses import dataclass
from pathlib import Path

from muteki.solver.cli_prompts import (
    CTF_ROLE_CONTRACT,
    _CTF_FACT_ASSERTION_NOTE,
    _CTF_WORKER_SYSTEM,
)


_REPO_SKILL = (
    Path(__file__).resolve().parents[2] / "skills" / "muteki-blackboard"
)

_OPERATOR_INPUT_SKILL_BLOCK = (
    "Request operator input only for an external resource or environment problem:\n\n"
    "```bash\n"
    "python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" request-input '<specific required input>'\n"
    "```\n\n"
)


def _autonomous_skill_projection(root: Path) -> Path:
    source = (_REPO_SKILL / "SKILL.md").read_text(encoding="utf-8")
    content = source.replace("operator requests, ", "").replace(
        _OPERATOR_INPUT_SKILL_BLOCK, ""
    )
    target = root / ".muteki-skills" / "muteki-blackboard"
    target.mkdir(parents=True, exist_ok=True)
    (target / "SKILL.md").write_text(content, encoding="utf-8")
    return target

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
    allow_operator_input: bool = True,
) -> list[str]:
    """Expose ``muteki-blackboard`` only inside one Worker's cwd.

    Local Workers receive symlinks to the repository copy.  Container Workers
    receive links to the immutable image copy because the host repository path is
    not mounted inside the Worker container.  Autonomous Workers receive a private
    documentation projection without the operator-input command.  A pre-existing
    non-symlink path is preserved; Muteki never overwrites project content supplied
    by the operator.
    """

    from muteki.capability_management import enabled as capability_enabled

    if not capability_enabled("skills", "muteki-blackboard"):
        raise RuntimeError("required muteki-blackboard Skill is disabled")

    root = Path(workdir).resolve()
    target = (
        Path("/opt/muteki/muteki-blackboard") if container else _REPO_SKILL
    )
    relative_target = False
    if not allow_operator_input:
        target = _autonomous_skill_projection(root)
        relative_target = True
    if not container and not target.is_dir():
        raise FileNotFoundError(f"muteki-blackboard skill source missing: {target}")

    staged: list[str] = []
    for relative in project_skill_roots(engine):
        skills_root = root / relative
        dest = skills_root / "muteki-blackboard"
        skills_root.mkdir(parents=True, exist_ok=True)
        link_target = (
            os.path.relpath(target, start=dest.parent)
            if relative_target else str(target)
        )
        try:
            if dest.is_symlink():
                if os.readlink(dest) == link_target:
                    staged.append(str(dest))
                    continue
                dest.unlink()
            elif dest.exists():
                # A Worker attachment/project may intentionally provide a skill
                # with this name.  Keep it intact and rely on the explicit script
                # path for the protocol implementation.
                staged.append(str(dest))
                continue
            dest.symlink_to(link_target, target_is_directory=True)
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


ROLE_CONTRACT_MARKER = "<!-- muteki-role-contract -->"


@dataclass(frozen=True)
class RoleContractStage:
    engine: str
    channel: str
    filename: str = ""
    path: str = ""
    reason: str = ""
    body: str = ""

    @property
    def folded_text(self) -> str:
        if self.channel != "user_fold":
            return ""
        return self.body


def role_contract_filename(engine: str) -> str:
    """Select the instruction filename by engine name, not driver class."""
    if str(engine or "").strip().lower() == "claude":
        return "CLAUDE.md"
    return "AGENTS.md"


def role_contract_body() -> str:
    return f"{CTF_ROLE_CONTRACT}\n\n{_CTF_FACT_ASSERTION_NOTE}"


def role_contract_document() -> str:
    return f"{ROLE_CONTRACT_MARKER}\n{role_contract_body()}\n"


def instruction_file_gitignored(workdir: str | Path, filename: str) -> bool:
    """True when git would hide this instruction file from an engine like Grok."""
    if not workdir or not filename:
        return False
    try:
        completed = subprocess.run(
            ["git", "check-ignore", "-q", "--", filename],
            cwd=str(Path(workdir)),
            check=False,
            capture_output=True,
        )
    except OSError:
        return False
    return completed.returncode == 0


def _noop_stage(engine: str, reason: str = "") -> RoleContractStage:
    return RoleContractStage(engine=str(engine or ""), channel="noop", reason=reason)


def _fold_stage(engine: str, filename: str, reason: str) -> RoleContractStage:
    return RoleContractStage(
        engine=str(engine or ""),
        channel="user_fold",
        filename=filename,
        reason=reason,
        body=role_contract_body(),
    )


def stage_role_contract(
    workdir: str | Path,
    *,
    engine: str,
    challenge_mode: str = "ctf",
    worker_mode: str = "explore",
    bare: bool = False,
    gitignore_blocked: bool = False,
) -> RoleContractStage:
    """Deliver the frozen CTF worker contract through a per-engine file channel.

    Pi is a hard no-op: its contract stays on ``MUTEKI_PI_SYSTEM_PROMPT``.
    OMP binds the same bytes through ``MUTEKI_OMP_SYSTEM_PROMPT`` and also
    skips files. Collision, ``--bare``, or a gitignored Grok instruction file
    fold the contract into the explore USER prompt instead of reporting success.
    """
    name = str(engine or "").strip().lower()
    if name == "pi":
        return _noop_stage(name)
    if str(challenge_mode or "ctf") != "ctf" or str(worker_mode or "") != "explore":
        return _noop_stage(name, "out_of_scope")
    if name == "omp":
        return _noop_stage(name, "omp_system")

    filename = role_contract_filename(name)
    if bare:
        return _fold_stage(name, filename, "bare")
    if gitignore_blocked:
        return _fold_stage(name, filename, "gitignore")
    if not workdir:
        return _fold_stage(name, filename, "missing_cwd")

    root = Path(workdir).expanduser().resolve()
    try:
        if root == Path.home().resolve():
            return _fold_stage(name, filename, "home")
    except OSError:
        pass

    dest = root / filename
    if dest.exists() or dest.is_symlink():
        if dest.is_symlink():
            return _fold_stage(name, filename, "symlink")
        try:
            existing = dest.read_text(encoding="utf-8")
        except OSError:
            return _fold_stage(name, filename, "collision")
        if ROLE_CONTRACT_MARKER not in existing:
            return _fold_stage(name, filename, "collision")

    document = role_contract_document()
    dest.write_text(document, encoding="utf-8")
    return RoleContractStage(
        engine=name,
        channel="file",
        filename=filename,
        path=str(dest),
        body=document,
    )


# Keep the frozen system bytes imported so accidental edits fail the identity check
# in tests (CTF_ROLE_CONTRACT is _CTF_WORKER_SYSTEM).
assert CTF_ROLE_CONTRACT is _CTF_WORKER_SYSTEM
