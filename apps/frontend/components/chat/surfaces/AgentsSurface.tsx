"use client";

import { useMemo, useRef, useState, type ReactNode } from "react";
import { useAgentPage } from "@/components/conversation/AgentPageContext";
import { ConversationRouteLink, useConversationNavigation } from "@/components/conversation/ConversationNavigation";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import { Badge, Button, EmptyState, IconButton, Menu, MenuItem, MenuSeparator, ScrollArea, toast, useContextMenu } from "@muteki/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { chatPanel, PANEL_SHEET_BREAKPOINT } from "@/lib/chatPanelStore";
import { controlErrorText, settleControlThread } from "@/lib/controlGrants";
import { sendConversationCommand } from "@/lib/useConversation";
import { workerEngine } from "@/lib/workers";
import {
  agentDisplayName,
  agentOriginLabel,
  agentPreviewText,
  agentRoutingSourceLabel,
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

const FINISHED_OPEN_LIMIT = 3;

function subtreeActive(agent: ConversationAgentNodeView): boolean {
  return !isAgentTerminal(String(agent.status)) || agent.children.some(subtreeActive);
}

function engineName(adapterId?: string | null): string {
  const id = adapterId?.split(".")[0] || "";
  if (!id) return "";
  const name = workerEngine(id);
  return name === "Worker" ? id : name;
}

function agentMeta(agent: ConversationAgentNodeView): string {
  const appOwned = agent.origin === "app_owned";
  const routingSource = agentRoutingSourceLabel(agent);
  return [
    [engineName(agent.adapterId), agent.model || "模型未上报"].filter(Boolean).join(" / "),
    routingSource || null,
    agent.effort ? `思考 ${agent.effort}` : null,
    agent.toolUses ? `${agent.toolUses} 次工具` : null,
    appOwned && agent.isolation === "worktree" ? (worktreeStateLabel(agent.worktreeState) || "worktree 隔离") : null,
  ].filter(Boolean).join(" · ");
}

interface RowActions {
  onOpenThread: (agent: ConversationAgentNodeView) => void;
  onOpenDetails: (agent: ConversationAgentNodeView) => void;
  onRequestCancel: (agent: ConversationAgentNodeView) => void;
  onSettle: (agent: ConversationAgentNodeView) => void;
  onStop: (agent: ConversationAgentNodeView) => void;
}

function AgentRow({ agent, actions }: { agent: ConversationAgentNodeView; actions: RowActions }) {
  const status = String(agent.status);
  const appOwned = agent.origin === "app_owned";
  const name = agentDisplayName(agent);
  const meta = agentMeta(agent);
  const preview = agentPreviewText(agent);
  const openable = Boolean(agent.childThreadId);
  const cancellable = appOwned
    ? agent.cancelSupported !== false && !["settled", "not_delivered"].includes(agent.delivery || "")
    : agent.cancelSupported === true && !isAgentTerminal(status);
  const contextMenu = useContextMenu();
  const menuItems = (
    <>
      {openable ? <MenuItem icon="arrowUpRight" onSelect={() => actions.onOpenThread(agent)}>打开委派线程</MenuItem> : null}
      {appOwned && agent.delivery === "awaiting_settle" ? <MenuItem icon="check" onSelect={() => actions.onSettle(agent)}>收尾委派线程</MenuItem> : null}
      {appOwned && agent.delivery === "running" ? <MenuItem icon="stopCircle" onSelect={() => actions.onStop(agent)}>停止委派线程</MenuItem> : null}
      {cancellable ? <><MenuSeparator /><MenuItem icon="stopCircle" danger disabled={agent.cancelRequested} onSelect={() => actions.onRequestCancel(agent)}>{agent.cancelRequested ? "已请求停止，等待引擎确认" : appOwned ? "取消委派线程…" : "停止原生子代理…"}</MenuItem></> : null}
    </>
  );
  return (
    <li
      data-agent-id={agent.agentId}
      data-parent-id={agent.parentId || ""}
      data-depth={agent.depth}
      data-agent-status={status}
      data-agent-origin={agent.origin || "provider_native"}
      data-child-thread-id={agent.childThreadId || undefined}
    >
      <div className="group/agent relative flex min-w-0 items-start gap-2.5 rounded-lg px-2 py-2 transition-colors hover:bg-cx-hover has-[:focus-visible]:bg-cx-hover" onContextMenu={contextMenu.onContextMenu} onKeyDown={contextMenu.onKeyDown}>
        <AgentAvatar agent={agent} size={22} className="mt-px" />
        <button
          type="button"
          data-agent-primary=""
          aria-keyshortcuts="Shift+F10"
          className="min-w-0 flex-1 rounded-md text-left outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
          title={openable ? "打开这个委派线程" : "查看任务与结果"}
          onClick={() => (openable ? actions.onOpenThread(agent) : actions.onOpenDetails(agent))}
        >
          <span className="flex min-w-0 items-center gap-1.5">
            <span className="min-w-0 truncate text-cx-sm font-medium text-cx-fg">{name}</span>
            <span className="shrink-0 rounded bg-cx-hover px-1 text-cx-2xs leading-4 text-cx-fg-3">{agentOriginLabel(agent)}</span>
            {agent.role && agent.role !== name ? (
              <span className="shrink-0 rounded bg-cx-hover px-1 text-cx-2xs leading-4 text-cx-fg-3">{agent.role}</span>
            ) : null}
          </span>
          <span className="mt-0.5 flex min-w-0 items-center gap-1.5 text-cx-xs leading-5">
            <AgentStatusLabel agent={agent} />
            {meta ? <span className="min-w-0 truncate text-cx-fg-4">{meta}</span> : null}
          </span>
          {appOwned && (agent.delivery === "awaiting_settle" || agent.delivery === "not_delivered" || agent.report) ? (
            <span className="mt-1 flex flex-wrap gap-1">
              {agent.delivery === "awaiting_settle" ? <Badge tone="warning">待收尾</Badge> : null}
              {agent.delivery === "not_delivered" ? <Badge tone="danger">未交付</Badge> : null}
              {agent.report === "reported" ? <Badge tone="success">已报告结果</Badge> : null}
              {agent.report === "unreported" ? <Badge>还没报告结果</Badge> : null}
              {agent.report === "failed" ? <Badge tone="danger">报告失败</Badge> : null}
            </span>
          ) : null}
          {agent.cancelReason ? <span className="mt-0.5 block text-cx-xs leading-5 text-cx-fg-4">{agent.cancelReason}</span> : null}
          {preview ? (
            <span className={cn("mt-0.5 line-clamp-2 break-words text-cx-xs leading-5", status === "failed" ? "text-cx-danger" : "text-cx-fg-3")}>
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
                label="打开委派线程"
                data-testid={`agent-open-thread-${agent.agentId}`}
                onClick={() => actions.onOpenThread(agent)}
              />
            ) : null}
            {appOwned && agent.delivery === "awaiting_settle" ? <IconButton size="xs" icon="check" label="收尾委派线程" onClick={() => actions.onSettle(agent)} /> : null}
            {appOwned && agent.delivery === "running" ? <IconButton size="xs" icon="stopCircle" label="停止委派线程" onClick={() => actions.onStop(agent)} /> : null}
            {cancellable ? (
              <IconButton
                size="xs"
                icon="stopCircle"
                label={agent.cancelRequested ? "已请求停止，等待引擎确认" : appOwned ? "取消委派线程" : "停止原生子代理"}
                disabled={agent.cancelRequested}
                data-testid={`agent-cancel-${agent.agentId}`}
                className="hover:text-cx-danger"
                onClick={() => actions.onRequestCancel(agent)}
              />
            ) : null}
            <Menu trigger={<IconButton size="xs" icon="more" label={`${name} 更多操作`} />} placement="bottom-end" ariaLabel={`${name} 操作`}>
              {menuItems}
            </Menu>
          </span>
        </span>
      </div>
      {contextMenu.render(menuItems, `${name} 操作`)}
      {agent.children.length ? (
        <ul className="m-0 ml-[18px] list-none border-l border-cx-border pl-1.5" aria-label={`${name} 的下级线程`}>
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
            className="-ml-1 flex h-6 items-center gap-1.5 rounded-md px-1 text-cx-xs font-medium text-cx-fg-3 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
          >
            <Icon name="chevronRight" size={12} className={cn("shrink-0 transition-transform duration-150", !collapsed && "rotate-90")} />
            {label}
          </button>
        ) : (
          <h3 className="flex items-center gap-1.5 text-cx-xs font-medium text-cx-fg-3">{label}</h3>
        )}
      </div>
      {note && !collapsed ? <p className="px-2 pb-1 text-cx-xs leading-4 text-cx-fg-4">{note}</p> : null}
      {collapsed ? null : <ul className="m-0 flex list-none flex-col gap-px">{children}</ul>}
    </section>
  );
}

export function AgentsSurface({ view, events, tools, onOpenDetails }: SurfaceProps) {
  const { router } = useConversationNavigation();
  const openAgentPage = useAgentPage();
  const threadId = view.thread.thread_id;
  const tree = useMemo(() => buildConversationAgentTree(events, view), [events, view]);
  const shellCount = useMemo(() => tools.filter(isShellLikeTool).length, [tools]);
  const counts = useMemo(() => countAgentStatuses(tree.agents), [tree.agents]);
  const groups = useMemo(() => {
    const owned = tree.roots.filter((agent) => agent.origin === "app_owned");
    return {
      running: owned.filter((agent) => agent.delivery === "running" || (!agent.delivery && subtreeActive(agent))),
      awaiting: owned.filter((agent) => agent.delivery === "awaiting_settle"),
      finished: owned.filter((agent) => agent.delivery === "settled"),
      undelivered: owned.filter((agent) => agent.delivery === "not_delivered" || (!agent.delivery && !subtreeActive(agent))),
      native: tree.roots.filter((agent) => agent.origin !== "app_owned"),
    };
  }, [tree.roots]);
  const [finishedChoice, setFinishedChoice] = useState<boolean | null>(null);
  const finishedOpen = finishedChoice ?? (!groups.running.length || groups.finished.length <= FINISHED_OPEN_LIMIT);
  const [cancelTarget, setCancelTarget] = useState<ConversationAgentNodeView | null>(null);
  const [cancelOpen, setCancelOpen] = useState(false);
  const surfaceRef = useRef<HTMLDivElement | null>(null);

  const closeSheet = () => {
    if (window.matchMedia(`(max-width: ${PANEL_SHEET_BREAKPOINT}px)`).matches) chatPanel.close(threadId);
  };

  const [settlingAll, setSettlingAll] = useState(false);
  const settleAgent = async (agent: ConversationAgentNodeView): Promise<boolean> => {
    try {
      await settleControlThread(agent.childThreadId || agent.agentId);
      return true;
    } catch (error) {
      toast({ title: `没能收尾「${agentDisplayName(agent)}」`, description: controlErrorText(error, "收尾失败"), tone: "danger" });
      return false;
    }
  };
  const settleAll = async () => {
    setSettlingAll(true);
    try {
      const results = await Promise.all(groups.awaiting.map(settleAgent));
      const done = results.filter(Boolean).length;
      if (done) toast({ title: `已收尾 ${done} 个委派线程`, tone: "success", duration: 2400 });
    } finally {
      setSettlingAll(false);
    }
  };
  const stopAgent = async (agent: ConversationAgentNodeView) => {
    try { await sendConversationCommand(agent.childThreadId || agent.agentId, "conversation.turn.interrupt", {}); }
    catch (error) {
      toast({ title: `没能停止「${agentDisplayName(agent)}」`, description: error instanceof Error ? error.message : "停止失败", tone: "danger" });
    }
  };
  const actions: RowActions = {
    onSettle: (agent) => { void settleAgent(agent); },
    onStop: (agent) => { void stopAgent(agent); },
    onOpenThread: (agent) => {
      if (!agent.childThreadId) return;
      closeSheet();
      if (openAgentPage) openAgentPage(agent.childThreadId);
      else router.push(`/chat/${encodeURIComponent(agent.childThreadId)}`);
    },
    onOpenDetails: (agent) => onOpenDetails(agentDrawerPayload(agent)),
    onRequestCancel: (agent) => {
      setCancelTarget(agent);
      setCancelOpen(true);
    },
  };

  const cancelAgent = async (agent: ConversationAgentNodeView) => {
    if (agent.origin === "app_owned") {
      await sendConversationCommand(threadId, "conversation.subagent.cancel", {
        subagent_id: agent.agentId,
        reason: "user_cancelled",
      });
    } else {
      await sendConversationCommand(threadId, "conversation.background_task.stop", { task_id: agent.agentId });
    }
    toast({ title: "已发送停止请求", tone: "neutral", duration: 2400 });
  };

  const summary = tree.agents.length ? [groups.awaiting.length ? `${groups.awaiting.length} 个待收尾` : "", formatAgentCounts(counts)].filter(Boolean).join(" · ") : (tree.toolActivitySummary || "没有委派线程");
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
      <SurfaceToolbar className="gap-1.5 pl-5 pr-3">
        <Icon name="bot" size={14} className="shrink-0 text-cx-fg-3" />
        <span className="min-w-0 flex-1 truncate text-cx-xs text-cx-fg-2" data-testid="agents-surface-count" title={summary}>
          {summary}
        </span>
        {groups.awaiting.length ? (
          <Button
            size="sm"
            variant="ghost"
            icon="check"
            loading={settlingAll}
            disabled={settlingAll}
            tooltip="收尾所有已完成、等待收尾的委派线程"
            onClick={() => void settleAll()}
          >
            全部收尾
          </Button>
        ) : null}
      </SurfaceToolbar>
      {tree.agents.length ? (
        <ScrollArea className="flex-1">
          <div className="flex flex-col gap-2 p-2" data-testid="agents-tree">
            {groups.awaiting.length ? <AgentSection title="待收尾" count={groups.awaiting.length} testId="agents-section-awaiting">{rows(groups.awaiting)}</AgentSection> : null}
            {groups.running.length ? (
              <AgentSection title="进行中" count={groups.running.length} testId="agents-section-running">
                {rows(groups.running)}
              </AgentSection>
            ) : null}
            {groups.finished.length ? (
              <AgentSection
                title="已收尾"
                count={groups.finished.length}
                testId="agents-section-finished"
                collapsed={!finishedOpen}
                onToggle={() => setFinishedChoice(!finishedOpen)}
              >
                {rows(groups.finished)}
              </AgentSection>
            ) : null}
            {groups.undelivered.length ? <AgentSection title="未交付" count={groups.undelivered.length} testId="agents-section-undelivered">{rows(groups.undelivered)}</AgentSection> : null}
            {groups.native.length ? (
              <AgentSection
                title={`${engineName(view.runtime.adapter_id) || "引擎"} 原生子代理`}
                count={groups.native.length}
                testId="agents-section-native"
                note="由引擎原生协议上报；有子会话时可打开，停止能力以引擎实际支持为准。"
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
            title={tree.unsupported ? "当前引擎没有上报子代理" : "还没有委派线程"}
            description={(
              <span data-testid="agents-activity-summary">
                {tree.unsupported ? (tree.unsupportedReason || "当前引擎不上报子代理的运行情况。") : (
                  <>
                    直接在对话里让 Agent 把任务交给别的模型，例如「让 Claude 并行排查这几个失败的测试」。委派出去的线程会出现在这里，可以打开、停止或收尾。你可以在{" "}
                    <ConversationRouteLink href="/settings/orchestration" className="text-cx-accent hover:underline">设置 → 多 Agent 编排</ConversationRouteLink>
                    {" "}按角色预设每个分工用哪个 Agent 与模型，并在{" "}
                    <ConversationRouteLink href="/settings/control" className="text-cx-accent hover:underline">设置 → 控制</ConversationRouteLink>
                    {" "}中确认开启协作与调度网关。
                  </>
                )}
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
