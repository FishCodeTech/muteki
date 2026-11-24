export enum EventType {
  RUN_PREPARING = "run.preparing",
  RUN_STARTED = "run.started",
  RUN_TITLED = "run.titled",
  RUN_FINISHED = "run.finished",
  RUN_REOPENED = "run.reopened",
  PENTEST_REPORT_STARTED = "pentest.report.started",
  PENTEST_REPORT_COMPLETED = "pentest.report.completed",
  PENTEST_REPORT_FAILED = "pentest.report.failed",
  FOLLOWUP_DELTA = "followup.delta",
  FOLLOWUP_PROGRESS = "followup.progress",
  FOLLOWUP_CANCELLED = "followup.cancelled",
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
  HITL_RESOLVED = "hitl.resolved",
  CONTROL_COMMAND = "control.command",
  HITL_TRANSLATED = "hitl.translated",
  AGENT_RUNTIME_EVENT = "agent.runtime",
  GRAPH_COMPACTED = "graph.compacted",
  WORKER_LIFECYCLE = "worker.lifecycle",
  LOCAL_PI_SESSION = "local_pi.session",
  LOCAL_PI_TURN_COMPLETED = "local_pi.turn_completed",
  WORKER_QUEUED = "worker.queued",
  WORKER_ERROR = "worker.error",
  WORKER_CREDENTIAL_REVOKED = "worker.credential_revoked",
  RUN_RESTART_RECOVERY = "run.restart_recovery",
  RUN_QUEUE_RECOVERED = "run.queue_recovered",
  GUARD_APPROVAL_REQUESTED = "guard.approval.requested",
  GUARD_APPROVAL_RESOLVED = "guard.approval.resolved",
  GUARD_ACTION_SETTLED = "guard.action.settled",
  GUARD_EGRESS_STATE_CHANGED = "guard.egress.state_changed",
  GUARD_EMERGENCY_STOP_RECEIPT = "guard.emergency_stop.receipt",
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
  phase?: string;
  intent_id?: string;
  tokens_spent?: number;
}

export type WorkerLaneRole = "worker" | "review" | "verifier";
export type WorkerConnectionKind = "official" | "custom_endpoint" | "system";

export type ArtifactView =
  | "usage" | "collaboration" | "workers" | "timeline" | "evidence"
  | "findings" | "credentials" | "pocs" | "routes" | "directives";

export interface SolverLane {
  solverId: string;
  reasoning: string;
  toolLines: string[];
  status: string;
  solved: boolean;
  flag?: string;
  online: boolean;
  statusReason?: string;
  engine?: string;
  role?: WorkerLaneRole;
  phase?: string;
  intentId?: string;
  tokensSpent?: number;
  paused?: boolean;
  session?: string;
  runtime?: WorkerRuntimeStatus;
  profileId?: string;
  profileLabel?: string;
  model?: string;
  accountId?: string;
  endpointHost?: string;
  connection?: WorkerConnectionKind;
  provider?: string;
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

export interface SolverCost {
  usd: number;
  tokensIn: number;
  tokensOut: number;
  unpricedCalls?: number;
  engine?: string;
}

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
  followupId?: string;
  followupKind?: string;
  followupDetail?: string;
  followupSucceeded?: boolean;
  reportId?: string;
  actor?: { engine?: string; model?: string; transport?: string };
  directiveId?: string;
  statusKey?: string;
  mainThread?: boolean;
  kind: "reasoning" | "text" | "tool" | "insight" | "guidance" | "flag" | "status" | "progress";
  content: string;
  ts: number;
  progressBrief?: ProgressBrief;
  i18nKey?: string;
  i18nVars?: Record<string, string>;
  sealed?: boolean;
  toolOutput?: string;
  toolFailed?: boolean;
  toolPending?: boolean;
  toolCallId?: string;
  toolStartedAt?: number;
  toolEndedAt?: number;
  toolTimingSource?: "call_id" | "serial";
  executionGeneration?: number;
}

export type HitlNeedKind =
  | "external_blocker" | "lane_lock_request" | "route_dead_end"
  | "worker_uncertainty" | "operator_directive_needed";

export interface HitlRequest {
  id: string;
  prompt: string;
  promptZh?: string;
  worker?: string;
  intentId?: string;
  routeHash?: string;
  options: string[];
  needKind?: HitlNeedKind;
  pausesBehavior?: boolean;
  deliveryCommandId?: string;
  deliveryStatus?: ControlCommandStatus;
  deliveryDetail?: string;
  deliveryAction?: string;
  deliveryUpdatedAt?: number;
  ts: number;
}

export interface HitlHistoryEntry {
  id: string;
  prompt: string;
  promptZh?: string;
  worker?: string;
  intentId?: string;
  routeHash?: string;
  needKind?: HitlNeedKind;
  pausesBehavior?: boolean;
  ts: number;
  resolvedTs: number;
  resolutionAction: string;
  deliveryCommandId?: string;
  deliveryStatus?: ControlCommandStatus;
  deliveryDetail?: string;
  deliveryMode?: string;
  continuationIntentId?: string;
}

export type ControlCommandStatus =
  | "received" | "persisted" | "routed" | "effect_observed"
  | "partial" | "failed" | "unknown" | "rejected";

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
  heldBy?: string;
  ts: number;
}

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
  | "applied" | "consumed" | "received" | "queued" | "bound" | "acted" | "superseded" | "expired" | "rejected";

export interface OperatorDirective {
  id: string;
  text: string;
  action: string;
  status: DirectiveStatus;
  preemption?: DirectivePreemption;
  boundWorker?: string;
  ts: number;
}

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
  question?: string;
  answer?: string;
  description?: string;
  goalId?: string;
  kind?: string;
  stepSeq?: number;
  resultDetail?: string;
  summary?: string;
  workerClass: string;
  fromFacts: number[];
  toFactSeq?: number;
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
  dispatchState?: IntentDispatchState;
  closeReason?: string;
  worker?: string;
  proposedBy?: string;
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
  factSeq?: number;
  sourceObservationSeq?: number;
  fact: string;
  summary?: string;
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
  state?: FactLifecycleState;
  retirementReason?: string;
  supersededBy?: string;
  replacementFactSeq?: number;
  retiredTs?: number;
  revisionSeq?: number;
  mergedInto?: number;
  intentId?: string;
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

export interface FlagOrigin {
  actor: string;
  ts: number;
  intentId?: string;
}

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
  kind: string;
  actor: string;
  ts: number;
  label: string;
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

export interface ChallengerView {
  status: "idle" | "running" | "finished" | "committed" | "failed" | "stopped";
  mode?: "ctf" | "pentest";
  startedTs?: number;
  finishedTs?: number;
  committedTs?: number;
  failedTs?: number;
  proposed?: number;
  error?: string;
  code?: string;
}

export interface BlackboardSubGoal {
  id: string;
  criterion: string;
  state: "open" | "satisfied";
  reason?: string;
  evidenceFactSeqs: number[];
  actor: string;
  createdTs: number;
  satisfiedTs?: number;
}

export interface BlackboardView {
  challenger: ChallengerView;
  subGoals: BlackboardSubGoal[];
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
  flag?: string;
  flags?: string[];
  flagOrigins: Record<string, FlagOrigin>;
  events: BlackboardEvent[];
  reasonRuns: ReasonRunView[];
  workers: string[];
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
  workerPrompts: Record<string, WorkerPromptRecord[]>;
  runId: string;
  challengeName: string;
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
  hitlHistory: HitlHistoryEntry[];
  runtimeEvents: AgentRuntimeFeedEvent[];
  controlCommands: ControlCommand[];
  controlGeneration: number;
  executionGeneration: number;
  operatorDirectives: OperatorDirective[];
  resourceLocks: ResourceLock[];
  lockRequests: LockRequest[];
  compactEpochs: number;
  lastCompactTs?: number;
  gauge: ContextGauge;
  usd: number;
  tokensIn: number;
  tokensOut: number;
  costBySolver: Record<string, SolverCost>;
  costHistory: Record<string, CostHistorySample[]>;
  started: boolean;
  preparing: boolean;
  finished: boolean;
  followupPending: boolean;
  activeFollowupJobId?: string;
  activeFollowupKind?: string;
  followupProgress?: { phase: string; summary: string; tool_name?: string; turn?: number; elapsed_ms?: number };
  reportGenerating?: boolean;
  activeReportJobId?: string;
  hasWriteup: boolean;
  auxiliaryCost?: SolverCost;
  startedAt?: number;
  finishedAt?: number;
  activeDurationMs: number;
  activeSince?: number;
  solved: boolean;
  flag?: string;
  flags: string[];
  invalidatedFlags: string[];
  expectedFlags: number;
  mode?: "ctf" | "pentest";
  driverKind?: "swarm" | "local_pi" | "idle";
  operatorLabel?: "pending_followup" | "processed" | "ignored" | null;
  operatorLabelUpdatedAt?: string | null;
  localPiSession?: Record<string, any> | null;
  chatThreadId?: string;
  platformConfirmationRequired?: boolean;
  flagConfirmations: FlagConfirmation[];
  expectedFindings: number;
  taskContract?: TaskContractView;
  completionKind?: "outcome" | "count" | "coverage";
  outcomePredicate?: "first_valid_report" | "command_execution" | "shell_access" | "admin_access";
  multiFlag?: boolean;
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
  goalWhy?: string;
  awaitingOperator?: string;
  racing?: boolean;
  raceTotal?: number;
  raceFinished?: number;
  verifying?: number;
  degradedEngines: Record<string, string>;
  productionGuard?: ProductionGuardState;
}

export type GuardEgressState = "open" | "held" | "revoked";
export type GuardDeliveryStatus = "not_sent" | "sent" | "outcome_unknown" | "completed";
export type GuardApprovalDecisionAction = "allow_once" | "deny_once" | "emergency_stop";
export type GuardStopStatus = "idle" | "emergency_stopping" | "stopped" | "stop_partial" | "stop_unknown";

export interface GuardCronusAssessment {
  available: boolean;
  decision?: string;
  decision_probs?: Record<string, number>;
  proceed_prob?: number;
  ask_prob?: number;
  reject_prob?: number;
  scope_p_true?: number;
  latency_ms?: number;
  fail_closed_reason?: string;
  error?: string;
  model_name?: string;
}

export interface GuardPendingApproval {
  approval_id: string;
  run_id?: string;
  tool_name: string;
  action_summary: string;
  command?: string;
  http_summary?: string;
  script_summary?: string;
  args_redacted?: string;
  worker_id?: string;
  worker_role?: string;
  delivery_status: GuardDeliveryStatus;
  status: "pending" | "resolving" | "approved" | "denied" | "expired" | "stopped";
  reason?: string;
  risk_level?: string;
  cronus?: GuardCronusAssessment | null;
  requested_at?: number | string;
  expires_at?: number | string;
}

export interface GuardActionReceipt {
  receipt_id: string;
  approval_id?: string;
  tool_name: string;
  action_summary: string;
  worker_id?: string;
  worker_role?: string;
  decision?: string;
  delivery_status: GuardDeliveryStatus;
  note?: string;
  cronus?: GuardCronusAssessment | null;
  ts: number | string;
}

export interface GuardEmergencyStopReceipt {
  stop_id?: string;
  status: string;
  egress_revoked?: boolean;
  workers_killed?: string[];
  workers_remaining?: string[];
  unknown_actions?: string[];
  detail?: string;
  ts?: number | string;
}

export interface GuardMetrics {
  total_evaluated: number;
  allowed: number;
  escalated: number;
  denied: number;
  cronus_errors: number;
  avg_cronus_ms: number;
}

export interface ProductionGuardState {
  enabled: boolean;
  mode: "off" | "protected";
  egress_state: GuardEgressState;
  pending_approvals: GuardPendingApproval[];
  recent_receipts: GuardActionReceipt[];
  emergency_stop_receipt: GuardEmergencyStopReceipt | null;
  metrics: GuardMetrics;
  stop_status?: GuardStopStatus;
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
