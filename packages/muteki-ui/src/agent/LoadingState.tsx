"use client";

import { useEffect, useState } from "react";
import { cn } from "../utils/cn";
import { ShimmerText, Spinner } from "../primitives/Feedback";

function formatElapsed(ms: number): string {
  const total = Math.max(0, ms) / 1000;
  if (total < 60) return `${total.toFixed(total < 10 ? 1 : 0)}s`;
  return `${Math.floor(total / 60)}m ${Math.round(total % 60)}s`;
}

function ElapsedClock({ startMs }: { startMs: number }) {
  const [elapsedMs, setElapsedMs] = useState(0);
  useEffect(() => {
    const tick = () => setElapsedMs(Date.now() - startMs);
    tick();
    const id = window.setInterval(tick, 1000);
    return () => window.clearInterval(id);
  }, [startMs]);
  return (
    <span className="cx-tabular font-cx-mono text-cx-xs text-cx-fg-4">
      {formatElapsed(elapsedMs)}
    </span>
  );
}

export interface LoadingStateProps {
  label?: string;
  variant?: "Drive" | "Dots" | "Orbit";
  elapsedSeconds?: number;
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
  className,
}: LoadingStateProps) {
  const [startMs] = useState(() => Date.now());
  return (
    <div
      data-slot="loading-state"
      className={cn("inline-flex w-fit items-center gap-2.5 text-cx-fg-3", className)}
      role="status"
    >
      {variant === "Dots" ? <Dots /> : <Spinner size={15} className="text-cx-fg-3" />}
      <ShimmerText className="text-cx-sm font-medium">{label}</ShimmerText>
      {elapsedSeconds !== undefined ? (
        <span className="cx-tabular font-cx-mono text-cx-xs text-cx-fg-4">
          {formatElapsed(elapsedSeconds * 1000)}
        </span>
      ) : showTimer ? (
        <ElapsedClock startMs={startMs} />
      ) : null}
    </div>
  );
}
