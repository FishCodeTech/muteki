"use client";

import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { Collapse, ShimmerText, Spinner } from "@/components/chat/ui";
import { ElapsedTimer } from "@/components/chat/timeline/ElapsedTimer";
import { ToolActivityGroup } from "@/components/chat/timeline/ToolActivityGroup";
import { SwapText, useEver, useFreshKeys, WorkingGlyph } from "@/components/chat/timeline/turnMotion";
import {
  formatTurnDuration,
  isWebResearchTool,
  reasoningSummaryTitle,
  type PresentableTool,
} from "@/components/chat/timeline/toolPresentation";
import { ConversationMessage } from "./ConversationMessage";
import { toolDrawerPayload } from "./ConversationToolFlow";
import { ConversationSubagentGroup } from "./ConversationSubagentGroup";
import { compactNumber } from "../chat/composer/format";
import { isAgentTerminal, type ConversationAgentNodeView } from "./conversationAgentTree";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import {
  resolveConversationTurnDurationMs,
  type ConversationToolRecord,
  type ConversationTurnSegment,
  type ConversationTurnTiming,
} from "./conversationEventViews";

type CompactionSegment = Extract<ConversationTurnSegment, { kind: "compaction" }>;

function CompactionEntry({ segment, active }: { segment: CompactionSegment; active: boolean }) {
  const kind = segment.trigger === "auto" ? "自动压缩上下文" : "压缩上下文";
  const title = active ? `正在${kind}`
    : segment.status === "started" ? `${kind}未收到结束信号`
      : segment.status === "failed" ? `${kind}失败`
        : segment.status === "interrupted" ? `${kind}已中断`
          : segment.confirmed ? `已${kind}` : "压缩命令已结束（引擎未上报压缩结果）";
  const tokens = segment.tokensBefore != null
    ? `${compactNumber(segment.tokensBefore)} → ${segment.tokensAfter != null ? compactNumber(segment.tokensAfter) : "?"} tok`
    : "";
  return (
    <div className="min-w-0 text-[13px]" data-testid="conversation-compaction-entry">
      <div className={cn("flex min-h-7 flex-wrap items-center gap-x-2", segment.status === "failed" ? "text-cx-danger" : "text-cx-fg-3")}>
        {active ? <ShimmerText>{title}</ShimmerText> : <span>{title}</span>}
        {tokens ? <span className="cx-tabular font-cx-mono text-[12px] text-cx-fg-4">{tokens}</span> : null}
      </div>
      {segment.message ? (
        <p className={cn("text-[12px] leading-snug", segment.status === "failed" ? "text-cx-danger" : "text-cx-fg-4")}>{segment.message}</p>
      ) : null}
      {segment.summary ? (
        <details className="mt-0.5 text-[12px] text-cx-fg-3">
          <summary className="cursor-pointer select-none text-cx-fg-4 hover:text-cx-fg-3">压缩摘要</summary>
          <p className="mt-1 whitespace-pre-wrap leading-snug text-cx-fg-2">{segment.summary}</p>
        </details>
      ) : null}
    </div>
  );
}

const Narration = memo(function Narration({ text, streaming, threadId }: { text: string; streaming: boolean; threadId?: string }) {
  return (
    <ConversationMessage
      role="assistant"
      text={text}
      isStreaming={streaming}
      showIdentity={false}
      showActions={false}
      threadId={threadId}
    />
  );
});

export interface ConversationTurnProcessProps {
  turnId: string;
  status: string;
  /** Work of the turn: everything except its final answer. */
  segments: ConversationTurnSegment[];
  running?: boolean;
  /** False while the turn waits on the user (approval, input): no live "working" line. */
  liveIndicator?: boolean;
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
  /** Final answer text; once it starts, the work above folds away. */
  answerText?: string;
}

const NO_AGENTS: ConversationAgentNodeView[] = [];
const NO_AGENT_TOOLS: Record<string, ConversationToolRecord[]> = {};

type TurnUnit =
  | { kind: "narration"; key: string; text: string; streaming: boolean }
  | { kind: "tools"; key: string; tools: ConversationToolRecord[]; research: boolean }
  | { kind: "subagents"; key: string }
  | { kind: "compaction"; key: string; segment: CompactionSegment; active: boolean };

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

/**
 * Lay the turn's work out as Codex does: commentary is ordinary prose,
 * consecutive tool calls share one group, and reasoning is not a row of its
 * own; its latest bold title only narrates the live state.
 */
function buildTurnUnits(
  segments: ConversationTurnSegment[],
  running: boolean,
  agentKeys: Set<string>,
  hasAgents: boolean,
): { units: TurnUnit[]; reasoningTitle: string } {
  const units: TurnUnit[] = [];
  let group: Extract<TurnUnit, { kind: "tools" }> | null = null;
  let subagentsPlaced = false;
  let reasoningTitle = "";
  const placeSubagents = () => {
    if (subagentsPlaced) return;
    units.push({ kind: "subagents", key: "subagents" });
    subagentsPlaced = true;
  };
  segments.forEach((segment, index) => {
    const last = index === segments.length - 1;
    if (segment.kind === "thinking") {
      reasoningTitle = reasoningSummaryTitle(segment.text) || reasoningTitle;
      return;
    }
    if (segment.kind === "text") {
      if (!segment.text.trim()) return;
      group = null;
      units.push({ kind: "narration", key: `text-${index}`, text: segment.text, streaming: running && last });
      return;
    }
    if (segment.kind === "compaction") {
      group = null;
      units.push({ kind: "compaction", key: `compaction-${index}`, segment, active: running && segment.status === "started" });
      return;
    }
    for (const tool of segment.tools) {
      if (agentKeys.has(tool.id)) {
        // The subagent card stands in for the delegation call that spawned it.
        placeSubagents();
        group = null;
        continue;
      }
      // Web research gets its own trail, so searches never share a group with other work.
      const research = isWebResearchTool(tool);
      if (!group || group.research !== research) {
        group = { kind: "tools", key: `tools-${tool.id}`, tools: [], research };
        units.push(group);
      }
      group.tools.push(tool);
    }
  });
  if (hasAgents) placeSubagents();
  return { units, reasoningTitle };
}

const INTERRUPTED = new Set(["interrupted", "aborted"]);

function settledLabel(status: string, durationMs?: number): string {
  const duration = durationMs == null ? "" : formatTurnDuration(durationMs);
  if (status === "failed") return duration ? `执行失败 · ${duration}` : "执行失败";
  if (status === "cancelled" || status === "canceled") return duration ? `已取消 · ${duration}` : "已取消";
  if (INTERRUPTED.has(status)) return duration ? `已中断 · ${duration}` : "已中断";
  return duration ? `用时 ${duration}` : "工作过程";
}

function parseStart(value?: string): number | null {
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

interface TurnBodyProps {
  units: TurnUnit[];
  running: boolean;
  live: boolean;
  liveFallback: string;
  trailingLive: boolean;
  threadId?: string;
  agentRoots: ConversationAgentNodeView[];
  agentTools: Record<string, ConversationToolRecord[]>;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  onOpenThread?: (threadId: string) => void;
  onCancelSubagent?: (subagentId: string) => void | Promise<void>;
  onSelectTool: (tool: PresentableTool) => void;
  onOpenDiff?: (filePath?: string) => void;
}

/** Mounted only while open, so work that was already there on open stays still. */
function TurnBody({
  units,
  running,
  live,
  liveFallback,
  trailingLive,
  threadId,
  agentRoots,
  agentTools,
  onOpenDrawer,
  onOpenThread,
  onCancelSubagent,
  onSelectTool,
  onOpenDiff,
}: TurnBodyProps) {
  const isFresh = useFreshKeys(units.map((unit) => unit.key), running);
  return (
    <div className="mt-3 flex min-w-0 flex-col gap-3" data-testid="turn-process-body">
      {units.map((unit, index) => {
        let content: ReactNode;
        if (unit.kind === "narration") {
          content = <Narration text={unit.text} streaming={unit.streaming} threadId={threadId} />;
        } else if (unit.kind === "compaction") {
          content = <CompactionEntry segment={unit.segment} active={unit.active} />;
        } else if (unit.kind === "subagents") {
          content = (
            <ConversationSubagentGroup
              roots={agentRoots}
              toolsByAgent={agentTools}
              threadId={threadId}
              onOpenDrawer={onOpenDrawer}
              onOpenThread={onOpenThread}
              onCancelSubagent={onCancelSubagent}
            />
          );
        } else {
          content = (
            <ToolActivityGroup
              tools={unit.tools}
              live={live && index === units.length - 1}
              liveFallback={liveFallback}
              threadId={threadId}
              onSelectTool={onSelectTool}
              onOpenDiff={onOpenDiff}
            />
          );
        }
        return <div key={unit.key} className={cn("min-w-0", isFresh(unit.key) && "cx-enter")}>{content}</div>;
      })}
      {trailingLive ? (
        <div className="cx-enter flex h-7 items-center text-[13px] text-cx-fg-3" data-testid="turn-process-live">
          <SwapText value={liveFallback} className="min-w-0">
            <ShimmerText className="inline">{liveFallback}</ShimmerText>
          </SwapText>
        </div>
      ) : null}
    </div>
  );
}

export function ConversationTurnProcess({
  turnId,
  status,
  segments,
  running = false,
  liveIndicator = true,
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
  const [answerStartedAt, setAnswerStartedAt] = useState<number | null>(null);
  const settledDurationMs = resolveConversationTurnDurationMs({
    timing,
    createdAt: turnCreatedAt,
    completedAt: turnCompletedAt,
  });
  const liveStart = running && settledDurationMs == null ? parseStart(timing?.startedAt || turnCreatedAt) : null;
  const answerStarted = Boolean(answerText.trim());
  // Settled history mounts still; only a turn seen running animates its fold.
  const sawRunning = useEver(running);

  useEffect(() => {
    setManualOpen(null);
    setAnswerStartedAt(null);
  }, [turnId]);

  // Freeze before paint so the collapsed header never flashes without a duration.
  useLayoutEffect(() => {
    if (running && answerStarted && answerStartedAt == null) setAnswerStartedAt(Date.now());
  }, [running, answerStarted, answerStartedAt]);

  const agentKeys = useMemo(() => collectAgentKeys(agentRoots), [agentRoots]);
  const { units, reasoningTitle } = useMemo(
    () => buildTurnUnits(segments, running, agentKeys.keys, agentRoots.length > 0),
    [segments, running, agentKeys, agentRoots.length],
  );

  // Stable handlers keep memoized groups from re-rendering on every frame.
  const handlersRef = useRef({ onOpenDrawer, onOpenToolDiff, turnId });
  useLayoutEffect(() => {
    handlersRef.current = { onOpenDrawer, onOpenToolDiff, turnId };
  });
  const selectTool = useCallback((tool: PresentableTool) => {
    handlersRef.current.onOpenDrawer(toolDrawerPayload(tool as ConversationToolRecord));
  }, []);
  const canOpenDiff = Boolean(onOpenToolDiff);
  const openDiff = useCallback((filePath?: string) => {
    handlersRef.current.onOpenToolDiff?.(handlersRef.current.turnId, filePath);
  }, []);

  const subagentsActive = agentKeys.active > 0;
  // Work folds away once the final answer starts; while subagents still run
  // their cards stay in view.
  const collapsible = (!running || answerStarted) && !(running && subagentsActive);
  const open = collapsible ? (forceOpen ?? manualOpen ?? false) : true;
  const live = running && liveIndicator && !answerStarted;
  const lastUnit = units.at(-1);
  const lastSegment = segments.at(-1);
  const liveFallback = reasoningTitle || "正在思考";
  const trailingLive = live
    && lastUnit?.kind !== "tools"
    && lastSegment?.kind !== "text"
    && !(lastUnit?.kind === "compaction" && lastUnit.active);

  if (!running && !units.length) return null;

  const waitingAgents = !running && subagentsActive ? agentKeys.active : 0;
  const failedTurn = !running && status === "failed";
  const frozenMs = answerStartedAt != null && liveStart != null ? Math.max(0, answerStartedAt - liveStart) : undefined;
  const collapsedLabel = running
    ? (frozenMs != null ? `用时 ${formatTurnDuration(frozenMs)}` : "用时")
    : settledLabel(status, settledDurationMs);

  const toggle = () => {
    const next = !open;
    setManualOpen(next);
    onOpenChange?.(next);
  };

  const popHeader = sawRunning;
  const headerKey = collapsible ? collapsedLabel : liveStart != null ? "working" : "pending";
  const headerContent = collapsible ? collapsedLabel
    : liveStart != null ? <>已处理 <ElapsedTimer startMs={liveStart} format={formatTurnDuration} /></>
      : "处理中";

  return (
    <section className="mb-3 mt-1 min-w-0" aria-label="本轮工作过程" data-running={running || undefined} data-testid="turn-process">
      <div className="flex min-h-7 flex-wrap items-center gap-2 text-[13px] text-cx-fg-3">
        <button
          type="button"
          onClick={collapsible ? toggle : undefined}
          disabled={!collapsible}
          aria-expanded={collapsible ? open : undefined}
          className={cn(
            "cx-press -ml-1 inline-flex min-w-0 max-w-full items-center gap-1.5 rounded-md px-1 py-0.5 outline-none transition-colors focus-visible:ring-2 focus-visible:ring-cx-focus",
            collapsible ? "hover:bg-cx-hover hover:text-cx-fg-2" : "cursor-default",
          )}
          data-testid={collapsible ? "turn-process-toggle" : "turn-process-working"}
        >
          {running && !collapsible ? <WorkingGlyph paused={!live} className="cx-enter" /> : null}
          {failedTurn ? <Icon name="circleAlert" size={14} className={cn("shrink-0 text-cx-danger", popHeader && "cx-pop")} /> : null}
          <SwapText value={headerKey} className={cn("min-w-0", failedTurn && "text-cx-danger")}>{headerContent}</SwapText>
          {collapsible ? (
            <span aria-hidden className={cn("cx-chevron -ml-0.5 inline-flex shrink-0", popHeader && "cx-pop")} data-open={open || undefined}>
              <Icon name="chevronRight" size={12} />
            </span>
          ) : null}
        </button>
        {planSummary ? (
          <span className="shrink-0 rounded-md bg-cx-hover px-1.5 text-[12px] font-medium leading-5 text-cx-fg-3">{planSummary}</span>
        ) : null}
        {waitingAgents ? (
          <span className="inline-flex shrink-0 items-center gap-1 rounded-md bg-cx-accent-soft px-1.5 text-[12px] font-medium leading-5 text-cx-accent" data-testid="turn-waiting-subagents">
            <Spinner size={10} />
            等待 {waitingAgents} 个子代理
          </span>
        ) : null}
      </div>
      <div className="pt-1">
        <div className="cx-turn-rule" data-live={live || undefined} />
      </div>
      <Collapse open={open} enterDuration={0.3} exitDuration={0.26}>
        <TurnBody
          units={units}
          running={running}
          live={live}
          liveFallback={liveFallback}
          trailingLive={trailingLive}
          threadId={threadId}
          agentRoots={agentRoots}
          agentTools={agentTools}
          onOpenDrawer={onOpenDrawer}
          onOpenThread={onOpenThread}
          onCancelSubagent={onCancelSubagent}
          onSelectTool={selectTool}
          onOpenDiff={canOpenDiff ? openDiff : undefined}
        />
      </Collapse>
    </section>
  );
}
