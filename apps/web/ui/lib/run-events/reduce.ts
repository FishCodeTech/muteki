/** Deck event reducer. Public entry point; event domains live in ./reducers/. */
import type { DeckState, MutekiEvent } from "./types";
import { reduceLifecycle } from "./reducers/lifecycle";
import { reduceConversation } from "./reducers/conversation";
import { reduceGraph } from "./reducers/graph";
import { reduceBlackboard } from "./reducers/blackboard";
import { reduceControl } from "./reducers/control";
import { reduceCompletion } from "./reducers/completion";
import { reduceWorkerPrompt } from "./reducers/worker-prompts";

const EMPTY_LIST: never[] = [];
const EMPTY_RECORD = {} as Record<string, never>;
const orEmpty = <T,>(list: T[] | undefined): T[] => list ?? (EMPTY_LIST as T[]);

/**
 * Keep `prev` when every own key of `next` still holds the same value. The
 * collaboration model memoises its layers on these container references.
 */
function settled<T extends object>(prev: T, next: T): T {
  const keys = Object.keys(next) as (keyof T)[];
  if (keys.length !== Object.keys(prev).length) return next;
  for (const key of keys) if (!Object.is(prev[key], next[key])) return next;
  return prev;
}

export function reduce(prev: DeckState, ev: MutekiEvent): DeckState {
  // Copy-on-write: the containers domain reducers assign into are shallow
  // copies; every array keeps its identity until a reducer replaces it.
  const s: DeckState = {
    ...prev,
    lanes: { ...prev.lanes },
    workerPrompts: prev.workerPrompts ?? EMPTY_RECORD,
    graph: { ...prev.graph },
    sharedGraph: { ...prev.sharedGraph },
    reason: { ...prev.reason },
    blackboard: {
      ...prev.blackboard,
      observations: orEmpty(prev.blackboard.observations),
      capabilities: orEmpty(prev.blackboard.capabilities),
      accessPaths: orEmpty(prev.blackboard.accessPaths),
      capabilityGaps: orEmpty(prev.blackboard.capabilityGaps),
      valueReceipts: orEmpty(prev.blackboard.valueReceipts),
      gatedFindings: orEmpty(prev.blackboard.gatedFindings),
      reviewFindings: orEmpty(prev.blackboard.reviewFindings),
      suppressedRoutes: orEmpty(prev.blackboard.suppressedRoutes),
      branches: orEmpty(prev.blackboard.branches),
      directives: orEmpty(prev.blackboard.directives),
      reasonRuns: orEmpty(prev.blackboard.reasonRuns),
      flags: orEmpty(prev.blackboard.flags),
      flagOrigins: prev.blackboard.flagOrigins ?? EMPTY_RECORD,
      truncated: prev.blackboard.truncated ?? EMPTY_RECORD,
    },
    runtimeEvents: orEmpty(prev.runtimeEvents),
    controlCommands: orEmpty(prev.controlCommands),
    operatorDirectives: orEmpty(prev.operatorDirectives),
    resourceLocks: orEmpty(prev.resourceLocks),
    lockRequests: orEmpty(prev.lockRequests),
    preflightFailures: orEmpty(prev.preflightFailures),
    flagConfirmations: orEmpty(prev.flagConfirmations),
    costHistory: { ...prev.costHistory },
  };

  const p = ev.payload || {};

  const executionGeneration = Number(p.execution_generation);
  if (Number.isFinite(executionGeneration) && executionGeneration >= 0) {
    if (executionGeneration < (s.executionGeneration ?? 0)) return prev;
    s.executionGeneration = Math.max(
      s.executionGeneration ?? 0,
      executionGeneration,
    );
  }

  const projectedControlGeneration = Number(p.control_generation);
  if (
    Number.isFinite(projectedControlGeneration)
    && projectedControlGeneration >= 0
  ) {
    s.controlGeneration = Math.max(
      s.controlGeneration ?? 0,
      projectedControlGeneration,
    );
  }

  const next = reduceWorkerPrompt(ev, s)
    ?? reduceLifecycle(prev, ev, s)
    ?? reduceConversation(ev, s)
    ?? reduceGraph(ev, s)
    ?? reduceBlackboard(ev, s)
    ?? reduceControl(ev, s)
    ?? reduceCompletion(ev, s)
    ?? s;

  next.lanes = settled(prev.lanes, next.lanes);
  next.graph = settled(prev.graph, next.graph);
  next.sharedGraph = settled(prev.sharedGraph, next.sharedGraph);
  next.reason = settled(prev.reason, next.reason);
  next.blackboard = settled(prev.blackboard, next.blackboard);
  return next;
}
