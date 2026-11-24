"use client";

import { useSyncExternalStore } from "react";

let cardHeight = 0;
const listeners = new Set<() => void>();

export function setThreadDetailsCardHeight(next: number) {
  const value = Math.max(0, Math.round(next));
  if (value === cardHeight) return;
  cardHeight = value;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useThreadDetailsCardHeight(): number {
  return useSyncExternalStore(subscribe, () => cardHeight, () => 0);
}

const CARD_TOP_IN_STREAM = 8;
const GAP = 8;

export function streamCornerTop(baseTop: number, detailsHeight: number): number {
  if (!detailsHeight) return baseTop;
  return Math.max(baseTop, CARD_TOP_IN_STREAM + detailsHeight + GAP + (baseTop - 12));
}
