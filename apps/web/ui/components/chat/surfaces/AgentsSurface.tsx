"use client";

import { useMemo, useRef, useState, type ReactNode } from "react";
import { useConversationNavigation } from "@/components/conversation/ConversationNavigation";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Button, EmptyState, IconButton, ScrollArea, toast } from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { buildThreadMessageHref } from "@/lib/conversationDeepLink";
import { chatPanel, PANEL_SHEET_BREAKPOINT } from "@/lib/chatPanelStore";
import { sendConversationCommand } from "@/lib/useConversation";
import {
  agentDisplayName,
  agentPreviewText,
  buildConversationAgentTree,
  countAgentStatuses,
  formatAgentCounts,
  isAgentTerminal,
  isShellLikeTool,
  worktreeStateLabel,
  type ConversationAgentNodeView,
} from "@/components/conversation/conversationAgentTree";
import {
  AgentAvatar,
  AgentDuration,
  AgentStatusLabel,
  SubagentCancelDialog,
  agentDrawerPayload,
} from "@/components/conversation/ConversationSubagentGroup";
import { SurfaceToolbar, openExecutionLog } from "./shared";

/** Finished lists stay open while short or when nothing else is running. */
const FINISHED_OPEN_LIMIT = 3;

function subtreeActive(agent: ConversationAgentNodeView): boolean {
  return !isAgentTerminal(String(agent.status)) || agent.children.some(subtreeActive);
}

function finishedAt(agent: ConversationAgentNodeView): number {
  const value = agent.completedAt ? Date.parse(agent.completedAt) : NaN;
  return Number.isFinite(value) ? value : 0;
}

function agentMeta(agent: ConversationAgentNodeView): string {
  const appOwned = agent.origin === "app_owned";
  return [
    appOwned ? [agent.adapterId, agent.model].filter(Boolean).join(" / ") : agent.model,
    agent.effort ? `思考 ${agent.effort}` : null,
    agent.toolUses ? `${agent.toolUses} 次工具` : null,
    appOwned && agent.isolation === "worktree" ? (worktreeStateLabel(agent.worktreeState) || "worktree 隔离") : null,
  ].filter(Boolean).join(" · ");
}

interface RowActions {
  onOpenThread: (agent: ConversationAgentNodeView) => void;
  onOpenDetails: (agent: ConversationAgentNodeView) => void;
  onJump: (agent: ConversationAgentNodeView) => void;
  canJump: (agent: ConversationAgentNodeView) => boolean;
  onRequestCancel: (agent: ConversationAgentNodeView) => void;
}

function AgentRow({ agent, actions }: { agent: ConversationAgentNodeView; actions: RowActions }) {
  const status = String(agent.status);
  const terminal = isAgentTerminal(status);
  const appOwned = agent.origin === "app_owned";
  const name = agentDisplayName(agent);
  const meta = agentMeta(agent);
  const preview = agentPreviewText(agent);
  const openable = appOwned && Boolean(agent.childThreadId);
  const cancellable = appOwned && !terminal;
  const jumpable = actions.canJump(agent);
  return (
    <li
      data-agent-id={agent.agentId}
      data-parent-id={agent.parentId || ""}
      data-depth={agent.depth}
      data-agent-status={status}
      data-agent-origin={agent.origin || "provider_native"}
      data-child-thread-id={agent.childThreadId || undefined}
    >
      <div className="group/agent relative flex min-w-0 items-start gap-2.5 rounded-lg px-2 py-2 transition-colors hover:bg-cx-hover has-[:focus-visible]:bg-cx-hover">
        <AgentAvatar agent={agent} size={22} className="mt-px" />
        <button
          type="button"
          data-agent-primary=""
          className="min-w-0 flex-1 rounded-md text-left outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
          title={openable ? "打开该子代理的会话" : "查看任务与结果"}
          onClick={() => (openable ? actions.onOpenThread(agent) : actions.onOpenDetails(agent))}
        >
          <span className="flex min-w-0 items-center gap-1.5">
            <span className="min-w-0 truncate text-[13px] font-medium text-cx-fg">{name}</span>
            {agent.role && agent.role !== name ? (
              <span className="shrink-0 rounded bg-cx-hover px-1 text-[11px] leading-4 text-cx-fg-3">{agent.role}</span>
            ) : null}
          </span>
          <span className="mt-0.5 flex min-w-0 items-center gap-1.5 text-[12px] leading-5">
            <AgentStatusLabel agent={agent} />
            {meta ? <span className="min-w-0 truncate text-cx-fg-4">{meta}</span> : null}
          </span>
          {preview ? (
            <span className={cn("mt-0.5 line-clamp-2 break-words text-[12px] leading-5", status === "failed" ? "text-cx-danger" : "text-cx-fg-3")}>
              {preview}
            </span>
          ) : null}
        </button>
        <span className="flex shrink-0 flex-col items-end gap-1">
          <AgentDuration agent={agent} />
          <span
            className={cn(
              "flex items-center gap-0.5 transition-opacity",
              "[@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover/agent:opacity-100 [@media(hover:hover)]:group-has-[:focus-visible]/agent:opacity-100",
              "[@media(max-width:640px)]:!opacity-100",
            )}
          >
            {openable ? (
              <IconButton
                size="xs"
                icon="arrowUpRight"
                label="打开子会话"
                data-testid={`agent-open-thread-${agent.agentId}`}
                onClick={() => actions.onOpenThread(agent)}
              />
            ) : null}
            {cancellable ? (
              <IconButton
                size="xs"
                icon="stopCircle"
                label={agent.cancelRequested ? "已请求取消，等待停止" : "取消子代理"}
                disabled={agent.cancelRequested}
                data-testid={`agent-cancel-${agent.agentId}`}
                className="hover:text-cx-danger"
                onClick={() => actions.onRequestCancel(agent)}
              />
            ) : null}
            <IconButton
              size="xs"
              icon="crosshair"
              label={jumpable ? "定位到委派它的对话消息" : "当前没有可定位的对话消息"}
              disabled={!jumpable}
              data-testid={`agent-jump-${agent.agentId}`}
              onClick={() => actions.onJump(agent)}
            />
            <IconButton
              size="xs"
              icon="maximize"
              label="查看任务与结果"
              data-testid={`agent-details-${agent.agentId}`}
              onClick={() => actions.onOpenDetails(agent)}
            />
          </span>
        </span>
      </div>
      {agent.children.length ? (
        <ul className="m-0 ml-[18px] list-none border-l border-cx-border pl-1.5" aria-label={`${name} 的子代理`}>
          {agent.children.map((child) => <AgentRow key={child.agentId} agent={child} actions={actions} />)}
        </ul>
      ) : null}
    </li>
  );
}

function AgentSection({
  title,
  count,
  note,
  collapsed,
  onToggle,
  testId,
  children,
}: {
  title: string;
  count: number;
  note?: ReactNode;
  collapsed?: boolean;
  onToggle?: () => void;
  testId: string;
  children: ReactNode;
}) {
  const label = (
    <>
      <span>{title}</span>
      <span className="cx-tabular text-cx-fg-4">{count}</span>
    </>
  );
  return (
    <section aria-label={title} data-testid={testId} className="flex flex-col">
      <div className="flex h-8 items-center px-2">
        {onToggle ? (
          <button
            type="button"
            data-agent-focus-fallback=""
            aria-expanded={!collapsed}
            onClick={onToggle}
            className="-ml-1 flex h-6 items-center gap-1.5 rounded-md px-1 text-[12px] font-medium text-cx-fg-3 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
          >
            <Icon name="chevronRight" size={12} className={cn("shrink-0 transition-transform duration-150", !collapsed && "rotate-90")} />
            {label}
          </button>
        ) : (
          <h3 className="flex items-center gap-1.5 text-[12px] font-medium text-cx-fg-3">{label}</h3>
        )}
      </div>
      {note && !collapsed ? <p className="px-2 pb-1 text-[11.5px] leading-4 text-cx-fg-4">{note}</p> : null}
      {collapsed ? null : <ul className="m-0 flex list-none flex-col gap-px">{children}</ul>}
    </section>
  );
}

export function AgentsSurface({ view, events, tools, onOpenDetails }: SurfaceProps) {
  const { router } = useConversationNavigation();
  const threadId = view.thread.thread_id;
  const tree = useMemo(() => buildConversationAgentTree(events, view), [events, view]);
  const shellCount = useMemo(() => tools.filter(isShellLikeTool).length, [tools]);
  const counts = useMemo(() => countAgentStatuses(tree.agents), [tree.agents]);
  const groups = useMemo(() => {
    const owned = tree.roots.filter((agent) => agent.origin === "app_owned");
    return {
      running: owned.filter(subtreeActive),
      finished: owned.filter((agent) => !subtreeActive(agent))
        .map((agent, index) => ({ agent, index }))
        .sort((a, b) => finishedAt(b.agent) - finishedAt(a.agent) || a.index - b.index)
        .map(({ agent }) => agent),
      native: tree.roots.filter((agent) => agent.origin !== "app_owned"),
    };
  }, [tree.roots]);
  const [finishedChoice, setFinishedChoice] = useState<boolean | null>(null);
  const finishedOpen = finishedChoice ?? (!groups.running.length || groups.finished.length <= FINISHED_OPEN_LIMIT);
  const [cancelTarget, setCancelTarget] = useState<ConversationAgentNodeView | null>(null);
  const [cancelOpen, setCancelOpen] = useState(false);
  const surfaceRef = useRef<HTMLDivElement | null>(null);

  const messageForAgent = (agent: ConversationAgentNodeView) => {
    if (agent.messageId) return agent.messageId;
    if (!agent.turnId) return null;
    const messages = view.messages.filter((message) => message.turn_id === agent.turnId);
    const loaded = messages.find((message) => message.role === "user") || messages[0];
    if (loaded) return loaded.message_id;
    const turn = view.turns.find((item) => item.turn_id === agent.turnId);
    if (turn?.text.trim()) return `msg-user-${agent.turnId}`;
    if (turn?.status === "completed") {
      return `msg-asst-${agent.turnId}`;
    }
    return null;
  };

  // A panel shown as a sheet covers the conversation, so it steps aside first.
  const closeSheet = () => {
    if (window.matchMedia(`(max-width: ${PANEL_SHEET_BREAKPOINT}px)`).matches) chatPanel.close(threadId);
  };

  const actions: RowActions = {
    onOpenThread: (agent) => {
      if (!agent.childThreadId) return;
      closeSheet();
      router.push(`/chat/${encodeURIComponent(agent.childThreadId)}`);
    },
    onOpenDetails: (agent) => onOpenDetails(agentDrawerPayload(agent)),
    onJump: (agent) => {
      const messageId = messageForAgent(agent);
      if (!messageId) return;
      closeSheet();
      router.push(buildThreadMessageHref(threadId, messageId));
    },
    canJump: (agent) => Boolean(messageForAgent(agent)),
    onRequestCancel: (agent) => {
      setCancelTarget(agent);
      setCancelOpen(true);
    },
  };

  const cancelAgent = async (agent: ConversationAgentNodeView) => {
    await sendConversationCommand(threadId, "conversation.subagent.cancel", {
      subagent_id: agent.agentId,
      reason: "user_cancelled",
    });
    toast({ title: "已请求取消子代理", tone: "neutral", duration: 2400 });
  };

  const summary = tree.agents.length ? formatAgentCounts(counts) : (tree.toolActivitySummary || "无子代理");
  const rows = (agents: ConversationAgentNodeView[]) => agents.map((agent) => <AgentRow key={agent.agentId} agent={agent} actions={actions} />);

  return (
    <div
      ref={surfaceRef}
      className="flex min-h-0 flex-1 flex-col"
      data-testid="agents-surface"
      data-agent-count={tree.agents.length}
      data-running-count={counts.running + counts.pending}
      data-shell-tool-count={shellCount}
    >
      <SurfaceToolbar className="gap-1.5 px-3">
        <Icon name="bot" size={14} className="shrink-0 text-cx-fg-3" />
        <span className="min-w-0 flex-1 truncate text-[12px] text-cx-fg-2" data-testid="agents-surface-count" title={summary}>
          {summary}
        </span>
        {tree.agents.length && (view.runtime.model || view.runtime.adapter_id) ? (
          <span className="flex min-w-0 max-w-[45%] shrink-0 items-center gap-1 text-[11.5px] text-cx-fg-4" title="主会话使用的模型；子代理各自的模型显示在每一行">
            <span className="shrink-0">主会话</span>
            <span className="truncate font-cx-mono">{view.runtime.model || view.runtime.adapter_id}</span>
          </span>
        ) : null}
      </SurfaceToolbar>
      {tree.agents.length ? (
        <ScrollArea className="flex-1">
          <div className="flex flex-col gap-2 p-2" data-testid="agents-tree">
            {groups.running.length ? (
              <AgentSection title="进行中" count={groups.running.length} testId="agents-section-running">
                {rows(groups.running)}
              </AgentSection>
            ) : null}
            {groups.finished.length ? (
              <AgentSection
                title="已结束"
                count={groups.finished.length}
                testId="agents-section-finished"
                collapsed={!finishedOpen}
                onToggle={() => setFinishedChoice(!finishedOpen)}
              >
                {rows(groups.finished)}
              </AgentSection>
            ) : null}
            {groups.native.length ? (
              <AgentSection
                title="引擎内置"
                count={groups.native.length}
                testId="agents-section-native"
                note="由当前引擎在运行时内部调度，只能查看，不能单独打开或取消。"
              >
                {rows(groups.native)}
              </AgentSection>
            ) : null}
          </div>
        </ScrollArea>
      ) : (
        <div data-testid="agents-empty" className="flex flex-1 flex-col">
          <EmptyState
            icon="bot"
            title={tree.unsupported ? "当前接入未上报子代理" : "暂无子代理"}
            description={(
              <span data-testid="agents-activity-summary">
                {tree.unsupportedReason || tree.toolActivitySummary || "会话委派任务后，这里会列出每个子代理的状态，可以打开、定位或取消。普通 shell 工具不会显示为子代理。"}
              </span>
            )}
            action={shellCount > 0 ? (
              <Button size="sm" variant="ghost" onClick={openExecutionLog} data-testid="agents-shell-note">查看 {shellCount} 条命令工具的 Agent 执行日志</Button>
            ) : undefined}
          />
        </div>
      )}
      <SubagentCancelDialog agent={cancelTarget} open={cancelOpen} onOpenChange={setCancelOpen} onConfirm={cancelAgent} focusRootRef={surfaceRef} />
    </div>
  );
}
