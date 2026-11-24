"use client";

import {
  createContext,
  useContext,
  useEffect,
  useState,
  useSyncExternalStore,
} from "react";
import { useTimelineEnv } from "./traceCopy";
import { formatDuration, toRealTime, type TraceEventSpec, type TraceNode } from "./traceModel";

export interface ClockStore {
  cursor: () => number;
  head: () => number;
  subscribe: (listener: () => void) => () => void;
  bindTarget: (element: HTMLElement | null) => void | (() => void);
}

export const ClockContext = createContext<ClockStore | null>(null);

const noopSubscribe = () => () => {};

export function useClockValue<T extends string | number>(select: (clock: ClockStore) => T, fallback: T, enabled = true): T {
  const contextClock = useContext(ClockContext);
  const clock = enabled ? contextClock : null;
  return useSyncExternalStore(
    clock ? clock.subscribe : noopSubscribe,
    () => (clock ? select(clock) : fallback),
    () => fallback,
  );
}

export function SpanDuration({ node, updateLive = true }: { node: TraceNode; updateLive?: boolean }) {
  const { copy, gaps } = useTimelineEnv();
  const live = node.status === "running";
  const clock = useContext(ClockContext);
  const startReal = toRealTime(gaps, node.spec.start);
  const formatAt = (cursor: number) => {
    const end = Math.floor(cursor / 100) * 100;
    return formatDuration(Math.max(0, toRealTime(gaps, end) - startReal));
  };
  const liveText = useClockValue(
    (value) => formatAt(value.cursor()),
    "0ms",
    live && !node.spec.point && updateLive,
  );
  if (node.spec.point) return <>{node.spec.kind === "llm" || node.spec.kind === "tool" ? copy.untimed : copy.instant}</>;
  const text = live
    ? (updateLive ? liveText : formatAt(clock?.cursor() ?? node.spec.start))
    : formatDuration(Math.max(0, toRealTime(gaps, node.end ?? node.spec.start) - startReal));
  return <>{node.spec.estimated ? "≈" : ""}{text}</>;
}

export interface Present<T> {
  key: string;
  item: T;
  leaving: boolean;
  leftAt: number;
}

function mergePresence<T>(prev: Present<T>[], next: readonly T[], getKey: (item: T) => string): Present<T>[] {
  if (
    prev.length === next.length
    && prev.every((entry, index) => !entry.leaving && entry.key === getKey(next[index]) && entry.item === next[index])
  ) {
    return prev;
  }
  const nextKeys = new Set(next.map(getKey));
  const after = new Map<string | null, Present<T>[]>();
  let anchor: string | null = null;
  let now = 0;
  for (const entry of prev) {
    if (nextKeys.has(entry.key)) {
      anchor = entry.key;
      continue;
    }
    const list = after.get(anchor) ?? [];
    if (entry.leaving) {
      list.push(entry);
    } else {
      if (!now) now = performance.now();
      list.push({ ...entry, leaving: true, leftAt: now });
    }
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

export function usePresence<T>(items: readonly T[], getKey: (item: T) => string, exitMs: number): Present<T>[] {
  const [tracked, setTracked] = useState<{ items: readonly T[]; getKey: (item: T) => string; entries: Present<T>[] }>(() => ({
    items,
    getKey,
    entries: items.map((item) => ({ key: getKey(item), item, leaving: false, leftAt: 0 })),
  }));
  let entries = tracked.entries;
  if (tracked.items !== items || tracked.getKey !== getKey) {
    entries = mergePresence(tracked.entries, items, getKey);
    setTracked({ items, getKey, entries });
  }
  let earliestLeftAt = Infinity;
  for (const entry of entries) {
    if (entry.leaving && entry.leftAt < earliestLeftAt) earliestLeftAt = entry.leftAt;
  }
  const hasLeaving = earliestLeftAt !== Infinity;
  useEffect(() => {
    if (!hasLeaving) return;
    const due = earliestLeftAt + exitMs - performance.now();
    const timer = window.setTimeout(() => {
      const cutoff = performance.now() - exitMs + 16;
      setTracked((prev) => {
        const filtered = prev.entries.filter((entry) => !entry.leaving || entry.leftAt > cutoff);
        return filtered.length === prev.entries.length ? prev : { ...prev, entries: filtered };
      });
    }, Math.max(0, due));
    return () => window.clearTimeout(timer);
  }, [hasLeaving, earliestLeftAt, exitMs]);
  return entries;
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
