"use client";

import { useSyncExternalStore } from "react";
import { readMotionPreference, subscribeMotionPreference } from "./motionPreference";

const QUERY = "(prefers-reduced-motion: reduce)";

function subscribe(onChange: () => void) {
  const media = window.matchMedia(QUERY);
  media.addEventListener("change", onChange);
  const unsubscribe = subscribeMotionPreference(onChange);
  return () => {
    media.removeEventListener("change", onChange);
    unsubscribe();
  };
}

export const prefersReducedMotion = () => typeof window !== "undefined"
  && (readMotionPreference() === "reduce" || window.matchMedia(QUERY).matches);
const readServerPreference = () => false;

/**
 * Live `prefers-reduced-motion` flag: re-renders when the OS setting flips,
 * so animation durations follow it without waiting for another state change.
 */
export function usePrefersReducedMotion(): boolean {
  return useSyncExternalStore(subscribe, prefersReducedMotion, readServerPreference);
}
