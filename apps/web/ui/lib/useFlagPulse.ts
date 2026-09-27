"use client";

import { RefObject, useEffect, useRef } from "react";

import { usePrefersReducedMotion } from "./usePrefersReducedMotion";

/**
 * Event-driven flag milestone pulse. CSS-only (`t-flag-pulse`) so a React
 * re-render cannot orphan a JS animation instance.
 */
export function useFlagPulse(rootRef: RefObject<HTMLElement | null>, flagCount: number) {
  const lastFlags = useRef(flagCount);
  const reduceMotion = usePrefersReducedMotion();

  useEffect(() => {
    const root = rootRef.current;
    const advanced = flagCount > lastFlags.current;
    lastFlags.current = flagCount;
    if (!root || !advanced || reduceMotion) return;
    for (const node of root.querySelectorAll<HTMLElement>(".t-flag-target")) {
      node.classList.remove("t-flag-pulse");
      void node.offsetWidth;
      node.classList.add("t-flag-pulse");
    }
  }, [flagCount, reduceMotion, rootRef]);
}
