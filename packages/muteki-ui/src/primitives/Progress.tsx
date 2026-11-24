"use client";

import type { ReactNode } from "react";
import { cn } from "../utils/cn";
import type { AlertTone } from "./Feedback";

const BAR_FILL: Record<AlertTone, string> = {
  neutral: "bg-cx-fg-3",
  accent: "bg-cx-accent",
  info: "bg-[var(--ui-info)]",
  success: "bg-cx-success",
  warning: "bg-cx-warning",
  danger: "bg-cx-danger",
  running: "bg-cx-accent",
};

export function ProgressBar({
  value,
  max = 100,
  indeterminate,
  label,
  valueLabel,
  tone = "accent",
  size = "md",
  ariaLabel,
  className,
}: {
  value?: number;
  max?: number;
  indeterminate?: boolean;
  label?: ReactNode;
  valueLabel?: ReactNode;
  tone?: AlertTone;
  size?: "sm" | "md";
  ariaLabel?: string;
  className?: string;
}) {
  const safeMax = Math.max(1, max);
  const clamped = value === undefined ? 0 : Math.min(100, Math.max(0, (value / safeMax) * 100));
  const isIndeterminate = Boolean(indeterminate || value === undefined);

  return (
    <div
      data-slot="progress-bar"
      role="progressbar"
      aria-label={ariaLabel ?? (typeof label === "string" ? label : undefined)}
      aria-valuemin={0}
      aria-valuemax={safeMax}
      aria-valuenow={isIndeterminate ? undefined : value}
      className={cn("flex w-full flex-col gap-1.5", className)}
    >
      {label || valueLabel ? (
        <div className="flex items-center justify-between gap-2 text-cx-xs">
          <span className="truncate font-medium text-cx-fg-2">{label}</span>
          <span className="cx-tabular shrink-0 font-cx-mono text-cx-fg-3">
            {valueLabel ?? (isIndeterminate ? "…" : `${Math.round(clamped)}%`)}
          </span>
        </div>
      ) : null}
      <div
        className={cn(
          "relative w-full overflow-hidden rounded-full border border-cx-border-subtle bg-cx-sunken",
          size === "sm" ? "h-1.5" : "h-2",
        )}
      >
        {isIndeterminate ? (
          <div
            className={cn(
              "h-full w-1/2 rounded-full",
              BAR_FILL[tone],
              "animate-[cx-progress_1.4s_ease-in-out_infinite]",
            )}
          />
        ) : (
          <div
            className={cn("h-full rounded-full transition-[width] duration-200 ease-cx-out", BAR_FILL[tone])}
            style={{ width: `${clamped}%` }}
          />
        )}
      </div>
    </div>
  );
}
