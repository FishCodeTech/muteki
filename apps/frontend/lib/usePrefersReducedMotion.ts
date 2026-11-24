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

export function usePrefersReducedMotion(): boolean {
  return useSyncExternalStore(subscribe, prefersReducedMotion, readServerPreference);
}

export function scrollBehavior(): ScrollBehavior {
  return prefersReducedMotion() ? "auto" : "smooth";
}
