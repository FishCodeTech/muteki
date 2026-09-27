"use client";

import { useMemo } from "react";
import { useRouter } from "next/navigation";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Button, EmptyState, ScrollArea, Spinner, StatusDot, type Tone } from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { buildThreadMessageHref } from "@/lib/conversationDeepLink";
import { chatPanel, PANEL_SHEET_BREAKPOINT } from "@/lib/chatPanelStore";
import {
  agentStatusLabel,
  buildConversationAgentTree,
  isShellLikeTool,
  type ConversationAgentNodeView,
} from "@/components/conversation/conversationAgentTree";
import { SurfaceToolbar } from "./shared";

function agentTone(status: string): Tone {
  if (status === "running") return "running";
  if (status === "failed") return "danger";
  if (status === "completed") return "success";
  if (status === "cancelled" || status === "declined") return "warning";
  return "neutral";
}

export function AgentsSurface({ view, events, tools, onOpenDetails }: SurfaceProps) {
  const router = useRouter();
  const tree = useMemo(() => buildConversationAgentTree(events, view), [events, view]);
  const shellCount = tools.filter(isShellLikeTool).length;

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

  const openAgent = (agent: ConversationAgentNodeView) => {
    const status = String(agent.status);
    onOpenDetails({
      type: status === "failed" ? "error" : "tool",
      title: agent.title,
      subtitle: [agent.parentId ? `父级 ${agent.parentId}` : "父级未上报", agent.turnId, agent.model].filter(Boolean).join(" · "),
      status: status === "running" || status === "failed" || status === "cancelled" || status === "declined" || status === "completed" ? status : "pending",
      toolName: agent.title,
      input: agent.request || "当前接入已上报任务身份，但尚未提供子任务请求正文。",
      output: agent.result || undefined,
      error: agent.error || undefined,
    });
  };

  const jumpToConversation = (agent: ConversationAgentNodeView) => {
    const threadId = view.thread.thread_id;
    const messageId = messageForAgent(agent);
    if (!messageId) return;
    if (window.matchMedia(`(max-width: ${PANEL_SHEET_BREAKPOINT}px)`).matches) {
      chatPanel.close(threadId);
    }
    router.push(buildThreadMessageHref(threadId, messageId));
  };

  const renderNode = (agent: ConversationAgentNodeView) => {
    const status = String(agent.status);
    const jumpMessageId = messageForAgent(agent);
    return (
      <li
        key={agent.agentId}
        data-agent-id={agent.agentId}
        data-parent-id={agent.parentId || ""}
        data-depth={agent.depth}
        className="relative"
      >
        <div className="group flex items-start gap-2 rounded-xl px-2 py-2 transition-colors hover:bg-cx-hover">
          <span className="mt-[3px] grid size-4 shrink-0 place-items-center">
            {status === "running" ? <Spinner size={13} className="text-cx-accent" /> : <StatusDot tone={agentTone(status)} className="size-2" />}
          </span>
          <button type="button" onClick={() => openAgent(agent)} className="min-w-0 flex-1 text-left">
            <span className="flex items-center gap-2">
              <span className="min-w-0 truncate text-[13px] font-medium text-cx-fg">{agent.title}</span>
              <span className="shrink-0 text-[11.5px] text-cx-fg-4">{agentStatusLabel(status)}</span>
            </span>
            <span className="block truncate text-[11.5px] text-cx-fg-4">
              {[agent.parentId ? `父级 ${agent.parentId}` : "父级未上报", agent.model, agent.turnId ? `回合 ${agent.turnId.slice(0, 8)}` : null].filter(Boolean).join(" · ")}
            </span>
            {agent.request ? (
              <code className="mt-1 line-clamp-2 block rounded-lg bg-cx-code px-2 py-1 font-cx-mono text-[11.5px] leading-[1.5] text-cx-fg-2">{agent.request}</code>
            ) : null}
            {agent.result ? (
              <span className={cn("mt-1 line-clamp-2 block text-[12px] leading-5", agent.error ? "text-cx-danger" : "text-cx-fg-3")}>{agent.result}</span>
            ) : null}
          </button>
          <Button
            size="xs"
            variant="ghost"
            className="shrink-0 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 [@media(max-width:640px)]:opacity-100 [@media(hover:none)]:opacity-100"
            data-testid={`agent-jump-${agent.agentId}`}
            disabled={!jumpMessageId}
            title={jumpMessageId ? "定位到产生此 Agent 的对话消息" : "当前没有可定位的对话消息"}
            onClick={() => jumpToConversation(agent)}
          >
            回到对话
          </Button>
        </div>
        {agent.children.length ? (
          <ul className="relative m-0 ml-[15px] list-none border-l border-cx-border pl-2" aria-label={`${agent.title} 的子 Agent`}>
            {agent.children.map(renderNode)}
          </ul>
        ) : null}
      </li>
    );
  };

  return (
    <div
      className="flex min-h-0 flex-1 flex-col"
      data-testid="agents-surface"
      data-agent-count={tree.agents.length}
      data-shell-tool-count={shellCount}
    >
      <SurfaceToolbar className="px-3">
        <Icon name="bot" size={14} className="text-cx-fg-3" />
        <span className="min-w-0 flex-1 truncate font-cx-mono text-[12px] text-cx-fg-2">{view.runtime.model || view.runtime.adapter_id}</span>
        <span className="shrink-0 text-[12px] text-cx-fg-4" data-testid="agents-surface-count">
          {tree.agents.length ? `${tree.agents.length} 个 Agent` : tree.toolActivitySummary || "无委派 Agent"}
        </span>
      </SurfaceToolbar>
      {tree.agents.length ? (
        <ScrollArea className="flex-1">
          <ul className="m-0 flex list-none flex-col p-2" data-testid="agents-tree">{tree.roots.map(renderNode)}</ul>
        </ScrollArea>
      ) : (
        <div data-testid="agents-empty" className="flex flex-1 flex-col">
          <EmptyState
            icon="bot"
            title={tree.unsupported ? "当前接入未上报委派 Agent" : "暂无委派 Agent"}
            description={(
              <span data-testid="agents-activity-summary">
                {tree.unsupportedReason || tree.toolActivitySummary || "会话启动委派任务后，这里会显示父子关系与执行归属。普通 shell 工具不会显示为独立 Agent。"}
              </span>
            )}
            action={shellCount > 0 ? (
              <span className="text-[12px] text-cx-fg-4" data-testid="agents-shell-note">已忽略 {shellCount} 条 shell/命令工具（见执行日志）</span>
            ) : undefined}
          />
        </div>
      )}
    </div>
  );
}
