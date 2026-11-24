"use client";

import { useSyncExternalStore } from "react";

export type Ticker = {
  subscribe: (listener: () => void) => () => void;
  read: () => number;
};

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

export function useTicker(ticker: Ticker, active = true): number {
  return useSyncExternalStore(active ? ticker.subscribe : subscribeNothing, ticker.read, ticker.read);
}

export const SECOND_TICKER = createTicker(1000);
export const COARSE_TICKER = createTicker(30_000);
