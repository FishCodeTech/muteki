"use client";

import { RefObject, useEffect, useRef } from "react";
import { animate, stagger } from "animejs";

type DeckMotionState = {
  flagCount: number;
};

function reducedMotion(): boolean {
  return typeof window !== "undefined"
    && window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
}

function scoped(root: HTMLElement, selector: string): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(selector));
}

/**
 * Deck motion. The deck's ENTRANCE animations (mount choreography, run-swap,
 * artifact/inspector open, chat bubbles) are all CSS keyframes in globals.css —
 * NOT here. That is a deliberate architecture choice learned the hard way:
 *
 *   Driving an `opacity:[0,1]` entrance with anime.js on an element React owns is
 *   fragile. anime writes opacity:0 at frame 0 and clears the inline style only in
 *   `onComplete`; but React re-renders these elements constantly (streaming SSE)
 *   or remounts them (run swap, artifact open) — which ORPHANS the in-flight anime
 *   instance so `onComplete` never fires, freezing the element at opacity:0. We hit
 *   this on bubbles AND the run-inspector. A keyed CSS keyframe has no JS instance
 *   to orphan: it runs once on mount and is immune to re-render.
 *
 * The ONE thing that stays on anime.js is the flag-milestone pulse below: it is
 * event-triggered (not mount-triggered), runs on stable chips that don't unmount,
 * and tweens `scale:[1, 1.018, 1]` — ending back at identity. Even if it were
 * orphaned it would rest at scale(1), the natural state, so it cannot freeze.
 */
export function useDeckMotion(rootRef: RefObject<HTMLElement | null>, state: DeckMotionState) {
  const lastFlags = useRef(state.flagCount);

  // ── Flag milestone pulse ───────────────────────────────────────────────────
  // ONLY on a real flag-count advance — that is a milestone worth a beat. Cost is
  // deliberately NOT a trigger: `deck.usd` ticks continuously during a run, so
  // pulsing every `.motion-feedback` chip on each cost update was a constant
  // "twitch". A monotonically rising flag count is rare and meaningful.
  useEffect(() => {
    const root = rootRef.current;
    const advanced = state.flagCount > lastFlags.current;
    lastFlags.current = state.flagCount;
    if (!root || reducedMotion() || !advanced) return;
    const targets = scoped(root, ".motion-feedback");
    if (!targets.length) return;
    animate(targets, {
      scale: [1, 1.018, 1],
      duration: 360,
      delay: stagger(18),
      ease: "outQuart",
    });
  }, [rootRef, state.flagCount]);
}
