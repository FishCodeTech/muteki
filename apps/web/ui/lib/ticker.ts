"use client";

import { useSyncExternalStore } from "react";

export type Ticker = {
  subscribe: (listener: () => void) => () => void;
  /** Epoch ms floored to the step; refreshed on subscribe and on every interval. */
  read: () => number;
};

/**
 * One interval per step size, shared by every subscriber; the timer only runs
 * while someone is subscribed, so an idle canvas or a finished run costs nothing.
 */
export function createTicker(stepMs: number): Ticker {
  const listeners = new Set<() => void>();
  let timer = 0;
  let value = Math.floor(Date.now() / stepMs) * stepMs;
  const refresh = () => {
    value = Math.floor(Date.now() / stepMs) * stepMs;
  };
  return {
    subscribe(listener) {
      listeners.add(listener);
      if (!timer) {
        // Refresh before the first tick so a remounted subscriber reads the current step, not the one the timer last wrote.
        refresh();
        timer = window.setInterval(() => {
          refresh();
          for (const notify of listeners) notify();
        }, stepMs);
      }
      return () => {
        listeners.delete(listener);
        if (!listeners.size) {
          window.clearInterval(timer);
          timer = 0;
        }
      };
    },
    read: () => value,
  };
}

const subscribeNothing = () => () => {};

/** Current ticker value; `active` false leaves the component unsubscribed (the last value is returned unchanged). */
export function useTicker(ticker: Ticker, active = true): number {
  return useSyncExternalStore(active ? ticker.subscribe : subscribeNothing, ticker.read, ticker.read);
}

/** Per-second clock for live elapsed counters. */
export const SECOND_TICKER = createTicker(1000);
/** 30-second clock for recency / idle judgements and edge animation windows. */
export const COARSE_TICKER = createTicker(30_000);
