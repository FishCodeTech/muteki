"use client";

import {
  createContext,
  useContext,
  useEffect,
  useLayoutEffect,
  useState,
  useSyncExternalStore,
} from "react";
import { useTimelineEnv } from "./traceCopy";
import { formatDuration, toRealTime, type TraceEventSpec, type TraceNode } from "./traceModel";

export interface ClockStore {
  cursor: () => number;
  head: () => number;
  subscribe: (listener: () => void) => () => void;
}

export const ClockContext = createContext<ClockStore | null>(null);

const noopSubscribe = () => () => {};

/** Re-renders only when `select` changes, so callers should quantize (e.g. to 100ms). */
export function useClockValue<T extends string | number>(select: (clock: ClockStore) => T, fallback: T): T {
  const clock = useContext(ClockContext);
  return useSyncExternalStore(
    clock ? clock.subscribe : noopSubscribe,
    () => (clock ? select(clock) : fallback),
    () => fallback,
  );
}

export function SpanDuration({ node }: { node: TraceNode }) {
  const { copy, gaps } = useTimelineEnv();
  const live = node.status === "running";
  const tenths = useClockValue((clock) => (live ? Math.floor(clock.cursor() / 100) : 0), 0);
  if (node.spec.point) return <>{node.spec.kind === "llm" || node.spec.kind === "tool" ? copy.untimed : copy.instant}</>;
  const end = live ? tenths * 100 : node.end ?? node.spec.start;
  const value = toRealTime(gaps, end) - toRealTime(gaps, node.spec.start);
  return <>{node.spec.estimated ? "≈" : ""}{formatDuration(Math.max(0, value))}</>;
}

export interface Present<T> {
  key: string;
  item: T;
  leaving: boolean;
  leftAt: number;
}

function mergePresence<T>(prev: Present<T>[], next: readonly T[], getKey: (item: T) => string): Present<T>[] {
  const nextKeys = new Set(next.map(getKey));
  const after = new Map<string | null, Present<T>[]>();
  let anchor: string | null = null;
  const now = performance.now();
  for (const entry of prev) {
    if (nextKeys.has(entry.key)) {
      anchor = entry.key;
      continue;
    }
    const list = after.get(anchor) ?? [];
    list.push(entry.leaving ? entry : { ...entry, leaving: true, leftAt: now });
    after.set(anchor, list);
  }
  const out: Present<T>[] = [...(after.get(null) ?? [])];
  for (const item of next) {
    const key = getKey(item);
    out.push({ key, item, leaving: false, leftAt: 0 });
    const tail = after.get(key);
    if (tail) out.push(...tail);
  }
  return out;
}

/**
 * Keeps removed items rendered (flagged `leaving`) for `exitMs` so they can
 * animate out. `getKey` must be stable; `items` should be memoized.
 */
export function usePresence<T>(items: readonly T[], getKey: (item: T) => string, exitMs: number): Present<T>[] {
  const [state, setState] = useState<Present<T>[]>(() =>
    items.map((item) => ({ key: getKey(item), item, leaving: false, leftAt: 0 })),
  );
  useLayoutEffect(() => {
    setState((prev) => mergePresence(prev, items, getKey));
  }, [items, getKey]);
  useEffect(() => {
    const leaving = state.filter((entry) => entry.leaving);
    if (!leaving.length) return;
    const due = Math.min(...leaving.map((entry) => entry.leftAt)) + exitMs - performance.now();
    const timer = window.setTimeout(() => {
      const cutoff = performance.now() - exitMs + 16;
      setState((prev) => prev.filter((entry) => !entry.leaving || entry.leftAt > cutoff));
    }, Math.max(0, due));
    return () => window.clearTimeout(timer);
  }, [state, exitMs]);
  return state;
}

export type KindFilter = "agent" | "llm" | "tool";

export interface FilterState {
  kinds: KindFilter[];
  errors: boolean;
  query: string;
}

export const EMPTY_FILTER: FilterState = { kinds: [], errors: false, query: "" };

export const kindOf = (node: TraceNode): KindFilter =>
  node.spec.kind === "llm" ? "llm" : node.spec.kind === "tool" ? "tool" : "agent";

export const filterActive = (filter: FilterState) =>
  filter.kinds.length > 0 || filter.errors || filter.query.trim() !== "";

export function nodeMatches(node: TraceNode, filter: FilterState): boolean {
  if (filter.kinds.length && !filter.kinds.includes(kindOf(node))) return false;
  if (filter.errors && node.status !== "failed") return false;
  const query = filter.query.trim().toLocaleLowerCase();
  if (query && !`${node.spec.name} ${node.spec.detail ?? ""}`.toLocaleLowerCase().includes(query)) return false;
  return true;
}

export function eventMatches(event: TraceEventSpec, node: TraceNode, filter: FilterState): boolean {
  if (filter.kinds.length) {
    const kind = event.kind === "call" ? kindOf(node) : "agent";
    if (!filter.kinds.includes(kind)) return false;
  }
  if (filter.errors && !(event.kind === "call" && node.status === "failed")) return false;
  const query = filter.query.trim().toLocaleLowerCase();
  if (query) {
    const hay = `${event.text} ${event.body ?? ""} ${node.spec.error ?? ""} ${node.actor.spec.name}`.toLocaleLowerCase();
    if (!hay.includes(query)
      && !node.spec.input?.toLocaleLowerCase().includes(query)
      && !node.spec.output?.toLocaleLowerCase().includes(query)) return false;
  }
  return true;
}

export const spanColor = (node: TraceNode) => node.spec.color ?? `var(--eng-${node.spec.engine ?? "default"}, var(--eng-default))`;
export const actorColor = (node: TraceNode) => spanColor(node.actor);

export const clamp = (value: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, value));
