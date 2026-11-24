import type { DeckState, WorkerPromptRecord } from "./events";
import { toEpochMs } from "./format";

const EMPTY: WorkerPromptRecord[] = [];

export function workerPrompts(deck: Pick<DeckState, "workerPrompts">, workerId: string, asOf?: number): WorkerPromptRecord[] {
  const rows = deck.workerPrompts?.[workerId] ?? EMPTY;
  if (asOf === undefined) return rows;
  return rows.filter((row) => toEpochMs(row.ts) <= asOf).map((row) =>
    toEpochMs(row.updatedAt) > asOf ? { ...row, status: "prepared", updatedAt: row.ts } : row);
}
