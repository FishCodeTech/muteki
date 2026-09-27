"use client";

import { useEffect, useRef, useState } from "react";

import { usePrefersReducedMotion } from "./usePrefersReducedMotion";

export type TransitionPhase = "closed" | "opening" | "open" | "closing";
export type AnimatedPresenceState = "open" | "closed";

export type TransitionStateOptions = {
  durationMs?: number;
  durationVar?: string;
};

function readDurationMs(durationVar: string | undefined, fallback: number, reduceMotion: boolean): number {
  if (reduceMotion) return 0;
  if (typeof document === "undefined" || !durationVar) return fallback;
  const raw = getComputedStyle(document.documentElement).getPropertyValue(durationVar).trim();
  if (!raw) return fallback;
  const durations = raw.split(",").map((value) => {
    const normalized = value.trim().toLowerCase();
    const parsed = Number.parseFloat(normalized);
    if (!Number.isFinite(parsed)) return fallback;
    return normalized.endsWith("ms") ? parsed : normalized.endsWith("s") ? parsed * 1000 : parsed;
  });
  return Math.max(...durations, 0);
}

/**
 * Presence + transitions.dev open/close phases.
 * Mounts in the rest state, then adds `.is-open` on the next frame so CSS
 * transitions actually play. On close, swaps to `.is-closing` and unmounts
 * after the recipe's close duration.
 */
export function useTransitionState(open: boolean, options: TransitionStateOptions = {}) {
  const durationMs = options.durationMs ?? 180;
  const durationVar = options.durationVar;
  const prefersReducedMotion = usePrefersReducedMotion();
  const mounted = useRef(false);
  const [present, setPresent] = useState(open);
  const [phase, setPhase] = useState<TransitionPhase>(open ? "opening" : "closed");

  useEffect(() => {
    let frameA = 0;
    let frameB = 0;
    let timer = 0;

    if (open) {
      setPresent(true);
      // Reverse an interrupted exit from its current position. Resetting to
      // the resting pose here makes fast toggles visibly jump.
      if (mounted.current || prefersReducedMotion) {
        mounted.current = true;
        setPhase("open");
        return;
      }
      mounted.current = true;
      setPhase("opening");
      frameA = window.requestAnimationFrame(() => {
        frameB = window.requestAnimationFrame(() => setPhase("open"));
      });
      return () => {
        window.cancelAnimationFrame(frameA);
        window.cancelAnimationFrame(frameB);
      };
    }

    if (!mounted.current || prefersReducedMotion) {
      mounted.current = false;
      setPresent(false);
      setPhase("closed");
      return;
    }
    setPhase((current) => (current === "closed" ? "closed" : "closing"));
    timer = window.setTimeout(() => {
      mounted.current = false;
      setPresent(false);
      setPhase("closed");
    }, readDurationMs(durationVar, durationMs, false));
    return () => window.clearTimeout(timer);
  }, [durationMs, durationVar, open, prefersReducedMotion]);

  const openClass = phase === "open" ? "is-open" : phase === "closing" ? "is-closing" : "";

  return {
    present,
    phase,
    openClass,
    state: (phase === "open" ? "open" : "closed") as AnimatedPresenceState,
  };
}

export function transitionClass(base: string, phase: TransitionPhase, extra = ""): string {
  const bits = [base];
  if (phase === "open") bits.push("is-open");
  if (phase === "closing") bits.push("is-closing");
  if (extra) bits.push(extra);
  return bits.filter(Boolean).join(" ");
}
