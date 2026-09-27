"use client";

import type { Transition, Variants } from "motion/react";
import { EASE_OUT } from "@/components/agentui/lib/ease";
import { usePrefersReducedMotion } from "@/lib/usePrefersReducedMotion";

export {
  EASE_DRAWER,
  EASE_IN_OUT,
  EASE_OUT,
  SPRING_LAYOUT,
  SPRING_PANEL,
  SPRING_PRESS,
  SPRING_SWAP,
} from "@/components/agentui/lib/ease";

export const SPRING_SOFT: Transition = { type: "spring", stiffness: 260, damping: 30, mass: 0.8 };

export const FADE_FAST: Transition = { duration: 0.14, ease: EASE_OUT };
export const FADE: Transition = { duration: 0.2, ease: EASE_OUT };
export const REVEAL: Transition = { duration: 0.24, ease: EASE_OUT };

export const revealVariants: Variants = {
  hidden: { opacity: 0, y: 8, filter: "blur(4px)" },
  visible: { opacity: 1, y: 0, filter: "blur(0px)", transition: REVEAL },
  exit: { opacity: 0, y: 4, filter: "blur(2px)", transition: FADE_FAST },
};

export const popVariants: Variants = {
  hidden: { opacity: 0, scale: 0.96, y: -4 },
  visible: { opacity: 1, scale: 1, y: 0, transition: { duration: 0.16, ease: EASE_OUT } },
  exit: { opacity: 0, scale: 0.97, y: -2, transition: { duration: 0.11, ease: EASE_OUT } },
};

/** Honors both the OS setting and Muteki's in-app motion preference. */
export function useReducedMotion(): boolean {
  return usePrefersReducedMotion();
}

/** Collapses variants to opacity-only when motion is reduced. */
export function motionSafe<T extends Record<string, unknown>>(reduced: boolean, full: T, minimal: T): T {
  return reduced ? minimal : full;
}
