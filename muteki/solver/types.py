"""Shared solver value types (config + outcome).

Extracted from the (now-removed) code-driven solver so the CLI executor and the
swarm can depend on these dataclasses without importing the old Solver class.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Optional

from muteki.models.solve_graph import SolveGraph


@dataclass
class SolverConfig:
    max_steps: int = 12
    code_timeout: float = 90.0
    temperature: float = 0.4
    max_tokens: int = 8000
    stdout_limit: int = 6000  # chars of stdout shown inline before overflow->artifact
    # P-C: auto-trigger the Reason phase (planner + evidence audit) when the
    # shared graph's fact/dead-end count grows. Off when there's no shared graph
    # (solo solver) — it only adds value with a shared, gated graph to plan over.
    reason_enabled: bool = True
    reason_model: str = ""  # empty → reuse the solver's own model (a cheap one is fine)
    reason_max_intents: int = 4
    # Watchdog: if this many consecutive reason cycles produce NO new verified fact,
    # the run is spinning — force the conclude turn early instead of burning the
    # whole step budget. 0 disables the watchdog. (Convergence guard: stop
    # re-planning a stuck run; don't wait for max_steps.)
    stale_reason_limit: int = 3


@dataclass
class SolveOutcome:
    solved: bool
    flag: Optional[str]
    steps: int
    graph: SolveGraph
    reason: str = ""
    # multi-flag: every distinct flag this worker accepted this run (dedup, in
    # discovery order). `flag` stays as the FIRST one for back-compat reads.
    # Single-flag challenges have len(flags) <= 1, so `flag` and `flags[0]` agree.
    flags: list[str] = field(default_factory=list)
    # CLI continuation handle (CliSolver only): the winning worker's shelled-CLI
    # session id + engine + cwd, so a post-solve standby driver can resume the
    # SAME session (`claude -r <session>`) to answer a human follow-up, mark a
    # false-positive and keep solving, or write a writeup — with the worker's full
    # memory intact. None for non-CLI paths or when no session was assigned.
    session: Optional[str] = None
    engine: str = ""
    workdir: str = ""
    # respond mode (standby) only: the worker's conversational reply / writeup body,
    # so the standby driver can persist it (e.g. writeup.md) without re-parsing the
    # event stream. Empty for solve runs.
    reply: str = ""
