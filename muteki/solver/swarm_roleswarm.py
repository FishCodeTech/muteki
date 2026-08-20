"""Role-specialized planning swarm (recon / exploit / verify in parallel).

Planning-layer difference
-------------------------
DualRush: two divergent long bootstraps, then forced-chain.
ChainForce: serial deep-first BOOT then fact→chain.
Phased: sequential RECON→ANALYZE→EXPLOIT→VERIFY phases.

SwarmRoleSwarm: keep **typed role seats** concurrent — at most one RECON,
one EXPLOIT, one VERIFY worker when slots allow. Roles are planning
artifacts (standing goals + dispatch rules), not env interrupt knobs.
Verified facts from RECON unlock EXPLOIT; candidate flags unlock VERIFY.
This is F8 (blackboard role swarm), structurally distinct from F3b/F3c.
"""

from __future__ import annotations

from typing import Any

from muteki.solver.planning_kernel_v1 import WorkingMemory
from muteki.solver.swarm_planning_base import PlanningSwarmBase


_ROLE_GOALS = {
    "recon": (
        "ROLE=RECON: inventory player files and extract durable facts "
        "(hashes, encodings, offsets, key material hints) from real command "
        "output. Do not invent flags. Write facts to the blackboard."
    ),
    "exploit": (
        "ROLE=EXPLOIT: using verified blackboard facts only, perform the "
        "decisive decode/decrypt/extract that yields flag{...} from real "
        "command output. Do not restart broad recon."
    ),
    "verify": (
        "ROLE=VERIFY: confirm any candidate flag{{...}} against challenge "
        "artifacts / checkers available in the player tree. Record acceptance "
        "or concrete rejection reasons as facts."
    ),
}


class SwarmRoleSwarm(PlanningSwarmBase):
    """Drop-in Swarm: concurrent recon/exploit/verify role seats."""

    architecture_name = "roleswarm"
    executor_timeout_s = 420
    initial_recon = True
    max_plan_rounds = 20

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._role_seq = 0
        self._active_roles: set[str] = set()

    async def plan_round(
        self,
        *,
        memory: WorkingMemory,
        round_index: int,
        free_slots: int,
        wall_remaining: float,
        active_count: int,
        force_seed: bool = False,
    ) -> list[dict[str, Any]]:
        del force_seed
        if free_slots <= 0:
            return []
        timeout = int(
            min(self.executor_timeout_s, max(90.0, wall_remaining - 25.0))
        )
        wanted: list[str] = []
        # Always prefer a recon seat early or when facts are thin.
        if "recon" not in self._active_roles and (
            len(memory.facts) < 4 or round_index < 2
        ):
            wanted.append("recon")
        if (
            "exploit" not in self._active_roles
            and len(memory.facts) >= 2
            and not memory.flags
        ):
            wanted.append("exploit")
        if "verify" not in self._active_roles and memory.flags:
            wanted.append("verify")
        # If nothing typed matches, keep exploring via exploit-shaped seat.
        if not wanted and "exploit" not in self._active_roles:
            wanted.append("exploit" if memory.facts else "recon")

        out: list[dict[str, Any]] = []
        for role in wanted:
            if len(out) >= min(free_slots, 3):
                break
            if role in self._active_roles:
                continue
            self._role_seq += 1
            unit_id = f"{role[0].upper()}{self._role_seq}"
            goal = _ROLE_GOALS[role]
            if role == "exploit" and memory.facts:
                blob = "; ".join(memory.facts[-6:])[:700]
                goal = f"{goal} Facts: {blob}"
            self._active_roles.add(role)
            out.append(
                {
                    "id": unit_id,
                    "goal": goal,
                    "mode": "explore",
                    "timeout": timeout,
                    "meta": {"role": role},
                }
            )
        return out

    async def on_unit_finished(
        self,
        *,
        memory: WorkingMemory,
        unit: dict[str, Any],
        outcome: Any,
        new_facts: list[str],
        found_flag: str,
    ) -> None:
        del memory, outcome, new_facts, found_flag
        role = str((unit.get("meta") or {}).get("role") or "")
        if role:
            self._active_roles.discard(role)
