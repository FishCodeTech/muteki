"use client";

import { useEffect, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { ShimmerText, Spinner } from "@/components/chat/ui";
import { AgentActivity, type AgentActivityItem } from "@/components/agentui/agents/agent-activity";
import { ActivityItem } from "@/components/chat/timeline/ActivityItem";
import { ElapsedTimer } from "@/components/chat/timeline/ElapsedTimer";
import { SourceChips } from "@/components/chat/timeline/SourceChips";
import { ToolEntry } from "@/components/chat/timeline/ToolEntry";
import { formatTurnDuration, summarizeTools } from "@/components/chat/timeline/toolPresentation";
import { ConversationMessage } from "./ConversationMessage";
import { ConversationThinking } from "./ConversationThinking";
import { toolDrawerPayload } from "./ConversationToolFlow";
import { ConversationSubagentGroup } from "./ConversationSubagentGroup";
import { extractUrls } from "./conversationSources";
import { isLocalPreviewUrl } from "@/lib/previewUrlDetect";
import { isAgentTerminal, type ConversationAgentNodeView } from "./conversationAgentTree";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import {
  resolveConversationTurnDurationMs,
  type ConversationToolRecord,
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
  /** Subagent trees spawned by this turn (roots only; children nested). */
  agentRoots?: ConversationAgentNodeView[];
  /** Subagent id → that subagent's own tool calls. */
  agentTools?: Record<string, ConversationToolRecord[]>;
  /** app_owned subagents: open the child conversation Thread. */
  onOpenThread?: (threadId: string) => void;
  /** app_owned subagents: cancel a running child (conversation.subagent.cancel). */
  onCancelSubagent?: (subagentId: string) => void | Promise<void>;
  /** Final answer text; its web links are listed on the finish row once the turn settles. */
  answerText?: string;
}

const NO_AGENTS: ConversationAgentNodeView[] = [];
const NO_AGENT_TOOLS: Record<string, ConversationToolRecord[]> = {};

function collectAgentKeys(roots: ConversationAgentNodeView[]): { keys: Set<string>; active: number } {
  const keys = new Set<string>();
  let active = 0;
  const walk = (nodes: ConversationAgentNodeView[]) => {
    for (const node of nodes) {
      keys.add(node.agentId);
      if (node.callId) keys.add(node.callId);
      if (!isAgentTerminal(String(node.status))) active += 1;
      walk(node.children);
    }
  };
  walk(roots);
  return { keys, active };
}

const INTERRUPTED = new Set(["interrupted", "aborted", "cancelled", "canceled"]);
/** Scrollable live window; reading older entries pauses automatic following. */
const LIVE_MAX_HEIGHT = 420;
/** Expanded after completion: show the whole record inline, no nested scroller. */
const SETTLED_MAX_HEIGHT = 1_000_000;

function settledLabel(status: string, durationMs?: number): string {
  const duration = durationMs == null ? "" : formatTurnDuration(durationMs);
  if (status === "failed") return duration ? `执行失败 · ${duration}` : "执行失败";
  if (status === "cancelled" || status === "canceled") return duration ? `已取消 · ${duration}` : "已取消";
  if (INTERRUPTED.has(status)) return duration ? `已中断 · ${duration}` : "已中断";
  return duration ? `已处理 ${duration}` : "工作过程";
}

function parseStart(value?: string): number | null {
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

type FinishTone = "done" | "failed" | "stopped";

function finishTone(status: string): FinishTone | null {
  if (status === "completed") return "done";
  if (status === "failed") return "failed";
  if (INTERRUPTED.has(status)) return "stopped";
  return null;
}

const FINISH_LABEL: Record<FinishTone, string> = {
  done: "已完成",
  failed: "执行失败",
  stopped: "已中断",
};

function FinishBadge({ tone }: { tone: FinishTone }) {
  return (
    <svg
      viewBox="0 0 20 20"
      aria-hidden
      className={cn(
        "cx-timeline-done size-4",
        tone === "done" && "text-cx-accent",
        tone === "failed" && "text-cx-danger",
        tone === "stopped" && "text-cx-fg-4",
      )}
    >
      <circle cx="10" cy="10" r="10" fill="currentColor" />
      <path
        data-tick
        d={tone === "done" ? "M6 10.3l2.7 2.7L14.2 7.4" : tone === "failed" ? "M7 7l6 6M13 7l-6 6" : "M6.5 10h7"}
        fill="none"
        stroke="white"
        strokeWidth="2.1"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function FinishRow({ tone, durationMs, sources }: { tone: FinishTone; durationMs?: number; sources: string[] }) {
  const duration = durationMs == null ? "" : formatTurnDuration(durationMs);
  const parts = [FINISH_LABEL[tone], duration, sources.length ? `回答引用 ${sources.length} 个来源` : ""].filter(Boolean);
  return (
    <div className="grid min-w-0 grid-cols-[1rem_minmax(0,1fr)] gap-x-2.5 px-1.5" data-testid="turn-process-finish">
      <span data-timeline-node className="grid h-8 w-4 place-items-center">
        <FinishBadge tone={tone} />
      </span>
      <div className="min-w-0">
        <div className={cn("flex h-8 items-center text-[13px]", tone === "failed" ? "text-cx-danger" : "text-cx-fg-2")}>
          {parts.join(" · ")}
        </div>
        <SourceChips urls={sources} className="mb-1.5" />
      </div>
    </div>
  );
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
  agentRoots = NO_AGENTS,
  agentTools = NO_AGENT_TOOLS,
  onOpenThread,
  onCancelSubagent,
  answerText = "",
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
    let reasoning = 0;
    let failed = 0;
    for (const segment of segments) {
      if (segment.kind === "thinking") reasoning += 1;
      if (segment.kind === "tools") {
        failed += segment.tools.filter((tool) => tool.status === "failed").length;
      }
    }
    return { reasoning, failed };
  }, [segments]);

  const agentKeys = useMemo(() => collectAgentKeys(agentRoots), [agentRoots]);
  const sources = useMemo(
    () => (running ? [] : [...new Set(extractUrls(answerText))].filter((url) => !isLocalPreviewUrl(url))),
    [running, answerText],
  );

  const items = useMemo<AgentActivityItem[]>(() => {
    const out: AgentActivityItem[] = [];
    let groupPlaced = false;
    const groupItem = (): AgentActivityItem => ({
      id: `subagents-${turnId}`,
      type: "custom",
      content: (
        <div className="px-1.5" data-timeline-skip>
          <ConversationSubagentGroup
            roots={agentRoots}
            toolsByAgent={agentTools}
            threadId={threadId}
            onOpenDrawer={onOpenDrawer}
            onOpenThread={onOpenThread}
            onCancelSubagent={onCancelSubagent}
          />
        </div>
      ),
    });
    segments.forEach((segment, index) => {
      const last = index === segments.length - 1;
      if (segment.kind === "thinking") {
        const thinkingRunning = running && last && segment.partial !== false;
        out.push({
          id: `thinking-${index}`,
          type: "custom",
          content: (
            <ActivityItem icon="dot" tone={thinkingRunning ? "running" : "muted"}>
              <ConversationThinking bare working={thinkingRunning} durationMs={segment.durationMs} summary={segment.text} />
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
            <ActivityItem icon="dot" tone={running && last ? "running" : "muted"}>
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
        if (agentKeys.keys.has(tool.id)) {
          // The group card stands in for the delegation call that spawned it.
          if (!groupPlaced) {
            out.push(groupItem());
            groupPlaced = true;
          }
          continue;
        }
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
    if (!groupPlaced && agentRoots.length) out.push(groupItem());
    const tone = running ? null : finishTone(status);
    if (tone && out.length) {
      out.push({
        id: `finish-${turnId}`,
        type: "custom",
        content: <FinishRow tone={tone} durationMs={settledDurationMs} sources={sources} />,
      });
    }
    return out;
  }, [segments, running, status, settledDurationMs, sources, threadId, onOpenDrawer, onOpenToolDiff, turnId, agentKeys, agentRoots, agentTools, onOpenThread, onCancelSubagent]);

  const toolSummary = useMemo(
    () => summarizeTools(segments.flatMap((segment) => (segment.kind === "tools" ? segment.tools : []))),
    [segments],
  );
  const summary = toolSummary || (counts.reasoning ? `${counts.reasoning} 段思考` : "");

  const waitingAgents = !running && agentKeys.active > 0 ? agentKeys.active : 0;
  const hasProcess = Boolean(
    segments.length
    || agentRoots.length
    || workingLabel
    || status === "completed"
    || status === "failed"
    || status === "interrupted"
    || status === "cancelled",
  );
  if (!hasProcess && settledDurationMs == null && liveStart == null) return null;

  const label = running ? (workingLabel || "正在执行") : settledLabel(status, settledDurationMs);
  const failedTurn = status === "failed";

  if (!hasProcess) {
    return (
      <section aria-label="本轮耗时">
        <div className="flex h-7 items-center text-[13px] text-cx-fg-4">{label}</div>
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
        timeline
        activeLabel={label}
        className="text-[13px]"
        contentClassName="py-1.5"
        renderWorkingStatus={() => (
          <span className="flex min-w-0 items-center gap-2">
            <ShimmerText className="min-w-0 truncate font-medium">{label}</ShimmerText>
            {liveStart != null ? (
              <ElapsedTimer startMs={liveStart} format={formatTurnDuration} className="shrink-0 text-[12px] text-cx-fg-4" />
            ) : null}
            {summary ? <span className="hidden min-w-0 truncate text-[12px] text-cx-fg-4 sm:inline">{summary}</span> : null}
          </span>
        )}
        renderCompletedStatus={() => (
          <span className="inline-flex min-w-0 items-center gap-2" aria-label={summary ? `${label}，${summary}` : label}>
            {failedTurn ? (
              <Icon name="circleAlert" size={14} className="shrink-0 text-cx-danger" />
            ) : null}
            <span className={cn("min-w-0 truncate", failedTurn && "text-cx-danger")}>{label}</span>
            {summary ? <span className="hidden min-w-0 truncate text-[12px] font-normal text-cx-fg-4 sm:inline">{summary}</span> : null}
            {counts.failed ? (
              <span className="shrink-0 rounded-md bg-cx-danger-soft px-1.5 text-[12px] font-medium leading-5 text-cx-danger">{counts.failed} 失败</span>
            ) : null}
            {planSummary ? (
              <span className="shrink-0 rounded-md bg-cx-hover px-1.5 text-[12px] font-medium leading-5 text-cx-fg-3">{planSummary}</span>
            ) : null}
            {waitingAgents ? (
              <span className="inline-flex shrink-0 items-center gap-1 rounded-md bg-cx-accent-soft px-1.5 text-[12px] font-medium leading-5 text-cx-accent" data-testid="turn-waiting-subagents">
                <Spinner size={10} />
                等待 {waitingAgents} 个子智能体
              </span>
            ) : null}
          </span>
        )}
      />
    </section>
  );
}
