"""Dual-rush planning swarm: parallel divergent bootstraps + forced chain.

Planning-layer difference
-------------------------
ChainForce (F3b) deep-first awaits **one** long BOOT before any chain unit.
Measured r16-cf-v4: even hard fails that eventually spawn `cli-claude-2` are
**serial** (w2 starts only after w1 ends). Easy PASSes stay at 1 worker.

SwarmDualRush: seed **two concurrent bootstrap workers** with divergent
standing goals (same challenge, different attack angles). First flag wins;
after the dual-rush window, fall through to ChainForce forced-chain /
exploit-from-facts with ``parallel_max≤2``. This is a planning/dispatch
change, not an env timeout knob.
"""

from __future__ import annotations

from typing import Any

from muteki.solver.swarm_chainforce import SwarmChainForce


class SwarmDualRush(SwarmChainForce):
    """Drop-in Swarm: parallel dual bootstrap then forced chain."""

    architecture_name = "dualrush"
    # Slightly shorter than single deep-first so two boots fit the wall.
    dual_boot_wall_fraction = 0.42
    dual_boot_min_s = 280
    dual_boot_max_s = 560

    async def _planning_bootstrap(
        self,
        memory: Any,
        active: dict,
        per_solver: dict,
        started: float,
    ) -> None:
        if not self.initial_recon:
            return
        boot_timeout = int(
            min(
                self.dual_boot_max_s,
                max(
                    self.dual_boot_min_s,
                    float(self.wall_clock_budget) * self.dual_boot_wall_fraction,
                ),
            )
        )
        briefs = [
            (
                "BOOT-A",
                (
                    "BOOTSTRAP RUSH A (primary path): solve end-to-end if possible. "
                    "Inventory player files, follow the most direct decode/extract "
                    "path to flag{...} from real command output, and write every "
                    "verified observation to the blackboard."
                ),
            ),
            (
                "BOOT-B",
                (
                    "BOOTSTRAP RUSH B (divergent path): do NOT mirror worker A. "
                    "Prefer an alternate angle first (strings/binwalk/entropy/"
                    "crypto primitives/network artifacts as category suggests). "
                    "Pursue a second hypothesis toward flag{...}; record distinct "
                    "facts. Stop cleanly if stuck."
                ),
            ),
        ]
        for unit_id, goal in briefs:
            if len(active) >= int(self.max_workers):
                break
            item = {
                "id": unit_id,
                "goal": goal,
                "mode": "bootstrap",
                "timeout": boot_timeout,
            }
            spawned = await self._spawn_planned_worker(item, memory=memory)
            if spawned is None:
                continue
            task, worker = spawned
            active[task] = worker
            sid = str(getattr(worker, "solver_id", "") or "")
            await self._emit_coord_bb(
                "worker_spawned",
                architecture=self.architecture_name,
                worker=sid,
                solver_id=sid,
                unit_id=unit_id,
                mode="bootstrap",
                active=len(active),
                max_workers=int(self.max_workers),
            )

        # Do NOT await / cancel the dual bootstrap window. Leave both workers
        # live and return to the plan loop so chain units can overlap in time
        # (the previous cancel-after-window forced serial phases and wasted
        # mid-solve context).
        await self._wait_any(active, timeout=8.0)
        await self._reap_finished(active, per_solver, memory, started)
        self._sync_flags_from_graph()
        if self._found_flags:
            self._record_flags(*list(self._found_flags))
        self._promote_flags_from_texts(
            list(memory.facts) + list(memory.receipts),
            actor="dualrush-bootstrap",
            memory=memory,
        )
