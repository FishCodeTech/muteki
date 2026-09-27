"use client";

import React from "react";
import { cn } from "@/lib/cn";
import { ToolChips, type ToolItem, type DiffItem } from "../ai-native/tool-chips";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";

export interface ConversationToolFlowProps {
  tools: ToolItem[];
  diffs?: DiffItem[];
  activeToolId?: string | null;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  className?: string;
  threadId?: string;
  /** Edit tools offer "在变更中查看"; receives the edited path when known. */
  onOpenToolDiff?: (tool: ToolItem, filePath?: string) => void;
  /** Emit bare rail items for a parent ActivityRail. */
  bare?: boolean;
}

export function toolDrawerPayload(tool: ToolItem): DrawerDetailPayload {
  return {
    type: tool.status === "failed" ? "error" : "tool",
    title: tool.name,
    subtitle: tool.chip || tool.argsSummary,
    status: tool.status,
    toolName: tool.name,
    input: tool.argsSummary,
    output: tool.outputSummary,
    terminalOutput: tool.outputSummary,
    error: tool.error,
  };
}

export function ConversationToolFlow({
  tools,
  diffs = [],
  activeToolId,
  onOpenDrawer,
  className = "",
  threadId,
  onOpenToolDiff,
  bare = false,
}: ConversationToolFlowProps) {
  if (!tools.length && !diffs.length) return null;

  const handleSelectTool = (toolId: string) => {
    const found = tools.find((tool) => tool.id === toolId);
    if (found) onOpenDrawer(toolDrawerPayload(found));
  };

  const handleSelectDiff = (diff: DiffItem) => {
    onOpenDrawer({
      type: "diff",
      title: `Diff: ${diff.file}`,
      subtitle: `+${diff.add} −${diff.del}`,
      diff: { file: diff.file, add: diff.add, del: diff.del },
    });
  };

  const chips = (
    <ToolChips
      tools={tools}
      diffs={diffs}
      activeToolId={activeToolId}
      onSelectTool={handleSelectTool}
      onSelectDiff={handleSelectDiff}
      onOpenToolDiff={onOpenToolDiff}
      threadId={threadId}
      bare={bare}
    />
  );
  if (bare) return chips;
  return <div className={cn("my-1.5", className)}>{chips}</div>;
}
