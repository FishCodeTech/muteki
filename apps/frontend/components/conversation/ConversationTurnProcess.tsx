"use client";

import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import { Button, Collapse, ShimmerText, Spinner } from "@muteki/ui";
import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";
import { ElapsedTimer } from "@/components/chat/timeline/ElapsedTimer";
import { ToolActivityGroup } from "@/components/chat/timeline/ToolActivityGroup";
import { SwapText, useEver, useFreshKeys, WorkingGlyph } from "@/components/chat/timeline/turnMotion";
import {
  formatTurnDuration,
  isWebResearchTool,
  reasoningSummaryTitle,
  unqualifiedToolName,
  type PresentableTool,
} from "@/components/chat/timeline/toolPresentation";
import { ConversationMessage, type ConversationMessageProps } from "./ConversationMessage";
import { toolDrawerPayload } from "./toolDrawerPayload";
import { ConversationSubagentGroup } from "./ConversationSubagentGroup";
import { compactNumber } from "../chat/composer/format";
import { isDelegationTool, type ConversationAgentNodeView } from "./conversationAgentTree";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import {
  resolveConversationTurnDurationMs,
  type ConversationToolRecord,
  type ConversationTurnSegment,
  type ConversationTurnTiming,
  type ConversationTextSpan,
} from "./conversationEventViews";

type CompactionSegment = Extract<ConversationTurnSegment, { kind: "compaction" }>;
type ReasoningSegment = Extract<ConversationTurnSegment, { kind: "thinking" }>;

function ReasoningHistory({ summaries, threadId }: { summaries: ReasoningSegment[]; threadId?: string }) {
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState<number | null>(null);
  const page = Math.min(selected ?? summaries.length - 1, summaries.length - 1);
  return (
    <details className="text-cx-sm text-cx-fg-3" onToggle={(event) => setOpen(event.currentTarget.open)}>
      <summary className="cursor-pointer select-none">思考摘要</summary>
      {open ? (
        <div className="mt-2 min-w-0">
          {summaries.length > 1 ? (
            <div className="mb-2 flex items-center gap-1 text-cx-xs">
              <Button size="xs" variant="ghost" disabled={page === 0} onClick={() => setSelected(page - 1)}>上一条摘要</Button>
              <span>{page + 1}/{summaries.length}</span>
              <Button size="xs" variant="ghost" disabled={page === summaries.length - 1} onClick={() => setSelected(page + 1)}>下一条摘要</Button>
            </div>
          ) : null}
          <ChatMarkdown text={summaries[page].text} threadId={threadId} />
        </div>
      ) : null}
    </details>
  );
}

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
    <div className="min-w-0 text-cx-sm" data-testid="conversation-compaction-entry">
      <div className={cn("flex min-h-7 flex-wrap items-center gap-x-2", segment.status === "failed" ? "text-cx-danger" : "text-cx-fg-3")}>
        {active ? <ShimmerText>{title}</ShimmerText> : <span>{title}</span>}
        {tokens ? <span className="cx-tabular font-cx-mono text-cx-xs text-cx-fg-4">{tokens}</span> : null}
      </div>
      {segment.message ? (
        <p className={cn("text-cx-xs leading-snug", segment.status === "failed" ? "text-cx-danger" : "text-cx-fg-4")}>{segment.message}</p>
      ) : null}
      {segment.summary ? (
        <details className="mt-0.5 text-cx-xs text-cx-fg-3">
          <summary className="cursor-pointer select-none text-cx-fg-4 hover:text-cx-fg-3">压缩摘要</summary>
          <p className="mt-1 whitespace-pre-wrap leading-snug text-cx-fg-2">{segment.summary}</p>
        </details>
      ) : null}
    </div>
  );
}

type CitationActions = Pick<ConversationMessageProps, "onCiteSelection" | "onAskSideChat">;

const Narration = memo(function Narration({ text, streaming, threadId, turnId, spans, sourceMessages, onCiteSelection, onAskSideChat }: CitationActions & { text: string; streaming: boolean; threadId?: string; turnId: string; spans?: ConversationTextSpan[]; sourceMessages?: ConversationMessageProps["sourceMessages"] }) {
  return (
    <ConversationMessage
      role="assistant"
      turnId={turnId}
      text={text}
      isStreaming={streaming}
      showIdentity={false}
      showActions={false}
      threadId={threadId}
      citationSources={spans || []}
      sourceMessages={sourceMessages}
      onCiteSelection={onCiteSelection}
      onAskSideChat={onAskSideChat}
    />
  );
});

export interface ConversationTurnProcessProps extends CitationActions {
  turnId: string;
  status: string;
  segments: ConversationTurnSegment[];
  running?: boolean;
  liveIndicator?: boolean;
  processRecorded?: boolean;
  timing?: ConversationTurnTiming;
  turnCreatedAt?: string;
  turnCompletedAt?: string;
  forceOpen?: boolean | null;
  onOpenChange?: (open: boolean) => void;
  planSummary?: string;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  threadId?: string;
  onOpenToolDiff?: (turnId: string, filePath?: string) => void;
  agentRoots?: ConversationAgentNodeView[];
  agentTools?: Record<string, ConversationToolRecord[]>;
  onOpenThread?: (threadId: string) => void;
  onCancelSubagent?: (subagentId: string) => void | Promise<void>;
  answerText?: string;
  sourceMessages?: ConversationMessageProps["sourceMessages"];
}

const NO_AGENTS: ConversationAgentNodeView[] = [];
const NO_AGENT_TOOLS: Record<string, ConversationToolRecord[]> = {};

type TurnUnit =
  | { kind: "narration"; key: string; text: string; streaming: boolean; turnId: string; spans?: ConversationTextSpan[] }
  | { kind: "tools"; key: string; tools: ConversationToolRecord[]; research: boolean }
  | { kind: "subagents"; key: string; agentIds: string[] }
  | { kind: "compaction"; key: string; segment: CompactionSegment; active: boolean };

function isPlatformSpawnTool(tool: ConversationToolRecord): boolean {
  const name = unqualifiedToolName(tool.name || "");
  return name === "muteki_start_thread" || name === "muteki_spawn_subagent";
}

function isNativeDelegationTool(tool: ConversationToolRecord): boolean {
  if (unqualifiedToolName(tool.name || "").startsWith("muteki_")) return false;
  return isDelegationTool(tool);
}

function toolMentionsAgent(tool: ConversationToolRecord, agentId: string): boolean {
  if (agentId.length < 8) return false;
  return `${tool.argsSummary || ""}\n${tool.outputSummary || ""}`.includes(agentId);
}

function anchorSubagentTools(
  segments: ConversationTurnSegment[],
  roots: ConversationAgentNodeView[],
): Map<ConversationToolRecord, string[]> {
  const anchors = new Map<ConversationToolRecord, string[]>();
  const used = new Set<string>();
  const tools: ConversationToolRecord[] = [];
  for (const segment of segments) {
    if (segment.kind === "tools") tools.push(...segment.tools);
  }
  const claim = (tool: ConversationToolRecord, agent: ConversationAgentNodeView) => {
    if (used.has(agent.agentId)) return;
    used.add(agent.agentId);
    const list = anchors.get(tool) ?? [];
    list.push(agent.agentId);
    anchors.set(tool, list);
  };

  for (const tool of tools) {
    for (const agent of roots) {
      if (used.has(agent.agentId)) continue;
      if (agent.agentId === tool.id || (agent.callId != null && agent.callId === tool.id)) {
        claim(tool, agent);
      }
    }
  }

  for (const tool of tools) {
    if (!isPlatformSpawnTool(tool) && !isNativeDelegationTool(tool)) continue;
    for (const agent of roots) {
      if (used.has(agent.agentId)) continue;
      if (toolMentionsAgent(tool, agent.agentId)) claim(tool, agent);
    }
  }

  return anchors;
}

function countActiveAgents(roots: ConversationAgentNodeView[]): number {
  let active = 0;
  const walk = (nodes: ConversationAgentNodeView[]) => {
    for (const node of nodes) {
      const status = String(node.status || "").toLowerCase();
      const delivery = String(node.delivery || "").toLowerCase();
      const settledDelivery = delivery === "awaiting_settle" || delivery === "settled" || delivery === "not_delivered";
      if (!settledDelivery && (status === "running" || status === "pending")) {
        active += 1;
      }
      walk(node.children);
    }
  };
  walk(roots);
  return active;
}

function subagentUnit(agentIds: string[]): TurnUnit {
  const key = `subagents-${[...agentIds].sort().join("+")}`;
  return { kind: "subagents", key, agentIds };
}

function buildTurnUnits(
  segments: ConversationTurnSegment[],
  running: boolean,
  roots: ConversationAgentNodeView[],
  pins: Map<string, number>,
): { units: TurnUnit[]; reasoningTitle: string; reasoningSummaries: ReasoningSegment[] } {
  const anchors = anchorSubagentTools(segments, roots);
  const chunks: TurnUnit[][] = [];
  let group: Extract<TurnUnit, { kind: "tools" }> | null = null;
  let reasoningTitle = "";
  const reasoningSummaries: ReasoningSegment[] = [];
  const placed = new Set<string>();
  segments.forEach((segment, index) => {
    const chunk: TurnUnit[] = [];
    chunks.push(chunk);
    const last = index === segments.length - 1;
    if (segment.kind === "thinking") {
      reasoningTitle = reasoningSummaryTitle(segment.text) || reasoningTitle;
      if (segment.text.trim()) reasoningSummaries.push(segment);
      return;
    }
    if (segment.kind === "text") {
      if (!segment.text.trim()) return;
      group = null;
      chunk.push({ kind: "narration", key: `text-${index}`, text: segment.text, streaming: running && last, turnId: segment.turnId, spans: segment.spans });
      return;
    }
    if (segment.kind === "compaction") {
      group = null;
      chunk.push({ kind: "compaction", key: `compaction-${index}`, segment, active: running && segment.status === "started" });
      return;
    }
    for (const tool of segment.tools) {
      const agentIds = anchors.get(tool);
      if (agentIds?.length) {
        group = null;
        for (const agentId of agentIds) placed.add(agentId);
        chunk.push(subagentUnit(agentIds));
        continue;
      }
      const research = isWebResearchTool(tool);
      if (!group || group.research !== research) {
        group = { kind: "tools", key: `tools-${tool.id}`, tools: [], research };
        chunk.push(group);
      }
      group.tools.push(tool);
    }
  });

  const byPin = new Map<number, string[]>();
  for (const agent of roots) {
    if (placed.has(agent.agentId)) continue;
    let pin = pins.get(agent.agentId);
    if (pin == null) {
      pin = segments.length > 0 ? segments.length - 1 : -1;
      pins.set(agent.agentId, pin);
    }
    if (segments.length === 0) pin = -1;
    else pin = Math.min(Math.max(pin, 0), segments.length - 1);
    const list = byPin.get(pin) ?? [];
    list.push(agent.agentId);
    byPin.set(pin, list);
  }

  const units: TurnUnit[] = [];
  const untimed = byPin.get(-1);
  if (untimed?.length) units.push(subagentUnit(untimed));
  chunks.forEach((chunk, index) => {
    units.push(...chunk);
    const extra = byPin.get(index);
    if (extra?.length) units.push(subagentUnit(extra));
  });
  return { units, reasoningTitle, reasoningSummaries };
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

interface TurnBodyProps extends CitationActions {
  units: TurnUnit[];
  running: boolean;
  live: boolean;
  liveFallback: string;
  trailingLive: boolean;
  reasoningSummaries: ReasoningSegment[];
  threadId?: string;
  sourceMessages?: ConversationMessageProps["sourceMessages"];
  agentRoots: ConversationAgentNodeView[];
  agentTools: Record<string, ConversationToolRecord[]>;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  onOpenThread?: (threadId: string) => void;
  onCancelSubagent?: (subagentId: string) => void | Promise<void>;
  onSelectTool: (tool: PresentableTool) => void;
  onOpenDiff?: (filePath?: string) => void;
}

function TurnBody({
  units,
  running,
  live,
  liveFallback,
  trailingLive,
  reasoningSummaries,
  threadId,
  sourceMessages,
  agentRoots,
  agentTools,
  onOpenDrawer,
  onOpenThread,
  onCancelSubagent,
  onSelectTool,
  onOpenDiff,
  onCiteSelection,
  onAskSideChat,
}: TurnBodyProps) {
  const isFresh = useFreshKeys(units.map((unit) => unit.key), running);
  const rootsById = new Map(agentRoots.map((agent) => [agent.agentId, agent]));
  return (
    <div className="mt-3 flex min-w-0 flex-col gap-3" data-testid="turn-process-body">
      {units.map((unit, index) => {
        let content: ReactNode;
        if (unit.kind === "narration") {
          content = <Narration text={unit.text} streaming={unit.streaming} threadId={threadId} turnId={unit.turnId} spans={unit.spans} sourceMessages={sourceMessages} onCiteSelection={onCiteSelection} onAskSideChat={onAskSideChat} />;
        } else if (unit.kind === "compaction") {
          content = <CompactionEntry segment={unit.segment} active={unit.active} />;
        } else if (unit.kind === "subagents") {
          const roots = unit.agentIds.flatMap((agentId) => {
            const agent = rootsById.get(agentId);
            return agent ? [agent] : [];
          });
          content = (
            <ConversationSubagentGroup
              roots={roots}
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
        const fresh = isFresh(unit.key);
        return <div key={unit.key} className={cn("min-w-0", fresh && "cx-enter")} data-fresh={(fresh && unit.kind !== "tools") || undefined}>{content}</div>;
      })}
      {trailingLive ? (
        <div className="cx-enter flex h-7 items-center text-cx-sm text-cx-fg-3" data-testid="turn-process-live">
          <SwapText value={liveFallback} className="min-w-0">
            <ShimmerText className="inline">{liveFallback}</ShimmerText>
          </SwapText>
        </div>
      ) : null}
      {reasoningSummaries.length ? <ReasoningHistory summaries={reasoningSummaries} threadId={threadId} /> : null}
    </div>
  );
}

export function ConversationTurnProcess({
  turnId,
  status,
  segments,
  running = false,
  liveIndicator = true,
  processRecorded = false,
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
  sourceMessages,
  onCiteSelection,
  onAskSideChat,
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
  const sawRunning = useEver(running);

  useEffect(() => {
    setManualOpen(null);
    setAnswerStartedAt(null);
  }, [turnId]);

  useLayoutEffect(() => {
    if (running && answerStarted && answerStartedAt == null) setAnswerStartedAt(Date.now());
  }, [running, answerStarted, answerStartedAt]);

  const activeAgentCount = useMemo(() => countActiveAgents(agentRoots), [agentRoots]);
  const subagentPins = useRef<{ turnId: string; pins: Map<string, number> }>({
    turnId: "",
    pins: new Map(),
  });
  if (subagentPins.current.turnId !== turnId) {
    subagentPins.current = { turnId, pins: new Map() };
  }
  const subagentPinMap = subagentPins.current.pins;
  const { units, reasoningTitle, reasoningSummaries } = useMemo(
    () => buildTurnUnits(segments, running, agentRoots, subagentPinMap),
    [segments, running, agentRoots, subagentPinMap],
  );

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

  const subagentsActive = activeAgentCount > 0;
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

  if (!running && !units.length && !reasoningSummaries.length && !processRecorded) return null;

  const waitingAgents = !running && subagentsActive ? activeAgentCount : 0;
  const failedTurn = !running && status === "failed";
  const timeSplit = !running && timing?.toolMs != null && settledDurationMs != null
    ? `模型思考与回答 ${formatTurnDuration(Math.max(0, settledDurationMs - timing.toolMs))} · 工具调用 ${formatTurnDuration(timing.toolMs)}`
    : "";
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
      <div className="flex min-h-7 flex-wrap items-center gap-2 text-cx-sm text-cx-fg-3">
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
          <span className="shrink-0 rounded-md bg-cx-hover px-1.5 text-cx-xs font-medium leading-5 text-cx-fg-3">{planSummary}</span>
        ) : null}
        {waitingAgents ? (
          <span className="inline-flex shrink-0 items-center gap-1 rounded-md bg-cx-accent-soft px-1.5 text-cx-xs font-medium leading-5 text-cx-accent" data-testid="turn-waiting-subagents">
            <Spinner size={10} />
            等待 {waitingAgents} 个委派线程
          </span>
        ) : null}
        {timeSplit ? <span className="shrink-0 text-cx-xs text-cx-fg-4">{timeSplit}</span> : null}
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
          reasoningSummaries={reasoningSummaries}
          threadId={threadId}
          sourceMessages={sourceMessages}
          agentRoots={agentRoots}
          agentTools={agentTools}
          onOpenDrawer={onOpenDrawer}
          onOpenThread={onOpenThread}
          onCancelSubagent={onCancelSubagent}
          onSelectTool={selectTool}
          onOpenDiff={canOpenDiff ? openDiff : undefined}
          onCiteSelection={onCiteSelection}
          onAskSideChat={onAskSideChat}
        />
      </Collapse>
    </section>
  );
}
