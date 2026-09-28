import type { BlackboardTruncation, BlackboardView } from "./types";

export const REVIEW_CAP = 80;
export const POC_CAP = 80;
export const ROUTE_CAP = 60;
export const DIRECTIVE_CAP = 60;
export const FACT_CAP = 200;
export const DEAD_END_CAP = 50;

export function markTruncated(bb: BlackboardView, key: keyof BlackboardTruncation): void {
  bb.truncated = { ...(bb.truncated ?? {}), [key]: true };
}

export function capPush<T>(list: T[], item: T, cap: number): { list: T[]; truncated: boolean } {
  const next = [...list, item];
  if (next.length <= cap) return { list: next, truncated: false };
  return { list: next.slice(-cap), truncated: true };
}
