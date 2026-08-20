"""WorkerShellState — the f10 edge worker's local, persistent belief state.

Design: docs/frameworks_2026/10_edge_cognition_swarm.md §2.1.

One file per worker shell: ``<workspace>/.shell/state.json``.  The worker (the
CliSolver shell loop) is the ONLY writer of this file; the central arbiter never
fabricates worker cognition — it ingests checkpoint *events* the worker appends
to the event bus (see shell.py).  Crash-resume works because every turn ends
with an atomic save of this file.
"""

from __future__ import annotations

import hashlib
import json
import os
import time
from pathlib import Path
from typing import Any, Optional

SHELL_SCHEMA = "muteki.edge.shell.v1"

# ── §7 cost-neutral parameters ────────────────────────────────────────────────
TURN_LIMIT = 10            # hard per-worker turn cap; past it the shell self-kills
PLAN_EVERY = 3             # one strong re-plan every N turns, cheap steps between
STUCK_LIMIT = 3            # consecutive barren turns → stuck self-kill
TOKEN_LIMIT = 120000       # per-worker token ceiling
TOKEN_WARN_FRACTION = 0.8  # advisory warning injected into the next prompt
TOKEN_KILL_FRACTION = 0.95 # force-end past this fraction of the ceiling
META_EXPLORE_INTERVAL_S = 300.0  # central meta-explore cadence (§4.1/§6)
META_EXPLORE_TOKEN_BUDGET = 8000

# Guidance transport: SwarmF10 hands the worker its IntentEnvelope as a single
# marker line inside standing_guidance (the only per-worker channel the
# coordinator exposes to frameworks).  CliSolver parses it back out.
GUIDE_PREFIX = "[f10-shell-v1]"


def state_dir(workspace: Any) -> Path:
    return Path(workspace) / ".shell"


def state_path(workspace: Any) -> Path:
    return state_dir(workspace) / "state.json"


def goal_id_for(goal: str) -> str:
    return "sha256:" + hashlib.sha256(str(goal or "").encode("utf-8")).hexdigest()


def new_state(
    *,
    shell_id: str,
    intent_id: str,
    goal: str,
    predicted_effects: Optional[list] = None,
    success_criteria: Optional[list] = None,
    token_limit: int = TOKEN_LIMIT,
    turn_limit: int = TURN_LIMIT,
) -> dict[str, Any]:
    """Fresh WorkerShellState (design §2.1). Cognition fields start EMPTY — the
    worker fills them from real tool output, nobody ghost-writes them."""
    now = time.time()
    return {
        "schema": SHELL_SCHEMA,
        "shell_id": str(shell_id),
        "intent_id": str(intent_id),
        "goal_id": goal_id_for(goal),
        "goal": str(goal or ""),
        "predicted_effects": list(predicted_effects or []),
        "success_criteria": list(success_criteria or []),
        "budget": {
            "token_limit": int(token_limit),
            "turn_limit": int(turn_limit),
            "tokens_used": 0,
            "turns_used": 0,
        },
        "working_memory": {
            "confirmed_subfacts": [],
            "active_hypotheses": [],
            "dead_ends": [],
            "last_tool_output_digest": "",
        },
        "plan_queue": [],
        "last_checkpoint_seq": 0,
        "stuck_counter": 0,
        "created_at": now,
        "updated_at": now,
    }


def load_state(workspace: Any) -> Optional[dict[str, Any]]:
    """Load a persisted WorkerShellState for crash-resume. Returns None when the
    file is missing, corrupt, or from another schema — the caller starts fresh."""
    path = state_path(workspace)
    try:
        raw = path.read_text(encoding="utf-8")
        data = json.loads(raw)
    except Exception:
        return None
    if not isinstance(data, dict) or data.get("schema") != SHELL_SCHEMA:
        return None
    if not isinstance(data.get("budget"), dict):
        return None
    return data


def save_state(workspace: Any, state: dict[str, Any]) -> None:
    """Atomic checkpoint write (tmp file + os.replace) so a crash mid-write can
    never leave a torn state.json behind."""
    path = state_path(workspace)
    path.parent.mkdir(parents=True, exist_ok=True)
    state["updated_at"] = time.time()
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    tmp.write_text(
        json.dumps(state, ensure_ascii=False, indent=2, sort_keys=True),
        encoding="utf-8",
    )
    os.replace(tmp, path)


def render_envelope_guidance(envelope: dict[str, Any]) -> str:
    """Serialize an IntentEnvelope as one standing-guidance marker line."""
    return GUIDE_PREFIX + json.dumps(
        envelope, ensure_ascii=False, sort_keys=True, default=str
    )


def parse_envelope_guidance(line: Any) -> Optional[dict[str, Any]]:
    """Parse a guidance line back into an IntentEnvelope; None if not a marker."""
    if not isinstance(line, str) or not line.startswith(GUIDE_PREFIX):
        return None
    try:
        data = json.loads(line[len(GUIDE_PREFIX):].strip())
    except Exception:
        return None
    if not isinstance(data, dict) or not data.get("intent_id"):
        return None
    return data
