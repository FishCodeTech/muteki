/**
 * Appearance-order replay for a finished collaboration canvas.
 *
 * Projects the already-reduced deck onto a wall-clock `asOf` using timestamps
 * on knowledge, relations, and agent lifecycle. Does not re-reduce the SSE
 * log: useRun keeps no event array, and a checkpointed reduce of the
 * copy-heavy reducer is O(n²).
 *
 * Omitted relative to a full historical replay:
 * - Lane phase / statusReason / tokens / cost stay terminal.
 * - statusKind is only online vs offline (SolverLane has no history).
 * - Retired / challenged / lock state has no dedicated timestamp.
 * - Reason-round counts have no per-round clock.
 * - Blackboard events cap at 300 and runtime events at 500, so long runs
 *   can miss early markers (see replayTruncated).
 */

import {
  COORDINATOR_ID,
  isCollaborationWorker,
  OPERATOR_ID,
  relationRefTarget,
  summarizeKnowledge,
  type AgentCollaborationModel,
  type CollaborationAgent,
  type CollaborationKnowledgeItem,
  type CollaborationRelation,
  type CoordinatorMetrics,
} from "./agentCollaboration";
import type { BlackboardEvent, BlackboardIntent, DeckState } from "./events";
import { toEpochMs } from "./format";
import { knowledgeStatusKey } from "./statusLabels";
import type { LaneStatusKind } from "./workerLanePresentation";

/** Matches `blackboard.ts` ring size. */
export const BLACKBOARD_EVENT_CAP = 300;
/** Matches `lifecycle.ts` ring size. */
export const RUNTIME_EVENT_CAP = 500;

export const REPLAY_MARK_KINDS = ["intent_claimed", "flag_found", "worker_spawned", "reason_done"] as const;
export type ReplayMarkKind = (typeof REPLAY_MARK_KINDS)[number];

export type ReplayMark = {
  id: string;
  kind: ReplayMarkKind;
  ts: number;
  label: string;
};

const MARK_KIND = new Set<string>(REPLAY_MARK_KINDS);

export function atOrBefore(ts: number | undefined, asOf: number): boolean {
  const ms = toEpochMs(ts);
  return ms > 0 && ms <= asOf;
}

export function replayBounds(deck: Pick<DeckState, "startedAt" | "finishedAt">): { start: number; end: number } {
  const start = toEpochMs(deck.startedAt);
  const end = toEpochMs(deck.finishedAt) || start;
  return { start, end };
}

export function replayTruncated(deck: DeckState): boolean {
  return deck.blackboard.events.length >= BLACKBOARD_EVENT_CAP
    || deck.runtimeEvents.length >= RUNTIME_EVENT_CAP;
}

export function replayMarks(events: BlackboardEvent[], rounds: DeckState["blackboard"]["reasonRuns"] = []): ReplayMark[] {
  const marks: ReplayMark[] = [];
  for (const event of events) {
    if (!MARK_KIND.has(event.kind)) continue;
    const ts = toEpochMs(event.ts);
    if (!ts) continue;
    marks.push({ id: event.id, kind: event.kind as ReplayMarkKind, ts, label: event.label });
  }
  const decisionTimes = new Set(marks.filter((mark) => mark.kind === "reason_done").map((mark) => mark.ts));
  for (const round of rounds) {
    const ts = toEpochMs(round.ts);
    if (!ts || decisionTimes.has(ts)) continue;
    marks.push({ id: `decision:${ts}`, kind: "reason_done", ts, label: "" });
    decisionTimes.add(ts);
  }
  return marks.sort((a, b) => a.ts - b.ts);
}

/** Coordinator is always present; a worker appears once firstSeenAt is known and ≤ asOf. */
export function agentPresentAt(agent: CollaborationAgent, asOf: number): boolean {
  if (agent.id === COORDINATOR_ID) return true;
  const seen = toEpochMs(agent.firstSeenAt ?? agent.startedAt);
  return !seen || seen <= asOf;
}

function agentOnlineAt(agent: CollaborationAgent, asOf: number): boolean {
  const ended = toEpochMs(agent.finishedAt ?? agent.endedAt);
  return !ended || ended > asOf;
}

function projectKnowledgeItem(item: CollaborationKnowledgeItem, asOf: number): CollaborationKnowledgeItem | undefined {
  if (!atOrBefore(item.appearedTs, asOf) && toEpochMs(item.appearedTs) > 0) return undefined;
  if (!toEpochMs(item.appearedTs) && toEpochMs(item.ts) > asOf) return undefined;

  if (item.kind === "intent" || item.kind === "step") {
    if (atOrBefore(item.concludedTs, asOf)) return item;
    if (atOrBefore(item.claimedTs, asOf)) {
      if (item.status === "claimed") return item;
      return {
        ...item,
        status: "claimed",
        statusKey: knowledgeStatusKey(item.kind, "claimed"),
        tone: "accent",
        ts: item.claimedTs ?? item.ts,
      };
    }
    return {
      ...item,
      agentId: COORDINATOR_ID,
      status: "open",
      statusKey: knowledgeStatusKey(item.kind, "open"),
      tone: "neutral",
      ts: item.appearedTs,
    };
  }

  if (
    item.kind === "fact"
    && item.status !== "recorded"
    && item.status !== "scoped_negative"
    && !atOrBefore(item.promotedTs, asOf)
  ) {
    return {
      ...item,
      kind: "candidate",
      status: "candidate",
      statusKey: knowledgeStatusKey("candidate", "candidate"),
      tone: item.tone === "muted" ? "muted" : "warning",
      ts: item.appearedTs,
    };
  }

  return item;
}

function projectRelation(
  relation: CollaborationRelation,
  asOf: number,
  presentIds: Set<string>,
): CollaborationRelation | undefined {
  if (!presentIds.has(relation.source) || !presentIds.has(relation.target)) return undefined;
  const { refIds, timestamps } = relation;
  const offset = refIds.length - timestamps.length;
  const keptIds: string[] = [];
  const keptTs: number[] = [];
  for (let i = 0; i < refIds.length; i += 1) {
    const ts = i >= offset ? timestamps[i - offset] : timestamps[0];
    if (!atOrBefore(ts, asOf)) continue;
    keptIds.push(refIds[i]);
    keptTs.push(ts);
  }
  if (!keptIds.length) return undefined;
  const lastTs = keptTs[keptTs.length - 1];
  return {
    ...relation,
    refIds: keptIds,
    timestamps: keptTs,
    count: keptIds.length,
    ts: keptTs[0],
    lastTs,
    active: false,
  };
}

function collectIntents(model: AgentCollaborationModel): BlackboardIntent[] {
  const byId = new Map<string, BlackboardIntent>();
  for (const agent of model.allAgents) {
    for (const intent of agent.intents) byId.set(intent.id, intent);
  }
  return Array.from(byId.values());
}

function intentsOpenAt(intents: BlackboardIntent[], asOf: number): BlackboardIntent[] {
  return intents
    .filter((intent) => atOrBefore(intent.proposedTs, asOf) && !atOrBefore(intent.claimedTs, asOf))
    .sort((a, b) => a.proposedTs - b.proposedTs);
}

function currentIntentAt(intents: BlackboardIntent[], asOf: number, online: boolean): BlackboardIntent | undefined {
  if (!online) return undefined;
  for (let i = intents.length - 1; i >= 0; i -= 1) {
    const intent = intents[i];
    if (atOrBefore(intent.claimedTs, asOf) && !atOrBefore(intent.concludedTs, asOf)) return intent;
  }
  return undefined;
}

function lastIntentAt(intents: BlackboardIntent[], asOf: number): BlackboardIntent | undefined {
  for (let i = intents.length - 1; i >= 0; i -= 1) {
    const intent = intents[i];
    if (atOrBefore(intent.concludedTs, asOf) || atOrBefore(intent.claimedTs, asOf)) return intent;
  }
  return undefined;
}

function knowledgeForAgent(
  agentId: string,
  rows: CollaborationKnowledgeItem[],
): CollaborationKnowledgeItem[] {
  const owned: CollaborationKnowledgeItem[] = [];
  for (const row of rows) {
    const owner = row.agentId || COORDINATOR_ID;
    if (owner === agentId) owned.push(row);
    else if (agentId === COORDINATOR_ID && row.kind === "intent" && row.proposerId === COORDINATOR_ID) owned.push(row);
    else if (agentId === COORDINATOR_ID && row.kind === "directive" && row.proposerId === OPERATOR_ID) owned.push(row);
  }
  return owned;
}

function replayStatusKind(agent: CollaborationAgent, online: boolean): LaneStatusKind {
  if (agent.id === COORDINATOR_ID) return online ? "thinking" : (agent.statusKind === "solved" ? "solved" : "offline");
  return online ? "thinking" : "offline";
}

function coordinationAt(
  knowledge: CollaborationKnowledgeItem[],
  workers: CollaborationAgent[],
  asOf: number,
  previous: CoordinatorMetrics | undefined,
): CoordinatorMetrics {
  let proposedIntents = 0;
  let assignedIntents = 0;
  let completedIntents = 0;
  let directives = 0;
  let flags = 0;
  let verifiedFacts = 0;
  for (const item of knowledge) {
    switch (item.kind) {
      case "intent":
      case "step":
        if (item.proposerId && item.proposerId !== COORDINATOR_ID) break;
        proposedIntents += 1;
        if (atOrBefore(item.claimedTs, asOf)) assignedIntents += 1;
        if (atOrBefore(item.concludedTs, asOf)) completedIntents += 1;
        break;
      case "directive":
        directives += 1;
        break;
      case "flag":
        flags += 1;
        break;
      case "fact":
        if (item.tone !== "muted") verifiedFacts += 1;
        break;
      case "candidate":
      case "observation":
      case "dead_end":
      case "poc":
      case "report":
      case "review":
      case "finding":
      case "route":
      case "branch":
      case "lock":
      case "goal":
        break;
      default: {
        const _never: never = item.kind;
        void _never;
      }
    }
  }
  let totalWorkers = 0;
  let onlineWorkers = 0;
  for (const worker of workers) {
    if (!worker.isCurrent || !agentPresentAt(worker, asOf)) continue;
    totalWorkers += 1;
    if (worker.online) onlineWorkers += 1;
  }
  return {
    proposedIntents,
    assignedIntents,
    completedIntents,
    retiredIntents: 0,
    reasonRounds: previous?.reasonRounds ?? 0,
    directives,
    flags,
    verifiedFacts,
    onlineWorkers,
    totalWorkers,
  };
}

/**
 * Filter knowledge / relations / live flags to `asOf` (epoch ms). Agents stay
 * in `allAgents` so the canvas can hide not-yet-spawned cards without
 * dropping them from the layout cache.
 */
export function projectCollaborationAsOf(model: AgentCollaborationModel, asOf: number): AgentCollaborationModel {
  const presentIds = new Set<string>();
  for (const agent of model.allAgents) {
    if (agentPresentAt(agent, asOf)) presentIds.add(agent.id);
  }

  const knowledge: CollaborationKnowledgeItem[] = [];
  for (const item of model.knowledge) {
    const projected = projectKnowledgeItem(item, asOf);
    if (projected) knowledge.push(projected);
  }

  const knowledgeById = new Map<string, CollaborationKnowledgeItem>();
  for (const item of knowledge) knowledgeById.set(item.id, item);

  const relations: CollaborationRelation[] = [];
  for (const relation of model.relations) {
    const projected = projectRelation(relation, asOf, presentIds);
    if (!projected) continue;
    relations.push(projected);
    for (const refId of projected.refIds) {
      if (knowledgeById.has(refId)) continue;
      const base = knowledgeById.get(relationRefTarget(refId));
      if (base) knowledgeById.set(refId, base);
    }
  }

  const allIntents = collectIntents(model);
  const openIntents = intentsOpenAt(allIntents, asOf);

  const projectAgent = (agent: CollaborationAgent): CollaborationAgent => {
    const present = presentIds.has(agent.id);
    const online = present && agent.role !== "source" && (agent.role === "decision" ? agent.online : agentOnlineAt(agent, asOf));
    const rows = present ? knowledgeForAgent(agent.id, knowledge) : [];
    const metrics = summarizeKnowledge(rows);
    const chronological = agent.intents.filter((intent) => atOrBefore(intent.claimedTs ?? intent.proposedTs, asOf));
    const statusKind = replayStatusKind(agent, online);
    return {
      ...agent,
      ...metrics,
      knowledge: rows,
      online,
      statusKind,
      currentIntent: currentIntentAt(chronological, asOf, online),
      lastIntent: lastIntentAt(chronological, asOf),
      intents: agent.id === COORDINATOR_ID
        ? allIntents.filter((intent) => atOrBefore(intent.proposedTs, asOf) && !atOrBefore(intent.claimedTs, asOf))
        : chronological,
      isAnomaly: false,
    };
  };

  const workers = model.allAgents
    .filter((agent) => agent.id !== COORDINATOR_ID)
    .map(projectAgent);
  const coordinatorBase = projectAgent(model.coordinator);
  const coordinator: CollaborationAgent = {
    ...coordinatorBase,
    coordination: coordinationAt(knowledge, workers.filter(isCollaborationWorker), asOf, model.coordinator.coordination),
  };
  const allAgents = [coordinator, ...workers];
  const agentById = new Map(allAgents.map((agent) => [agent.id, agent]));
  const visibleAgents = model.agents.map((agent) => agentById.get(agent.id)!).filter(Boolean);

  return {
    ...model,
    agents: visibleAgents,
    allAgents,
    coordinator,
    agentById,
    relations,
    knowledge,
    knowledgeById,
    openIntents,
  };
}
