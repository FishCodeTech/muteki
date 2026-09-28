import type { DeckState } from "./events";

export type RunCanvasMode = "ctf" | "pentest";

export const CTF_CANVAS_KIND_NAMES = [
  "fact", "observation", "dead_end", "step", "poc", "goal", "flag",
] as const;
export const PENTEST_CANVAS_KIND_NAMES = [
  "intent", "fact", "candidate", "dead_end", "poc",
  "review", "finding", "route", "branch", "directive", "flag", "lock",
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
