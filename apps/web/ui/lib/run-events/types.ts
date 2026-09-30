/** Event and deck types. Moved from events.ts. */
export enum EventType {
  RUN_PREPARING = "run.preparing",
  RUN_STARTED = "run.started",
  RUN_TITLED = "run.titled",
  RUN_FINISHED = "run.finished",
  RUN_REOPENED = "run.reopened",
  FOLLOWUP_STARTED = "followup.started",
  FOLLOWUP_COMPLETED = "followup.completed",
  FOLLOWUP_FAILED = "followup.failed",
  PROGRESS_BRIEF = "progress.brief",
  PROJECTION_INCOMPLETE = "projection.incomplete",
  WORKER_STATUS = "worker.status",
  WORKER_PROMPT = "worker.prompt",
  WORKER_FINISHED = "worker.finished",
  TEXT_MESSAGE_DELTA = "text.delta",
  REASONING_DELTA = "reasoning.delta",
  TOOL_CALL_START = "tool.start",
  TOOL_CALL_ARGS = "tool.args",
  TOOL_CALL_RESULT = "tool.result",
  TERMINAL_OUTPUT = "terminal.output",
  CONTEXT_STATE = "context.state",
  SOLVE_GRAPH_DELTA = "solvegraph.delta",
  INSIGHT_BUS_EVENT = "insight.event",
  SHARED_GRAPH_DELTA = "sharedgraph.delta",
  REASON_INTENT = "reason.intent",
  BLACKBOARD_DELTA = "blackboard.delta",
  NODE_SUMMARIZED = "node.summarized",
  COST_UPDATE = "cost.update",
  STALLED = "guard.stalled",
  GUIDANCE_INJECTED = "coordinator.guidance",
  HITL_REQUEST = "hitl.request",
  HITL_RESPONSE = "hitl.response",
  CONTROL_COMMAND = "control.command",
  HITL_TRANSLATED = "hitl.translated",
  AGENT_RUNTIME_EVENT = "agent.runtime",
  GRAPH_COMPACTED = "graph.compacted",
  WORKER_LIFECYCLE = "worker.lifecycle",
}

export interface MutekiEvent {
  event_type: EventType;
  seq: number;
  ts: number;
  run_id: string;
  challenge_id?: string | null;
  solver_id?: string | null;
  payload: Record<string, any>;
}

/** Runtime execution status folded from backend worker.status payloads. */
export interface WorkerRuntimeStatus {
  backend?: string;
  exec_id?: string;
  container?: string;
  tag?: string;
  driver?: string;
  cwd?: string;
  argv0?: string;
  status?: string;
  started_at?: number;
  finished_at?: number | null;
  rc?: number | null;
  timed_out?: boolean;
  oom_killed?: boolean;
  cancelled?: boolean;
  steered?: boolean;
  error?: string;
  phase?: string;          // I: granular lifecycle phase
  intent_id?: string;      // I: the intent this worker is executing
  tokens_spent?: number;   // I: running token total for this worker
}

export type WorkerLaneRole = "worker" | "review" | "verifier";
export type WorkerConnectionKind = "official" | "custom_endpoint" | "system";

/** The runtime panel's secondary detail views (the conversation stays primary). */
export type ArtifactView =
  | "usage" | "collaboration" | "workers" | "timeline" | "evidence"
  | "findings" | "credentials" | "pocs" | "routes" | "directives";

/** Per-solver derived view the deck renders (the "race" lanes). */
export interface SolverLane {
  solverId: string;
  reasoning: string; // accumulated reasoning deltas (current step)
  toolLines: string[]; // condensed tool results
  status: string;
  solved: boolean;
  flag?: string;
  online: boolean;
  statusReason?: string;
  engine?: string;
  role?: WorkerLaneRole;
  phase?: string;
  intentId?: string;      // I: the intent this worker is executing
  tokensSpent?: number;   // I: running token total for this worker
  paused?: boolean;       // I: worker is paused (operator pause / lane wait)
  session?: string; // live CLI session id — `claude -r <id>` / `codex exec resume <id>`
  runtime?: WorkerRuntimeStatus;
  profileId?: string;
  profileLabel?: string;
  model?: string;
  accountId?: string;
  endpointHost?: string;
  connection?: WorkerConnectionKind;
  provider?: string;
  /** Sticky: spawned during race-scout (phase may later read bootstrap). */
  raceScout?: boolean;
  firstSeenAt?: number;
  finishedAt?: number;
  spawnPhase?: string;
  spawnedBy?: string;
}

export interface WorkerPromptRecord {
  id: string;
  workerId: string;
  prompt: string;
  kind: "execute" | "resume";
  status: "prepared" | "sent" | "not_sent" | "unknown";
  transport: "argv" | "stdin";
  redacted: boolean;
  session?: string;
  engine?: string;
  model?: string;
  intentId?: string;
  phase?: string;
  ts: number;
  updatedAt: number;
}

/** A single agent's running cost/token totals, for the cost hover card. */
export interface SolverCost {
  usd: number;
  tokensIn: number;
  tokensOut: number;
  unpricedCalls?: number;
  engine?: string; // "claude" | "codex" | "cursor" | "deepseek" (best-effort)
}

/** One COST_UPDATE sample: cumulative tokens for that solver at `ts`. */
export interface CostHistorySample {
  ts: number;
  tokens: number;
}

export interface SolveGraphView {
  evidence: string[];
  hypotheses: { id: string; statement: string; status: string }[];
  deadEnds: string[];
  flag?: string;
}

/** P-A/P-B: the shared, evidence-gated graph — split verified vs candidate. */
export interface SharedEvidence {
  fact: string;
  verified: boolean;
  confidence: number;
  actor: string;
  verifier: string;
}
export interface SharedGraphView {
  verified: SharedEvidence[];
  candidates: SharedEvidence[];
}

/** P-C: a typed intent proposed by the reason phase. */
export interface ReasonIntent {
  id: string;
  goal: string;
  workerClass: string;
}
export interface ReasonView {
  goalMet: boolean;
  intents: ReasonIntent[];
  audit: string[];
}

export interface ContextGauge {
  zones: { label: string; tokens: number }[];
  total: number;
  limit: number;
}

/** A turn in the conversation transcript (ChatGPT/Claude-style view). The agent
 *  side is derived from the event stream (reasoning/text/tool/insight); the human
 *  side is what the operator typed and the system echoes of HITL commands. */
export type ChatRole = "agent" | "human" | "system";

export interface ProgressBriefRef {
  kind: string;
  id: string;
}

export interface ProgressBriefItem {
  text?: string;
  textKey?: string;
  ref?: ProgressBriefRef;
}

export interface ProgressBrief {
  id: string;
  kind: "periodic" | "milestone" | "blocker" | "stalled" | "final" | "requested";
  phase: string;
  mode: "ctf" | "pentest";
  trigger: string;
  summary?: string;
  summaryKey?: string;
  summaryVars?: Record<string, string | number>;
  sourceFromSeq: number;
  sourceToSeq: number;
  sections: {
    confirmed: ProgressBriefItem[];
    active: ProgressBriefItem[];
    blocked: ProgressBriefItem[];
    next: ProgressBriefItem[];
  };
}

export interface ChatMessage {
  id: string;
  role: ChatRole;
  solverId?: string;
  /** Correlates a post-solve pending row with its terminal event. */
  followupId?: string;
  followupKind?: string;
  /** Stable correlation for replay-safe system status rows. */
  statusKey?: string;
  // True for worker-produced conversational follow-ups (post-solve standby ask /
  // writeup) that should still appear in the main coordinator thread. The solverId
  // is kept for activity/diagnostics, but the conversation spine treats it as an
  // operator-facing answer rather than worker firehose.
  mainThread?: boolean;
  kind: "reasoning" | "text" | "tool" | "insight" | "guidance" | "flag" | "status" | "progress";
  content: string;
  ts: number;
  progressBrief?: ProgressBrief;
  // Optional i18n hook for system-generated lifecycle lines (run started /
  // solved / finished). The render layer translates `i18nKey` with `i18nVars`
  // when present; `content` stays as the English fallback. Agent-produced text
  // (reasoning/insight/tool) carries no key — it renders verbatim, in whatever
  // language the swarm emitted.
  i18nKey?: string;
  i18nVars?: Record<string, string>;
  // Sealed bubbles no longer accept streamed deltas. Set when a Pi/OMP
  // message_end (or equivalent) closes the current assistant message so the
  // next turn opens a new row instead of concatenating into the previous one.
  sealed?: boolean;
  // Tool rows: command stays in `content`; stdout lives here so the ledger can
  // collapse a group to one line and expand a single call for the output.
  toolOutput?: string;
  toolFailed?: boolean;
  toolPending?: boolean;
}

/** A pending human-in-the-loop decision the agent asked for. */
export type HitlNeedKind =
  | "external_blocker" | "lane_lock_request" | "route_dead_end"
  | "worker_uncertainty" | "operator_directive_needed";

export interface HitlRequest {
  id: string;
  prompt: string;       // the raw (often English) hand-raise text
  promptZh?: string;    // async zh translation (HITL_TRANSLATED), shown when ready
  worker?: string;      // solver_id that raised it — matches the translation event
  options: string[];
  needKind?: HitlNeedKind;  // F: how the swarm triaged the hand-raise
  pausesBehavior?: boolean; // F: only external_blocker freezes the swarm for an answer
  /** Once `decision_closed=true` proves the durable DecisionAnswer companion
   *  exists, the decision becomes read-only. A later delivery failure does not
   *  mean the answer was never recorded, so the UI recovers that command instead
   *  of submitting a second answer for the same request. */
  deliveryCommandId?: string;
  deliveryStatus?: ControlCommandStatus;
  deliveryDetail?: string;
  ts: number;
}

export type ControlCommandStatus =
  | "received" | "persisted" | "routed" | "effect_observed"
  | "partial" | "failed" | "unknown" | "rejected";

/** Auditable lifecycle of one operator command. An accepted HTTP request is not
 *  proof of an effect; only `effect_observed` may change displayed runtime state. */
export interface ControlCommand {
  id: string;
  action: string;
  target: string;
  status: ControlCommandStatus;
  requestId?: string;
  effect?: Record<string, any>;
  detail?: string;
  ts: number;
}

export type HitlDeliveryPhase =
  | "open" | "pending" | "observed"
  | "partial" | "failed" | "unknown" | "rejected";

/** Pure decision-delivery projection shared by the reducer tests and HitlCard.
 *  Anything except `open` is deliberately locked: even UNKNOWN/FAILED/PARTIAL
 *  is a durable answer command whose effect needs recovery, not a second answer. */
export function hitlDeliveryState(req: HitlRequest): {
  phase: HitlDeliveryPhase;
  locked: boolean;
  commandId?: string;
  detail?: string;
} {
  if (!req.deliveryCommandId && !req.deliveryStatus) {
    return { phase: "open", locked: false };
  }
  const status = req.deliveryStatus;
  const phase: HitlDeliveryPhase = status === "effect_observed" ? "observed"
    : status === "partial" ? "partial"
      : status === "failed" ? "failed"
        : status === "unknown" ? "unknown"
          : status === "rejected" ? "rejected"
            : "pending";
  return {
    phase,
    locked: true,
    commandId: req.deliveryCommandId,
    detail: req.deliveryDetail,
  };
}

export interface ResourceLock {
  lockId: string;
  resourceKey: string;
  scope: string;
  riskClass?: string;
  status: "active" | "released" | "expired" | "denied" | "requested" | "deferred";
  ownerWorker?: string;
  /** Current holder when the record is a denial (empty for active/released). */
  heldBy?: string;
  ts: number;
}

/** A denied lock request: `requester` asked for a resource `holder` still owns. */
export interface LockRequest {
  lockId: string;
  resourceKey: string;
  scope: string;
  riskClass?: string;
  requester: string;
  holder: string;
  ts: number;
}

export type DirectivePreemption = "none" | "soft_rebind" | "graceful_drain" | "force_cancel";
export type DirectiveStatus =
  | "received" | "queued" | "bound" | "acted" | "superseded" | "expired" | "rejected";

export interface OperatorDirective {
  id: string;
  text: string;
  action: string;
  status: DirectiveStatus;
  preemption?: DirectivePreemption;
  boundWorker?: string;
  ts: number;
}

/** The shared knowledge blackboard (SQLiteSharedGraph), folded from the
 *  blackboard.delta event stream. This is the swarm's COLLABORATION layer:
 *  every intent's claim lifecycle (who took it, is it done), every fact with its
 *  full provenance, dead-ends, the flag — plus a raw event timeline. */
export type IntentDispatchState = "active" | "resume" | "retired" | "closed";
export type FactLifecycleState =
  | "unresolved" | "challenged" | "revalidated"
  | "rejected" | "merged" | "superseded";

export interface FactEvidenceProvenance {
  target?: string;
  artifactRefs?: {
    artifactId: string;
    sha256?: string;
    size?: number;
    command?: string;
    toolEventSeq?: number;
  }[];
  toolEventId?: string;
  toolEventSeq?: number;
  toolEventTs?: number;
  toolCallId?: string;
  workerId?: string;
  intentId?: string;
  targetEpoch?: string;
  artifactId?: string;
  artifactSha256?: string;
  observedAt?: number;
  promotedAt?: number;
}

export interface FactObservation {
  observationSeq?: number;
  actor: string;
  verified: boolean;
  confidence: number;
  artifactId?: string;
  witness?: string;
  provenance?: FactEvidenceProvenance;
  ts: number;
}

export interface BlackboardIntent {
  id: string;
  goal: string;
  summary?: string;      // deepseek-flash zh gist (replaces goal in the card head)
  workerClass: string;
  fromFacts: number[];   // shared_graph fact seqs that motivated this intent
  toFactSeq?: number;    // shared_graph fact seq produced when the intent concluded
  dependsOn?: string[];
  expectedObservable?: string;
  stopCondition?: string;
  coverageKey?: string;
  routeHash?: string;
  laneKey?: string;
  priority?: number;
  requestedPriority?: string;
  priorityReason?: string;
  valueClaim?: Record<string, unknown>;
  noveltyKey?: string;
  requiresCapabilities?: string[];
  requiredPocs?: string[];
  status: "open" | "claimed" | "done";
  dispatchState?: IntentDispatchState;  // A/J: active | resume | retired | closed
  closeReason?: string;  // why it left the dispatch pool
  worker?: string;       // the solver that claimed it
  proposedBy?: string;   // actor of intent_proposed (reason / coordinator / a worker)
  proposedTs: number;
  claimedTs?: number;
  concludedTs?: number;
}
export interface BlackboardObservation {
  observationSeq: number;
  text: string;
  actor: string;
  intentId?: string;
  targetEpoch?: string;
  witness?: string;
  artifactId?: string;
  confidence: number;
  admitted: boolean;
  admittedFactSeq?: number;
  claimedVerified: boolean;
  canonicalKey?: string;
  provenance?: FactEvidenceProvenance;
  ts: number;
}

export interface BlackboardCapability {
  key: string;
  targetEpoch: string;
  kind: string;
  quality?: string;
  sharing?: string;
  state: "active" | "retired";
  accessPathId?: string;
  ts: number;
}

export interface BlackboardAccessPath {
  id: string;
  targetEpoch: string;
  endpoint?: string;
  reach: string[];
  operations: string[];
  quality?: string;
  capabilityKeys: string[];
  state: "ready" | "degraded" | "closed";
  ts: number;
}

export interface BlackboardCapabilityGap {
  id: string;
  targetEpoch: string;
  description: string;
  requiredCapabilities: string[];
  consumers: string[];
  state: "open" | "resolved";
  ts: number;
}

export interface BlackboardValueReceipt {
  id: string;
  intentId: string;
  effect?: string;
  requestedPriority?: string;
  effectivePriority: number;
  achieved: boolean;
  capabilitiesAdded: string[];
  unblockedCount: number;
  ts: number;
}
export interface BlackboardFact {
  factSeq?: number;      // shared_graph event seq; stable node id / parent link
  sourceObservationSeq?: number;
  fact: string;
  summary?: string;      // deepseek-flash zh gist (replaces fact in the card head)
  verified: boolean;
  confidence: number;
  actor: string;
  verifier: string;
  witness?: string;
  artifactId?: string;
  targetEpoch?: string;
  identitySha256?: string;
  provenance?: FactEvidenceProvenance;
  observations?: FactObservation[];
  promotedTs?: number;
  challenged?: boolean;
  challengeReason?: string;
  revalidated?: boolean;
  state?: FactLifecycleState;  // A: lifecycle state (rejected/merged/superseded retire it)
  mergedInto?: number;         // when state=merged: the fact seq it folded into
  intentId?: string;           // G0: the intent that PRODUCED this fact (intent_products edge)
  ts: number;
}
export interface BlackboardDeadEnd {
  deadEndSeq?: number;
  reason: string;
  testedScope?: string;
  observedResult?: string;
  intentId?: string;
  targetEpoch?: string;
  actor: string;
  ts: number;
}
export interface BlackboardReviewFinding {
  id: string;
  kind: string;
  severity: string;
  summary: string;
  routeHash?: string;
  branchId?: string;
  recommendedActions?: string[];
  evidenceSeqs?: number[];
  intentIds?: string[];
  /** solver_id of the review worker that produced the finding (actor stays coordinator). */
  worker?: string;
  actor: string;
  ts: number;
}
export interface BlackboardGatedFinding {
  id: string;
  findingClass: string;
  resourceId: string;
  identityA?: string;
  identityB?: string;
  title?: string;
  source?: string;
  actor: string;
  ts: number;
}
export type BlackboardTruncation = {
  facts?: boolean;
  reviews?: boolean;
  pocs?: boolean;
  routes?: boolean;
  directives?: boolean;
  deadEnds?: boolean;
};

export {
  DEAD_END_CAP,
  DIRECTIVE_CAP,
  FACT_CAP,
  POC_CAP,
  REVIEW_CAP,
  ROUTE_CAP,
  capPush,
  markTruncated,
} from "./projection-limits";

export interface BlackboardSuppressedRoute {
  routeHash: string;
  label?: string;
  reason: string;
  actor: string;
  ts: number;
  reopened?: boolean;
}
export interface BlackboardBranch {
  branchId: string;
  title: string;
  actor: string;
  ts: number;
  status?: string;
}
export interface BlackboardDirective {
  action: string;
  directive: string;
  actor: string;
  routeHash?: string;
  ts: number;
}

/** Who reported a flag and under which intent, keyed by flag text (first sighting wins). */
export interface FlagOrigin {
  actor: string;
  ts: number;
  intentId?: string;
}

/** Internal graph record vs platform confirmation. Never treat internal as score. */
export type PlatformConfirmationStatus = "internal" | "pending" | "accepted" | "rejected";

export interface FlagConfirmation {
  id: string;
  flag: string;
  submissionId?: string;
  candidateId?: string;
  status: PlatformConfirmationStatus;
  code?: string;
  title?: string;
  source?: string;
  detail?: string;
  actor?: string;
  ts: number;
}
export interface BlackboardPoc {
  id: string;
  name: string;
  entryCommand: string;
  status: "available" | "wip" | "directional" | "spent" | "quarantined" | "rejected";
  note?: string;
  intentId?: string;
  artifactId?: string;
  path?: string;
  worker?: string;
  savedTs: number;
  claimedTs?: number;
  concludedTs?: number;
}
export interface BlackboardEvent {
  id: string;
  kind: string;          // intent_proposed | intent_claimed | … | fact_added | dead_end | flag_found
  actor: string;
  ts: number;
  label: string;         // pre-rendered one-line summary for the timeline
}

export interface ReasonAttemptView {
  attemptIndex: number;
  responseStatus: string;
  timedOut: boolean;
  finishReason: string;
  rawResponseArtifactId?: string;
  rawResponseSha256?: string;
  rawResponseChars: number;
  parseStatus: string;
  parseDetail?: string;
  rawIntentCount: number;
  parsedIntentCount: number;
}

export interface ReasonIntentDecisionView {
  rawIndex: number;
  modelIntentId: string;
  resolvedIntentId?: string;
  goal?: string;
  outcome: string;
  stage: string;
  reasonCode: string;
}

export interface ReasonRunView {
  ts: number;
  proposed: number;
  droppedTotal: number;
  plannerFailure?: string;
  attempts: ReasonAttemptView[];
  intentDecisions: ReasonIntentDecisionView[];
}

export interface BlackboardView {
  intents: BlackboardIntent[];
  facts: BlackboardFact[];
  observations: BlackboardObservation[];
  capabilities: BlackboardCapability[];
  accessPaths: BlackboardAccessPath[];
  capabilityGaps: BlackboardCapabilityGap[];
  valueReceipts: BlackboardValueReceipt[];
  pocs: BlackboardPoc[];
  deadEnds: BlackboardDeadEnd[];
  gatedFindings: BlackboardGatedFinding[];
  reviewFindings: BlackboardReviewFinding[];
  suppressedRoutes: BlackboardSuppressedRoute[];
  branches: BlackboardBranch[];
  directives: BlackboardDirective[];
  flag?: string;                       // first/primary flag (back-comat)
  flags?: string[];                    // every distinct flag captured (multi-flag)
  flagOrigins: Record<string, FlagOrigin>;
  events: BlackboardEvent[];           // append-only timeline
  reasonRuns: ReasonRunView[];          // full diagnostics from existing reason_done events
  workers: string[];                   // distinct actors seen (for lanes/legend)
  truncated?: BlackboardTruncation;
}

export interface AgentRuntimeFeedEvent {
  id: string;
  solverId: string;
  eventType: string;
  adapter?: string;
  nativeType?: string;
  payload: Record<string, unknown>;
  ts: number;
}

export interface DeckState {
  /** Full prompt history, independent of the capped activity feed. */
  workerPrompts: Record<string, WorkerPromptRecord[]>;
  runId: string;
  challengeName: string;
  /** True when an older autogenerated name may be replaced by RUN_TITLED. */
  challengeNameAutogen?: boolean;
  category: string;
  target: string;
  lanes: Record<string, SolverLane>;
  graph: SolveGraphView;
  sharedGraph: SharedGraphView;
  reason: ReasonView;
  insights: string[];
  terminal: string[];
  chat: ChatMessage[];
  blackboard: BlackboardView;
  hitlRequests: HitlRequest[];
  runtimeEvents: AgentRuntimeFeedEvent[];
  controlCommands: ControlCommand[];          // durable command lifecycle/effects
  controlGeneration: number;                  // latest observed desired-state generation
  executionGeneration: number;                // latest run execution generation
  operatorDirectives: OperatorDirective[];  // B: first-class operator steering
  resourceLocks: ResourceLock[];            // E: unified site/account/listener locks
  lockRequests: LockRequest[];              // denied lock requests (requester → holder)
  compactEpochs: number;                    // H: how many times the graph was compacted
  lastCompactTs?: number;                   // H: timestamp of the last compaction
  gauge: ContextGauge;
  usd: number;
  // cumulative token usage across all engines (deepseek API + claude/codex/cursor
  // CLI workers). usd/tokensIn/tokensOut are the GLOBAL totals, derived by summing
  // costBySolver (the backend emits per-solver-scoped COST_UPDATE events carrying
  // each solver's running total, so we key by solver and re-sum — a single payload
  // is never the global total). Shown next to the $ figure; cursor contributes
  // tokens at $0 (subscription-backed).
  tokensIn: number;
  tokensOut: number;
  // per-agent breakdown for the cost/token hover card: solverId → running totals.
  costBySolver: Record<string, SolverCost>;
  /** Cumulative token samples per solver from COST_UPDATE, newest 120 kept. */
  costHistory: Record<string, CostHistorySample[]>;
  started: boolean;
  preparing: boolean;
  finished: boolean;
  /** Ask/Writeup lifecycle is independent from the completed run lifecycle. */
  followupPending: boolean;
  // wall-clock bookends (event ts, seconds or ms — normalised at read time).
  // startedAt = first RUN_STARTED; finishedAt = RUN_FINISHED. A running run has
  // startedAt but no finishedAt → elapsed is measured against "now".
  startedAt?: number;
  finishedAt?: number;
  solved: boolean;
  flag?: string;
  // multi-flag: every distinct flag collected (dedup, order). `flag` stays the
  // first for back-compat. expectedFlags drives the "collecting N/total" state.
  flags: string[];
  // Flags the operator marked false. A reopened run reuses the same graph, so old
  // flag_found events may be replayed; keep them out of the current solved state.
  invalidatedFlags: string[];
  expectedFlags: number;
  mode?: "ctf" | "pentest";
  /** True only for platform-bound runs (e.g. TSec). Internal findings are not the score. */
  platformConfirmationRequired?: boolean;
  flagConfirmations: FlagConfirmation[];
  expectedFindings: number;
  taskContract?: TaskContractView;
  /** Parsed from the natural-language pentest request by the backend planner. */
  completionKind?: "outcome" | "count" | "coverage";
  outcomePredicate?: "first_valid_report" | "command_execution" | "shell_access" | "admin_access";
  // multi-flag MODE bit. When true with an unknown count (expectedFlags<=1), a
  // saved flag does NOT mark the run solved — it keeps "collecting" until the run
  // finishes (operator STOP / no-progress pause). Decouples save from finish in the
  // UI exactly as the backend does. Default false = single-flag (first flag solves).
  multiFlag?: boolean;
  // how a finished run concluded: "solved" = a CTF flag was gated; "goal_met" =
  // a pentest engagement goal was reached (no flag); "finished" = ended without
  // either. Drives the outcome label (flag chip vs findings summary).
  outcomeReason?: "solved" | "goal_met" | "finished" | "operator_stop" | "budget_exhausted" | "runtime_failure" | "preflight_failed" | "no_progress";
  outcomeDetail?: string;
  outcomeErrorId?: string;
  outcomeFailureCode?: string;
  outcomeFailurePhase?: string;
  preflightFailures: Array<{
    errorId?: string;
    profileId: string;
    engine: string;
    model?: string;
    backend?: string;
    runtime?: string;
    stage?: string;
    layer?: string;
    code?: string;
    detail: string;
  }>;
  // pentest: why the engagement goal was judged met (from the goal_complete event).
  goalWhy?: string;
  // set when the coordinator PAUSED waiting for operator input (a worker raised a
  // NEED_INPUT / env_down). Holds the outstanding ask(s). Cleared on resume.
  awaitingOperator?: string;
  // race-scout layer: true while the front race round (3 engines in parallel,
  // single-shot) is running, before the main coordinator loop. Drives the "racing" status pill.
  racing?: boolean;
  /** Race-scout roster size (from race_started). */
  raceTotal?: number;
  /** Race-scout workers that have finished (from race_worker_finished). */
  raceFinished?: number;
  /** Active report-reproduction verifier workers (independent channel). */
  verifying?: number;
  // engines dropped from THIS run's roster by a dispatch-time health-check failure
  // (e.g. cursor headless auth lapsed → "Authentication required"). engine → reason.
  // Lets the worker panel / engine bar show "cursor degraded: …" instead of the
  // engine silently never appearing. Cleared per-engine on a recover event.
  degradedEngines: Record<string, string>;
}

export interface TaskContractView {
  rawInstruction: string;
  mode: "ctf" | "pentest";
  title: string;
  category: string;
  attachments: Array<{ path: string; name: string; size: number; summary: string }>;
  executionTarget?: string;
  authorizationScope: string;
  completion: {
    kind: "ctf_flag" | "outcome" | "count" | "coverage";
    goal: string;
    taskType: string;
    quantity?: number;
    flagFormat: string;
    flagFormatHint: string;
    expectedFlags: number;
    multiFlag: boolean;
    findingClass: string;
    outcomePredicate: string;
    collectUntilCoverage: boolean;
  };
}
