"""CliSolver — a swarm worker whose EXECUTOR is a shelled CLI agent (claude/codex)
instead of the local code-driven kernel loop.

It is a drop-in for `Solver` in the swarm: same construction surface, same
`run() -> SolveOutcome`, same `solver_id` / `graph`, and it emits the same event
stream (RUN_STARTED → reasoning/insight → fact_added/flag_found → RUN_FINISHED) so
the deck, shared_graph, and blackboard telemetry keep working unchanged.

What's different: instead of driving an LLM through run_python tool-calls, it hands
the challenge to a CLI agent (full shell, its own agentic loop), then runs the SAME
provenance gate (`muteki.solver.gate.flag_ok`) on the agent's real output. The
moat is preserved — a flag still only counts if it traces to actual output.

Bare host (no isolation — user deferred P-D0). The CLI runs in a per-solver scratch
workdir; for a service challenge the agent only needs the target URL.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
import shutil
import signal
import stat
import tempfile
import threading
import time
import uuid
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Mapping, Optional
from urllib.parse import unquote, urlsplit

from muteki.control.models import stable_decision_request_id
from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.events import (
    Event, EventType, blackboard_delta_payload, hitl_request_payload,
    insight_payload, shared_graph_delta_payload, solve_graph_delta_payload,
    tool_result_payload, worker_status_payload, worker_lifecycle_payload,
)
from muteki.models.solve_graph import Challenge, SolveGraph, engagement_goal_of
from muteki.solver.cli_driver import (
    CliDriver, CliResult, KB_MCP_NAME, SecurePromptUnsupported, StreamStep,
    apply_runtime_argv, driver_for, run_cli, run_cli_streaming,
)
from muteki.solver.gate import (
    flag_ok as _gate_flag_ok,
    finding_ok as _gate_finding_ok,
    finding_key as _finding_key,
    is_placeholder_flag,
    parse_finding_claim,
)
from muteki.solver.vuln_report import (
    VERIFIER_PROMPT as _VERIFIER_PROMPT,
    _PENTEST_REPORT_BLOCK,
    completeness_code,
    missing_report_fields,
    parse_report_files,
    report_id_from_intent,
    report_sse_fields,
    replay_attempted,
    witness_in_corpus,
    VALUE_REJECT_DUPLICATE,
    VALUE_REJECT_INCOMPLETE,
    VALUE_REJECT_NOT_REPRODUCIBLE,
)
from muteki.solver.result import ArtifactStore
from muteki.solver.result_codes import (
    RESULT_CANCELLED,
    RESULT_DEAD_END,
    RESULT_EXPLORED,
    RESULT_OOM,
    RESULT_SOLVED,
    RESULT_STEERED,
    RESULT_TIMED_OUT,
)
from muteki.solver.types import SolverConfig, SolveOutcome
from muteki.solver.worker_profiles import profile_uses_endpoint, worker_identity_from_env
from muteki.solver.workspace import (
    ensure_workspace,
    link_input_into_worker,
    link_shared_into_worker,
    materialize_input,
    materialize_shared_artifact,
    relative_symlink,
    workspace_root_for_worker,
)


_WORKER_HEARTBEAT_SECONDS = 15.0
_IDLE_REPEAT_LIMIT = 5
_IDLE_REPEAT_STEER = (
    "你在重复读取黑板且内容无变化，停止重复读取，按当前 intent 目标执行实际操作"
)


def _is_reasoning_replay(accumulated: str, incoming: str) -> bool:
    """True when `incoming` restates the whole turn already streamed.

    Pi/OMP emit token deltas, then `message_end` repeats the full assistant
    text. A replay is that full snapshot: equal to the turn, or a longer
    snapshot that already contains it as a prefix. A short suffix is a
    normal incremental token — incremental engines often emit one letter
    that already ends the buffer. Thinking deltas never enter the
    accumulator (StreamStep.thinking), so the answer-only snapshot always
    matches here instead of arriving as an unsealable suffix.
    """
    acc = (accumulated or "").strip()
    text = (incoming or "").strip()
    if not acc or not text:
        return False
    if text == acc:
        return True
    return len(text) >= len(acc) and text.startswith(acc)


_WORKER_PATH_PREFIX = (
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    # macOS Wireshark.app — host tshark/capinfos often live here, not in PATH.
    "/Applications/Wireshark.app/Contents/MacOS",
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


# The muteki-blackboard skill ships in the repo at <repo>/skills/muteki-blackboard/.
# cli_solver.py lives at <repo>/muteki/solver/, so the repo root is two parents up.
_REPO_BLACKBOARD_SCRIPT = (
    Path(__file__).resolve().parent.parent.parent
    / "skills" / "muteki-blackboard" / "blackboard.py"
)


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


# A focused, black-box prompt. No domain how-to — just "become an expert";
# only the contract: probe the real target, read output fully, prove the flag.
_EXEC_PROMPT = (
    "You are an expert CTF solver working a BLACK-BOX challenge with a FULL shell "
    "(curl, python3, nc, jq, common tooling). Become an expert in whatever this "
    "challenge's domain is.\n\n"
    "{ctx}\n"
    "{kb}\n"
    "Actually RUN commands and probe the real target/files — do not just plan. Read "
    "every response/output end-to-end (follow redirects, keep session/cookies across "
    "requests, inspect full bodies, not just the first lines). Drive ONE promising "
    "direction to a conclusion before switching.\n\n"
    "## Share findings with your team (a swarm is solving this with you)\n"
    "As you confirm things, print them on their own lines so teammates benefit and "
    "nobody repeats your work:\n"
    "  VERIFIED_FACT=<a confirmed, objective finding from REAL output>\n"
    "  DEADEND=<a direction you ruled out — so nobody retries it>\n"
    "You may print several of each, as you go.\n"
    "Print VERIFIED_FACT only for NEW findings not already on the team board. Do NOT "
    "re-state facts, passwords, or unlocks a teammate already recorded — reuse them. "
    "Put long data blobs in a file and reference the path in the fact, not the blob. "
    "(For the freshest teammate state before a long re-derivation, check the live board "
    "via the blackboard skill, not just the snapshot file.)\n\n"
    "## If you are BLOCKED on something only the operator can give you\n"
    "Some blockers are NOT solvable by trying harder or a different angle — you are "
    "missing an external RESOURCE (a public VPS / reverse-shell host, a credential, "
    "a tool) or the challenge ENVIRONMENT is objectively unusable (the target is "
    "unreachable / the instance looks expired). When that is the case, do NOT keep "
    "blindly retrying or scanning — state exactly what you need on its own line:\n"
    "  NEED_INPUT=<the ONE specific thing the operator must supply, concretely>\n"
    "  NEED_KIND=<external_blocker|lane_lock_request|route_dead_end|worker_uncertainty|operator_directive_needed>\n"
    "e.g. NEED_INPUT=a public VPS I can SSH to for a reverse-shell catcher (I'm behind NAT), "
    "or NEED_INPUT=the target is returning connection-refused, the instance may have expired. "
    "Print NEED_INPUT only for a genuine external blocker, not for a hard puzzle.\n\n"
    "The flag is shaped like {fmt}. When you recover it FROM REAL output (not a "
    "guess), submit it through the team Blackboard API exactly as:\n"
    "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<the flag>'\n"
    "The flag value must first appear verbatim in real command output or a real "
    "artifact. Ordinary reply text and FOUND_FLAG= do not submit a result."
)

_PROTOCOL2_CANARY_PROMPT = (
    "You are executing a neutral local runtime conformance check, not a security "
    "challenge. Work only in the current worker directory and follow the fixture "
    "description below. Run only the minimal local shell command needed to read the "
    "explicitly staged fixture file. Do not enumerate parent or unrelated paths, do "
    "not contact a target service, and do not use web/search tools.\n\n"
    "{ctx}\n\n"
    "When the requested token has appeared in real local command output, submit it "
    "once with `python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<token>'`. "
    "The accepted shape is {fmt}. Never guess or manufacture a token."
)

# Pentest mode (BE-pentest-mode, Origin/Goal/Hints framing): the SAME swarm,
# but the objective is operator-defined (find + prove vulnerabilities in scope)
# instead of a flag. No new pipeline — recon/audit/exploit all emerge from the
# Goal text + the agent's general skill. Findings are reported with the EXISTING
# VERIFIED_FACT= marker so they pass the SAME witness gate (a claim is only a
# finding once it's backed by real output — the provenance moat is unchanged).
_PENTEST_EXEC_PROMPT = (
    "You are an expert penetration tester with a FULL shell "
    "(curl, python3, and common offensive tooling).\n\n"
    "{ctx}\n"
    "{kb}\n"
    "## Engagement goal\n{goal}\n\n"
    "## Scope / authorization — operate STRICTLY within this\n{scope}\n\n"
    "Work the goal against the live in-scope HTTP origin. Send real requests. "
    "If the target exposes login, try common username field names "
    "(user, username, email) with the credentials you actually have; do not "
    "invent extra accounts. Save request/response bodies to files and cite them.\n\n"
    "Do NOT read parent directories, the muteki source tree, labs/, or any "
    "server.py on disk. The origin in Scope is the only system you test.\n\n"
    f"{_PENTEST_REPORT_BLOCK}"
    "Also print team markers:\n"
    "  VERIFIED_FACT=<confirmed fact + pointer to the request/response evidence>\n"
    "  DEADEND=<a direction you ruled out — so nobody retries it>\n"
    "Print VERIFIED_FACT only for NEW facts not already on the team board.\n\n"
    "## If you are BLOCKED on something only the operator can give you\n"
    "When you are missing an external RESOURCE (a public VPS, a credential, a tool) "
    "or the target is objectively unusable (unreachable / out of scope / expired), "
    "do NOT keep blindly retrying — state exactly what you need on its own line:\n"
    "  NEED_INPUT=<the ONE specific thing the operator must supply, concretely>\n"
    "  NEED_KIND=<external_blocker|lane_lock_request|route_dead_end|worker_uncertainty|operator_directive_needed>\n"
)

# Injected only when a KB MCP is configured AND mounted. Teaches the agent to
# DISPATCH to the knowledge base as a first-class tool at the right moments — not to
# dump it into context. The KB itself is whatever MCP the operator points
# MUTEKI_KB_MCP_NAME at (e.g. a security-intel / CVE / writeup index); there is no
# bundled service, so this prompt is empty out of the box.
_KB_PROMPT = (
    f"\nYou ALSO have a `{KB_MCP_NAME}` knowledge-base tool (a searchable security "
    "knowledge base — e.g. tools, CVEs/PoCs, repos, payload helpers). "
    "Use it like an expert teammate — call it when, and only when, it shortcuts the "
    "solve:\n"
    "  • a service/version/tech fingerprint → search the KB for known CVEs + PoCs;\n"
    "  • need a specific tool/technique → look it up instead of reinventing;\n"
    "  • need an obfuscated/WAF-bypass payload → look for a generator/helper.\n"
    "Don't browse the KB aimlessly or paste large dumps; query it with a sharp term, "
    "take the one useful hit, and get back to running commands on the real target.\n"
) if KB_MCP_NAME else ""

_RESUME_PROMPT = (
    "CONCLUDE: stop exploring now. If you already saw a correctly-formatted flag in "
    "REAL output this session, submit it through `python3 "
    "\"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<flag>'`. If not, report the "
    "furthest confirmed fact. Do not guess."
)

# P3: a resume turn that KEEPS WORKING (not conclude), used when teammates reported
# new facts/dead-ends during the last turn. The worker continues from where it was,
# building on the freshly-folded teammate knowledge appended after this line.
# (removed in P-3: _CONTINUE_PROMPT was the multi-turn "fold teammate findings into
#  a resume turn" prompt — single-shot has no resume turn, so it had zero callers.)

# (removed in the single-shot cleanup: _STEER_RESUME_PROMPT + _build_steer_prompt
#  were the "operator steered → resume the SAME session with folded guidance" path.
#  There is no resume turn under single-shot — steering ends the current pass and the
#  guidance flows to the NEXT spawned worker. They had zero production callers.)

# ── Explore mode (one intent at a time) ─────────────────────────────────────
# Instead of "solve the whole challenge", Explore claims a single intent from
# the blackboard, explores that ONE direction, and reports a structured Fact.
# This prevents the worker from drilling into a dead end for 40 minutes — each
# Explore is short and scoped; Reason re-evaluates the board between them.
_EXPLORE_PROMPT = (
    "You are an expert CTF solver with a FULL shell. Become an expert in whatever "
    "this challenge's domain is.\n\n"
    "{ctx}\n"
    "{kb}\n"
    "## Your assigned direction\n"
    "You have been assigned ONE specific exploration direction:\n"
    "  {intent_goal}\n\n"
    "Explore ONLY this direction. Actually RUN commands and probe — do not just plan. "
    "Read every response end-to-end. If this direction leads nowhere, that is a valid "
    "result — report it as a dead-end.\n\n"
    "## What to output\n"
    "When done (or stuck), report your findings with these markers on their own lines.\n"
    "Print VERIFIED_FACT only for NEW findings not already on the team board — reuse "
    "facts/passwords/unlocks teammates already recorded; put long data in a file and "
    "reference the path:\n"
    "  VERIFIED_FACT=<a confirmed, objective finding from REAL output>\n"
    "  DEADEND=<why this direction failed — so nobody retries it>\n"
    "  NEED_INPUT=<an EXTERNAL blocker only the operator can fix: a VPS/reverse-shell "
    "host, a credential, a tool, or the target being unreachable/expired — NOT a hard "
    "puzzle>\n"
    "  NEED_KIND=<external_blocker|lane_lock_request|route_dead_end|worker_uncertainty|operator_directive_needed>\n"
    "  POC_SAVE=<path>|<entry_command>|<status>|<note>  (optional: register a reusable "
    "PoC/payload/script from YOUR cwd; status is available/wip/directional/spent)\n"
    "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<the flag>'  "
    "(only after REAL output produced it)\n\n"
    "You may output multiple VERIFIED_FACT lines. The flag is shaped like {fmt}."
)

_PENTEST_EXPLORE_PROMPT = (
    "You are an expert penetration tester with a FULL shell "
    "(curl, python3, and common offensive tooling).\n\n"
    "{ctx}\n"
    "{kb}\n"
    "## Engagement goal\n{goal}\n\n"
    "## Scope / authorization — operate STRICTLY within this\n{scope}\n\n"
    "{box}\n\n"
    "## Your assigned direction\n"
    "You have been assigned ONE specific exploration direction:\n"
    "  {intent_goal}\n\n"
    "Explore ONLY this direction against the live in-scope HTTP origin. "
    "Send real requests. If the target exposes login, try common username "
    "field names (user, username, email) with the credentials you actually "
    "have; do not invent extra accounts. Save request/response bodies to "
    "files and cite them. If this direction leads nowhere, that is a valid "
    "result — report it as a dead-end.\n\n"
    "Do NOT read parent directories, the muteki source tree, labs/, or any "
    "server.py on disk. The origin in Scope is the only system you test.\n\n"
    f"{_PENTEST_REPORT_BLOCK}"
    "Also print team markers:\n"
    "  VERIFIED_FACT=<confirmed fact + pointer to the request/response evidence>\n"
    "  DEADEND=<a direction you ruled out — so nobody retries it>\n"
    "  NEED_INPUT=<an EXTERNAL blocker only the operator can fix>\n"
    "  NEED_KIND=<external_blocker|lane_lock_request|route_dead_end|worker_uncertainty|operator_directive_needed>\n"
    "  POC_SAVE=<path>|<entry_command>|<status>|<note>\n"
    "Print VERIFIED_FACT only for NEW facts not already on the team board. "
    "Do not submit a flag."
)

_EXPLORE_CONCLUDE_PROMPT = (
    "CONCLUDE: stop exploring NOW. Do not run any more commands.\n"
    "Summarize ONLY what you have already confirmed in REAL output, using these "
    "markers on their own lines:\n"
    "  VERIFIED_FACT=<a confirmed finding from real output>\n"
    "  DEADEND=<why this direction failed>\n"
    "  POC_SAVE=<path>|<entry_command>|<status>|<note>\n"
    "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<the flag>'  "
    "(only if seen in real output this session)\n"
    "If you found nothing, output DEADEND=<reason>. Do not guess."
)

_PENTEST_EXPLORE_CONCLUDE_PROMPT = (
    "CONCLUDE: stop exploring NOW. Do not run any more commands.\n"
    "Summarize ONLY what you have already confirmed in REAL output, using these "
    "markers on their own lines:\n"
    "  SUBMIT_REPORT=<path>  (only if you already wrote a complete report file)\n"
    "  VERIFIED_FACT=<a confirmed finding from real output>\n"
    "  DEADEND=<why this direction failed>\n"
    "  POC_SAVE=<path>|<entry_command>|<status>|<note>\n"
    "If you found nothing, output DEADEND=<reason>. Do not guess. Do not submit a flag."
)

_PENTEST_RESUME_PROMPT = (
    "CONCLUDE: stop exploring now. If you already wrote a complete in-scope "
    "report file this session, print SUBMIT_REPORT=<path>. Otherwise report the "
    "furthest confirmed fact. Do not guess. Do not submit a flag."
)

# ── f10 edge-cognition worker shell loop (design §1/§2) ──────────────────────
# Active ONLY when the worker carries an IntentEnvelope (SwarmF10 injects it as
# a [f10-shell-v1] guidance marker, or a caller passes shell_envelope=). The
# worker becomes a multi-step mini-agent: observe → plan → execute → verify →
# reflect → checkpoint, ≤ TURN_LIMIT turns, re-planning every PLAN_EVERY turns,
# with a local .shell/state.json checkpoint after every turn (crash-resumable).
_SHELL_TURN1_PROMPT = (
    "You are an expert CTF solver running as an EDGE WORKER SHELL with a FULL "
    "shell — a multi-step autonomous mini-agent, NOT a one-shot executor.\n\n"
    "{ctx}\n"
    "{kb}\n"
    "## Your assignment envelope\n"
    "Goal: {goal}\n"
    "Goal-ID: {goal_id}\n"
    "{effects_block}"
    "{criteria_block}"
    "## Your cognitive loop (you get at most {turn_limit} turns — this is turn 1)\n"
    "Each turn: OBSERVE (the board snapshot/workspace is refreshed for you) → "
    "act on ONE atomic step → VERIFY it against the verifier you predefined → "
    "REPORT. Rules:\n"
    "  - Reject soliloquizing: every observation must come from REAL tool/shell "
    "output. Never claim a finding you did not see in output.\n"
    "  - This is a PLANNING turn: decide your next 1-3 atomic steps. For each, "
    "predefine a runnable verifier (a passing condition you can check in output):\n"
    "      PLAN_STEP=<step#>|<one atomic action>|<verifier — what output proves it worked>\n"
    "  - Then execute step 1 immediately (run real commands).\n"
    "  - End EVERY turn with a one-line belief verdict:\n"
    "      REFLECT=<changed|unchanged>: <what you learned, or why nothing new>\n"
    "  - If you discover a genuinely DIFFERENT direction that deserves its own "
    "worker, declare it once:\n"
    "      SUB_INTENT=<a concrete new goal for the central arbiter to queue>\n"
    "{resume_block}"
    "## Standard reporting (unchanged)\n"
    "  VERIFIED_FACT=<a confirmed, objective finding from REAL output>\n"
    "  DEADEND=<a direction you ruled out — so nobody retries it>\n"
    "  NEED_INPUT=<an EXTERNAL blocker only the operator can fix>\n"
    "  NEED_KIND=<external_blocker|lane_lock_request|route_dead_end|worker_uncertainty|operator_directive_needed>\n"
    "  POC_SAVE=<path>|<entry_command>|<status>|<note>\n"
    "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<the flag>'  "
    "(only after REAL output produced it)\n"
    "The flag is shaped like {fmt}."
)

_PENTEST_SHELL_TURN1_PROMPT = (
    "You are an expert penetration tester running as an EDGE WORKER SHELL with a "
    "FULL shell — a multi-step autonomous mini-agent, NOT a one-shot executor.\n\n"
    "{ctx}\n"
    "{kb}\n"
    "## Engagement goal\n{engagement_goal}\n\n"
    "## Scope / authorization — operate STRICTLY within this\n{scope}\n\n"
    "{box}\n\n"
    "## Your assignment envelope\n"
    "Goal: {goal}\n"
    "Goal-ID: {goal_id}\n"
    "{effects_block}"
    "{criteria_block}"
    "## Your cognitive loop (you get at most {turn_limit} turns — this is turn 1)\n"
    "Each turn: OBSERVE (the board snapshot/workspace is refreshed for you) → "
    "act on ONE atomic step → VERIFY it against the verifier you predefined → "
    "REPORT. Rules:\n"
    "  - Reject soliloquizing: every observation must come from REAL tool/shell "
    "output. Never claim a finding you did not see in output.\n"
    "  - This is a PLANNING turn: decide your next 1-3 atomic steps. For each, "
    "predefine a runnable verifier (a passing condition you can check in output):\n"
    "      PLAN_STEP=<step#>|<one atomic action>|<verifier — what output proves it worked>\n"
    "  - Then execute step 1 immediately (run real commands).\n"
    "  - End EVERY turn with a one-line belief verdict:\n"
    "      REFLECT=<changed|unchanged>: <what you learned, or why nothing new>\n"
    "  - If you discover a genuinely DIFFERENT direction that deserves its own "
    "worker, declare it once:\n"
    "      SUB_INTENT=<a concrete new goal for the central arbiter to queue>\n"
    "{resume_block}"
    "## Standard reporting (unchanged)\n"
    "  SUBMIT_REPORT=<path>  (complete JSON report file; FOUND_FINDING is ignored)\n"
    "  VERIFIED_FACT=<a confirmed, objective finding from REAL output>\n"
    "  DEADEND=<a direction you ruled out — so nobody retries it>\n"
    "  NEED_INPUT=<an EXTERNAL blocker only the operator can fix>\n"
    "  NEED_KIND=<external_blocker|lane_lock_request|route_dead_end|worker_uncertainty|operator_directive_needed>\n"
    "  POC_SAVE=<path>|<entry_command>|<status>|<note>\n"
    "Do not submit a flag. Stay inside Scope."
)

_SHELL_PLAN_TURN_PROMPT = (
    "TURN {turn}/{turn_limit} — PLANNING turn (strong re-plan).\n"
    "Budget: ~{tokens_used}/{token_limit} tokens used.{budget_warn}\n"
    "Re-evaluate your direction against the assignment goal:\n  {goal}\n"
    "{effects_block}"
    "Confirmed so far this shell:\n{subfacts_block}"
    "Dead-ends so far (do NOT retry):\n{deadends_block}"
    "Decide the next 1-3 atomic steps, each with a runnable verifier:\n"
    "  PLAN_STEP=<step#>|<one atomic action>|<verifier>\n"
    "Then execute the first step NOW with real commands. If the current direction "
    "is dead, say DEADEND=<why> and pivot within the goal, or declare "
    "SUB_INTENT=<new direction> if it deserves a separate worker.\n"
    "End with: REFLECT=<changed|unchanged>: <one line>"
)

_SHELL_EXEC_TURN_PROMPT = (
    "TURN {turn}/{turn_limit} — EXECUTE turn (no re-planning).\n"
    "Budget: ~{tokens_used}/{token_limit} tokens used.{budget_warn}\n"
    "Execute this planned step NOW with real commands:\n"
    "  step {step}: {action}\n"
    "  verifier: {verifier}\n"
    "Check the output against the verifier. If it PASSED, report "
    "VERIFIED_FACT=<what is now confirmed>. If it FAILED, adjust within the step "
    "once; if the whole approach is ruled out, report DEADEND=<why>.\n"
    "End with: REFLECT=<changed|unchanged>: <one line>"
)

_REVIEW_PROMPT = (
    "You are the Review-Arbiter for a CTF/pentest-solving swarm. You do NOT solve "
    "directly and you must NEVER declare the run solved. Your job is to audit the "
    "shared graph and turn doubt into scheduling control: challenge weak facts, "
    "suppress repeated routes, split conflicting branches, propose verifier work, "
    "or issue a coordinator directive.\n\n"
    "{ctx}\n"
    "{kb}\n"
    "{engagement}"
    "## Review assignment\n{intent_goal}\n\n"
    "## Full review board\n{review_board}\n\n"
    "Output only machine-readable markers, one per line. Every challenge must carry "
    "a follow-up action. Valid markers:\n"
    "  REVIEW_FINDING=<json>\n"
    "  FACT_CHALLENGE=<json>\n"
    "  FACT_MERGE=<json>\n"
    "  FACT_SUPERSEDE=<json>\n"
    "  FACT_REJECT=<json>\n"
    "  FACT_REVALIDATION=<json>\n"
    "  ROUTE_SUPPRESS=<json>\n"
    "  ROUTE_REOPEN=<json>\n"
    "  BRANCH_SPLIT=<json>\n"
    "  LANE_LOCK=<json>\n"
    "  LANE_UNLOCK=<json>\n"
    "  COORDINATOR_DIRECTIVE=<json>\n"
    "  NEXT_INTENT=<json>\n"
    "  NEED_INPUT=<text>\n"
    "  NEED_KIND=<external_blocker|lane_lock_request|route_dead_end|worker_uncertainty|operator_directive_needed>\n\n"
    "DEDUP IS THE #1 JOB. Candidates that merely restate an existing verified fact "
    "(same finding, different wording, or an '[engine]' echo) must be RETIRED with "
    "FACT_MERGE — NOT FACT_CHALLENGE. FACT_CHALLENGE spawns a whole verifier worker "
    "and leaves the duplicate sitting in the active set; FACT_MERGE folds it into the "
    "canonical fact and removes it immediately. Only FACT_CHALLENGE a candidate that "
    "is genuinely unproven AND not a duplicate. When a duplicate has no canonical "
    "verified twin yet, FACT_REJECT it. Reserve verifier work for real open questions, "
    "not bookkeeping.\n"
    "Required examples:\n"
    "FACT_MERGE={{\"from_fact_seq\":144,\"to_fact_seq\":136,"
    "\"reason\":\"echo duplicate of verified .26 host fact, same ports\"}}\n"
    "FACT_REJECT={{\"fact_seq\":151,\"reason\":\"cursor-echo restate, no new evidence\"}}\n"
    "FACT_CHALLENGE={{\"fact_seq\":42,\"reason\":\"unverified JWT alg\","
    "\"verification_goal\":\"Decode a real captured JWT header and verify alg.\"}}\n"
    "ROUTE_SUPPRESS={{\"route_hash\":\"web:login:sqli\",\"label\":\"login SQLi\","
    "\"reason\":\"repeated by multiple workers with no new evidence\","
    "\"until\":\"new_evidence\",\"matching_intents\":[\"I1\"]}}\n"
    "COORDINATOR_DIRECTIVE={{\"action\":\"rebootstrap\",\"priority\":\"high\","
    "\"directive\":\"Stop login SQLi. Build from verified upload parser fact.\"}}\n"
    "LANE_LOCK={{\"lane_key\":\"destructive:tcp:445@172.22.11.45\","
    "\"risk_class\":\"destructive\",\"reason\":\"serialize SMB exploit attempts\"}}\n"
    "LANE_UNLOCK={{\"lane_key\":\"destructive:tcp:445@172.22.11.45\","
    "\"reason\":\"owner finished or lock was stale\"}}\n"
    "NEXT_INTENT={{\"worker_class\":\"verifier\",\"goal\":\"Verify JWT alg from a real token\"}}\n\n"
    "Never submit a flag. If you see a flag in the board, treat it as existing "
    "state only; the normal flag gate already handled it."
)

# Capture EVERYTHING after FOUND_FLAG= to end-of-line (not \S+), so a flag whose
# body legitimately contains SPACES (e.g. CTF flags like `flag{H1570rY 12'N7 ...}`)
# isn't truncated at the first space. _clean_flag_token() then extracts the real
# token from the captured tail. (run-15161: \S+ truncated `flag{H1570rY 12'N7...}`
# to `flag{H1570rY` → never closed → never registered, despite the worker solving.)
_FLAG_LINE = re.compile(r"FOUND_FLAG=\s*(.+)")
_FINDING_LINE = re.compile(r"FOUND_FINDING=\s*(.+)")
_SUBMIT_REPORT_LINE = re.compile(r"SUBMIT_REPORT=\s*[`'\"]?([^\s`'\"]+)")
_REPRODUCED_LINE = re.compile(r"REPRODUCED=\s*(yes|no)\b", re.IGNORECASE)
_REPRO_WITNESS_LINE = re.compile(r"REPRO_WITNESS=\s*(.+)")
_REPRO_REASON_LINE = re.compile(r"REPRO_REASON=\s*(.+)")
# A xxx{...} brace-structured flag anywhere in the captured tail — inner spaces OK.
_BRACE_FLAG = re.compile(r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}")


def _clean_flag_token(raw: str) -> str:
    """Turn the raw FOUND_FLAG= tail into the actual flag token. Two shapes:
      - brace flag `xxx{...}`: return the FULL brace structure (spaces allowed
        inside) — fixes both the markdown-`**` pollution (BUG-1) and the
        space-truncation (BUG-2), since we grab exactly `…{…}` and drop any
        trailing prose / `**` / punctuation the worker appended on the same line.
      - bare token (no braces): take the first whitespace-delimited word, stripped
        of wrapping quotes/backticks/markdown — old behavior preserved.
    """
    s = (raw or "").strip().strip("`'\"*_ ").strip()
    m = _BRACE_FLAG.search(s)
    if m:
        return m.group(0)
    # bare token: first word, minus wrapping markdown/punctuation
    tok = s.split()[0] if s.split() else ""
    return tok.strip("`'\"*_.,;:!").strip()
_VERIFIED_FACT_LINE = re.compile(r"VERIFIED_FACT=\s*(.+)")
_FACT_WITNESS_LINE = re.compile(r"FACT_WITNESS\s*=\s*(.+)", re.IGNORECASE)
_DEADEND_LINE = re.compile(r"DEADEND=\s*(.+)")
# the worker raises its hand: it needs something it cannot get on its own (a VPS /
# reverse-shell host, a credential, a tool) OR the challenge environment is
# objectively unusable (target unreachable / instance expired). This is NOT a
# dead-end (a ruled-out direction) — it's a request for the operator to supply
# something, after which the run can continue. Surfaced as a HITL_REQUEST.
_NEED_INPUT_LINE = re.compile(r"NEED_INPUT=\s*(.+)")
_NEED_KIND_LINE = re.compile(r"NEED_KIND\s*=\s*([a-z_]+)", re.IGNORECASE)
_POC_SAVE_LINE = re.compile(r"POC_SAVE=\s*([^|]+)\|([^|]+)\|([^|]+)\|(.*)")
# ── f10 edge-cognition shell-loop markers (parsed only on the shell path) ─────
# PLAN_STEP=<n>|<atomic action>|<verifier>  — the worker's own 1-3 step sub-plan,
# refreshed on planning turns (every PLAN_EVERY). SUB_INTENT= hands a genuinely
# new direction back to the central arbiter's intent queue. REFLECT= is the
# per-turn belief-change verdict used for stuck detection.
_SHELL_PLAN_LINE = re.compile(r"PLAN_STEP=\s*(.+)")
_SHELL_SUB_INTENT_LINE = re.compile(r"SUB_INTENT=\s*(.+)")
_SHELL_REFLECT_LINE = re.compile(r"REFLECT=\s*(.+)")
_SECRET_LITERAL_RE = re.compile(
    r"(-----BEGIN [A-Z ]*PRIVATE KEY-----|"
    r"\b(?:api[_-]?key|token|secret|password)\s*[:=]\s*['\"]?[A-Za-z0-9_./+=-]{12,})",
    re.IGNORECASE,
)

# ── rate-limited verifier (submission gate) markers + lockout parsing ────────
# A worker about to submit to a rate-limited verifier first declares this so the
# coordinator can serialize submissions (grant the submit-lock to one worker at a
# time). The value is a one-line self-check summary (e.g. "local validator OK,
# 18/18 people, admiralty conservative").
_READY_TO_SUBMIT_LINE = re.compile(r"READY_TO_SUBMIT=\s*(.+)")
_REVIEW_JSON_MARKERS = {
    "REVIEW_FINDING", "FACT_CHALLENGE", "FACT_REVALIDATION",
    "FACT_REJECT", "FACT_MERGE", "FACT_SUPERSEDE",
    "ROUTE_SUPPRESS", "ROUTE_REOPEN", "BRANCH_SPLIT", "BRANCH_RESOLVE",
    "COORDINATOR_DIRECTIVE", "NEXT_INTENT", "LANE_LOCK", "LANE_UNLOCK",
}
# Parse a cooldown / burn-lockout duration out of the verifier's own output so the
# swarm backs off for exactly that long. Covers the common phrasings: "wait N
# minutes", "locked for N min", "try again in N seconds", "cooldown: Ns". Returns
# seconds. Conservative — only fires on an explicit duration next to a lock word.
_LOCKOUT_RE = re.compile(
    r"(?:lock(?:ed|out)?|cooldown|wait|try again|rate.?limit|too many|burn)\D{0,40}?"
    r"(\d+(?:\.\d+)?)\s*(seconds?|secs?|s|minutes?|mins?|m|hours?|hrs?|h)\b",
    re.IGNORECASE)

_VALID_NEED_KINDS = {
    "external_blocker",
    "operator_directive_needed",
    "lane_lock_request",
    "route_dead_end",
    "worker_uncertainty",
}


def _normalize_need_kind(kind: str) -> str:
    k = (kind or "").strip().lower()
    return k if k in _VALID_NEED_KINDS else ""


def classify_need_kind(text: str) -> str:
    """Fine-grained HITL classifier, separate from legacy env_down/need_input."""
    low = (text or "").lower()
    if any(k in low for k in (
        "ask operator", "operator decide", "需要 operator", "need a decision from",
    )):
        return "operator_directive_needed"
    if any(k in low for k in (
        "exclusive", "独占", "serialize", "序列化", "another worker",
        "其它 worker", "其他 worker", "same target", "stop hammering",
    )):
        return "lane_lock_request"
    if any(k in low for k in (
        "unreachable", "connection refused", "refused", "timed out", "timeout",
        "expired", "instance", "502", "503", "down", "credential", "凭据",
        "vps", "attachment", "附件", "token", "runtime", "container",
    )):
        return "external_blocker"
    if any(k in low for k in (
        "已知失败", "repeatedly fail", "known dead", "dead end", "dead-end",
        "route dead", "route failed", "no longer viable", "打不通", "走死",
    )):
        return "route_dead_end"
    return "worker_uncertainty"


def _parse_lockout_seconds(text: str) -> float:
    """Return the LARGEST lockout duration (in seconds) mentioned in `text`, or 0
    if none. We take the max because a worker's output may quote both an 8s
    per-attempt cooldown and a 30-min burn-lockout in the same chunk — the long one
    is the binding backoff. Bare unit letters (s/m/h) are honored too."""
    best = 0.0
    for m in _LOCKOUT_RE.finditer(text or ""):
        try:
            n = float(m.group(1))
        except (TypeError, ValueError):
            continue
        unit = (m.group(2) or "s").lower().rstrip("s")  # minutes→minute, hrs→hr
        if unit in ("minute", "min", "m"):
            n *= 60
        elif unit in ("hour", "hr", "h"):
            n *= 3600
        best = max(best, n)
    return best


# A lockout is real ONLY if this text is the verifier's OWN verdict output — not a
# worker reading a local file/doc that merely DESCRIBES the lockout. Without this,
# any chunk that MENTIONS "burn-lockout … 30 min" trips a phantom backoff the whole
# swarm then honors (run-11553: cursor read docs/PROBLEM_verifier_rate_limit_*.md →
# the lock prose matched → fake VERIFIER_LOCKED, nobody had even run the verifier).
# Same bug class as run-11550's grep-pattern-as-flag: "text ABOUT X" ≠ "X happened".
#
# Two-part rule. POSITIVE: the verifier's characteristic verdict phrasing must be
# present (the binary prints "burn-lockout: N burns in last M min" / "wait N before
# … attempt" / "N attempts remaining" — phrasings a doc paraphrase rarely
# reproduces verbatim). NEGATIVE: if the chunk is dominated by file-read signatures
# (a `read:`/`cat`/`head` of a path, or our own doc/brief filenames), it's prose.
_VERIFIER_VERDICT_RE = re.compile(
    r"burn-?lock(?:out)?\s*[:\-]|"            # "burn-lockout:" — the verifier's own header
    r"\d+\s*burns?\s+in\s+(?:the\s+)?last|"   # "3 burns in last 30 min"
    r"attempts?\s+(?:left|remaining)|"        # "2 attempts remaining"
    r"too\s+many\s+(?:wrong\s+)?attempts?|"   # "too many wrong attempts"
    r"locked\s+for\s+\d|"                     # "locked for 30 minutes"
    r"wait\s+(?:for\s+)?(?:the\s+)?cooldown",  # "wait for the cooldown to lift"
    re.IGNORECASE)
# Reading a local file/doc that talks ABOUT the lockout — the false-trigger surface.
_DOC_READ_RE = re.compile(
    r"(?:^|\n)\s*read:\s|"                    # the CLI's file-read step label
    r"PROBLEM_verifier|BRIEFING|known_intel|missions?\.json|"
    r"\.md\b|DESIGN_|SOP_",                   # our own markdown docs
    re.IGNORECASE)


# The verifier's invocation footprint — present when its verdict was actually
# produced (the script ran), absent in free narrative prose.
_VERIFIER_INVOKE_RE = re.compile(
    r"(?:specter-verify|verify-[a-z0-9-]+\.sh|/opt/verify-)", re.IGNORECASE)


def _looks_like_verifier_output(text: str) -> bool:
    """True only when a lock phrase in `text` is the VERIFIER's own verdict. Three
    gates, all required:
      1. characteristic verdict PHRASING (burn-lockout: / N burns in last / locked
         for N / too many attempts / cooldown) — generic "it's locked" isn't enough;
      2. the verifier INVOCATION footprint (specter-verify / verify-*.sh / /opt/
         verify-) — proof the verdict was actually produced, not narrated;
      3. NOT a doc/file read (read:/our markdown/BRIEFING/intel) — the false-trigger
         surface (run-11553: cursor read PROBLEM_verifier_*.md, which has BOTH the
         phrasing AND mentions /opt/verify-…sh, so the footprint alone is fooled —
         the doc-read guard is what rejects it)."""
    t = text or ""
    if not _VERIFIER_VERDICT_RE.search(t):
        return False           # generic "locked 30 min" prose → not a verdict
    if not _VERIFIER_INVOKE_RE.search(t):
        return False           # no invocation footprint → narrated, not produced
    if _DOC_READ_RE.search(t):
        return False           # footprint + phrasing came from a doc read → prose
    return True

# ── Respond mode (post-solve standby) ───────────────────────────────────────
# A human typed something after the challenge was solved (or the server
# restarted). We resume the winner's SAME CLI session so it answers WITH its full
# memory of how it solved the challenge. Three intents:
#   ask        — a plain question / follow-up about the solve
#   mark_false — the flag was a false positive; keep solving, avoid it
#   writeup    — produce a concise solve writeup from confirmed facts
_RESPOND_ASK_PROMPT = (
    "The operator has a follow-up about the challenge you just worked. Answer it "
    "directly and concretely, drawing on what you already confirmed this session. "
    "If answering needs a quick check, you may run a command — but do not start a "
    "long new investigation; this is a conversation, not a fresh solve.\n\n"
    "Operator: {text}"
)

_RESPOND_MARK_FALSE_PROMPT = (
    "IMPORTANT: the flag you reported — {flag} — is a FALSE POSITIVE (the operator "
    "verified it does not work). Treat it as a dead-end: do NOT report it again. "
    "Resume solving from the facts you already confirmed and find the REAL flag.\n"
    "{note}\n"
    "Actually RUN commands against the real target/files. When you recover the TRUE "
    "flag from REAL output, call the Blackboard API exactly as:\n"
    "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<flag>'\n"
    "It must appear verbatim in your shell output first. Also print VERIFIED_FACT=<...> / "
    "DEADEND=<...> lines as you go so the team's board stays current."
)

_RESPOND_WRITEUP_PROMPT = (
    "Write a concise CTF WRITEUP for the challenge you just solved, in Chinese. "
    "Base it ONLY on what you actually confirmed this session — do not invent steps. "
    "Do not run commands, call tools, search the filesystem, or continue the "
    "investigation. Synthesize the report from the confirmed session history now. "
    "Structure it as:\n"
    "  ## 漏洞点  (the root cause / vulnerability)\n"
    "  ## 利用步骤  (numbered, reproducible — the real commands/requests you used)\n"
    "  ## Flag  (the flag and where it came from)\n"
    "Keep it tight and technical. Output ONLY the markdown writeup, nothing else."
)

_RESPOND_PENTEST_WRITEUP_PROMPT = (
    "Write a concise penetration-testing report in Chinese for the engagement you "
    "just completed. Base it ONLY on evidence confirmed this session. "
    "Do not run commands, call tools, search the filesystem, or continue the "
    "investigation. Synthesize the report from the confirmed session history now. "
    "Structure it as:\n  ## 范围与目标\n  ## 已确认发现\n  ## 复现步骤\n  ## 影响与修复建议\n"
    "Do not invent findings, commands, requests, or impact. Output ONLY markdown."
)


class CliSolver:
    """Swarm worker backed by a shelled CLI agent. Mirrors Solver's interface."""

    def __init__(
        self,
        spec: Any,                       # ModelSpec (we only use .solver_id)
        challenge: Challenge,
        *,
        sandbox: Any = None,             # unused (CLI has its own shell) — kept for parity
        bus: Optional[EventBus] = None,
        cost: Optional[CostController] = None,
        artifacts: Optional[ArtifactStore] = None,
        config: Optional[SolverConfig] = None,
        run_id: Optional[str] = None,
        insight: Optional[Any] = None,
        knowledge: Optional[Any] = None,
        shared_graph: Optional[Any] = None,
        driver: Optional[CliDriver] = None,
        engine: str = "claude",
        max_turns: int = 80,
        timeout: int = 2400,
        workdir: Optional[str] = None,
        web_access: bool = True,
        kb: bool = True,
        kb_config: Optional[str] = None,
        mode: str = "bootstrap",
        intent_goal: str = "",
        intent_id: str = "",
        conclude_timeout: int = 300,
        resume_session: Optional[str] = None,
        hitl_cmd: Optional[dict] = None,
        solver_label: Optional[str] = None,
        lifecycle_scope: str = "run",
        standing_guidance: Optional[list] = None,
        found_flags: Optional[list] = None,
        container: "Optional[object]" = None,
        worker_env: Optional[dict[str, str]] = None,
        identity: Optional[dict[str, Any]] = None,
        execution_occurrence: Optional[str] = None,
        resolve_epoch: Optional[str | int] = None,
        shell_envelope: Optional[dict] = None,
    ) -> None:
        self.spec = spec
        self.challenge = challenge
        self.bus = bus
        self.cost = cost
        self.config = config or SolverConfig()
        # container backend: a ContainerHandle → run this worker in the run's Kali
        # tool container (consistent toolchain). None → host subprocess.
        self.container = container
        self._extra_worker_env = dict(worker_env or {})
        self.identity = {
            str(key): str(value)
            for key, value in (identity or {}).items()
            if value
        } or worker_identity_from_env(self._extra_worker_env)
        self.run_id = run_id or challenge.id
        self.graph = SolveGraph(challenge=challenge)
        # solver_id: prefer an explicit label (the coordinator hands each spawned
        # worker a UNIQUE one like "cli-claude#3" so the deck draws one lane per
        # worker — without it every claude worker would collapse onto the single
        # "cli-claude" lane and you couldn't tell parallel/re-bootstrapped workers
        # apart). Then the spec's label; in race mode specs may be shared or None,
        # so fall back to the engine. The "cli-<engine>" prefix is preserved in all
        # cases so workerEngine() on the deck still detects the engine badge.
        base = (solver_label
                or (getattr(spec, "solver_id", None) if spec is not None else None)
                or f"cli-{engine}")
        self.solver_id = base
        self.insight = insight
        self.shared_graph = shared_graph
        self.artifacts = artifacts or ArtifactStore(root=str(Path(tempfile.gettempdir()) / "muteki-cli-arts"))
        self.driver = driver or driver_for(engine)
        self.max_turns = max_turns
        self.timeout = timeout
        self._workdir = workdir
        self._staged_files: list[str] = []  # attachment basenames copied into cwd
        # eval hygiene: offline mode denies the agent's web tools so a bench run
        # can't be contaminated by a writeup lookup. Keep web ON for real CTF.
        self.web_access = web_access
        # KB: the optional knowledge-base MCP (point it at your own service, e.g.
        # a security-intel / CVE / writeup index) is registered at USER scope (see
        # `claude mcp add --scope user`), so a worker INHERITS it automatically —
        # no per-run --mcp-config (which would re-trigger the project-server trust
        # gate that headless `claude -p` can't clear). We only (a) tell the model
        # the KB exists via _KB_PROMPT, and (b) actively SUPPRESS it when kb is
        # off by denying its mcp tools. kb_config is accepted for back-compat /
        # tests but is no longer used to mount the server.
        # KB is OFF unless a KB MCP is configured (MUTEKI_KB_MCP_NAME); there is no
        # bundled KB service, so out of the box this is always off. When configured,
        # only the claude engine inherits the user-scope KB (codex has its own config
        # dir and doesn't see it), so KB is on iff a name is set AND requested AND
        # this worker runs claude. (kb_config kept for back-compat; unused now.)
        self.kb = bool(KB_MCP_NAME) and bool(kb) and self.driver.name == "claude"
        self.kb_config = kb_config
        # mode: "bootstrap" = whole-challenge rush (current behavior);
        # "explore" = claim one intent, explore that direction only,
        # conclude with structured facts. Explore prevents context explosion by
        # keeping each worker's scope narrow.
        self.mode = mode
        self.intent_goal = intent_goal
        self.intent_id_assigned = intent_id
        self.conclude_timeout = conclude_timeout
        # respond mode (post-solve standby): resume the winner's CLI session to
        # answer a human follow-up / mark a false positive / write a writeup.
        # resume_session is the winner's session id (None → fresh session +
        # blackboard context as a fallback). hitl_cmd is the operator's command
        # {action, text} that this respond worker is serving.
        self.resume_session = resume_session
        self.hitl_cmd = hitl_cmd or {}
        # Decision ids must distinguish a fresh execution encountering the same
        # blocker from an SSE replay of the original occurrence.  The occurrence is
        # created once per CliSolver instance (or injected by a deterministic caller);
        # a resolve epoch may be supplied explicitly or carried by the standby HITL
        # command.  Both are emitted with the request for audit/debug correlation.
        supplied_occurrence = (execution_occurrence
                               or self.hitl_cmd.get("execution_occurrence"))
        self._execution_occurrence = str(supplied_occurrence or uuid.uuid4().hex)
        supplied_epoch = (resolve_epoch if resolve_epoch is not None
                          else self.hitl_cmd.get("resolve_epoch", ""))
        self._resolve_epoch = str(supplied_epoch if supplied_epoch is not None else "")
        self._decision_request_ids: "dict[tuple[str, str], str]" = {}
        # lifecycle_scope: "run" = this solver IS the run (mock / race / standby) →
        # its terminal emit is a run-level RUN_FINISHED. "worker" = a swarm sub-worker
        # under a coordinator that re-bootstraps until solved/stopped → its terminal
        # emit is a worker-level WORKER_FINISHED, so the deck does NOT mark the whole
        # run finished every time one worker ends (the run-7345 "怎么又结束了" bug).
        # The coordinator emits the single run-level RUN_FINISHED when ITS loop exits.
        self.lifecycle_scope = lifecycle_scope
        # The live CLI session id for THIS worker's run (claude pre-seeds a uuid;
        # codex scrapes one after turn 1). Surfaced to the deck via WORKER_STATUS so
        # the operator can manually attach to a worker mid-solve — `claude -r <id>`
        # or `codex exec resume <id>`. None until the worker's first turn assigns it.
        self._cli_session: Optional[str] = None
        self._last_runtime_status: dict = {}

        # ── Runtime control channel (live dispatcher control) ─────────────────
        # The CLI worker is a subprocess; these let the swarm/HITL steer it while
        # it runs instead of fire-and-forget. cancel() kills the subprocess (so a
        # winner actually stops the losers); the pause monitor SIGSTOP/SIGCONTs it.
        self._cancel_event = threading.Event()
        # steer_event: a SECOND signal distinct from cancel. cancel = die; steer =
        # end this turn but keep the session so the loop resumes with operator
        # guidance folded in (the "steering" channel). The monitor sets it on a
        # redirect; the streaming runner kills the subproc and reports res.steered.
        self._steer_event = threading.Event()
        self._stalled_at: "Optional[float]" = None
        self._idle_repeat_steered = False
        # P2 regression guard: only allow an in-turn steer (FLAG / standing
        # correction → _steer_event.set) while a subprocess turn is ACTUALLY
        # running. Without this, a freshly-spawned worker's _drain_control replays
        # the InsightBus HISTORY backlog (every prior FLAG + standing hint) and
        # would steer-kill the brand-new subprocess the instant it starts → 0
        # tokens → "explore → conclude fallback" (run-40726: 60+ claude workers
        # quick-exited after flag1 landed because flag1 sat in the replay backlog).
        # Set True only inside _run_streaming; the backlog is consumed (folded into
        # _already_found / _standing_guidance) but does NOT steer.
        self._turn_active = False
        # Resume-safety guard: a `build_resume` (claude `-r <sid>` / codex resume)
        # against a session the engine never actually established returns
        # "No conversation found" → 0 tokens → "(no output)" → instant dead_end,
        # and never-give-up re-spawns into the same trap (run-42598: claude-5..33
        # each lived ~1.7s, 0 tokens, after a turn-1 execute had failed to seat the
        # pre-seeded uuid). We only mark a session established once a turn actually
        # produced output / a real session id; resume turns fall back to a fresh
        # `build_execute` (carrying the same board+peer context) until then.
        self._session_established = False
        # guards the cross-thread guidance paths: the monitor thread (_drain_control)
        # appends operator hints to _standing_guidance / sets _target_override while
        # the loop thread reads them to build the next prompt. (The old _guidance_buf
        # one-shot fold-in buffer was removed in the single-shot cleanup — headless
        # CLIs take no mid-turn input, so guidance flows to the next spawned worker.)
        self._guidance_lock = threading.Lock()
        # standing guidance (VPS/SSH creds, global constraints): persistent text
        # injected into EVERY turn's prompt, not consumed like a one-shot steer.
        # Seeded from the coordinator's canonical list at spawn (so a worker created
        # AFTER the operator gave a VPS hint still carries it in turn-1), then grows
        # via the live InsightBus inbox while running.
        self._standing_guidance: "list[str]" = (
            [str(s) for s in standing_guidance if s] if standing_guidance else [])
        # Operator context is UNTRUSTED input, never flag evidence.  Keep the exact
        # strings separately from the execution-output corpus so a worker cannot turn
        # an operator-supplied candidate into a "real" flag by running
        # `echo FOUND_FLAG=<candidate>`.  The target/verifier must produce a value that
        # did not originate in operator context for it to pass the hard gate.
        self._operator_context_texts: "list[str]" = list(self._standing_guidance)
        # Exact plaintext values materialised from secret:// ContextResources are
        # installed by the Swarm/standby builder.  Their prompt is delivered over a
        # non-persistent stdin-only engine invocation (never argv); every durable
        # event/graph/artifact boundary additionally uses this list for deterministic
        # output redaction.
        self._control_secret_values: "list[str]" = []
        for _key in ("text", "hint", "flag", "url", "target_url"):
            _value = self.hitl_cmd.get(_key)
            if _value:
                self._remember_operator_context(str(_value))
        # a redirect can retarget the worker at a new URL; _build_prompt prefers it
        # over challenge.target. Per-worker (NOT mutating the shared Challenge, which
        # sibling workers share by reference).
        self._target_override: "Optional[str]" = None
        # a standby/respond worker built from a redirect/standing HITL command picks
        # up its new target + standing guidance immediately (the live path gets these
        # via the InsightBus; the cold-started path gets them via hitl_cmd).
        if self.hitl_cmd.get("url"):
            self._target_override = self.hitl_cmd["url"]
        if self.hitl_cmd.get("standing") and self.hitl_cmd.get("text"):
            self._standing_guidance.append(str(self.hitl_cmd["text"]))
        self._live_procs: "set[Any]" = set()   # Popen handles of running subprocs
        self._procs_lock = threading.Lock()
        # asyncio cancellation does not stop a function already running inside
        # to_thread. Keep runner Tasks independently from the coroutine that awaited
        # them so callers can prove the runtime really exited after the outer worker
        # or standby task has already unwound.
        self._runner_tasks: "set[asyncio.Task[Any]]" = set()
        self._runner_proc_local = threading.local()
        # Typed operator context is reserved before prompt materialisation and
        # committed only when the prompt-carrying subprocess exists.  The swarm
        # installs the journal callbacks on constructed workers.
        self._pending_control_context_reservations: list[tuple[str, str]] = []
        # One row per durable reservation, carrying the exact materialized text
        # expected in the final invocation prompt.  The coordinator installs this
        # after construction.  It is reconciled immediately before argv/stdin is
        # built, so prompt budgeting cannot falsely consume a context that never
        # reached the model.
        self._control_context_prompt_manifest: "list[dict[str, Any]]" = []
        self._control_context_prompt_manifest_finalized = False
        self._control_context_prompt_included: "tuple[tuple[str, str], ...]" = ()
        self._context_committer: Optional[Any] = None
        self._context_releaser: Optional[Any] = None
        self._context_delivery_unknown_marker: Optional[Any] = None
        self._context_binding_worker_id = ""
        self._context_delivery_callback: Optional[Any] = None
        self._context_delivery_lock = threading.Lock()
        self._control_context_delivery_unknown = False
        self._control_context_delivery_committed = False
        # C6: the host may bind one sealed, attempt-specific ContextPacket after
        # canonical admission preparation and before this worker can launch.  The
        # worker can render the lossy view but receives no store/compiler authority.
        self._cognitive_context_packet: Optional[Any] = None
        self._c6_invocation_runner: Optional[Any] = None
        self._cognitive_pending_prompt: Optional[str] = None
        self._cognitive_stage_released = False
        self._cognitive_stage_unknown = False
        self._cognitive_stage_lock = threading.Lock()
        # Monotonic execution fence used by coordinator retirement.  A claimed
        # intent is replay-safe only when the worker runtime is proven exited and
        # this flag is still false (no Popen/remote child ever existed).
        self._runtime_process_started = False
        self._remote_start_uncertain = False
        # M9/M10: a mkdtemp fallback scratch dir THIS worker owns (set only when the
        # swarm didn't provide a managed self._workdir). run()'s finally rmtree's it on
        # EVERY exit path — solved-early-return, cancel, exception — except when the
        # worker solved AND the dir is its returned winner artifact. None ⇒ nothing to
        # clean (managed worker_root dirs are swept by the swarm's cleanup_worker_scratch).
        self._owned_scratch: "Optional[Path]" = None
        self._paused = False
        # M7: set while the operator has this worker SIGSTOP'd. The streaming runner
        # reads it to EXCLUDE paused wall-clock from the turn timeout — a worker frozen
        # by the operator must not be killed as "timed_out" just for being paused.
        self._paused_event = threading.Event()
        # InsightBus inbox (HITL pause/resume/hint + sibling FLAG); subscribed in
        # run(). The base Solver drains this between turns — a CLI worker has no
        # between-turn point, so a monitor thread drains it while the subproc runs.
        self._insight_inbox: "Optional[asyncio.Queue]" = None
        # dedupe set for VERIFIED_FACT=/DEADEND= markers published live (a marker
        # appears in both the tool-call echo and its result, and again at end-of-run).
        self._published_markers: "set[tuple[str, str]]" = set()
        self._published_pocs: "set[str]" = set()
        self._claimed_pocs: "set[str]" = set()
        self._inherited_pocs: "list[dict[str, str]]" = []
        self._current_workdir: "Optional[Path]" = None
        self._worker_stop_reason = ""
        # multi-flag: every flag THIS worker has already accepted + broadcast, so
        # it never double-counts one (across turns) and skips flags a sibling
        # already found (seeded from the bus on each FLAG insight in _drain_control).
        # A re-bootstrapped worker is seeded with the run's already-found flags so
        # its prompt lists them and it hunts only the rest.
        self._already_found: "set[str]" = set(found_flags or [])
        self._already_found_findings: "set[str]" = set()
        self._already_submitted_reports: "set[str]" = set()
        self._pending_report_paths: "list[str]" = []
        # Protocol 2 must return same-attempt accepted values to its host authority for
        # receipt reconciliation without projecting them through the legacy SolveGraph.
        # Unlike _already_found, this contains only flags accepted by THIS worker.
        self._protocol2_accepted_flags: "list[str]" = []
        # flags accepted via the LIVE stream (_stream_markers caught a FOUND_FLAG=
        # in an intermediate/streamed chunk that never reached the terminal result
        # text). run()'s per-turn flag extraction reads res.text only, so the run
        # loops consult this set too to decide solved (run-11189: a clean FOUND_FLAG
        # in cursor's mid-stream assistant text was extracted as a fact but the flag
        # itself was dropped because it wasn't in the terminal `result`).
        self._stream_accepted: "list[str]" = []
        # ── raw command-output provenance corpus (run-75379) ─────────────────────
        # The FULL, untruncated stdout/stderr of every tool execution this worker ran,
        # captured live from each tool_result StreamStep BEFORE the 600-char deck
        # truncation and BEFORE the engine summarizes everything into CliResult.text.
        # This is the authoritative evidence a flag must trace to: a flag claimed via
        # FOUND_FLAG= is accepted only if its value appears in here (or in the result
        # text). A flag past char 600 of a command's output, or one read on a pivoted
        # host via nested ssh (the outer ssh forwards remote stdout into this buffer),
        # is gateable here even though it never reaches the truncated chunk or the
        # summarized result envelope. A flag with NO trace in this corpus is surfaced
        # as UNVERIFIED, not auto-promoted to solved. Bounded so a chatty run can't
        # balloon memory; the per-chunk artifact persist keeps the full transcript.
        self._raw_tool_outputs: "list[str]" = []
        self._raw_tool_outputs_chars = 0
        # Target-identity attestation (round-10): the command text paired with each
        # raw output record, same ring-trim. Lets the flag gate distinguish a flag
        # EXTRACTED from a sanctioned resource from one the worker manufactured
        # (planted file / self-seeded service / writeup fetch).
        self._raw_tool_commands: "list[str]" = []
        # Whether each raw result was paired to an observed tool invocation. Unmatched
        # results remain available for audit but cannot become authoritative evidence.
        self._raw_tool_attributed: "list[bool]" = []
        # Protocol 2 stores the capture receipt alongside the exact raw record. Empty
        # outside Protocol 2. Keeping it parallel preserves existing corpus consumers
        # while making receipt-backed authority mechanically checkable.
        self._raw_tool_capture_receipts: "list[str]" = []
        # Protocol 1 Flag authority is API-driven.  The blackboard command appends
        # flag_submission; only candidates consumed and provenance-validated by this
        # Worker may reach _accept_flag.  Plain FOUND_FLAG text remains diagnostic.
        self._validated_flag_submissions: set[str] = set()
        self._last_flag_submission_seq = 0
        self._flag_submission_dir: "Optional[Path]" = None
        if self.shared_graph is not None:
            try:
                existing_events = self.shared_graph.events()
                self._last_flag_submission_seq = max(
                    (int(row.get("seq") or 0) for row in existing_events),
                    default=0,
                )
            except Exception:
                pass
        # Ordered unresolved calls. A result with an id consumes only its exact match;
        # an id-less result consumes the oldest call. The FIFO fallback matters because
        # some engine versions expose execution identity on only the tool side.
        self._pending_tool_calls: "list[tuple[str, str]]" = []
        # Live stream activity counters — used to price a floor COST_UPDATE when the
        # CLI is killed before a final usage block / CliResult can be salvaged
        # (NYU-AB metering gap: ~900s FAILs with dozens of tools but cost_usd=0).
        self._stream_tool_starts = 0
        self._stream_reasoning_chars = 0
        self._stream_cost_flushed = False
        self._reasoning_turn_acc = ""
        # ── submission gate (only meaningful when challenge.verifier_rate_limited) ──
        # _submit_blocked_until: epoch-secs deadline before which a SIBLING holds the
        #   global submit-lock → this worker should HOLD its own submission. Advisory
        #   (it only changes the prompt; the worker keeps reconning/refining). Set on
        #   a SUBMIT_LOCKED broadcast with a self-clearing lease so a stuck lock can't
        #   freeze the swarm.
        # _verifier_locked_until: epoch-secs deadline before which NO submission may
        #   happen (a real cooldown/burn-lockout), parsed from a broadcast
        #   VERIFIER_LOCKED. Stronger than _submit_blocked_until.
        # Both written from the monitor thread (_drain_control) and read by the prompt
        # builder at a turn boundary; plain floats are fine (no compound invariant).
        self._submit_blocked_until = 0.0
        self._verifier_locked_until = 0.0
        # how long a sibling's SUBMIT_LOCKED holds us off before self-clearing.
        self._SUBMIT_HOLD_S = 90.0
        # ── f10 edge-cognition worker shell loop (OFF by default) ──────────────
        # Enabled two ways, both opt-in so every other framework and the default
        # single-shot path are byte-identical: (a) an explicit shell_envelope
        # kwarg (tests / direct construction), or (b) a "[f10-shell-v1]{json}"
        # IntentEnvelope marker line inside standing_guidance, injected by
        # SwarmF10.framework_worker_guidance_for_intent — the only per-worker
        # channel the coordinator exposes to frameworks. Only meaningful for the
        # solve modes; respond/review never enter the loop.
        self._shell_envelope: Optional[dict] = (
            dict(shell_envelope) if isinstance(shell_envelope, dict) else None)
        if self._shell_envelope is None:
            for _g in self._standing_guidance:
                _env = self._parse_shell_envelope_marker(_g)
                if _env:
                    self._shell_envelope = _env
                    break
        self._shell_loop = bool(self._shell_envelope) and self.mode in (
            "bootstrap", "explore", "shell")

    @staticmethod
    def _parse_shell_envelope_marker(line: Any) -> Optional[dict]:
        """Parse one standing-guidance line into an f10 IntentEnvelope (None when
        it is not a shell marker). Import is lazy so the default path never
        touches the frameworks package."""
        try:
            from muteki.frameworks.f10_edge_cognition.state import (
                parse_envelope_guidance,
            )
            return parse_envelope_guidance(line)
        except Exception:
            return None

    def _redact_control_secrets(self, text: Any) -> Any:
        if not isinstance(text, str) or not text:
            return text
        redacted = text
        exact_tokens: set[str] = set()
        component_tokens: set[str] = set()
        for raw in self._control_secret_values:
            value = str(raw or "")
            if not value:
                continue
            # Always protect the exact materialized value.
            exact_tokens.add(value)
            # Workers often echo only the credential component rather than the
            # operator's full ``password=...`` or URL. Derive only sufficiently
            # distinctive components to avoid corrupting ordinary prose.
            for match in re.finditer(
                r"(?i)(?:password|passwd|token|secret|credential|api[_ -]?key|"
                r"private[_ -]?key|bearer)\s*(?::|=|\bis\b)\s*([^\s,;]+)",
                value,
            ):
                component = match.group(1).strip("'\"()[]{}")
                if component:
                    component_tokens.add(component)
            try:
                parsed = urlsplit(value)
            except ValueError:
                parsed = None
            if parsed is not None and parsed.netloc:
                for component in (parsed.username, parsed.password):
                    if component:
                        for candidate in {component, unquote(component)}:
                            if candidate:
                                component_tokens.add(candidate)
        values = sorted(exact_tokens | component_tokens, key=len, reverse=True)
        for value in values:
            if len(value) >= 6:
                redacted = redacted.replace(
                    value, "<REDACTED_OPERATOR_SECRET>")
            else:
                # Short exact values (notably decision answers such as "A"/"1")
                # and short parsed credentials are still secrets, but global
                # substring replacement would destroy unrelated telemetry. Match a
                # standalone token so the value is hidden without turning ACTIVE,
                # phase-1, or every ordinary word into redaction noise.
                redacted = re.sub(
                    rf"(?<![A-Za-z0-9_]){re.escape(value)}(?![A-Za-z0-9_])",
                    "<REDACTED_OPERATOR_SECRET>", redacted,
                )
        return redacted

    def _redact_control_secret_payload(self, value: Any) -> Any:
        if isinstance(value, str):
            return self._redact_control_secrets(value)
        if isinstance(value, dict):
            return {
                key: self._redact_control_secret_payload(child)
                for key, child in value.items()
            }
        if isinstance(value, list):
            return [self._redact_control_secret_payload(child) for child in value]
        if isinstance(value, tuple):
            return tuple(
                self._redact_control_secret_payload(child) for child in value)
        return value

    _PROTOCOL2_PRIVATE_CONTENT_EVENTS = frozenset({
        EventType.TEXT_MESSAGE_DELTA,
        EventType.REASONING_DELTA,
        EventType.TOOL_CALL_START,
        EventType.TOOL_CALL_ARGS,
        EventType.TOOL_CALL_RESULT,
        EventType.TERMINAL_OUTPUT,
        EventType.SOLVE_GRAPH_DELTA,
        EventType.INSIGHT_BUS_EVENT,
        EventType.SHARED_GRAPH_DELTA,
        EventType.BLACKBOARD_DELTA,
        EventType.NODE_SUMMARIZED,
        EventType.HITL_REQUEST,
    })

    async def _emit(self, etype: EventType, **payload: Any) -> None:
        if (bool(getattr(self, "_protocol2_mode", False))
                and etype in self._PROTOCOL2_PRIVATE_CONTENT_EVENTS):
            # Protocol 2 provider/terminal content is private capture input. It can
            # contain an accepted value before the canonical gate commits, so no
            # legacy chat, tool, graph, insight, blackboard, or equivalent content
            # projection may persist it. Lifecycle/cost telemetry remains public;
            # accepted values are projected only as reconciled typed flag.accepted.
            return
        if self.bus is not None:
            await self.bus.emit(Event(
                event_type=etype, run_id=self.run_id,
                challenge_id=self.challenge.id, solver_id=self.solver_id,
                payload=self._redact_control_secret_payload(payload),
            ))

    def _decision_request_identity(self, need: str, need_kind: str) -> tuple[str, dict]:
        """Return one stable id + correlation payload for this execution occurrence."""
        key = (str(need), str(need_kind))
        request_id = self._decision_request_ids.get(key)
        execution_id = str(
            self._cli_session or self.resume_session
            or f"attempt:{self._execution_occurrence}"
        )
        if request_id is None:
            request_id = stable_decision_request_id(
                run_id=self.run_id,
                worker_id=self.solver_id,
                prompt=str(need),
                kind=str(need_kind),
                correlation_key=str(
                    getattr(self, "_intent_id", "") or self.intent_id_assigned or self.mode
                ),
                execution_id=execution_id,
                execution_occurrence=self._execution_occurrence,
                resolve_epoch=self._resolve_epoch,
            )
            self._decision_request_ids[key] = request_id
        return request_id, {
            "execution_id": execution_id,
            "execution_occurrence": self._execution_occurrence,
            "resolve_epoch": self._resolve_epoch,
        }

    async def _emit_bb(self, kind: str, **fields: Any) -> None:
        """One blackboard.delta — the swarm's collaboration layer (intent claim
        lifecycle / facts / dead-ends / flag) that the OneNote board renders."""
        await self._emit(
            EventType.BLACKBOARD_DELTA,
            **blackboard_delta_payload(kind, actor=self.solver_id, **fields))

    def _record_intent_db(self, goal: str) -> None:
        """P1-B: register a whole-challenge (bootstrap/re-bootstrap) intent in the DB
        intents table + claim it. Without this, only explore intents lived in the
        table, so _attempted_intents_block (status='done' query) never saw the
        whole-challenge attempts that are the bulk of "重走老路". Best-effort —
        a DB hiccup must never disturb a solve."""
        if self.shared_graph is None:
            return
        try:
            self.shared_graph.propose_intent(
                actor=self.solver_id, intent_id=self._intent_id, goal=goal)
            self.shared_graph.claim_intent(
                worker=self.solver_id, intent_id=self._intent_id)
        except Exception:
            pass

    def _conclude_intent_db(self, *, result: str,
                            to_fact_seq: "Optional[int]" = None,
                            result_detail: str = "") -> None:
        """P1-B: conclude this worker's intent in the DB intents table (status='done'
        + result text) so the NEXT bootstrap worker's board shows this direction was
        already attempted and what came of it. Best-effort + owner-fenced inside
        conclude_intent."""
        if self.shared_graph is None:
            return
        if self._control_context_delivery_unknown and not result_detail:
            # Popen already received argv, so an unconfirmed journal commit is a
            # terminal disclosure-unknown attempt, not a safe-to-retry pre-start
            # failure. A new operator command can create fresh context without
            # replaying this one-shot material.
            result_detail = (
                "required operator context delivery became unknown after process start")
        result = self._redact_control_secrets(result)
        result_detail = self._redact_control_secrets(result_detail)
        try:
            self.shared_graph.conclude_intent(
                actor=self.solver_id, intent_id=self._intent_id,
                result=result, to_fact_seq=to_fact_seq,
                result_detail=result_detail)
        except Exception:
            pass

    async def _emit_finished(self, *, flag: Optional[str], solved: bool,
                             flags: Optional[list[str]] = None) -> None:
        """Emit this solver's terminal lifecycle event, scoped correctly.

        scope="run"    → RUN_FINISHED (this solver IS the run: mock / race / standby).
        scope="worker" → WORKER_FINISHED (a swarm sub-worker; the coordinator owns the
                         single run-level RUN_FINISHED emitted when its loop exits).
        Payload carries `flag` (first, back-compat) + `flags` (all, multi-flag) +
        `solved`. Worker-scoped events also carry `reason` (timeout / oom /
        cancelled / steered / ...). Run-scoped RUN_FINISHED keeps its own reason
        vocabulary and is not overwritten here."""
        etype = (EventType.RUN_FINISHED if self.lifecycle_scope == "run"
                 else EventType.WORKER_FINISHED)
        # Protocol 2 publishes accepted values only through its typed reconciliation
        # layer.  The lifecycle may still report completion, but it must not become an
        # additional public flag projection.
        public_flag = None if bool(getattr(self, "_protocol2_mode", False)) else flag
        public_flags = (
            [] if bool(getattr(self, "_protocol2_mode", False))
            else list(flags or ([flag] if flag else []))
        )
        stop_reason = (
            self._worker_stop_reason
            or ("solved" if solved else "finished")
        ).strip()
        if stop_reason:
            self._note_worker_stop(stop_reason)
        # I: granular lifecycle — the worker has exited (with its final token total).
        await self._emit_lifecycle("exited", solved=solved)
        finished_payload: dict[str, Any] = {
            "flag": public_flag, "flags": public_flags, "solved": solved,
        }
        if etype is EventType.WORKER_FINISHED:
            finished_payload["reason"] = stop_reason
        await self._emit(etype, **finished_payload)

    async def _emit_worker_status(
        self, *, online: bool, reason: str, status: Optional[str] = None,
    ) -> None:
        await self._emit(
            EventType.WORKER_STATUS,
            **worker_status_payload(
                online,
                status=status or ("online" if online else "offline"),
                reason=reason,
                engine=self.driver.name,
                session=self._cli_session or "",
                runtime=self._last_runtime_status if not online else None,
                worker_role=self.mode,
                identity=self.identity or None,
            ))

    def _tokens_spent(self) -> int:
        """Best-effort running token total for THIS worker (in+out), for the
        lifecycle telemetry. Reads the cost ledger's per-solver snapshot if present."""
        try:
            if self.cost is not None and hasattr(self.cost, "snapshot"):
                snap = self.cost.snapshot() or {}
                by = snap.get("by_solver") or snap.get("bySolver") or {}
                row = by.get(self.solver_id) or {}
                return int(row.get("tokens_in", 0) or 0) + int(row.get("tokens_out", 0) or 0)
        except Exception:
            pass
        return 0

    async def _emit_lifecycle(self, phase: str, *, paused: bool = False,
                              **extra: Any) -> None:
        """I: emit a granular worker-lifecycle event (spawned/phase_changed/
        stalled/exited). Best-effort; never disturbs the solve."""
        if phase == "stalled" and getattr(self, "_stalled_at", None) is None:
            self._stalled_at = time.monotonic()
        try:
            await self._emit(
                EventType.WORKER_LIFECYCLE,
                **worker_lifecycle_payload(
                    phase,
                    intent_id=getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or "",
                    tokens_spent=self._tokens_spent(),
                    paused=paused,
                    engine=self.driver.name, worker_role=self.mode,
                    **{**self.identity, **extra}))
        except Exception:
            pass

    async def _note_cli_session(self, session: Optional[str]) -> None:
        """Record the worker's live CLI session id and (if it just became known)
        re-emit worker status so the deck can show the resume id for manual
        intervention. Idempotent — only emits when the id actually changes."""
        if not session or session == self._cli_session:
            return
        self._cli_session = session
        try:
            await self._emit_worker_status(online=True, reason="started")
        except Exception:
            pass

    def _note_worker_stop(self, reason: str) -> None:
        if reason:
            self._worker_stop_reason = reason

    def _blackboard_script_path(self) -> str:
        # Container: baked into the worker image at /usr/local/bin/blackboard.py
        # (docker/worker/blackboard.py), kept fresh by image rebuilds.
        if self.container is not None:
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

    def _worker_env(self, cwd: Optional[str] = None) -> dict:
        """Env vars handed to the worker subprocess.

        The PRIMARY board mechanism is coordinator-driven: the board
        snapshot is injected into the prompt and facts are extracted from output
        (works for any engine, no DB access needed). That's the reliable path and
        the reason the live tests show facts flowing.

        We ALSO expose MUTEKI_BLACKBOARD_DB so the muteki-blackboard skill works for
        a worker that reaches for it (observed: both claude and codex try to read the
        live board mid-solve — claude via the skill, codex via raw sqlite). The skill
        gives a FRESHER read than the prompt snapshot (teammates' newest facts), so
        it complements injection rather than replacing it. Without this var the skill
        exits non-zero (it can't find the DB) — a dangling failure we avoid."""
        # The host-owned invocation runner consumes this environment directly.
        # Merge driver-level model variables here so custom Claude endpoints use
        # the same model selection in both standard and host-owned execution.
        from muteki.solver.worker_skills import stage_blackboard_skill

        skill_workdir = cwd or self._workdir
        if skill_workdir:
            stage_blackboard_skill(
                skill_workdir,
                engine=self.driver.name,
                container=self.container is not None,
            )

        env_extra = getattr(self.driver, "env_extra", None)
        driver_env = env_extra() if callable(env_extra) else {}
        env = {**driver_env, **self._extra_worker_env}
        state_root = Path(cwd or self._workdir).resolve() if (cwd or self._workdir) else None
        if self.driver.name == "opencode" and state_root is not None:
            opencode_root = state_root / ".muteki-opencode"
            for dirname in ("data", "config", "cache"):
                (opencode_root / dirname).mkdir(parents=True, exist_ok=True)
            runtime_root = str(opencode_root)
            mapper = getattr(self.container, "to_container_path", None)
            if callable(mapper):
                runtime_root = mapper(runtime_root)
            env.setdefault("XDG_DATA_HOME", f"{runtime_root}/data")
            env.setdefault("XDG_CONFIG_HOME", f"{runtime_root}/config")
            env.setdefault("XDG_CACHE_HOME", f"{runtime_root}/cache")
        if self.driver.name == "dsh" and state_root is not None:
            sessions = state_root / ".muteki-dsh-sessions"
            sessions.mkdir(parents=True, exist_ok=True)
            runtime_sessions = str(sessions)
            runtime_cwd = str(state_root)
            mapper = getattr(self.container, "to_container_path", None)
            if callable(mapper):
                runtime_sessions = mapper(runtime_sessions)
                runtime_cwd = mapper(runtime_cwd)
            env.setdefault("DSH_SESSION_ROOT", runtime_sessions)
            env.setdefault("DSH_CWD", runtime_cwd)
            env.setdefault("DSH_TELEMETRY_DISABLED", "1")
        profile = getattr(self.driver, "profile", None)
        if (
            self.driver.name == "claude"
            and isinstance(profile, dict)
            and profile_uses_endpoint(profile)
        ):
            # Claude Code loads ~/.claude/settings.json after process launch. A
            # host-level ANTHROPIC_BASE_URL / ANTHROPIC_MODEL there can replace
            # the selected Worker's custom endpoint even though the subprocess
            # received the correct environment. Keep endpoint-backed Workers in a
            # per-worker config directory; subscription Workers continue to use
            # the operator's normal Claude configuration.
            config_root = cwd or self._workdir
            if config_root:
                config_host = Path(config_root).resolve() / ".muteki-claude-config"
                config_host.mkdir(parents=True, exist_ok=True)
                if self.container is not None:
                    # Claude creates `session-env/` below CLAUDE_CONFIG_DIR before
                    # its Bash/Write tools run.  This directory is also created
                    # after the run-level workspace chown, so make the late path
                    # writable by the container worker as well.
                    from muteki.solver.container_exec import _chown_tree_to_worker

                    _chown_tree_to_worker(
                        str(config_host), image=self.container.image
                    )
                config_path = str(config_host)
                mapper = getattr(self.container, "to_container_path", None)
                if callable(mapper):
                    try:
                        config_path = mapper(config_path)
                    except Exception:
                        pass
                env["CLAUDE_CONFIG_DIR"] = config_path
        env["PATH"] = _stable_worker_path(env.get("PATH") or os.environ.get("PATH", ""))
        env["MUTEKI_WORKER_ID"] = self.solver_id
        intent_id = getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or ""
        if intent_id:
            env["MUTEKI_INTENT_ID"] = intent_id
        env["MUTEKI_BLACKBOARD_SCRIPT"] = self._blackboard_script_path()
        if cwd:
            submission_host = Path(cwd).resolve() / ".muteki-flag-submissions"
            submission_host.mkdir(parents=True, exist_ok=True)
            if self.container is not None:
                # The run workspace is chowned when the container is created, but
                # this per-Worker ingress directory is created later, immediately
                # before the CLI starts.  A root-owned directory prevents the
                # container's unprivileged `kali` user from atomically writing a
                # submission request.  Align this late-created path with the image's
                # actual worker uid/gid just like the rest of the mounted workspace.
                from muteki.solver.container_exec import _chown_tree_to_worker

                _chown_tree_to_worker(
                    str(submission_host), image=self.container.image
                )
            self._flag_submission_dir = submission_host
            submission_path = str(submission_host)
            mapper = getattr(self.container, "to_container_path", None)
            if callable(mapper):
                try:
                    submission_path = mapper(submission_path)
                except Exception:
                    pass
            env["MUTEKI_FLAG_SUBMISSION_DIR"] = submission_path
        db = getattr(self.shared_graph, "db_path", None)
        if db and not bool(getattr(self, "_protocol2_mode", False)):
            # ABSOLUTE path: the worker subprocess runs with cwd=<its own workdir>,
            # so a relative db_path would resolve against the wrong dir and the
            # blackboard skill / raw sqlite would hit "unable to open database file"
            # (observed run-7352). abspath resolves against OUR cwd, which is correct.
            db_path = os.path.abspath(str(db))
            mapper = getattr(self.container, "to_container_path", None)
            if callable(mapper):
                try:
                    db_path = mapper(db_path)
                except Exception:
                    pass
            env["MUTEKI_BLACKBOARD_DB"] = db_path
        return env

    @staticmethod
    def _signal_proc(proc: Any, sig: int) -> bool:
        """Send `sig` to a worker's whole PROCESS GROUP (the CLI agent spawns
        curl/python/sh helpers; signalling only the parent leaves them running —
        the deeper form of bug #2). The subprocess is a group leader because the
        runner starts it with start_new_session=True. Falls back to the bare pid /
        proc.kill() when the group send isn't available (e.g. test fakes)."""
        # Container backend: the worker runs via `docker exec`, so its real pgid is
        # INSIDE the container — a host-side os.killpg on the docker-exec client pid
        # would not reach it. A _ContainerProc exposes _container_signal to route the
        # signal in via `docker exec kill`. Prefer it when present.
        cont_sig = getattr(proc, "_container_signal", None)
        if callable(cont_sig):
            try:
                # RCP returns an explicit delivery/verification bool. Legacy
                # container wrappers predate that contract and return None after a
                # successful synchronous docker call, which remains accepted.
                return cont_sig(sig) is not False
            except Exception:
                return False
        local_owner = getattr(proc, "_muteki_process_owner", None)
        owner_signal = getattr(local_owner, "signal", None)
        if callable(owner_signal):
            try:
                return owner_signal(sig) is not False
            except Exception:
                return False
        pid = getattr(proc, "pid", None)
        if pid is not None:
            try:
                os.killpg(os.getpgid(pid), sig)
                return True
            except Exception:
                pass
            try:
                os.kill(pid, sig)
                return True
            except Exception:
                pass
        # last resort for SIGKILL: Popen.kill() (covers test fakes with no pid)
        if sig == getattr(signal, "SIGKILL", -9):
            try:
                proc.kill()
                return True
            except Exception:
                pass
        return False

    def cancel(self) -> bool:
        """Stop this worker NOW. Sets the cancel flag (the streaming runner's
        watcher kills the subprocess) and force-kills any live subprocess group
        directly — so a winning sibling actually stops this one (bug #2), not just
        cancels the asyncio task while the CLI agent keeps running."""
        self._cancel_event.set()
        with self._procs_lock:
            procs = list(self._live_procs)
        if not procs:
            # The durable cancel flag fences any future subprocess registration.
            return True
        results = [
            self._signal_proc(p, getattr(signal, "SIGKILL", 9))
            for p in procs
        ]
        return all(results)

    def _notify_context_delivery(self, ok: bool) -> None:
        callback = None
        with self._context_delivery_lock:
            callback = self._context_delivery_callback
            self._context_delivery_callback = None
        if callable(callback):
            try:
                callback(bool(ok))
            except Exception:
                pass

    def _context_delivery_reservation_snapshot(self) -> tuple[tuple[str, str], ...]:
        """Capture the reservation batch owned by one prompt transport attempt."""
        with self._context_delivery_lock:
            return tuple(self._pending_control_context_reservations)

    def _commit_control_context_delivery(
        self, *, actor: str,
        reservations: "Optional[tuple[tuple[str, str], ...]]" = None,
    ) -> bool:
        """Commit reservations only after their actual transport boundary.

        Ordinary prompts cross that boundary at Popen because the prompt is already
        in argv. Secret-bearing prompts cross it only after the stdin writer (or the
        acknowledged remote supervisor) accepts the complete payload.
        """
        with self._context_delivery_lock:
            current = list(self._pending_control_context_reservations)
            pending = list(
                reservations if reservations is not None else tuple(current))
            binding_worker_id = str(
                self._context_binding_worker_id or self.solver_id)
            if not pending:
                # An explicitly captured empty batch is valid only while no later
                # reservation has appeared.  More importantly, a late callback for
                # a non-empty batch never turns into success merely because another
                # owner prematurely cleared the mutable pending list.
                confirmed = (
                    not current and not self._control_context_delivery_unknown)
            else:
                committer = self._context_committer
                confirmed = callable(committer)
                if confirmed:
                    for context_id, reservation_id in pending:
                        try:
                            if not committer(
                                context_id, worker_id=binding_worker_id,
                                reservation_id=reservation_id,
                            ):
                                confirmed = False
                                break
                        except Exception:
                            confirmed = False
                            break
                if confirmed:
                    consumed = set(pending)
                    self._pending_control_context_reservations = [
                        reservation for reservation in current
                        if reservation not in consumed
                    ]
                    self._control_context_delivery_committed = True
        if confirmed:
            self._notify_context_delivery(True)
            return True
        self._mark_control_context_delivery_unknown(
            actor=actor,
            reason="delivery commit unavailable after prompt transport",
            reservations=tuple(pending),
        )
        return False

    def _mark_control_context_delivery_unknown(
        self, *, actor: str, reason: str,
        reservations: "Optional[tuple[tuple[str, str], ...]]" = None,
    ) -> None:
        with self._context_delivery_lock:
            current = list(self._pending_control_context_reservations)
            pending = list(
                reservations if reservations is not None else tuple(current))
            binding_worker_id = str(
                self._context_binding_worker_id or self.solver_id)
            marker = self._context_delivery_unknown_marker
            if callable(marker):
                for context_id, reservation_id in pending:
                    try:
                        marker(
                            context_id, worker_id=binding_worker_id,
                            reservation_id=reservation_id,
                            actor=actor, reason=reason,
                        )
                    except Exception:
                        pass
            # Even if the journal marker failed, a post-transport reservation must
            # remain stranded for recovery; generic cleanup may never replay it.
            consumed = set(pending)
            self._pending_control_context_reservations = [
                reservation for reservation in current
                if reservation not in consumed
            ]
            if pending:
                self._control_context_delivery_unknown = True
        self._cancel_event.set()
        self.cancel()
        self._notify_context_delivery(False)

    def _on_proc(
        self, proc: Any, *, context_via_stdin: bool = False,
        context_reservations: "Optional[tuple[tuple[str, str], ...]]" = None,
    ) -> None:
        """Called by the streaming runner with the live Popen — track it so cancel()
        and the pause monitor can signal it. If a cancel already fired before the
        subprocess registered, kill it immediately. If the operator PAUSED this worker
        (M8) before this subprocess started — e.g. the pause arrived during the gap
        between the execute pass and the conclude-fallback subprocess — freeze the new
        process too, so pause state doesn't silently leak across the turn boundary and
        let a paused worker keep running."""
        self._runtime_process_started = True
        with self._procs_lock:
            self._live_procs.add(proc)
        runner_procs = getattr(self._runner_proc_local, "procs", None)
        if isinstance(runner_procs, set):
            runner_procs.add(proc)
        if not context_via_stdin:
            self._commit_control_context_delivery(
                actor="cli-popen-commit", reservations=context_reservations)
            if (
                self._c6_invocation_runner is None
                and not self._record_cognitive_context_release(proc)
            ):
                raise RuntimeError("CognitiveContextReleaseUnconfirmed")
        if self._cancel_event.is_set():
            self._signal_proc(proc, getattr(signal, "SIGKILL", 9))
        elif self._paused:
            sig = getattr(signal, "SIGSTOP", None)
            if sig is not None:
                self._signal_proc(proc, sig)

    def _on_proc_start_uncertain(
        self,
        reservations: "Optional[tuple[tuple[str, str], ...]]" = None,
        *,
        mark_c6_unknown: bool = True,
    ) -> None:
        """Fail closed when a remote StartWorker was sent but no started ACK arrived.

        The supervisor may already have spawned the prompt-carrying process. Without
        a worker id there is no honest Popen binding or targeted kill proof, so mark
        every pending disclosure unknown and prevent generic pre-start cleanup from
        replaying it.
        """
        # Dispatch crossed the remote supervisor boundary.  Even without a started
        # ACK, a child may exist, so this intent is never pre-start replay-safe.
        self._runtime_process_started = True
        self._remote_start_uncertain = True
        self._mark_control_context_delivery_unknown(
            actor="rcp-start-uncertain",
            reason="remote StartWorker dispatched without started ACK",
            reservations=reservations,
        )
        if mark_c6_unknown:
            self._mark_cognitive_context_stage_unknown(
                reason="worker start crossed a remote boundary without an exact ACK",
            )

    def _set_paused(self, paused: bool) -> bool:
        """SIGSTOP/SIGCONT every live subprocess GROUP — a genuine freeze (same
        PIDs), not a kill. POSIX only; on platforms without SIGSTOP it no-ops."""
        sig = getattr(signal, "SIGSTOP", None) if paused else getattr(signal, "SIGCONT", None)
        with self._procs_lock:
            procs = list(self._live_procs)
        # No child exists yet: recording the desired flag is itself sufficient;
        # _on_proc applies it before the future subprocess can do work.
        if not procs:
            self._paused = paused
            if paused:
                self._paused_event.set()
            else:
                self._paused_event.clear()
            return True
        if sig is None:
            return False
        signalled: list[Any] = []
        for proc in procs:
            if self._signal_proc(proc, sig):
                signalled.append(proc)
                continue
            # All-or-nothing confirmation: compensate every process already
            # signalled, leaving the prior logical state intact.
            inverse = (getattr(signal, "SIGCONT", None) if paused
                       else getattr(signal, "SIGSTOP", None))
            if inverse is not None:
                for prior in signalled:
                    self._signal_proc(prior, inverse)
            return False
        self._paused = paused
        if paused:
            self._paused_event.set()
        else:
            self._paused_event.clear()
        return True

    def _enable_in_turn_steer(self) -> bool:
        """P2: whether a teammate's new flag / an operator correction may
        END the current turn (via _steer_event) so the worker re-plans immediately
        instead of racing the same flag / ignoring the correction for the rest of a
        long turn. A steer only ends the CURRENT turn cleanly (session kept), it
        never cancels the worker.

        Gated on _turn_active: a steer is ONLY valid while a subprocess turn is
        actually running. A freshly-spawned worker drains the InsightBus history
        backlog (prior FLAGs + standing hints) before/between turns — those must be
        CONSUMED (folded into _already_found / _standing_guidance) but must NOT
        steer-kill the not-yet-started subprocess (run-40726 regression). Disabled
        entirely by a `_no_in_turn_steer` attr (tests / special modes)."""
        if getattr(self, "_no_in_turn_steer", False):
            return False
        return bool(getattr(self, "_turn_active", False))

    def _stop_on_sibling_flag(self) -> bool:
        """Whether a sibling FlagFound should end this worker's current pass.

        Only the legacy single-flag path does this. Multi-flag collection keeps
        live workers running until ALL_FLAGS_FOUND, while still recording each peer
        flag in _already_found so prompts avoid re-hunting it.
        """
        if bool(getattr(self.challenge, "multi_flag", False)):
            return False
        return self._expected_flags() <= 1

    def _drain_control(self) -> None:
        """Drain the InsightBus inbox for HITL commands + sibling FLAG. Runs from
        the monitor thread (the worker has no between-turn point like base Solver).

        pause→SIGSTOP, resume→SIGCONT, sibling FLAG→record it (single-flag may
        still end the current pass). Operator
        Non-standing hint/focus guidance is recorded for later prompts but does NOT
        interrupt a healthy live worker. A redirect is the explicit interrupting
        correction: it ends the current pass via _steer_event (gated on an active
        turn). `standing` guidance (VPS/SSH creds) is held separately and injected
        into every future worker's prompt without killing the current turn."""
        inbox = self._insight_inbox
        if inbox is None:
            return
        from muteki.swarm.insight_bus import InsightKind
        while True:
            try:
                ins = inbox.get_nowait()
            except Exception:
                break
            try:
                if ins.kind is InsightKind.GUIDANCE:
                    target = getattr(ins, "target", "global") or "global"
                    intent_id = str(
                        getattr(self, "intent_id_assigned", "")
                        or getattr(self, "_intent_id", "") or "")
                    lane = str(getattr(self, "lane", "") or "")
                    engine = str(getattr(self, "engine", "") or "")
                    eligible_targets = {
                        "global", self.solver_id,
                        f"solver:{self.solver_id}", f"worker:{self.solver_id}",
                    }
                    if engine:
                        eligible_targets.add(f"engine:{engine}")
                    if intent_id:
                        eligible_targets.update({intent_id, f"intent:{intent_id}"})
                    if lane:
                        eligible_targets.update({lane, f"lane:{lane}"})
                    if target not in eligible_targets:
                        continue
                    act = (getattr(ins, "action", "hint") or "hint").lower()
                    if act == "pause":
                        self._set_paused(True)
                    elif act == "resume":
                        self._set_paused(False)
                    elif act in ("hint", "redirect", "focus"):
                        text = getattr(ins, "text", "") or ""
                        url = getattr(ins, "url", "") or ""
                        standing = bool(getattr(ins, "standing", False))
                        self._remember_operator_context(text)
                        self._remember_operator_context(url)
                        if standing:
                            # persistent background guidance — held, not consumed.
                            with self._guidance_lock:
                                if text and text not in self._standing_guidance:
                                    self._standing_guidance.append(text)
                                if url:
                                    self._target_override = url
                            # Standing guidance is persistent background context
                            # (VPS/SSH creds, global constraints). Killing a live
                            # single-shot worker here makes a run that posts standing
                            # guidance just after race start empty-exit every worker.
                            # Keep it for prompts; non-standing hint/redirect/focus
                            # remain the explicit steer/correction path below.
                        else:
                            # A normal hint/focus is additive guidance. Killing every
                            # live worker on a hint makes the deck look frozen and can
                            # chain-kill freshly spawned workers when the InsightBus
                            # history is replayed. Record it for the next prompt (and
                            # any conclude fallback), but only `redirect` is an explicit
                            # "current target/path is wrong; restart now" interruption.
                            with self._guidance_lock:
                                if text and text not in self._standing_guidance:
                                    self._standing_guidance.append(text)
                                if url:
                                    self._target_override = url
                            if act == "redirect" and self._enable_in_turn_steer():
                                self._steer_event.set()
                elif ins.kind is InsightKind.FLAG:
                    # a sibling found a flag. Multi-flag: DON'T stop — just note it
                    # so we don't re-hunt the same one (and let the worker prompt's
                    # "already found" list stay accurate). We only die on the
                    # ALL_FLAGS_FOUND signal below.
                    txt = getattr(ins, "text", "") or ""
                    if txt and txt not in self._already_found:
                        self._already_found.add(txt)
                        # Single-flag compatibility: the first sibling flag can still
                        # end this pass. Multi-flag workers stay alive until the
                        # explicit ALL_FLAGS_FOUND signal; killing them on every
                        # partial FlagFound collapsed a 4-flag race to one worker.
                        if self._stop_on_sibling_flag() and self._enable_in_turn_steer():
                            self._steer_event.set()
                elif ins.kind is InsightKind.ALL_FLAGS_FOUND:
                    # the run collected every expected flag — stop wasting budget.
                    self.cancel()
                elif ins.kind is InsightKind.FACT or ins.kind is InsightKind.DEAD_END:
                    # A teammate confirmed a fact / ruled out a direction mid-turn.
                    # We do NOT fold it here: a teammate's verified fact is written to
                    # the shared graph by _record_fact (shared_graph.add_evidence), and
                    # every worker's next-turn prompt carries the FULL board via
                    # _board_markdown() → to_board_markdown() (facts + ruled-out, no
                    # truncation). The InsightBus FACT/DEAD_END event is a redundant
                    # second channel; consuming it into a per-worker prompt buffer was
                    # dead (the renderer had 0 callers). The board IS the channel.
                    pass
                elif ins.kind is InsightKind.SUBMIT_LOCKED:
                    # a sibling holds the global submit-lock → don't submit now. We do
                    # NOT pause the process: the worker keeps reconning / refining its
                    # answer; the next turn's prompt tells it to hold submission. A
                    # self-clearing lease guarantees a stuck lock can't freeze us.
                    self._submit_blocked_until = max(
                        self._submit_blocked_until, time.time() + self._SUBMIT_HOLD_S)
                elif ins.kind is InsightKind.SUBMIT_UNLOCKED:
                    # the holder released early → re-open submission immediately
                    # (a verifier lockout, if any, is tracked separately and still binds).
                    self._submit_blocked_until = 0.0
                elif ins.kind is InsightKind.VERIFIER_LOCKED:
                    # the verifier hit a cooldown/burn-lockout → nobody submits until
                    # it elapses. Record the absolute deadline so the prompt can show
                    # the remaining time and the worker spends it improving the answer.
                    try:
                        secs = float(getattr(ins, "text", "0") or 0)
                    except (TypeError, ValueError):
                        secs = 0.0
                    if secs > 0:
                        self._verifier_locked_until = max(
                            self._verifier_locked_until, time.time() + secs)
            except Exception:
                continue

    def _apply_runtime_argv(self, argv: list[str], env: dict) -> list[str]:
        """Apply profile/runtime options that must be command-line flags."""
        out = apply_runtime_argv(argv, driver=self.driver, env=env)
        seatbelt = str(env.get("MUTEKI_NETWORK_SANDBOX_PROFILE") or "")
        if seatbelt:
            if not bool(getattr(self, "_protocol2_mode", False)):
                raise RuntimeError("network sandbox profile is Protocol 2 only")
            if self.container is not None:
                raise RuntimeError("Darwin host network sandbox cannot wrap a container")
            sandbox_exec = "/usr/bin/sandbox-exec"
            if not os.path.isfile(sandbox_exec):
                raise RuntimeError("sandbox-exec is unavailable")
            out = [sandbox_exec, "-p", seatbelt, *out]
        return out

    @staticmethod
    def _proc_is_alive(proc: Any) -> bool:
        """Conservatively decide whether a registered child may still be alive.

        Real ``Popen`` and container handles expose ``poll``.  Unknown/fake handles
        are deliberately retained: dropping an unprovably-dead handle is precisely
        how a cancelled ``asyncio.to_thread`` runner loses its last kill boundary.
        """
        local_owner = getattr(proc, "_muteki_process_owner", None)
        owner_has_live = getattr(local_owner, "has_live_processes", None)
        if callable(owner_has_live):
            try:
                if owner_has_live():
                    return True
            except Exception:
                return True
        if hasattr(proc, "_exit_confirmed"):
            # RCP logical handles have a dedicated wire-level fence. A returned or
            # failed transport wrapper is not equivalent to the supervisor's exit
            # frame and must never prune an unknown remote owner.
            return not bool(getattr(proc, "_exit_confirmed", False))
        poll = getattr(proc, "poll", None)
        # The legacy docker-exec wrapper exposes its host Popen as _client_proc.
        # Prefer that real poll fence rather than treating the wrapper as immortal.
        if not callable(poll):
            poll = getattr(getattr(proc, "_client_proc", None), "poll", None)
        if not callable(poll):
            # Unknown logical handles without either poll, a dedicated RCP fence,
            # or this legacy transport-return proof remain conservatively live.
            return not bool(getattr(proc, "_muteki_runner_exited", False))
        try:
            return poll() is None
        except Exception:
            return True

    def _prune_finished_procs(self) -> None:
        with self._procs_lock:
            self._live_procs = {
                proc for proc in self._live_procs if self._proc_is_alive(proc)
            }

    def runtime_exit_confirmed(self) -> bool:
        """Query the hard runtime termination fence.

        True means every to_thread runner has returned AND every registered process
        handle is provably exited (``poll() is not None`` or the equivalent RCP
        transport-return proof). It deliberately remains False when only the outer
        asyncio worker task has finished.
        """
        self._prune_finished_procs()
        self._runner_tasks = {
            task for task in self._runner_tasks if not task.done()
        }
        with self._procs_lock:
            has_live_process = bool(self._live_procs)
        return not self._runner_tasks and not has_live_process

    async def wait_runtime_exit(self, timeout: Optional[float] = None) -> bool:
        """Await runtime termination without cancelling runner Tasks.

        ``timeout=None`` waits until proof exists. A finite timeout returns False;
        it never turns a kill request or an already-done wrapper into proof.
        Multiple waiters (driver cleanup plus RunManager receipt fencing) are safe.
        """
        loop = asyncio.get_running_loop()
        deadline = None if timeout is None else loop.time() + max(0.0, float(timeout))
        while True:
            if self.runtime_exit_confirmed():
                return True
            if deadline is not None:
                remaining = deadline - loop.time()
                if remaining <= 0:
                    return False
            else:
                remaining = 0.05

            pending = tuple(
                task for task in self._runner_tasks if not task.done()
            )
            wait_for = min(0.05, remaining) if deadline is not None else 0.05
            if pending:
                # asyncio.wait observes completion without propagating this waiter's
                # cancellation into the shielded runner tasks.
                await asyncio.wait(pending, timeout=wait_for)
            else:
                # A process may expose its terminal poll code shortly after runner
                # return; keep polling conservatively until the caller's deadline.
                await asyncio.sleep(wait_for)

    @staticmethod
    def _thread_cancel_cleanup_timeout() -> float:
        try:
            return max(0.01, float(os.environ.get(
                "MUTEKI_CLI_CANCEL_CLEANUP_TIMEOUT", "2")))
        except (TypeError, ValueError):
            return 2.0

    async def _to_thread_with_cancel_cleanup(
        self, func: Any, /, *args: Any, **kwargs: Any
    ) -> Any:
        """Run a blocking CLI call without orphaning it on asyncio cancellation.

        ``asyncio.to_thread`` keeps running after its awaiting task is cancelled.
        Shield the runner task, signal the real subprocess tree via ``cancel()``,
        then give the runner a bounded window to reap.  If that window expires the
        thread may still finish later, so live process handles remain registered and
        the done callback prunes them only after ``poll`` proves they exited.
        """
        runner_procs: set[Any] = set()

        def _blocking_call() -> Any:
            self._runner_proc_local.procs = runner_procs
            try:
                return func(*args, **kwargs)
            finally:
                # RCP logical handles do not expose poll(); transport return is their
                # own explicit exit-frame proof. Unknown legacy logical handles use
                # transport return, while RCP handles must never be laundered by it.
                # Handles with a real poll still require the poll result.
                for proc in tuple(runner_procs):
                    if hasattr(proc, "_exit_confirmed"):
                        continue
                    try:
                        setattr(proc, "_muteki_runner_exited", True)
                    except Exception:
                        pass
                try:
                    del self._runner_proc_local.procs
                except AttributeError:
                    pass

        runner_task = asyncio.create_task(asyncio.to_thread(_blocking_call))
        self._runner_tasks.add(runner_task)

        def _runner_done(done: "asyncio.Task[Any]") -> None:
            # Retrieve a late exception when the outer coroutine already left after
            # its bounded cancellation window; otherwise asyncio logs an unrelated
            # "Task exception was never retrieved" warning during shutdown.
            try:
                done.exception()
            except (asyncio.CancelledError, Exception):
                pass
            self._prune_finished_procs()
            self._runner_tasks.discard(done)

        runner_task.add_done_callback(_runner_done)
        try:
            result = await asyncio.shield(runner_task)
        except asyncio.CancelledError:
            # The outer task was cancelled, but shield kept the blocking runner
            # alive.  Kill its real child first, then wait briefly for thread/proc
            # teardown before propagating cancellation to the caller.
            self.cancel()
            salvaged: Any = None
            try:
                salvaged = await asyncio.wait_for(
                    asyncio.shield(runner_task),
                    timeout=self._thread_cancel_cleanup_timeout(),
                )
            except asyncio.TimeoutError:
                # A second signal is idempotent and catches a process that registered
                # just as the first snapshot was taken.  Do NOT clear its handle.
                self.cancel()
            except asyncio.CancelledError:
                # A second cancellation of this coroutine must still re-signal the
                # child; the runner remains shielded and its handle remains retained.
                self.cancel()
            except Exception:
                # The blocking runner failed while unwinding.  The original asyncio
                # cancellation remains the externally meaningful terminal cause.
                pass
            finally:
                self._prune_finished_procs()
            # NYU-AB metering gap: asyncio cancel used to discard a completed
            # CliResult, so COST_UPDATE never fired despite real tool activity.
            # Salvage tokens/cost before re-raising so receipts stay measurable.
            # If the runner outlives the cleanup window, price a floor from live
            # stream activity so zero-dollar cells cannot erase a real run.
            if salvaged is None and runner_task.done() and not runner_task.cancelled():
                try:
                    salvaged = runner_task.result()
                except Exception:
                    salvaged = None
            if isinstance(salvaged, CliResult):
                try:
                    await self._stream_cost(salvaged)
                except Exception:
                    pass
            else:
                try:
                    await self._flush_stream_activity_cost()
                except Exception:
                    pass
            raise
        finally:
            # Normal completion/exception may leave a short-lived dead Popen handle.
            # Poll before removal; never use an unconditional clear here.
            self._prune_finished_procs()
        return result

    async def _run_streaming(
        self, argv: list[str], *, cwd: str, timeout: int,
        stdin_text: "Optional[str]" = None,
        runtime_env: "Optional[dict]" = None,
        argv_is_final: bool = False,
        on_c6_process_started: "Optional[Callable[[Any], None]]" = None,
        on_c6_start_uncertain: "Optional[Callable[[], None]]" = None,
    ) -> CliResult:
        """Run a CLI worker and stream each step (think / tool call / tool result)
        to the deck live. The subprocess blocks in a worker thread; its per-line
        callback schedules processing back onto THIS event loop. With no bus the
        event emission becomes a no-op, but the same Popen/on_proc delivery fence is
        still mandatory for context disclosure and cancellation correctness.

        Runtime control: the cancel_event lets the swarm kill the subprocess (winner
        found / abort); a daemon monitor thread polls the InsightBus inbox so HITL
        pause/resume (SIGSTOP/SIGCONT) and a sibling's FLAG reach this live worker."""
        env = dict(runtime_env) if runtime_env is not None else self._worker_env(cwd)
        if not argv_is_final:
            argv = self._apply_runtime_argv(argv, env)
        loop = asyncio.get_running_loop()
        step_futures: "list[Any]" = []
        step_tail: "list[Any]" = []
        step_tasks: "set[asyncio.Task[Any]]" = set()
        step_ingress_failures: "list[BaseException]" = []
        step_ingress_lock = threading.Lock()
        step_ingress_open = True
        step_abort = False

        def on_step(step: StreamStep) -> None:
            # Called from one worker thread. Chain each coroutine to the prior step:
            # scheduling order alone does not preserve completion order when _emit_step
            # awaits a slow event sink, and attribution state must match stream order.
            nonlocal step_ingress_open
            with step_ingress_lock:
                if not step_ingress_open:
                    return
                previous = step_tail[-1] if step_tail else None

                async def emit_in_order() -> None:
                    task = asyncio.current_task()
                    if task is not None:
                        step_tasks.add(task)
                    try:
                        if step_abort:
                            return
                        if previous is not None:
                            try:
                                await asyncio.wrap_future(previous)
                            except Exception:
                                # Protocol 2 is prefix-closed at the stream-step level:
                                # once one step fails, successors cannot add more local
                                # provenance. Ordinary workers remain best effort.
                                if bool(getattr(self, "_protocol2_mode", False)):
                                    raise
                        if step_abort:
                            return
                        await self._emit_step(step)
                    finally:
                        if task is not None:
                            step_tasks.discard(task)

                pending = emit_in_order()
                try:
                    future = asyncio.run_coroutine_threadsafe(pending, loop)
                except RuntimeError as exc:
                    pending.close()
                    if bool(getattr(self, "_protocol2_mode", False)):
                        step_ingress_failures.append(exc)
                        step_ingress_open = False
                    return
                step_tail[:] = [future]
                step_futures.append(future)

        # Control queues belong to this event loop. Poll them in a loop-owned task;
        # the previous daemon thread submitted untracked drain coroutines that could
        # resume and mutate state after _run_streaming had already returned.
        monitor_stop = asyncio.Event()

        async def _monitor() -> None:
            while not monitor_stop.is_set():
                await self._drain_control_async()
                try:
                    await asyncio.wait_for(monitor_stop.wait(), timeout=0.1)
                except asyncio.TimeoutError:
                    pass

        monitor_task: "Optional[asyncio.Task[Any]]" = None
        # Drain any replayed InsightBus history while no subprocess is active. This
        # folds prior human hints into the prompt context without letting old guidance
        # kill the brand-new turn the moment it starts.
        self._turn_active = False
        self._drain_control()
        monitor_task = asyncio.create_task(_monitor())
        heartbeat_stop = asyncio.Event()

        async def _heartbeat() -> None:
            if _WORKER_HEARTBEAT_SECONDS <= 0:
                return
            beats = 0
            stalled_emitted = False
            while not heartbeat_stop.is_set():
                try:
                    await asyncio.wait_for(
                        heartbeat_stop.wait(), timeout=_WORKER_HEARTBEAT_SECONDS)
                    return
                except asyncio.TimeoutError:
                    pass
                if self._turn_active and not self._paused_event.is_set():
                    await self._emit_worker_status(
                        online=True, reason="busy", status="online")
                    # I: after ~5 consecutive busy heartbeats with no exit, flag the
                    # worker as STALLED (telemetry only — never kills it; the OODA
                    # lease loop owns reclamation). One-shot per turn.
                    beats += 1
                    if beats >= 5 and not stalled_emitted:
                        stalled_emitted = True
                        await self._emit_lifecycle("stalled")
                elif self._paused_event.is_set():
                    await self._emit_lifecycle("phase_changed", paused=True)

        heartbeat_task = asyncio.create_task(_heartbeat())
        # P2 regression guard: clear any steer left set by history-backlog replay
        # BEFORE the subprocess starts (a fresh worker drains prior FLAGs/standing
        # while _turn_active was False — those folded into context but must not
        # kill the not-yet-started subprocess), then mark the turn active so real
        # in-turn redirects/flags are valid for the duration of THIS subprocess only.
        self._steer_event.clear()
        self._turn_active = True
        self._stalled_at = None
        self._idle_repeat_steered = False
        self._current_workdir = Path(cwd).resolve()
        context_via_stdin = stdin_text is not None
        context_reservations = self._context_delivery_reservation_snapshot()

        def on_proc(proc: Any) -> None:
            self._on_proc(
                proc, context_via_stdin=context_via_stdin,
                context_reservations=context_reservations)
            if on_c6_process_started is not None:
                on_c6_process_started(proc)

        def on_stdin_delivered() -> None:
            self._commit_control_context_delivery(
                actor="cli-stdin-delivery", reservations=context_reservations)

        def on_stdin_uncertain() -> None:
            self._mark_control_context_delivery_unknown(
                actor="cli-stdin-uncertain",
                reason="prompt stdin handoff failed or did not finish",
                reservations=context_reservations,
            )
            if self._c6_invocation_runner is None:
                self._mark_cognitive_context_stage_unknown(
                    reason="prompt stdin handoff failed or did not finish",
                )

        def on_start_uncertain() -> None:
            self._on_proc_start_uncertain(
                context_reservations,
                mark_c6_unknown=self._c6_invocation_runner is None,
            )
            if on_c6_start_uncertain is not None:
                on_c6_start_uncertain()

        runner_returned = False
        result: "Optional[CliResult]" = None
        runner_failure: "Optional[tuple[BaseException, Any]]" = None
        caller_cancel: "Optional[tuple[asyncio.CancelledError, Any]]" = None
        try:
            result = await self._to_thread_with_cancel_cleanup(
                run_cli_streaming, self.driver, argv, cwd=cwd, timeout=timeout,
                on_step=on_step, env=env, cancel_event=self._cancel_event,
                on_proc=on_proc, steer_event=self._steer_event,
                on_start_uncertain=on_start_uncertain,
                on_stdin_delivered=(on_stdin_delivered
                                    if context_via_stdin else None),
                on_stdin_uncertain=(on_stdin_uncertain
                                    if context_via_stdin else None),
                paused_event=self._paused_event,
                container=self.container, stdin_text=stdin_text)
            runner_returned = True
            self._last_runtime_status = getattr(result, "runtime_status", {}) or {}
        except asyncio.CancelledError as exc:
            current = asyncio.current_task()
            outcome = (exc, exc.__traceback__)
            if current is not None and current.cancelling():
                caller_cancel = outcome
            else:
                runner_failure = outcome
        except BaseException as exc:
            runner_failure = (exc, exc.__traceback__)

        # Close ingress before the first cleanup await. The blocking runner can outlive
        # cancellation briefly, but callbacks arriving after this fence own no state.
        with step_ingress_lock:
            step_ingress_open = False
            pending_steps = tuple(step_futures)
            ingress_failure = (step_ingress_failures[0]
                               if step_ingress_failures else None)

        def abort_steps() -> None:
            # Do not cancel the run_coroutine_threadsafe proxy futures: a cancelled
            # proxy can report done before its actual loop Task has stopped. The abort
            # bit prevents late-starting callbacks from mutating state; actual Tasks are
            # the cancellation handles and the untouched proxies remain completion
            # fences for callbacks that have not registered yet.
            nonlocal step_abort
            step_abort = True
            for task in tuple(step_tasks):
                if not task.done():
                    task.cancel()
            if monitor_task is not None and not monitor_task.done():
                monitor_task.cancel()
            if not heartbeat_task.done():
                heartbeat_task.cancel()

        if caller_cancel is not None or runner_failure is not None:
            abort_steps()

        # Steers and helper tasks stop at the synchronous terminal fence. Keep the
        # workdir until callbacks retire because Cursor spill materialization needs it.
        self._turn_active = False
        monitor_stop.set()
        heartbeat_stop.set()
        if monitor_task is not None:
            monitor_task.cancel()
        heartbeat_task.cancel()

        async def retire_turn() -> "Optional[BaseException]":
            step_failure = ingress_failure
            try:
                step_results: "list[Any]" = []
                if pending_steps:
                    step_results = await asyncio.gather(
                        *(asyncio.wrap_future(f) for f in pending_steps),
                        return_exceptions=True)

                # Tasks can register after the proxy snapshot while their coroutine is
                # queued on the loop. Re-snapshot until every actual callback Task has
                # retired; repeated caller cancellation re-enters abort_steps().
                while True:
                    live_steps = tuple(
                        task for task in step_tasks if not task.done())
                    if not live_steps:
                        break
                    if step_abort:
                        for task in live_steps:
                            task.cancel()
                    await asyncio.gather(*live_steps, return_exceptions=True)

                if (step_failure is None and runner_returned
                        and bool(getattr(self, "_protocol2_mode", False))):
                    step_failure = next(
                        (item for item in step_results
                         if isinstance(item, BaseException)), None)
                return step_failure
            finally:
                self._current_workdir = None
                # A steer is scoped to one turn; cancel remains permanent.
                self._steer_event.clear()
                self._prune_finished_procs()
                helpers = tuple(
                    task for task in (heartbeat_task, monitor_task)
                    if task is not None)
                if helpers:
                    await asyncio.gather(*helpers, return_exceptions=True)

        # A retained teardown Task is the completion fence. Shield it repeatedly so a
        # second or third caller cancellation cannot expose partially retired state.
        retirement = asyncio.create_task(retire_turn())
        step_failure: "Optional[BaseException]" = None
        retirement_failure: "Optional[tuple[BaseException, Any]]" = None
        while not retirement.done():
            try:
                step_failure = await asyncio.shield(retirement)
            except asyncio.CancelledError as exc:
                current = asyncio.current_task()
                if current is not None and current.cancelling():
                    if caller_cancel is None:
                        caller_cancel = (exc, exc.__traceback__)
                    abort_steps()
                    continue
                retirement_failure = (exc, exc.__traceback__)
                break
        if retirement.done():
            try:
                step_failure = retirement.result()
            except BaseException as exc:
                retirement_failure = (exc, exc.__traceback__)

        if caller_cancel is not None:
            exc, traceback = caller_cancel
            raise exc.with_traceback(traceback)
        if runner_failure is not None:
            exc, traceback = runner_failure
            raise exc.with_traceback(traceback)
        if step_failure is not None:
            raise RuntimeError(
                "Protocol2CaptureOrIngressFailed") from step_failure
        if retirement_failure is not None:
            exc, traceback = retirement_failure
            raise exc.with_traceback(traceback)
        assert result is not None
        return result

    def _mark_session_if_live(self, res: "CliResult") -> None:
        """Record that the engine session is really established — i.e. a turn just
        produced output or echoed a real session id. Only then is a later
        `build_resume` against it safe; before it, `-r <sid>` hits "No conversation
        found" (run-42598 claude spawn-death loop). Cancel/steer don't count (the
        turn was cut short, but the session was already seated if it ran)."""
        if res is None:
            return
        if self._control_secret_values:
            # stdin secret invocations are intentionally non-persistent.  Some CLIs
            # still report an in-memory session id in their result envelope; treating
            # it as resumable would create a guaranteed failed/disk-unsafe follow-up.
            return
        if res.cancelled or res.steered:
            # the turn was interrupted; if it had run at all it already seated the
            # session on a prior call — don't downgrade, just don't upgrade here.
            return
        produced = bool((res.text or "").strip()) or bool(res.output_tokens) \
            or bool(res.session)
        if produced:
            self._session_established = True

    def _execute_invocation(
        self, prompt: str, session: "Optional[str]",
    ) -> "tuple[list[str], Optional[str]]":
        """Build one invocation, selecting the stdin-only capability for secrets."""
        self._finalize_control_context_prompt_manifest(prompt)
        if self._control_secret_values:
            # Direct stdin only reports a write after a child exists.  Strict C6
            # refuses to mislabel that as a pre-effect ContextPacket handoff.
            self._stage_cognitive_context_prompt(prompt, transport="stdin")
            preflight = getattr(self.driver, "secure_prompt_preflight", None)
            if not callable(preflight):
                raise SecurePromptUnsupported(
                    f"{self.driver.name} has no secure prompt preflight")
            supported, detail = preflight()
            if not supported:
                raise SecurePromptUnsupported(
                    detail or f"{self.driver.name} secure prompt transport unavailable")
            argv = self.driver.build_execute_stdin(
                prompt, session, web_access=self.web_access,
                kb_access=self.kb, stream=True)
            return argv, prompt
        self._stage_cognitive_context_prompt(prompt, transport="argv")
        return self.driver.build_execute(
            prompt, session, web_access=self.web_access,
            kb_access=self.kb, stream=True), None

    def _resume_invocation(
        self, prompt: str, session: str,
    ) -> "tuple[list[str], Optional[str]]":
        """Build a resume only after the same final-prompt delivery fence.

        Resume used to bypass ``_execute_invocation`` entirely.  A pending typed
        context could therefore be absent from the resume prompt yet still be in the
        reservation snapshot committed by ``_on_proc``.  Reconcile first; if an
        included secret remains, deliberately fall back to a fresh ephemeral stdin
        invocation because persisted sessions are not a valid secret boundary.
        """
        # A resume may be just a short control instruction.  Strict C6 still binds
        # every released CLI invocation to the exact packet, so restore the sealed
        # block rather than relying on a vendor's opaque session memory.
        cognitive = self._cognitive_context_block()
        if cognitive and cognitive not in prompt:
            prompt = prompt.rstrip() + "\n" + cognitive
        self._finalize_control_context_prompt_manifest(prompt)
        if self._control_secret_values:
            return self._execute_invocation(prompt, None)
        self._stage_cognitive_context_prompt(prompt, transport="argv")
        return self.driver.build_resume(
            prompt, session, web_access=self.web_access,
            kb_access=self.kb, stream=True), None

    async def _run_invocation(
        self, argv: list[str], *, cwd: str, timeout: int,
        stdin_text: "Optional[str]" = None,
    ) -> CliResult:
        """Run an invocation without perturbing legacy/mock runner signatures."""
        runner = self._c6_invocation_runner
        if runner is not None:
            # Phase-A C6 is intentionally argv/local-Popen only.  Freeze every
            # runtime argv mutation *before* the host broker seals it; a later
            # _run_streaming mutation would make the receipt describe a different
            # child command than the one Popen receives.
            if stdin_text is not None:
                raise RuntimeError("C6 Phase A refuses stdin prompt transport")
            if self.container is not None:
                raise RuntimeError("C6 Phase A refuses container/remote execution")
            with self._cognitive_stage_lock:
                prompt = self._cognitive_pending_prompt
            if type(prompt) is not str or not prompt:
                raise RuntimeError("C6 host runner has no pending argv prompt")
            env = self._worker_env(cwd)
            final_argv = self._apply_runtime_argv(argv, env)
            try:
                result = await runner.run(
                    prompt=prompt,
                    final_argv=final_argv,
                    cwd=cwd,
                    timeout=timeout,
                    runtime_env=env,
                )
            except Exception:
                # The broker records canonical UNKNOWN if an invocation crossed
                # the launch boundary.  This local bit only prevents UI/worker code
                # from treating a failed C6 turn as confirmed delivery.
                with self._cognitive_stage_lock:
                    self._cognitive_stage_unknown = True
                raise
            with self._cognitive_stage_lock:
                self._cognitive_stage_released = True
            await self._drain_blackboard_flag_submissions()
            return result
        # A ContextPacket without the host-owned runner is intentionally render-only.
        # It must never fall back to this worker-owned streaming path, because that
        # would bypass the durable C6 launch claim and its Popen interlock.
        if self._cognitive_context_packet is not None:
            raise RuntimeError(
                "strict C6 execution requires the host-owned invocation runner"
            )
        if stdin_text is None:
            result = await self._run_streaming(argv, cwd=cwd, timeout=timeout)
        else:
            result = await self._run_streaming(
                argv, cwd=cwd, timeout=timeout, stdin_text=stdin_text)
        # A few CLIs buffer tool frames until process exit. Drain the host-owned
        # handoff once more at the invocation boundary so those submissions do not
        # depend on a vendor-specific streaming event.
        await self._drain_blackboard_flag_submissions()
        return result

    def _resume_or_execute_argv(self, prompt: str, session: "Optional[str]", *,
                                fresh_prompt: "Optional[str]" = None
                                ) -> "tuple[list[str], Optional[str]]":
        """Build a resume argv when the session is known-established; otherwise fall
        back to a FRESH execute (the prior turn never seated the session, so `-r`
        would 0-token-die). The fallback still carries the solve context: callers
        pass the resume prompt (which already folds in board + peer/guidance), or a
        richer `fresh_prompt` to use when starting clean."""
        if self._control_secret_values:
            # A secret-bearing turn is explicitly non-persistent, so there is no
            # session that may be resumed safely.  Start another ephemeral stdin
            # turn and require callers to provide the full fresh context when the
            # short follow-up prompt alone would be insufficient.
            return self._execute_invocation(
                fresh_prompt if fresh_prompt is not None else prompt, None)
        if self._session_established and session:
            return self._resume_invocation(prompt, session)
        new_sid = self.driver.new_session()
        return self._execute_invocation(
            fresh_prompt if fresh_prompt is not None else prompt, new_sid)

    async def _drain_control_async(self) -> None:
        """Loop-thread wrapper around _drain_control (Queue.get_nowait must run on
        the loop that owns the queue)."""
        self._drain_control()

    async def _emit_step(self, step: StreamStep) -> None:
        """Map one live StreamStep onto the deck's event stream (reasoning bubble /
        tool bubble / tool-result lane). Best-effort — never raises into the run."""
        try:
            if step.kind == "reasoning" and step.text:
                incoming = step.text
                if _is_reasoning_replay(self._reasoning_turn_acc, incoming):
                    await self._emit(EventType.REASONING_DELTA, text="", turn_end=True)
                    self._reasoning_turn_acc = ""
                else:
                    self._stream_reasoning_chars += len(incoming)
                    # Thinking deltas are displayed but not accumulated: the
                    # message_end snapshot repeats only the answer text, so the
                    # accumulator must hold answer text only, otherwise the
                    # snapshot becomes a suffix the replay check cannot seal.
                    if not getattr(step, "thinking", False):
                        self._reasoning_turn_acc = (self._reasoning_turn_acc or "") + incoming
                    await self._emit(EventType.REASONING_DELTA, text=incoming)
            elif step.kind == "tool":
                self._reasoning_turn_acc = ""
                self._stream_tool_starts += 1
                label = f"{step.tool}: {step.text}" if step.text else step.tool
                await self._emit(EventType.TOOL_CALL_START, tool=label[:200])
            # LIVE marker streaming: if this step's text carries a VERIFIED_FACT=/
            # DEADEND=/FOUND_FLAG= marker, push it to the board NOW (not at end-of-run)
            # so a teammate racing in parallel sees it mid-solve — the whole point of a
            # shared board. Deduped: the marker shows up in both the tool echo and
            # its result.
            #
            # Marker sourcing is split by step kind so a flag can ONLY come from real
            # command output, never from the worker's prose (run-75379 provenance fix):
            #
            #  - "tool" (the COMMAND about to run): NEVER scanned. Its text is the
            #    worker's INTENT, not output. A worker that greps for
            #    `FOUND_FLAG=bl_...` puts that literal marker in the command string;
            #    scanning it registered the grep PATTERN as a real flag (run-11550:
            #    codex ran `grep -E 'FOUND_FLAG=bl_|VERIFIED_FACT=.*L4|...'` → false
            #    flag `bl_|VERIFIED_FACT=.*L4|...`).
            #
            #  - "reasoning" (the worker's own thought): facts/dead-ends ONLY, NEVER a
            #    flag (allow_flags=False). A worker restating `FOUND_FLAG=flag{x}` in
            #    its reasoning is a CLAIM; gating that flag against the reasoning chunk
            #    itself is self-referential (`flag in raw_output` where raw_output IS
            #    the claim) → trivially true. That is exactly how the hallucinated
            #    flag02 (run-75379) got laundered through prose. Flags must trace to
            #    real output, so reasoning can't be their source.
            #
            #  - "tool_result" (REAL command output): may source a flag, gated against
            #    the FULL UNTRUNCATED output (step.raw), not the 600-char deck chunk —
            #    so a flag past char 600, or in a nested-ssh remote stdout the outer
            #    ssh forwarded here, is still gateable. The raw output is also persisted
            #    (see _persist_raw_tool_output) so the end-of-run backstop can gate
            #    against what commands actually printed, not the summarized envelope.
            if step.kind == "tool":
                # Keep command/output identity when the engine exposes a call id. The
                # FIFO fallback covers older stream shapes that preserve order only.
                command = (step.text or "")[:2000]
                self._pending_tool_calls.append((step.call_id, command))
            if step.kind == "tool_result":
                raw = step.raw or step.text
                spill_status: "Optional[dict[str, Any]]" = None
                if step.spill_path:
                    spilled, spill_status = self._materialize_tool_result_spill(step)
                    if spilled:
                        raw = spilled
                    if (spill_status.get("status") != "loaded"
                            and bool(getattr(self, "_protocol2_mode", False))):
                        await self._emit(
                            EventType.TOOL_CALL_RESULT,
                            **tool_result_payload(
                                self.driver.name,
                                {"condensed": "", "spill": spill_status}))
                        raise RuntimeError("Protocol2SpillMaterializationFailed")
                match_idx = None
                if step.call_id:
                    match_idx = next((
                        idx for idx, (call_id, _) in enumerate(self._pending_tool_calls)
                        if call_id == step.call_id
                    ), None)
                if (match_idx is None and not step.call_id
                        and self._pending_tool_calls):
                    match_idx = 0
                attributed = match_idx is not None
                command = (self._pending_tool_calls[match_idx][1]
                           if attributed else "")
                capture_receipt = ""
                if raw:
                    capture_receipt = self._persist_raw_tool_output(
                        raw, command=command, attributed=attributed)
                # Capture/persistence is the Protocol 2 authority boundary. A failed
                # capture leaves the invocation pending and emits no result event or
                # marker. Once it succeeds, consuming the invocation is safe even if a
                # later best-effort candidate publication fails.
                if attributed:
                    self._pending_tool_calls.pop(match_idx)
                result_text = raw[:600] if raw else ""
                result_view: "dict[str, Any]" = {"condensed": result_text}
                if spill_status is not None:
                    result_view["spill"] = spill_status
                if result_text or spill_status is not None:
                    await self._emit(
                        EventType.TOOL_CALL_RESULT,
                        **tool_result_payload(
                            self.driver.name, result_view,
                            truncated=len(raw) > len(result_text)))
                if raw:
                    try:
                        await self._stream_markers(
                            result_text, allow_flags=attributed,
                            flag_provenance=raw)
                    finally:
                        # The Blackboard command commits flag_submission during
                        # this exact tool turn.  Submission admission must not be
                        # skipped when any legacy marker/fact parser above raises:
                        # the API event is the sole Protocol 1 completion input.
                        await self._drain_blackboard_flag_submissions()
                    if not attributed:
                        await self._surface_unverified_flags(
                            raw, reason="tool result has no matching invocation; "
                                        "execution source is unattributed")
            elif step.kind == "reasoning" and step.text:
                await self._stream_markers(step.text, allow_flags=False)
        except Exception:
            if bool(getattr(self, "_protocol2_mode", False)):
                raise

    # bound the in-memory raw-output corpus (the per-chunk artifact persist keeps the
    # full transcript; this buffer only needs to be a searchable provenance haystack).
    _RAW_OUTPUT_CHAR_CAP = 4_000_000
    _SPILL_OUTPUT_BYTE_CAP = 4_000_000

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

    def _materialize_tool_result_spill(
        self, step: StreamStep,
    ) -> "tuple[str, dict[str, Any]]":
        """Read a bounded Cursor spill without following symlinks.

        Metadata remains observable in the result event, but rejected paths and
        partial/oversized bytes never enter provenance or marker extraction.
        """
        audit: "dict[str, Any]" = {
            "path": str(step.spill_path or ""),
            "status": "rejected",
            "reason": "invalid_path",
            "declared_size_bytes": int(step.spill_size_bytes),
            "declared_line_count": int(step.spill_line_count),
        }
        mapped = self._spill_host_path(step.spill_path)
        if mapped is None:
            return "", audit
        cwd, candidate = mapped
        try:
            rel = candidate.relative_to(cwd)
        except ValueError:
            return "", audit
        if not rel.parts or any(part in {"", ".", ".."} for part in rel.parts):
            return "", audit
        flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
        nofollow = getattr(os, "O_NOFOLLOW", 0)
        directory = getattr(os, "O_DIRECTORY", 0)
        dir_fd: "Optional[int]" = None
        file_fd: "Optional[int]" = None
        try:
            dir_fd = os.open(str(cwd), flags | directory | nofollow)
            for part in rel.parts[:-1]:
                next_fd = os.open(part, flags | directory | nofollow, dir_fd=dir_fd)
                os.close(dir_fd)
                dir_fd = next_fd
            file_fd = os.open(rel.parts[-1], flags | nofollow, dir_fd=dir_fd)
            info = os.fstat(file_fd)
            if not stat.S_ISREG(info.st_mode):
                audit["reason"] = "not_regular"
                return "", audit
            if info.st_size > self._SPILL_OUTPUT_BYTE_CAP:
                audit["reason"] = "oversized"
                audit["actual_size_bytes"] = int(info.st_size)
                return "", audit
            chunks: "list[bytes]" = []
            remaining = self._SPILL_OUTPUT_BYTE_CAP + 1
            while remaining > 0:
                chunk = os.read(file_fd, min(1024 * 1024, remaining))
                if not chunk:
                    break
                chunks.append(chunk)
                remaining -= len(chunk)
            data = b"".join(chunks)
            audit["actual_size_bytes"] = len(data)
            if len(data) > self._SPILL_OUTPUT_BYTE_CAP:
                audit["reason"] = "oversized"
                return "", audit
            audit["status"] = "loaded"
            audit["reason"] = ""
            return data.decode("utf-8", errors="replace"), audit
        except OSError as exc:
            audit["reason"] = f"os_error:{exc.errno or 0}"
            return "", audit
        finally:
            if file_fd is not None:
                os.close(file_fd)
            if dir_fd is not None:
                os.close(dir_fd)

    def _persist_raw_tool_output(
        self, raw: str, *, command: str = "", attributed: bool = True,
    ) -> str:
        """Append one tool execution's FULL stdout/stderr to the audit corpus.
        Called from the LIVE path (_emit_step) for every tool_result, BEFORE any
        truncation/summarization. Results without a matching invocation are retained
        for audit but marked non-authoritative. The paired `command` is kept for the
        target-identity attestation (round-10). Protocol 2 captures first and requires
        a digest receipt before local bytes can become authoritative."""
        if not raw and not command:
            return ""
        receipt = ""
        if bool(getattr(self, "_protocol2_mode", False)):
            capture = getattr(self, "_protocol2_capture_callback", None)
            if not callable(capture):
                raise RuntimeError("Protocol2CaptureUnavailable")
            receipt = str(capture(raw) or "")
            expected = hashlib.sha256(
                raw.encode("utf-8", errors="replace")).hexdigest()
            if receipt != expected:
                raise RuntimeError("Protocol2CaptureReceiptMismatch")
        # Build the complete trimmed state off to the side, then swap all parallel
        # fields together so an exception cannot leave their indexes misaligned.
        outputs = [*self._raw_tool_outputs, raw]
        commands = [*self._raw_tool_commands, command or ""]
        attributed_rows = [*self._raw_tool_attributed, bool(attributed)]
        receipts = [*self._raw_tool_capture_receipts, receipt]
        chars = self._raw_tool_outputs_chars + len(raw)
        while chars > self._RAW_OUTPUT_CHAR_CAP and len(outputs) > 1:
            chars -= len(outputs.pop(0))
            commands.pop(0)
            attributed_rows.pop(0)
            receipts.pop(0)
        self._raw_tool_outputs = outputs
        self._raw_tool_commands = commands
        self._raw_tool_attributed = attributed_rows
        self._raw_tool_capture_receipts = receipts
        self._raw_tool_outputs_chars = chars
        self._maybe_steer_idle_repeat()
        return receipt

    def _maybe_steer_idle_repeat(self) -> None:
        """Soft-correct a worker repeating the same command with unchanged output."""
        if self.mode in ("review", "verifier"):
            return
        self._stalled_at = None
        cmds = list(getattr(self, "_raw_tool_commands", []) or [])
        outs = list(getattr(self, "_raw_tool_outputs", []) or [])
        n = _IDLE_REPEAT_LIMIT
        if len(cmds) < n or len(outs) < n:
            return
        window_cmds = cmds[-n:]
        window_outs = outs[-n:]
        if len(set(window_cmds)) != 1:
            return
        hashes = [
            hashlib.sha256((out or "").encode("utf-8", errors="replace")).hexdigest()
            for out in window_outs
        ]
        if len(set(hashes)) != 1:
            return
        if getattr(self, "_idle_repeat_steered", False):
            return
        with self._guidance_lock:
            if _IDLE_REPEAT_STEER not in self._standing_guidance:
                self._standing_guidance.append(_IDLE_REPEAT_STEER)
        if self._enable_in_turn_steer():
            self._idle_repeat_steered = True
            self._steer_event.set()

    def _provenance_corpus(self) -> str:
        """Return the ONLY authoritative flag-evidence corpus: raw tool output.

        Assistant reasoning/final text, resumed-session replay, operator context, and
        result envelopes are claims or context — never evidence.  Callers used to pass
        the assistant's final ``result_text`` as ``extra`` here, making a
        ``FOUND_FLAG=...`` claim self-proving.  Keeping this method argument-free makes
        that category error impossible at every bootstrap/explore/respond call site.
        """
        return "\n".join(
            output for idx, output in enumerate(self._raw_tool_outputs)
            if (idx < len(self._raw_tool_attributed)
                and self._raw_tool_attributed[idx])
        )

    def _finding_evidence_corpus(self) -> str:
        """Request (attributed command) plus response (that command's output).

        FOUND_FINDING fields are request/response fragments. Flag provenance is
        output-only so a FOUND_FLAG= line cannot self-prove; findings need the
        argv/URL as well, because resource_id and input often live in the
        request and never appear again in the body.
        """
        parts: list[str] = []
        for idx, output in enumerate(self._raw_tool_outputs):
            if (idx >= len(self._raw_tool_attributed)
                    or not self._raw_tool_attributed[idx]):
                continue
            cmd = (self._raw_tool_commands[idx]
                   if idx < len(self._raw_tool_commands) else "")
            parts.append(f"{cmd}\n{output}")
        return "\n".join(parts)

    def _flag_provenance_records(self, flag: str, raw_output: str) -> "list[str]":
        """Return candidate-bearing command+output records represented by one gate input.

        End-of-run callers pass the flattened corpus, while live callers pass the just-
        persisted tool result.  Only executions whose OUTPUT contains the candidate are
        evidence for that candidate; unrelated board reads must not poison it.  Commands
        remain paired with their own output so an internal-file read cannot hide its
        source. Direct callers without persisted records retain single-record behavior.
        """
        text = raw_output or ""
        if not self._raw_tool_outputs:
            return [text]
        if text == self._provenance_corpus():
            indexes = [
                idx for idx, output in enumerate(self._raw_tool_outputs)
                if (idx < len(self._raw_tool_attributed)
                    and self._raw_tool_attributed[idx]
                    and flag in output)
            ]
        else:
            # Live admission follows the newest matching execution. Output strings can
            # repeat; identity comes from stream order/call id, not value equality.
            indexes = [
                idx for idx in range(len(self._raw_tool_outputs) - 1, -1, -1)
                if (idx < len(self._raw_tool_attributed)
                    and self._raw_tool_attributed[idx]
                    and self._raw_tool_outputs[idx] == text)
            ][:1]
            if not indexes:
                if any(output == text for output in self._raw_tool_outputs):
                    return []  # known execution, but its invocation was unattributed
                return [text]
        return [
            ((self._raw_tool_commands[idx]
              if idx < len(self._raw_tool_commands) else "")
             + "\n" + self._raw_tool_outputs[idx])
            for idx in indexes
        ]

    async def _stream_markers(self, text: str, *, allow_flags: bool = True,
                              flag_provenance: "Optional[str]" = None) -> None:
        """Parse FOUND_FLAG=/VERIFIED_FACT=/DEADEND=/NEED_INPUT= markers out of a
        live stream chunk and publish each (once) so racing teammates get it
        immediately. End-of-run extraction still runs as a backstop.

        FOUND_FLAG is extracted HERE — not just from the terminal result text —
        because a worker often emits the flag in an intermediate/streamed assistant
        block (or buried in a Bash echo's output) that never lands in the driver's
        final `result` field, which is the only thing run()'s per-turn
        _extract_flags(res.text) sees (run-11189: 2 real flags recovered, 0
        registered, every worker.finished solved=False).

        PROVENANCE (run-75379): a flag is accepted only if it traces to REAL command
        output, never to the worker's own prose.
        - `allow_flags=False` → this chunk may NOT source a flag at all (used for
          REASONING-kind text: the worker restating `FOUND_FLAG=flag{x}` in its
          thoughts is a CLAIM, not evidence — gating it against the reasoning chunk
          itself is self-referential and trivially passes, which is exactly how the
          hallucinated flag02 got laundered through prose). Reasoning still yields
          facts/dead-ends/need-input, just never flags.
        - `flag_provenance` → the corpus flags are both EXTRACTED FROM and GATED
          AGAINST, when it differs from `text`. For a tool_result we pass the FULL
          untruncated command output (StreamStep.raw): the FOUND_FLAG= marker itself
          can sit past char 600, so scanning the 600-char `text` would miss it
          entirely — we must read the marker out of the raw output AND verify the value
          against that same raw output. Facts/dead-ends/need-input still come from
        `text` (the displayed chunk). Defaults to `text`."""
        if self.mode == "review":
            return
        raw_text = text
        prov = flag_provenance if flag_provenance is not None else raw_text
        # Marker-derived facts/dead-ends/requests are durable graph/event data.
        # Preserve unredacted provenance only in memory for the hard flag gate.
        text = self._redact_control_secrets(raw_text)
        if allow_flags:
            # extract AND gate flags from the provenance corpus (the marker can be past
            # the 600-char truncation point of `text`).
            for f in self._extract_flags(prov):
                if f in self._already_found:
                    continue
                if not self._flag_ok(f, prov):
                    continue
                if await self._accept_flag(f):
                    self._stream_accepted.append(f)
            # Bare-flag harvest (tsec-f03: a-01/a-02/a-04 were really solved but
            # scored 0 — the flag sat in the raw tool output of a file read / shell
            # command and the worker never wrote FOUND_FLAG=, so nothing was ever
            # registered or submitted). This scans ONLY real captured command
            # output: allow_flags=True is restricted to attributed tool_result
            # chunks (reasoning/prose always passes allow_flags=False), so the
            # historical prose false positives (run-1619/3613/4305) do not apply.
            # Every candidate still clears the placeholder filter (inside
            # _poc_flag_literals) and the full provenance/launder/target gate.
            for f in self._poc_flag_literals(prov):
                if f in self._already_found:
                    continue
                if not self._flag_ok(f, prov):
                    continue
                if await self._accept_flag(f):
                    self._stream_accepted.append(f)
        if getattr(self.challenge, "mode", "ctf") == "pentest":
            await self._stream_report_submissions(raw_text)
            if self.mode == "verifier":
                await self._stream_repro_decision(raw_text)
            else:
                await self._stream_finding_claims(raw_text, flag_provenance)
        for path_text, entry_command, status, note in self._extract_poc_saves(text):
            await self._handle_poc_save(path_text, entry_command, status, note)
        facts, deadends = self._extract_structured_facts(text)
        witness_hint = self._extract_fact_witness(text)
        for f_text in facts:
            key = ("F", f_text[:200])
            if key in self._published_markers:
                continue
            self._published_markers.add(key)
            # bind the marker line itself as the provenance artifact so a live fact
            # still traces to evidence (the full transcript is persisted at end-of-run).
            try:
                aid = self.artifacts.put(f"VERIFIED_FACT={f_text}\n", suffix=".txt")
            except Exception:
                aid = ""
            verified = self._fact_witnessed_in_chunk(f_text, text)
            if not verified:
                await self._emit_bb(
                    "fact_witness_downgraded",
                    fact=f"[{self.driver.name}] {f_text[:200]}",
                    reason="VERIFIED_FACT marker lacked matching non-marker output in this chunk",
                )
            await self._record_fact(
                f"[{self.driver.name}] {f_text[:200]}", verified=verified,
                artifact_id=aid, witness=witness_hint)
        for d_text in deadends:
            key = ("D", d_text[:200])
            if key in self._published_markers:
                continue
            self._published_markers.add(key)
            dead_end_seq = None
            if self.shared_graph is not None:
                try:
                    seq = self.shared_graph.add_dead_end(
                        actor=self.solver_id, reason=f"[{self.driver.name}] {d_text[:200]}")
                    if seq > 0:
                        dead_end_seq = seq
                except Exception:
                    pass
            if self.insight is not None:
                try:
                    await self.insight.dead_end(
                        self.solver_id, f"[{self.driver.name}] {d_text[:200]}")
                except Exception:
                    pass
            await self._emit_bb("dead_end", reason=f"[{self.driver.name}] {d_text[:200]}",
                                dead_end_seq=dead_end_seq)
            await self._mark_claimed_pocs_spent(d_text)
        # ── NEED_INPUT: the worker is raising its hand for the operator ──────
        for need, reported_kind in self._extract_need_requests(text):
            key = ("N", str(need)[:200])
            if key in self._published_markers:
                continue
            self._published_markers.add(key)
            # heuristic kind: env-down vs missing-resource (just for the deck label;
            # both pause the same way).
            low = str(need).lower()
            kind = ("env_down" if any(k in low for k in (
                "unreachable", "connection refused", "refused", "timed out",
                "timeout", "expired", "instance", "502", "503", "down"))
                else "need_input")
            need_kind = _normalize_need_kind(reported_kind) or classify_need_kind(str(need))
            # surface to the operator (HITL_REQUEST — previously dead code) AND drop a
            # board marker the coordinator polls to decide whether to pause. The
            # operator decision CARD is the one place full fidelity matters, so cap
            # generously (1000) and append an ellipsis when clipped so a long ask
            # isn't silently cut mid-sentence (the "...the data, or" symptom).
            need_card = str(need) if len(str(need)) <= 1000 else (str(need)[:1000] + " …")
            try:
                request_id, correlation = self._decision_request_identity(
                    need_card, need_kind)
                await self._emit(
                    EventType.HITL_REQUEST,
                    **hitl_request_payload(self.solver_id, need_card, kind=kind,
                                           need_kind=need_kind,
                                           request_id=request_id,
                                           **correlation))
            except Exception:
                pass
            # blackboard delta: first arg is the delta KIND ("need_input"); the
            # classification goes in a distinct field (`need_kind`) to avoid clashing
            # with _emit_bb's positional `kind`.
            await self._emit_bb("need_input", need=need_card,
                                need_kind=need_kind, legacy_kind=kind)

        # ── submission gate: READY_TO_SUBMIT + cooldown detection (opted-in) ────
        if self._verifier_rate_limited():
            for ready in self._extract_ready_to_submit(text):
                key = ("R", ready[:120])
                if key in self._published_markers:
                    continue
                self._published_markers.add(key)
                # surface the intent-to-submit so the coordinator can serialize it
                # (broadcast SUBMIT_LOCKED to siblings). The coordinator's gate sink
                # decides; the worker proceeds to submit in this same turn regardless
                # (it can't pause mid-turn) — the value is telling OTHERS to hold.
                await self._emit_bb("ready_to_submit", note=ready[:200])
                if self.insight is not None:
                    try:
                        await self.insight.submit_locked(self.solver_id)
                    except Exception:
                        pass
            await self._maybe_broadcast_lockout(text)
    async def _drain_blackboard_flag_submissions(self) -> None:
        """Validate this Worker's pending Blackboard API Flag submissions."""
        sg = getattr(self, "shared_graph", None)
        if sg is None or bool(getattr(self, "_protocol2_mode", False)):
            return
        queued: list[dict[str, Any]] = []
        request_dir = self._flag_submission_dir
        if request_dir is not None:
            try:
                request_paths = sorted(request_dir.glob("fs-*.json"))
            except OSError:
                request_paths = []
            for request_path in request_paths:
                try:
                    if request_path.stat().st_size > 4096:
                        raise ValueError("submission request exceeds 4096 bytes")
                    payload = json.loads(request_path.read_text(encoding="utf-8"))
                    if not isinstance(payload, dict):
                        raise ValueError("submission request is not an object")
                    submission_id = str(payload.get("submission_id") or "")
                    if (submission_id != request_path.stem
                            or not re.fullmatch(r"fs-[0-9a-f]{16}", submission_id)):
                        raise ValueError("submission id does not match request file")
                    if str(payload.get("actor") or "") != self.solver_id:
                        raise ValueError("submission actor does not own this Worker")
                    if str(payload.get("protocol") or "") != "blackboard-api-v1":
                        raise ValueError("unsupported submission protocol")
                    flag = str(payload.get("flag") or "").strip()
                    intent_id = str(payload.get("intent_id") or "")
                    submission_seq = sg.flag_submission(
                        actor=self.solver_id,
                        submission_id=submission_id,
                        flag=flag,
                        intent_id=intent_id or None,
                    )
                    queued.append({
                        "seq": max(0, int(submission_seq or 0)),
                        "actor": self.solver_id,
                        "payload": {
                            "submission_id": submission_id,
                            "flag": flag,
                            "intent_id": intent_id,
                            "protocol": "blackboard-api-v1",
                        },
                    })
                except Exception as exc:
                    await self._emit_bb(
                        "flag_submission_rejected",
                        submission_id=request_path.stem,
                        reason=f"invalid submission request: {type(exc).__name__}: {exc}",
                    )
                finally:
                    try:
                        request_path.unlink()
                    except OSError:
                        pass
        try:
            rows = sg.events_since(
                self._last_flag_submission_seq, kinds=["flag_submission"])
        except Exception:
            rows = []
        queued_ids = {
            str((row.get("payload") or {}).get("submission_id") or "")
            for row in queued
        }
        rows = [
            *queued,
            *(row for row in rows
              if str((row.get("payload") or {}).get("submission_id") or "")
              not in queued_ids),
        ]
        for row in rows:
            seq = int(row.get("seq") or 0)
            self._last_flag_submission_seq = max(self._last_flag_submission_seq, seq)
            if str(row.get("actor") or "") != self.solver_id:
                continue
            payload = dict(row.get("payload") or {})
            submission_id = str(payload.get("submission_id") or f"seq-{seq}")
            flag = str(payload.get("flag") or "").strip()
            try:
                accepted = bool(
                    flag and self._flag_ok(flag, self._provenance_corpus()))
                code = "accepted" if accepted else "provenance_rejected"
                detail = (
                    "Flag matched captured execution evidence"
                    if accepted else
                    "Flag did not match an admissible prior command output or artifact"
                )
            except Exception as exc:
                accepted = False
                code = "validation_error"
                detail = (
                    "Flag provenance validation failed: "
                    f"{type(exc).__name__}: {str(exc)[:160]}"
                )
            try:
                sg.flag_submission_decision(
                    actor=self.solver_id,
                    submission_id=submission_id,
                    accepted=accepted,
                    code=code,
                    detail=detail,
                )
            except Exception:
                pass
            if accepted:
                self._validated_flag_submissions.add(flag)
                if await self._accept_flag(flag):
                    self._stream_accepted.append(flag)
            else:
                await self._emit_bb(
                    "flag_submission_rejected",
                    submission_id=submission_id,
                    reason=detail,
                )

    async def _maybe_broadcast_lockout(self, text: str) -> None:
        """Parse a cooldown/burn-lockout duration out of verifier output and, if it
        extends the known lock, record it locally + broadcast VERIFIER_LOCKED once
        per distinct deadline so siblings stop submitting. Best-effort.

        GUARD: only treat this as a real lockout if the text is the VERIFIER's own
        verdict (not a worker reading a doc/file that describes the lockout) — else
        any chunk mentioning "burn-lockout … 30 min" broadcasts a phantom backoff
        (run-11553: a worker read docs/PROBLEM_verifier_*.md → fake 30-min lock)."""
        if not _looks_like_verifier_output(text):
            return
        secs = _parse_lockout_seconds(text)
        if secs <= 0:
            return
        deadline = time.time() + secs
        # only act if this lock is MEANINGFULLY later than what we already know (avoid
        # re-broadcasting the same lock every chunk); 5s slop absorbs clock jitter.
        if deadline <= self._verifier_locked_until + 5:
            return
        self._verifier_locked_until = deadline
        await self._emit_bb("dead_end",
                            reason=f"[{self.driver.name}] verifier locked ~{int(secs)}s — "
                                   "swarm backing off submissions")
        if self.insight is not None:
            try:
                await self.insight.verifier_locked(self.solver_id, secs)
            except Exception:
                pass

    # signatures that the flag in `raw_output` was SCRAPED from local muteki /
    # agent storage (other runs' logs, the engine's own conversation history,
    # sibling process titles) rather than RECOVERED from the challenge target.
    # run-11551: a codex worker ran `rg 'FOUND_FLAG=bl_…' ~/.codex/sessions
    # sessions/run-…` and found the L2 flag from a PRIOR run on disk, then
    # restated it as its own FOUND_FLAG — falsely "solving" L4 with the L2 flag.
    # A real recovered flag traces to the TARGET's verifier output, never to a
    # grep of the operator's local muteki/agent files.
    # IMPORTANT: match ONLY signatures of reading muteki/agent INTERNAL storage
    # (other runs' event logs, the engine's own conversation history, sibling
    # process titles). Do NOT match a worker's own cwd — every worker legitimately
    # works under sessions/<this-run>/workspace/workers/<id>/ and reads its own
    # board/attachments there, so a bare `/sessions/run-` or `/workspace/workers/`
    # path fragment is NORMAL and must not trip the guard (it would false-reject a
    # genuine flag that happens to appear in cwd output — e.g. a crypto/forensics
    # solve that wrote its result to a local file). The reliable launder tells are:
    # a run's persisted EVENT LOG file (run-NNNN.jsonl), the engine history dirs,
    # and the "harvest from process output" phrasing. None of these are ever touched
    # by a worker that is actually solving the challenge.
    # IMPORTANT: match ONLY unambiguous signatures of reading muteki/agent INTERNAL
    # storage (the engine's own conversation history, another run's persisted event
    # log / winner.json, eval scratch). Do NOT add a bare `/sessions/run-` or
    # `/workspace/workers/` — every worker legitimately works under
    # sessions/<THIS-run>/workspace/workers/<id>/ and a genuine flag can appear in
    # its own cwd output (a crypto/forensics solve writing to a local file). The
    # job of telling "this run's cwd" apart from "another run's data" is the
    # cross-run dedup (#2), not this regex.
    # Part 1 — UNAMBIGUOUS launder signatures. These name muteki/agent INTERNAL
    # storage that no genuine target solve ever touches (the engine's own
    # conversation history dirs, another run's PERSISTED EVENT LOG — `run-NNNN.jsonl` /
    # `rollout-*.jsonl` are muteki/codex internal filenames a CTF target would never
    # serve), or describe the "harvest from a sibling process" behavior in words. They
    # fire on their own, scanned over the WHOLE raw_output (a grep-from-disk launder
    # can restate the stolen flag arbitrarily far from the read, so localizing to the
    # FOUND_FLAG line would miss it — run-11551).
    _LAUNDER_RE = re.compile(
        r"(?:\.codex/sessions|\.claude/projects|\.agents/skills|"
        r"\.cursor/projects|agent-transcripts|"   # cursor's own conversation history
        r"run-\d{3,}\.jsonl|rollout-[0-9a-f-]+\.jsonl|"  # other runs' persisted logs
        r"process title|process output so the token|live process output|"
        r"from (?:a |another |the )?(?:teammate|sibling).*process)",
        re.IGNORECASE)

    # Part 2 — AMBIGUOUS file/path tokens that are launder tells ONLY when a worker is
    # READING them. Unlike `run-NNNN.jsonl` (Part 1), `winner.json` / `shared_graph.db`
    # / `/eval_runs/` can legitimately appear in a TARGET's output (a service serving
    # such a path, a forensics blob that mentions the string), and the old
    # context-free match false-rejected a genuine flag in the same transcript
    # (Rivulet-class). We require a read-action verb to accompany the token (still
    # whole-output, so a restated steal is caught), which the run-11551 grep-from-disk
    # launder does and an innocent target mention does NOT.
    _LAUNDER_FILE_RE = re.compile(
        r"shared_graph\.db|winner\.json|/eval_runs?/",
        re.IGNORECASE)
    _READ_ACTION_RE = re.compile(
        r"\b(?:cat|grep|less|more|tail|head|read|open|cp|copy|view|"
        r"strings|xxd|sed|awk|jq|type|Get-Content)\b|"
        r"\b(?:read(?:ing)?|grepp?(?:ing|ed)?|harvest(?:ing|ed)?|"
        r"scrap(?:e|ing|ed)|extract(?:ing|ed)?|pull(?:ing|ed)?|"
        r"dump(?:ing|ed)?)\b",
        re.IGNORECASE)

    def _remember_operator_context(self, text: str) -> None:
        """Remember untrusted operator text for anti-echo flag tainting.

        This deliberately stores text only in worker memory.  It does not add it to
        the evidence graph or the raw-output corpus.  Exact-string dedupe keeps a
        standing hint replay from growing the list forever.
        """
        clean = str(text or "").strip()
        if not clean:
            return
        contexts = getattr(self, "_operator_context_texts", None)
        if contexts is None:
            self._operator_context_texts = []
            contexts = self._operator_context_texts
        if clean not in contexts:
            contexts.append(clean)

    def _operator_context_corpus(self) -> str:
        """Return all operator-authored context visible to this worker.

        Shared-graph directives are included dynamically because a respawn/reopen can
        learn them from the board without receiving ``standing_guidance`` directly.
        Failure to read the graph degrades closed: the in-memory context remains.
        """
        texts = list(getattr(self, "_operator_context_texts", []) or [])
        sg = getattr(self, "shared_graph", None)
        if sg is not None and hasattr(sg, "operator_directives"):
            try:
                for directive in sg.operator_directives(active_only=False):
                    text = str((directive or {}).get("text") or "").strip()
                    if text and text not in texts:
                        texts.append(text)
            except Exception:
                pass
        return "\n".join(texts)

    def _flag_from_operator_context(self, flag: str) -> bool:
        """True when ``flag`` was supplied by the operator rather than discovered."""
        return bool(flag and flag in self._operator_context_corpus())

    # the REAL provenance gate — the one shared, hardcoded acceptance check.
    def _flag_ok(self, flag: str, raw_output: str) -> bool:
        # run-75379 defense-in-depth: a flag the operator invalidated as a false
        # positive is permanently refused at the gate too, so it never even reaches
        # the accept attempt or the live _stream_accepted bookkeeping. The
        # load-bearing enforcement is in _accept_flag (before broadcast); this is the
        # belt to that suspenders — both read the same durable shared-graph set.
        if flag and flag in self._rejected_flags():
            return False
        # Operator input has no truth privilege.  Even if the worker echoes it through
        # a shell/tool (thereby placing it in raw stdout), it remains tainted and cannot
        # satisfy provenance.  This closes the operator → `echo FOUND_FLAG=...` laundering
        # path while leaving the hardcoded gate itself unchanged.
        if self._flag_from_operator_context(flag):
            return False
        # anti-launder: a flag whose provenance text shows it was SCRAPED from local
        # muteki / agent storage (another run's log, the engine's own history, a
        # sibling process title) is NOT a solve — it was harvested, not recovered
        # from the target (run-11551). A genuine recovery traces to the target's
        # real output, never to these paths/phrases. This BEHAVIOR signature (reading
        # internal storage) is the reliable, sufficient defense.
        #
        # NOTE: there is deliberately NO cross-run value-dedup here. A previous
        # `_flag_belongs_to_another_run` rejected any flag whose VALUE was registered
        # by another run — but most CTF/range flags are STATIC (same value every run),
        # so re-solving the same challenge would have its real flag rejected as a
        # "launder" (Rivulet: flag1/2/4 truly recovered but refused → operator stuck
        # at a false 1/4 for hours). That value-dedup was also silently dead for
        # months (missing `import json` → NameError swallowed → never fired), proving
        # it added no value, while _LAUNDER_RE independently catches the real
        # run-11551 grep-from-disk launder via its path signature. Removed.
        text = raw_output or ""
        evidence_records = self._flag_provenance_records(flag, text)
        if not evidence_records:
            return False
        if any(self._LAUNDER_RE.search(record) for record in evidence_records):
            return False
        # file-token launder: an internal-storage filename is a tell ONLY when the
        # SAME tool execution also shows a READ action (cat/grep/read/harvest/…). The
        # command and its output form one record, but unrelated executions do not: a
        # prior board read must not poison a later genuine extraction (round-15 F).
        if any(
            self._LAUNDER_FILE_RE.search(record)
            and self._READ_ACTION_RE.search(record)
            for record in evidence_records
        ):
            return False
        accepted = _gate_flag_ok(
            flag, raw_output, flag_format=self.challenge.flag_format,
            artifacts=self.artifacts)
        if not accepted:
            return False
        # Target-identity attestation (round-10): the flag's evidence must trace
        # to a SANCTIONED resource — the declared target, or pure computation
        # over the provisioned files. A flag the worker manufactured (planted
        # into a self-hosted instance, fetched from a public writeup, or
        # otherwise present in its own commands before any tool produced it)
        # fails here even when the final extraction output is genuine: the
        # provenance moat binds flag→tool-output, this binds flag→resource-origin.
        origin_rejection = self._origin_tainted(flag)
        if origin_rejection is not None:
            self._note_flag_origin_rejected(flag, origin_rejection)
            return False
        protocol2_gate = getattr(self, "_protocol2_gate_callback", None)
        if callable(protocol2_gate):
            # Missing/divergent V2 receipt rejects; there is no v1 fail-open.
            return bool(protocol2_gate(flag, raw_output))
        return True

    _ORIGIN_URL_RE = re.compile(r"https?://[^\s\"'<>\)\\]+")

    def _sanctioned_origin_hosts(self) -> "set[str]":
        """Hosts a flag may be extracted FROM: the live challenge target (an
        operator redirect's override wins). Computation over provisioned
        workspace files needs no host entry; a file challenge (no target)
        sanctions NO network origin."""
        out: set[str] = set()
        raw = self._target() or ""
        if raw:
            parsed = urlsplit(raw if "://" in raw else f"tcp://{raw}")
            host = (parsed.hostname or "").lower()
            if host:
                out.add(host)
                if parsed.port:
                    out.add(f"{host}:{parsed.port}")
        return out

    def _origin_tainted(self, flag: str) -> "Optional[str]":
        """Why this flag's evidence chain fails target-identity, else None.

        1. WORKER FOREKNOWLEDGE — the value appears in one of the worker's
           COMMANDS before it appears in ANY tool output: the worker wrote
           the flag before recovering it (self-manufactured resource — the
           round-10 run-7 chain: echo the writeup flag into a self-hosted
           flag.php, then 'extract' it back). Extraction must precede
           knowledge; equal-index (an echo FOUND_FLAG= report) is fine.
        2. UNSANCTIONED ENDPOINT — EVERY output record containing the flag
           references an unsanctioned network endpoint: the bytes only ever
           originated from outside the sanctioned set (a public writeup
           fetch, a self-hosted copy on 127.0.0.1 with no declared target).
           A writeup READ for hints followed by a real extraction from the
           declared target stays clean — the target record is a clean
           occurrence. Pure local computation has no endpoint and is clean."""
        if not flag or not self._raw_tool_outputs:
            return None
        first_cmd: "Optional[int]" = None
        first_out: "Optional[int]" = None
        occurrences: list[int] = []
        for idx, out in enumerate(self._raw_tool_outputs):
            if (idx < len(self._raw_tool_attributed)
                    and self._raw_tool_attributed[idx]
                    and flag in out):
                if first_out is None:
                    first_out = idx
                occurrences.append(idx)
        for idx, cmd in enumerate(self._raw_tool_commands):
            if (idx < len(self._raw_tool_attributed)
                    and self._raw_tool_attributed[idx]
                    and flag in cmd):
                first_cmd = idx
                break
        if first_cmd is not None and (first_out is None or first_cmd < first_out):
            return (
                "worker foreknowledge: the flag value appeared in a worker "
                "command before any tool output produced it (self-manufactured "
                "resource — planted file or self-seeded service)")
        if not occurrences:
            return None
        sanctioned = self._sanctioned_origin_hosts()
        first_tainted_reason: "Optional[str]" = None
        for idx in occurrences:
            cmd = (self._raw_tool_commands[idx]
                   if idx < len(self._raw_tool_commands) else "")
            record = cmd + "\n" + self._raw_tool_outputs[idx]
            record_clean = True
            for url in self._ORIGIN_URL_RE.findall(record):
                parsed = urlsplit(url)
                host = (parsed.hostname or "").lower()
                if not host:
                    continue
                hp = f"{host}:{parsed.port}" if parsed.port else host
                if host not in sanctioned and hp not in sanctioned:
                    record_clean = False
                    if first_tainted_reason is None:
                        first_tainted_reason = (
                            f"unsanctioned origin: flag evidence traces to "
                            f"{url[:80]} — not the declared target "
                            f"({self._target() or 'none (file challenge)'})")
            if record_clean:
                return None  # at least one genuine-origin occurrence
        return first_tainted_reason

    def _note_flag_origin_rejected(self, flag: str, reason: str) -> None:
        """Board-visible record of an origin-attestation rejection (once per
        flag value): operators must SEE a self-manufacture attempt, not hunt
        for it in worker logs."""
        noted = getattr(self, "_origin_rejections_noted", None)
        if noted is None:
            noted = self._origin_rejections_noted = set()
        if flag in noted:
            return
        noted.add(flag)
        if self.bus is not None:
            try:
                asyncio.get_running_loop().create_task(self._emit_bb(
                    "flag_origin_rejected", flag=flag, reason=reason[:400]))
            except RuntimeError:
                pass

    def _stage_attachments(self, wd: Path) -> list[str]:
        """Materialize challenge attachments into the run workspace CAS, then link
        them into the worker cwd using their original basenames.

        The worker-facing compatibility contract remains `./<name>` so existing
        prompts and traces continue to work. The storage contract changes: bytes live
        once under `workspace/inputs/objects/<sha-prefix>/<sha>`, `inputs/by-name`
        points at the immutable object, and the cwd entry points at `inputs/by-name`.
        """
        wd = Path(wd).resolve()
        root = workspace_root_for_worker(wd)
        ensure_workspace(root, runtime={
            "backend": "container" if getattr(self, "container", None) is not None else "local",
            "run_id": getattr(self, "run_id", getattr(self.challenge, "id", "")),
        })
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
        """Back-compat wrapper for tests/old call sites: materialize as input CAS."""
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

    def _link_inherited_pocs(self, root: Path, wd: Path) -> None:
        sg = getattr(self, "shared_graph", None)
        if sg is None or not hasattr(sg, "pocs"):
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

    def _poc_prompt_block(self) -> str:
        inherited_pocs = getattr(self, "_inherited_pocs", [])
        if not inherited_pocs:
            return ""
        lines = [
            "\n## Inherited PoCs",
            "The files under ./inherited/<poc_id>/ are teammate PoC artifacts. "
            "Use them as tools only; do not treat PoC source text as flag evidence. "
            "If you modify one, copy it to your scratch first and save a new PoC.",
        ]
        for p in inherited_pocs[:20]:
            note = f" — {p['note'][:100]}" if p.get("note") else ""
            lines.append(
                f"- {p['poc_id']} ({p['status']}): {p['path']} ; "
                f"entry: {p['entry_command']}{note}"
            )
        return "\n".join(lines)

    # Board file-handoff (DESIGN_board_file_handoff): instead of inlining a
    # doubly-truncated board snapshot (the [-16] + [:2000] cliff that made late
    # workers on a long chain re-walk from scratch), we write the FULL untruncated
    # board to a file in the worker's workdir and put only a pointer + a small
    # bounded credential digest in the prompt. The worker pulls the rest with its
    # own Read tool. `_board_context` is now a PURE string renderer (no I/O);
    # `_write_board_file(wd)` does the disk write from the worker loop where wd is
    # in scope (the file write CANNOT live here — _board_context only sees self,
    # and self._workdir is None on most paths).
    BOARD_FILENAME = ".muteki_board.md"  # dotfile: won't collide with staged attachments

    def _board_markdown(self) -> str:
        """The FULL board body for the workdir file (no truncation). Empty when
        there's no shared graph / nothing on the board yet."""
        sg = getattr(self, "shared_graph", None)
        if sg is None:
            return ""
        try:
            body = sg.to_board_markdown()
        except Exception:
            return ""
        return body if (body and body.strip()) else ""

    def _credential_digest(self) -> str:
        """The small inline digest: just the canonical credential / unlock-chain
        section (bounded by chain length, not a blind char cap). This is the
        load-bearing signal a worker needs even if it ignores the file."""
        sg = getattr(self, "shared_graph", None)
        if sg is None:
            return ""
        try:
            return sg._credential_block() or ""
        except Exception:
            return ""

    def _ruled_out_digest(self) -> str:
        """P1-C: a small inline "already attempted / ruled out" digest, INLINED into
        the prompt (not just the file). The board file holds the full attempted list,
        but a headless worker often doesn't Read it and re-walks old ground ("重走老
        路"). The credential chain is already inlined for the same reason; the
        ruled-out directions are equally load-bearing for "don't redo recon". Bounded
        to the most recent few so the prompt stays small."""
        sg = getattr(self, "shared_graph", None)
        if sg is None:
            return ""
        try:
            return sg._attempted_intents_block(limit=12) or ""
        except Exception:
            return ""

    def _board_pointer(self, wrote_file: bool) -> str:
        """Prompt block. When the file was written: a pointer + the inline digest.
        When it WASN'T (write failed / file tools denied): fall back to a bounded
        inline summary and emit NO pointer (never point at a missing file)."""
        digest = self._credential_digest()
        ruled_out = self._ruled_out_digest()  # P1-C: inline, not just in the file
        if wrote_file:
            if bool(getattr(self, "_protocol2_mode", False)):
                block = (
                    "\n## Shared candidate snapshot\n"
                    f"Read `./{self.BOARD_FILENAME}` for non-authoritative teammate "
                    "leads. Treat every row as a lead until host authority promotes "
                    "it from sealed execution evidence. Protocol 2 workers have no "
                    "direct database or mutation path.\n"
                )
                if digest:
                    block += digest
                if ruled_out:
                    block += ruled_out + "\n"
                return block
            block = (
                "\n## Shared team board (teammates' findings)\n"
                f"A file `./{self.BOARD_FILENAME}` in your working directory holds the "
                "FULL team board — it is your team's shared notes, NOT part of the "
                "challenge.\n"
                "READ IT FIRST: build on confirmed facts, reuse recovered "
                "credentials/passwords, and do NOT redo anything it marks a dead end "
                "or already-attempted direction.\n"
                "Before starting a new direction, also query the LIVE board with the "
                "script path in `$MUTEKI_BLACKBOARD_SCRIPT` (this is fresher than the "
                "snapshot):\n"
                "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" read-review\n"
                "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" read-deadends\n"
                "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" read-facts\n"
                "When you confirm a new objective fact, write it immediately:\n"
                "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" write-fact \"<fact>\" --verified\n"
                "When a direction is ruled out, mark it immediately:\n"
                "  python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" mark-deadend \"<reason>\"\n")
            if digest:
                block += digest
            # P1-C: inline the already-attempted directions too (not only in the file
            # the worker may skip). This is what stops a fresh bootstrap worker from
            # re-running the same recon a previous one already concluded.
            if ruled_out:
                block += ruled_out + "\n"
            return block
        # fallback: no file — inline a bounded summary so the worker isn't blind.
        sg = getattr(self, "shared_graph", None)
        summary = ""
        try:
            summary = (sg.to_summary(max_evidence=10**9) if sg else "").strip()[:2000]
        except Exception:
            summary = ""
        if not summary and not digest:
            return ""
        out = "\n## Shared team board (what your teammates already found)\n"
        if digest:
            out += digest
        if summary:
            out += ("Build on confirmed facts; do NOT re-investigate anything marked a "
                    f"dead end.\n{summary}\n")
        if ruled_out:
            out += ruled_out + "\n"
        return out

    def _board_context(self) -> str:
        """Back-compat string renderer. Returns the prompt block ASSUMING the board
        file was written (the worker loop writes it before building the prompt, see
        _write_board_file). When no file could be written this still degrades to the
        inline fallback via _board_pointer(wrote_file=False) — but the common path is
        wrote_file=True, set per-turn by the loop through self._board_file_written."""
        return self._board_pointer(bool(getattr(self, "_board_file_written", False)))

    def _intent_neighborhood_context(self) -> str:
        sg = getattr(self, "shared_graph", None)
        intent_id = getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or ""
        if sg is None or not intent_id:
            return ""
        try:
            return sg.intent_neighborhood_block(intent_id) or ""
        except Exception:
            return ""

    @staticmethod
    def _workspace_protocol_block() -> str:
        return (
            "\n## Workspace sharing protocol\n"
            "`./shared/` is the team's shared artifact area for reusable outputs, "
            "payloads, notes, and PoCs. Attached input files are immutable source "
            "material; if you need to modify one, copy it into your own scratch "
            "file first. Publish reusable results through explicit shared/PoC "
            "markers instead of editing original inputs. Inherited teammate PoCs, "
            "when present, are mounted read-only under `./inherited/<poc_id>/` and "
            "must be treated as tools, never as flag evidence.\n"
        )

    def _write_board_file(self, wd: "Path") -> bool:
        """Write the FULL board once at run-workspace root, then symlink it into cwd.
        Returns True on success.
        Called from the worker loop at each turn boundary (wd in scope). On ANY
        failure (no graph, permissions, denied file tools) returns False so the
        caller emits the inline fallback instead of a dangling pointer.

        Collision: if a same-named NON-board file exists (a staged attachment or a
        worker scratch file), write to a suffixed name and skip — never clobber it.
        We tag our own files with a sentinel first line to tell them apart."""
        body = self._board_markdown()
        self._board_file_written = False
        if not body:
            return False
        try:
            wd = Path(wd).resolve()
            wd.mkdir(parents=True, exist_ok=True)
            root = workspace_root_for_worker(wd)
            ensure_workspace(root)
            board_path = root / self.BOARD_FILENAME
            path = wd / self.BOARD_FILENAME
            sentinel = "<!-- muteki-team-board -->\n"
            if path.exists():
                head = ""
                try:
                    head = path.read_text(errors="ignore")[:64]
                except Exception:
                    head = ""
                if sentinel.strip() not in head:
                    return False  # a non-board file holds this name — don't clobber
            board_path.write_text(sentinel + body)
            relative_symlink(path, board_path)
            self._board_file_written = True
            return True
        except Exception:
            self._board_file_written = False
            return False

    def _target(self) -> "Optional[str]":
        """The live target URL: an operator redirect's _target_override wins over the
        Challenge's original target (the challenge moved / a new host was given)."""
        return getattr(self, "_target_override", None) or self.challenge.target

    # P0 defect-4: char budget for injected standing guidance. The swarm LRU-caps
    # the COUNT (_STANDING_MAX); this caps the TOTAL CHARS in the prompt as a second
    # guard so a few very long hints still can't bloat it (the 36k-token claude
    # empty-exit). Most-recent hints win when over budget.
    _STANDING_CHAR_BUDGET = 4000

    def _finalize_control_context_prompt_manifest(self, prompt: str) -> None:
        """Fence durable context against the *rendered* invocation prompt.

        Context is reserved before worker construction, while ``_standing_block``
        enforces a separate size budget later.  This final pre-Popen reconciliation
        is therefore the first point where we can truthfully say which reservations
        are represented in the bytes handed to the CLI.  Missing optional rows are
        released (restoring one-shot capacity); missing exact-continuation rows abort
        the launch instead of manufacturing a delivery receipt.
        """
        manifest = list(getattr(self, "_control_context_prompt_manifest", []) or [])
        if not manifest:
            return
        pending = set(self._pending_control_context_reservations)
        worker_id = str(self._context_binding_worker_id or self.solver_id)
        included: list[tuple[str, str]] = []
        dropped: list[tuple[str, str]] = []
        included_secret_texts: set[str] = set()
        dropped_texts: set[str] = set()
        for item in manifest:
            reservation = (
                str(item.get("context_id") or ""),
                str(item.get("reservation_id") or ""),
            )
            if reservation not in pending:
                continue
            text = str(item.get("text") or "")
            if text and text in prompt:
                included.append(reservation)
                if bool(item.get("secret")):
                    included_secret_texts.add(text)
                continue
            if bool(item.get("required")):
                raise RuntimeError(
                    "required operator continuation context is absent from the "
                    "rendered worker prompt")
            releaser = self._context_releaser
            if not callable(releaser):
                raise RuntimeError(
                    "optional operator context was omitted but its reservation "
                    "cannot be released")
            try:
                released = releaser(
                    reservation[0], worker_id=worker_id,
                    reservation_id=reservation[1])
            except Exception as exc:
                raise RuntimeError(
                    "optional operator context reservation release failed") from exc
            if released is not True:
                raise RuntimeError(
                    "optional operator context reservation release was not confirmed")
            dropped.append(reservation)
            if text:
                dropped_texts.add(text)

        if dropped:
            dropped_set = set(dropped)
            self._pending_control_context_reservations = [
                reservation
                for reservation in self._pending_control_context_reservations
                if reservation not in dropped_set
            ]
            # Dropped secrets never enter argv/stdin and therefore must not force a
            # secure-transport preflight.  Retain any value represented by another
            # included manifest row.
            self._control_secret_values = [
                value for value in self._control_secret_values
                if value not in dropped_texts or value in included_secret_texts
            ]
            if dropped_texts:
                self._operator_context_texts = [
                    value for value in self._operator_context_texts
                    if value not in dropped_texts or value in prompt
                ]
        self._control_context_prompt_included = tuple(included)
        self._control_context_prompt_manifest_finalized = True

    def _standing_block(self) -> str:
        """Persistent operator guidance (VPS/SSH creds, global constraints) folded
        into every turn's prompt. Empty when none set. Bounded to the most recent
        hints within _STANDING_CHAR_BUDGET chars (defect-4)."""
        sg = getattr(self, "_standing_guidance", None)
        if not sg:
            return ""
        # Exact continuation context is not optional background: reserve its full
        # text first, even when it alone exceeds the ordinary standing budget.  The
        # final invocation fence below will reject the launch if it somehow fails to
        # appear.  All other guidance retains the most-recent-first budget policy.
        required_texts = {
            str(item.get("text") or "")
            for item in (getattr(
                self, "_control_context_prompt_manifest", []) or [])
            if bool(item.get("required")) and item.get("text")
        }
        if required_texts:
            kept_indices = {
                index for index, value in enumerate(sg)
                if value in required_texts
            }
            used = sum(len(sg[index]) + 3 for index in kept_indices)
            for index in range(len(sg) - 1, -1, -1):
                if index in kept_indices:
                    continue
                cost = len(sg[index]) + 3
                if used + cost > self._STANDING_CHAR_BUDGET:
                    continue
                kept_indices.add(index)
                used += cost
            kept = [value for index, value in enumerate(sg)
                    if index in kept_indices]
            body = "\n".join(f"- {s}" for s in kept)
            return ("\n## Operator standing guidance (applies to ALL your work):\n"
                    f"{body}\n")

        # keep the most recent hints that fit the char budget (iterate newest-first)
        kept: list[str] = []
        used = 0
        for s in reversed(sg):
            cost = len(s) + 3  # "- " + newline
            if kept and used + cost > self._STANDING_CHAR_BUDGET:
                break
            kept.append(s)
            used += cost
        kept.reverse()
        body = "\n".join(f"- {s}" for s in kept)
        return ("\n## Operator standing guidance (applies to ALL your work):\n"
                f"{body}\n")

    def _verifier_rate_limited(self) -> bool:
        return bool(getattr(self.challenge, "verifier_rate_limited", False))

    def bind_cognitive_context(
        self,
        packet: Any,
        *,
        stage_port: Any = None,
        invocation_runner: Any = None,
    ) -> None:
        """Install one host-compiled packet and its narrow launch interface.

        A bare binding remains useful for prompt-render tests, but it cannot execute.
        A real Protocol 2 C6 attempt receives an exact ``C6InvocationRunner`` before
        admission.  The old direct stage port was removed because an arbitrary PID
        must never be enough to manufacture a canonical local-Popen receipt.
        """

        from muteki.runtime.cognition import DeliveredContextPacketV1
        from muteki.runtime.c6_transport import C6InvocationRunner

        if type(packet) is not DeliveredContextPacketV1:
            raise TypeError("packet must be DeliveredContextPacketV1")
        if stage_port is not None:
            raise RuntimeError("direct C6 stage ports are retired; use the host runner")
        if invocation_runner is not None and type(invocation_runner) is not C6InvocationRunner:
            raise TypeError("invocation_runner must be C6InvocationRunner or None")
        if self._cognitive_context_packet is not None:
            if self._cognitive_context_packet.binding.digest != packet.binding.digest:
                raise RuntimeError("cognitive context is already bound")
            if (
                invocation_runner is not None
                and self._c6_invocation_runner is not None
                and self._c6_invocation_runner is not invocation_runner
            ):
                raise RuntimeError("cognitive invocation runner is already bound")
            if invocation_runner is not None:
                self._c6_invocation_runner = invocation_runner
            return
        if packet.packet.manifest.run_id != self.run_id:
            raise ValueError("cognitive context belongs to another run")
        self._cognitive_context_packet = packet
        self._c6_invocation_runner = invocation_runner

    def cognitive_context_stage_confirmed(self) -> bool:
        if self._cognitive_context_packet is None:
            return True
        with self._cognitive_stage_lock:
            return bool(self._c6_invocation_runner is not None and (
                self._cognitive_stage_released and not self._cognitive_stage_unknown
            ))

    def _stage_cognitive_context_prompt(self, prompt: str, *, transport: str) -> None:
        if self._cognitive_context_packet is None:
            return
        if type(prompt) is not str or not prompt:
            raise ValueError("materialized cognitive prompt must be non-empty")
        if self._c6_invocation_runner is not None:
            if transport != "argv":
                raise RuntimeError("C6 Phase A requires argv prompt transport")
            # The broker seals/stages only after final runtime argv materialisation.
            # Holding the exact prompt here grants no write/effect authority.
            with self._cognitive_stage_lock:
                self._cognitive_pending_prompt = prompt
                self._cognitive_stage_released = False
                self._cognitive_stage_unknown = False
            return
        raise RuntimeError("strict C6 execution requires the host-owned invocation runner")

    def _bind_cognitive_context_invocation(self, argv: list[str]) -> None:
        """Seal the exact locally constructed argv before ``Popen`` is reachable."""

        if self._cognitive_context_packet is None:
            return
        del argv
        raise RuntimeError("C6 host broker owns final argv invocation binding")

    def _mark_cognitive_context_stage_unknown(self, *, reason: str) -> None:
        if self._cognitive_context_packet is None:
            return
        del reason
        with self._cognitive_stage_lock:
            if self._cognitive_stage_released or self._cognitive_stage_unknown:
                return
            self._cognitive_stage_unknown = True
        self._cancel_event.set()
        self.cancel()

    def _record_cognitive_context_release(self, proc: Any) -> bool:
        if self._cognitive_context_packet is None:
            return True
        del proc
        # Only C6HostPopenAdapter can now write a C6 RELEASED receipt.  A passive
        # ContextPacket without that runner is intentionally non-executable.
        return self._c6_invocation_runner is not None

    def _cognitive_context_block(self) -> str:
        packet = getattr(self, "_cognitive_context_packet", None)
        if packet is None:
            return ""
        from muteki.runtime.cognition import DeliveredContextPacketV1

        if type(packet) is not DeliveredContextPacketV1:
            raise RuntimeError("cognitive context binding changed type")
        return packet.render_for_prompt()

    def _verifier_locked_now(self) -> bool:
        """True while a broadcast verifier cooldown/burn-lockout is still in force."""
        return self._verifier_locked_until > time.time()

    def _submit_blocked_now(self) -> bool:
        """True while a sibling holds the submit-lock (advisory hold, self-clearing)."""
        return self._submit_blocked_until > time.time()

    def _submit_gate_block(self) -> str:
        """Prompt block for a rate-limited-verifier challenge. Empty unless the
        challenge opted in (verifier_rate_limited) — so every ordinary CTF prompt
        is byte-identical. Encodes the submission discipline the burn-lockout
        punishes: self-check offline to the max FIRST, treat each verifier run as a
        scarce shared resource, and HOLD submission while a teammate is submitting
        or the verifier is cooling down."""
        if not self._verifier_rate_limited():
            return ""
        lines = [
            "\n## Verifier submission discipline (this target rate-limits submissions)",
            "The target's scoring verifier punishes wrong/concurrent submissions with "
            "a per-player burn-lockout (a handful of wrong tries → locked out for a "
            "long cooldown, shared across the whole team and across SSH sessions). "
            "Treat each verifier run as an EXPENSIVE, SCARCE, SHARED resource:",
            "1. Before you EVER run the verifier, validate your answer OFFLINE to the "
            "max — build a local checker, cross-check every field/count/format, "
            "exclude decoys. When you believe it is 100% correct, print one line "
            "`READY_TO_SUBMIT=<one-line self-check summary>` and only THEN run the "
            "verifier ONCE.",
            "2. Do NOT run the verifier to 'see what it says' or to probe — a wrong "
            "submission burns the shared budget for everyone.",
            "3. For any calibration/confidence field, prefer CONSERVATIVE (under- not "
            "over-claim) — over-claiming trips the verifier's canary and burns a try.",
            "4. If the verifier reports a cooldown / lockout / 'try again in N', STOP "
            "submitting. Print `DEADEND=verifier locked for <N>` and spend the cooldown "
            "improving the answer — do NOT re-run it on a timer.",
        ]
        if self._verifier_locked_now():
            remain = int(self._verifier_locked_until - time.time())
            lines.append(
                f"5. ⚠️ The verifier is CURRENTLY locked (~{remain}s left, a teammate "
                "hit the cooldown). Do NOT submit now. Use this time to perfect your "
                "answer offline so the next single submission lands.")
        elif self._submit_blocked_now():
            lines.append(
                "5. ⚠️ A teammate is submitting RIGHT NOW. Hold your own submission "
                "until their result comes back (it will appear on the board); keep "
                "refining your answer meanwhile, do NOT run the verifier yet.")
        return "\n".join(lines) + "\n"

    def _engagement_goal(self) -> str:
        """The goal string used for the blackboard intent + as the anchor Reason
        judges completion against. CTF keeps the original wording (so the CTF path
        is byte-identical); pentest uses the operator's goal."""
        c = self.challenge
        if getattr(c, "mode", "ctf") == "pentest":
            return (c.goal.strip() if getattr(c, "goal", "") else
                    f"Find and prove exploitable vulnerabilities in {c.name}.")
        return f"Solve {c.name} [{c.category}]"

    def _engagement_scope(self) -> str:
        c = self.challenge
        if getattr(c, "mode", "ctf") != "pentest":
            return ""
        if getattr(c, "scope", "") and c.scope.strip():
            return c.scope.strip()
        tgt = self._target()
        if tgt:
            return tgt
        return "Only the target/files provided above. Ask if unsure."

    def _box_mode_line(self) -> str:
        staged = list(getattr(self, "_staged_files", None) or [])
        attachments = list(getattr(self.challenge, "attachments", None) or [])
        if staged or attachments:
            return (
                "White-box: attached files in the working directory may be reviewed "
                "as source. The live origin in Scope is still the only system you test."
            )
        return (
            "Black-box: test the live HTTP origin only. Do not search the host "
            "filesystem for challenge source."
        )

    def _review_engagement_block(self) -> str:
        if getattr(self.challenge, "mode", "ctf") != "pentest":
            return ""
        return (
            "## Engagement goal\n"
            f"{self._engagement_goal()}\n\n"
            "## Scope / authorization\n"
            f"{self._engagement_scope()}\n\n"
            "Review does not accept findings or reports. Stay inside scope. "
            "Reproduction and value judgment are separate from Review.\n\n"
        )

    def _explore_conclude_text(self) -> str:
        if self.mode == "verifier":
            return (
                "CONCLUDE: stop now. If YOUR command output already contains the "
                "report witness, print REPRODUCED=yes and REPRO_WITNESS=<snippet>. "
                "Otherwise print REPRODUCED=no and REPRO_REASON=<why>."
            )
        if getattr(self.challenge, "mode", "ctf") == "pentest":
            return _PENTEST_EXPLORE_CONCLUDE_PROMPT
        return _EXPLORE_CONCLUDE_PROMPT

    def _resume_text(self) -> str:
        if getattr(self.challenge, "mode", "ctf") == "pentest":
            return _PENTEST_RESUME_PROMPT
        return _RESUME_PROMPT

    def _build_prompt(self) -> str:
        c = self.challenge
        ctx_lines = [f"Challenge: {c.name} [{c.category}]"]
        tgt = self._target()
        if tgt:
            ctx_lines.append(f"Target: {tgt}")
        if getattr(self, "_staged_files", None):
            ctx_lines.append(
                "Attached files (already in your working directory — inspect them "
                "FIRST): " + ", ".join(self._staged_files))
        if c.description:
            ctx_lines.append(f"Brief: {c.description.strip()[:600]}")
        if not bool(getattr(self, "web_access", True)):
            ctx_lines.append(
                "\n## Offline black-box evaluation boundary\n"
                "Use only the challenge target and files already staged in your "
                "current working directory. Do NOT inspect parent directories, "
                "benchmark metadata, challenge manifests, README solution text, "
                "reference solvers, generators containing answers, other run logs, "
                "or agent/session history. If a required player-facing file is not "
                "staged here, report it missing instead of searching the host."
            )
        cognitive = self._cognitive_context_block()
        if cognitive:
            ctx_lines.append(cognitive)
        if bool(getattr(self, "_protocol2_mode", False)):
            return _PROTOCOL2_CANARY_PROMPT.format(
                ctx="\n".join(ctx_lines), fmt=self._flag_hint())
        board = self._board_context()
        if board:
            ctx_lines.append(board)
        neighborhood = self._intent_neighborhood_context()
        if neighborhood:
            ctx_lines.append(neighborhood)
        ctx_lines.append(self._workspace_protocol_block())
        poc_block = self._poc_prompt_block()
        if poc_block:
            ctx_lines.append(poc_block)
        standing = self._standing_block()
        if standing:
            ctx_lines.append(standing)
        # rate-limited verifier → submission discipline + live submit-lock / cooldown
        # status. Empty for ordinary challenges (byte-identical prompt).
        gate = self._submit_gate_block()
        if gate:
            ctx_lines.append(gate)
        # re-bootstrap: a course-correction direction from Reason. Steer the rush
        # without narrowing it to a single Explore intent. getattr-guarded so a
        # solver built via __new__ in tests stays safe.
        if getattr(self, "intent_goal", ""):
            ctx_lines.append(
                "\n## Course correction (the run drifted — focus here):\n"
                f"{self.intent_goal[:600]}")
        # defect-2: multi-flag PROGRESS (N/total from the shared graph). Unified into
        # _team_context_block so explore/resume get it too, not just bootstrap.
        team = self._team_context_block()
        if team:
            ctx_lines.append(team)
        rejected = self._rejected_flags_block()
        if rejected:
            ctx_lines.append(rejected)
        # pentest mode → goal-driven prompt (no flag); else the unchanged CTF prompt.
        if getattr(c, "mode", "ctf") == "pentest":
            ctx_lines.append(self._box_mode_line())
            return _PENTEST_EXEC_PROMPT.format(
                ctx="\n".join(ctx_lines),
                kb=_KB_PROMPT if self.kb else "",
                goal=self._engagement_goal(),
                scope=self._engagement_scope())
        return _EXEC_PROMPT.format(
            ctx="\n".join(ctx_lines),
            kb=_KB_PROMPT if self.kb else "",
            fmt=self._flag_hint())

    def _extract_flag(self, text: str) -> Optional[str]:
        """Extract a flag the worker EXPLICITLY claimed via a FOUND_FLAG= marker.

        We deliberately do NOT blind-scan the transcript for a flag_format-shaped
        token. That fallback was the source of every false positive: the worker
        writes the flag SHAPE in prose ("scanning pages for flag{...}",
        "icon={name}.ico", or even a whole sentence `flag{`, which means …}`) and a
        greedy `\\{[^}]{1,200}\\}` scoops it up as the answer (run-1619 flag{...},
        run-3613 {name}, run-4305 a full reasoning sentence). The flag is the
        worker's CLAIM, signalled by the marker — not any brace-shaped substring.
        If the worker really found it but forgot the marker, the conclude/resume
        turn re-asks for FOUND_FLAG=. Provenance + placeholder checks in the gate
        are the second line of defense on whatever the marker yields."""
        from muteki.solver.gate import is_placeholder_flag
        cand: Optional[str] = None
        # take the LAST FOUND_FLAG= marker (the conclude turn's is the most final).
        for m in _FLAG_LINE.finditer(text):
            raw = m.group(1).strip()
            if raw.upper().startswith("NONE"):
                continue
            tok = _clean_flag_token(raw)
            if tok and tok != "NONE":
                cand = tok
        if cand is None:
            return None
        # the marker value should itself be a single flag-shaped token; if the
        # model appended trailing prose on the same line, _FLAG_LINE's \\S+ already
        # stopped at whitespace, so `cand` is the bare token. Reject placeholders.
        if is_placeholder_flag(cand):
            return None
        return cand

    def _expected_flags(self) -> int:
        return max(1, getattr(self.challenge, "expected_flags", 1) or 1)

    def _known_flags(self) -> "list[str]":
        """All flags the RUN already holds — union of this worker's accepted set and
        the SHARED graph's flags (defect-0 made the graph the durable source). The
        union closes the staleness window: a sibling that found a flag has it on the
        shared graph even before this worker's _already_found was seeded."""
        known = set(self._already_found)
        sg = getattr(self, "shared_graph", None)
        if sg is not None:
            try:
                known.update(sg.snapshot().flags or [])
            except Exception:
                pass
        return sorted(known)

    def _team_context_block(self) -> str:
        """defect-2: the multi-flag PROGRESS block — how many flags the challenge
        has, how many the team already holds (from the shared graph, not just this
        worker), and how many remain. Injected into EVERY prompt builder (bootstrap/
        exec, explore, resume) so an explore worker also knows N/total and doesn't
        stop after one. Empty for a single-flag challenge (byte-identical prompt)."""
        n = self._expected_flags()
        if getattr(self.challenge, "mode", "ctf") == "pentest":
            eg = engagement_goal_of(self.challenge)
            if eg.quantity == "recon":
                return ""
            want = max(1, int(eg.expected_findings or 1))
            if eg.quantity == "collect" and want <= 1 and eg.collect_until_coverage:
                return ""
            got = 0
            sg = getattr(self, "shared_graph", None)
            if sg is not None and hasattr(sg, "accepted_reports"):
                try:
                    got = len(sg.accepted_reports() or [])
                except Exception:
                    got = 0
            remaining = max(0, want - got)
            block = [
                f"\n## This engagement needs {want} accepted report(s) "
                f"({got}/{want} in the report collection, {remaining} remaining).",
                "Write a complete JSON report file, then print SUBMIT_REPORT=<path>.",
                "FOUND_FINDING= is ignored. A report is accepted only after a "
                "different Worker reproduces it and a host-side value check passes.",
            ]
            if got and sg is not None:
                try:
                    titles = [
                        str(r.get("title") or r.get("resource_id") or r.get("report_id") or "")
                        for r in (sg.accepted_reports() or [])
                    ]
                    titles = [t for t in titles if t]
                    if titles:
                        block.append("Already accepted (do not resubmit the same issue):")
                        block += [f"  - {t}" for t in titles[:12]]
                except Exception:
                    pass
            return "\n".join(block)
        if n <= 1:
            return ""
        got = self._known_flags()
        remaining = max(0, n - len(got))
        block = [f"\n## This challenge has {n} flags — find them ALL "
                 f"({len(got)}/{n} captured, {remaining} remaining).",
                 "Keep going after each flag until the team has all of them; submit "
                 "each with `python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<flag>'` "
                 "the moment you recover it "
                 "from REAL output. Do NOT stop until all are captured or you hit a "
                 "hard wall only the operator can clear."]
        if got:
            block.append("Already found by the team (do NOT re-hunt or re-submit "
                         "these — find the remaining " + str(remaining) + "):")
            block += [f"  - {f}" for f in got]
        return "\n".join(block)

    def _rejected_flags_block(self) -> str:
        """run-75379: known-BAD flag values the operator already rejected as false
        positives. A reopened/fresh worker re-runs the producing intent from the
        verified facts, so without this it cheerfully re-derives the SAME bad value
        and re-submits it (the gate then drops it, but the worker wastes the turn and
        the operator sees churn). Telling the model the value is a confirmed dead end
        stops the re-derivation at the source. Rendered for single- AND multi-flag
        runs (a false positive happens in both); empty when nothing was rejected, so
        the prompt is byte-identical on the common path."""
        bad = sorted(self._rejected_flags())
        if not bad:
            return ""
        block = ["\n## Known-BAD flags (operator marked these FALSE POSITIVES — do "
                 "NOT submit or re-derive them):"]
        block += [f"  - {f}" for f in bad]
        block.append("These values are confirmed wrong. If your work leads back to "
                     "one, that path is a dead end — pursue a different lead.")
        return "\n".join(block)

    def _flag_hint(self) -> str:
        """The 'what a flag looks like' line for the worker prompt. A token-mode
        challenge (flag is a bare secret — a level password, an extracted value),
        NOT flag{...}, must NOT be told to hunt for `flag{...}` (run-11189: workers
        SSH'd through the levels but never emitted FOUND_FLAG= because the prompt
        said the flag is shaped like flag{...}, which this challenge has none of).
        Token mode is selected only by flag_format="token". Multi-flag is just a
        collection mode; it must not imply bare-token flags."""
        fmt = getattr(self.challenge, "flag_format", "") or ""
        hint = (getattr(self.challenge, "flag_format_hint", "") or "").strip()
        if hint and fmt != "token":
            return hint
        if fmt == "token":
            return ("a bare token recovered from REAL output (e.g. a level "
                    "password, an extracted secret) — it may NOT be wrapped in "
                    "flag{...}")
        if not fmt or r"\{" in fmt:
            return "flag{...}"
        return "a string matching the configured flag format"

    @staticmethod
    def _stderr_tail(res: CliResult, *, max_chars: int = 1800) -> str:
        err = (getattr(res, "raw_stderr", "") or "").strip()
        if not err:
            return ""
        return "\n".join(err.splitlines()[-12:])[-max_chars:]

    def _result_text_with_stderr(self, res: CliResult) -> str:
        text = res.text or ""
        if text.strip():
            return text
        tail = self._stderr_tail(res)
        if not tail:
            return text
        return f"[{self.driver.name} stderr]\n{tail}"

    async def _emit_empty_stderr_diagnostic(self, res: CliResult) -> None:
        if (res.text or "").strip():
            return
        tail = self._stderr_tail(res, max_chars=1200)
        if tail:
            await self._emit(
                EventType.REASONING_DELTA,
                text=f"[{self.driver.name}] produced no stdout; stderr tail:\n{tail}\n",
            )

    def _pentest_findings_ready(self) -> bool:
        """True after this worker submitted at least one complete report.

        Product success is still Coordinator accept (repro + value). This only
        stops the submitter from concluding as a CTF 'no flag' miss.
        """
        if getattr(self.challenge, "mode", "ctf") != "pentest":
            return False
        return bool(self._already_submitted_reports)

    async def _finish_pentest_finding(
        self, *, session: "Optional[str]", wd: Path, steps: int, label: str,
    ) -> SolveOutcome:
        lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
        self._conclude_intent_db(
            result=RESULT_SOLVED, to_fact_seq=lfs,
            result_detail="Exploit report submitted.")
        await self._emit_bb(
            "intent_concluded", intent_id=self._intent_id,
            worker=self.solver_id, result=RESULT_SOLVED,
            to_fact_seq=lfs,
            result_detail="Exploit report submitted.")
        self._note_worker_stop("solved")
        await self._emit_finished(flag=None, flags=[], solved=True)
        return SolveOutcome(
            True, None, steps, self.graph,
            f"solved via {self.driver.name} {label}",
            session=session, engine=self.driver.name, workdir=str(wd),
            flags=[])

    async def _stream_finding_claims(
        self, text: str, provenance: "Optional[str]",
    ) -> None:
        """Pentest: FOUND_FINDING= is no longer a success channel."""
        if getattr(self.challenge, "mode", "ctf") != "pentest":
            return
        if self.mode == "verifier":
            return
        blobs = [text or ""]
        if provenance and provenance != text:
            blobs.append(provenance)
        seen: set[str] = set()
        for blob in blobs:
            for claim in self._extract_findings(blob):
                key = _finding_key(claim)
                if not key or key in seen:
                    continue
                seen.add(key)
                await self._emit_bb(
                    "finding_rejected",
                    finding_class=claim.get("finding_class", ""),
                    reason="submit_report_required")

    def _workdir_path(self) -> "Optional[Path]":
        for raw in (getattr(self, "_current_workdir", None), getattr(self, "_workdir", None)):
            if not raw:
                continue
            try:
                return Path(raw).resolve()
            except OSError:
                continue
        return None

    def _workdir_roots(self) -> list[Path]:
        roots: list[Path] = []
        seen: set[Path] = set()
        for raw in (getattr(self, "_current_workdir", None), getattr(self, "_workdir", None)):
            if not raw:
                continue
            try:
                path = Path(raw).resolve()
            except OSError:
                continue
            if path in seen:
                continue
            seen.add(path)
            roots.append(path)
        return roots

    @staticmethod
    def _path_inside(path: Path, root: Path) -> bool:
        try:
            path.resolve().relative_to(root.resolve())
            return True
        except (ValueError, OSError):
            return False

    def _resolve_report_path(self, raw: str) -> "Optional[Path]":
        text = (raw or "").strip().strip("`\"'")
        if not text or text.startswith("<") or text.endswith(">"):
            return None
        roots = self._workdir_roots()
        wd = roots[0] if roots else None
        path = Path(text)
        candidates: list[Path] = []
        if path.is_absolute():
            candidates.append(path)
        elif wd is not None:
            candidates.append(wd / path)
        name = Path(text).name
        for root in roots:
            if name:
                candidates.append(root / name)
                try:
                    candidates.extend(root.glob(name))
                    candidates.extend(root.glob(f"*/{name}"))
                    candidates.extend(root.glob(f"*/*/{name}"))
                except OSError:
                    pass
        seen: set[Path] = set()
        for cand in candidates:
            try:
                resolved = cand.resolve()
            except OSError:
                continue
            if resolved in seen or not resolved.is_file():
                continue
            seen.add(resolved)
            if name and resolved.name != name:
                continue
            if roots and not any(self._path_inside(resolved, root) for root in roots):
                continue
            return resolved
        return None

    async def _stream_report_submissions(self, text: str) -> None:
        if getattr(self.challenge, "mode", "ctf") != "pentest":
            return
        if self.mode in {"review", "verifier"}:
            return
        for match in _SUBMIT_REPORT_LINE.finditer(text or ""):
            await self._submit_report_path(match.group(1), final=False)

    async def _finalize_report_submissions(self) -> None:
        if getattr(self.challenge, "mode", "ctf") != "pentest":
            return
        if self.mode in {"review", "verifier"}:
            return
        pending = list(self._pending_report_paths)
        self._pending_report_paths = []
        for raw in pending:
            await self._submit_report_path(raw, final=True)

    async def _submit_report_path(self, raw_path: str, *, final: bool = True) -> None:
        text = (raw_path or "").strip().strip("`\"'")
        if not text or text.startswith("<") or text.endswith(">"):
            return
        path = self._resolve_report_path(text)
        if path is None:
            if not final:
                self._pending_report_paths.append(text)
                return
            await self._emit_bb(
                "report_rejected",
                code=VALUE_REJECT_INCOMPLETE,
                reason="report file missing or outside the worker directory")
            if self.shared_graph is not None:
                try:
                    self.shared_graph.report_rejected(
                        actor=self.solver_id, report_id="unknown",
                        code=VALUE_REJECT_INCOMPLETE,
                        detail="report file missing or outside the worker directory",
                        intent_id=getattr(self, "_intent_id", "") or None)
                except Exception:
                    pass
            return
        reports = parse_report_files(path)
        if not reports:
            await self._reject_report_payload(
                {}, VALUE_REJECT_INCOMPLETE, "incomplete: parse", path)
            return
        for report in reports:
            await self._accept_or_reject_report(report, path)

    async def _reject_report_payload(
        self, report: dict, code: str, detail: str, path: "Optional[Path]" = None,
    ) -> None:
        await self._emit_bb(
            "report_rejected",
            report_id=(report or {}).get("report_id", ""),
            title=(report or {}).get("title", ""),
            code=code,
            reason=detail)
        if self.shared_graph is not None:
            try:
                self.shared_graph.report_rejected(
                    actor=self.solver_id,
                    report_id=str((report or {}).get("report_id") or (path.name if path else "unknown")),
                    code=code, detail=detail,
                    intent_id=getattr(self, "_intent_id", "") or None)
            except Exception:
                pass

    async def _accept_or_reject_report(self, report: dict, path: Path) -> None:
        try:
            code = completeness_code(
                report,
                scope=getattr(self.challenge, "scope", "") or "",
                target=self._target() or "",
            )
        except Exception as exc:
            await self._reject_report_payload(
                report, VALUE_REJECT_INCOMPLETE,
                f"completeness check failed: {exc}"[:200], path)
            return
        if code != "ok":
            missing = missing_report_fields(report or {})
            detail = code if code != VALUE_REJECT_INCOMPLETE else (
                "incomplete: " + ",".join(missing or ["parse"])
            )
            await self._reject_report_payload(report, code, detail, path)
            return
        report_id = str(report.get("report_id") or "")
        if not report_id or report_id in self._already_submitted_reports:
            return
        self._already_submitted_reports.add(report_id)
        report["submitter"] = self.solver_id
        report["path"] = str(path)
        if self.shared_graph is not None:
            try:
                states = self.shared_graph.report_states()
                existing = states.get(report_id) or {}
                if existing.get("status") in {"accepted", "submitted", "reproduced", "value_accepted"}:
                    await self._emit_bb(
                        "report_rejected",
                        report_id=report_id, code=VALUE_REJECT_DUPLICATE,
                        reason="duplicate report identity")
                    self.shared_graph.report_rejected(
                        actor=self.solver_id, report_id=report_id,
                        code=VALUE_REJECT_DUPLICATE, detail="duplicate report identity",
                        intent_id=getattr(self, "_intent_id", "") or None)
                    return
                self.shared_graph.report_submitted(
                    actor=self.solver_id, report=report,
                    intent_id=getattr(self, "_intent_id", "") or None)
            except Exception:
                pass
        await self._emit_bb("report_submitted", **report_sse_fields(report))

    async def _stream_repro_decision(self, text: str) -> None:
        if self.mode != "verifier":
            return
        report_id = report_id_from_intent(
            getattr(self, "_intent_id", "") or self.intent_id_assigned or "")
        if not report_id or getattr(self, "_repro_decided", False):
            return
        reproduced = None
        for match in _REPRODUCED_LINE.finditer(text or ""):
            reproduced = match.group(1).strip().lower() == "yes"
        if reproduced is None:
            return
        self._repro_decided = True
        report = {}
        if self.shared_graph is not None:
            try:
                report = self.shared_graph.report_record(report_id) or {}
            except Exception:
                report = {}
        corpus = self._finding_evidence_corpus()
        commands = list(getattr(self, "_raw_tool_commands", []) or [])
        witness = str(report.get("witness") or "").strip()
        for match in _REPRO_WITNESS_LINE.finditer(text or ""):
            claimed = match.group(1).strip()
            if claimed:
                witness = claimed
        reason = ""
        for match in _REPRO_REASON_LINE.finditer(text or ""):
            reason = match.group(1).strip()
        ok = bool(
            reproduced
            and witness_in_corpus(str(report.get("witness") or ""), corpus)
            and replay_attempted(report, commands)
        )
        if not ok:
            if not reproduced:
                code = VALUE_REJECT_NOT_REPRODUCIBLE
                detail = reason or "verifier printed REPRODUCED=no"
            elif not replay_attempted(report, commands):
                code = VALUE_REJECT_NOT_REPRODUCIBLE
                detail = "verifier did not re-run a replay command"
            else:
                code = VALUE_REJECT_NOT_REPRODUCIBLE
                detail = "witness missing from verifier command output"
            await self._emit_bb(
                "report_repro_failed",
                report_id=report_id, code=code, reason=detail)
            if self.shared_graph is not None:
                try:
                    self.shared_graph.report_repro_decision(
                        actor=self.solver_id, report_id=report_id,
                        reproduced=False, detail=detail,
                        intent_id=getattr(self, "_intent_id", "") or None)
                except Exception:
                    pass
            return
        await self._emit_bb(
            "report_reproduced",
            report_id=report_id,
            title=report.get("title", ""),
            finding_class=report.get("finding_class", ""))
        if self.shared_graph is not None:
            try:
                self.shared_graph.report_repro_decision(
                    actor=self.solver_id, report_id=report_id,
                    reproduced=True, witness=str(report.get("witness") or ""),
                    intent_id=getattr(self, "_intent_id", "") or None)
            except Exception:
                pass

    async def _finalize_verifier_repro(self) -> None:
        if self.mode != "verifier" or getattr(self, "_repro_decided", False):
            return
        await self._stream_repro_decision(
            "REPRODUCED=no\nREPRO_REASON=verifier did not print REPRODUCED=")

    def _extract_findings(self, text: str) -> list[dict]:
        """FOUND_FINDING= claims, last-wins per finding_key."""
        found: dict[str, dict] = {}
        order: list[str] = []
        for m in _FINDING_LINE.finditer(text or ""):
            claim = parse_finding_claim(m.group(1))
            if not claim:
                continue
            key = _finding_key(claim)
            if key not in found:
                order.append(key)
            found[key] = claim
        return [found[k] for k in order]

    def _finding_ok(self, finding: dict, raw_output: str) -> bool:
        eg = engagement_goal_of(self.challenge)
        expected = eg.finding_class if getattr(self.challenge, "mode", "ctf") == "pentest" else ""
        return _gate_finding_ok(
            finding, raw_output, expected_class=expected, artifacts=self.artifacts,
            scope=getattr(self.challenge, "scope", "") or "",
            target=self._target() or "")

    async def _accept_finding(self, finding: dict) -> bool:
        if not finding:
            return False
        key = _finding_key(finding)
        if not key or key in self._already_found_findings:
            return False
        self._already_found_findings.add(key)
        if self.shared_graph is not None:
            try:
                self.shared_graph.finding_found(
                    actor=self.solver_id, finding=finding,
                    intent_id=getattr(self, "_intent_id", "") or None)
            except Exception:
                pass
        try:
            self.graph.add_finding(finding)
        except Exception:
            pass
        await self._emit_bb(
            "finding_found",
            finding_class=finding.get("finding_class", ""),
            resource_id=finding.get("resource_id", ""),
            identity_a=finding.get("identity_a", ""),
            identity_b=finding.get("identity_b", ""))
        return True

    def _extract_flags(self, text: str) -> list[str]:
        """Every distinct flag the worker explicitly claimed via FOUND_FLAG=
        markers (dedup, in marker order, placeholders rejected). Multi-flag: one
        turn can legitimately yield several flags. _extract_flag stays the
        single-flag convenience (= last marker) for back-compat callers."""
        from muteki.solver.gate import is_placeholder_flag
        out: list[str] = []
        for m in _FLAG_LINE.finditer(text):
            raw = m.group(1).strip()
            if raw.upper().startswith("NONE"):
                continue
            tok = _clean_flag_token(raw)
            if not tok or tok == "NONE" or is_placeholder_flag(tok):
                continue
            if tok not in out:
                out.append(tok)
        return out

    def _extract_structured_facts(self, text: str) -> tuple[list[str], list[str]]:
        """Parse VERIFIED_FACT= and DEADEND= markers from worker output."""
        facts, deadends = [], []
        for line in (text or "").splitlines():
            m = _VERIFIED_FACT_LINE.match(line.strip())
            if m:
                facts.append(m.group(1).strip())
            m2 = _DEADEND_LINE.match(line.strip())
            if m2:
                deadends.append(m2.group(1).strip())
        return facts, deadends

    @staticmethod
    def _extract_fact_witness(text: str) -> str:
        for line in (text or "").splitlines():
            m = _FACT_WITNESS_LINE.match(line.strip())
            if m:
                return m.group(1).strip()[:500]
        return ""

    @staticmethod
    def _fact_witnessed_in_chunk(fact: str, text: str) -> bool:
        """A VERIFIED_FACT must be backed by non-marker output in the same chunk.

        The marker line itself is the worker's claim; it cannot be its own proof.
        We accept either an exact fact-text hit in the surrounding output or a
        strong overlap of significant tokens, which keeps concise factual markers
        usable while rejecting bare assertions.
        """
        fact = (fact or "").strip()
        if not fact:
            return False
        raw_lines = []
        for line in (text or "").splitlines():
            s = line.strip()
            if (_VERIFIED_FACT_LINE.match(s) or _DEADEND_LINE.match(s)
                    or _FLAG_LINE.match(s) or _NEED_INPUT_LINE.match(s)
                    or _POC_SAVE_LINE.match(s) or _FACT_WITNESS_LINE.match(s)):
                continue
            raw_lines.append(line)
        raw = "\n".join(raw_lines).strip().lower()
        if not raw:
            return False
        fact_l = fact.lower()
        if fact_l in raw:
            return True
        tokens = [
            t for t in re.findall(r"[a-z0-9_./:-]{4,}", fact_l)
            if t not in {"http", "https", "true", "false", "with", "from",
                         "that", "this", "there", "have", "confirmed"}
        ]
        if not tokens:
            return False
        hits = sum(1 for t in dict.fromkeys(tokens) if t in raw)
        needed = max(2, int(len(set(tokens)) * 0.6 + 0.5))
        return hits >= needed

    def _extract_need_requests(self, text: str) -> list[tuple[str, str]]:
        """Parse NEED_INPUT= markers — the worker asking the operator for a resource
        / flagging an unusable environment.

        A NEED_INPUT ask is often MULTI-LINE (the worker explains what it needs and
        why). The old single-line match truncated the ask at the first newline, so
        the operator's decision card showed only the first sentence (the "...the
        data, or" cut-off). We accumulate from a NEED_INPUT= line through to the end
        of its block — a blank line, or the next FOUND_FLAG=/VERIFIED_FACT=/
        DEADEND=/POC_SAVE=/NEED_INPUT= marker — to capture the whole request.
        """
        _STOP = ("FOUND_FLAG=", "VERIFIED_FACT=", "DEADEND=", "DEAD_END=",
                 "POC_SAVE=", "READY_TO_SUBMIT=", "ALL_FLAGS_FOUND=")
        if getattr(self.challenge, "mode", "ctf") == "pentest":
            _STOP = _STOP + ("FOUND_FINDING=", "SUBMIT_REPORT=", "REPRODUCED=")
        lines = (text or "").splitlines()
        out: list[tuple[str, str]] = []
        i = 0
        while i < len(lines):
            m = _NEED_INPUT_LINE.match(lines[i].strip())
            if not m:
                i += 1
                continue
            buf = [m.group(1).strip()]
            reported_kind = ""
            i += 1
            while i < len(lines):
                nxt = lines[i].strip()
                if not nxt:  # blank line ends the block
                    break
                if _NEED_INPUT_LINE.match(nxt) or any(nxt.startswith(s) for s in _STOP):
                    break
                km = _NEED_KIND_LINE.match(nxt)
                if km:
                    reported_kind = _normalize_need_kind(km.group(1))
                    i += 1
                    continue
                buf.append(nxt)
                i += 1
            need = "\n".join(buf).strip()
            if need:
                out.append((need, reported_kind))
        return out

    def _extract_need_inputs(self, text: str) -> list[str]:
        return [need for need, _kind in self._extract_need_requests(text)]

    def _extract_ready_to_submit(self, text: str) -> list[str]:
        """Parse READY_TO_SUBMIT= markers — the worker declaring its answer passed
        offline self-check and it is about to run the rate-limited verifier."""
        out = []
        for line in (text or "").splitlines():
            m = _READY_TO_SUBMIT_LINE.match(line.strip())
            if m:
                note = m.group(1).strip()
                if note:
                    out.append(note)
        return out

    def _extract_poc_saves(self, text: str) -> list[tuple[str, str, str, str]]:
        out: list[tuple[str, str, str, str]] = []
        for line in (text or "").splitlines():
            m = _POC_SAVE_LINE.match(line.strip())
            if m:
                out.append(tuple(part.strip() for part in m.groups()))
        return out

    def _poc_flag_literals(self, body: str) -> list[str]:
        matches = set(_BRACE_FLAG.findall(body or ""))
        # flag_format="token" is a MODE SELECTOR (bare-secret flags), not a regex —
        # findall("token", body) would manufacture junk candidates.
        fmt = (self.challenge.flag_format or "")
        if fmt and fmt != "token":
            try:
                matches.update(re.findall(fmt, body or ""))
            except re.error:
                pass
        out = sorted(f for f in matches if not is_placeholder_flag(f))
        # De-glue (tsec-f03 a-04): real command output often concatenates a status
        # marker straight onto the flag (`SHELL_OKflag{...}`); the greedy prefix in
        # flag_format swallows the glue and the judge rejects the combined token.
        # Also emit the variant re-anchored at the wrapper word when one appears
        # past position 0, so BOTH forms reach the submit gate (the judge sorts out
        # which is real). The wrapper set is `flag` plus any hint-derived wrapper.
        wrappers = ["flag"]
        hint = (getattr(self.challenge, "flag_format_hint", "") or "").strip()
        hm = re.match(r"^([A-Za-z0-9_]+)\{", hint)
        if hm and hm.group(1) not in wrappers:
            wrappers.append(hm.group(1))
        extra: list[str] = []
        for f in out:
            for w in wrappers:
                idx = f.find(w + "{", 1)
                if idx > 0:
                    v = f[idx:]
                    if v not in out and not is_placeholder_flag(v):
                        extra.append(v)
        return sorted(set(out) | set(extra))

    def _sanitize_poc_body(self, body: str) -> tuple[str, bool, str]:
        quarantined = False
        note_parts: list[str] = []
        out = self._redact_control_secrets(body)
        if out != body:
            quarantined = True
            note_parts.append("operator secret redacted")
        flags = self._poc_flag_literals(out)
        for flag in flags:
            out = out.replace(flag, "<PRIOR_FLAG>")
        if flags:
            quarantined = True
            note_parts.append("flag-like literal redacted")
        if _SECRET_LITERAL_RE.search(out):
            out = _SECRET_LITERAL_RE.sub("<SECRET>", out)
            quarantined = True
            note_parts.append("secret-like literal redacted")
        return out, quarantined, "; ".join(note_parts)

    async def _handle_poc_save(self, path_text: str, entry_command: str,
                               status: str, note: str) -> None:
        cwd = (self._current_workdir
               or (Path(self._workdir).resolve() if self._workdir else None))
        if cwd is None:
            return
        try:
            src = (cwd / path_text).resolve() if not Path(path_text).is_absolute() else Path(path_text).resolve()
            src.relative_to(cwd)
        except (OSError, ValueError):
            await self._emit_bb("poc_saved", status="rejected", path=path_text,
                                note="POC_SAVE path must stay inside this worker cwd")
            return
        if not src.exists() or not src.is_file():
            await self._emit_bb("poc_saved", status="rejected", path=path_text,
                                note="POC_SAVE path is not a regular file")
            return
        marker_key = f"{src}:{entry_command}:{status}:{note}"
        if marker_key in self._published_pocs:
            return
        self._published_pocs.add(marker_key)

        # The PoC save does blocking filesystem + hashing work (read_text,
        # write_text, sha256-stream + possible copytree in materialize_shared_
        # artifact). This runs from _stream_markers → _on_step, a LIVE streaming
        # callback on the event loop, so doing it inline would stall every other
        # worker's stream during a large PoC write (#13). Push the whole sync block
        # to a thread, exactly like the subprocess paths already do.
        def _save_blocking() -> "Optional[tuple[dict, str, str]]":
            try:
                raw = src.read_text(encoding="utf-8", errors="replace")
            except OSError:
                return None
            sanitized, quarantined, sanitize_note = self._sanitize_poc_body(raw)
            status_ = (status or "available").strip().lower()
            if status_ not in {"available", "wip", "directional", "spent"}:
                status_ = "available"
            local_note = note
            if quarantined:
                status_ = "quarantined"
                local_note = "; ".join(p for p in [note, sanitize_note] if p)
            save_src = src
            if sanitized != raw:
                save_src = cwd / f".muteki_sanitized_{src.name}"
                save_src.write_text(sanitized, encoding="utf-8")
            try:
                root = workspace_root_for_worker(cwd)
                art = materialize_shared_artifact(
                    root, save_src, name=src.name, kind="poc", status=status_,
                    metadata={
                        "entry_command": entry_command,
                        "intent_id": getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", ""),
                        "solver_id": self.solver_id,
                    },
                )
            except (OSError, FileNotFoundError):
                return None
            return art, status_, local_note

        result = await asyncio.to_thread(_save_blocking)
        if result is None:
            return
        artifact, clean_status, note = result
        poc_id = f"poc-{artifact['sha256'][:12]}"
        intent_id = getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or None
        if self.shared_graph is not None:
            try:
                self.shared_graph.save_poc(
                    actor=self.solver_id,
                    poc_id=poc_id,
                    path=str(artifact["path"]),
                    artifact_id=artifact["sha256"],
                    entry_command=entry_command,
                    status=clean_status,
                    note=note,
                    intent_id=intent_id,
                    name=src.name,
                )
            except Exception:
                pass
        await self._emit_bb(
            "poc_saved", poc_id=poc_id, intent_id=intent_id, name=src.name,
            path=str(artifact["path"]), artifact_id=artifact["sha256"],
            entry_command=entry_command, status=clean_status, note=note)

    async def _mark_claimed_pocs_spent(self, reason: str) -> None:
        if self.shared_graph is None or not self._claimed_pocs:
            return
        for poc_id in list(self._claimed_pocs):
            try:
                self.shared_graph.conclude_poc(
                    actor=self.solver_id, poc_id=poc_id, status="spent",
                    note=f"direction dead-end: {reason[:160]}")
            except Exception:
                continue
            await self._emit_bb(
                "poc_concluded", poc_id=poc_id, status="spent",
                note=f"direction dead-end: {reason[:160]}")

    def _build_explore_prompt(self) -> str:
        c = self.challenge
        ctx_lines = [f"Challenge: {c.name} [{c.category}]"]
        tgt = self._target()
        if tgt:
            ctx_lines.append(f"Target: {tgt}")
        if getattr(self, "_staged_files", None):
            ctx_lines.append(
                "Attached files (already in your working directory — inspect them "
                "FIRST): " + ", ".join(self._staged_files))
        if c.description:
            ctx_lines.append(f"Brief: {c.description.strip()[:600]}")
        cognitive = self._cognitive_context_block()
        if cognitive:
            ctx_lines.append(cognitive)
        board = self._board_context()
        if board:
            ctx_lines.append(board)
        neighborhood = self._intent_neighborhood_context()
        if neighborhood:
            ctx_lines.append(neighborhood)
        ctx_lines.append(self._workspace_protocol_block())
        poc_block = self._poc_prompt_block()
        if poc_block:
            ctx_lines.append(poc_block)
        standing = self._standing_block()
        if standing:
            ctx_lines.append(standing)
        # defect-2: an explore worker must also see N/total flag progress (it didn't
        # before — only _build_prompt had it), so it doesn't stop after the first.
        team = self._team_context_block()
        if team:
            ctx_lines.append(team)
        rejected = self._rejected_flags_block()
        if rejected:
            ctx_lines.append(rejected)
        if getattr(c, "mode", "ctf") == "pentest":
            if self.mode == "verifier":
                report_json = self.intent_goal or "{}"
                return _VERIFIER_PROMPT.format(
                    ctx="\n".join(ctx_lines),
                    goal=self._engagement_goal(),
                    scope=self._engagement_scope(),
                    report_json=report_json)
            return _PENTEST_EXPLORE_PROMPT.format(
                ctx="\n".join(ctx_lines),
                kb=_KB_PROMPT if self.kb else "",
                goal=self._engagement_goal(),
                scope=self._engagement_scope(),
                box=self._box_mode_line(),
                intent_goal=self.intent_goal or "general exploration")
        return _EXPLORE_PROMPT.format(
            ctx="\n".join(ctx_lines),
            kb=_KB_PROMPT if self.kb else "",
            intent_goal=self.intent_goal or "general exploration",
            fmt=self._flag_hint())

    def _build_review_prompt(self) -> str:
        c = self.challenge
        ctx_lines = [f"Challenge: {c.name} [{c.category}]"]
        tgt = self._target()
        if tgt:
            ctx_lines.append(f"Target: {tgt}")
        if c.description:
            ctx_lines.append(f"Brief: {c.description.strip()[:1000]}")
        cognitive = self._cognitive_context_block()
        if cognitive:
            ctx_lines.append(cognitive)
        standing = self._standing_block()
        if standing:
            ctx_lines.append(standing)
        review_board = ""
        if self.shared_graph is not None:
            try:
                review_board = self.shared_graph.to_review_summary()
            except Exception:
                review_board = self._board_markdown()
        return _REVIEW_PROMPT.format(
            ctx="\n".join(ctx_lines),
            kb=_KB_PROMPT if self.kb else "",
            engagement=self._review_engagement_block(),
            intent_goal=self.intent_goal or "Audit the current swarm trajectory.",
            review_board=review_board or "(no shared graph available)")

    def _extract_review_actions(self, text: str) -> list[tuple[str, Any]]:
        actions: list[tuple[str, Any]] = []
        for line in (text or "").splitlines():
            s = line.strip()
            if not s:
                continue
            if s.startswith("NEED_INPUT="):
                need = s.split("=", 1)[1].strip()
                if need:
                    actions.append(("NEED_INPUT", need))
                continue
            for marker in _REVIEW_JSON_MARKERS:
                prefix = f"{marker}="
                if not s.startswith(prefix):
                    continue
                raw = s[len(prefix):].strip()
                try:
                    payload = json.loads(raw)
                    if not isinstance(payload, dict):
                        raise ValueError("payload must be object")
                    actions.append((marker, payload))
                except Exception as exc:  # noqa: BLE001
                    actions.append(("REVIEW_FINDING", {
                        "kind": "invalid_marker",
                        "severity": "warn",
                        "summary": f"{marker} invalid JSON: {exc}",
                        "recommended_actions": [],
                    }))
                break
        return actions

    async def _apply_review_actions(self, actions: list[tuple[str, Any]]) -> int:
        if self.shared_graph is None:
            return 0
        proposed = 0
        for marker, payload in actions:
            try:
                if marker == "NEED_INPUT":
                    need = str(payload).strip()
                    if not need:
                        continue
                    need_kind = classify_need_kind(need)
                    need_card = need[:1000]
                    request_id, correlation = self._decision_request_identity(
                        need_card, need_kind)
                    await self._emit(
                        EventType.HITL_REQUEST,
                        **hitl_request_payload(self.solver_id, need_card,
                                               kind="need_input",
                                               need_kind=need_kind,
                                               request_id=request_id,
                                               **correlation))
                    await self._emit_bb("need_input", need=need[:1000],
                                        need_kind=need_kind,
                                        legacy_kind="need_input")
                    proposed += 1
                    continue
                tier = "tier2" if marker in {
                    "ROUTE_SUPPRESS", "COORDINATOR_DIRECTIVE",
                    "LANE_LOCK", "LANE_UNLOCK",
                } else "tier1"
                payload = dict(payload or {})
                seq = self.shared_graph.add_review_proposal(
                    actor=self.solver_id, marker=marker, payload=payload, tier=tier)
                await self._emit_bb(
                    "review_proposal",
                    seq=seq,
                    marker=marker,
                    tier=tier,
                    route_hash=str(payload.get("route_hash") or ""),
                    summary=str(
                        payload.get("summary") or payload.get("reason")
                        or payload.get("goal") or payload.get("directive")
                        or marker
                    )[:240],
                )
                proposed += 1
            except Exception as exc:  # noqa: BLE001
                try:
                    seq = self.shared_graph.add_review_proposal(
                        actor=self.solver_id, marker="REVIEW_FINDING",
                        payload={"kind": "invalid_action", "severity": "warn",
                                 "summary": f"{marker} rejected: {exc}"},
                        tier="tier1")
                    await self._emit_bb("review_proposal", seq=seq,
                                        marker="REVIEW_FINDING", tier="tier1",
                                        severity="warn",
                                        summary=f"{marker} rejected: {exc}")
                    proposed += 1
                except Exception:
                    pass
        return proposed

    async def run(self) -> SolveOutcome:
        # Subscribe to the InsightBus so HITL pause/resume + a sibling's FLAG reach
        # this live worker (drained by the monitor thread in _run_streaming).
        if self.insight is not None and self._insight_inbox is None:
            try:
                self._insight_inbox = self.insight.subscribe(self.solver_id)
                # Drain the InsightBus history before any subprocess turn is active.
                # Historical guidance should become prompt context for this worker,
                # not a replayed live steer that kills the first pass immediately.
                self._drain_control()
            except Exception:
                self._insight_inbox = None
        outcome: "Optional[SolveOutcome]" = None
        try:
            await self._emit_worker_status(
                online=True, reason="standby" if self.mode == "respond" else "started")
            # I: granular lifecycle — the worker spawned, in its role/phase.
            await self._emit_lifecycle("spawned", phase_label=self.mode)
            if self.mode == "respond":
                outcome = await self._run_respond()
            elif self.mode == "review":
                outcome = await self._run_review()
            elif self._shell_loop:
                # f10 edge cognition: thick worker — multi-step internal
                # observe→plan→execute→verify→reflect→checkpoint loop.
                outcome = await self._run_shell()
            elif self.mode in ("explore", "verifier"):
                outcome = await self._run_explore()
            else:
                outcome = await self._run_bootstrap()
            if not self._worker_stop_reason:
                self._note_worker_stop("solved" if outcome.solved else "finished")
            return outcome
        except asyncio.CancelledError:
            self._note_worker_stop("cancelled")
            exact_control_intent = str(
                getattr(self, "_intent_id", "")
                or self.intent_id_assigned or "").startswith("I-control-")
            if exact_control_intent and (
                    self._control_context_delivery_committed
                    or self._control_context_delivery_unknown):
                self._conclude_intent_db(
                    result=RESULT_CANCELLED,
                    result_detail=(
                        "exact operator continuation terminated after its context "
                        "crossed the process boundary"),
                )
            raise
        except Exception:
            self._note_worker_stop("error")
            exact_control_intent = str(
                getattr(self, "_intent_id", "")
                or self.intent_id_assigned or "").startswith("I-control-")
            if exact_control_intent and (
                    self._control_context_delivery_committed
                    or self._control_context_delivery_unknown):
                self._conclude_intent_db(
                    result=RESULT_CANCELLED,
                    result_detail=(
                        "exact operator continuation failed after its context "
                        "crossed the process boundary"),
                )
            raise
        finally:
            # A runner thread can outlive cancellation of the coroutine awaiting it.
            # Reap provably-dead handles and re-signal anything still live before
            # this CliSolver relinquishes its final process-control boundary.
            self._prune_finished_procs()
            with self._procs_lock:
                has_live_process = bool(self._live_procs)
            if has_live_process:
                self.cancel()
            # M9/M10: clean a mkdtemp scratch dir we own on EVERY exit path (the
            # in-method rmtree only ran on the no-flag fall-through — solved returned
            # early, cancel/exception skipped it, and respond never cleaned at all).
            # Keep it ONLY when this worker solved and returned that dir as the winner
            # artifact (the swarm persists the winner's session from it).
            sc = self._owned_scratch
            if sc is not None:
                solved_winner = bool(
                    outcome is not None and outcome.solved
                    and getattr(outcome, "workdir", None)
                    and Path(outcome.workdir) == Path(sc))
                if not solved_winner:
                    try:
                        shutil.rmtree(sc, ignore_errors=True)
                    except Exception:
                        pass
                self._owned_scratch = None
            if not self._worker_stop_reason:
                self._note_worker_stop("cancelled" if self._cancel_event.is_set() else "finished")
            try:
                await self._emit_worker_status(online=False, reason=self._worker_stop_reason)
            except asyncio.CancelledError:
                pass
            except Exception:
                pass
            if self.insight is not None:
                try:
                    self.insight.unsubscribe(self.solver_id)
                except Exception:
                    pass

    # ── f10 edge-cognition worker shell loop ─────────────────────────────────
    # Reached only when self._shell_loop is on (opt-in envelope). Everything in
    # here is additive: the default bootstrap/explore/review/respond paths are
    # untouched.

    def _shell_context_lines(self) -> "list[str]":
        """Context block for the shell turn-1 prompt. Mirrors the explore
        prompt's context assembly — duplicated deliberately so the default
        explore prompt stays byte-identical."""
        c = self.challenge
        ctx_lines = [f"Challenge: {c.name} [{c.category}]"]
        tgt = self._target()
        if tgt:
            ctx_lines.append(f"Target: {tgt}")
        if getattr(self, "_staged_files", None):
            ctx_lines.append(
                "Attached files (already in your working directory — inspect them "
                "FIRST): " + ", ".join(self._staged_files))
        if c.description:
            ctx_lines.append(f"Brief: {c.description.strip()[:600]}")
        cognitive = self._cognitive_context_block()
        if cognitive:
            ctx_lines.append(cognitive)
        board = self._board_context()
        if board:
            ctx_lines.append(board)
        neighborhood = self._intent_neighborhood_context()
        if neighborhood:
            ctx_lines.append(neighborhood)
        if getattr(self.challenge, "mode", "ctf") == "pentest":
            ctx_lines.append(self._box_mode_line())
        ctx_lines.append(self._workspace_protocol_block())
        poc_block = self._poc_prompt_block()
        if poc_block:
            ctx_lines.append(poc_block)
        standing = self._standing_block()
        if standing:
            ctx_lines.append(standing)
        team = self._team_context_block()
        if team:
            ctx_lines.append(team)
        rejected = self._rejected_flags_block()
        if rejected:
            ctx_lines.append(rejected)
        return ctx_lines

    @staticmethod
    def _shell_effects_block(envelope: dict) -> str:
        effects = envelope.get("predicted_effects") or []
        if not effects:
            return ""
        lines = ["Predicted effects (falsifiable targets — verify or refute):"]
        for e in list(effects)[:6]:
            if isinstance(e, dict):
                lines.append(
                    f"  - {e.get('selector', '?')} {e.get('op', '?')} "
                    f"{e.get('value', '?')}")
            else:
                lines.append(f"  - {e}")
        return "\n".join(lines) + "\n"

    @staticmethod
    def _shell_criteria_block(envelope: dict) -> str:
        criteria = envelope.get("success_criteria") or []
        if not criteria:
            return ""
        lines = ["Success criteria for this intent:"]
        lines += [f"  - {str(c)[:200]}" for c in list(criteria)[:6]]
        return "\n".join(lines) + "\n"

    @staticmethod
    def _shell_parse_plan_steps(text: str) -> "list[dict]":
        """PLAN_STEP=<n>|<action>|<verifier> → plan_queue entries (max 3)."""
        steps: "list[dict]" = []
        for line in (text or "").splitlines():
            m = _SHELL_PLAN_LINE.match(line.strip())
            if not m:
                continue
            parts = [p.strip() for p in m.group(1).strip().split("|")]
            action = parts[1] if len(parts) > 1 else (parts[0] if parts else "")
            verifier = parts[2] if len(parts) > 2 else ""
            try:
                step_no = int(parts[0]) if parts else len(steps) + 1
            except (TypeError, ValueError):
                step_no = len(steps) + 1
            if action:
                steps.append({"step": step_no, "action": action[:300],
                              "verifier": verifier[:300]})
        return steps[:3]

    @staticmethod
    def _shell_parse_sub_intents(text: str) -> "list[str]":
        out: "list[str]" = []
        for line in (text or "").splitlines():
            m = _SHELL_SUB_INTENT_LINE.match(line.strip())
            if m:
                goal = m.group(1).strip()
                if goal and goal not in out:
                    out.append(goal[:600])
        return out[:2]

    @staticmethod
    def _shell_parse_reflect(text: str) -> str:
        for line in (text or "").splitlines():
            m = _SHELL_REFLECT_LINE.match(line.strip())
            if m:
                return m.group(1).strip()[:300]
        return ""

    async def _run_shell(self) -> SolveOutcome:
        """f10 edge-cognition worker shell loop (docs/frameworks_2026/
        10_edge_cognition_swarm.md): the worker is a thick mini-agent running
        observe → plan → execute → verify → reflect → checkpoint internally,
        ≤ turn_limit turns, re-planning every plan_every turns.

        - Belief state persists to <wd>/.shell/state.json after EVERY turn, so
          a crashed worker resumes from its last checkpoint instead of
          re-walking the recon chain (§7).
        - Verified checkpoints and SUB_INTENT= declarations are appended to the
          event bus; the central arbiter ingests them into its ledger (the
          central never ghost-writes worker state).
        - Stuck detection: stuck_limit consecutive turns with no new
          fact/dead-end/artifact → self-kill (§3 Stuck Detection).
        - Budget: the envelope's token/turn ceilings are enforced locally as a
          hard stop; the reported budget_signal lets the central Budget
          Enforcer cancel a runaway shell.
        """
        from muteki.frameworks.f10_edge_cognition import shell as edge_shell
        from muteki.frameworks.f10_edge_cognition import state as edge_state

        env = dict(self._shell_envelope or {})
        env_budget = env.get("budget") or {}
        env_profile = env.get("profile") or {}
        turn_limit = max(1, int(env_budget.get("turn_limit")
                                or edge_state.TURN_LIMIT))
        token_limit = max(1, int(env_budget.get("token_limit")
                                 or edge_state.TOKEN_LIMIT))
        plan_every = max(1, int(env_profile.get("plan_every")
                                or edge_state.PLAN_EVERY))
        stuck_limit = max(1, int(env_profile.get("stuck_limit")
                                 or edge_state.STUCK_LIMIT))

        await self._emit(EventType.RUN_STARTED,
                         challenge=self.challenge.model_dump())
        goal = str(env.get("goal") or self.intent_goal
                   or self._engagement_goal())
        await self._emit(
            EventType.REASONING_DELTA,
            text=f"[{self.driver.name}] edge shell loop — intent: "
                 f"{goal[:120]}, ≤{turn_limit} turns, plan every "
                 f"{plan_every}.\n")

        self._intent_id = str(
            env.get("intent_id")
            or getattr(self, "intent_id_assigned", "")
            or f"intent:{self.solver_id}")
        shell_id = str(env.get("shell_id") or f"shell-{uuid.uuid4().hex[:10]}")
        # the central Budget Enforcer finds this live worker by shell_id.
        self._f10_shell_id = shell_id
        self._last_fact_seq = -1
        await self._emit_bb("intent_claimed", intent_id=self._intent_id,
                            worker=self.solver_id)
        if self.mode == "bootstrap":
            # standalone shell (no central intent row): register the attempt so
            # the board shows this direction was tried (same as _run_bootstrap).
            self._record_intent_db(goal)

        wd = Path(self._workdir) if self._workdir else Path(
            tempfile.mkdtemp(prefix=f"muteki-shell-{self.solver_id}-"))
        if not self._workdir:
            self._owned_scratch = wd   # M9: cleanup on ALL exit paths
        wd.mkdir(parents=True, exist_ok=True)
        self._staged_files = self._stage_attachments(wd)

        # crash-resume: adopt the persisted shell state when one exists; the
        # current envelope's ceilings always win (they may have shrunk).
        st = edge_state.load_state(wd)
        resumed = st is not None
        if st is None:
            st = edge_state.new_state(
                shell_id=shell_id, intent_id=self._intent_id, goal=goal,
                predicted_effects=env.get("predicted_effects"),
                success_criteria=env.get("success_criteria"),
                token_limit=token_limit, turn_limit=turn_limit)
        else:
            st.setdefault("budget", {})
            st["budget"]["token_limit"] = token_limit
            st["budget"]["turn_limit"] = turn_limit
            await self._emit(
                EventType.REASONING_DELTA,
                text=f"[{self.driver.name}] shell resumed from checkpoint: turn "
                     f"{st['budget'].get('turns_used', 0)}/{turn_limit}.\n")

        session = None if self._control_secret_values else self.driver.new_session()
        await self._note_cli_session(session)
        all_text = ""
        accepted: "Optional[str]" = None
        worker_cancelled = False
        worker_steered = False
        worker_oom_killed = False
        worker_timed_out = False
        stuck_killed = False
        budget_killed = False
        local_tokens = 0
        res: CliResult = CliResult(text="")

        async def _absorb(r: CliResult) -> None:
            """Fold one turn's result into worker state: cost + markers +
            transcript + every gate-passing flag (same semantics as the
            bootstrap/explore absorb)."""
            nonlocal all_text, accepted, local_tokens
            self._mark_session_if_live(r)
            await self._emit_empty_stderr_diagnostic(r)
            result_text = self._result_text_with_stderr(r)
            all_text = (all_text + "\n" + result_text).strip()
            local_tokens += int(r.input_tokens or 0) + int(r.output_tokens or 0)
            await self._stream_cost(r)
            prov = self._provenance_corpus()
            await self._stream_markers(result_text, flag_provenance=prov)
            concl = result_text.strip().splitlines()
            if concl:
                await self._emit(EventType.REASONING_DELTA,
                                 text=f"[{self.driver.name}] ⮑ "
                                      f"{concl[-1][:300]}\n")
            for f in [x for x in self._extract_flags(result_text)
                      if self._flag_ok(x, prov)]:
                if await self._accept_flag(f):
                    accepted = accepted or f
            if self._stream_accepted and accepted is None:
                accepted = self._stream_accepted[0]

        def _tokens_now() -> int:
            # cost-ledger total when available; else the local per-turn sum.
            return max(self._tokens_spent(), local_tokens)

        def _checkpoint(turn_no: int, *, facts: list, deadends: list,
                        artifacts: list, reflect: str, next_action: str,
                        stuck: bool) -> None:
            """Persist the local shell state + append the real checkpoint event
            for the central arbiter. Never raises into the solve."""
            st["budget"]["tokens_used"] = _tokens_now()
            st["budget"]["turns_used"] = turn_no
            seq = -1
            if self.shared_graph is not None:
                try:
                    seq = edge_shell.emit_shell_checkpoint(
                        self.shared_graph,
                        shell_id=shell_id,
                        intent_id=self._intent_id,
                        checkpoint={
                            "turn": turn_no,
                            "new_facts": [{"text": str(f)[:300]} for f in facts],
                            "new_deadends": [{"reason": str(d)[:300]}
                                             for d in deadends],
                            "artifacts": [str(a)[:300] for a in artifacts],
                            "reflect": reflect,
                            "next_action_summary": str(next_action)[:300],
                        },
                        budget_signal=dict(st["budget"]),
                        stuck=stuck)
                except Exception:
                    seq = -1
            if seq and seq > 0:
                st["last_checkpoint_seq"] = int(seq)
            try:
                edge_state.save_state(wd, st)
            except Exception:
                pass

        effects_block = self._shell_effects_block(env)
        criteria_block = self._shell_criteria_block(env)
        resume_block = ""
        if resumed:
            done_turns = int(st["budget"].get("turns_used") or 0)
            confirmed = (st.get("working_memory", {}) or {}).get(
                "confirmed_subfacts", []) or []
            resume_block = (
                f"\n## RESUMED FROM CHECKPOINT (turn {done_turns} already spent)\n"
                "You crashed or were interrupted; continue from your prior "
                "belief state — do NOT redo confirmed work.\n"
                "Confirmed so far:\n"
                + ("\n".join(f"  - {str(f)[:200]}" for f in confirmed[-10:])
                   or "  (none)")
                + "\n")
        if getattr(self.challenge, "mode", "ctf") == "pentest":
            turn1_prompt = _PENTEST_SHELL_TURN1_PROMPT.format(
                ctx="\n".join(self._shell_context_lines()),
                kb=_KB_PROMPT if self.kb else "",
                engagement_goal=self._engagement_goal(),
                scope=self._engagement_scope(),
                box=self._box_mode_line(),
                goal=goal[:600],
                goal_id=str(env.get("goal_id") or ""),
                effects_block=effects_block,
                criteria_block=criteria_block,
                turn_limit=turn_limit,
                resume_block=resume_block)
        else:
            turn1_prompt = _SHELL_TURN1_PROMPT.format(
                ctx="\n".join(self._shell_context_lines()),
                kb=_KB_PROMPT if self.kb else "",
                goal=goal[:600],
                goal_id=str(env.get("goal_id") or ""),
                effects_block=effects_block,
                criteria_block=criteria_block,
                turn_limit=turn_limit,
                resume_block=resume_block,
                fmt=self._flag_hint())

        # ── the cognitive loop ────────────────────────────────────────────────
        while int(st["budget"].get("turns_used") or 0) < turn_limit:
            if self._cancel_event.is_set():
                worker_cancelled = True
                break
            if self._steer_event.is_set():
                worker_steered = True
                break
            turn_no = int(st["budget"].get("turns_used") or 0) + 1
            planning_turn = (
                turn_no == 1
                or (turn_no - 1) % plan_every == 0
                or not st.get("plan_queue"))
            # observe: refresh the board snapshot so this turn sees teammates.
            self._write_board_file(wd)
            if turn_no == 1:
                argv, stdin_text = self._execute_invocation(
                    turn1_prompt, session)
            else:
                if planning_turn:
                    confirmed = (st.get("working_memory", {}) or {}).get(
                        "confirmed_subfacts", []) or []
                    dead = (st.get("working_memory", {}) or {}).get(
                        "dead_ends", []) or []
                    tokens_now = _tokens_now()
                    warn = (
                        " WARNING: ≥80% of your token budget is gone — converge."
                        if tokens_now >= int(token_limit
                                             * edge_state.TOKEN_WARN_FRACTION)
                        else "")
                    prompt = _SHELL_PLAN_TURN_PROMPT.format(
                        turn=turn_no, turn_limit=turn_limit,
                        tokens_used=tokens_now, token_limit=token_limit,
                        budget_warn=warn,
                        goal=goal[:600],
                        effects_block=effects_block,
                        subfacts_block=(
                            "\n".join(f"  - {str(f)[:200]}"
                                      for f in confirmed[-10:])
                            or "  (none yet)"),
                        deadends_block=(
                            "\n".join(f"  - {str(d.get('reason', d))[:200]}"
                                      for d in dead[-10:])
                            or "  (none)"))
                else:
                    nxt = (st.get("plan_queue") or [{}])[0]
                    tokens_now = _tokens_now()
                    warn = (
                        " WARNING: ≥80% of your token budget is gone — converge."
                        if tokens_now >= int(token_limit
                                             * edge_state.TOKEN_WARN_FRACTION)
                        else "")
                    prompt = _SHELL_EXEC_TURN_PROMPT.format(
                        turn=turn_no, turn_limit=turn_limit,
                        tokens_used=tokens_now, token_limit=token_limit,
                        budget_warn=warn,
                        step=nxt.get("step", "?"),
                        action=str(nxt.get("action") or "continue the plan")[:300],
                        verifier=str(nxt.get("verifier")
                                     or "new fact or dead-end")[:300])
                argv, stdin_text = self._resume_or_execute_argv(
                    prompt, session,
                    fresh_prompt=turn1_prompt + "\n\n" + prompt)
            res = await self._run_invocation(
                argv, cwd=str(wd), timeout=self.timeout, stdin_text=stdin_text)
            session = (None if self._control_secret_values
                       else (res.session or session))
            await self._note_cli_session(session)
            await _absorb(res)
            worker_cancelled = worker_cancelled or res.cancelled
            worker_steered = worker_steered or res.steered
            worker_oom_killed = (worker_oom_killed
                                 or getattr(res, "oom_killed", False))
            worker_timed_out = res.timed_out

            # verify + reflect: did this turn actually change belief?
            turn_text = self._result_text_with_stderr(res)
            facts, deadends = self._extract_structured_facts(turn_text)
            artifacts = [p for p, *_ in self._extract_poc_saves(turn_text)]
            reflect = self._shell_parse_reflect(turn_text)
            new_steps = (self._shell_parse_plan_steps(turn_text)
                         if planning_turn else [])
            belief_changed = bool(facts or deadends or artifacts or accepted)
            st["stuck_counter"] = (
                0 if belief_changed
                else int(st.get("stuck_counter") or 0) + 1)
            wm = st.setdefault("working_memory", {})
            wm["last_tool_output_digest"] = (
                "sha256:" + hashlib.sha256(
                    turn_text.encode("utf-8", "replace")).hexdigest())
            if facts:
                confirmed = list(wm.get("confirmed_subfacts") or [])
                confirmed += [str(f)[:300] for f in facts]
                wm["confirmed_subfacts"] = confirmed[-20:]
            if deadends:
                dead = list(wm.get("dead_ends") or [])
                dead += [{"reason": str(d)[:300], "turn": turn_no}
                         for d in deadends]
                wm["dead_ends"] = dead[-20:]
            # the step just executed leaves the queue; a fresh plan replaces it.
            if new_steps:
                st["plan_queue"] = list(new_steps)[1:]
            elif st.get("plan_queue"):
                st["plan_queue"] = list(st["plan_queue"])[1:]

            # sub-intent writeback (§4.1b): append-only event; the central
            # arbiter persists + dispatches it.
            for sub_goal in self._shell_parse_sub_intents(turn_text):
                sub_id = f"{self._intent_id}-sub-{uuid.uuid4().hex[:6]}"
                if self.shared_graph is not None:
                    try:
                        edge_shell.emit_sub_intent(
                            self.shared_graph,
                            shell_id=shell_id,
                            intent_id=sub_id,
                            goal=sub_goal,
                            parent_intent_id=self._intent_id)
                    except Exception:
                        pass
                await self._emit_bb(
                    "sub_intent_declared", intent_id=sub_id,
                    parent_intent_id=self._intent_id, goal=sub_goal[:200],
                    worker=self.solver_id)

            stuck_now = int(st.get("stuck_counter") or 0) >= stuck_limit
            _checkpoint(
                turn_no, facts=facts, deadends=deadends, artifacts=artifacts,
                reflect=reflect,
                next_action=((st.get("plan_queue") or [{}])[0].get("action", "")
                             if st.get("plan_queue") else ""),
                stuck=stuck_now)

            if accepted or self._pentest_findings_ready():
                break
            if worker_cancelled or worker_steered or worker_oom_killed:
                break
            if _tokens_now() >= int(token_limit * edge_state.TOKEN_KILL_FRACTION):
                budget_killed = True
                break
            if stuck_now:
                stuck_killed = True
                await self._emit(
                    EventType.REASONING_DELTA,
                    text=f"[{self.driver.name}] stuck: {stuck_limit} barren "
                         f"turns → self-kill.\n")
                break

        # one conclude fallback when the loop ended without a flag for a
        # "natural" reason (turn limit / budget / stuck / timeout): force a
        # summary of what is confirmed. cancel/steer/OOM skip it — die now.
        if (not accepted and not worker_cancelled and not worker_steered
                and not worker_oom_killed
                and not self._pentest_findings_ready()):
            await self._emit(EventType.REASONING_DELTA,
                             text=f"[{self.driver.name}] shell loop ended → one "
                                  f"conclude turn.\n")
            self._write_board_file(wd)
            argv, stdin_text = self._resume_or_execute_argv(
                self._explore_conclude_text(), session,
                fresh_prompt=turn1_prompt + "\n\n" + self._explore_conclude_text())
            res = await self._run_invocation(
                argv, cwd=str(wd), timeout=min(self.timeout, 600),
                stdin_text=stdin_text)
            session = (None if self._control_secret_values
                       else (res.session or session))
            await self._note_cli_session(session)
            await _absorb(res)
            worker_cancelled = worker_cancelled or res.cancelled
            worker_steered = worker_steered or res.steered

        # persist the agent's full transcript as a provenance artifact; end-of-run
        # marker backstop + unverified-flag surfacing (same as bootstrap).
        self.artifacts.put(
            self._redact_control_secrets(all_text), suffix=".txt")
        await self._stream_markers(
            all_text, flag_provenance=self._provenance_corpus())
        await self._finalize_report_submissions()
        await self._finalize_verifier_repro()
        if self._stream_accepted and accepted is None:
            accepted = self._stream_accepted[0]
        await self._surface_unverified_flags(all_text)
        try:
            edge_state.save_state(wd, st)
        except Exception:
            pass

        if accepted:
            found = self._accepted_flags_for_outcome()
            lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
            self._conclude_intent_db(result=RESULT_SOLVED, to_fact_seq=lfs,
                                     result_detail="Verified flag accepted.")
            await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                                worker=self.solver_id, result=RESULT_SOLVED,
                                to_fact_seq=lfs,
                                result_detail="Verified flag accepted.")
            self._note_worker_stop("solved")
            await self._emit_finished(flag=accepted, flags=found, solved=True)
            return SolveOutcome(
                True, accepted, int(st["budget"].get("turns_used") or 1),
                self.graph, f"solved via {self.driver.name} edge shell",
                session=session, engine=self.driver.name, workdir=str(wd),
                flags=found)
        if self._pentest_findings_ready():
            return await self._finish_pentest_finding(
                session=session, wd=wd,
                steps=int(st["budget"].get("turns_used") or 1),
                label="edge shell")

        _, deadends = self._extract_structured_facts(all_text)
        if worker_cancelled:
            self._note_worker_stop("cancelled")
            result_code = RESULT_CANCELLED
            detail = "Worker was cancelled before a verified flag."
        elif worker_steered:
            self._note_worker_stop("steered")
            result_code = RESULT_STEERED
            detail = "Worker was steered before producing a verified flag."
        elif worker_oom_killed:
            self._note_worker_stop("oom")
            result_code = RESULT_OOM
            detail = "Killed by the OOM killer before a verified flag."
        elif stuck_killed:
            self._note_worker_stop("stuck")
            result_code = RESULT_DEAD_END
            detail = (f"Shell self-killed after {stuck_limit} consecutive "
                      "barren turns (stuck detection).")
        elif budget_killed:
            self._note_worker_stop("budget")
            result_code = RESULT_EXPLORED
            detail = (f"Shell stopped at its token budget "
                      f"({token_limit} token ceiling).")
        elif deadends:
            self._note_worker_stop("finished")
            result_code = RESULT_DEAD_END
            detail = f"Worker explicitly ruled out: {deadends[0][:220]}"
        elif worker_timed_out:
            self._note_worker_stop("timeout")
            result_code = RESULT_TIMED_OUT
            detail = "Timed out before a verified flag."
        else:
            self._note_worker_stop("finished")
            result_code = RESULT_EXPLORED
            detail = "Shell loop exhausted its turn budget without a flag."
        self._conclude_intent_db(result=result_code, result_detail=detail)
        await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                            worker=self.solver_id, result=result_code,
                            result_detail=detail)
        partial_flags = list(self.graph.flags)
        await self._emit_finished(flag=None, flags=partial_flags, solved=False)
        return SolveOutcome(False, None,
                            int(st["budget"].get("turns_used") or 1),
                            self.graph,
                            f"{self.driver.name} edge shell: no verified flag",
                            flags=partial_flags)

    async def _run_bootstrap(self) -> SolveOutcome:
        await self._emit(EventType.RUN_STARTED, challenge=self.challenge.model_dump())
        mode = "offline" if not self.web_access else "web"
        kb_note = " +KB" if self.kb else ""
        await self._emit(
            EventType.REASONING_DELTA,
            text=f"[{self.driver.name}] delegating to shelled CLI agent — full shell, "
                 f"black-box, {mode}{kb_note}, up to {self.max_turns} turns.\n")

        # Blackboard collaboration layer: a CLI worker takes the WHOLE challenge as
        # one intent it owns end-to-end. Planning architectures assign and persist an
        # intent before constructing the Worker; use that same id for claim, conclude,
        # and runtime retirement. Direct race Workers have no assigned id and retain
        # the per-worker fallback.
        self._intent_id = (
            getattr(self, "intent_id_assigned", "")
            or f"intent:{self.solver_id}"
        )
        self._last_fact_seq = -1
        # pentest mode → the operator's goal is the engagement objective; CTF mode
        # keeps the original "Solve {name} [{category}]" string byte-for-byte.
        goal = self._engagement_goal()
        await self._emit_bb("intent_proposed", intent_id=self._intent_id,
                            goal=goal, worker_class=self.driver.name)
        await self._emit_bb("intent_claimed", intent_id=self._intent_id,
                            worker=self.solver_id)
        # P1-B: ALSO record this whole-challenge intent in the DB intents table (not
        # only the SSE _emit_bb above). The explore path already does this; bootstrap
        # / re-bootstrap didn't, so _attempted_intents_block (status='done' query)
        # NEVER saw the whole-challenge attempts — and those are the MAJORITY of the
        # workers that "重走老路" (each new bootstrap worker re-running recon). With
        # this, a concluded bootstrap attempt shows up on the board for the next one.
        self._record_intent_db(goal)

        # per-solver scratch workdir (CLI cwd). Service challenges only need the
        # target URL; FILE challenges (crypto/rev/forensics/misc) need their
        # attachments present in the cwd so the agent can inspect them directly.
        wd = Path(self._workdir) if self._workdir else Path(
            tempfile.mkdtemp(prefix=f"muteki-cli-{self.solver_id}-"))
        if not self._workdir:
            self._owned_scratch = wd   # M9: ensure cleanup on ALL exit paths
        wd.mkdir(parents=True, exist_ok=True)
        self._staged_files = self._stage_attachments(wd)

        # ── Single-shot execution (see DESIGN_single_shot_migration.md) ──────────
        # One execute pass for the whole-challenge rush, then at MOST one conclude
        # fallback on timeout (force the agent to summarize what it confirmed). NO
        # multi-turn resume loop: a worker no longer lives across turns accumulating
        # context (run-7352 → 80-turn long-lived was a death-spiral overcompensation;
        # the stall-kill that forced it is gone, so we revert to one-shot).
        # Operator guidance / teammate findings now reach the NEXT worker the swarm
        # spawns (intent-level HITL), not a resume of this live one. The session is
        # used only for the immediate conclude fallback, then discarded.
        session = None if self._control_secret_values else self.driver.new_session()
        await self._note_cli_session(session)  # claude pre-seeds; codex stays None
        worker_timed_out = False
        worker_cancelled = False
        worker_steered = False
        all_text = ""
        accepted: "Optional[str]" = None
        res: CliResult = CliResult(text="")

        async def _absorb(r: CliResult) -> None:
            """Fold one subprocess result into the worker's state: stream cost +
            markers, append transcript, accept every gate-passing flag."""
            nonlocal all_text, accepted
            self._mark_session_if_live(r)
            await self._emit_empty_stderr_diagnostic(r)
            result_text = self._result_text_with_stderr(r)
            all_text = (all_text + "\n" + result_text).strip()
            await self._stream_cost(r)
            # Gate summary claims against raw tool stdout/stderr ONLY.  The assistant's
            # final result is deliberately absent from this corpus; otherwise its own
            # FOUND_FLAG line self-proves the claim.
            prov = self._provenance_corpus()
            await self._stream_markers(result_text, flag_provenance=prov)
            concl = result_text.strip().splitlines()
            if concl:
                await self._emit(EventType.REASONING_DELTA,
                                 text=f"[{self.driver.name}] ⮑ {concl[-1][:300]}\n")
            # accept EVERY new, gate-passing flag (multi-flag). dedup via _accept_flag.
            for f in [x for x in self._extract_flags(result_text)
                      if self._flag_ok(x, prov)]:
                if await self._accept_flag(f):
                    accepted = accepted or f  # keep the FIRST for back-compat
            # a flag may have appeared only in intermediate/streamed text (run-11189);
            # _stream_markers already accepted+deduped it — fold into `accepted`.
            if self._stream_accepted and accepted is None:
                accepted = self._stream_accepted[0]

        # refresh the board file so the prompt's pointer/digest reflects teammates'
        # latest, then run the one execute pass.
        self._write_board_file(wd)
        initial_prompt = self._build_prompt()
        argv, stdin_text = self._execute_invocation(initial_prompt, session)
        res = await self._run_invocation(
            argv, cwd=str(wd), timeout=self.timeout, stdin_text=stdin_text)
        session = None if self._control_secret_values else (res.session or session)
        await self._note_cli_session(session)
        await _absorb(res)
        worker_cancelled = res.cancelled
        worker_steered = res.steered
        worker_timed_out = res.timed_out
        # OOM-killed by the kernel (a sibling run's container starved the Docker VM —
        # no per-container --memory cap). NOT a timeout: the worker died early with an
        # empty transcript. Surfaced as its own stop reason so it isn't misread as a
        # budget expiry, and the conclude fallback is SKIPPED — resuming the same
        # session would just OOM again under the same memory pressure.
        worker_oom_killed = getattr(res, "oom_killed", False)

        # one conclude fallback: if it timed out WITHOUT enough flags, resume the same
        # session once to force a summary of what it confirmed (the conclude step). A
        # cancel (sibling won / stop) or an OOM-kill skips it — die immediately.
        if (not worker_cancelled
                and not worker_steered
                and not worker_oom_killed
                and worker_timed_out
                and len(self._already_found) < self._expected_flags()
                and not self._pentest_findings_ready()
                and not getattr(self, "_skip_bootstrap_conclude", False)):
            await self._emit(EventType.REASONING_DELTA,
                             text=f"[{self.driver.name}] timed out → one conclude turn.\n")
            self._write_board_file(wd)
            argv, stdin_text = self._resume_or_execute_argv(
                self._resume_text(), session,
                fresh_prompt=initial_prompt + "\n\n" + self._resume_text())
            res = await self._run_invocation(
                argv, cwd=str(wd), timeout=min(self.timeout, 600),
                stdin_text=stdin_text)
            session = None if self._control_secret_values else (res.session or session)
            await self._note_cli_session(session)
            await _absorb(res)
            worker_cancelled = worker_cancelled or res.cancelled
            worker_steered = worker_steered or res.steered

        # persist the agent's full transcript as a provenance artifact.
        aid = self.artifacts.put(
            self._redact_control_secrets(all_text), suffix=".txt")

        if worker_steered and not accepted:
            self._note_worker_stop("steered")
            detail = "Worker was steered before producing a verified flag."
            self._conclude_intent_db(result=RESULT_STEERED, result_detail=detail)
            await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                                worker=self.solver_id, result=RESULT_STEERED,
                                result_detail=detail)
            partial_flags = list(self.graph.flags)
            await self._emit_finished(flag=None, flags=partial_flags, solved=False)
            return SolveOutcome(False, None, 1, self.graph,
                                f"{self.driver.name} CLI: steered",
                                flags=partial_flags)

        # the coordinator extracts structured facts/dead-ends from the
        # worker's output and writes them to the board (the worker is a stateless
        # executor). Most markers already streamed live to the board mid-solve via
        # _emit_step → _stream_markers (bug #1); this end-of-run pass is a backstop
        # that catches anything missed and is deduped against what already went out.
        # Gate flags against raw command output only.  The assistant transcript is a
        # claim source for facts/dead-ends, never flag evidence.
        await self._stream_markers(
            all_text, flag_provenance=self._provenance_corpus())
        await self._finalize_report_submissions()
        await self._finalize_verifier_repro()
        # the end-of-run backstop may have accepted a flag from the streamed-only
        # transcript — promote it to `accepted` so the solved branch fires.
        if self._stream_accepted and accepted is None:
            accepted = self._stream_accepted[0]
        # UNVERIFIED surfacing: a flag the worker CLAIMED (FOUND_FLAG=) but that traces
        # to NO real command output and NO result text is NOT auto-promoted to solved —
        # it's flagged for the operator. This is the nested-ssh false-negative guard:
        # if a flag was genuinely read on a pivoted host but its remote stdout never
        # made it into any captured output, the system says "unverified, check this"
        # instead of silently dropping it OR silently trusting prose (run-75379 BUG①).
        await self._surface_unverified_flags(all_text)

        # The transcript remains a provenance artifact, but its closing prose is
        # telemetry, not evidence.  Manufacturing a candidate from the final line
        # made an empty worker look like progress and kept the coordinator alive.

        if accepted:
            found = self._accepted_flags_for_outcome()
            lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
            # P1-B: conclude in DB unconditionally (was gated on lfs is not None,
            # which dropped the conclude when no fact seq was recorded → the intent
            # stayed status='claimed' and never showed as attempted).
            self._conclude_intent_db(result=RESULT_SOLVED, to_fact_seq=lfs,
                                     result_detail="Verified flag accepted.")
            await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                                worker=self.solver_id, result=RESULT_SOLVED,
                                to_fact_seq=lfs,
                                result_detail="Verified flag accepted.")
            self._note_worker_stop("solved")
            # flags were already accepted+broadcast in the loop; emit the terminal
            # lifecycle event ONCE here carrying all of them.
            await self._emit_finished(flag=accepted, flags=found, solved=True)
            return SolveOutcome(
                True, accepted, 1, self.graph, f"solved via {self.driver.name} CLI",
                session=session, engine=self.driver.name, workdir=str(wd),
                flags=found)
        if self._pentest_findings_ready():
            return await self._finish_pentest_finding(
                session=session, wd=wd, steps=1, label="CLI")

        if worker_cancelled:
            self._note_worker_stop("cancelled")
        elif worker_steered:
            self._note_worker_stop("steered")
        elif worker_oom_killed:
            self._note_worker_stop("oom")
        elif worker_timed_out:
            self._note_worker_stop("timeout")
        else:
            self._note_worker_stop("finished")
        _, deadends = self._extract_structured_facts(all_text)
        if deadends:
            result_code = RESULT_DEAD_END
            detail = f"Worker explicitly ruled out: {deadends[0][:220]}"
        elif worker_cancelled:
            result_code = RESULT_CANCELLED
            detail = "Worker was cancelled before a verified flag."
        elif worker_steered:
            result_code = RESULT_STEERED
            detail = "Worker was steered before producing a verified flag."
        elif worker_oom_killed:
            result_code = RESULT_OOM
            detail = "Killed by the OOM killer before a verified flag."
        elif worker_timed_out:
            result_code = RESULT_TIMED_OUT
            detail = "Timed out before a verified flag."
        else:
            result_code = RESULT_EXPLORED
            detail = "Explored but found no verified flag."
        # P1-B: conclude in the DB intents table too (was only _emit_bb). result
        # carries WHAT this whole-challenge attempt amounted to, so the next
        # bootstrap worker's board shows "this direction was already tried → <reason>"
        # instead of re-running the same recon.
        self._conclude_intent_db(result=result_code, result_detail=detail)
        await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                            worker=self.solver_id, result=result_code,
                            result_detail=detail)
        partial_flags = list(self.graph.flags)
        await self._emit_finished(flag=None, flags=partial_flags, solved=False)
        # scratch cleanup is centralized in run()'s finally (M9) via _owned_scratch.
        return SolveOutcome(False, None, 1, self.graph,
                            f"{self.driver.name} CLI: no verified flag",
                            flags=partial_flags)

    async def _run_explore(self) -> SolveOutcome:
        """Explore: claim one intent, explore that direction, report
        structured Fact(s). Short-scoped — prevents context explosion by keeping
        each worker's scope narrow. If the worker times out or produces unparseable
        output, a conclude fallback fires (same session, forced summary)."""
        await self._emit(EventType.RUN_STARTED, challenge=self.challenge.model_dump())
        mode_str = "offline" if not self.web_access else "web"
        kb_note = " +KB" if self.kb else ""
        await self._emit(
            EventType.REASONING_DELTA,
            text=f"[{self.driver.name}] explore mode — intent: {self.intent_goal[:120]}, "
                 f"{mode_str}{kb_note}\n")

        self._intent_id = getattr(self, "intent_id_assigned", "") or f"intent:{self.solver_id}"
        self._last_fact_seq = -1
        await self._emit_bb("intent_claimed", intent_id=self._intent_id,
                            worker=self.solver_id)

        wd = Path(self._workdir) if self._workdir else Path(
            tempfile.mkdtemp(prefix=f"muteki-explore-{self.solver_id}-"))
        if not self._workdir:
            self._owned_scratch = wd   # M9: ensure cleanup on ALL exit paths
        wd.mkdir(parents=True, exist_ok=True)
        self._staged_files = self._stage_attachments(wd)

        # ── Single-shot explore (see DESIGN_single_shot_migration.md) ────────────
        # One execute pass on the assigned intent, then at MOST one conclude fallback
        # (on timeout or no structured markers) to force a summary. NO multi-turn
        # resume loop. Operator guidance reaches the NEXT spawned worker, not a resume
        # of this one. The session is used only for the conclude fallback, then dropped.
        session = None if self._control_secret_values else self.driver.new_session()
        await self._note_cli_session(session)  # claude pre-seeds; codex stays None
        worker_cancelled = False
        worker_steered = False
        all_text = ""
        res: CliResult = CliResult(text="")

        self._write_board_file(wd)
        initial_prompt = self._build_explore_prompt()
        argv, stdin_text = self._execute_invocation(initial_prompt, session)
        res = await self._run_invocation(
            argv, cwd=str(wd), timeout=self.timeout, stdin_text=stdin_text)
        session = None if self._control_secret_values else (res.session or session)
        self._mark_session_if_live(res)
        await self._note_cli_session(session)
        await self._emit_empty_stderr_diagnostic(res)
        result_text = self._result_text_with_stderr(res)
        all_text = (all_text + "\n" + result_text).strip()
        await self._stream_cost(res)
        await self._stream_markers(
            result_text, flag_provenance=self._provenance_corpus())
        worker_cancelled = res.cancelled
        worker_steered = res.steered
        worker_timed_out = res.timed_out

        # one conclude fallback: timed out OR no structured markers at all → resume
        # the same session once to force a summary. A cancel skips it (die now).
        if not worker_cancelled and not worker_steered:
            facts, deadends = self._extract_structured_facts(all_text)
            flag = self._extract_flag(all_text)
            if ((not self._pentest_findings_ready())
                    and (worker_timed_out or (not facts and not deadends and not flag))):
                await self._emit(EventType.REASONING_DELTA,
                                 text=f"[{self.driver.name}] explore → one conclude fallback.\n")
                self._write_board_file(wd)
                argv, stdin_text = self._resume_or_execute_argv(
                    self._explore_conclude_text(), session,
                    fresh_prompt=initial_prompt + "\n\n" + self._explore_conclude_text())
                res = await self._run_invocation(
                    argv, cwd=str(wd), timeout=self.conclude_timeout,
                    stdin_text=stdin_text)
                session = None if self._control_secret_values else (res.session or session)
                self._mark_session_if_live(res)
                await self._note_cli_session(session)
                await self._emit_empty_stderr_diagnostic(res)
                result_text = self._result_text_with_stderr(res)
                all_text = (all_text + "\n" + result_text).strip()
                await self._stream_cost(res)
                await self._stream_markers(
                    result_text, flag_provenance=self._provenance_corpus())
                worker_cancelled = worker_cancelled or res.cancelled
                worker_steered = worker_steered or res.steered

        # final marker extraction across the whole transcript.
        facts, deadends = self._extract_structured_facts(all_text)
        flag = self._extract_flag(all_text)

        # provenance: persist full transcript, bind artifact to each fact
        aid = self.artifacts.put(
            self._redact_control_secrets(all_text), suffix=".txt")
        # A command output may contain a valid-looking Flag before the Worker has
        # submitted it through the Blackboard API.  Keep that text as evidence,
        # but derive the outcome only from `_stream_accepted`, which is populated
        # after a matching flag_submission passes the provenance check.
        provenance = self._provenance_corpus()
        accepted = self._stream_accepted[0] if self._stream_accepted else None

        # write structured facts/dead-ends to the board — deduped against anything
        # already streamed live mid-solve (bug #1). The full combined transcript is
        # re-scanned so the conclude-pass markers are caught too. This also accepts
        # any FOUND_FLAG that only ever appeared in streamed/intermediate text.
        await self._stream_markers(all_text, flag_provenance=provenance)
        await self._finalize_report_submissions()
        await self._finalize_verifier_repro()
        # a flag accepted only via the live stream (not in all_text's terminal-result
        # tail) still makes this explore worker solved (run-11189).
        if self._stream_accepted and accepted is None:
            accepted = self._stream_accepted[0]
        # recompute the full marker set for the result decision below.
        facts, deadends = self._extract_structured_facts(all_text)

        # Unstructured transcript text stays in the artifact only.  It must not be
        # promoted to a synthetic candidate that resets no-progress containment.

        if accepted:
            lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
            # #13: conclude UNCONDITIONALLY (was gated on `lfs is not None`). An explore
            # worker that solved but recorded no fact-seq (lfs is None) used to leave
            # its intent status='claimed'; the lease then expired and _open_intents
            # re-dispatched the already-solved direction to a fresh worker. Route
            # through _conclude_intent_db (owner-fenced; result="solved" intentionally
            # bypasses the owner fence per conclude_intent) so to_fact_seq=None is a
            # valid no-op. Mirrors the dead-end exit below, which already concludes
            # unconditionally.
            self._conclude_intent_db(result=RESULT_SOLVED, to_fact_seq=lfs,
                                     result_detail="Verified flag accepted.")
            await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                                worker=self.solver_id, result=RESULT_SOLVED,
                                to_fact_seq=lfs,
                                result_detail="Verified flag accepted.")
            self._note_worker_stop("solved")
            found = self._accepted_flags_for_outcome()
            await self._emit_finished(flag=accepted, flags=found, solved=True)
            return SolveOutcome(
                True, accepted, 1, self.graph,
                f"solved via {self.driver.name} explore",
                session=session, engine=self.driver.name, workdir=str(wd),
                flags=found)
        if self._pentest_findings_ready():
            return await self._finish_pentest_finding(
                session=session, wd=wd, steps=1, label="explore")

        if worker_cancelled:
            self._note_worker_stop("cancelled")
        elif worker_steered:
            self._note_worker_stop("steered")
        elif worker_timed_out:
            self._note_worker_stop("timeout")
        else:
            self._note_worker_stop("finished")
        if worker_steered:
            result_label = RESULT_STEERED
            result_detail = "Worker was steered before finishing this intent."
        elif worker_timed_out:
            result_label = RESULT_TIMED_OUT
            result_detail = "Timed out before finishing this intent."
        elif deadends:
            result_label = RESULT_DEAD_END
            result_detail = f"Worker explicitly ruled out: {deadends[0][:220]}"
        else:
            result_label = RESULT_EXPLORED
            result_detail = "Explored this intent and produced no explicit dead-end."
        lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
        result_label = self._redact_control_secrets(result_label)
        result_detail = self._redact_control_secrets(result_detail)
        # ALWAYS flip the intent to done on exit — even when this worker recorded NO
        # new fact (lfs is None). Previously the DB conclude was gated on `lfs is not
        # None`, so a "need operator" / no-fact dead-end left the intent status=
        # 'claimed'; its lease then expired and _open_intents re-dispatched the SAME
        # stale direction to a fresh worker, forever (run-11190: 93/173 claimed
        # intents never concluded → 238-worker churn on L2). The owner-fence in
        # conclude_intent makes a late conclude safe (a re-dispatched intent owned by
        # a newer worker is not clobbered); to_fact_seq=None is a valid no-op for the
        # fact pointer. Concluding a no-fact intent retires the direction so it stops
        # resurrecting.
        if self.shared_graph is not None:
            try:
                self.shared_graph.conclude_intent(
                    actor=self.solver_id, intent_id=self._intent_id,
                    result=result_label, to_fact_seq=lfs,
                    result_detail=result_detail)
            except Exception:
                pass
        await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                            worker=self.solver_id, result=result_label,
                            to_fact_seq=lfs, result_detail=result_detail)
        partial_flags = list(self.graph.flags)
        await self._emit_finished(flag=None, flags=partial_flags, solved=False)
        # scratch cleanup is centralized in run()'s finally (M9) via _owned_scratch.
        return SolveOutcome(False, None, 1, self.graph,
                            f"{self.driver.name} explore: {result_label}",
                            flags=partial_flags)

    async def _run_review(self) -> SolveOutcome:
        """Review-Arbiter: audit global graph and emit executable control actions.
        It never accepts flags or marks the run solved."""
        await self._emit(EventType.RUN_STARTED, challenge=self.challenge.model_dump())
        await self._emit(
            EventType.REASONING_DELTA,
            text=f"[{self.driver.name}] review-arbiter mode — auditing swarm trajectory.\n")

        self._intent_id = getattr(self, "intent_id_assigned", "") or f"review:{self.solver_id}"
        self._last_fact_seq = -1
        await self._emit_bb("intent_claimed", intent_id=self._intent_id,
                            worker=self.solver_id, worker_class="review")

        wd = Path(self._workdir) if self._workdir else Path(
            tempfile.mkdtemp(prefix=f"muteki-review-{self.solver_id}-"))
        if not self._workdir:
            self._owned_scratch = wd
        wd.mkdir(parents=True, exist_ok=True)
        self._staged_files = self._stage_attachments(wd)

        session = None if self._control_secret_values else self.driver.new_session()
        await self._note_cli_session(session)
        self._write_board_file(wd)
        prompt = self._build_review_prompt()
        argv, stdin_text = self._execute_invocation(prompt, session)
        res = await self._run_invocation(
            argv, cwd=str(wd), timeout=self.timeout, stdin_text=stdin_text)
        session = None if self._control_secret_values else (res.session or session)
        self._mark_session_if_live(res)
        await self._note_cli_session(session)
        await self._emit_empty_stderr_diagnostic(res)
        text = self._result_text_with_stderr(res)
        await self._stream_cost(res)

        safe_text = self._redact_control_secrets(text)
        aid = self.artifacts.put(safe_text, suffix=".txt")
        actions = self._extract_review_actions(safe_text)
        applied = await self._apply_review_actions(actions)
        if not actions:
            try:
                seq = self.shared_graph.add_review_proposal(
                    actor=self.solver_id, marker="REVIEW_FINDING",
                    payload={"kind": "no_action", "severity": "info",
                             "summary": "Review completed without executable markers."},
                    tier="tier1") if self.shared_graph is not None else 0
                await self._emit_bb("review_proposal", seq=seq,
                                    marker="REVIEW_FINDING", tier="tier1",
                                    severity="info",
                                    summary="Review completed without executable markers.")
                applied += 1
            except Exception:
                pass

        result = f"reviewed: {applied} proposal(s)"
        result = self._redact_control_secrets(result)
        if self.shared_graph is not None:
            try:
                self.shared_graph.conclude_intent(
                    actor=self.solver_id, intent_id=self._intent_id,
                    result=result, to_fact_seq=None)
            except Exception:
                pass
        await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                            worker=self.solver_id, result=result,
                            artifact_id=aid)
        self._note_worker_stop("finished")
        await self._emit_finished(flag=None, flags=list(self.graph.flags), solved=False)
        return SolveOutcome(False, None, 1, self.graph,
                            f"{self.driver.name} review: {result}",
                            session=session, engine=self.driver.name,
                            workdir=str(wd), flags=list(self.graph.flags))

    async def _run_respond(self) -> SolveOutcome:
        """Post-solve standby: serve ONE operator command by resuming the winner's
        CLI session (full memory of the solve). action ∈ {ask, mark_false, writeup}.

        ask/writeup are conversational — their output streams to the deck as the
        worker's reply and (writeup) is persisted; neither produces a flag, so the
        provenance gate is not involved. mark_false re-opens the solve: the worker
        keeps going and any NEW flag it finds STILL passes the real gate."""
        action = (self.hitl_cmd.get("action") or "ask").lower()
        text = (self.hitl_cmd.get("text") or "").strip()
        followup_id = str(self.hitl_cmd.get("followup_id") or "")
        if action in {"ask", "writeup"}:
            await self._emit(
                EventType.FOLLOWUP_STARTED,
                followup_id=followup_id,
                kind=action,
                question=text,
            )
        else:
            await self._emit(EventType.RUN_STARTED, challenge=self.challenge.model_dump())
            await self._emit(
                EventType.REASONING_DELTA,
                text=f"[{self.driver.name}] standby — resuming session for "
                     f"{action}{(': ' + text[:80]) if text else ''}\n")

        # per-worker cwd: reuse the winner's workdir if it still exists (keeps any
        # files it downloaded), else a fresh scratch dir. Computed FIRST so we can
        # write a fresh board file into THIS worker's wd before building the prompt
        # (standby reuses a possibly-stale winner dir → rewrite, don't trust a
        # leftover board file).
        _reuse_winner = bool(self._workdir and Path(self._workdir).exists())
        wd = Path(self._workdir) if _reuse_winner \
            else Path(tempfile.mkdtemp(prefix=f"muteki-respond-{self.solver_id}-"))
        if not _reuse_winner:
            self._owned_scratch = wd   # M10: respond mkdtemp was never cleaned before
        wd.mkdir(parents=True, exist_ok=True)
        self._write_board_file(wd)  # sets _board_file_written for _board_context below

        # build the prompt for this command
        if action == "mark_false":
            note = self._board_context() or ""
            prompt = _RESPOND_MARK_FALSE_PROMPT.format(
                flag=self.hitl_cmd.get("flag") or "(the reported flag)", note=note)
        elif action == "writeup":
            prompt = (
                _RESPOND_PENTEST_WRITEUP_PROMPT
                if str(getattr(self.challenge, "mode", "ctf")) == "pentest"
                else _RESPOND_WRITEUP_PROMPT
            )
        else:  # ask / hint / redirect / anything conversational
            question = text or "(no question text)"
            if action == "redirect":
                endpoint = str(self.hitl_cmd.get("url") or "").strip()
                # A redirect is not delivered merely because it reached
                # _target_override.  This one-shot standby path does not call the
                # normal solve prompt builder, so the resumed CLI must receive the
                # endpoint in this exact turn before the reservation can be
                # committed at Popen.
                if endpoint:
                    question = (
                        "Continue the investigation against this new target "
                        f"endpoint: {endpoint}"
                        + (f"\nOperator note: {text}" if text else "")
                    )
            prompt = _RESPOND_ASK_PROMPT.format(text=question)

        # RESUME the winner's session (full memory) when we have one; otherwise a
        # fresh session, with the prompt already carrying the board context.
        # MIGRATION NOTE (DESIGN_single_shot_migration.md, D-1): this is the ONLY
        # resume path the single-shot migration keeps. standby is a SINGLE cold
        # answer-turn, not a long-lived loop accumulating context across a solve —
        # so resuming the winner's session here doesn't reintroduce the bloat the
        # migration removed; it just gives the answer the winner's full memory.
        stdin_text: "Optional[str]" = None
        if self._control_secret_values:
            # The winner session may contain durable history and Cursor cannot make
            # it ephemeral.  A secret-bearing response is always a fresh stdin-only
            # turn with an explicit board snapshot instead of a resume.
            board = self._board_context()
            if board:
                prompt = prompt + "\n" + board
            argv, stdin_text = self._execute_invocation(prompt, None)
        elif self.resume_session:
            await self._note_cli_session(self.resume_session)
            argv, stdin_text = self._resume_invocation(
                prompt, self.resume_session)
        else:
            if action != "mark_false":
                # no session to resume → seed the conversational prompt with the
                # board so the worker still has the solve context.
                board = self._board_context()
                if board:
                    prompt = prompt + "\n" + board
            session = None if self._control_secret_values else self.driver.new_session()
            await self._note_cli_session(session)
            argv, stdin_text = self._execute_invocation(prompt, session)

        # Conversational follow-ups should return promptly. A resumed model can
        # otherwise start another investigation and leave the deck pending for the
        # normal 40-minute solve timeout. mark_false remains a real re-solve and
        # retains the longer bound.
        respond_timeout = 300 if action in {"ask", "writeup"} else 1200
        res: CliResult = await self._run_invocation(
            argv, cwd=str(wd), timeout=min(self.timeout, respond_timeout),
            stdin_text=stdin_text)
        await self._emit_empty_stderr_diagnostic(res)
        await self._stream_cost(res)
        all_text = self._result_text_with_stderr(res)
        safe_all_text = self._redact_control_secrets(all_text)

        if action in {"ask", "writeup"}:
            observed_rc = res.returncode
            if observed_rc is None:
                runtime_rc = (res.runtime_status or {}).get("rc")
                try:
                    observed_rc = int(runtime_rc) if runtime_rc is not None else None
                except (TypeError, ValueError):
                    observed_rc = None
            if res.timed_out:
                raise RuntimeError(f"standby {action} worker timed out")
            if res.oom_killed:
                raise RuntimeError(f"standby {action} worker was terminated by OOM")
            if res.cancelled or res.steered:
                raise RuntimeError(f"standby {action} worker did not complete")
            if observed_rc not in {None, 0}:
                raise RuntimeError(
                    f"standby {action} worker exited with code {observed_rc}")
            if not safe_all_text.strip():
                raise RuntimeError(f"standby {action} worker returned an empty response")

        # stream the reply to the deck (the worker's answer / writeup body).
        if safe_all_text.strip() and action not in {"ask", "writeup"}:
            await self._emit(
                EventType.TEXT_MESSAGE_DELTA,
                text=safe_all_text.strip(),
                main_thread=action in {"ask", "writeup"},
            )

        # mark_false: the worker kept solving — try to accept a NEW flag through the
        # real gate (same provenance rule). On success, re-conclude + accept.
        if action == "mark_false":
            provenance = self._provenance_corpus()
            await self._stream_markers(all_text, flag_provenance=provenance)
            accepted = self._stream_accepted[0] if self._stream_accepted else None
            # a flag accepted only via the live stream still counts (consistency
            # with bootstrap/explore — run-11189).
            if self._stream_accepted and accepted is None:
                accepted = self._stream_accepted[0]
            # Preserve the response as an artifact, but do not manufacture a
            # candidate from its transcript tail when the re-solve misses.
            self.artifacts.put(
                self._redact_control_secrets(all_text), suffix=".txt")
            if accepted:
                self._note_worker_stop("solved")
                found = self._accepted_flags_for_outcome()
                await self._emit_finished(flag=accepted, flags=found, solved=True)
                return SolveOutcome(
                    True, accepted, 1, self.graph,
                    f"re-solved via {self.driver.name} standby",
                    session=(None if self._control_secret_values
                             else res.session or self.resume_session),
                    engine=self.driver.name, workdir=str(wd), flags=found)
            partial_flags = list(self.graph.flags)
            await self._emit_finished(flag=None, flags=partial_flags, solved=False)
            return SolveOutcome(False, None, 1, self.graph,
                                f"{self.driver.name} standby: still searching",
                                flags=partial_flags)

        # ask / writeup: conversational — record a NON-flag artifact, no gate, no
        # RUN_FINISHED solved-state churn. The writeup body is persisted by the
        # standby driver (it owns the run dir); here we just surface the text.
        self._note_worker_stop("finished")
        return SolveOutcome(
            False, None, 1, self.graph, f"{self.driver.name} standby {action}",
            session=(None if self._control_secret_values
                     else res.session or self.resume_session),
            engine=self.driver.name, workdir=str(wd), reply=safe_all_text.strip(),
            flags=list(self.graph.flags))

    def _estimated_cli_result_from_stream(self) -> CliResult:
        """Floor usage from live stream activity when the vendor omitted usage.

        DeepSeek-via-claude.orig kills often leave no final ``result`` / usage
        block. Tool starts and reasoning chars still prove the model ran.
        """
        tools = int(getattr(self, "_stream_tool_starts", 0) or 0)
        reason_chars = int(getattr(self, "_stream_reasoning_chars", 0) or 0)
        out_tok = 0
        if tools or reason_chars:
            out_tok = max(1, (reason_chars // 4) + (tools * 256))
        in_tok = out_tok * 4 if out_tok else 0
        return CliResult(
            text="",
            cost_usd=None,
            input_tokens=in_tok or None,
            output_tokens=out_tok or None,
        )

    async def _flush_stream_activity_cost(self) -> None:
        """Emit a floor COST_UPDATE from stream counters if nothing was charged yet."""
        if self.cost is None or self._stream_cost_flushed:
            return
        try:
            usage = None
            if hasattr(self.cost, "solver_usage_or_none"):
                usage = self.cost.solver_usage_or_none(self.solver_id)
            if usage and (
                int(usage.get("output_tokens") or 0) > 0
                or int(usage.get("cost_micro_usd") or 0) > 0
            ):
                self._stream_cost_flushed = True
                return
        except Exception:
            pass
        est = self._estimated_cli_result_from_stream()
        if not (est.input_tokens or est.output_tokens):
            return
        await self._stream_cost(est)

    async def _stream_cost(self, res: CliResult) -> None:
        if self.cost is None:
            return
        usd = res.cost_usd
        in_tok = int(res.input_tokens or 0)
        out_tok = int(res.output_tokens or 0)
        # Record when we have EITHER a dollar cost OR token usage. claude reports a
        # real dollar cost; codex's was re-derived from tokens in the driver; cursor
        # is subscription-backed (usd is None → $0) but still reports tokens, so it
        # contributes to the deck's token-usage column at zero cost.
        if usd is None and not (in_tok or out_tok):
            # Last resort: price from live stream activity so a killed turn with
            # real tools cannot seal a zero-cost receipt.
            est = self._estimated_cli_result_from_stream()
            in_tok = int(est.input_tokens or 0)
            out_tok = int(est.output_tokens or 0)
            if not (in_tok or out_tok):
                return
        try:
            reported = None if usd is None else float(usd)
            # DeepSeek-via-claude.orig often emits usage with total_cost_usd=0 (or
            # omits it). When dollars are missing/zero but tokens exist, price from
            # the local table (same path Reason uses) so receipts stay measurable.
            if (reported is None or reported <= 0.0) and (in_tok or out_tok):
                model = (
                    str(getattr(self, "model", "") or "").strip()
                    or str(
                        (getattr(self, "profile", None) or {}).get("model") or ""
                    ).strip()
                    or str(
                        __import__("os").environ.get("MUTEKI_WORKER_MODEL") or ""
                    ).strip()
                    or "deepseek-v4-flash"
                )
                await self.cost.record(
                    model=model,
                    input_tokens=in_tok,
                    output_tokens=out_tok,
                    run_id=self.run_id,
                    solver_id=self.solver_id,
                    challenge_id=self.challenge.id,
                )
                self._stream_cost_flushed = True
                return
            await self.cost.add_external_usd(
                float(reported or 0.0), run_id=self.run_id,
                solver_id=self.solver_id, challenge_id=self.challenge.id,
                input_tokens=in_tok, output_tokens=out_tok)
            self._stream_cost_flushed = True
        except Exception:
            pass

    # P0 defect-1 (DESIGN_swarm_defect_remediation.md): a worker must NOT be able to
    # turn the run "solved / 已解 / task complete" by ASSERTING it in a VERIFIED_FACT.
    # Only the flag gate (_flag_ok) + _flags_complete() decide completion. A fact text
    # that is a bare completion CLAIM (no flag, no concrete evidence) gets downgraded
    # to an unverified candidate so it never enters reason's VERIFIED-evidence view
    # and never broadcasts to siblings as confirmed — killing the "44 '已解' facts
    # poison the board → swarm collectively rationalizes 'solved'" failure (run-42599:
    # 28 solved-like verified facts). Real evidence facts ("admin panel at /x",
    # "creds a:b work") are untouched — they don't match these completion-claim shapes.
    _SOLVED_CLAIM_RE = re.compile(
        r"(?:^|\b)(?:"
        r"challenge\s+(?:is\s+)?solved|already\s+solved|task\s+(?:is\s+)?complete|"
        r"successfully\s+solved|solution\s+complete|single[- ]?flag|"
        r"已解(?:决|出)?|已完成|任务完成|不需要(?:再)?打|本来就不需要|无需(?:再)?(?:打|攻)"
        r")",
        re.IGNORECASE,
    )

    def _is_solved_claim(self, fact: str) -> bool:
        """True if `fact` is a bare completion claim (not concrete evidence). A claim
        accompanying a real flag this worker actually accepted is NOT downgraded —
        the flag is the proof, the surrounding text is fine."""
        if not fact:
            return False
        if self._already_found:
            return False  # this worker holds a real gated flag → its claims are earned
        return bool(self._SOLVED_CLAIM_RE.search(fact))

    async def _record_fact(self, fact: str, *, verified: bool, artifact_id: str,
                           witness: str = "") -> int:
        """Record a fact and return its shared_graph seq (or -1 if unavailable)."""
        fact = self._redact_control_secrets(fact)
        witness = self._redact_control_secrets(witness)
        if bool(getattr(self, "_protocol2_mode", False)):
            # The worker can only submit a candidate.  It cannot promote its own
            # prose into canonical truth, and the compatibility graph row remains
            # explicitly unverified.
            verified = False
            candidate = getattr(self, "_protocol2_candidate_callback", None)
            if callable(candidate):
                candidate("fact", {
                    "artifact_id": str(artifact_id or ""),
                    "fact": str(fact),
                    "worker_id": self.solver_id,
                })
        # defect-1: downgrade a bare "solved/已解" claim to an unverified candidate so
        # it can't masquerade as confirmed evidence on the shared board.
        if verified and self._is_solved_claim(fact):
            verified = False
            await self._emit_bb("claim_solved_rejected", claim=fact[:200],
                                worker=self.solver_id)
        # defect-8: a VERIFIED evidence fact must carry a provenance artifact. With no
        # artifact, the "fact" is an unbacked assertion (the no-evidence hallucination
        # — worker states a conclusion it never actually observed) → downgrade to an
        # unverified candidate so reason's verified-evidence view stays grounded.
        elif verified and not (artifact_id and artifact_id.strip()):
            verified = False
        self.graph.add_evidence(source=self.driver.name, fact=fact, artifact_id=artifact_id)
        await self._emit(
            EventType.SOLVE_GRAPH_DELTA,
            **solve_graph_delta_payload("evidence_added", source=self.driver.name, fact=fact))
        fact_seq = -1
        if self.shared_graph is not None:
            try:
                fact_seq = self.shared_graph.add_evidence(
                    actor=self.solver_id, source=self.driver.name, fact=fact,
                    artifact_id=artifact_id, verified=verified,
                    confidence=1.0 if verified else 0.4,
                    witness=witness or None, verifier=self.driver.name,
                    intent_id=getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or None)
            except Exception:
                pass
            if fact_seq <= 0:
                return fact_seq
        await self._emit(
            EventType.SHARED_GRAPH_DELTA,
            **shared_graph_delta_payload(fact, verified=verified,
                                         confidence=1.0 if verified else 0.4,
                                         actor=self.solver_id, artifact_id=artifact_id,
                                         verifier=self.driver.name, fact_seq=fact_seq))
        await self._emit_bb("fact_added", fact=fact, verified=verified,
                            confidence=1.0 if verified else 0.4,
                            verifier=self.driver.name, artifact_id=artifact_id,
                            fact_seq=fact_seq)
        # The fact is already on the shared graph (add_evidence above), so every
        # teammate's next-turn prompt carries it via the FULL board (_board_markdown
        # → to_board_markdown). We ALSO broadcast verified facts on the InsightBus so
        # a sibling's _drain_control sees them in real time (used for live signals like
        # FLAG / SUBMIT_LOCKED); the FACT/DEAD_END events themselves are not folded
        # into a prompt buffer — the board is the propagation channel. Only verified —
        # candidate facts are too noisy to push to siblings.
        if verified and self.insight is not None:
            try:
                await self.insight.fact(self.solver_id, fact, artifact_id or None)
            except Exception:
                pass
        if fact_seq > 0:
            self._last_fact_seq = fact_seq
            self._summarize_async(fact, node_kind="fact", fact_seq=fact_seq)
        return fact_seq

    def _summarize_async(self, text: str, *, node_kind: str,
                         fact_seq: int = -1, intent_id: str = "") -> None:
        """Fire-and-forget a deepseek-flash zh gist for a fact/intent node.

        Only runs in a web/bus context (the deck renders the gist); a bare CLI
        race with no bus skips it. Never blocks the worker: the summary lands a
        few seconds later via NODE_SUMMARIZED and is stored once on the graph.
        Skips trivially short text — a 30-char fact is already its own gist."""
        if self.bus is None or len((text or "").strip()) < 48:
            return
        from muteki.solver.summarizer import summarize_node
        try:
            asyncio.create_task(summarize_node(
                text, node_kind=node_kind, fact_seq=fact_seq, intent_id=intent_id,
                shared_graph=self.shared_graph, bus=self.bus,
                run_id=self.run_id, challenge_id=self.challenge.id))
        except RuntimeError:
            # no running loop (shouldn't happen on the async path) — skip silently
            pass

    def _rejected_flags(self) -> "set[str]":
        """Flag values the operator marked as FALSE POSITIVES on the shared graph.

        Reads the SAME durable, respawn-surviving log the coordinator's flag
        reconciliation uses (shared_graph.invalidated_flags → EV_FLAG_INVALIDATED) —
        ONE source of truth, not a parallel one. NOT this worker's `_already_found`
        (per-instance; a fresh worker after a false-positive reopen inherits the
        SURVIVING flags but never the rejected ones). Best-effort: an unreachable
        graph yields an empty set rather than blocking acceptance."""
        sg = getattr(self, "shared_graph", None)
        if sg is None:
            return set()
        try:
            return set(sg.invalidated_flags() or set())
        except Exception:
            return set()

    def _accepted_flags_for_outcome(self) -> "list[str]":
        """Return accepted values to the owning runtime without public projection."""
        if bool(getattr(self, "_protocol2_mode", False)):
            return list(self._protocol2_accepted_flags)
        return list(self.graph.flags)

    async def _accept_flag(self, flag: str) -> bool:
        """Record + broadcast ONE distinct flag. Dedup against this worker's
        already-accepted set (a flag found twice, or one a sibling already
        broadcast, is a no-op). Returns True if it was new. Does NOT emit the
        terminal lifecycle event — that fires once in run() with ALL flags, so a
        multi-flag worker can accept several and finish once."""
        if not flag or flag in self._already_found:
            return False
        if (not bool(getattr(self, "_protocol2_mode", False))
                and flag not in self._validated_flag_submissions):
            key = ("FLAG_API_REQUIRED", flag[:200])
            if key not in self._published_markers:
                self._published_markers.add(key)
                await self._emit_bb(
                    "flag_submission_required",
                    reason=("Flag text was observed but no validated "
                            "muteki-blackboard submit-flag request exists"),
                )
            return False
        # run-75379 LOAD-BEARING reject gate: a flag an operator invalidated must
        # NEVER be re-accepted, even after its producing intent is reopened and a
        # fresh worker re-derives it. This check sits at the very TOP — BEFORE the
        # live broadcast and BEFORE any state mutation — because re-occupation
        # happens via this in-memory + broadcast path, NOT via the DB flag row
        # (shared_graph.flag_found already dedups the DB on the permanent
        # `flag::<value>` key, so a second DB row is impossible; the live broadcast is
        # the actual hole). The reject set is durable (read from the shared graph), so
        # it survives worker respawn. Same permanence as a placeholder flag.
        if flag in self._rejected_flags():
            await self._emit_bb(
                "flag_reaccept_blocked", flag=flag,
                reason="operator marked this flag false-positive; permanently rejected")
            return False
        self._already_found.add(flag)
        if bool(getattr(self, "_protocol2_mode", False)):
            # GateAuthority has already committed FLAG_ACCEPTED + its immutable
            # flag.accepted outbox before this method runs.  Keep only the private
            # worker bookkeeping that lets Protocol2RunSession resolve that canonical
            # handoff from the returned outcome.  Every public legacy projection below
            # remains Protocol 1-only; the reconciliation layer owns Protocol 2's typed
            # flag.accepted publication.
            self._protocol2_accepted_flags.append(flag)
            return True
        self.graph.add_flag(flag)
        # P0 defect-0 (DESIGN_swarm_defect_remediation.md): ALSO record the flag on
        # the SHARED graph, not just this worker's local SolveGraph. shared_graph.
        # flag_found writes an EV_FLAG_FOUND event that snapshot() materializes into
        # .flags — without this call the shared graph DB never recorded any flag, so
        # reason / board / progress could not read real flag progress from the graph
        # (run-11190 RUN_FINISHED had empty flags). flag_found dedups on flag::{flag}.
        if self.shared_graph is not None:
            try:
                self.shared_graph.flag_found(
                    actor=self.solver_id, flag=flag,
                    intent_id=getattr(self, "_intent_id", "") or None)
            except Exception:
                pass
        await self._emit(EventType.SOLVE_GRAPH_DELTA,
                         **solve_graph_delta_payload("flag", flag=flag))
        await self._emit(EventType.INSIGHT_BUS_EVENT,
                         **insight_payload("FlagFound", flag=flag, by=self.solver_id))
        await self._emit_bb("flag_found", flag=flag)
        if self.insight is not None:
            try:
                await self.insight.flag_found(self.solver_id, flag)
            except Exception:
                pass
        return True

    async def _surface_unverified_flags(
        self, all_text: str, *, reason: "Optional[str]" = None,
    ) -> None:
        """run-75379 BUG①: a flag the worker CLAIMED via FOUND_FLAG= but that did NOT
        pass the provenance gate (it traces to NO real command output — only to the
        worker's prose) is neither silently dropped nor trusted as a solve. It is
        surfaced to the operator as "unverified — check this", so a flag genuinely
        read on a pivoted host whose remote stdout never reached captured output isn't
        lost, while a hallucinated flag laundered through reasoning still does NOT
        auto-solve the run.

        Emit-only and conservative: this NEVER mutates flag state or accepts anything
        (the gate already refused these). A claimed flag that WAS accepted, is a known
        placeholder, or was operator-rejected is skipped. Deduped via
        _published_markers so the live + end-of-run passes don't double-report."""
        if self.mode == "review":
            return
        rejected = self._rejected_flags()
        for flag in self._extract_flags(all_text):
            if flag in self._already_found:
                continue  # accepted — already a real flag, nothing to surface
            if self._flag_from_operator_context(flag):
                continue  # known operator context, not a newly discovered candidate
            if flag in rejected:
                continue  # operator already ruled this value out; don't re-nag
            key = ("U", flag[:200])
            if key in self._published_markers:
                continue
            self._published_markers.add(key)
            await self._emit_bb(
                "flag_unverified", flag=flag,
                reason=reason or (
                    "claimed via FOUND_FLAG= but traces to no real command "
                    "output — operator verification needed"))
