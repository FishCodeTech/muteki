"use client";

import React from "react";
import { cn } from "@/lib/cn";
import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";
import { Thinking, type ThinkingRow } from "@muteki/ui/agent";

export interface ConversationThinkingProps {
  working?: boolean;
  durationMs?: number;
  summary?: string;
  rows?: ThinkingRow[];
  activeLabel?: string;
  className?: string;
  bare?: boolean;
}

export function ConversationThinking({
  working = false,
  durationMs,
  summary,
  rows = [],
  activeLabel = "正在思考…",
  className = "",
  bare = false,
}: ConversationThinkingProps) {
  if (!working && !summary && rows.length === 0) return null;

  return (
    <div className={cn(!bare && "my-1.5", className)}>
      <Thinking
        working={working}
        durationMs={durationMs}
        summary={summary}
        rows={rows}
        activeLabel={activeLabel}
        bare={bare}
        renderMarkdown={(text, streaming) => (
          <ChatMarkdown text={text} streaming={streaming} size="sm" className="text-cx-fg-3" />
        )}
      />
    </div>
  );
}
