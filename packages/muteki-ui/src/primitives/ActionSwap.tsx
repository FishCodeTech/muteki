"use client";

import { useEffect, useState, type ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";
import { EASE_OUT, SPRING_SWAP } from "./motion";

export interface ActionSwapIconProps {
  value: string | number;
  children: ReactNode;
  animation?: "blur" | "roll";
  className?: string;
}

export function ActionSwapIcon({
  value,
  children,
  animation = "blur",
  className,
}: ActionSwapIconProps) {
  const reduced = useReducedMotion();
  const [mounted, setMounted] = useState(false);

  useEffect(() => {
    setMounted(true);
  }, []);

  if (!mounted || reduced) {
    return (
      <span className={cn("relative inline-grid shrink-0 place-items-center", className)}>
        <span className="col-start-1 row-start-1 inline-flex items-center justify-center">{children}</span>
      </span>
    );
  }

  const initial =
    animation === "roll"
      ? { opacity: 0, y: 6 }
      : { opacity: 0, scale: 0.72, filter: "blur(3px)" };

  const animate =
    animation === "roll"
      ? { opacity: 1, y: 0, transition: SPRING_SWAP }
      : {
          opacity: 1,
          scale: 1,
          filter: "blur(0px)",
          transition: SPRING_SWAP,
          transitionEnd: { filter: "none" },
        };

  const exit =
    animation === "roll"
      ? { opacity: 0, y: -6, transition: { duration: 0.12, ease: EASE_OUT } }
      : { opacity: 0, scale: 0.72, filter: "blur(3px)", transition: { duration: 0.12, ease: EASE_OUT } };

  return (
    <span className={cn("relative inline-grid shrink-0 place-items-center", className)}>
      <AnimatePresence initial={false}>
        <motion.span
          key={String(value)}
          initial={initial}
          animate={animate}
          exit={exit}
          className="col-start-1 row-start-1 inline-flex items-center justify-center will-change-transform"
        >
          {children}
        </motion.span>
      </AnimatePresence>
    </span>
  );
}

export interface ActionSwapRollTextProps {
  value: string | number;
  children: ReactNode;
  className?: string;
}

export function ActionSwapRollText({ value, children, className }: ActionSwapRollTextProps) {
  const reduced = useReducedMotion();
  const [mounted, setMounted] = useState(false);

  useEffect(() => {
    setMounted(true);
  }, []);

  if (!mounted || reduced) {
    return (
      <span className={cn("inline-block whitespace-nowrap", className)}>
        {children}
      </span>
    );
  }

  return (
    <span
      className={cn("relative inline-grid whitespace-nowrap align-baseline", className)}
      style={{ clipPath: "inset(0 -999px)" }}
    >
      <span aria-hidden="true" className="invisible col-start-1 row-start-1 inline-block whitespace-nowrap">
        {children}
      </span>
      <AnimatePresence initial={false}>
        <motion.span
          key={String(value)}
          initial={{ y: "90%", opacity: 0 }}
          animate={{ y: "0%", opacity: 1, transition: SPRING_SWAP }}
          exit={{ y: "-90%", opacity: 0, transition: { duration: 0.14, ease: EASE_OUT } }}
          className="col-start-1 row-start-1 inline-block whitespace-nowrap will-change-transform"
        >
          {children}
        </motion.span>
      </AnimatePresence>
    </span>
  );
}
