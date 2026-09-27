"use client";

/* ─────────────────────────────────────────────────────────
 * TOOL CHIPS — Tool-call ledger rows & diff summary chips.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * ───────────────────────────────────────────────────────── */

import React from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { DiffStat } from "@/components/chat/ui";
import { ActivityItem, ActivityRail } from "@/components/chat/timeline/ActivityItem";
import { ToolEntry } from "@/components/chat/timeline/ToolEntry";

export interface ToolItem {
  id: string;
  name: string;
  status: "pending" | "running" | "completed" | "failed" | "cancelled" | "declined";
  durationMs?: number;
  chip?: string;
  mono?: boolean;
  argsSummary?: string;
  outputSummary?: string;
  error?: string;
  icon?: "think" | "write" | "run" | "read" | "search" | string;
  detailLines?: Array<{ text: string; tone?: "add" | "del" | "info" }>;
}

export interface DiffItem {
  file: string;
  add: number;
  del: number;
  sha256?: string;
}

export interface ToolChipsProps {
  tools: ToolItem[];
  diffs?: DiffItem[];
  activeToolId?: string | null;
  onSelectTool?: (toolId: string) => void;
  onSelectDiff?: (diff: DiffItem) => void;
  /** Edit tools get "在变更中查看"; receives the edited file when known. */
  onOpenToolDiff?: (tool: ToolItem, filePath?: string) => void;
  threadId?: string;
  /** Render bare rail items so a parent ActivityRail can own the list. */
  bare?: boolean;
  className?: string;
}

export function ToolChips({
  tools,
  diffs = [],
  activeToolId,
  onSelectTool,
  onSelectDiff,
  onOpenToolDiff,
  threadId,
  bare = false,
  className = "",
}: ToolChipsProps) {
  if (!tools.length && !diffs.length) return null;

  const items = (
    <>
      {tools.map((tool) => (
        <ToolEntry
          key={tool.id}
          tool={tool}
          active={activeToolId === tool.id}
          threadId={threadId}
          onSelect={onSelectTool ? () => onSelectTool(tool.id) : undefined}
          onOpenDiff={onOpenToolDiff ? (filePath) => onOpenToolDiff(tool, filePath) : undefined}
        />
      ))}
      {diffs.length ? (
        <ActivityItem icon="fileDiff">
          <div className="flex flex-wrap gap-1.5 py-1">
            {diffs.map((diff) => (
              <button
                key={diff.sha256 || diff.file}
                type="button"
                onClick={() => onSelectDiff?.(diff)}
                className="cx-press inline-flex h-7 max-w-full items-center gap-1.5 rounded-lg border border-cx-border bg-cx-elevated px-2 font-cx-mono text-[11.5px] text-cx-fg-2 hover:border-cx-border-strong hover:text-cx-fg"
              >
                <Icon name="file" size={12} className="shrink-0 text-cx-fg-4" />
                <span className="min-w-0 truncate">{diff.file}</span>
                <DiffStat additions={diff.add} deletions={diff.del} className="text-[11px]" />
              </button>
            ))}
          </div>
        </ActivityItem>
      ) : null}
    </>
  );

  if (bare) return items;
  return <ActivityRail className={cn(className)}>{items}</ActivityRail>;
}
