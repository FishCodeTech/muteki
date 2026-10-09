"use client";

/* ─────────────────────────────────────────────────────────
 * LOADING STATE — Spinner + shimmer label with an optional elapsed clock.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * ───────────────────────────────────────────────────────── */

import React, { useState } from "react";
import { cn } from "@/lib/cn";
import { ShimmerText, Spinner } from "@/components/chat/ui";
import { ElapsedTimer } from "@/components/chat/timeline/ElapsedTimer";

function formatElapsed(ms: number): string {
  const total = ms / 1000;
  if (total < 60) return `${total.toFixed(total < 10 ? 1 : 0)}s`;
  return `${Math.floor(total / 60)}m ${Math.round(total % 60)}s`;
}

export interface LoadingStateProps {
  label?: string;
  variant?: "Drive" | "Dots" | "Orbit";
  elapsedSeconds?: number;
  /** Show a self-running elapsed clock when `elapsedSeconds` is not given. */
  showTimer?: boolean;
  className?: string;
}

function Dots() {
  return (
    <span aria-hidden className="inline-flex items-center gap-[3px]">
      {[0, 160, 320].map((delay) => (
        <span
          key={delay}
          className="cx-pulse-dot size-[5px] rounded-full bg-cx-fg-3"
          style={{ animationDelay: `${delay}ms` }}
        />
      ))}
    </span>
  );
}

export function LoadingState({
  label = "Agent 正在执行…",
  variant = "Orbit",
  elapsedSeconds,
  showTimer = false,
  className = "",
}: LoadingStateProps) {
  const [startMs] = useState(() => Date.now());
  return (
    <div className={cn("inline-flex w-fit items-center gap-2.5 text-cx-fg-3", className)} role="status">
      {variant === "Dots" ? <Dots /> : <Spinner size={15} className="text-cx-fg-3" />}
      <ShimmerText className="text-[13px] font-medium">{label}</ShimmerText>
      {elapsedSeconds !== undefined ? (
        <span className="cx-tabular font-cx-mono text-[12px] text-cx-fg-4">{formatElapsed(elapsedSeconds * 1000)}</span>
      ) : showTimer ? (
        <ElapsedTimer startMs={startMs} format={formatElapsed} className="font-cx-mono text-[12px] text-cx-fg-4" />
      ) : null}
    </div>
  );
}
