import { toEpochMs } from "./format";
import {
  hitlDeliveryState,
  isFactRetired,
  isWorkerLane,
  swarmPhase,
  workerGeneration,
  type AgentRuntimeFeedEvent,
  type BlackboardEvent,
  type BlackboardFact,
  type BlackboardIntent,
  type BlackboardReviewFinding,
  type BlackboardView,
  type ChatMessage,
  type DeckState,
  type LockRequest,
  type OperatorDirective,
  type SolverLane,
  type WorkerLaneRole,
} from "./events";
import {
  activityTitleKey,
  knowledgeStatusKey,
} from "./statusLabels";
import {
  laneActivityDetail,
  laneStatusKind,
  type LaneStatusKind,
  type WorkerLanePresentationInput,
} from "./workerLanePresentation";
import { canvasModeOf } from "./swarmProjection";

export type CollaborationScope = "current" | "all";
export type CollaborationRelationKind =
  | "dispatch" | "handoff" | "review" | "verify" | "report" | "directive" | "lock" | "model";
export type CollaborationKnowledgeKind =
  | "intent" | "step" | "fact" | "observation" | "candidate" | "dead_end" | "poc" | "report"
  | "review" | "finding" | "route" | "branch" | "directive" | "flag" | "lock" | "goal";
export type CollaborationTone = "neutral" | "accent" | "success" | "warning" | "danger" | "muted";
export type CollaborationAgentRole = "coordinator" | "decision" | "source" | "worker" | "review" | "verifier";
/** Where an agent's start/end timestamps were taken from. */
export type CollaborationLifecycleSource = "lane" | "runtime" | "inferred" | "run";

export interface CollaborationKnowledgeItem {
  id: string;
  kind: CollaborationKnowledgeKind;
  title: string;
  detail: string;
  agentId?: string;
  ts: number;
  /** First time this row existed (intent proposed, fact observed, …). */
  appearedTs: number;
  claimedTs?: number;
  concludedTs?: number;
  promotedTs?: number;
  tone: CollaborationTone;
  status?: string;
  /** i18n key for `status`; omitted when the raw value should stay visible. */
  statusKey?: string;
  intentId?: string;
  factSeq?: number;
  artifactId?: string;
  witness?: string;
  confidence?: number;
  /** Intent rows: who proposed it (coordinator, operator, or a worker id). */
  proposerId?: string;
  /** Fact rows: the backend verifier string (solver id or a legacy engine name). */
  verifier?: string;
  /** Fact rows: verifier resolved to one worker of the model; unset when ambiguous. */
  verifierAgentId?: string;
  /** Lowercased title + detail + status for filter `includes`. */
  searchText: string;
}

/** The lane fields `toWorkerIdentity` reads; the display layer never sees the raw SolverLane. */
export type CollaborationAgentIdentity = Pick<
  SolverLane,
  "engine" | "profileId" | "profileLabel" | "model" | "accountId" | "endpointHost" | "connection" | "provider"
>;

/** Output counters shared by worker cards and the coordinator. */
export interface CollaborationMetrics {
  facts: number;
  observations: number;
  candidates: number;
  deadEnds: number;
  pocs: number;
  reviews: number;
}

/** Coordinator-only counters (planning and dispatch rather than evidence). */
export interface CoordinatorMetrics {
  proposedIntents: number;
  assignedIntents: number;
  completedIntents: number;
  retiredIntents: number;
  reasonRounds: number;
  directives: number;
  flags: number;
  /** Verified (non-muted) facts across the whole board. */
  verifiedFacts: number;
  /** Current-generation workers still online / in total. */
  onlineWorkers: number;
  totalWorkers: number;
}

export interface CollaborationAgent extends CollaborationMetrics {
  id: string;
  role: CollaborationAgentRole;
  generation: number;
  /** Belongs to the current execution generation (the only current/previous test). */
  isCurrent: boolean;
  identity: CollaborationAgentIdentity;
  presentation: WorkerLanePresentationInput;
  session?: string;
  spawnedBy?: string;
  spawnPhase?: string;
  /** Spawned during the race-scout round. */
  raceScout?: boolean;
  phase: string;
  statusKind: LaneStatusKind;
  online: boolean;
  /** Online worker's claimed intent; never set for a finished worker. */
  currentIntent?: BlackboardIntent;
  /** Most recent claimed/done intent, shown as "last task" once the worker is offline. */
  lastIntent?: BlackboardIntent;
  intents: BlackboardIntent[];
  knowledge: CollaborationKnowledgeItem[];
  /** Lowercased id / phase / identity / intent / activity / knowledge titles. */
  searchText: string;
  latestActivity: string;
  latestActivityTs?: number;
  firstSeenAt?: number;
  finishedAt?: number;
  startedAt?: number;
  endedAt?: number;
  lifecycleSource: CollaborationLifecycleSource;
  /** Newest event / knowledge / chat timestamp attributed to this agent. */
  lastEventAt?: number;
  /**
   * Newest board-level step (fact / report / review written, intent claimed or
   * concluded, lane finished). Chat and runtime streams do not move it, so the
   * "recent progress" glow fires on substance rather than on every token.
   */
  lastProgressAt?: number;
  tokens: number;
  usd: number;
  unpricedCalls?: number;
  locks: number;
  isAnomaly: boolean;
  /** i18n key when statusReason is oom / timeout / error / budget. */
  anomalyReasonKey?: string;
  /** Open HITL prompts raised by this worker. */
  pendingHitl: number;
  coordination?: CoordinatorMetrics;
}

export interface CollaborationRelation {
  id: string;
  kind: CollaborationRelationKind;
  source: string;
  target: string;
  /** Always equals refIds.length. */
  count: number;
  refIds: string[];
  ts: number;
  lastTs: number;
  /** One entry per distinct reference, newest last, capped at RELATION_TS_CAP. */
  timestamps: number[];
  active: boolean;
  /** A non-agent participant that initiated the relation (the operator). */
  initiator?: string;
}

export interface CollaborationActivityItem {
  id: string;
  title: string;
  /** i18n key for `title`; render falls back to `title` when missing. */
  titleKey?: string;
  detail: string;
  ts: number;
  tone: string;
}

export interface AgentCollaborationModel {
  agents: CollaborationAgent[];
  /** Coordinator first, then every worker; never empty. */
  allAgents: CollaborationAgent[];
  coordinator: CollaborationAgent;
  agentById: Map<string, CollaborationAgent>;
  /**
   * Worker ids in roster order. Keeps its identity while the roster and every
   * lifecycle lane field hold, so layouts can depend on it instead of allAgents.
   */
  workers: string[];
  /** Kept by reference while no relation gained a reference or changed activity. */
  relations: CollaborationRelation[];
  knowledge: CollaborationKnowledgeItem[];
  /** Also resolves handoff refIds (`fact:N→intent:X`) to the fact row. */
  knowledgeById: Map<string, CollaborationKnowledgeItem>;
  /** Open intents still dispatchable (dispatchState active or unset). */
  openIntents: BlackboardIntent[];
  /** Open intents parked as `resume` at run end; not counted as unassigned. */
  resumableIntents: BlackboardIntent[];
  /** deck.usd minus every per-solver total (challenge-scoped cost with no owner). */
  unattributedUsd: number;
  currentGeneration: number;
}

/** How recently an agent produced an event, relative to a clock the caller owns. */
export interface CollaborationRecency {
  /** Milliseconds since lastEventAt; unset when the agent never produced an event. */
  sinceMs?: number;
  /** Something landed within RECENT_EVENT_WINDOW_MS. */
  recent: boolean;
  /** Online, in a thinking/waiting state, and silent for IDLE_THRESHOLD_MS or longer. */
  idle: boolean;
}

export const COORDINATOR_ID = "coordinator";
export const DECISION_ID = "decision-model";
export const INPUT_SOURCE_ID = "origin";

/** Presentation entities are never executable worker lanes. */
export function isCollaborationWorker(agent: Pick<CollaborationAgent, "role">): boolean {
  return agent.role === "worker" || agent.role === "review" || agent.role === "verifier";
}
/** Fixed id for the human operator; a participant in relations and knowledge, never an agent card. */
export const OPERATOR_ID = "operator";
/** "Recent progress" highlight window. */
export const RECENT_EVENT_WINDOW_MS = 10_000;
/** Silence threshold; matches the backend tool-stall guard (fruitless_interrupt tool_stall_seconds). */
export const IDLE_THRESHOLD_MS = 120_000;
/** An active relation keeps its edge animated for this long after its newest reference. */
export const RELATION_RECENT_WINDOW_MS = 60_000;
/** The toolbar's "recently active" filter keeps agents with an event inside this window. */
export const ACTIVITY_FILTER_WINDOW_MS = 60_000;
// "preflight" (preflight checks) and "legacy-reconciler" (backfilled flag_found)
// fold into the coordinator card like the other control-plane actors.
const CONTROL_ACTORS = new Set(["reason", "coordinator", "report-value", "dispatch", "preflight", "legacy-reconciler"]);
const OPERATOR_ACTORS = new Set(["operator", "human"]);
const RELATION_TS_CAP = 50;
const SPENT_POC = new Set(["spent", "rejected", "quarantined"]);
const EMPTY_INTENTS: BlackboardIntent[] = [];
const EMPTY_ROWS: CollaborationKnowledgeItem[] = [];

function searchHaystack(...parts: Array<string | undefined>): string {
  return parts.filter(Boolean).join(" ").toLowerCase();
}

export function collaborationAgentId(raw?: string | null): string | undefined {
  const id = String(raw || "").trim();
  if (!id || id === "system") return undefined;
  if (OPERATOR_ACTORS.has(id)) return OPERATOR_ID;
  return CONTROL_ACTORS.has(id) ? COORDINATOR_ID : id;
}

/** `fact:4→intent:x` → `fact:4`; other refIds pass through. */
export function relationRefTarget(refId: string): string {
  const arrow = refId.indexOf("→");
  return arrow >= 0 ? refId.slice(0, arrow) : refId;
}

export function collaborationOpenIntents(deck: DeckState): BlackboardIntent[] {
  return deck.blackboard.intents
    .filter((intent) => intent.status === "open" && !intent.worker && (intent.dispatchState ?? "active") === "active")
    .sort((a, b) => a.proposedTs - b.proposedTs);
}

export function collaborationResumableIntents(deck: DeckState): BlackboardIntent[] {
  return deck.blackboard.intents
    .filter((intent) => intent.status === "open" && !intent.worker && intent.dispatchState === "resume")
    .sort((a, b) => a.proposedTs - b.proposedTs);
}

/** Cost the ledger reported at challenge scope with no solver to own it. */
export function unattributedUsd(deck: DeckState): number {
  const attributed = Object.values(deck.costBySolver).reduce((sum, cost) => sum + (cost.usd || 0), 0);
  return Math.max(0, deck.usd - attributed);
}

function countKnowledge(metrics: CollaborationMetrics, item: CollaborationKnowledgeItem): void {
  switch (item.kind) {
    case "fact": if (item.tone !== "muted") metrics.facts += 1; break;
    case "observation": metrics.observations += 1; break;
    case "candidate": if (item.tone !== "muted") metrics.candidates += 1; break;
    case "dead_end": metrics.deadEnds += 1; break;
    case "route": if (item.status === "suppressed") metrics.deadEnds += 1; break;
    case "poc": if (!SPENT_POC.has(item.status || "")) metrics.pocs += 1; break;
    case "report": break;
    case "review": metrics.reviews += 1; break;
    case "step":
    case "goal":
    case "intent":
    case "finding":
    case "branch":
    case "directive":
    case "flag":
    case "lock":
      break;
    default: {
      const _never: never = item.kind;
      void _never;
      break;
    }
  }
}

function emptyMetrics(): CollaborationMetrics {
  return { facts: 0, observations: 0, candidates: 0, deadEnds: 0, pocs: 0, reviews: 0 };
}

/** The six counters with one exclusion rule set for every agent. */
export function summarizeKnowledge(items: CollaborationKnowledgeItem[]): CollaborationMetrics {
  const metrics = emptyMetrics();
  for (const item of items) countKnowledge(metrics, item);
  return metrics;
}

/** Recency of an agent's newest event against `now` (epoch ms). */
export function recencyOf(
  agent: Pick<CollaborationAgent, "lastEventAt" | "online" | "statusKind">,
  now: number,
): CollaborationRecency {
  const last = toEpochMs(agent.lastEventAt);
  if (!last) return { recent: false, idle: false };
  const sinceMs = Math.max(0, now - last);
  const quiet = agent.statusKind === "thinking" || agent.statusKind === "waiting";
  return {
    sinceMs,
    recent: sinceMs < RECENT_EVENT_WINDOW_MS,
    idle: agent.online && quiet && sinceMs >= IDLE_THRESHOLD_MS,
  };
}

// ---------------------------------------------------------------------------
// Layered derivation. Each layer is a single-slot memo keyed on the identity
// of its inputs; the deck reducer keeps untouched sub-objects by reference, so
// a streaming chat delta only recomputes the stream index and the agent
// assembly, while knowledge rows and relations are reused as they are.
// ---------------------------------------------------------------------------

function layer<A extends unknown[], R>(compute: (...args: A) => R, settle?: (next: R, prev: R) => R): (...args: A) => R {
  let lastArgs: A | undefined;
  let lastResult: R | undefined;
  return (...args: A): R => {
    if (lastArgs && lastArgs.length === args.length && lastArgs.every((value, index) => Object.is(value, args[index]))) {
      return lastResult as R;
    }
    let next = compute(...args);
    if (settle && lastArgs) next = settle(next, lastResult as R);
    lastArgs = args;
    lastResult = next;
    return next;
  };
}

// ---- layer 1: one pass over chat and runtime events, bucketed by solver id ----

interface StreamStats {
  firstTs: number;
  lastTs: number;
  /** Newest tool bubble (the command text). */
  lastTool?: { text: string; ts: number };
  lastRuntime?: { text: string; ts: number };
  /** Spoke as an agent bubble, so it is a worker even without a lane. */
  chatWorker: boolean;
}
type StreamIndex = Map<string, StreamStats>;

const streamIndexOf = layer((chat: ChatMessage[], runtimeEvents: AgentRuntimeFeedEvent[]): StreamIndex => {
  const index: StreamIndex = new Map();
  const touch = (id: string, ts: number): StreamStats => {
    let stats = index.get(id);
    if (!stats) {
      stats = { firstTs: 0, lastTs: 0, chatWorker: false };
      index.set(id, stats);
    }
    if (ts > 0) {
      if (!stats.firstTs || ts < stats.firstTs) stats.firstTs = ts;
      if (ts > stats.lastTs) stats.lastTs = ts;
    }
    return stats;
  };
  for (const message of chat) {
    if (!message.solverId) continue;
    const stats = touch(message.solverId, message.ts);
    if (message.role !== "agent") continue;
    if (isWorkerLane(message.solverId)) stats.chatWorker = true;
    if (message.kind === "tool" && message.content) stats.lastTool = { text: message.content, ts: message.ts };
  }
  for (const event of runtimeEvents) {
    const stats = touch(event.solverId, event.ts);
    stats.lastRuntime = { text: [event.eventType, event.nativeType].filter(Boolean).join(" · "), ts: event.ts };
  }
  return index;
});

// ---- layer 2: roster and the lane fields that only move on lifecycle events ----

interface LaneFacts extends CollaborationAgentIdentity {
  role?: WorkerLaneRole;
  phase?: string;
  spawnPhase?: string;
  spawnedBy?: string;
  raceScout?: boolean;
  firstSeenAt?: number;
  finishedAt?: number;
  runtimeStartedAt?: number;
  runtimeFinishedAt?: number;
  intentId?: string;
  session?: string;
}

const LANE_FACT_KEYS: (keyof LaneFacts)[] = [
  "engine", "profileId", "profileLabel", "model", "accountId", "endpointHost", "connection", "provider",
  "role", "phase", "spawnPhase", "spawnedBy", "raceScout", "firstSeenAt", "finishedAt",
  "runtimeStartedAt", "runtimeFinishedAt", "intentId", "session",
];

function laneFactsOf(lane: SolverLane | undefined): LaneFacts {
  return {
    engine: lane?.engine,
    profileId: lane?.profileId,
    profileLabel: lane?.profileLabel,
    model: lane?.model,
    accountId: lane?.accountId,
    endpointHost: lane?.endpointHost,
    connection: lane?.connection,
    provider: lane?.provider,
    role: lane?.role,
    phase: lane?.phase,
    spawnPhase: lane?.spawnPhase,
    spawnedBy: lane?.spawnedBy,
    raceScout: lane?.raceScout,
    firstSeenAt: lane?.firstSeenAt || undefined,
    finishedAt: lane?.finishedAt || undefined,
    runtimeStartedAt: lane?.runtime?.started_at || undefined,
    runtimeFinishedAt: lane?.runtime?.finished_at || undefined,
    intentId: lane?.intentId,
    session: lane?.session,
  };
}

function sameLaneFacts(a: LaneFacts, b: LaneFacts): boolean {
  return LANE_FACT_KEYS.every((key) => Object.is(a[key], b[key]));
}

interface Roster {
  workers: string[];
  workerSet: Set<string>;
  currentIds: Set<string>;
  currentGeneration: number;
  /** lowercase lane.engine → worker ids sharing it */
  engineIndex: Map<string, string[]>;
  laneFacts: Map<string, LaneFacts>;
}

const hasGenerationSuffix = (id: string) => /-g\d+$/.test(id);

const rosterOf = layer((
  lanes: Record<string, SolverLane>,
  stream: StreamIndex,
  boardWorkers: string[],
  executionGeneration: number,
): Roster => {
  // Same union as selectors.workerIds (lanes, then agent chat), plus board actors.
  const base = new Set<string>();
  for (const lane of Object.values(lanes)) if (isWorkerLane(lane.solverId)) base.add(lane.solverId);
  for (const [id, stats] of stream) if (stats.chatWorker && isWorkerLane(id)) base.add(id);
  const ghost = (id: string) => id === "solver" && !lanes[id]?.firstSeenAt && !boardWorkers.includes(id);
  const ids = new Set<string>(base);
  for (const id of boardWorkers) if (isWorkerLane(id)) ids.add(id);
  const workers = Array.from(ids).filter((id) => id !== INPUT_SOURCE_ID && !ghost(id));

  // currentGenWorkerIds semantics on the base set, then the wider roster rules.
  const baseIds = Array.from(base).filter((id) => !ghost(id));
  const baseSuffixed = baseIds.some(hasGenerationSuffix);
  const baseGeneration = executionGeneration > 0
    ? executionGeneration
    : baseIds.reduce((max, id) => Math.max(max, workerGeneration(id)), 1);
  const selectorCurrent = new Set(baseSuffixed ? baseIds.filter((id) => workerGeneration(id) === baseGeneration) : baseIds);
  const suffixed = workers.some(hasGenerationSuffix);
  const currentGeneration = executionGeneration > 0
    ? executionGeneration
    : workers.reduce((max, id) => Math.max(max, workerGeneration(id)), 1);
  const currentIds = new Set(workers.filter((id) =>
    selectorCurrent.has(id) || (suffixed && workerGeneration(id) === currentGeneration) || !suffixed));

  const laneFacts = new Map<string, LaneFacts>();
  const engineIndex = new Map<string, string[]>();
  for (const id of workers) {
    const facts = laneFactsOf(lanes[id]);
    laneFacts.set(id, facts);
    const engine = String(facts.engine || "").trim().toLowerCase();
    if (!engine) continue;
    const rows = engineIndex.get(engine) || [];
    rows.push(id);
    engineIndex.set(engine, rows);
  }
  return { workers, workerSet: new Set(workers), currentIds, currentGeneration, engineIndex, laneFacts };
}, (next, prev) => {
  if (next.currentGeneration !== prev.currentGeneration) return next;
  if (next.workers.length !== prev.workers.length || next.currentIds.size !== prev.currentIds.size) return next;
  for (let i = 0; i < next.workers.length; i += 1) {
    const id = next.workers[i];
    if (id !== prev.workers[i] || next.currentIds.has(id) !== prev.currentIds.has(id)) return next;
    if (!sameLaneFacts(next.laneFacts.get(id)!, prev.laneFacts.get(id)!)) return next;
  }
  return prev;
});

function rosterOfDeck(deck: DeckState): Roster {
  return rosterOf(deck.lanes, streamIndexOf(deck.chat, deck.runtimeEvents), deck.blackboard.workers, deck.executionGeneration);
}

/** Worker ids that become agent cards: lanes, chat-derived ids and blackboard actors, minus ghosts. */
export function collaborationWorkerIds(deck: DeckState): string[] {
  return rosterOfDeck(deck).workers;
}

/** Worker ids of the current execution generation; the same test every card's isCurrent uses. */
export function collaborationCurrentIds(deck: DeckState): Set<string> {
  return rosterOfDeck(deck).currentIds;
}

/** Coordinator plus current-generation workers; the runtime tab badge uses this. */
export function collaborationAgentCount(deck: DeckState): number {
  return 1 + rosterOfDeck(deck).currentIds.size;
}

function pendingHitlFor(deck: DeckState, workerId: string): number {
  let n = 0;
  for (const request of deck.hitlRequests) {
    if (request.worker === workerId && hitlDeliveryState(request).phase === "open") n += 1;
  }
  return n;
}

function anomalyReasonKey(reason?: string): string | undefined {
  switch (reason) {
    case "oom":
      return "worker.oom";
    case "timeout":
      return "worker.timeout";
    case "error":
      return "worker.error";
    case "budget":
      return "worker.budget";
    default:
      return undefined;
  }
}

/** Agents whose cards count as issues: error/paused/stalled workers, open HITL, coordinator preflight or HITL pause. */
export function collaborationAnomalyCount(deck: DeckState): number {
  const roster = rosterOfDeck(deck);
  let n = 0;
  for (const id of roster.workers) {
    const lane = deck.lanes[id];
    const online = lane?.online ?? !deck.finished;
    const kind = laneStatusKind({
      solved: lane?.solved,
      status: lane?.status,
      statusReason: lane?.statusReason,
      paused: lane?.paused,
    }, online);
    if (kind === "paused" || kind === "stalled" || kind === "error" || pendingHitlFor(deck, id) > 0) n += 1;
  }
  if (!deck.finished && (deck.preflightFailures.length > 0 || deck.awaitingOperator)) n += 1;
  return n;
}

// ---- layer 3: knowledge rows, bucketed and counted per agent in one pass ----

function intentTitle(intent: BlackboardIntent): string {
  return (intent.summary || "").trim() || intent.goal || intent.id;
}

function factTitle(fact: BlackboardFact): string {
  return (fact.summary || "").trim() || fact.fact || `#${fact.factSeq ?? "?"}`;
}

function isControlActor(actor: string): boolean {
  return !actor || CONTROL_ACTORS.has(actor);
}

/**
 * Map a backend verifier string to one worker: a solver id directly, or a
 * legacy engine name when exactly one worker runs that engine.
 */
function resolveVerifierAgent(verifier: string | undefined, roster: Roster): string | undefined {
  const raw = String(verifier || "").trim();
  if (!raw) return undefined;
  const direct = collaborationAgentId(raw);
  if (direct && (roster.workerSet.has(direct) || direct === COORDINATOR_ID)) return direct;
  const matches = roster.engineIndex.get(raw.toLowerCase()) || [];
  return matches.length === 1 ? matches[0] : undefined;
}

function laneWindowContains(facts: LaneFacts | undefined, ts: number): boolean {
  if (!facts?.firstSeenAt) return false;
  return facts.firstSeenAt <= ts && ts <= (facts.finishedAt ?? Number.MAX_SAFE_INTEGER);
}

/** The review worker for a finding: the payload's worker, else the one review lane active at finding.ts. */
function reviewerFor(roster: Roster, finding: BlackboardReviewFinding): string | undefined {
  const explicit = collaborationAgentId(finding.worker);
  if (explicit && roster.workerSet.has(explicit)) return explicit;
  let found: string | undefined;
  let matches = 0;
  for (const id of roster.workers) {
    const facts = roster.laneFacts.get(id);
    const isReview = facts?.role === "review" || String(facts?.phase || "").includes("review");
    if (isReview && laneWindowContains(facts, finding.ts)) {
      matches += 1;
      found = id;
    }
  }
  return matches === 1 ? found : undefined;
}

function rowStatus(
  kind: CollaborationKnowledgeKind,
  status?: string,
): Pick<CollaborationKnowledgeItem, "status" | "statusKey"> {
  return { status, statusKey: knowledgeStatusKey(kind, status) };
}

function knowledgeRows(
  bb: BlackboardView,
  roster: Roster,
  intentById: Map<string, BlackboardIntent>,
  operatorDirectives: OperatorDirective[],
  lockRequests: LockRequest[],
  startedAt: number | undefined,
  finishedAt: number | undefined,
): CollaborationKnowledgeItem[] {
  const rows: Array<Omit<CollaborationKnowledgeItem, "searchText" | "appearedTs"> & { appearedTs?: number }> = [];

  for (const intent of bb.intents) {
    rows.push({
      id: `intent:${intent.id}`,
      kind: "intent",
      title: intentTitle(intent),
      detail: intent.goal,
      agentId: collaborationAgentId(intent.worker) || (intent.status === "open" ? COORDINATOR_ID : undefined),
      proposerId: collaborationAgentId(intent.proposedBy) || COORDINATOR_ID,
      ts: intent.concludedTs ?? intent.claimedTs ?? intent.proposedTs,
      appearedTs: intent.proposedTs,
      claimedTs: intent.claimedTs,
      concludedTs: intent.concludedTs,
      tone: intent.dispatchState === "retired" ? "muted" : intent.status === "claimed" ? "accent" : intent.status === "done" ? "success" : "neutral",
      ...rowStatus("intent", intent.dispatchState || intent.status),
      intentId: intent.id,
    });
  }

  for (const fact of bb.facts) {
    const retired = isFactRetired(fact);
    const producer = collaborationAgentId(fact.actor);
    const verifierAgentId = resolveVerifierAgent(fact.verifier, roster);
    const selfReported = !!producer && (verifierAgentId === producer || collaborationAgentId(fact.verifier) === producer);
    rows.push({
      id: `fact:${fact.factSeq ?? `${fact.actor}:${fact.ts}`}`,
      kind: fact.verified ? "fact" : "candidate",
      title: factTitle(fact),
      detail: fact.fact,
      agentId: producer,
      ts: fact.promotedTs ?? fact.ts,
      appearedTs: fact.ts,
      promotedTs: fact.promotedTs,
      tone: retired ? "muted" : fact.challenged ? "warning" : fact.verified ? "success" : "warning",
      ...rowStatus(fact.verified ? "fact" : "candidate", fact.state || (fact.verified ? "verified" : "candidate")),
      intentId: fact.intentId,
      factSeq: fact.factSeq,
      artifactId: fact.artifactId,
      witness: fact.witness,
      confidence: fact.confidence,
      verifier: selfReported ? undefined : (String(fact.verifier || "").trim() || undefined),
      verifierAgentId: selfReported ? undefined : verifierAgentId,
    });
  }

  for (const observation of bb.observations) {
    if (observation.admitted) continue;
    rows.push({
      id: `observation:${observation.observationSeq}`,
      kind: "observation",
      title: observation.text,
      detail: observation.text,
      agentId: collaborationAgentId(observation.actor),
      ts: observation.ts,
      tone: "warning",
      ...rowStatus("observation", "retained"),
      intentId: observation.intentId,
      artifactId: observation.artifactId,
      witness: observation.witness,
      confidence: observation.confidence,
    });
  }

  bb.deadEnds.forEach((dead, index) => rows.push({
    id: `dead:${dead.deadEndSeq ?? `${dead.actor}:${dead.ts}:${index}`}`,
    kind: "dead_end",
    title: dead.reason,
    detail: [dead.reason, dead.testedScope, dead.observedResult].filter(Boolean).join("\n"),
    agentId: collaborationAgentId(dead.actor),
    ts: dead.ts,
    tone: "danger",
    ...rowStatus("dead_end", "dead_end"),
    intentId: dead.intentId,
  }));

  for (const poc of bb.pocs) rows.push({
    id: `poc:${poc.id}`,
    kind: "poc",
    title: poc.name || poc.id,
    detail: poc.note || poc.entryCommand,
    agentId: collaborationAgentId(poc.worker) || collaborationAgentId(poc.intentId ? intentById.get(poc.intentId)?.worker : undefined),
    ts: poc.concludedTs ?? poc.claimedTs ?? poc.savedTs,
    appearedTs: poc.savedTs,
    claimedTs: poc.claimedTs,
    concludedTs: poc.concludedTs,
    tone: poc.status === "rejected" || poc.status === "quarantined" ? "danger" : poc.status === "spent" ? "muted" : "accent",
    ...rowStatus("poc", poc.status),
    intentId: poc.intentId,
    artifactId: poc.artifactId,
  });

  for (const finding of bb.reviewFindings) rows.push({
    id: `review:${finding.id}`,
    kind: "review",
    title: finding.summary || finding.kind,
    detail: finding.recommendedActions?.join("\n") || finding.summary,
    agentId: reviewerFor(roster, finding) || collaborationAgentId(finding.actor),
    ts: finding.ts,
    tone: finding.severity === "blocker" || finding.severity === "high" || finding.severity === "critical" ? "danger" : finding.severity === "warn" || finding.severity === "medium" ? "warning" : "neutral",
    ...rowStatus("review", finding.severity || finding.kind),
    intentId: finding.intentIds?.[0],
    factSeq: finding.evidenceSeqs?.[0],
  });

  for (const finding of bb.gatedFindings) rows.push({
    id: `finding:${finding.id}`,
    kind: "finding",
    title: finding.title || finding.findingClass || finding.resourceId,
    detail: [finding.source, finding.resourceId].filter(Boolean).join(" · "),
    agentId: collaborationAgentId(finding.actor),
    ts: finding.ts,
    tone: "accent",
    ...rowStatus("finding", finding.findingClass),
  });

  for (const route of bb.suppressedRoutes) rows.push({
    id: `route:${route.routeHash}`,
    kind: "route",
    title: route.label || route.routeHash,
    detail: route.reason,
    agentId: collaborationAgentId(route.actor),
    ts: route.ts,
    tone: route.reopened ? "success" : "danger",
    ...rowStatus("route", route.reopened ? "reopened" : "suppressed"),
  });

  for (const branch of bb.branches) rows.push({
    id: `branch:${branch.branchId}`,
    kind: "branch",
    title: branch.title || branch.branchId,
    detail: branch.title,
    agentId: collaborationAgentId(branch.actor),
    ts: branch.ts,
    tone: branch.status === "resolved" ? "success" : "neutral",
    ...rowStatus("branch", branch.status || "open"),
  });

  bb.directives.forEach((directive, index) => rows.push({
    id: `directive:${directive.ts}:${index}`,
    kind: "directive",
    title: directive.directive,
    detail: directive.directive,
    agentId: collaborationAgentId(directive.actor) || COORDINATOR_ID,
    ts: directive.ts,
    tone: "accent",
    ...rowStatus("directive", directive.action),
  }));

  for (const directive of operatorDirectives) rows.push({
    id: `operator:${directive.id}`,
    kind: "directive",
    title: directive.text,
    detail: directive.text,
    agentId: collaborationAgentId(directive.boundWorker) || COORDINATOR_ID,
    proposerId: OPERATOR_ID,
    ts: directive.ts,
    tone: directive.status === "rejected" ? "danger" : directive.status === "acted" ? "success" : "accent",
    ...rowStatus("directive", directive.status),
  });

  for (const request of lockRequests) rows.push({
    id: `lock:${request.lockId}:${request.ts}`,
    kind: "lock",
    title: request.resourceKey,
    detail: `${request.requester} → ${request.holder}${request.riskClass ? ` · ${request.riskClass}` : ""}`,
    agentId: collaborationAgentId(request.requester),
    ts: request.ts,
    tone: "warning",
    ...rowStatus("lock", "denied"),
  });

  const flags = bb.flags?.length ? bb.flags : bb.flag ? [bb.flag] : [];
  flags.forEach((flag, index) => {
    const origin = bb.flagOrigins?.[flag];
    rows.push({
      id: `flag:${index}:${flag}`,
      kind: "flag",
      title: flag,
      detail: flag,
      agentId: collaborationAgentId(origin?.actor) || COORDINATOR_ID,
      ts: origin?.ts ?? finishedAt ?? startedAt ?? 0,
      tone: "success",
      ...rowStatus("flag", "found"),
      intentId: origin?.intentId,
    });
  });

  return rows
    .sort((a, b) => b.ts - a.ts || a.id.localeCompare(b.id))
    .map((row) => ({
      ...row,
      appearedTs: row.appearedTs ?? row.ts,
      searchText: searchHaystack(row.title, row.detail, row.status, row.kind, row.agentId, row.intentId),
    }));
}

function projectCtfKnowledgeRows(
  rows: CollaborationKnowledgeItem[],
  deck: Pick<DeckState, "taskContract" | "expectedFlags" | "solved" | "startedAt" | "finishedAt" | "reason">,
): CollaborationKnowledgeItem[] {
  const projected: CollaborationKnowledgeItem[] = [];
  for (const row of rows) {
    switch (row.kind) {
      case "intent":
      case "step":
        projected.push({ ...row, kind: "step", searchText: searchHaystack(row.title, row.detail, row.status, "step", row.agentId, row.intentId) });
        break;
      case "fact":
        projected.push({
          ...row,
          kind: "fact",
          status: row.status === "verified" ? "recorded" : row.status,
          statusKey: row.status === "verified" ? "collab.status.recorded" : row.statusKey,
          searchText: searchHaystack(row.title, row.detail, "fact", row.agentId),
        });
        break;
      case "observation":
      case "dead_end":
      case "poc":
        projected.push(row);
        break;
      case "candidate":
        // Legacy unadmitted Fact events are not CTF Facts. The graph migrates
        // them to Observations, so never relabel one as a verified claim.
        break;
      case "flag":
      case "goal":
        projected.push(row);
        break;
      case "finding":
      case "report":
      case "review":
      case "route":
      case "branch":
      case "lock":
      case "directive":
        break;
      default: {
        const _never: never = row.kind;
        void _never;
        break;
      }
    }
  }
  if (!projected.some((row) => row.kind === "goal")) {
    const title = (deck.taskContract?.completion.goal || "").trim() || "提交全部 FLAG";
    const expected = Math.max(1, deck.taskContract?.completion.expectedFlags || deck.expectedFlags || 1);
    const satisfied = !!deck.solved || !!deck.reason.goalMet;
    const ts = deck.finishedAt || deck.startedAt || 0;
    projected.push({
      id: "goal:final",
      kind: "goal",
      title,
      detail: `expected ${expected}`,
      ts,
      appearedTs: deck.startedAt || ts,
      tone: satisfied ? "success" : "neutral",
      status: satisfied ? "satisfied" : "open",
      statusKey: satisfied ? "collab.status.satisfied" : "collab.status.open",
      searchText: searchHaystack(title, "goal", String(expected)),
    });
  }
  return projected;
}

interface AgentKnowledge {
  rows: CollaborationKnowledgeItem[];
  metrics: CollaborationMetrics;
  /** Oldest row at or after the run start (0 when none); newest row overall. */
  minTs: number;
  maxTs: number;
}

interface AgentIntents {
  /** Oldest claim first. */
  chronological: BlackboardIntent[];
  newestFirst: BlackboardIntent[];
}

interface KnowledgeLayer {
  rows: CollaborationKnowledgeItem[];
  byId: Map<string, CollaborationKnowledgeItem>;
  byAgent: Map<string, AgentKnowledge>;
  coordinator: AgentKnowledge;
  intentsByAgent: Map<string, AgentIntents>;
  intentById: Map<string, BlackboardIntent>;
  factsBySeq: Map<number, BlackboardFact>;
  lastBoardEventByActor: Map<string, number>;
  lastControlEvent?: BlackboardEvent;
  coordination: Omit<CoordinatorMetrics, "flags" | "onlineWorkers" | "totalWorkers">;
}

function isOpenClaim(intent: BlackboardIntent): boolean {
  return intent.status === "claimed" && intent.dispatchState !== "retired" && intent.dispatchState !== "closed";
}

function intentOrder(intent: BlackboardIntent): number {
  return intent.concludedTs ?? intent.claimedTs ?? intent.proposedTs;
}

const knowledgeLayerOf = layer((
  roster: Roster,
  bb: BlackboardView,
  operatorDirectives: OperatorDirective[],
  lockRequests: LockRequest[],
  startedAt: number | undefined,
  finishedAt: number | undefined,
  mode: DeckState["mode"],
  taskContract: DeckState["taskContract"],
  expectedFlags: number,
  flags: string[],
  solved: boolean,
  goalMet: boolean,
): KnowledgeLayer => {
  const intentById = new Map<string, BlackboardIntent>();
  const intentsByAgent = new Map<string, AgentIntents>();
  const coordination = {
    proposedIntents: 0, assignedIntents: 0, completedIntents: 0, retiredIntents: 0,
    reasonRounds: bb.reasonRuns?.length ?? 0, directives: 0, verifiedFacts: 0,
  };
  for (const intent of bb.intents) {
    intentById.set(intent.id, intent);
    const owner = collaborationAgentId(intent.worker);
    if (owner) {
      const bucket = intentsByAgent.get(owner) || { chronological: [], newestFirst: [] };
      bucket.chronological.push(intent);
      intentsByAgent.set(owner, bucket);
    }
    if (!isControlActor(intent.proposedBy || "")) continue;
    coordination.proposedIntents += 1;
    if (intent.worker) coordination.assignedIntents += 1;
    if (intent.status === "done") coordination.completedIntents += 1;
    else if (intent.dispatchState === "retired" || intent.dispatchState === "closed") coordination.retiredIntents += 1;
  }
  for (const bucket of intentsByAgent.values()) {
    bucket.chronological.sort((a, b) => (a.claimedTs ?? a.proposedTs) - (b.claimedTs ?? b.proposedTs));
    bucket.newestFirst = [...bucket.chronological].sort((a, b) => intentOrder(b) - intentOrder(a));
  }

  const factsBySeq = new Map<number, BlackboardFact>();
  for (const fact of bb.facts) if (fact.factSeq) factsBySeq.set(fact.factSeq, fact);

  const rawRows = knowledgeRows(bb, roster, intentById, operatorDirectives, lockRequests, startedAt, finishedAt);
  const rows = canvasModeOf({ mode }) === "ctf"
    ? projectCtfKnowledgeRows(rawRows, {
        taskContract,
        expectedFlags,
        solved,
        startedAt,
        finishedAt,
        reason: { goalMet, intents: [], audit: [] },
      })
    : rawRows;
  const byId = new Map<string, CollaborationKnowledgeItem>();
  const byAgent = new Map<string, AgentKnowledge>();
  const floor = startedAt || 0;
  const bucketFor = (id: string): AgentKnowledge => {
    let bucket = byAgent.get(id);
    if (!bucket) {
      bucket = { rows: [], metrics: emptyMetrics(), minTs: 0, maxTs: 0 };
      byAgent.set(id, bucket);
    }
    return bucket;
  };
  const collect = (bucket: AgentKnowledge, row: CollaborationKnowledgeItem) => {
    bucket.rows.push(row);
    countKnowledge(bucket.metrics, row);
    if (row.ts > bucket.maxTs) bucket.maxTs = row.ts;
    if (row.ts > 0 && row.ts >= floor && (!bucket.minTs || row.ts < bucket.minTs)) bucket.minTs = row.ts;
  };
  for (const row of rows) {
    byId.set(row.id, row);
    if (row.kind === "fact" && row.tone !== "muted" && row.status !== "scoped_negative") coordination.verifiedFacts += 1;
    const owner = row.agentId || COORDINATOR_ID;
    collect(bucketFor(owner), row);
    // Intents the coordinator proposed stay on its card after a worker claims them.
    if ((row.kind === "intent" || row.kind === "step") && row.proposerId === COORDINATOR_ID && owner !== COORDINATOR_ID) {
      collect(bucketFor(COORDINATOR_ID), row);
    }
    // Operator-issued board directives are relayed by the coordinator; list them on its card too.
    if (row.kind === "directive" && row.agentId === OPERATOR_ID) collect(bucketFor(COORDINATOR_ID), row);
  }
  const coordinator = byAgent.get(COORDINATOR_ID) || { rows: EMPTY_ROWS, metrics: emptyMetrics(), minTs: 0, maxTs: 0 };
  // Directive count is the number of directives issued, whoever issued them.
  coordination.directives = bb.directives.length + operatorDirectives.length;

  const lastBoardEventByActor = new Map<string, number>();
  let lastControlEvent: BlackboardEvent | undefined;
  for (const event of bb.events) {
    if (event.ts > (lastBoardEventByActor.get(event.actor) || 0)) lastBoardEventByActor.set(event.actor, event.ts);
    if (isControlActor(event.actor)) lastControlEvent = event;
  }

  return { rows, byId, byAgent, coordinator, intentsByAgent, intentById, factsBySeq, lastBoardEventByActor, lastControlEvent, coordination };
});

// ---- layer 4: relations between agents, independent of live status ----

interface RelationLayer {
  /** Every relation with `active` unset (false); ordered by first ts, then id. */
  relations: CollaborationRelation[];
  /** Knowledge rows plus handoff aliases (`fact:N→intent:X` → the fact row). */
  knowledgeById: Map<string, CollaborationKnowledgeItem>;
}

function relationKey(kind: CollaborationRelationKind, source: string, target: string): string {
  return `${kind}:${source}:${target}`;
}

const relationLayerOf = layer((
  roster: Roster,
  knowledge: KnowledgeLayer,
  bb: BlackboardView,
  operatorDirectives: OperatorDirective[],
  lockRequests: LockRequest[],
  startedAt: number | undefined,
): RelationLayer => {
  const allAgentIds = new Set<string>([COORDINATOR_ID, INPUT_SOURCE_ID, ...roster.workers]);
  const relationMap = new Map<string, CollaborationRelation>();

  const addRelation = (
    kind: CollaborationRelationKind,
    sourceRaw: string | undefined,
    targetRaw: string | undefined,
    refId: string,
    ts: number,
    initiator?: string,
  ) => {
    const source = collaborationAgentId(sourceRaw);
    const target = collaborationAgentId(targetRaw);
    if (!source || !target || source === target || !allAgentIds.has(source) || !allAgentIds.has(target)) return;
    const key = relationKey(kind, source, target);
    const prior = relationMap.get(key);
    if (prior) {
      if (!prior.refIds.includes(refId)) {
        prior.refIds.push(refId);
        prior.timestamps = [...prior.timestamps, ts].slice(-RELATION_TS_CAP);
      }
      prior.count = prior.refIds.length;
      prior.ts = Math.max(prior.ts, ts);
      prior.lastTs = Math.max(prior.lastTs, ts);
      if (initiator && !prior.initiator) prior.initiator = initiator;
      return;
    }
    relationMap.set(key, {
      id: key, kind, source, target, count: 1, refIds: [refId], ts, lastTs: ts, timestamps: [ts], active: false, initiator,
    });
  };

  // dispatch: one reference per intent the worker claimed; spawn-only workers keep the agent reference.
  for (const id of roster.workers) {
    const facts = roster.laneFacts.get(id)!;
    const spawner = collaborationAgentId(facts.spawnedBy);
    const initiator = spawner === OPERATOR_ID ? OPERATOR_ID : undefined;
    const source = spawner && allAgentIds.has(spawner) ? spawner : COORDINATOR_ID;
    const intents = knowledge.intentsByAgent.get(id)?.chronological || EMPTY_INTENTS;
    if (intents.length) {
      for (const intent of intents) {
        addRelation("dispatch", source, id, `intent:${intent.id}`, intent.claimedTs ?? intent.proposedTs, initiator);
      }
    } else {
      addRelation("dispatch", source, id, `agent:${id}`, facts.firstSeenAt || facts.runtimeStartedAt || startedAt || 0, initiator);
    }
  }

  // handoff: producer → claimant, one reference per (fact, intent) pair.
  // Intents bulk-closed by the coordinator carry worker="coordinator"; only
  // roster workers count as claimants.
  for (const intent of bb.intents) {
    if (!intent.worker) continue;
    const claimant = collaborationAgentId(intent.worker);
    if (!claimant || !roster.workerSet.has(claimant)) continue;
    for (const seq of intent.fromFacts || []) {
      const fact = knowledge.factsBySeq.get(seq);
      if (fact) addRelation("handoff", fact.actor, claimant, `fact:${seq}→intent:${intent.id}`, intent.claimedTs ?? intent.proposedTs);
    }
  }

  // review: reviewer → the worker whose facts / intents the finding covers.
  for (const finding of bb.reviewFindings) {
    const reviewer = reviewerFor(roster, finding);
    if (!reviewer) continue;
    const targets = new Set<string>();
    for (const seq of finding.evidenceSeqs || []) {
      const target = collaborationAgentId(knowledge.factsBySeq.get(seq)?.actor);
      if (target) targets.add(target);
    }
    for (const intentId of finding.intentIds || []) {
      const target = collaborationAgentId(knowledge.intentById.get(intentId)?.worker);
      if (target) targets.add(target);
    }
    for (const target of targets) addRelation("review", reviewer, target, `review:${finding.id}`, finding.ts);
  }

  // verify: verifier → producer.
  for (const fact of bb.facts) {
    const verifier = resolveVerifierAgent(fact.verifier, roster);
    if (verifier) addRelation("verify", verifier, fact.actor, `fact:${fact.factSeq ?? fact.ts}`, fact.promotedTs ?? fact.ts);
  }
  // report: worker → coordinator, one reference per conclusion the worker wrote to the board.
  for (const id of roster.workers) {
    for (const item of knowledge.byAgent.get(id)?.rows || EMPTY_ROWS) {
      const conclusion = item.kind === "fact" || item.kind === "candidate" || item.kind === "observation" || item.kind === "dead_end" || item.kind === "flag"
        || (item.kind === "intent" && item.status !== "active" && item.status !== "claimed" && item.status !== "open");
      if (conclusion) addRelation("report", id, COORDINATOR_ID, item.id, item.ts);
    }
  }

  // directive: coordinator → worker; operator directives bind directly, board directives route by route_hash.
  for (const directive of operatorDirectives) {
    if (!directive.boundWorker) continue;
    addRelation("directive", COORDINATOR_ID, directive.boundWorker, `operator:${directive.id}`, directive.ts, OPERATOR_ID);
  }
  bb.directives.forEach((directive, index) => {
    if (!directive.routeHash) return;
    const initiator = collaborationAgentId(directive.actor) === OPERATOR_ID ? OPERATOR_ID : undefined;
    const targets = new Set<string>();
    for (const intent of bb.intents) {
      if (intent.routeHash === directive.routeHash && intent.worker) targets.add(intent.worker);
    }
    for (const target of targets) addRelation("directive", COORDINATOR_ID, target, `directive:${directive.ts}:${index}`, directive.ts, initiator);
  });

  // lock: requester → holder for every denied request.
  for (const request of lockRequests) {
    addRelation("lock", request.requester, request.holder, `lock:${request.lockId}:${request.ts}`, request.ts);
  }

  const relations = Array.from(relationMap.values()).sort((a, b) => a.ts - b.ts || a.id.localeCompare(b.id));
  const knowledgeById = new Map(knowledge.byId);
  for (const relation of relations) {
    for (const refId of relation.refIds) {
      if (knowledgeById.has(refId)) continue;
      const base = knowledgeById.get(relationRefTarget(refId));
      if (base) knowledgeById.set(refId, base);
    }
  }
  return { relations, knowledgeById };
});

/**
 * Attach `active` and drop relations outside the visible set. A relation whose
 * base row and activity did not change keeps its object; when every row is
 * kept the previous array is returned as well.
 */
const relationCache = new Map<string, { base: CollaborationRelation; item: CollaborationRelation }>();
let relationCacheBases: CollaborationRelation[] | undefined;
let lastRelations: CollaborationRelation[] = [];

function finalizeRelations(
  bases: CollaborationRelation[],
  visibleIds: Set<string>,
  isOnline: (id: string) => boolean,
): CollaborationRelation[] {
  if (relationCacheBases !== bases) {
    relationCache.clear();
    relationCacheBases = bases;
  }
  const next: CollaborationRelation[] = [];
  for (const base of bases) {
    if (!visibleIds.has(base.source) || !visibleIds.has(base.target)) continue;
    const active = isOnline(base.target);
    const hit = relationCache.get(base.id);
    if (hit && hit.base === base && hit.item.active === active) {
      next.push(hit.item);
      continue;
    }
    const item = { ...base, active };
    relationCache.set(base.id, { base, item });
    next.push(item);
  }
  if (next.length === lastRelations.length && next.every((item, index) => item === lastRelations[index])) return lastRelations;
  lastRelations = next;
  return next;
}

// ---- layer 5: agent assembly (runs on every model build) ----

interface Lifecycle {
  startedAt?: number;
  endedAt?: number;
  source: CollaborationLifecycleSource;
}

/**
 * Start: the lane's own firstSeenAt, else the runtime process clock, else the
 * oldest attributed activity at or after the run start, else the run bounds.
 * End: the lane's finishedAt; an offline worker without one ends at the later
 * of the run end (once the run finished) and its newest event, so a start
 * inferred from post-run chat never lands after the end.
 */
function lifecycleOf(deck: DeckState, facts: LaneFacts, online: boolean, inferred: number | undefined, lastEventAt: number | undefined): Lifecycle {
  const runEnd = deck.finished ? deck.finishedAt : undefined;
  const endedOffline = online ? undefined : (Math.max(runEnd || 0, lastEventAt || 0) || undefined);
  if (facts.firstSeenAt) {
    return { startedAt: facts.firstSeenAt, endedAt: facts.finishedAt ?? facts.runtimeFinishedAt ?? endedOffline, source: "lane" };
  }
  if (facts.runtimeStartedAt) {
    return { startedAt: facts.runtimeStartedAt, endedAt: facts.runtimeFinishedAt ?? facts.finishedAt ?? endedOffline, source: "runtime" };
  }
  if (inferred) return { startedAt: inferred, endedAt: facts.finishedAt ?? endedOffline, source: "inferred" };
  return { startedAt: deck.startedAt, endedAt: deck.finished ? deck.finishedAt : undefined, source: "run" };
}

interface ActivitySnapshot {
  text: string;
  ts?: number;
}

/**
 * The card's activity line: the roster's own detail rule first (tool line
 * instead of a status word), then the newest tool bubble, runtime event,
 * current intent title, or the last intent's close reason.
 */
function workerActivity(
  lane: SolverLane | undefined,
  presentation: WorkerLanePresentationInput,
  online: boolean,
  stream: StreamStats | undefined,
  lastEventAt: number | undefined,
  currentIntent?: BlackboardIntent,
  lastIntent?: BlackboardIntent,
): ActivitySnapshot {
  const fromLane = lane ? laneActivityDetail(presentation, online, lane.toolLines) : "";
  if (fromLane) return { text: fromLane, ts: lastEventAt };
  if (stream?.lastTool) return { text: stream.lastTool.text, ts: stream.lastTool.ts };
  if (stream?.lastRuntime) return { text: stream.lastRuntime.text, ts: stream.lastRuntime.ts };
  if (currentIntent) return { text: intentTitle(currentIntent), ts: currentIntent.claimedTs ?? currentIntent.proposedTs };
  if (lastIntent?.closeReason) return { text: lastIntent.closeReason, ts: lastIntent.concludedTs ?? lastIntent.claimedTs };
  return { text: "", ts: lastEventAt };
}

function agentRole(facts: LaneFacts): CollaborationAgentRole {
  if (facts.role === "review" || String(facts.phase || "").includes("review")) return "review";
  if (facts.role === "verifier" || String(facts.phase || "").includes("verifier")) return "verifier";
  return "worker";
}

function agentIdentity(facts: LaneFacts): CollaborationAgentIdentity {
  return {
    engine: facts.engine,
    profileId: facts.profileId,
    profileLabel: facts.profileLabel,
    model: facts.model,
    accountId: facts.accountId,
    endpointHost: facts.endpointHost,
    connection: facts.connection,
    provider: facts.provider,
  };
}

function agentPresentation(lane?: SolverLane): WorkerLanePresentationInput {
  return {
    solved: lane?.solved,
    status: lane?.status,
    statusReason: lane?.statusReason,
    paused: lane?.paused,
  };
}

/** Walk `source` newest-first and keep at most `limit` matching rows. */
function takeNewest<T>(
  source: readonly T[],
  limit: number,
  match: (item: T) => boolean,
  toRow: (item: T) => CollaborationActivityItem,
  into: CollaborationActivityItem[],
) {
  let kept = 0;
  for (let i = source.length - 1; i >= 0 && kept < limit; i -= 1) {
    const item = source[i];
    if (!match(item)) continue;
    into.push(toRow(item));
    kept += 1;
  }
}

/** Newest-first activity rows for one agent, capped at `limit`. `asOf` is epoch ms. */
export function activityForAgent(deck: DeckState, agent: CollaborationAgent, limit: number, asOf?: number): CollaborationActivityItem[] {
  const within = (ts: number) => asOf === undefined || toEpochMs(ts) <= asOf;
  const rows: CollaborationActivityItem[] = [];
  if (agent.id === COORDINATOR_ID) {
    takeNewest(deck.blackboard.events, limit, (event) => within(event.ts), (event) => ({
      id: event.id,
      title: event.kind,
      titleKey: activityTitleKey("event", event.kind),
      detail: event.label,
      ts: event.ts,
      tone: "control",
    }), rows);
    takeNewest(deck.controlCommands, limit, (command) => within(command.ts), (command) => ({
      id: `control:${command.id}`,
      title: command.action,
      titleKey: activityTitleKey("action", command.action),
      detail: command.detail || command.status,
      ts: command.ts,
      tone: command.status,
    }), rows);
  } else {
    takeNewest(deck.runtimeEvents, limit, (event) => event.solverId === agent.id && within(event.ts), (event) => ({
      id: event.id,
      title: event.nativeType || event.eventType,
      titleKey: activityTitleKey("runtime", event.eventType),
      detail: String(event.payload.message || event.payload.text || event.payload.command || event.nativeType || ""),
      ts: event.ts,
      tone: "runtime",
    }), rows);
    takeNewest(deck.chat, limit, (message) => message.solverId === agent.id && within(message.ts), (message) => ({
      id: `chat:${message.id}`,
      title: message.kind,
      titleKey: activityTitleKey("message", message.kind),
      detail: message.content || message.toolOutput || "",
      ts: message.ts,
      tone: message.toolFailed ? "failed" : message.kind,
    }), rows);
  }
  return rows.sort((a, b) => b.ts - a.ts || a.id.localeCompare(b.id)).slice(0, limit);
}

/** Match count only — no row objects — so the activity heading can show a total. `asOf` is epoch ms. */
export function activityCountForAgent(deck: DeckState, agent: CollaborationAgent, asOf?: number): number {
  const within = (ts: number) => asOf === undefined || toEpochMs(ts) <= asOf;
  if (agent.id === COORDINATOR_ID) {
    let n = 0;
    for (const event of deck.blackboard.events) if (within(event.ts)) n += 1;
    for (const command of deck.controlCommands) if (within(command.ts)) n += 1;
    return n;
  }
  let n = 0;
  for (const event of deck.runtimeEvents) if (event.solverId === agent.id && within(event.ts)) n += 1;
  for (const message of deck.chat) if (message.solverId === agent.id && within(message.ts)) n += 1;
  return n;
}

/** Solver ids whose ledger totals belong to the coordinator card. */
const COORDINATOR_COST_IDS = ["reason", "coordinator", "report-value"];

export function recordedAmount(usd: number, tokens: number, unpricedCalls?: number): string {
  if (unpricedCalls == null) return tokens > 0 || usd > 0 ? "金额口径待核" : "—";
  if (unpricedCalls && usd === 0) return "未定价";
  if (unpricedCalls) return `$${usd.toFixed(4)}（部分未定价）`;
  return `$${usd.toFixed(4)}`;
}

function coordinatorCost(deck: DeckState): { tokens: number; usd: number; unpricedCalls: number | undefined } {
  let tokens = 0;
  let usd = 0;
  let unpricedCalls: number | undefined;
  for (const id of COORDINATOR_COST_IDS) {
    const cost = deck.costBySolver[id];
    if (!cost) continue;
    tokens += (cost.tokensIn || 0) + (cost.tokensOut || 0);
    usd += cost.usd || 0;
    if (cost.unpricedCalls != null) unpricedCalls = (unpricedCalls ?? 0) + cost.unpricedCalls;
  }
  return { tokens, usd, unpricedCalls };
}

function buildWorker(
  deck: DeckState,
  id: string,
  roster: Roster,
  stream: StreamStats | undefined,
  knowledge: KnowledgeLayer,
  locks: number,
): CollaborationAgent {
  const lane = deck.lanes[id];
  const facts = roster.laneFacts.get(id)!;
  const online = lane?.online ?? !deck.finished;
  const intents = knowledge.intentsByAgent.get(id);
  const chronological = intents?.chronological || EMPTY_INTENTS;
  const newestFirst = intents?.newestFirst || EMPTY_INTENTS;
  const laneIntent = facts.intentId ? knowledge.intentById.get(facts.intentId) : undefined;
  const currentIntent = online
    ? (newestFirst.find(isOpenClaim) || (laneIntent && isOpenClaim(laneIntent) ? laneIntent : undefined))
    : undefined;
  const lastIntent = newestFirst.find((intent) => intent.status === "done" || intent.status === "claimed") || laneIntent;
  const agentKnowledge = knowledge.byAgent.get(id);
  const presentation = agentPresentation(lane);
  const statusKind = laneStatusKind(presentation, online);
  const pendingHitl = pendingHitlFor(deck, id);
  const reasonKey = anomalyReasonKey((lane?.statusReason || "").trim());
  const cost = deck.costBySolver[id];

  let last = 0;
  const bump = (value?: number) => { if (value && value > last) last = value; };
  bump(facts.finishedAt);
  bump(agentKnowledge?.maxTs);
  bump(knowledge.lastBoardEventByActor.get(id));
  for (const intent of chronological) {
    bump(intent.claimedTs);
    bump(intent.concludedTs);
  }
  // Board-level progress is read before the chat / runtime stream joins in.
  const progress = last;
  bump(stream?.lastTs);

  const floor = deck.startedAt || 0;
  let first = 0;
  const lower = (value?: number) => { if (value && value >= floor && (!first || value < first)) first = value; };
  for (const intent of chronological) lower(intent.claimedTs ?? intent.proposedTs);
  lower(agentKnowledge?.minTs);
  lower(stream?.firstTs);
  const lifecycle = lifecycleOf(deck, facts, online, first || undefined, last || undefined);
  // Once a worker is offline, later bookkeeping (batch intent_concluded on stop
  // or continuation, late board events) does not move its last activity past
  // its own end.
  const clampToEnd = (value: number) => (!online && lifecycle.endedAt ? Math.min(value, lifecycle.endedAt) : value);
  const lastEventAt = last ? clampToEnd(last) : undefined;
  const lastProgressAt = progress ? clampToEnd(progress) : undefined;
  const activity = workerActivity(lane, presentation, online, stream, lastEventAt, currentIntent, lastIntent);

  return {
    id,
    role: agentRole(facts),
    generation: workerGeneration(id),
    isCurrent: roster.currentIds.has(id),
    identity: agentIdentity(facts),
    presentation,
    session: facts.session,
    spawnedBy: facts.spawnedBy,
    spawnPhase: facts.spawnPhase,
    raceScout: facts.raceScout,
    phase: facts.phase || facts.spawnPhase || "worker",
    statusKind,
    online,
    currentIntent,
    lastIntent,
    intents: chronological,
    knowledge: agentKnowledge?.rows || EMPTY_ROWS,
    searchText: searchHaystack(
      id,
      facts.phase,
      facts.spawnPhase,
      facts.engine,
      facts.model,
      facts.profileId,
      facts.profileLabel,
      currentIntent?.id,
      currentIntent?.goal,
      currentIntent?.summary,
      activity.text,
      ...(agentKnowledge?.rows || EMPTY_ROWS).map((row) => row.title),
    ),
    latestActivity: activity.text,
    latestActivityTs: activity.ts,
    firstSeenAt: lifecycle.startedAt,
    finishedAt: lifecycle.endedAt,
    startedAt: lifecycle.startedAt,
    endedAt: lifecycle.endedAt,
    lifecycleSource: lifecycle.source,
    lastEventAt,
    lastProgressAt,
    tokens: lane?.tokensSpent ?? ((cost?.tokensIn || 0) + (cost?.tokensOut || 0)),
    usd: cost?.usd || 0,
    unpricedCalls: cost?.unpricedCalls,
    ...(agentKnowledge?.metrics || emptyMetrics()),
    locks,
    anomalyReasonKey: reasonKey,
    pendingHitl,
    isAnomaly: statusKind === "paused" || statusKind === "stalled" || statusKind === "error" || pendingHitl > 0,
  };
}

function buildCoordinator(
  deck: DeckState,
  roster: Roster,
  knowledge: KnowledgeLayer,
  workers: CollaborationAgent[],
  locks: number,
): CollaborationAgent {
  const statusKind: LaneStatusKind = deck.finished ? (deck.solved ? "solved" : "offline")
    : deck.preflightFailures.length ? "error"
      : deck.awaitingOperator ? "paused"
        : deck.preparing ? "waiting" : "thinking";
  const controlEvent = knowledge.lastControlEvent;
  const cost = coordinatorCost(deck);
  const lastEventAt = controlEvent?.ts ?? deck.controlCommands[deck.controlCommands.length - 1]?.ts;
  let totalWorkers = 0;
  let onlineWorkers = 0;
  for (const worker of workers) {
    if (!worker.isCurrent) continue;
    totalWorkers += 1;
    if (worker.online) onlineWorkers += 1;
  }
  return {
    id: COORDINATOR_ID,
    role: "coordinator",
    generation: roster.currentGeneration,
    isCurrent: true,
    identity: {},
    presentation: {},
    phase: swarmPhase(deck),
    statusKind,
    online: deck.started && !deck.finished,
    intents: deck.blackboard.intents.filter((intent) => !intent.worker),
    knowledge: knowledge.coordinator.rows,
    searchText: searchHaystack(
      COORDINATOR_ID,
      swarmPhase(deck),
      controlEvent?.label,
      ...knowledge.coordinator.rows.map((row) => row.title),
    ),
    latestActivity: controlEvent?.label || "",
    latestActivityTs: controlEvent?.ts,
    firstSeenAt: deck.startedAt,
    finishedAt: deck.finishedAt,
    startedAt: deck.startedAt,
    endedAt: deck.finishedAt,
    lifecycleSource: "run",
    lastEventAt,
    // The coordinator has no chat stream; its control events are its progress.
    lastProgressAt: lastEventAt,
    tokens: cost.tokens,
    usd: cost.usd,
    unpricedCalls: cost.unpricedCalls,
    ...knowledge.coordinator.metrics,
    locks,
    pendingHitl: 0,
    isAnomaly: statusKind === "paused" || statusKind === "error",
    coordination: { ...knowledge.coordination, flags: deck.flags.length, onlineWorkers, totalWorkers },
  };
}

/**
 * Cost per build with W workers, C chat rows, R runtime events, K knowledge
 * rows and I intents: a streaming event recomputes the stream index (C + R)
 * and the assembly (W plus the per-worker intent scan); a board event adds
 * the knowledge pass (K log K) and the relation pass (I + K + findings).
 * Nothing is scanned once per worker any more.
 */
export function buildAgentCollaborationModel(deck: DeckState, scope: CollaborationScope): AgentCollaborationModel {
  const stream = streamIndexOf(deck.chat, deck.runtimeEvents);
  const roster = rosterOf(deck.lanes, stream, deck.blackboard.workers, deck.executionGeneration);
  const knowledge = knowledgeLayerOf(
    roster,
    deck.blackboard,
    deck.operatorDirectives,
    deck.lockRequests,
    deck.startedAt,
    deck.finishedAt,
    deck.mode,
    deck.taskContract,
    deck.expectedFlags,
    deck.flags,
    deck.solved,
    deck.reason.goalMet,
  );
  const relationLayer = relationLayerOf(roster, knowledge, deck.blackboard, deck.operatorDirectives, deck.lockRequests, deck.startedAt);

  const locksByOwner = new Map<string, number>();
  for (const lock of deck.resourceLocks) {
    if (lock.status !== "active") continue;
    const owner = collaborationAgentId(lock.ownerWorker);
    if (owner) locksByOwner.set(owner, (locksByOwner.get(owner) || 0) + 1);
  }

  const workers = roster.workers
    .map((id) => buildWorker(deck, id, roster, stream.get(id), knowledge, locksByOwner.get(id) || 0))
    .sort((a, b) => (a.startedAt ?? 0) - (b.startedAt ?? 0) || a.id.localeCompare(b.id));
  const coordinator = buildCoordinator(deck, roster, knowledge, workers, locksByOwner.get(COORDINATOR_ID) || 0);
  // These are read-only projections of records already received by the UI.
  // Never backfill a historical model from today's global configuration.
  const reasonEvents = deck.blackboard.events.filter((event) => event.kind === "reason_start" || event.kind === "reason_done");
  const rounds = deck.blackboard.reasonRuns;
  const lastReason = reasonEvents[reasonEvents.length - 1];
  const decisionActive = !deck.finished && lastReason?.kind === "reason_start";
  const decisionIdentity = deck.lanes.reason ? agentIdentity(laneFactsOf(deck.lanes.reason)) : {};
  const decision: CollaborationAgent | undefined = rounds.length || reasonEvents.length ? {
    ...coordinator,
    id: DECISION_ID,
    role: "decision",
    identity: decisionIdentity,
    phase: canvasModeOf(deck) === "ctf" ? "Decide" : "Reason",
    statusKind: decisionActive ? "thinking" : "offline",
    online: decisionActive,
    intents: [],
    knowledge: [],
    ...emptyMetrics(),
    coordination: undefined,
    tokens: (deck.costBySolver.reason?.tokensIn || 0) + (deck.costBySolver.reason?.tokensOut || 0),
    usd: deck.costBySolver.reason?.usd || 0,
    unpricedCalls: deck.costBySolver.reason?.unpricedCalls,
    latestActivity: lastReason?.label || "",
    firstSeenAt: reasonEvents[0]?.ts ?? rounds[0]?.ts,
    startedAt: reasonEvents[0]?.ts ?? rounds[0]?.ts,
    finishedAt: decisionActive ? undefined : lastReason?.ts ?? rounds[rounds.length - 1]?.ts,
    endedAt: decisionActive ? undefined : lastReason?.ts ?? rounds[rounds.length - 1]?.ts,
    lastEventAt: lastReason?.ts,
    lastProgressAt: lastReason?.ts,
    isAnomaly: rounds.some((round) => !!round.plannerFailure),
    searchText: searchHaystack("Decide", "Reason", "决策模型", decisionIdentity.model),
  } : undefined;
  const inputKnowledge = knowledge.rows.filter((item) => item.agentId === INPUT_SOURCE_ID);
  const source: CollaborationAgent | undefined = inputKnowledge.length ? {
    ...coordinator,
    id: INPUT_SOURCE_ID,
    role: "source",
    phase: "input",
    statusKind: "offline",
    online: false,
    intents: [],
    knowledge: inputKnowledge,
    ...summarizeKnowledge(inputKnowledge),
    coordination: undefined,
    tokens: 0,
    usd: 0,
    locks: 0,
    isAnomaly: false,
    firstSeenAt: inputKnowledge[0].appearedTs,
    startedAt: undefined,
    finishedAt: undefined,
    endedAt: undefined,
    latestActivity: "",
    latestActivityTs: inputKnowledge[inputKnowledge.length - 1].ts,
    lastEventAt: inputKnowledge[inputKnowledge.length - 1].ts,
    lastProgressAt: inputKnowledge[inputKnowledge.length - 1].ts,
    searchText: searchHaystack("origin", "任务输入", "input source", ...inputKnowledge.map((item) => item.title)),
  } : undefined;
  const fullAgents = [coordinator, ...(decision ? [decision] : []), ...(source ? [source] : []), ...workers];
  const visibleAgents = scope === "all" ? fullAgents : fullAgents.filter((agent) => agent.isCurrent);
  const agentById = new Map(fullAgents.map((agent) => [agent.id, agent]));
  const visibleIds = new Set(visibleAgents.map((agent) => agent.id));
  const relations = [...finalizeRelations(relationLayer.relations, visibleIds, (id) => !!agentById.get(id)?.online)];
  const callTimes = [...rounds.map((round) => round.ts), ...(decisionActive && lastReason ? [lastReason.ts] : [])];
  if (decision && callTimes.length) {
    relations.push({
      id: "model:coordinator:decision-model", kind: "model", source: COORDINATOR_ID, target: DECISION_ID,
      count: callTimes.length, refIds: callTimes.map((ts) => `reason:${ts}`),
      timestamps: callTimes, ts: callTimes[0], lastTs: callTimes[callTimes.length - 1],
      active: decisionActive,
    });
  }

  return {
    agents: visibleAgents,
    allAgents: fullAgents,
    coordinator,
    agentById,
    workers: roster.workers,
    relations,
    knowledge: knowledge.rows,
    knowledgeById: relationLayer.knowledgeById,
    openIntents: collaborationOpenIntents(deck),
    resumableIntents: collaborationResumableIntents(deck),
    unattributedUsd: unattributedUsd(deck),
    currentGeneration: roster.currentGeneration,
  };
}
