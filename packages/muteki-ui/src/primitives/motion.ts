"use client";

import type { Transition, Variants } from "motion/react";
import { usePrefersReducedMotion, useReducedMotion } from "../utils/reducedMotion";

export const EASE_OUT = [0.16, 1, 0.3, 1] as const;
export const EASE_IN_OUT = [0.77, 0, 0.175, 1] as const;
export const EASE_DRAWER = [0.32, 0.72, 0, 1] as const;
export const EASE_OUT_CSS = "var(--ui-ease-out, var(--cx-ease-out))";

export function resolveEaseOutWaapi(): string {
  if (typeof document === "undefined") return "ease-out";
  return (
    getComputedStyle(document.documentElement).getPropertyValue("--cx-ease-out").trim() ||
    "cubic-bezier(0.16, 1, 0.3, 1)"
  );
}

export const SPRING_PRESS = {
  type: "spring",
  stiffness: 500,
  damping: 30,
  mass: 0.6,
} as const;

export const SPRING_SWAP = {
  type: "spring",
  stiffness: 460,
  damping: 30,
  mass: 0.55,
} as const;

export const SPRING_PANEL = {
  type: "spring",
  stiffness: 420,
  damping: 40,
  mass: 0.5,
} as const;

export const SPRING_LAYOUT = {
  type: "spring",
  stiffness: 360,
  damping: 32,
  mass: 0.6,
} as const;

export const SPRING_MOUSE = {
  stiffness: 200,
  damping: 15,
  mass: 0.3,
} as const;

export const SPRING_GLIDE = {
  stiffness: 700,
  damping: 50,
  mass: 0.5,
} as const;

export const SPRING_SOFT: Transition = { type: "spring", stiffness: 260, damping: 30, mass: 0.8 };

export const FADE_FAST: Transition = { duration: 0.14, ease: EASE_OUT };
export const FADE: Transition = { duration: 0.2, ease: EASE_OUT };
export const REVEAL: Transition = { duration: 0.24, ease: EASE_OUT };

export const revealVariants: Variants = {
  hidden: { opacity: 0, transform: "translateY(6px)" },
  visible: { opacity: 1, transform: "translateY(0px)", transition: REVEAL },
  exit: { opacity: 0, transform: "translateY(4px)", transition: FADE_FAST },
};

export const popVariants: Variants = {
  hidden: { opacity: 0, transform: "translateY(-4px) scale(0.96)" },
  visible: { opacity: 1, transform: "translateY(0px) scale(1)", transition: { duration: 0.16, ease: EASE_OUT } },
  exit: { opacity: 0, transform: "translateY(-2px) scale(0.97)", transition: { duration: 0.11, ease: EASE_OUT } },
};

export { usePrefersReducedMotion, useReducedMotion };

export function motionSafe<T extends Record<string, unknown>>(reduced: boolean, full: T, minimal: T): T {
  return reduced ? minimal : full;
}
