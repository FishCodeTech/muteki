/**
 * The typed event stream contract — mirrors muteki/core/events.py EventType +
 * Event. The UI is a DUMB SUBSCRIBER (§3): it never calls the core, it only
 * consumes these events and POSTs HITL. Keep this enum in lockstep with Python.
 */

export enum EventType {
  RUN_STARTED = "run.started",
  RUN_TITLED = "run.titled",
  RUN_FINISHED = "run.finished",
  RUN_REOPENED = "run.reopened",
  WORKER_STATUS = "worker.status",
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
  HITL_TRANSLATED = "hitl.translated",
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

export type WorkerLaneRole = "worker" | "review";

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
}

/** A single agent's running cost/token totals, for the cost hover card. */
export interface SolverCost {
  usd: number;
  tokensIn: number;
  tokensOut: number;
  engine?: string; // "claude" | "codex" | "cursor" | "deepseek" (best-effort)
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
export interface ChatMessage {
  id: string;
  role: ChatRole;
  solverId?: string;
  // True for worker-produced conversational follow-ups (post-solve standby ask /
  // writeup) that should still appear in the main coordinator thread. The solverId
  // is kept for activity/diagnostics, but the conversation spine treats it as an
  // operator-facing answer rather than worker firehose.
  mainThread?: boolean;
  kind: "reasoning" | "text" | "tool" | "insight" | "guidance" | "flag" | "status";
  content: string;
  ts: number;
  // Optional i18n hook for system-generated lifecycle lines (run started /
  // solved / finished). The render layer translates `i18nKey` with `i18nVars`
  // when present; `content` stays as the English fallback. Agent-produced text
  // (reasoning/insight/tool) carries no key — it renders verbatim, in whatever
  // language the swarm emitted.
  i18nKey?: string;
  i18nVars?: Record<string, string>;
}

/** Derived graph model for the Cytoscape view. Nodes accrete as the run evolves:
 *  one challenge root, one node per solver, fact nodes (verified vs candidate),
 *  intent nodes (from the reason phase), dead-end nodes, and the flag. Edges link
 *  facts/intents to the solver that produced them, and the flag to its winner. */
export type GraphNodeType =
  | "challenge" | "solver" | "fact" | "candidate" | "intent" | "dead_end" | "poc" | "flag";
export interface GraphNode {
  id: string;
  type: GraphNodeType;
  label: string;
  meta?: Record<string, any>;
}
export interface GraphEdge {
  id: string;
  source: string;
  target: string;
  kind: string;
}
export interface GraphModel {
  nodes: GraphNode[];
  edges: GraphEdge[];
  version: number; // bumps on every structural change so the view re-lays-out
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
  ts: number;
}

export interface ResourceLock {
  lockId: string;
  resourceKey: string;
  scope: string;
  riskClass?: string;
  status: "active" | "released" | "expired" | "denied" | "requested" | "deferred";
  ownerWorker?: string;
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

export interface BlackboardIntent {
  id: string;
  goal: string;
  summary?: string;      // deepseek-flash zh gist (replaces goal in the card head)
  workerClass: string;
  fromFacts: number[];   // shared_graph fact seqs that motivated this intent
  toFactSeq?: number;    // shared_graph fact seq produced when the intent concluded
  status: "open" | "claimed" | "done";
  dispatchState?: IntentDispatchState;  // A/J: active | resume | retired | closed
  closeReason?: string;  // why it left the dispatch pool
  worker?: string;       // the solver that claimed it
  proposedTs: number;
  claimedTs?: number;
  concludedTs?: number;
}
export interface BlackboardFact {
  factSeq?: number;      // shared_graph event seq; stable node id / parent link
  fact: string;
  summary?: string;      // deepseek-flash zh gist (replaces fact in the card head)
  verified: boolean;
  confidence: number;
  actor: string;
  verifier: string;
  witness?: string;
  artifactId?: string;
  challenged?: boolean;
  challengeReason?: string;
  revalidated?: boolean;
  state?: FactLifecycleState;  // A: lifecycle state (rejected/merged/superseded retire it)
  mergedInto?: number;         // when state=merged: the fact seq it folded into
  intentId?: string;           // G0: the intent that PRODUCED this fact (intent_products edge)
  ts: number;
}
export interface BlackboardReviewFinding {
  id: string;
  kind: string;
  severity: string;
  summary: string;
  routeHash?: string;
  branchId?: string;
  actor: string;
  ts: number;
}
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
export interface BlackboardView {
  intents: BlackboardIntent[];
  facts: BlackboardFact[];
  pocs: BlackboardPoc[];
  deadEnds: { deadEndSeq?: number; reason: string; actor: string; ts: number }[];
  reviewFindings: BlackboardReviewFinding[];
  suppressedRoutes: BlackboardSuppressedRoute[];
  branches: BlackboardBranch[];
  directives: BlackboardDirective[];
  flag?: string;                       // first/primary flag (back-comat)
  flags?: string[];                    // every distinct flag captured (multi-flag)
  events: BlackboardEvent[];           // append-only timeline
  workers: string[];                   // distinct actors seen (for lanes/legend)
}

export interface DeckState {
  runId: string;
  challengeName: string;
  category: string;
  target: string;
  lanes: Record<string, SolverLane>;
  graph: SolveGraphView;
  sharedGraph: SharedGraphView;
  reason: ReasonView;
  insights: string[];
  terminal: string[];
  chat: ChatMessage[];
  model: GraphModel;
  blackboard: BlackboardView;
  hitlRequests: HitlRequest[];
  operatorDirectives: OperatorDirective[];  // B: first-class operator steering
  resourceLocks: ResourceLock[];            // E: unified site/account/listener locks
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
  started: boolean;
  finished: boolean;
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
  expectedFlags: number;
  // multi-flag MODE bit. When true with an unknown count (expectedFlags<=1), a
  // saved flag does NOT mark the run solved — it keeps "collecting" until the run
  // finishes (operator STOP / no-progress pause). Decouples save from finish in the
  // UI exactly as the backend does. Default false = single-flag (first flag solves).
  multiFlag?: boolean;
  // how a finished run concluded: "solved" = a CTF flag was gated; "goal_met" =
  // a pentest engagement goal was reached (no flag); "finished" = ended without
  // either. Drives the outcome label (flag chip vs findings summary).
  outcomeReason?: "solved" | "goal_met" | "finished" | "operator_stop" | "budget_exhausted" | "runtime_failure";
  outcomeDetail?: string;
  // pentest: why the engagement goal was judged met (from the goal_complete event).
  goalWhy?: string;
  // set when the coordinator PAUSED waiting for operator input (a worker raised a
  // NEED_INPUT / env_down). Holds the outstanding ask(s). Cleared on resume.
  awaitingOperator?: string;
  // race-scout layer: true while the front race round (3 engines in parallel,
  // single-shot) is running, before the main coordinator loop. Drives the "racing" status pill.
  racing?: boolean;
  // engines dropped from THIS run's roster by a dispatch-time health-check failure
  // (e.g. cursor headless auth lapsed → "Authentication required"). engine → reason.
  // Lets the worker panel / engine bar show "cursor degraded: …" instead of the
  // engine silently never appearing. Cleared per-engine on a recover event.
  degradedEngines: Record<string, string>;
}

export function emptyDeck(runId: string): DeckState {
  return {
    runId,
    challengeName: "",
    category: "",
    target: "",
    lanes: {},
    graph: { evidence: [], hypotheses: [], deadEnds: [] },
    sharedGraph: { verified: [], candidates: [] },
    reason: { goalMet: false, intents: [], audit: [] },
    insights: [],
    terminal: [],
    chat: [],
    model: { nodes: [{ id: "challenge", type: "challenge", label: runId }], edges: [], version: 0 },
    blackboard: {
      intents: [],
      facts: [],
      pocs: [],
      deadEnds: [],
      reviewFindings: [],
      suppressedRoutes: [],
      branches: [],
      directives: [],
      flags: [],
      events: [],
      workers: [],
    },
    hitlRequests: [],
    operatorDirectives: [],
    resourceLocks: [],
    compactEpochs: 0,
    gauge: { zones: [], total: 0, limit: 0 },
    usd: 0,
    tokensIn: 0,
    tokensOut: 0,
    costBySolver: {},
    started: false,
    finished: false,
    solved: false,
    flags: [],
    expectedFlags: 1,
    multiFlag: false,
    degradedEngines: {},
  };
}

/** Accumulate a flag into the deck (dedup, keep order), maintaining the
 *  flag/flags[0] invariant. Accepts a single flag or a list. */
function mergeFlags(s: DeckState, flags?: string | string[] | null): void {
  const list = typeof flags === "string" ? [flags] : (flags ?? []);
  for (const f of list) {
    if (f && !s.flags.includes(f)) s.flags.push(f);
  }
  if (s.flags.length && !s.flag) s.flag = s.flags[0];
}

function lane(state: DeckState, sid?: string | null): SolverLane {
  const id = sid || "solver";
  if (!state.lanes[id]) {
    state.lanes[id] = {
      solverId: id,
      reasoning: "",
      toolLines: [],
      status: "thinking",
      solved: false,
      online: true,
    };
  }
  return state.lanes[id];
}

function reviewRoleFromPhase(phase?: string | null): WorkerLaneRole | undefined {
  return phase && phase.toLowerCase().includes("review") ? "review" : undefined;
}

function roleFromWorkerRole(workerRole?: unknown): WorkerLaneRole | undefined {
  const raw = (workerRole ?? "").toString().toLowerCase();
  if (!raw) return undefined;
  return raw === "review" ? "review" : "worker";
}

function activityOnline(state: DeckState): boolean {
  return !state.finished;
}

// ---- graph model helpers (mutate the cloned draft in reduce) ----------------

let _gid = 0;
function gid(prefix: string): string {
  _gid += 1;
  return `${prefix}_${_gid}`;
}

/** Ensure a solver node exists and is linked to the challenge root. */
function ensureSolverNode(m: GraphModel, sid: string): void {
  if (sid && !m.nodes.some((n) => n.id === `solver:${sid}`)) {
    m.nodes.push({ id: `solver:${sid}`, type: "solver", label: sid });
    m.edges.push({ id: gid("e"), source: "challenge", target: `solver:${sid}`, kind: "runs" });
    m.version += 1;
  }
}

function addGraphNode(
  m: GraphModel, type: GraphNodeType, label: string,
  opts: { id?: string; from?: string; fromMulti?: string[]; kind?: string; meta?: Record<string, any> } = {}
): string {
  const id = opts.id || gid(type);
  const exists = m.nodes.some((n) => n.id === id);
  // keep the FULL raw text in meta.raw (for the <details>/tooltip) even though the
  // visible label is clipped — node.summarized later swaps label for a zh gist.
  if (!exists) {
    m.nodes.push({ id, type, label: label.slice(0, 140), meta: { ...(opts.meta || {}), raw: label } });
    m.version += 1;
  }
  const sources = opts.fromMulti?.filter((s) => m.nodes.some((n) => n.id === s));
  if (sources && sources.length > 0) {
    for (const src of sources) {
      addGraphEdge(m, src, id, opts.kind || "derives");
    }
  } else {
    const src = opts.from && m.nodes.some((n) => n.id === opts.from) ? opts.from : "challenge";
    addGraphEdge(m, src, id, opts.kind || "derives");
  }
  return id;
}

function addGraphEdge(m: GraphModel, source: string, target: string, kind: string): void {
  if (!m.nodes.some((n) => n.id === source) || !m.nodes.some((n) => n.id === target)) {
    if (
      typeof console !== "undefined" &&
      (typeof process === "undefined" || process.env?.NODE_ENV !== "production")
    ) {
      console.warn("[muteki graph] skipped edge with missing endpoint", {
        source, target, kind, nodes: m.nodes.map((n) => n.id),
      });
    }
    return;
  }
  if (m.edges.some((e) => e.source === source && e.target === target && e.kind === kind)) return;
  m.edges.push({ id: gid("e"), source, target, kind });
  m.version += 1;
}

function blackboardOwningIntent(
  bb: BlackboardView,
  actor: string,
  ts: number,
  factSeq?: number,
): BlackboardIntent | undefined {
  if (factSeq) {
    const exact = bb.intents.find((it) => it.toFactSeq === factSeq);
    if (exact) return exact;
  }
  const within = (it: BlackboardIntent) => {
    const lo = it.claimedTs ?? it.proposedTs;
    const hi = it.concludedTs ?? Infinity;
    return ts >= lo - 1 && ts <= hi + 1;
  };
  return [...bb.intents]
    .reverse()
    .find((it) => it.worker && it.worker === actor && within(it));
}

// defect-7: chat is the conversation spine — a 400-message ring buffer silently
// dropped the EARLIEST turns on a long multi-worker run (the persistence loss the
// operator hit). Raised to CHAT_CAP=4000 so a full run's history survives; that's
// well within what the (scrolled) feed renders without jank, and the backend SSE
// replay already rehydrates up to its own ring on reconnect.
const CHAT_CAP = 4000;
function pushChat(s: DeckState, msg: Omit<ChatMessage, "id">): void {
  s.chat = [...s.chat, { ...msg, id: gid("c") }].slice(-CHAT_CAP);
}

/** The operator's launch input, reconstructed from the RUN_STARTED challenge for
 *  display as the opening "you" bubble: the free-text description first, then a
 *  compact context footer (target / goal / scope / attachments) for whatever was
 *  actually supplied. Returns "" when there's nothing the user typed to show. */
function userPromptBubble(ch: Record<string, any> | undefined): string {
  if (!ch) return "";
  const lines: string[] = [];
  const desc = (ch.description || "").trim();
  if (desc) lines.push(desc);
  const ctx: string[] = [];
  if (ch.target) ctx.push(`target: ${ch.target}`);
  if (ch.mode === "pentest" && ch.goal) ctx.push(`goal: ${ch.goal}`);
  if (ch.mode === "pentest" && ch.scope) ctx.push(`scope: ${ch.scope}`);
  const atts: unknown = ch.attachments;
  if (Array.isArray(atts) && atts.length) {
    const names = atts.map((p) => String(p).split("/").pop()).filter(Boolean);
    ctx.push(`attachments: ${names.join(", ")}`);
  }
  if (ctx.length) lines.push(ctx.join("\n"));
  return lines.join("\n\n").trim();
}

/** Fold one event into the deck state (pure-ish; mutates a draft copy). */
export function reduce(prev: DeckState, ev: MutekiEvent): DeckState {
  const s: DeckState = {
    ...prev,
    lanes: { ...prev.lanes },
    graph: { ...prev.graph, evidence: [...prev.graph.evidence], hypotheses: [...prev.graph.hypotheses], deadEnds: [...prev.graph.deadEnds] },
    sharedGraph: { verified: [...prev.sharedGraph.verified], candidates: [...prev.sharedGraph.candidates] },
    reason: { ...prev.reason, intents: [...prev.reason.intents], audit: [...prev.reason.audit] },
    insights: [...prev.insights],
    terminal: [...prev.terminal],
    chat: prev.chat,
    model: { nodes: [...prev.model.nodes], edges: [...prev.model.edges], version: prev.model.version },
    blackboard: {
      intents: [...prev.blackboard.intents],
      facts: [...prev.blackboard.facts],
      pocs: [...prev.blackboard.pocs],
      deadEnds: [...prev.blackboard.deadEnds],
      reviewFindings: [...(prev.blackboard.reviewFindings ?? [])],
      suppressedRoutes: [...(prev.blackboard.suppressedRoutes ?? [])],
      branches: [...(prev.blackboard.branches ?? [])],
      directives: [...(prev.blackboard.directives ?? [])],
      events: [...prev.blackboard.events],
      workers: [...prev.blackboard.workers],
      flag: prev.blackboard.flag,
      flags: [...(prev.blackboard.flags ?? [])],
    },
    hitlRequests: [...prev.hitlRequests],
    operatorDirectives: [...(prev.operatorDirectives ?? [])],
    resourceLocks: [...(prev.resourceLocks ?? [])],
  };
  const m = s.model;
  const sid = ev.solver_id || "";
  if (sid) ensureSolverNode(m, sid);
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.RUN_STARTED: {
      // RUN_STARTED fires once PER worker; only the first one opens the thread.
      const firstStart = !prev.started;
      s.started = true;
      s.finished = false;
      if (firstStart) { s.startedAt = ev.ts; s.finishedAt = undefined; }
      s.challengeName = p.challenge?.name ?? s.challengeName;
      s.category = p.challenge?.category ?? s.category;
      s.target = p.challenge?.target ?? s.target;
      // multi-flag: pick up the target flag count + mode bit so "collecting" works
      // from the start of the run, not just at RUN_FINISHED.
      if (typeof p.challenge?.expected_flags === "number") s.expectedFlags = p.challenge.expected_flags;
      if (typeof p.challenge?.multi_flag === "boolean") s.multiFlag = p.challenge.multi_flag;
      if (sid) {
        const l = lane(s, sid);
        s.lanes[l.solverId] = { ...l, online: true, status: "online", statusReason: "started" };
      }
      // relabel the root with the real challenge name
      const root = m.nodes.find((n) => n.id === "challenge");
      if (root && s.challengeName) { root.label = s.challengeName; m.version += 1; }
      if (firstStart) {
        // surface the operator's launch prompt as the opening "you" bubble, so
        // the thread shows what kicked the run off — not just a system line.
        const prompt = userPromptBubble(p.challenge);
        if (prompt) pushChat(s, { role: "human", kind: "text", content: prompt, ts: ev.ts });
        pushChat(s, { role: "system", kind: "status", content: `Run started — ${s.challengeName || s.runId}`, ts: ev.ts, i18nKey: "sys.runStarted", i18nVars: { name: s.challengeName || s.runId } });
      }
      break;
    }
    case EventType.WORKER_STATUS: {
      const l = lane(s, ev.solver_id);
      const online = p.online !== false;
      const phase = (p.phase ?? l.phase ?? "").toString() || undefined;
      const role = roleFromWorkerRole(p.worker_role) ?? reviewRoleFromPhase(phase) ?? l.role;
      s.lanes[l.solverId] = {
        ...l,
        online,
        status: (p.status ?? (online ? "online" : "offline")).toString(),
        statusReason: (p.reason ?? l.statusReason ?? "").toString(),
        engine: (p.engine ?? l.engine ?? "").toString() || undefined,
        role,
        phase,
        // I: carry intent/tokens/paused when the status payload (or its runtime
        // block) supplies them; sticky otherwise.
        intentId: (p.intent_id ?? p.runtime?.intent_id ?? l.intentId ?? "").toString() || undefined,
        tokensSpent: typeof p.tokens_spent === "number" ? p.tokens_spent
          : (typeof p.runtime?.tokens_spent === "number" ? p.runtime.tokens_spent : l.tokensSpent),
        paused: p.reason === "paused" ? true : (online ? (l.paused ?? false) : false),
        // sticky: a later status emit with no session must not wipe a known id.
        session: (p.session ?? l.session ?? "").toString() || undefined,
        runtime: (p.runtime && typeof p.runtime === "object") ? p.runtime : l.runtime,
      };
      break;
    }
    case EventType.WORKER_LIFECYCLE: {
      // I: granular lifecycle — spawned | phase_changed | stalled | exited. Updates
      // this worker's lane without disturbing its WORKER_STATUS online/offline state.
      const l = lane(s, ev.solver_id);
      const phaseName = (p.phase ?? "").toString();
      const next: SolverLane = { ...l };
      if (typeof p.tokens_spent === "number") next.tokensSpent = p.tokens_spent;
      if (p.intent_id) next.intentId = String(p.intent_id);
      if (typeof p.paused === "boolean") next.paused = p.paused;
      if (phaseName === "spawned") {
        next.phase = (p.phase_label ?? next.phase ?? "").toString() || undefined;
        next.online = true;
      } else if (phaseName === "phase_changed") {
        if (p.phase_label) next.phase = String(p.phase_label);
      } else if (phaseName === "stalled") {
        next.status = "stalled";
      } else if (phaseName === "exited") {
        next.online = false;
      }
      s.lanes[l.solverId] = next;
      break;
    }
    case EventType.RUN_TITLED: {
      // Auto-title landed (ChatGPT-style). Only adopt it as the conversation
      // name if the operator hasn't supplied a real challenge name.
      const title = p.title ?? "";
      if (title && !s.challengeName) {
        s.challengeName = title;
        const root = m.nodes.find((n) => n.id === "challenge");
        if (root) { root.label = title; m.version += 1; }
      }
      break;
    }
    case EventType.REASONING_DELTA: {
      const l = lane(s, ev.solver_id);
      s.lanes[l.solverId] = {
        ...l,
        reasoning: (l.reasoning + (p.text ?? "")).slice(-4000),
        status: "thinking",
        online: activityOnline(s),
        statusReason: undefined,
      };
      // stream reasoning into the chat transcript: append to the last open
      // reasoning bubble for this solver, else start a new one.
      const last = s.chat[s.chat.length - 1];
      if (last && last.role === "agent" && last.kind === "reasoning" && last.solverId === l.solverId) {
        s.chat = [...s.chat.slice(0, -1), { ...last, content: (last.content + (p.text ?? "")).slice(-6000) }];
      } else if (p.text) {
        pushChat(s, { role: "agent", solverId: l.solverId, kind: "reasoning", content: p.text, ts: ev.ts });
      }
      break;
    }
    case EventType.TEXT_MESSAGE_DELTA: {
      const l = lane(s, ev.solver_id);
      s.lanes[l.solverId] = { ...l, online: activityOnline(s), statusReason: undefined };
      const mainThread = !!p.main_thread || l.statusReason === "standby";
      const last = s.chat[s.chat.length - 1];
      if (last && last.role === "agent" && last.kind === "text" && last.solverId === l.solverId && !!last.mainThread === mainThread) {
        s.chat = [...s.chat.slice(0, -1), { ...last, content: (last.content + (p.text ?? "")).slice(-6000) }];
      } else if (p.text) {
        pushChat(s, { role: "agent", solverId: l.solverId, mainThread, kind: "text", content: p.text, ts: ev.ts });
      }
      break;
    }
    case EventType.TOOL_CALL_START: {
      const l = lane(s, ev.solver_id);
      s.lanes[l.solverId] = {
        ...l,
        status: `tool: ${p.tool ?? "?"}`,
        reasoning: "",
        online: activityOnline(s),
        statusReason: undefined,
      };
      pushChat(s, { role: "agent", solverId: l.solverId, kind: "tool", content: `▶ ${p.tool ?? "tool"}`, ts: ev.ts });
      break;
    }
    case EventType.TOOL_CALL_RESULT: {
      const l = lane(s, ev.solver_id);
      const res = p.result || {};
      const cond = (res.condensed ?? "").toString();
      const head = cond.split("\n")[0] || "(result)";
      // keep a fuller preview in the transcript (up to ~6 lines / 800 chars) so a
      // tool's output isn't reduced to a single line — the timeline redesign lets
      // the bubble wrap/scroll instead of truncating to the first line.
      const preview = cond.split("\n").slice(0, 6).join("\n").slice(0, 800) || head;
      s.lanes[l.solverId] = {
        ...l,
        toolLines: [...l.toolLines, head].slice(-12),
        online: activityOnline(s),
        statusReason: undefined,
      };
      pushChat(s, { role: "agent", solverId: l.solverId, kind: "tool", content: `↳ ${preview}`, ts: ev.ts });
      break;
    }
    case EventType.TERMINAL_OUTPUT:
      s.terminal = [...s.terminal, p.text ?? ""].slice(-1000);
      break;
    case EventType.SOLVE_GRAPH_DELTA:
      if (p.kind === "evidence_added") s.graph.evidence = [...s.graph.evidence, p.fact].slice(-50);
      else if (p.kind === "flag") { s.graph.flag = p.flag; mergeFlags(s, p.flag); }
      else if (p.kind === "dead_end") {
        s.graph.deadEnds = [...s.graph.deadEnds, p.reason];
        addGraphNode(m, "dead_end", p.reason ?? "dead end", { from: sid ? `solver:${sid}` : undefined, kind: "refutes" });
      }
      break;
    case EventType.INSIGHT_BUS_EVENT: {
      const line = `${p.kind}: ${p.flag ?? p.text ?? ""}${sid ? " (" + sid + ")" : ""}`;
      s.insights = [...s.insights, line].slice(-50);
      if (p.kind === "DeadEndMarked") {
        addGraphNode(m, "dead_end", p.text ?? "dead end", { from: sid ? `solver:${sid}` : undefined, kind: "refutes" });
      }
      pushChat(s, { role: "system", kind: "insight", content: `insight · ${line}`, ts: ev.ts });
      break;
    }
    case EventType.SHARED_GRAPH_DELTA: {
      const row: SharedEvidence = {
        fact: p.fact ?? "", verified: !!p.verified, confidence: p.confidence ?? 0,
        actor: p.actor ?? sid ?? "", verifier: p.verifier ?? "",
      };
      if (row.verified) s.sharedGraph.verified = [...s.sharedGraph.verified, row].slice(-50);
      else s.sharedGraph.candidates = [...s.sharedGraph.candidates, row].slice(-50);
      const from = row.actor ? `solver:${row.actor}` : (sid ? `solver:${sid}` : undefined);
      if (from) ensureSolverNode(m, row.actor || sid);
      // Use the same fact:{seq} id the blackboard fact_added delta uses, so the
      // two events for one fact collapse onto a SINGLE node (addGraphNode dedupes
      // by id). Without this the fact appears twice (seq-id + gid).
      const sgSeq = typeof p.fact_seq === "number" && p.fact_seq > 0 ? p.fact_seq : undefined;
      const sgOwner = blackboardOwningIntent(s.blackboard, row.actor || sid, ev.ts, sgSeq);
      addGraphNode(m, row.verified ? "fact" : "candidate", row.fact, {
        id: sgSeq ? `fact:${sgSeq}` : undefined,
        from: sgOwner ? `intent:${sgOwner.id}` : from,
        kind: sgOwner ? "produces" : (row.verified ? "verifies" : "claims"),
        // actor = the worker that produced this fact. It drives the per-engine
        // colour ring in the radar view; the data was always on `row.actor` but
        // never made it into meta, so the graph couldn't colour facts by worker.
        // intentId = the owning intent (for the intent-centric radar grouping).
        meta: {
          confidence: row.confidence, verifier: row.verifier, verified: row.verified,
          actor: row.actor || undefined,
          intentId: sgOwner ? sgOwner.id : undefined,
        },
      });
      break;
    }
    case EventType.REASON_INTENT: {
      const intents = (p.intents ?? []).map((it: any) => ({
        id: it.id ?? it.intent_id ?? "", goal: it.goal ?? "",
        workerClass: it.worker_class ?? "code",
      }));
      s.reason = { goalMet: !!p.goal_met, intents, audit: p.audit ?? [] };
      for (const it of intents) {
        addGraphNode(m, "intent", it.goal, { from: sid ? `solver:${sid}` : undefined, kind: "plans", meta: { workerClass: it.workerClass, proposer: sid || undefined } });
      }
      break;
    }
    case EventType.BLACKBOARD_DELTA: {
      const bb = s.blackboard;
      const actor = p.actor ?? sid ?? "";
      if (actor && !bb.workers.includes(actor)) bb.workers = [...bb.workers, actor];
      const tlabel = (txt: string) => {
        bb.events = [...bb.events, { id: gid("bbe"), kind: p.kind, actor, ts: ev.ts, label: txt }].slice(-300);
      };
      switch (p.kind) {
        case "intent_proposed": {
          const fromFacts: number[] = Array.isArray(p.from_facts)
            ? p.from_facts.filter((seq: any) => typeof seq === "number" && seq > 0)
            : [];
          if (!bb.intents.some((i) => i.id === p.intent_id)) {
            bb.intents = [...bb.intents, {
              id: p.intent_id, goal: p.goal ?? "", workerClass: p.worker_class ?? "code",
              fromFacts, status: "open", proposedTs: ev.ts,
            }];
          } else {
            bb.intents = bb.intents.map((i) =>
              i.id === p.intent_id ? { ...i, fromFacts: fromFacts.length ? fromFacts : i.fromFacts } : i);
          }
          tlabel(`proposed ${p.intent_id} · ${(p.goal ?? "").slice(0, 60)}`);
          const fromIds = fromFacts.map((seq: number) => `fact:${seq}`);
          const solverFrom = actor ? `solver:${actor}` : (sid ? `solver:${sid}` : undefined);
          if (solverFrom) ensureSolverNode(m, actor || sid);
          addGraphNode(m, "intent", p.goal ?? "", {
            id: `intent:${p.intent_id}`,
            from: solverFrom,
            fromMulti: fromIds.length > 0 ? fromIds : undefined,
            kind: "plans",
            // intentId = stable id for radar grouping; proposer = who planned it
            // (usually the coordinator/reason actor) so the radar can label the
            // intent centre without re-deriving it from incoming solver edges.
            meta: { workerClass: p.worker_class ?? "code", intentId: p.intent_id, proposer: actor || sid || undefined },
          });
          break;
        }
        case "intent_claimed": {
          bb.intents = bb.intents.map((i) =>
            i.id === p.intent_id ? { ...i, status: "claimed", worker: p.worker ?? actor, claimedTs: ev.ts } : i);
          tlabel(`${p.worker ?? actor} claimed ${p.intent_id}`);
          break;
        }
        case "intent_concluded": {
          const toFactSeq = typeof p.to_fact_seq === "number" && p.to_fact_seq > 0 ? p.to_fact_seq : undefined;
          bb.intents = bb.intents.map((i) =>
            i.id === p.intent_id ? { ...i, status: "done", dispatchState: "closed", closeReason: p.result ?? i.closeReason, worker: i.worker ?? p.worker ?? actor, concludedTs: ev.ts, toFactSeq: toFactSeq ?? i.toFactSeq } : i);
          tlabel(`${p.worker ?? actor} concluded ${p.intent_id}`);
          if (toFactSeq) {
            addGraphEdge(m, `intent:${p.intent_id}`, `fact:${toFactSeq}`, "produces");
          }
          break;
        }
        case "fact_added": {
          const rawFactSeq = typeof p.fact_seq === "number" ? p.fact_seq : undefined;
          if (rawFactSeq !== undefined && rawFactSeq <= 0) break;
          const factSeq = rawFactSeq && rawFactSeq > 0 ? rawFactSeq : undefined;
          const row: BlackboardFact = {
            factSeq, fact: p.fact ?? "", verified: !!p.verified, confidence: p.confidence ?? 0,
            actor, verifier: p.verifier ?? "", witness: p.witness ?? undefined,
            artifactId: p.artifact_id ?? undefined, ts: ev.ts,
            // G0: the intent this fact was PRODUCED by (intent_products edge). The
            // backend now attaches every worker-produced fact to its intent and the
            // DB→bus bridge carries intent_id on fact_added; persist it so the canvas
            // draws the real produces-edge instead of guessing by time-window.
            intentId: p.intent_id ? String(p.intent_id) : undefined,
          };
          if (factSeq) {
            const existing = bb.facts.find((f) => f.factSeq === factSeq);
            bb.facts = existing
              ? bb.facts.map((f) => f.factSeq === factSeq
                  // never let a re-emit with a blank intent_id clobber a known one
                  ? { ...f, ...row, summary: f.summary, intentId: row.intentId ?? f.intentId }
                  : f)
              : [...bb.facts, row].slice(-100);
          } else {
            bb.facts = [...bb.facts, row].slice(-100);
          }
          tlabel(`${actor} ${p.verified ? "verified" : "candidate"}: ${(p.fact ?? "").slice(0, 56)}`);
          const factSolver = actor ? `solver:${actor}` : (sid ? `solver:${sid}` : undefined);
          if (factSolver) ensureSolverNode(m, actor || sid);
          const explicitIntent = p.intent_id ? String(p.intent_id) : "";
          const owner = explicitIntent
            ? bb.intents.find((i) => i.id === explicitIntent)
            : blackboardOwningIntent(bb, actor, ev.ts, factSeq);
          addGraphNode(m, p.verified ? "fact" : "candidate", p.fact ?? "", {
            id: factSeq ? `fact:${factSeq}` : undefined,
            from: owner ? `intent:${owner.id}` : factSolver,
            kind: owner ? "produces" : (p.verified ? "verifies" : "claims"),
            // carry actor (producing worker → engine colour ring) + owning intent
            // id (radar grouping) into meta — see the SHARED_GRAPH_DELTA path above.
            meta: {
              confidence: p.confidence, verifier: p.verifier, verified: p.verified,
              actor: actor || undefined,
              intentId: owner ? owner.id : undefined,
            },
          });
          break;
        }
        case "dead_end": {
          const rawDeadSeq = typeof p.dead_end_seq === "number" ? p.dead_end_seq : undefined;
          if (rawDeadSeq !== undefined && rawDeadSeq <= 0) break;
          const deadEndSeq = rawDeadSeq && rawDeadSeq > 0 ? rawDeadSeq : undefined;
          const reason = p.reason ?? "";
          const norm = (x: string) => x.toLowerCase().replace(/\s+/g, " ").trim();
          const row = { deadEndSeq, reason, actor, ts: ev.ts };
          const sameText = (d: typeof bb.deadEnds[number]) =>
            d.actor === actor && norm(d.reason) === norm(reason);
          const existing = deadEndSeq
            ? bb.deadEnds.find((d) => d.deadEndSeq === deadEndSeq || (!d.deadEndSeq && sameText(d)))
            : bb.deadEnds.find((d) => !d.deadEndSeq && sameText(d));
          bb.deadEnds = existing
            ? bb.deadEnds.map((d) =>
                (deadEndSeq
                  ? (d.deadEndSeq === deadEndSeq || (!d.deadEndSeq && sameText(d)))
                  : (!d.deadEndSeq && sameText(d)))
                  ? { ...d, ...row }
                  : d)
            : [...bb.deadEnds, row].slice(-50);
          tlabel(`${actor} dead-end: ${(p.reason ?? "").slice(0, 56)}`);
          // also grow a dead-end node on the FACT GRAPH (red octagon). Prefer hanging
          // it off the INTENT it refuted (so the intent-radar groups dead-ends with
          // their intent), falling back to the solver that ruled it out when no owning
          // intent is known — a false-positive still shows up either way.
          const deSolver = actor ? `solver:${actor}` : (sid ? `solver:${sid}` : undefined);
          const deOwner = p.intent_id
            ? bb.intents.find((i) => i.id === String(p.intent_id))
            : blackboardOwningIntent(bb, actor, ev.ts, deadEndSeq);
          if (!deOwner && deSolver) ensureSolverNode(m, actor || sid);
          addGraphNode(m, "dead_end", p.reason ?? "", {
            id: deadEndSeq ? `dead:${deadEndSeq}` : undefined,
            from: deOwner ? `intent:${deOwner.id}` : deSolver,
            kind: "refutes",
            meta: { actor: actor || undefined, intentId: deOwner ? deOwner.id : undefined },
          });
          break;
        }
        case "poc_saved": {
          const id = p.poc_id ?? gid("poc");
          const existing = bb.pocs.find((x) => x.id === id);
          const row: BlackboardPoc = {
            id,
            name: p.name ?? id,
            entryCommand: p.entry_command ?? "",
            status: p.status ?? "available",
            note: p.note ?? undefined,
            intentId: p.intent_id ?? undefined,
            artifactId: p.artifact_id ?? undefined,
            path: p.path ?? undefined,
            worker: actor || undefined,
            savedTs: existing?.savedTs ?? ev.ts,
            claimedTs: existing?.claimedTs,
            concludedTs: existing?.concludedTs,
          };
          bb.pocs = existing
            ? bb.pocs.map((x) => x.id === id ? { ...x, ...row } : x)
            : [...bb.pocs, row].slice(-80);
          tlabel(`${actor} saved PoC ${id} (${row.status})`);
          const pocFrom = row.intentId ? `intent:${row.intentId}` : (actor ? `solver:${actor}` : undefined);
          addGraphNode(m, "poc", row.entryCommand || row.name, {
            id: `poc:${id}`,
            from: pocFrom,
            kind: "saves",
            meta: { status: row.status, note: row.note, artifactId: row.artifactId },
          });
          break;
        }
        case "poc_claimed": {
          bb.pocs = bb.pocs.map((x) =>
            x.id === p.poc_id ? { ...x, status: "wip", worker: p.worker ?? actor, claimedTs: ev.ts } : x);
          tlabel(`${p.worker ?? actor} claimed PoC ${p.poc_id}`);
          break;
        }
        case "poc_concluded": {
          bb.pocs = bb.pocs.map((x) =>
            x.id === p.poc_id ? { ...x, status: p.status ?? "spent", note: p.note ?? x.note, concludedTs: ev.ts } : x);
          tlabel(`${actor} concluded PoC ${p.poc_id} → ${p.status ?? "spent"}`);
          const node = m.nodes.find((n) => n.id === `poc:${p.poc_id}`);
          if (node) {
            node.meta = { ...(node.meta || {}), status: p.status ?? "spent", note: p.note ?? node.meta?.note };
            m.version += 1;
          }
          break;
        }
        case "runtime_degraded":
        case "worker_backend_degraded": {
          tlabel(`${actor} runtime degraded: ${p.reason ?? p.backend ?? "unknown"}`);
          const sid = p.solver_id ?? p.engine ?? actor;
          if (sid) {
            const l = lane(s, sid);
            s.lanes[l.solverId] = { ...l, runtime: {
              backend: p.backend ?? p.requested_backend ?? "local",
              status: p.status ?? "degraded",
            } };
          }
          break;
        }
        case "engine_degraded": {
          // an engine was dropped from (or restored to) the run roster by a
          // dispatch-time health check. Track engine → reason so the worker panel /
          // engine bar can show "cursor degraded: Authentication required" instead
          // of the engine silently never appearing. status="recovered" clears it.
          const eng = String(p.engine ?? "");
          if (eng) {
            const next = { ...s.degradedEngines };
            if (p.status === "recovered") {
              delete next[eng];
              tlabel(`engine ${eng} recovered`);
            } else {
              next[eng] = String(p.reason ?? "unavailable");
              tlabel(`engine ${eng} degraded: ${p.reason ?? "unavailable"}`);
            }
            s.degradedEngines = next;
          }
          break;
        }
        case "phase_transition": {
          tlabel(`${p.from ?? "phase"} → ${p.to ?? "phase"}`);
          break;
        }
        case "worker_budget_exhausted":
        case "cost_budget_exhausted": {
          tlabel(`${p.kind ?? "budget"} exhausted`);
          break;
        }
        case "intent_reopened": {
          // a false-positive re-opened the intent — flip its status back to open
          // on the board and recolour the graph intent node.
          bb.intents = bb.intents.map((i) =>
            i.id === p.intent_id ? { ...i, status: "open", dispatchState: "active", closeReason: undefined, worker: undefined, concludedTs: undefined } : i);
          tlabel(`↻ reopened ${p.intent_id} (false positive)`);
          const inode = m.nodes.find((n) => n.id === `intent:${p.intent_id}`);
          if (inode) {
            inode.meta = { ...(inode.meta || {}), reopened: true };
            m.version += 1;
          }
          break;
        }
        case "fact_rejected":
        case "fact_superseded": {
          // A: a fact failed review — mark its lifecycle state so the board dims it
          // and verified/candidate selectors exclude it.
          const fs = typeof p.fact_seq === "number" ? p.fact_seq : undefined;
          const newState: FactLifecycleState = p.kind === "fact_rejected" ? "rejected" : "superseded";
          bb.facts = bb.facts.map((f) =>
            f.factSeq === fs ? { ...f, state: newState, verified: false } : f);
          tlabel(`${actor} ${newState} fact #${fs ?? "?"}`);
          if (fs !== undefined) {
            const fnode = m.nodes.find((n) => n.id === `fact:${fs}`);
            if (fnode) { fnode.meta = { ...(fnode.meta || {}), [newState]: true }; m.version += 1; }
          }
          break;
        }
        case "fact_merged": {
          const fromSeq = typeof p.from_fact_seq === "number" ? p.from_fact_seq : undefined;
          const intoSeq = typeof p.to_fact_seq === "number" ? p.to_fact_seq : undefined;
          bb.facts = bb.facts.map((f) =>
            f.factSeq === fromSeq ? { ...f, state: "merged", mergedInto: intoSeq, verified: false } : f);
          tlabel(`${actor} merged fact #${fromSeq ?? "?"} → #${intoSeq ?? "?"}`);
          if (fromSeq !== undefined) {
            const fnode = m.nodes.find((n) => n.id === `fact:${fromSeq}`);
            if (fnode) { fnode.meta = { ...(fnode.meta || {}), merged: true, mergedInto: intoSeq }; m.version += 1; }
          }
          break;
        }
        case "intent_state_changed": {
          // A/J: dispatch_state transition (finalize → resume/closed, revive → active).
          const ds = (p.dispatch_state ?? p.dispatchState) as IntentDispatchState | undefined;
          // a single delta can carry a comma-joined list of intent ids.
          const ids = String(p.intent_id ?? "").split(",").map((x) => x.trim()).filter(Boolean);
          const idSet = new Set(ids);
          bb.intents = bb.intents.map((i) =>
            idSet.has(i.id)
              ? { ...i, dispatchState: ds ?? i.dispatchState, closeReason: p.close_reason ?? p.stop_reason ?? i.closeReason }
              : i);
          if (ds) tlabel(`${ids.length} intent(s) → ${ds}`);
          break;
        }
        case "operator_directive_changed": {
          // B: upsert the directive state chain + surface it as a chat bubble.
          const did = String(p.directive_id ?? p.id ?? "");
          if (did) {
            const row: OperatorDirective = {
              id: did,
              text: p.text ?? "",
              action: p.action ?? "directive",
              status: (p.status ?? "received") as DirectiveStatus,
              preemption: (p.preemption ?? p.preempt_policy) as DirectivePreemption | undefined,
              boundWorker: p.bound_worker ?? p.boundWorker ?? undefined,
              ts: ev.ts,
            };
            const existing = s.operatorDirectives.find((d) => d.id === did);
            s.operatorDirectives = existing
              ? s.operatorDirectives.map((d) => d.id === did ? { ...d, ...row, text: row.text || d.text } : d)
              : [...s.operatorDirectives, row];
            // first time we see it bound/received, announce it in the thread.
            if (!existing) {
              pushChat(s, { role: "system", kind: "guidance",
                content: `operator ${row.action}: ${row.text}`, ts: ev.ts });
            }
          }
          tlabel(`operator ${p.action ?? "directive"} → ${p.status ?? "?"}`);
          break;
        }
        case "hitl_classified": {
          // F: annotate the matching hand-raise with its triage; auto-resolving
          // kinds (non external_blocker) are silently dismissed from the card stack.
          const nk = (p.need_kind ?? p.needKind) as HitlNeedKind | undefined;
          const w = String(p.worker ?? actor ?? "");
          const pauses = nk ? nk === "external_blocker" : true;
          s.hitlRequests = s.hitlRequests
            .map((r) => (r.worker === w && (!r.needKind))
              ? { ...r, needKind: nk, pausesBehavior: pauses } : r)
            // auto-resolving kinds don't need an operator decision — drop their cards
            .filter((r) => !(r.worker === w && nk && nk !== "external_blocker" && !r.pausesBehavior));
          tlabel(`${w} hand-raise classified: ${nk ?? "external_blocker"}`);
          break;
        }
        case "resource_lock_changed": {
          // E: a unified resource lock was acquired/released/expired/denied.
          const lid = String(p.lock_id ?? p.lockId ?? "");
          const status = (p.status ?? "active") as ResourceLock["status"];
          if (lid) {
            const row: ResourceLock = {
              lockId: lid,
              resourceKey: p.resource_key ?? p.resourceKey ?? lid,
              scope: p.scope ?? "activity",
              riskClass: p.risk_class ?? p.riskClass ?? undefined,
              status,
              ownerWorker: p.owner_worker ?? p.ownerWorker ?? actor ?? undefined,
              ts: ev.ts,
            };
            const idx = s.resourceLocks.findIndex((l) => l.lockId === lid);
            if (status === "released" || status === "expired" || status === "denied") {
              s.resourceLocks = s.resourceLocks.filter((l) => l.lockId !== lid);
            } else {
              s.resourceLocks = idx >= 0
                ? s.resourceLocks.map((l) => l.lockId === lid ? { ...l, ...row } : l)
                : [...s.resourceLocks, row];
            }
          }
          tlabel(`resource ${p.status ?? "lock"}: ${(p.resource_key ?? "").slice(0, 48)}`);
          break;
        }
        case "flag_invalidated": {
          // the recovered flag was wrong — drop it from the board and strike the
          // flag node on the graph. multi-flag: remove only THIS flag from the set.
          if (bb.flag === p.flag) bb.flag = undefined;
          if (p.flag) {
            s.flags = s.flags.filter((f) => f !== p.flag);
            if (s.flag === p.flag) s.flag = s.flags[0];
          }
          bb.flags = [...s.flags]; // keep the board's flag set in sync after a drop
          if (!bb.flag) bb.flag = bb.flags[0];
          tlabel(`flag invalidated: ${(p.flag ?? "").slice(0, 40)}`);
          // strike ONLY the node for THIS flag (multi-flag: leave the others). The
          // flag value lives in label (clipped) AND meta.raw (full) — after
          // node.summarized the label is a zh gist so meta.raw is authoritative; the
          // node id is `flag:<value>` for multi, plain "flag" for single.
          if (p.flag) {
            for (const n of m.nodes) {
              if (
                n.type === "flag" &&
                (n.meta?.raw === p.flag || n.label === p.flag || n.id === `flag:${p.flag}`)
              ) {
                n.meta = { ...(n.meta || {}), invalidated: true };
                m.version += 1;
              }
            }
          }
          break;
        }
        case "flag_found": {
          bb.flag = p.flag ?? bb.flag;
          // multi-flag: collect on the deck in real time so the UI shows N/total
          // as flags land, not just at run end.
          mergeFlags(s, p.flag);
          bb.flags = [...s.flags]; // mirror the deduped flag set onto the blackboard
          tlabel(`${actor} FLAG ${p.flag ?? ""}`);
          break;
        }
        case "need_input": {
          // a worker flagged it needs operator input (mirror of the HITL_REQUEST).
          tlabel(`needs input: ${(p.need ?? "").toString().slice(0, 80)}`);
          break;
        }
        case "awaiting_operator": {
          // the coordinator PAUSED — it will not re-spawn until the operator acts.
          const ask = (p.reason ?? "waiting for operator input").toString();
          s.awaitingOperator = ask;
          tlabel(`awaiting operator: ${ask.slice(0, 80)}`);
          pushChat(s, {
            role: "system", kind: "status",
            content: `Paused — waiting for you: ${ask}`, ts: ev.ts,
          });
          break;
        }
        case "operator_paused": {
          const ask = (p.reason ?? "operator paused").toString();
          s.awaitingOperator = ask;
          tlabel(`operator paused: ${ask.slice(0, 80)}`);
          pushChat(s, {
            role: "system", kind: "status",
            content: "Paused by operator.", ts: ev.ts,
          });
          break;
        }
        case "operator_resumed": {
          s.awaitingOperator = undefined;
          tlabel("▶ operator responded — resuming");
          pushChat(s, { role: "system", kind: "status",
            content: "▶ Resuming with your input.", ts: ev.ts });
          break;
        }
        case "worker_spawned": {
          // a worker joined (bootstrap / explore / rebootstrap / operator). The
          // real worker id is in `worker`; register it for lanes/legend. Lane
          // presence flips via WORKER_STATUS; here we just narrate + remember it.
          const w = p.worker ?? actor;
          if (w && !bb.workers.includes(w)) bb.workers = [...bb.workers, w];
          if (w) {
            const phase = (p.phase ?? "").toString() || undefined;
            const l = lane(s, w);
            s.lanes[l.solverId] = {
              ...l,
              role: roleFromWorkerRole(p.worker_role) ?? reviewRoleFromPhase(phase) ?? l.role,
              phase: phase ?? l.phase,
              statusReason: phase ?? l.statusReason,
            };
          }
          tlabel(`+ ${w} spawned${p.phase ? ` (${p.phase})` : ""}`);
          break;
        }
        case "review_started": {
          const w = p.worker ?? actor;
          if (w && !bb.workers.includes(w)) bb.workers = [...bb.workers, w];
          if (w) {
            const l = lane(s, w);
            s.lanes[l.solverId] = {
              ...l,
              role: "review",
              phase: "review",
              status: "reviewing",
              online: true,
              statusReason: p.trigger ?? "review",
            };
          }
          tlabel(`review started${p.trigger ? ` (${p.trigger})` : ""}`);
          break;
        }
        case "review_finished": {
          const w = p.worker ?? actor;
          if (w && s.lanes[w]) {
            s.lanes[w] = {
              ...s.lanes[w],
              role: "review",
              phase: s.lanes[w].phase ?? "review",
              status: "finished",
              online: false,
              statusReason: "review_finished",
            };
          }
          tlabel(`review finished${w ? `: ${w}` : ""}`);
          break;
        }
        case "review_finding": {
          const id = String(p.finding_id ?? p.seq ?? gid("rvw"));
          const row: BlackboardReviewFinding = {
            id,
            kind: String(p.finding_kind ?? "finding"),
            severity: String(p.severity ?? "info"),
            summary: String(p.summary ?? ""),
            routeHash: p.route_hash ? String(p.route_hash) : undefined,
            branchId: p.branch_id ? String(p.branch_id) : undefined,
            actor,
            ts: ev.ts,
          };
          bb.reviewFindings = [...bb.reviewFindings, row].slice(-80);
          tlabel(`review ${row.severity}: ${row.summary.slice(0, 72)}`);
          break;
        }
        case "fact_challenged": {
          const factSeq = Number(p.fact_seq || 0);
          const reason = String(p.reason ?? "");
          bb.facts = bb.facts.map((f) =>
            f.factSeq === factSeq ? { ...f, challenged: true, revalidated: false, challengeReason: reason } : f);
          tlabel(`fact #${factSeq} challenged: ${reason.slice(0, 64)}`);
          const node = factSeq > 0 ? m.nodes.find((n) => n.id === `fact:${factSeq}`) : undefined;
          if (node) {
            node.meta = { ...(node.meta || {}), challenged: true, challengeReason: reason };
            m.version += 1;
          }
          break;
        }
        case "fact_revalidated": {
          const factSeq = Number(p.fact_seq || 0);
          bb.facts = bb.facts.map((f) =>
            f.factSeq === factSeq ? { ...f, challenged: false, revalidated: true, challengeReason: undefined } : f);
          tlabel(`fact #${factSeq} revalidated`);
          const node = factSeq > 0 ? m.nodes.find((n) => n.id === `fact:${factSeq}`) : undefined;
          if (node) {
            node.meta = { ...(node.meta || {}), challenged: false, revalidated: true };
            m.version += 1;
          }
          break;
        }
        case "route_suppressed": {
          const routeHash = String(p.route_hash ?? "");
          const row: BlackboardSuppressedRoute = {
            routeHash,
            label: p.label ? String(p.label) : undefined,
            reason: String(p.reason ?? ""),
            actor,
            ts: ev.ts,
            reopened: false,
          };
          bb.suppressedRoutes = [
            ...bb.suppressedRoutes.filter((r) => r.routeHash !== routeHash),
            row,
          ].slice(-60);
          tlabel(`route suppressed: ${routeHash}${row.reason ? ` · ${row.reason.slice(0, 48)}` : ""}`);
          const from = actor ? `solver:${actor}` : undefined;
          if (from) ensureSolverNode(m, actor);
          addGraphNode(m, "dead_end", `suppressed ${routeHash}: ${row.reason}`, {
            id: `route:${routeHash}`,
            from,
            kind: "suppresses",
            meta: { routeHash, reason: row.reason },
          });
          break;
        }
        case "route_reopened": {
          const routeHash = String(p.route_hash ?? "");
          bb.suppressedRoutes = bb.suppressedRoutes.map((r) =>
            r.routeHash === routeHash ? { ...r, reopened: true } : r);
          tlabel(`route reopened: ${routeHash}`);
          break;
        }
        case "branch_split": {
          const branchId = String(p.branch_id ?? p.parent ?? "");
          bb.branches = [...bb.branches, {
            branchId,
            title: String(p.title ?? branchId),
            actor,
            ts: ev.ts,
            status: "open",
          }].slice(-60);
          tlabel(`branch split: ${String(p.title ?? branchId).slice(0, 72)}`);
          break;
        }
        case "branch_resolved": {
          const ids = [p.branch_id, ...(Array.isArray(p.resolved) ? p.resolved : [])]
            .map((x) => String(x ?? "").trim())
            .filter(Boolean);
          const idSet = new Set(ids);
          bb.branches = bb.branches.map((b) =>
            idSet.has(b.branchId) ? { ...b, status: "resolved" } : b);
          tlabel(`branch resolved: ${ids.join(", ") || "branch"}`);
          break;
        }
        case "coordinator_directive": {
          const directive = String(p.directive ?? "");
          const action = String(p.action ?? "note");
          bb.directives = [...bb.directives, { action, directive, actor, ts: ev.ts }].slice(-60);
          tlabel(`directive ${action}: ${directive.slice(0, 72)}`);
          if (directive) {
            pushChat(s, { role: "system", kind: "status",
              content: `Review directive (${action}): ${directive}`, ts: ev.ts });
          }
          break;
        }
        case "worker_killed": {
          // operator stopped a specific worker — mark its lane offline so the dock
          // greys it immediately (WORKER_FINISHED also lands, but may lag the kill).
          const w = p.worker;
          if (w && s.lanes[w]) {
            s.lanes[w] = { ...s.lanes[w], online: false, status: "killed", statusReason: "killed" };
          }
          tlabel(`${w ?? actor} killed`);
          break;
        }
        case "worker_spawn_rejected": {
          const why = p.reason === "max_workers"
            ? "at max workers" : p.reason === "unknown_engine"
            ? `engine not in roster${p.engine ? ` (${p.engine})` : ""}` : (p.reason ?? "rejected");
          tlabel(`spawn rejected: ${why}`);
          pushChat(s, { role: "system", kind: "status",
            content: `Could not add worker — ${why}.`, ts: ev.ts });
          break;
        }
        case "worker_finished": {
          // coordinator-level reap narration (the lane itself is handled by the
          // WORKER_FINISHED event); just add a timeline line.
          tlabel(`− ${p.worker ?? actor} finished${p.result ? ` (${p.result})` : ""}`);
          break;
        }
        case "goal_complete": {
          // pentest: the engagement goal was judged met (no flag). Mark the run
          // solved-by-goal and stash the rationale for the outcome panel.
          s.solved = true;
          s.outcomeReason = "goal_met";
          if (p.why) s.goalWhy = String(p.why);
          tlabel(`goal complete${p.why ? `: ${String(p.why).slice(0, 60)}` : ""}`);
          pushChat(s, { role: "system", kind: "status",
            content: `Goal met — ${p.why ?? "engagement objective reached"}`,
            ts: ev.ts, i18nKey: "sys.goalMet", i18nVars: { why: String(p.why ?? "") } });
          break;
        }
        case "budget_exhausted": {
          tlabel(`budget exhausted (${p.elapsed ?? "?"}s)`);
          pushChat(s, { role: "system", kind: "status",
            content: "Wall-clock budget exhausted.", ts: ev.ts,
            i18nKey: "sys.budgetExhausted" });
          break;
        }
        case "reason_start": {
          tlabel(`reasoning${p.trigger ? ` (${p.trigger})` : ""}`);
          break;
        }
        case "reason_done": {
          const dup = Number(p.dropped_dup ?? 0);
          tlabel(
            `reason → ${p.proposed ?? 0} intent(s)` +
              (dup > 0 ? ` (${dup} dup dropped)` : "")
          );
          break;
        }
        case "race_started": {
          // the front race-scout round started: N engines probe the whole challenge
          // in parallel (single-shot) before the main coordinator loop. Show a status pill.
          s.racing = true;
          const engines = Array.isArray(p.engines) ? p.engines.join(", ") : "";
          tlabel(`race scout started${engines ? `: ${engines}` : ""}`);
          pushChat(s, { role: "system", kind: "status",
            content: `Race scout — ${engines || "engines"} probing in parallel`,
            ts: ev.ts, i18nKey: "sys.raceStarted", i18nVars: { engines } });
          break;
        }
        case "race_concluded": {
          // the race round ended: either it captured the flag (fast path) or its
          // facts go to the main coordinator loop. Clear the pill; the run-level events narrate
          // the outcome (solved / continuing).
          s.racing = false;
          const n = Number(p.flags ?? 0);
          tlabel(`race scout concluded (${p.solved ? "flag" : `${n} flag(s)`})`);
          if (!p.solved) {
            pushChat(s, { role: "system", kind: "status",
              content: "Race scout found no flag — handing facts to the planner",
              ts: ev.ts, i18nKey: "sys.raceHandoff" });
          }
          break;
        }
        default:
          break;
      }
      break;
    }
    case EventType.NODE_SUMMARIZED: {
      // a zh gist for a fact/intent node landed: swap the graph node's label to
      // the gist (keep the raw in meta.raw) and store summary on the bb card.
      const summary = (p.summary ?? "").trim();
      if (!summary) break;
      const nodeId = p.node_kind === "intent"
        ? `intent:${p.intent_id}`
        : (typeof p.fact_seq === "number" && p.fact_seq > 0 ? `fact:${p.fact_seq}` : "");
      if (nodeId) {
        const m = s.model;
        const i = m.nodes.findIndex((n) => n.id === nodeId);
        if (i >= 0) {
          const n = m.nodes[i];
          m.nodes[i] = { ...n, label: summary, meta: { ...(n.meta || {}), summary, raw: n.meta?.raw ?? n.label } };
          m.version += 1;
        }
      }
      const bb = s.blackboard;
      if (p.node_kind === "intent") {
        bb.intents = bb.intents.map((it) => it.id === p.intent_id ? { ...it, summary } : it);
      } else if (typeof p.fact_seq === "number" && p.fact_seq > 0) {
        bb.facts = bb.facts.map((f) => f.factSeq === p.fact_seq ? { ...f, summary } : f);
        // Older cards may not have a factSeq (rehydrated from early streams), so
        // also match by the raw text we stored on the graph node.
        const raw = s.model.nodes.find((n) => n.id === `fact:${p.fact_seq}`)?.meta?.raw;
        if (raw) bb.facts = bb.facts.map((f) => f.fact === raw ? { ...f, summary } : f);
      }
      break;
    }
    case EventType.CONTEXT_STATE:
      s.gauge = { zones: p.zones ?? [], total: p.total ?? 0, limit: p.limit ?? 0 };
      break;
    case EventType.COST_UPDATE: {
      // The payload carries the CUMULATIVE ledger totals for the scope it was
      // emitted at (the backend emits the most specific scope available — almost
      // always per-solver, since reason/coordinator/CLI calls all carry a
      // solver_id). usd and the token counts move together in one payload.
      const cu = {
        usd: typeof p.usd === "number" ? p.usd : 0,
        tokensIn: typeof p.input_tokens === "number" ? p.input_tokens : 0,
        tokensOut: typeof p.output_tokens === "number" ? p.output_tokens : 0,
      };
      if (p.scope === "solver" && sid) {
        // store this agent's running total, then re-sum across agents for the
        // headline. The lane's engine tags the row in the hover card.
        s.costBySolver = {
          ...s.costBySolver,
          [sid]: { ...cu, engine: s.lanes[sid]?.engine },
        };
        const all = Object.values(s.costBySolver);
        s.usd = all.reduce((a, c) => a + c.usd, 0);
        s.tokensIn = all.reduce((a, c) => a + c.tokensIn, 0);
        s.tokensOut = all.reduce((a, c) => a + c.tokensOut, 0);
      } else {
        // global/challenge scope (rare) — take the payload as the headline total,
        // but never let it shrink below the per-agent sum we already have.
        const sum = Object.values(s.costBySolver);
        s.usd = Math.max(cu.usd, sum.reduce((a, c) => a + c.usd, 0));
        s.tokensIn = Math.max(cu.tokensIn, sum.reduce((a, c) => a + c.tokensIn, 0));
        s.tokensOut = Math.max(cu.tokensOut, sum.reduce((a, c) => a + c.tokensOut, 0));
      }
      break;
    }
    case EventType.GUIDANCE_INJECTED:
      // the solver acknowledged a human command landed in its context
      pushChat(s, { role: "system", kind: "guidance", content: p.note ?? "(human guidance applied)", ts: ev.ts });
      break;
    case EventType.HITL_REQUEST: {
      // a worker raised its hand: it needs a resource (need_input) or the env is
      // down (env_down). payload = {worker, need, kind}. Fall back to the older
      // prompt/options shape if present.
      const need = (p.need ?? p.prompt ?? "needs your input").toString();
      const isEnv = p.kind === "env_down";
      const needKind = (p.need_kind ?? p.needKind) as HitlNeedKind | undefined;
      s.hitlRequests = [...s.hitlRequests, {
        id: p.id ?? gid("hitl"),
        prompt: need,
        worker: p.worker ? String(p.worker) : undefined,
        options: p.options ?? [],
        needKind,
        // F: only an external_blocker freezes the swarm for an answer; the rest
        // auto-resolve (lane lock / route suppress / candidate). default-on for
        // back-compat (no need_kind → treat as a blocker, as before).
        pausesBehavior: needKind ? needKind === "external_blocker" : true,
        ts: ev.ts,
      }];
      const lead = isEnv ? "environment problem" : "needs input";
      pushChat(s, {
        role: "agent", solverId: sid || undefined, kind: "text",
        content: `${lead}: ${need}`, ts: ev.ts,
      });
      break;
    }
    case EventType.HITL_RESPONSE: {
      // human command was accepted by the backend — echo as a human chat bubble
      // (this is what makes the operator's reply show up as normal history that
      // scrolls up into the coordinator thread).
      const t = p.text ?? p.hint ?? "";
      pushChat(s, { role: "human", kind: "guidance", content: `/${p.action ?? "hint"}${t ? " " + t : ""}  → ${p.target ?? "global"}`, ts: ev.ts });
      // ANY operator reply resolves the pending decision card: the backend wakes
      // the swarm on every operator command (not just submit/answer — a hint /
      // redirect / stop also clears _pending_help and sets _operator_event), so
      // the blocking "needs your decision" card must clear too. Before this it
      // only cleared on submit/answer, so replying with a /hint (e.g. handing the
      // worker a VPS) left the card stuck on screen even though the run resumed.
      // The reply itself stays as the human bubble above (history); the card just
      // stops blocking.
      s.hitlRequests = [];
      if (p.action !== "pause") {
        s.awaitingOperator = undefined;
      }
      break;
    }
    case EventType.HITL_TRANSLATED: {
      // a zh translation of a worker's hand-raise arrived (async). Match it to the
      // pending card by (worker, raw need) and attach promptZh so the card swaps to
      // Chinese; the raw English stays available. Eventual-consistency: the card was
      // rendered first from HITL_REQUEST, this just enriches it.
      const w = p.worker ? String(p.worker) : "";
      const raw = String(p.need ?? "");
      const zh = String(p.need_zh ?? "");
      if (zh) {
        s.hitlRequests = s.hitlRequests.map((r) =>
          (r.worker === w && r.prompt === raw) ? { ...r, promptZh: zh } : r);
      }
      break;
    }
    case EventType.GRAPH_COMPACTED: {
      // H: a long-run compaction epoch landed — bump the counter + announce it.
      s.compactEpochs = (s.compactEpochs ?? 0) + 1;
      s.lastCompactTs = ev.ts;
      const retired = typeof p.retired_intents === "number" ? p.retired_intents : 0;
      pushChat(s, { role: "system", kind: "guidance",
        content: `graph compacted (${p.trigger ?? "no progress"}) — retired ${retired} stale intent(s)`,
        ts: ev.ts });
      break;
    }
    case EventType.WORKER_FINISHED: {
      // ONE swarm sub-worker ended — worker-level, NOT the run. The coordinator
      // keeps re-bootstrapping until solved/stopped, so we must NOT set s.finished
      // here (that was the run-7345 "怎么又结束了" bug: a worker ending made the
      // whole deck read 'finished' while the run was still going). Only update THIS
      // worker's lane; presence already flipped via WORKER_STATUS. If this worker
      // actually found the flag, reflect that on its lane + graft the flag node, and
      // record the flag/solved on the deck — but leave s.finished to RUN_FINISHED.
      const wf = (p.flags as string[] | undefined) ?? (p.flag ? [p.flag] : []);
      if (wf.length) {
        mergeFlags(s, wf);
        s.solved = !!p.solved || s.solved;
        const winner = sid ? `solver:${sid}` : undefined;
        const latestFact = [...m.nodes]
          .reverse()
          .find((n) => (n.type === "fact" || n.type === "candidate") && n.meta?.verified !== false);
        // graft a node per distinct flag this worker landed
        wf.forEach((f, i) => {
          addGraphNode(m, "flag", f, {
            id: wf.length > 1 ? `flag:${f}` : "flag",
            from: latestFact?.id ?? winner,
            kind: "solves",
            meta: { winner: sid || undefined },
          });
        });
      }
      if (ev.solver_id) {
        const l = lane(s, ev.solver_id);
        s.lanes[l.solverId] = {
          ...l,
          solved: !!p.solved || l.solved,
          flag: p.flag ?? l.flag,
          status: p.solved ? "SOLVED" : l.solved ? "SOLVED" : "done",
          online: false,
          statusReason: p.solved ? "solved" : "finished",
        };
      }
      if (p.solved) {
        pushChat(s, { role: "system", kind: "status", content: `SOLVED — ${p.flag ?? ""}`, ts: ev.ts, i18nKey: "sys.solved", i18nVars: { flag: p.flag ?? "" } });
      }
      break;
    }
    case EventType.RUN_FINISHED: {
      if (!s.started) {
        s.started = true;
        if (s.startedAt == null) s.startedAt = ev.ts;
      }
      s.finished = true;
      s.awaitingOperator = undefined;
      s.hitlRequests = [];
      // Keep the FIRST finish ts of the current run cycle — the backend re-emits
      // run.finished (with a fresh ts) when a finished run is reloaded/reconnected,
      // which would otherwise stretch the duration to "load time". A genuine
      // re-open fires RUN_STARTED first, which clears finishedAt, so the next
      // finish is captured correctly.
      if (s.finishedAt == null) s.finishedAt = ev.ts;
      mergeFlags(s, (p.flags as string[] | undefined) ?? p.flag);
      if (typeof p.expected_flags === "number") s.expectedFlags = p.expected_flags;
      if (typeof p.multi_flag === "boolean") s.multiFlag = p.multi_flag;
      s.solved = !!p.solved || s.solved;
      // classify the outcome so the UI shows the right thing: a gated flag →
      // "solved"; solved without a flag → pentest "goal_met"; else "finished".
      // The backend may also send payload.reason; goal_complete may have set it
      // already, in which case we don't downgrade it.
      const finishReason = String(p.reason ?? "");
      const finishDetail = String(p.detail ?? p.error ?? "").trim();
      s.outcomeReason = s.flag
        ? "solved"
        : (finishReason === "goal_met" || s.solved || s.outcomeReason === "goal_met")
          ? "goal_met"
          : (finishReason === "operator_stop" || finishReason === "budget_exhausted" || finishReason === "runtime_failure")
            ? finishReason
            : "finished";
      s.outcomeDetail = finishDetail || s.outcomeDetail;
      if (p.flag) {
        const winner = sid ? `solver:${sid}` : undefined;
        const latestFact = [...m.nodes]
          .reverse()
          .find((n) => (n.type === "fact" || n.type === "candidate") && n.meta?.verified !== false);
        addGraphNode(m, "flag", p.flag, {
          id: "flag",
          from: latestFact?.id ?? winner,
          kind: "solves",
          meta: { winner: sid || undefined },
        });
      }
      if (ev.solver_id) {
        const l = lane(s, ev.solver_id);
        // Don't let a run-level RUN_FINISHED (which the coordinator may emit with
        // p.solved=false even after a lane already solved) wipe a lane's solved
        // boolean — preserve it with `|| l.solved` (mock/race/standby finishes).
        const stillSolved = !!p.solved || l.solved;
        s.lanes[l.solverId] = {
          ...l,
          solved: stillSolved,
          flag: p.flag ?? l.flag,
          status: stillSolved ? "SOLVED" : "done",
          online: false,
          statusReason: stillSolved ? "solved" : (finishReason || "finished"),
        };
      }
      for (const id of Object.keys(s.lanes)) {
        const l = s.lanes[id];
        const isWinner = !!ev.solver_id && id === ev.solver_id;
        s.lanes[id] = {
          ...l,
          status: (isWinner && p.solved) || l.solved ? "SOLVED" : "done",
          online: false,
          statusReason: isWinner
            ? (p.solved ? "solved" : (finishReason || "finished"))
            : (l.statusReason && l.online === false ? l.statusReason : (finishReason || "finished")),
        };
      }
      if (p.solved && s.flag) {
        pushChat(s, {
          role: "system", kind: "status", content: `SOLVED — ${p.flag ?? ""}`,
          ts: ev.ts, i18nKey: "sys.solved", i18nVars: { flag: p.flag ?? "" },
        });
      } else if (p.solved) {
        pushChat(s, {
          role: "system", kind: "status",
          content: `Goal met — ${s.goalWhy ?? "engagement objective reached"}`,
          ts: ev.ts, i18nKey: "sys.goalMet", i18nVars: { why: s.goalWhy ?? "" },
        });
      } else if (s.outcomeReason === "runtime_failure") {
        pushChat(s, {
          role: "system", kind: "status",
          content: `Run failed — ${s.outcomeDetail || "runtime failure"}`,
          ts: ev.ts, i18nKey: "sys.runtimeFailure",
          i18nVars: { detail: s.outcomeDetail || "runtime failure" },
        });
      } else {
        pushChat(s, {
          role: "system", kind: "status", content: "Run finished (no flag)",
          ts: ev.ts, i18nKey: "sys.finishedNoFlag",
        });
      }
      // 刀2: defensively demote any still-active open intents at run finish so they
      // stop reading as "in flight". The backend now emits intent_state_changed for
      // finalize, but this guards missed deltas and older runs replayed from JSONL.
      // Solved → closed; otherwise held as resume (matches the DB finalize sweep).
      if (s.blackboard?.intents?.length) {
        const ds: IntentDispatchState = p.solved ? "closed" : "resume";
        s.blackboard.intents = s.blackboard.intents.map((i) =>
          i.status !== "done" && (i.dispatchState ?? "active") === "active"
            ? { ...i, dispatchState: ds }
            : i);
      }
      break;
    }
    case EventType.RUN_REOPENED: {
      // The same lifecycle event reopens a run for either "continue solving" or a
      // false-positive flag invalidation. Keep the operator copy precise.
      s.finished = false;
      s.solved = false;
      s.awaitingOperator = undefined;
      s.hitlRequests = [];
      const reopenedForResolve = p.reason === "resolve";
      if (reopenedForResolve) {
        // Continue solving from the same evidence graph: keep already recovered
        // flags visible and let new worker prompts inherit them.
      } else if (p.flag && s.flags.length) {
        s.flags = s.flags.filter((f) => f !== p.flag);
      } else {
        s.flags = [];
      }
      s.flag = s.flags[0];
      s.outcomeReason = undefined;
      s.outcomeDetail = undefined;
      s.goalWhy = undefined;
      pushChat(s, { role: "system", kind: "status",
        content: reopenedForResolve ? "↻ continuing solve" : "↻ flag marked false — re-solving",
        ts: ev.ts,
        i18nKey: reopenedForResolve ? "sys.resolveReopened" : "sys.reopened",
        i18nVars: { flag: p.flag ?? "" } });
      break;
    }
    default:
      break;
  }
  return s;
}

// ============================================================================
// Derived selectors — coordinator ↔ worker split + run digest.
//
// The conversation-first deck renders the MAIN thread as operator ↔ COORDINATOR
// (the DeepSeek `reason` actor: planning / auditing / verdicts) and pushes the
// WORKER firehose (cli-claude / cli-codex / cursor shell loops) into secondary
// panels + the right-column inspector. The split is by `solver_id`, so it needs
// NO backend change — see AGENTS.md (reason = coordinator, cli-* = workers).
// ============================================================================

/** solver_ids that are the COORDINATOR, not a shell worker. */
export const COORDINATOR_IDS = new Set(["reason", "coordinator"]);

/** Is this solver id a shell WORKER (vs the coordinator / unscoped)? */
export function isWorkerLane(id?: string | null): boolean {
  return !!id && !COORDINATOR_IDS.has(id);
}

/** Review/arbiter workers are shell workers with a review phase/role. Keep the
 *  predicate centralized so the roster, worker lanes, and timeline do not drift. */
export function isReviewWorkerLane(lane?: Pick<SolverLane, "role" | "phase" | "statusReason"> | null): boolean {
  if (!lane) return false;
  if (lane.role === "review") return true;
  const phase = (lane.phase || lane.statusReason || "").toLowerCase();
  return phase.includes("review");
}

/** A chat message belongs to the main (coordinator) thread when it's operator
 *  input, a system lifecycle line, or coordinator/unscoped agent text. Worker
 *  agent bubbles are excluded — they live in the secondary worker panels. */
export function isCoordinatorMessage(m: ChatMessage): boolean {
  if (m.role === "human" || m.role === "system") return true;
  if (m.mainThread) return true;
  return !m.solverId || COORDINATOR_IDS.has(m.solverId);
}

export function coordinatorThread(deck: DeckState): ChatMessage[] {
  return deck.chat.filter(isCoordinatorMessage);
}

/** All worker agent bubbles (reasoning/text/tool), in time order. */
export function workerChat(deck: DeckState): ChatMessage[] {
  return deck.chat.filter((m) => m.role === "agent" && isWorkerLane(m.solverId));
}

/** Lanes for actual shell workers (drops the coordinator `reason` lane). */
export function workerLanes(deck: DeckState): SolverLane[] {
  return Object.values(deck.lanes).filter((l) => isWorkerLane(l.solverId));
}

/** Distinct worker ids seen across lanes AND chat (a worker that only streamed
 *  text before a WORKER_STATUS lane was created still appears). */
export function workerIds(deck: DeckState): string[] {
  const ids = new Set<string>();
  for (const l of workerLanes(deck)) ids.add(l.solverId);
  for (const m of workerChat(deck)) if (m.solverId) ids.add(m.solverId);
  return Array.from(ids);
}

function uniqueTexts(values: string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of values) {
    const v = (raw || "").replace(/\s+/g, " ").trim();
    if (!v || seen.has(v)) continue;
    seen.add(v);
    out.push(v);
  }
  return out;
}

// A: rejected/merged/superseded facts are retired by review — never counted as
// verified or candidate evidence (they leave the planner/worker view).
const FACT_RETIRED: ReadonlySet<string> = new Set(["rejected", "merged", "superseded"]);
export function isFactRetired(f: BlackboardFact): boolean {
  return !!f.state && FACT_RETIRED.has(f.state);
}

export function verifiedFactTexts(deck: DeckState): string[] {
  const bb = deck.blackboard.facts
    .filter((f) => f.verified && !isFactRetired(f))
    .map((f) => f.summary || f.fact);
  return uniqueTexts(bb.length ? bb : deck.sharedGraph.verified.map((e) => e.fact));
}

export function candidateFactTexts(deck: DeckState): string[] {
  const bb = deck.blackboard.facts
    .filter((f) => !f.verified && !isFactRetired(f))
    .map((f) => f.summary || f.fact);
  return uniqueTexts(bb.length ? bb : deck.sharedGraph.candidates.map((e) => e.fact));
}

export function openIntentTexts(deck: DeckState): string[] {
  const doneIds = new Set(deck.blackboard.intents.filter((i) => i.status === "done").map((i) => i.id));
  const values = deck.blackboard.intents
    // A/J: only dispatchable (active) directions are "open" — resume/retired/closed
    // are held back (undefined dispatchState defaults to active for back-compat).
    .filter((i) => i.status !== "done" && (i.dispatchState ?? "active") === "active")
    .map((i) => i.summary || i.goal);
  for (const it of deck.reason.intents) if (!doneIds.has(it.id)) values.push(it.goal);
  return uniqueTexts(values);
}

export function deadEndTexts(deck: DeckState): string[] {
  return uniqueTexts([
    ...deck.blackboard.deadEnds.map((d) => d.reason),
    ...deck.graph.deadEnds,
  ]);
}

/** A rolling, synthesized status of the swarm — drives the coordinator thread's
 *  "progress" / "answer" turns and the quiet-meta strip WITHOUT any new event
 *  (computed purely from already-folded state). */
export interface SwarmDigest {
  phase: "draft" | "racing" | "running" | "collecting" | "paused" | "solved" | "goal_met" | "finished";
  verified: number;
  candidates: number;
  openIntents: number;
  deadEnds: number;
  onlineWorkers: number;
  totalWorkers: number;
  latestVerified?: string;
  flag?: string;
  flags: string[];
  expectedFlags: number;
  goalWhy?: string;
  usd: number;
  tokensIn: number;
  tokensOut: number;
  // per-agent cost/token breakdown for the hover card (solverId → totals).
  costBySolver: Record<string, SolverCost>;
  // run wall-clock bookends (raw event ts). The component ticks live elapsed off
  // startedAt while the run is open; once finishedAt is set the duration freezes.
  startedAt?: number;
  finishedAt?: number;
}

export function swarmDigest(deck: DeckState): SwarmDigest {
  const verified = verifiedFactTexts(deck);
  const candidates = candidateFactTexts(deck);
  const intents = openIntentTexts(deck);
  const deads = deadEndTexts(deck);
  const lanes = workerLanes(deck);
  const ids = workerIds(deck);
  const online = ids.filter((id) => (deck.lanes[id]?.online ?? !deck.finished) !== false).length;
  const need = Math.max(1, deck.expectedFlags || 1);
  // collect mode with an UNKNOWN count (multiFlag + need<=1): a saved flag never
  // auto-completes the run — it stays "collecting" while live (decouples save from
  // finish in the UI, mirroring the backend _flags_complete). With a known count, or
  // in single-flag mode, completion is the count as before.
  const collectUnknown = !!deck.multiFlag && need <= 1;
  const complete = !collectUnknown && deck.flags.length >= need;
  // multi-flag: "solved" requires the count met (or any flag in single-flag mode). A
  // live run with some-but-not-all flags is "collecting". A finished run with a flag
  // reads "solved" even if short (it ended — no more coming).
  const phase: SwarmDigest["phase"] = !deck.started
    ? "draft"
    : complete || (deck.finished && deck.flags.length > 0)
      ? "solved"
      : deck.awaitingOperator
        ? "paused"
        : deck.flags.length > 0 && !deck.finished
          ? "collecting"
          : deck.racing && !deck.finished
            ? "racing"
            : deck.outcomeReason === "goal_met"
              ? "goal_met"
              : deck.finished
                ? "finished"
                : "running";
  return {
    phase,
    verified: verified.length,
    candidates: candidates.length,
    openIntents: intents.length,
    deadEnds: deads.length,
    onlineWorkers: online,
    totalWorkers: Math.max(ids.length, lanes.length),
    latestVerified: verified[verified.length - 1],
    flag: deck.flag,
    flags: deck.flags,
    expectedFlags: need,
    goalWhy: deck.goalWhy,
    usd: deck.usd,
    tokensIn: deck.tokensIn,
    tokensOut: deck.tokensOut,
    costBySolver: deck.costBySolver,
    startedAt: deck.startedAt,
    finishedAt: deck.finishedAt,
  };
}

/** UI-level liveness for controls and chrome.
 *
 * `RUN_FINISHED` is still the authoritative terminal event, but live SSE clients
 * can transiently miss the final frame while already having folded a gated flag
 * from worker/blackboard events. In single-flag mode, or when expected_flags is
 * already satisfied, the digest is enough to close live controls; partial
 * multi-flag collection stays active.
 */
export function isRunActive(deck: DeckState): boolean {
  if (!deck.started || deck.finished) return false;
  const phase = swarmDigest(deck).phase;
  return phase !== "solved" && phase !== "goal_met" && phase !== "finished";
}
