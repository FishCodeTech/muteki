/** Derived deck selectors. Moved from events.ts. */
import type {
  BlackboardFact, BlackboardVulnReport, ChatMessage, DeckState,
  SolverCost, SolverLane,
} from "./types";

// ============================================================================
// Derived selectors — coordinator ↔ worker split + run digest.
//
// The conversation-first deck renders the MAIN thread as operator ↔ COORDINATOR
// (the DeepSeek `reason` actor: planning / auditing / verdicts) and pushes the
// WORKER firehose (cli-claude / cli-codex / cursor shell loops) into secondary
// panels + the right-column inspector. The split is by `solver_id`, so it needs
// NO backend change — see AGENTS.md (reason = coordinator, cli-* = workers).
// ============================================================================

// "operator" is the actor the backend stamps on operator-issued blackboard
// events (flag invalidation, focus/redirect); it is a participant, never a lane.
// "dispatch" is the rail-label Planner's actor.
// "preflight" stamps preflight checks; "legacy-reconciler" stamps backfilled
// flag_found records. Neither is a worker.
const CONTROL_SOLVER_IDS = new Set(["reason", "coordinator", "report-value", "dispatch", "operator", "preflight", "legacy-reconciler"]);

/** Is this solver id a shell WORKER (vs the coordinator / unscoped)? */
export function isWorkerLane(id?: string | null): boolean {
  return !!id && !CONTROL_SOLVER_IDS.has(id);
}

/** Execution generation of a worker id: continued runs (web resolve) mint ids
 * with a `-gN` suffix (cli-pi-g2); anything without the suffix is generation 1.
 * Canonical home is events.ts (the dependency-free event contract); workers.ts
 * re-exports from here. */
export function workerGeneration(id: string): number {
  const m = id.match(/-g(\d+)$/);
  return m ? parseInt(m[1], 10) : 1;
}

/** True when any worker id in the set carries an explicit generation suffix —
 * i.e. this run has seen a resolve under the generation-aware naming. */
function hasGenerationSuffixedIds(ids: Iterable<string>): boolean {
  for (const id of ids) if (/-g\d+$/.test(id)) return true;
  return false;
}

/** Review/arbiter workers are shell workers with a review phase/role. Keep the
 *  predicate centralized so the roster, worker lanes, and timeline do not drift. */
export function isReviewWorkerLane(lane?: Pick<SolverLane, "role" | "phase" | "statusReason"> | null): boolean {
  if (!lane) return false;
  if (lane.role === "review") return true;
  const phase = (lane.phase || lane.statusReason || "").toLowerCase();
  return phase.includes("review");
}

export function isVerifierWorkerLane(lane?: Pick<SolverLane, "role" | "phase" | "statusReason"> | null): boolean {
  if (!lane) return false;
  if (lane.role === "verifier") return true;
  const phase = (lane.phase || lane.statusReason || "").toLowerCase();
  return phase.includes("verifier");
}

function lanePhaseToken(lane: SolverLane): string {
  return (lane.phase || lane.statusReason || lane.role || "").toLowerCase();
}

function isRaceWorkerLane(lane: SolverLane): boolean {
  if (lane.raceScout) return true;
  return lanePhaseToken(lane).includes("race");
}

function isLaneActive(lane: SolverLane): boolean {
  if (lane.online === false) return false;
  const status = (lane.status || "").toLowerCase();
  return status !== "done" && status !== "finished" && status !== "error";
}

function raceWorkerCounts(deck: DeckState): { active: number; total: number; finished: number } {
  const raceLanes = workerLanes(deck).filter(isRaceWorkerLane);
  const active = raceLanes.filter(isLaneActive).length;
  const total = deck.raceTotal ?? raceLanes.length;
  const finished = deck.raceFinished ?? Math.max(0, total - active);
  return { active, total, finished };
}

function reportPipelineCounts(deck: DeckState): {
  submitted: number;
  reproducing: number;
  accepted: number;
  qualified: number;
} {
  const rows = deck.blackboard.vulnReports ?? [];
  const submitted = rows.filter((row) => row.status === "submitted").length;
  const reproducing = Math.max(0, deck.verifying ?? 0);
  const accepted = rows.filter((row) => row.status === "accepted").length;
  const qualified = rows.filter((row) =>
    row.status === "accepted" && row.goalQualified !== false).length;
  return { submitted, reproducing, accepted, qualified };
}

/** A chat message belongs to the main (coordinator) thread when it's operator
 *  input, a system lifecycle line, or coordinator/unscoped agent text. Worker
 *  agent bubbles are excluded — they live in the secondary worker panels. */
function isCoordinatorMessage(m: ChatMessage): boolean {
  if (m.role === "human" || m.role === "system") return true;
  if (m.mainThread) return true;
  // Reason is a structured planner. Historical recordings may contain its raw
  // JSON as text/reasoning deltas; keep those out of the operator conversation.
  if (m.solverId === "reason" && (m.kind === "text" || m.kind === "reasoning")) return false;
  return !m.solverId || CONTROL_SOLVER_IDS.has(m.solverId);
}

export function coordinatorThread(deck: DeckState): ChatMessage[] {
  const messages = deck.chat.filter(isCoordinatorMessage);
  const latestProgressByContent = new Map<string, number>();
  const progressKeys = messages.map((message, index) => {
    if (message.kind !== "progress" || !message.progressBrief) return "";
    const brief = message.progressBrief;
    const key = JSON.stringify({
      summary: brief.summary ?? "",
      summaryKey: brief.summaryKey ?? "",
      summaryVars: brief.summaryVars ?? {},
      sections: brief.sections,
    });
    latestProgressByContent.set(key, index);
    return key;
  });
  return messages.filter((message, index) => (
    message.kind !== "progress"
    || !message.progressBrief
    || latestProgressByContent.get(progressKeys[index]) === index
  ));
}

/** All worker agent bubbles (reasoning/text/tool), in time order. */
export function workerChat(deck: DeckState): ChatMessage[] {
  return deck.chat.filter((m) => m.role === "agent" && isWorkerLane(m.solverId));
}

/** Lanes for actual shell workers (drops the coordinator `reason` lane). */
function workerLanes(deck: DeckState): SolverLane[] {
  return Object.values(deck.lanes).filter((l) => isWorkerLane(l.solverId));
}

/** Distinct worker ids seen across lanes AND chat (a worker that only streamed
 *  text before a WORKER_STATUS lane was created still appears). Chat-derived ids
 *  pass the same isWorkerLane filter as lanes so system actors (reason /
 *  coordinator / report-value) never inflate the roster denominator. */
export function workerIds(deck: DeckState): string[] {
  const ids = new Set<string>();
  for (const l of workerLanes(deck)) ids.add(l.solverId);
  for (const m of workerChat(deck)) if (m.solverId && isWorkerLane(m.solverId)) ids.add(m.solverId);
  return Array.from(ids);
}

/** Worker ids of the CURRENT execution generation. Continued runs (web resolve)
 * mint generation-suffixed ids (cli-pi-g2); when any such id exists, only the
 * latest generation counts — previous generations are finished workers kept for
 * display, not roster members. Runs without suffixed ids (fresh starts, older
 * recordings) keep the legacy behavior: every known id counts. */
export function currentGenWorkerIds(deck: DeckState): string[] {
  const ids = workerIds(deck);
  if (!hasGenerationSuffixedIds(ids)) return ids;
  const cur = deck.executionGeneration > 0
    ? deck.executionGeneration
    : Math.max(...ids.map(workerGeneration));
  return ids.filter((id) => workerGeneration(id) === cur);
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
  const facts = deck.blackboard.facts;
  const bb = facts
    .filter((f) => f.verified && !f.challenged && !isFactRetired(f))
    .map((f) => f.summary || f.fact);
  return uniqueTexts(facts.length ? bb : deck.sharedGraph.verified.map((e) => e.fact));
}

export function candidateFactTexts(deck: DeckState): string[] {
  if (deck.blackboard.observations.length) {
    return uniqueTexts(
      deck.blackboard.observations
        .filter((observation) => !observation.admitted)
        .map((observation) => observation.text),
    );
  }
  const facts = deck.blackboard.facts;
  const bb = facts
    .filter((f) => (!f.verified || !!f.challenged) && !isFactRetired(f))
    .map((f) => f.summary || f.fact);
  return uniqueTexts(facts.length ? bb : deck.sharedGraph.candidates.map((e) => e.fact));
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
  mode?: "ctf" | "pentest";
  platformConfirmationRequired: boolean;
  platformAccepted: number;
  platformPending: number;
  platformRejected: number;
  openSteps: number;
  goals: number;
  reports: BlackboardVulnReport[];
  expectedReports: number;
  /** Pentest report pipeline: submitted → reproducing → accepted. */
  reportSubmitted: number;
  reportReproducing: number;
  reportAccepted: number;
  /** Accepted reports that also satisfy the task completion contract. */
  reportQualified: number;
  /** Active race-scout workers (when racing). */
  raceActive: number;
  raceTotal: number;
  /** Race-scout workers already finished (while phase may still be racing). */
  raceFinished: number;
  /** Active verifier workers vs configured concurrent cap. */
  verifyingActive: number;
  verifyingMax: number;
  challengeName: string;
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

function qualifiedReports(deck: DeckState): BlackboardVulnReport[] {
  return (deck.blackboard.vulnReports ?? []).filter((row) =>
    row.status === "accepted" && row.goalQualified !== false);
}

/** The digest phase alone: reads run flags and reports, never the chat or lanes. */
export function swarmPhase(deck: DeckState): SwarmDigest["phase"] {
  const pentest = deck.mode === "pentest";
  const need = Math.max(1, deck.expectedFlags || 1);
  const collectUnknown = !!deck.multiFlag && need <= 1;
  const platformRequired = !!deck.platformConfirmationRequired;
  const platformAccepted = (deck.flagConfirmations || []).filter((row) => row.status === "accepted").length;
  const flagComplete = !collectUnknown && (platformRequired ? platformAccepted >= need : deck.flags.length >= need);
  const reports = qualifiedReports(deck);
  return !deck.started
    ? "draft"
    : (!pentest && flagComplete && deck.outcomeReason !== "runtime_failure"
      && deck.outcomeReason !== "preflight_failed"
      && deck.outcomeReason !== "no_progress"
      && deck.outcomeReason !== "operator_stop"
      && deck.outcomeReason !== "budget_exhausted")
      ? "solved"
      : deck.outcomeReason === "goal_met"
        ? "goal_met"
        : deck.awaitingOperator
          ? "paused"
          : ((pentest ? reports.length > 0 : deck.flags.length > 0) && !deck.finished)
            ? "collecting"
            : deck.racing && !deck.finished
              ? "racing"
              : deck.finished
                ? (pentest && deck.solved ? "goal_met" : "finished")
                : "running";
}

export function swarmDigest(deck: DeckState): SwarmDigest {
  const verified = verifiedFactTexts(deck);
  const candidates = candidateFactTexts(deck);
  const intents = openIntentTexts(deck);
  const deads = deadEndTexts(deck);
  // Roster denominator = current generation only (continued runs keep previous
  // generations' finished workers visible but out of the count).
  const curIds = currentGenWorkerIds(deck);
  const online = curIds.filter((id) => (deck.lanes[id]?.online ?? !deck.finished) !== false).length;
  const reports = qualifiedReports(deck);
  const pipeline = reportPipelineCounts(deck);
  const raceCounts = raceWorkerCounts(deck);
  const expectedReports = Math.max(1, deck.expectedFindings || 1);
  const need = Math.max(1, deck.expectedFlags || 1);
  const verifyingActive = deck.verifying ?? 0;
  const verifyingMax = Math.max(1, verifyingActive);
  const phase = swarmPhase(deck);
  const confirmations = deck.flagConfirmations || [];
  const platformAccepted = confirmations.filter((row) => row.status === "accepted").length;
  const platformPending = confirmations.filter((row) => row.status === "pending").length;
  const platformRejected = confirmations.filter((row) => row.status === "rejected").length;
  return {
    phase,
    verified: verified.length,
    candidates: candidates.length,
    openIntents: intents.length,
    openSteps: intents.length,
    goals: deck.mode === "pentest" ? 0 : 1,
    deadEnds: deads.length,
    onlineWorkers: online,
    totalWorkers: curIds.length,
    latestVerified: verified[verified.length - 1],
    flag: deck.flag,
    flags: deck.flags,
    expectedFlags: need,
    mode: deck.mode,
    platformConfirmationRequired: !!deck.platformConfirmationRequired,
    platformAccepted,
    platformPending,
    platformRejected,
    reports,
    expectedReports,
    reportSubmitted: pipeline.submitted,
    reportReproducing: pipeline.reproducing,
    reportAccepted: pipeline.accepted,
    reportQualified: pipeline.qualified,
    raceActive: raceCounts.active,
    raceTotal: raceCounts.total,
    raceFinished: raceCounts.finished,
    verifyingActive,
    verifyingMax,
    challengeName: deck.challengeName,
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
 *
 * Pentest can emit `goal_complete` (and the digest may show `goal_met`) while
 * workers are still being cancelled. Keep stop/steer until `RUN_FINISHED`.
 */
export function isRunActive(deck: DeckState): boolean {
  if (!deck.started || deck.finished) return false;
  if (deck.mode === "pentest") return true;
  const phase = swarmDigest(deck).phase;
  return phase !== "solved" && phase !== "goal_met" && phase !== "finished";
}
