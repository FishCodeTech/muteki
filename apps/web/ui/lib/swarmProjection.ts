import type { DeckState } from "./events";
import type { BlackboardSubGoal } from "./run-events/types";

export type RunCanvasMode = "ctf" | "pentest";

export const CTF_CANVAS_KIND_NAMES = [
  "fact", "observation", "dead_end", "step", "poc", "goal", "flag",
] as const;
export const PENTEST_CANVAS_KIND_NAMES = [
  "intent", "step", "goal", "observation", "fact", "candidate", "dead_end", "poc",
  "review", "report", "finding", "route", "branch", "directive", "flag", "lock",
] as const;

export function canvasModeOf(
  deck: Pick<DeckState, "mode">,
): RunCanvasMode {
  if (deck.mode === "pentest") return "pentest";
  return "ctf";
}

export function isCtfCanvasKindName(kind: string): boolean {
  return CTF_CANVAS_KIND_NAMES.some((name) => name === kind);
}

/** A satisfied goal reopens when its supporting facts leave the active graph. */
export function subGoalState(goal: BlackboardSubGoal, deck: Pick<DeckState, "blackboard">): BlackboardSubGoal["state"] {
  if (goal.state !== "satisfied") return goal.state;
  const retired = new Set(["rejected", "merged", "superseded"]);
  return goal.evidenceFactSeqs.length > 0 && goal.evidenceFactSeqs.every((seq) =>
    deck.blackboard.facts.some((fact) =>
      fact.factSeq === seq && fact.verified && !retired.has(fact.state ?? "")))
    ? "satisfied" : "open";
}
