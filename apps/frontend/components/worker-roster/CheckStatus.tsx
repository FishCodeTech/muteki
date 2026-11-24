// ai-design-check-ignore-file use-shared-components -- Button disables pointer events when disabled, which would hide the status tooltip
"use client";

import { StatusDot, Tooltip, type Tone } from "@muteki/ui";
import { cn } from "@/lib/cn";
import type { WorkerModelTestResult } from "@/lib/useRun";
import { CHECK_LABEL, checkDetail, seatCheckState, type SeatCheckState } from "./model";

const TONE: Record<SeatCheckState, Tone> = {
  off: "neutral",
  checking: "running",
  ok: "success",
  bad: "danger",
  idle: "warning",
};

const TEXT: Record<SeatCheckState, string> = {
  off: "text-cx-fg-4",
  checking: "text-cx-accent",
  ok: "text-cx-fg-2",
  bad: "text-cx-danger",
  idle: "text-cx-fg-3",
};

export function CheckStatus({
  enabled, testing, result, onCheck, className,
}: {
  enabled: boolean;
  testing: boolean;
  result?: WorkerModelTestResult;
  onCheck?: () => void;
  className?: string;
}) {
  const state = seatCheckState(enabled, testing, result);
  const interactive = Boolean(onCheck) && enabled && !testing;
  return (
    <Tooltip content={checkDetail(enabled, testing, result)} placement="top">
      <button
        type="button"
        disabled={!interactive}
        onClick={onCheck}
        aria-live="polite"
        className={cn(
          "inline-flex h-7 min-w-0 items-center gap-1.5 whitespace-nowrap rounded-md px-1.5 text-cx-xs",
          TEXT[state],
          interactive ? "hover:bg-cx-hover" : "cursor-default",
          className,
        )}
      >
        <StatusDot tone={TONE[state]} />
        {CHECK_LABEL[state]}
      </button>
    </Tooltip>
  );
}
