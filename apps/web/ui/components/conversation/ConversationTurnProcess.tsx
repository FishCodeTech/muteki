"use client";

import { useEffect, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { ShimmerText } from "@/components/chat/ui";
import { AgentActivity, type AgentActivityItem } from "@/components/agentui/agents/agent-activity";
import { ActivityItem } from "@/components/chat/timeline/ActivityItem";
import { ElapsedTimer } from "@/components/chat/timeline/ElapsedTimer";
import { ToolEntry } from "@/components/chat/timeline/ToolEntry";
import { formatTurnDuration } from "@/components/chat/timeline/toolPresentation";
import { ConversationMessage } from "./ConversationMessage";
import { ConversationThinking } from "./ConversationThinking";
import { toolDrawerPayload } from "./ConversationToolFlow";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import {
  resolveConversationTurnDurationMs,
  type ConversationTurnSegment,
  type ConversationTurnTiming,
} from "./conversationEventViews";

export interface ConversationTurnProcessProps {
  turnId: string;
  status: string;
  segments: ConversationTurnSegment[];
  running?: boolean;
  workingLabel?: string;
  timing?: ConversationTurnTiming;
  turnCreatedAt?: string;
  turnCompletedAt?: string;
  forceOpen?: boolean | null;
  onOpenChange?: (open: boolean) => void;
  planSummary?: string;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  threadId?: string;
  /** Edit tools jump into this turn's diff, optionally focused on a file. */
  onOpenToolDiff?: (turnId: string, filePath?: string) => void;
}

const INTERRUPTED = new Set(["interrupted", "aborted", "cancelled", "canceled"]);
/** Live window before the stream starts gliding (AgentUI activity behaviour). */
const LIVE_MAX_HEIGHT = 420;
/** Expanded after completion: show the whole record inline, no nested scroller. */
const SETTLED_MAX_HEIGHT = 1_000_000;

function settledLabel(status: string, durationMs?: number): string {
  const duration = durationMs == null ? "" : formatTurnDuration(durationMs);
  if (status === "failed") return duration ? `执行失败 · ${duration}` : "执行失败";
  if (INTERRUPTED.has(status)) return duration ? `已中断 · ${duration}` : "已中断";
  return duration ? `已处理 ${duration}` : "工作过程";
}

function parseStart(value?: string): number | null {
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

export function ConversationTurnProcess({
  turnId,
  status,
  segments,
  running = false,
  workingLabel = "",
  timing,
  turnCreatedAt,
  turnCompletedAt,
  forceOpen = null,
  onOpenChange,
  planSummary = "",
  onOpenDrawer,
  threadId,
  onOpenToolDiff,
}: ConversationTurnProcessProps) {
  const [manualOpen, setManualOpen] = useState<boolean | null>(null);
  const settledDurationMs = resolveConversationTurnDurationMs({
    timing,
    createdAt: turnCreatedAt,
    completedAt: turnCompletedAt,
  });
  const liveStart = running && settledDurationMs == null ? parseStart(timing?.startedAt || turnCreatedAt) : null;

  useEffect(() => {
    setManualOpen(null);
  }, [turnId]);

  const counts = useMemo(() => {
    let progress = 0;
    let reasoning = 0;
    let tools = 0;
    let failed = 0;
    for (const segment of segments) {
      if (segment.kind === "text") progress += 1;
      if (segment.kind === "thinking") reasoning += 1;
      if (segment.kind === "tools") {
        tools += segment.tools.length;
        failed += segment.tools.filter((tool) => tool.status === "failed").length;
      }
    }
    return { progress, reasoning, tools, failed };
  }, [segments]);

  const items = useMemo<AgentActivityItem[]>(() => {
    const out: AgentActivityItem[] = [];
    segments.forEach((segment, index) => {
      const last = index === segments.length - 1;
      if (segment.kind === "thinking") {
        out.push({
          id: `thinking-${index}`,
          type: "custom",
          content: (
            <ActivityItem icon="brain" tone={running && last ? "running" : "muted"}>
              <ConversationThinking bare working={running && last} durationMs={segment.durationMs} summary={segment.text} />
            </ActivityItem>
          ),
        });
        return;
      }
      if (segment.kind === "text") {
        out.push({
          id: `progress-${index}`,
          type: "custom",
          content: (
            <ActivityItem icon="messageCircle" tone="muted">
              <div className="py-1">
                <ConversationMessage
                  role="assistant"
                  text={segment.text}
                  isStreaming={running && last}
                  showIdentity={false}
                  showActions={false}
                  threadId={threadId}
                />
              </div>
            </ActivityItem>
          ),
        });
        return;
      }
      for (const tool of segment.tools) {
        out.push({
          id: `tool-${tool.id}`,
          type: "custom",
          content: (
            <ToolEntry
              tool={tool}
              threadId={threadId}
              onSelect={() => onOpenDrawer(toolDrawerPayload(tool))}
              onOpenDiff={onOpenToolDiff ? (filePath) => onOpenToolDiff(turnId, filePath) : undefined}
            />
          ),
        });
      }
    });
    return out;
  }, [segments, running, threadId, onOpenDrawer, onOpenToolDiff, turnId]);

  const summaryParts = [
    counts.tools ? `${counts.tools} 个工具` : "",
    counts.reasoning ? `${counts.reasoning} 段思考` : "",
    counts.progress ? `${counts.progress} 条进度` : "",
  ].filter(Boolean);
  const summary = summaryParts.join(" · ");

  const hasProcess = Boolean(
    segments.length
    || workingLabel
    || status === "completed"
    || status === "failed"
    || status === "interrupted",
  );
  if (!hasProcess && settledDurationMs == null && liveStart == null) return null;

  const label = running ? (workingLabel || "正在执行") : settledLabel(status, settledDurationMs);
  const failedTurn = status === "failed";

  if (!hasProcess) {
    return (
      <section aria-label="本轮耗时">
        <div className="flex h-7 items-center text-[12.5px] text-cx-fg-4">{label}</div>
      </section>
    );
  }

  return (
    <section className="group/process -ml-1.5" aria-label="本轮工作过程" data-running={running || undefined}>
      <AgentActivity
        items={items}
        status={running ? "working" : "complete"}
        open={forceOpen ?? manualOpen ?? false}
        onOpenChange={(next) => {
          setManualOpen(next);
          onOpenChange?.(next);
        }}
        maxHeight={LIVE_MAX_HEIGHT}
        completedMaxHeight={SETTLED_MAX_HEIGHT}
        className="text-[13px]"
        contentClassName="py-1.5"
        renderWorkingStatus={() => (
          <span className="flex min-w-0 items-center gap-2 px-1.5">
            <ShimmerText className="min-w-0 truncate font-medium">{label}</ShimmerText>
            {liveStart != null ? <ElapsedTimer startMs={liveStart} className="shrink-0 text-[12px] text-cx-fg-4" /> : null}
            {summary ? <span className="hidden min-w-0 truncate text-[12px] text-cx-fg-4 sm:inline">{summary}</span> : null}
          </span>
        )}
        renderCompletedStatus={() => (
          <span className="inline-flex min-w-0 items-center gap-2 pl-1.5" aria-label={summary ? `${label}，${summary}` : label}>
            {failedTurn ? (
              <Icon name="circleAlert" size={14} className="shrink-0 text-cx-danger" />
            ) : (
              <Icon name="checkCheck" size={14} className="shrink-0 text-cx-fg-4" />
            )}
            <span className={cn("min-w-0 truncate", failedTurn && "text-cx-danger")}>{label}</span>
            {summary ? <span className="hidden min-w-0 truncate text-[12px] font-normal text-cx-fg-4 sm:inline">{summary}</span> : null}
            {counts.failed ? (
              <span className="shrink-0 rounded-md bg-cx-danger-soft px-1.5 text-[11px] font-medium leading-5 text-cx-danger">{counts.failed} 失败</span>
            ) : null}
            {planSummary ? (
              <span className="shrink-0 rounded-md bg-cx-hover px-1.5 text-[11px] font-medium leading-5 text-cx-fg-3">{planSummary}</span>
            ) : null}
          </span>
        )}
      />
    </section>
  );
}
