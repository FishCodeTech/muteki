"use client";

/* ─────────────────────────────────────────────────────────
 * THINKING — Collapsible reasoning summary with optional trace rows.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * ───────────────────────────────────────────────────────── */

import React, { useId, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Collapse, DiffStat, ShimmerText, Spinner } from "@/components/chat/ui";
import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";

export interface ThinkingRow {
  primary: string;
  secondary?: string;
  mono?: boolean;
  add?: number;
  del?: number;
  status?: "running" | "completed" | "failed";
  href?: string;
  onClick?: () => void;
}

export interface ThinkingProps {
  working?: boolean;
  durationMs?: number;
  summary?: string;
  rows?: ThinkingRow[];
  activeLabel?: string;
  doneLabel?: string;
  defaultExpanded?: boolean;
  /** Parent already draws a timeline rail; skip the nested left border. */
  bare?: boolean;
  className?: string;
}

export function formatThinkingDuration(ms?: number): string {
  if (ms == null || Number.isNaN(ms) || ms <= 0) return "";
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds < 10 ? seconds.toFixed(1) : Math.round(seconds)} 秒`;
  return `${Math.floor(seconds / 60)} 分 ${Math.round(seconds % 60)} 秒`;
}

function RowStatus({ status }: { status?: ThinkingRow["status"] }) {
  if (status === "running") return <Spinner size={12} className="text-cx-accent" />;
  if (status === "failed") return <Icon name="x" size={13} className="text-cx-danger" />;
  return <Icon name="check" size={13} className="text-cx-fg-4" />;
}

export function Thinking({
  working = false,
  durationMs,
  summary = "",
  rows = [],
  activeLabel = "正在思考…",
  doneLabel,
  defaultExpanded,
  bare = false,
  className = "",
}: ThinkingProps) {
  const [manualExpanded, setManualExpanded] = useState<boolean | null>(null);
  const disclosureId = useId();
  const expanded = manualExpanded ?? defaultExpanded ?? working;
  const duration = formatThinkingDuration(durationMs);
  const resolvedDoneLabel = doneLabel || (duration ? `已思考 ${duration}` : "思考过程");
  const hasContent = Boolean(summary.trim() || rows.length > 0);

  return (
    <div className={cn("flex w-full flex-col", className)}>
      <button
        type="button"
        aria-expanded={hasContent ? expanded : undefined}
        aria-controls={hasContent && expanded ? disclosureId : undefined}
        disabled={!hasContent}
        onClick={() => setManualExpanded((current) => !(current ?? defaultExpanded ?? working))}
        className="cx-press group -ml-1.5 inline-flex h-7 w-fit items-center gap-1.5 rounded-lg px-1.5 text-[13px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg-2 disabled:pointer-events-none"
      >
        {working ? <ShimmerText className="font-medium">{activeLabel}</ShimmerText> : <span className="font-medium">{resolvedDoneLabel}</span>}
        {hasContent ? (
          <Icon name="chevronRight" size={12} className={cn("text-cx-fg-4 transition-transform duration-150 ease-cx-out", expanded && "rotate-90")} />
        ) : null}
      </button>

      {hasContent ? (
        <Collapse open={expanded}>
          <div id={disclosureId} className={cn("mb-1 mt-1", !bare && "ml-[6px] border-l border-cx-border pl-3.5")}>
            {summary ? <ChatMarkdown text={summary} streaming={working} size="sm" className="text-cx-fg-3" /> : null}
            {rows.length ? (
              <ul className="m-0 flex list-none flex-col gap-0.5 px-0 pb-0 pt-1.5">
                {rows.map((row, index) => {
                  const content = (
                    <>
                      <RowStatus status={row.status} />
                      <span className="min-w-0 truncate text-[13px] font-medium text-cx-fg-2">{row.primary}</span>
                      {row.secondary ? (
                        <span className={cn("min-w-0 truncate text-[12px] text-cx-fg-4", row.mono && "font-cx-mono")}>{row.secondary}</span>
                      ) : null}
                      {row.add !== undefined ? <DiffStat additions={row.add} deletions={row.del ?? 0} className="ml-auto shrink-0 text-[12px]" /> : null}
                    </>
                  );
                  const rowClass = "flex h-7 w-full min-w-0 items-center gap-2 rounded-md px-1.5 text-left transition-colors hover:bg-cx-hover";
                  if (row.href) {
                    return (
                      <li key={index}>
                        <a href={row.href} target="_blank" rel="noreferrer" className={rowClass}>{content}</a>
                      </li>
                    );
                  }
                  if (row.onClick) {
                    return (
                      <li key={index}>
                        <button type="button" onClick={row.onClick} className={rowClass}>{content}</button>
                      </li>
                    );
                  }
                  return <li key={index} className={rowClass}>{content}</li>;
                })}
              </ul>
            ) : null}
          </div>
        </Collapse>
      ) : null}
    </div>
  );
}
