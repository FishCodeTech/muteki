"""CliSolver — a swarm worker whose EXECUTOR is a shelled CLI agent (claude/codex)
instead of the local code-driven kernel loop.

It is a drop-in for `Solver` in the swarm: same construction surface, same
`run() -> SolveOutcome`, same `solver_id` / `graph`, and it emits the same event
stream (RUN_STARTED → reasoning/insight → fact_added/flag_found → RUN_FINISHED) so
the deck, shared_graph, and blackboard telemetry keep working unchanged.

What's different: instead of driving an LLM through run_python tool-calls, it hands
the challenge to a CLI agent (full shell, its own agentic loop). A Flag counts only
when that agent explicitly calls the Muteki Blackboard ``submit-flag`` command.

Bare host (no isolation — user deferred P-D0). The CLI runs in a per-solver scratch
workdir; for a service challenge the agent only needs the target URL.
"""

from __future__ import annotations

import asyncio
import re
import shutil
import signal
import tempfile
import threading
import time
import uuid
from pathlib import Path
from typing import Any, Optional

from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.models.solve_graph import Challenge, SolveGraph
from muteki.solver.cli_driver import (
    CliDriver, KB_MCP_NAME, driver_for,
)
from muteki.solver.cli_launch_check import launch_failure_code
from muteki.solver.result import ArtifactStore
from muteki.solver.result_codes import RESULT_CANCELLED, RESULT_SOLVED
from muteki.solver.types import SolverConfig, SolveOutcome
from muteki.solver.worker_profiles import worker_identity_from_env



from muteki.solver.cli_prompts import *  # noqa: F401,F403
from muteki.solver.cli_protocol import *  # noqa: F401,F403
from muteki.solver.cli_workspace import (  # noqa: F401
    _stable_worker_path, _file_sha256, _repo_blackboard_script,
    sync_deployed_blackboard_skills,
)
from muteki.solver import cli_board_context as _cli_board
from muteki.solver import cli_control as _cli_control
from muteki.solver import cli_events as _cli_events
from muteki.solver import cli_poc_review as _cli_poc_review
from muteki.solver import cli_modes as _cli_modes
from muteki.solver import cli_process as _cli_process
from muteki.solver import cli_protocol as _cli_protocol
from muteki.solver import cli_results as _cli_results
from muteki.solver import cli_runtime as _cli_runtime
from muteki.solver import cli_supervision as _cli_supervision
from muteki.solver import cli_workspace as _cli_workspace

_WORKER_HEARTBEAT_SECONDS = 15.0



class CliSolver:
    """Swarm worker backed by a shelled CLI agent. Mirrors Solver's interface."""

    _RAW_OUTPUT_CHAR_CAP = 4_000_000
    _SPILL_OUTPUT_BYTE_CAP = 4_000_000
    BOARD_FILENAME = ".muteki_board.md"  # dotfile: won't collide with staged attachments
    _STANDING_CHAR_BUDGET = 4000
    _SOLVED_CLAIM_RE = re.compile(
        r"(?:^|\b)(?:"
        r"challenge\s+(?:is\s+)?solved|already\s+solved|task\s+(?:is\s+)?complete|"
        r"successfully\s+solved|solution\s+complete|single[- ]?flag|"
        r"已解(?:决|出)?|已完成|任务完成|不需要(?:再)?打|本来就不需要|无需(?:再)?(?:打|攻)"
        r")",
        re.IGNORECASE,
    )

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
        conclude_timeout: int = 120,
        checkpoint_interval: int = 300,
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
        target_epoch: Optional[str | int] = None,
        session_supervisor: Optional[Any] = None,
        worker_profile: Optional[dict] = None,
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
        self._task_file_name = ""
        self._task_file_sha256 = ""
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
        self.from_facts: list[int] = []
        self.expected_observable = ""
        self.requires_capabilities: list[str] = []
        self.value_claim: dict = {}
        self.stop_condition = ""
        self.coverage_key = ""
        self.route_hash = ""
        self.branch_id = ""
        self.lane = ""
        self.risk_class = ""
        self.resource_key = ""
        self.conclude_timeout = conclude_timeout
        self.checkpoint_interval = max(1, int(checkpoint_interval))
        self._session_handoff_active = False
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
        # Target epoch is the identity boundary for observations about a concrete
        # challenge instance.  A Fact may only be promoted from a tool result
        # captured in this same epoch; redirect/instance replacement advances it.
        supplied_target_epoch = (
            target_epoch
            if target_epoch not in (None, "")
            else getattr(challenge, "target_epoch", None)
        )
        self._target_epoch = str(
            supplied_target_epoch if supplied_target_epoch not in (None, "") else "1"
        )
        self._worker_started_at = time.time()
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
        # guidance folded in (the "steering" channel). The monitor sets it for a
        # non-standing correction; the runner ends this bounded call and reports
        # res.steered so the exact session can enter its checkpoint prompt.
        self._steer_event = threading.Event()
        # commit-step is a normal Worker terminal boundary.  It needs its own
        # signal so the CLI process can stop immediately without being reported
        # as cancelled or steered.
        self._finish_event = threading.Event()
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
        # the loop thread reads them to build the same-session checkpoint prompt.
        self._guidance_lock = threading.Lock()
        # standing guidance (VPS/SSH creds, global constraints): persistent text
        # injected into EVERY turn's prompt, not consumed like a one-shot steer.
        # Seeded from the coordinator's canonical list at spawn (so a worker created
        # AFTER the operator gave a VPS hint still carries it in turn-1), then grows
        # via the live InsightBus inbox while running.
        self._standing_guidance: "list[str]" = (
            [str(s) for s in standing_guidance if s] if standing_guidance else [])
        # Exact plaintext values materialised from secret:// ContextResources are
        # installed by the Swarm/standby builder.  Their prompt is delivered over a
        # non-persistent stdin-only engine invocation (never argv); every durable
        # event/graph/artifact boundary additionally uses this list for deterministic
        # exact runtime delivery.
        self._control_secret_values: "list[str]" = []
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
        # Monotonic execution fence used by coordinator retirement.  A claimed
        # intent is replay-safe only when the worker runtime is proven exited and
        # this flag is still false (no Popen/remote child ever existed).
        self._runtime_process_started = False
        self._runtime_started_at: "Optional[float]" = None
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
        # Flags this model submitted through the Blackboard Skill during this Worker.
        self._stream_accepted: "list[str]" = []
        # Full, untruncated tool output remains available for facts, findings, and
        # audit artifacts. It is never scanned for Flags.
        self._raw_tool_outputs: "list[str]" = []
        self._raw_tool_outputs_chars = 0
        # Command text remains paired with raw output for finding evidence.
        self._raw_tool_commands: "list[str]" = []
        # Whether each raw result was paired to an observed tool invocation. Unmatched
        # results remain available for audit but cannot become authoritative evidence.
        self._raw_tool_attributed: "list[bool]" = []
        # Authoritative provenance records.  The parallel legacy arrays above are
        # retained for transcript/interrupt consumers; Fact admission reads only
        # these records because they carry exact event and target identity.
        self._tool_evidence_records: "list[Any]" = []
        # Structured worker result. CTF publishes its one final Fact at
        # commit-step; other result types are accumulated until conclusion.
        self._pending_observations: "list[Any]" = []
        self._pending_observation_index: "dict[str, int]" = {}
        # CTF submit-fact is one overwriteable draft.  It becomes visible only
        # when commit-step atomically publishes the Step's final Fact.
        self._draft_fact: "Optional[dict[str, str]]" = None
        self._step_committed = False
        self._pending_dead_ends: "list[Any]" = []
        self._pending_pocs: "list[Any]" = []
        self._pending_need_input: "Optional[Any]" = None
        self._worker_checkpoint_index = 0
        # Set by _commit_worker_result on its first attempt so teardown paths
        # and the coordinator's crash salvage never commit this worker twice.
        self._worker_result_committed = False
        self._last_worker_result: "Optional[Any]" = None
        self._last_worker_result_commit: dict[str, Any] = {}
        self._worker_result_commit_error = ""
        self._transcript_artifact_id = ""
        self._blackboard_ingress_dir: "Optional[Path]" = None
        self._blackboard_drain_task: "Optional[asyncio.Task]" = None
        self._blackboard_drain_lock = asyncio.Lock()
        self._accepted_blackboard_requests = 0
        # Ordered unresolved calls. A result with an id consumes only its exact match;
        # an id-less result consumes the oldest call. The FIFO fallback matters because
        # some engine versions expose execution identity on only the tool side.
        self._pending_tool_calls: "list[dict[str, str]]" = []
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
        # ── EXEC-01: Worker Session Supervisor（可选叠加，CLI 路径不变） ──────
        # 提供时由 supervisor 负责 Profile→Adapter 解析、AgentSession identity
        # （run_id, execution_generation, solver_id）、统一 AgentEvent 与
        # generation fencing、退出分类；进程 spawn/流式解析/Flag 提取仍由本类
        # 与 CliDriver/gate 承担。监督层任何失败都不阻断求解（best-effort）。
        self._session_supervisor = session_supervisor
        self._worker_profile: Optional[dict] = (
            dict(worker_profile) if isinstance(worker_profile, dict) else None)
        self._adapter_resolution: Optional[Any] = None
        self._agent_session_ref: Optional[Any] = None




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
            # EXEC-01：开工前把 Worker 注册进会话监督层（AgentSession identity
            # + Adapter 解析）。解析、构造和健康检查失败都会终止 Worker。
            self._open_supervised_session()
            if self._blackboard_drain_task is None:
                self._blackboard_drain_task = asyncio.create_task(
                    self._blackboard_drain_loop())
            await self._emit_worker_status(
                online=True, reason="standby" if self.mode == "respond" else "started")
            # I: granular lifecycle — the worker spawned, in its role/phase.
            await self._emit_lifecycle(
                "spawned", phase_label=self.mode,
                **self._runtime_adapter_event_fields())
            if self.mode == "respond":
                outcome = await self._run_respond()
            elif self.mode == "review":
                outcome = await self._run_review()
            elif self.mode in ("explore", "fact_verifier"):
                outcome = await self._run_explore()
            else:
                outcome = await self._run_bootstrap()
            if not self._worker_stop_reason:
                self._note_worker_stop("solved" if outcome.solved else "finished")
            return outcome
        except asyncio.CancelledError:
            completed_by_this_worker = bool(
                self._stream_accepted and self._flags_complete_for_worker()
            )
            self._note_worker_stop(
                "solved" if completed_by_this_worker else "cancelled")
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
            elif not exact_control_intent and self.mode != "respond":
                # Goal completion cancels every task immediately.  A Worker that
                # already submitted the completing Flag has a complete handoff;
                # preserve its solved result instead of rewriting it as missing.
                try:
                    if completed_by_this_worker:
                        await self._commit_worker_result(
                            RESULT_SOLVED,
                            result_detail=(
                                "Configured Flag count reached before "
                                "Coordinator cancellation."
                            ),
                        )
                    else:
                        await self._commit_worker_result(
                            RESULT_CANCELLED,
                            result_detail=(
                                "Worker was cancelled before handing off its result."
                            ),
                            handoff_missing=True,
                        )
                except asyncio.CancelledError:
                    pass
                except Exception:
                    pass
            raise
        except Exception as e:
            self._note_worker_stop("error")
            launch_code = launch_failure_code(e)
            if launch_code and not isinstance(
                    e, _cli_results.WorkerRuntimeUnavailable):
                e = _cli_results.WorkerRuntimeUnavailable(
                    str(e), code=launch_code)
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
            elif not exact_control_intent and self.mode != "respond":
                # Crash/cancel salvage: commit whatever this worker accumulated,
                # marked handoff_missing. Once, and never raise from cleanup.
                # Control intents are coordinator-owned: no commit here.
                try:
                    await self._commit_worker_result(
                        "error", result_detail=str(e),
                        handoff_missing=True)
                except asyncio.CancelledError:
                    pass
                except Exception:
                    pass
            raise e
        finally:
            drain_task = self._blackboard_drain_task
            self._blackboard_drain_task = None
            if drain_task is not None:
                drain_task.cancel()
                try:
                    await drain_task
                except asyncio.CancelledError:
                    pass
                except Exception:
                    pass
            try:
                await self._drain_blackboard_requests_once()
            except Exception:
                pass
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
            # EXEC-01：CLI Runtime 退出后同步收尾，不产生伪造完成事件。
            self._close_supervised_session()
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

CliSolver._run_bootstrap = _cli_modes._run_bootstrap
CliSolver._run_explore = _cli_modes._run_explore
CliSolver._run_review = _cli_modes._run_review
CliSolver._run_respond = _cli_modes._run_respond
CliSolver._emit = _cli_events._emit
CliSolver._decision_request_identity = _cli_events._decision_request_identity
CliSolver._emit_bb = _cli_events._emit_bb
CliSolver._record_intent_db = _cli_events._record_intent_db
CliSolver._conclude_intent_db = _cli_events._conclude_intent_db
CliSolver._emit_finished = _cli_events._emit_finished
CliSolver._emit_worker_status = _cli_events._emit_worker_status
CliSolver._tokens_spent = _cli_events._tokens_spent
CliSolver._emit_lifecycle = _cli_events._emit_lifecycle

CliSolver._fact_witnessed_in_chunk = _cli_protocol._fact_witnessed_in_chunk
CliSolver._blackboard_script_path = _cli_workspace._blackboard_script_path
CliSolver._ensure_shared_attachment = _cli_workspace._ensure_shared_attachment
CliSolver._link_existing_shared_artifacts = _cli_workspace._link_existing_shared_artifacts
CliSolver._link_inherited_pocs = _cli_workspace._link_inherited_pocs
CliSolver._link_shared_attachment = _cli_workspace._link_shared_attachment
CliSolver._spill_host_path = _cli_workspace._spill_host_path
CliSolver._stage_attachments = _cli_workspace._stage_attachments
CliSolver._workdir_path = _cli_workspace._workdir_path
CliSolver._board_context = _cli_board._board_context
CliSolver._board_markdown = _cli_board._board_markdown
CliSolver._board_pointer = _cli_board._board_pointer
CliSolver._box_mode_line = _cli_board._box_mode_line
CliSolver._build_explore_prompt = _cli_board._build_explore_prompt
CliSolver._build_prompt = _cli_board._build_prompt
CliSolver._build_review_prompt = _cli_board._build_review_prompt
CliSolver._credential_digest = _cli_board._credential_digest
CliSolver._dead_ends_scoped_block = _cli_board._dead_ends_scoped_block
CliSolver._engagement_goal = _cli_board._engagement_goal
CliSolver._engagement_scope = _cli_board._engagement_scope
CliSolver._expected_flags = _cli_board._expected_flags
CliSolver._flag_hint = _cli_board._flag_hint
CliSolver._flags_complete_for_worker = _cli_board._flags_complete_for_worker
CliSolver._intent_neighborhood_context = _cli_board._intent_neighborhood_context
CliSolver._known_flags = _cli_board._known_flags
CliSolver._live_blackboard_context = _cli_board._live_blackboard_context
CliSolver._persist_context_manifest = _cli_board._persist_context_manifest
CliSolver._poc_prompt_block = _cli_board._poc_prompt_block
CliSolver._rejected_flags_block = _cli_board._rejected_flags_block
CliSolver._review_engagement_block = _cli_board._review_engagement_block
CliSolver._ruled_out_digest = _cli_board._ruled_out_digest
CliSolver._source_artifacts_block = _cli_board._source_artifacts_block
CliSolver._source_facts_block = _cli_board._source_facts_block
CliSolver._standing_block = _cli_board._standing_block
CliSolver._step_contract_block = _cli_board._step_contract_block
CliSolver._submit_blocked_now = _cli_board._submit_blocked_now
CliSolver._submit_gate_block = _cli_board._submit_gate_block
CliSolver._target = _cli_board._target
CliSolver._team_context_block = _cli_board._team_context_block
CliSolver._verifier_locked_now = _cli_board._verifier_locked_now
CliSolver._verifier_rate_limited = _cli_board._verifier_rate_limited
CliSolver._workspace_protocol_block = _cli_board._workspace_protocol_block
CliSolver._write_board_file = _cli_board._write_board_file
CliSolver._remove_unscoped_board_file = _cli_board._remove_unscoped_board_file
CliSolver._apply_runtime_argv = _cli_runtime._apply_runtime_argv
CliSolver._close_supervised_session = _cli_supervision._close_supervised_session
CliSolver._commit_control_context_delivery = _cli_control._commit_control_context_delivery
CliSolver._context_delivery_reservation_snapshot = _cli_control._context_delivery_reservation_snapshot
CliSolver._drain_control = _cli_control._drain_control
CliSolver._drain_control_async = _cli_runtime._drain_control_async
CliSolver._emit_step = _cli_runtime._emit_step
CliSolver._enable_in_turn_steer = _cli_control._enable_in_turn_steer
CliSolver._execute_invocation = _cli_runtime._execute_invocation
CliSolver._finalize_control_context_prompt_manifest = _cli_control._finalize_control_context_prompt_manifest
CliSolver._mark_control_context_delivery_unknown = _cli_control._mark_control_context_delivery_unknown
CliSolver._mark_session_if_live = _cli_runtime._mark_session_if_live
CliSolver._materialize_tool_result_spill = _cli_runtime._materialize_tool_result_spill
CliSolver._maybe_steer_idle_repeat = _cli_control._maybe_steer_idle_repeat
CliSolver._note_cli_session = _cli_supervision._note_cli_session
CliSolver._note_worker_stop = _cli_supervision._note_worker_stop
CliSolver._notify_context_delivery = _cli_control._notify_context_delivery
CliSolver._on_proc = _cli_runtime._on_proc
CliSolver._on_proc_start_uncertain = _cli_runtime._on_proc_start_uncertain
CliSolver._open_supervised_session = _cli_supervision._open_supervised_session
CliSolver._proc_is_alive = _cli_process._proc_is_alive
CliSolver._prune_finished_procs = _cli_process._prune_finished_procs
CliSolver._resume_invocation = _cli_runtime._resume_invocation
CliSolver._resume_or_execute_argv = _cli_runtime._resume_or_execute_argv
CliSolver._run_invocation = _cli_runtime._run_invocation
CliSolver._run_streaming = _cli_runtime._run_streaming
CliSolver._runtime_adapter_event_fields = _cli_supervision._runtime_adapter_event_fields
CliSolver._set_paused = _cli_control._set_paused
CliSolver._signal_proc = _cli_process._signal_proc
CliSolver._stop_on_sibling_flag = _cli_control._stop_on_sibling_flag
CliSolver._thread_cancel_cleanup_timeout = _cli_process._thread_cancel_cleanup_timeout
CliSolver._to_thread_with_cancel_cleanup = _cli_process._to_thread_with_cancel_cleanup
CliSolver._ensure_role_contract = _cli_process._ensure_role_contract
CliSolver._worker_env = _cli_process._worker_env
CliSolver.runtime_exit_confirmed = _cli_process.runtime_exit_confirmed
CliSolver.wait_runtime_exit = _cli_process.wait_runtime_exit
CliSolver._accept_flag = _cli_results._accept_flag
CliSolver._accept_submitted_flag = _cli_results._accept_submitted_flag
CliSolver._accepted_flags_for_outcome = _cli_results._accepted_flags_for_outcome
CliSolver._apply_review_actions = _cli_poc_review._apply_review_actions
CliSolver._bind_tool_evidence_event = _cli_results._bind_tool_evidence_event
CliSolver._build_worker_result = _cli_results._build_worker_result
CliSolver._commit_worker_result = _cli_results._commit_worker_result
CliSolver._controlled_result_stop = _cli_results._controlled_result_stop
CliSolver._current_run_tool_allows_verified = _cli_results._current_run_tool_allows_verified
CliSolver._blackboard_drain_loop = _cli_results._blackboard_drain_loop
CliSolver._blackboard_operation_allowed = _cli_results._blackboard_operation_allowed
CliSolver._drain_blackboard_requests_once = _cli_results._drain_blackboard_requests_once
CliSolver._handle_blackboard_request = _cli_results._handle_blackboard_request
CliSolver._write_blackboard_result = _cli_results._write_blackboard_result
CliSolver._emit_empty_stderr_diagnostic = _cli_results._emit_empty_stderr_diagnostic
CliSolver._estimated_cli_result_from_stream = _cli_results._estimated_cli_result_from_stream
CliSolver._finding_evidence_corpus = _cli_results._finding_evidence_corpus
CliSolver._flush_stream_activity_cost = _cli_results._flush_stream_activity_cost
CliSolver._handle_poc_save = _cli_poc_review._handle_poc_save
CliSolver._is_solved_claim = _cli_results._is_solved_claim
CliSolver._mark_claimed_pocs_spent = _cli_poc_review._mark_claimed_pocs_spent
CliSolver._maybe_broadcast_lockout = _cli_results._maybe_broadcast_lockout
CliSolver._persist_raw_tool_output = _cli_results._persist_raw_tool_output
CliSolver._provenance_corpus = _cli_results._provenance_corpus
CliSolver._record_artifact_matches = _cli_results._record_artifact_matches
CliSolver._record_fact = _cli_results._record_fact
CliSolver._rejected_flags = _cli_results._rejected_flags
CliSolver._result_text_with_stderr = _cli_results._result_text_with_stderr
CliSolver._stderr_tail = _cli_results._stderr_tail
CliSolver._stream_cost = _cli_results._stream_cost
CliSolver._summarize_async = _cli_results._summarize_async
