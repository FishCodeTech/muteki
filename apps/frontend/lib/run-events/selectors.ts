import type {
  BlackboardFact, ChatMessage, DeckState,
  SolverCost, SolverLane,
} from "./types";


const CONTROL_SOLVER_IDS = new Set(["followup", "standby", "reason", "challenger", "coordinator", "origin", "report-value", "dispatch", "operator", "preflight", "legacy-reconciler"]);

export function isWorkerLane(id?: string | null): boolean {
  return !!id && !CONTROL_SOLVER_IDS.has(id) && !id.startsWith("btw-") && !id.endsWith("-standby");
}

export function workerGeneration(id: string): number {
  const m = id.match(/-g(\d+)$/);
  return m ? parseInt(m[1], 10) : 1;
}

function hasGenerationSuffixedIds(ids: Iterable<string>): boolean {
  for (const id of ids) if (/-g\d+$/.test(id)) return true;
  return false;
}

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

function isCoordinatorMessage(m: ChatMessage, driverKind: DeckState["driverKind"]): boolean {
  if (m.role === "human" || m.role === "system") return true;
  if (m.mainThread) return true;
  if (driverKind === "local_pi") return true;
  if ((m.solverId === "reason" || m.solverId === "challenger") && (m.kind === "text" || m.kind === "reasoning")) return false;
  return !m.solverId || CONTROL_SOLVER_IDS.has(m.solverId);
}

export function coordinatorThread({ chat, driverKind }: Pick<DeckState, "chat" | "driverKind">): ChatMessage[] {
  const messages = chat.filter((m) => isCoordinatorMessage(m, driverKind));
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

export function workerChat(deck: DeckState): ChatMessage[] {
  return deck.chat.filter((m) => m.role === "agent" && isWorkerLane(m.solverId));
}

function workerLanes(deck: DeckState): SolverLane[] {
  return Object.values(deck.lanes).filter((l) => isWorkerLane(l.solverId));
}

export function workerIds(deck: DeckState): string[] {
  const ids = new Set<string>();
  for (const l of workerLanes(deck)) ids.add(l.solverId);
  for (const m of workerChat(deck)) if (m.solverId && isWorkerLane(m.solverId)) ids.add(m.solverId);
  return Array.from(ids);
}

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

const FACT_RETIRED: ReadonlySet<string> = new Set(["rejected", "merged", "superseded"]);
export function isFactRetired(f: BlackboardFact): boolean {
  return !!f.state && FACT_RETIRED.has(f.state);
}

export function verifiedFactTexts(deck: DeckState): string[] {
  const facts = deck.blackboard.facts;
  const bb = facts
    .filter((f) => f.verified && !f.challenged && !isFactRetired(f) && f.actor !== "origin")
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
  steps: number;
  goals: number;
  raceActive: number;
  raceTotal: number;
  raceFinished: number;
  verifyingActive: number;
  verifyingMax: number;
  challengeName: string;
  goalWhy?: string;
  usd: number;
  tokensIn: number;
  tokensOut: number;
  costBySolver: Record<string, SolverCost>;
  startedAt?: number;
  finishedAt?: number;
  activeDurationMs: number;
  activeSince?: number;
}

export function swarmPhase(deck: DeckState): SwarmDigest["phase"] {
  const pentest = deck.mode === "pentest";
  const need = Math.max(1, deck.expectedFlags || 1);
  const collectUnknown = !!deck.multiFlag && need <= 1;
  const platformRequired = !!deck.platformConfirmationRequired;
  const platformAccepted = (deck.flagConfirmations || []).filter((row) => row.status === "accepted").length;
  const flagComplete = collectUnknown
    ? (deck.finished && !platformRequired && deck.flags.length > 0)
    : (platformRequired ? platformAccepted >= need : deck.flags.length >= need);
  return !deck.started
    ? "draft"
    : (!pentest && (flagComplete || (deck.finished && deck.solved))
      && deck.outcomeReason !== "runtime_failure"
      && deck.outcomeReason !== "preflight_failed"
      && (deck.solved || (deck.outcomeReason !== "no_progress"
        && deck.outcomeReason !== "operator_stop"
        && deck.outcomeReason !== "budget_exhausted")))
      ? "solved"
      : deck.outcomeReason === "goal_met"
        ? "goal_met"
        : deck.awaitingOperator
          ? "paused"
          : (deck.flags.length > 0 && !deck.finished)
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
  const curIds = currentGenWorkerIds(deck);
  const online = curIds.filter((id) => (deck.lanes[id]?.online ?? !deck.finished) !== false).length;
  const raceCounts = raceWorkerCounts(deck);
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
    steps: deck.blackboard.intents.length,
    goals: (deck.blackboard.subGoals?.length ?? 0) + (deck.taskContract?.completion.goal || deck.challengeName ? 1 : 0),
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
    activeDurationMs: deck.activeDurationMs,
    activeSince: deck.activeSince,
  };
}

export function isRunActive(deck: DeckState): boolean {
  if (!deck.started || deck.finished) return false;
  if (deck.driverKind === "local_pi") return true;
  if (deck.mode === "pentest") return true;
  const phase = swarmPhase(deck);
  return phase !== "solved" && phase !== "goal_met" && phase !== "finished";
}
