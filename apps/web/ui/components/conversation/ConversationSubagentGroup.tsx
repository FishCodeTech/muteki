"use client";

import { useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { Spinner, StatusDot, type Tone } from "@/components/chat/ui";
import { ElapsedTimer } from "@/components/chat/timeline/ElapsedTimer";
import { ToolEntry } from "@/components/chat/timeline/ToolEntry";
import { formatTurnDuration } from "@/components/chat/timeline/toolPresentation";
import { chatPanel } from "@/lib/chatPanelStore";
import { toolDrawerPayload } from "./ConversationToolFlow";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import type { ConversationToolRecord } from "./conversationEventViews";
import {
  agentAvatarHue,
  agentDisplayName,
  agentDurationMs,
  agentInitial,
  agentOriginLabel,
  agentStatusLabel,
  countAgentStatuses,
  formatAgentCounts,
  isAgentTerminal,
  worktreeStateLabel,
  type ConversationAgentNodeView,
} from "./conversationAgentTree";

const MAX_AVATARS = 4;

export function agentTone(status: string): Tone {
  if (status === "running") return "running";
  if (status === "failed") return "danger";
  if (status === "completed") return "success";
  if (status === "cancelled" || status === "declined") return "warning";
  return "neutral";
}

export function AgentAvatar({
  agent,
  size = 18,
  className,
}: {
  agent: ConversationAgentNodeView;
  size?: number;
  className?: string;
}) {
  const hue = agentAvatarHue(agent);
  return (
    <span
      aria-hidden
      className={cn(
        "grid shrink-0 place-items-center rounded-full font-semibold leading-none ring-2 ring-cx-bg",
        String(agent.status) === "running" && "animate-pulse",
        className,
      )}
      style={{
        width: size,
        height: size,
        fontSize: Math.round(size * 0.52),
        background: `oklch(0.72 0.12 ${hue} / 0.22)`,
        color: `oklch(0.62 0.15 ${hue})`,
      }}
    >
      {agentInitial(agent)}
    </span>
  );
}

export function AgentAvatarStack({
  agents,
  size = 18,
  max = MAX_AVATARS,
}: {
  agents: ConversationAgentNodeView[];
  size?: number;
  max?: number;
}) {
  const shown = agents.slice(0, max);
  const rest = agents.length - shown.length;
  return (
    <span className="flex shrink-0 items-center -space-x-1.5">
      {shown.map((agent) => <AgentAvatar key={agent.agentId} agent={agent} size={size} />)}
      {rest > 0 ? (
        <span
          className="grid shrink-0 place-items-center rounded-full bg-cx-hover font-medium text-cx-fg-3 ring-2 ring-cx-bg"
          style={{ width: size, height: size, fontSize: Math.round(size * 0.48) }}
        >
          +{rest}
        </span>
      ) : null}
    </span>
  );
}

export function AgentDuration({ agent }: { agent: ConversationAgentNodeView }) {
  const status = String(agent.status);
  const start = agent.startedAt ? Date.parse(agent.startedAt) : NaN;
  if (!isAgentTerminal(status) && Number.isFinite(start)) {
    return <ElapsedTimer startMs={start} className="shrink-0 text-[12px] text-cx-fg-4" />;
  }
  const ms = agentDurationMs(agent);
  return ms != null && ms > 0 ? (
    <span className="cx-tabular shrink-0 text-[12px] text-cx-fg-4">{formatTurnDuration(ms)}</span>
  ) : null;
}

function agentDrawerPayload(agent: ConversationAgentNodeView): DrawerDetailPayload {
  const status = String(agent.status);
  const appOwned = agent.origin === "app_owned";
  return {
    type: status === "failed" ? "error" : "tool",
    title: agentDisplayName(agent),
    subtitle: [
      agentOriginLabel(agent),
      agent.role,
      agent.adapterId,
      agent.model,
      agent.sessionRef,
      appOwned && agent.lineageDepth != null ? `深度 ${agent.lineageDepth}` : null,
      appOwned && agent.isolation === "worktree"
        ? (agent.worktreePath || worktreeStateLabel(agent.worktreeState))
        : null,
    ].filter(Boolean).join(" · "),
    status: ["running", "failed", "cancelled", "declined", "completed", "interrupted"].includes(status) ? status : "pending",
    toolName: agent.title,
    input: agent.request || "子智能体没有上报任务正文。",
    output: agent.result || undefined,
    error: agent.error || undefined,
  };
}

function SubagentRow({
  agent,
  tools,
  threadId,
  onOpenDrawer,
  onOpenThread,
  onCancelSubagent,
}: {
  agent: ConversationAgentNodeView;
  tools: ConversationToolRecord[];
  threadId?: string;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  onOpenThread?: (threadId: string) => void;
  onCancelSubagent?: (subagentId: string) => void | Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const status = String(agent.status);
  const terminal = isAgentTerminal(status);
  const appOwned = agent.origin === "app_owned";
  const name = agentDisplayName(agent);
  const line = terminal
    ? (agent.error || agent.result || agent.activity || "")
    : (agent.activity || agent.request || "");
  const toolCount = agent.toolUses ?? (tools.length || null);
  const hasDetails = Boolean(agent.request || agent.result || agent.error || tools.length);
  const meta = appOwned
    ? [
        [agent.adapterId, agent.model].filter(Boolean).join(" / "),
        agent.lineageDepth != null ? `深度 ${agent.lineageDepth}` : null,
        agent.isolation === "worktree"
          ? (worktreeStateLabel(agent.worktreeState) || "worktree 隔离")
          : null,
        agent.accessMode ? `权限 ${agent.accessMode}` : null,
      ].filter(Boolean).join(" · ")
    : "";
  const cancellable = appOwned && !terminal && Boolean(onCancelSubagent);

  return (
    <li data-agent-id={agent.agentId} data-agent-status={status} data-agent-origin={agent.origin || "provider_native"} style={{ paddingLeft: agent.depth * 16 }}>
      <div className="group/agent flex min-w-0 items-start gap-2 rounded-lg px-1.5 py-1.5 hover:bg-cx-hover">
        <AgentAvatar agent={agent} size={20} className="mt-px" />
        <button
          type="button"
          className="min-w-0 flex-1 text-left"
          aria-expanded={hasDetails ? open : undefined}
          onClick={() => hasDetails && setOpen((value) => !value)}
        >
          <span className="flex min-w-0 items-center gap-1.5">
            <span className="min-w-0 truncate text-[13px] font-medium text-cx-fg">{name}</span>
            <span className={cn(
              "shrink-0 rounded px-1 text-[11px] leading-4",
              appOwned ? "bg-cx-accent-soft text-cx-accent" : "bg-cx-hover text-cx-fg-3",
            )}>
              {agentOriginLabel(agent)}
            </span>
            {agent.role && agent.role !== name ? (
              <span className="shrink-0 rounded bg-cx-hover px-1 text-[11px] leading-4 text-cx-fg-3">{agent.role}</span>
            ) : null}
            <span className="flex shrink-0 items-center gap-1 text-[12px] text-cx-fg-4">
              {status === "running" ? <Spinner size={11} className="text-cx-accent" /> : <StatusDot tone={agentTone(status)} className="size-1.5" />}
              {agentStatusLabel(status)}
            </span>
          </span>
          {meta ? (
            <span className="mt-0.5 line-clamp-1 block text-[12px] leading-5 text-cx-fg-4">{meta}</span>
          ) : null}
          {line ? (
            <span className={cn("mt-0.5 line-clamp-1 block text-[12px] leading-5", status === "failed" ? "text-cx-danger" : "text-cx-fg-3")}>
              {line}
            </span>
          ) : null}
        </button>
        <span className="mt-0.5 flex shrink-0 items-center gap-2">
          {toolCount ? <span className="text-[12px] text-cx-fg-4">{toolCount} 次工具</span> : null}
          <AgentDuration agent={agent} />
          {appOwned && agent.childThreadId && onOpenThread ? (
            <button
              type="button"
              className="rounded p-0.5 text-cx-fg-4 transition-opacity hover:text-cx-fg [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover/agent:opacity-100 focus-visible:opacity-100"
              aria-label={`打开 ${name} 的子会话`}
              title="打开子会话"
              onClick={() => onOpenThread(agent.childThreadId!)}
            >
              <Icon name="externalLink" size={13} />
            </button>
          ) : null}
          {cancellable ? (
            <button
              type="button"
              className="rounded p-0.5 text-cx-fg-4 transition-opacity hover:text-cx-danger [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover/agent:opacity-100 focus-visible:opacity-100 disabled:opacity-40"
              aria-label={`取消 ${name}`}
              title={agent.cancelRequested ? "已请求取消，等待停止" : "取消该子代理"}
              disabled={cancelling || agent.cancelRequested}
              onClick={async () => {
                if (!onCancelSubagent) return;
                setCancelling(true);
                try {
                  await onCancelSubagent(agent.agentId);
                } finally {
                  setCancelling(false);
                }
              }}
            >
              <Icon name="x" size={13} />
            </button>
          ) : null}
          <button
            type="button"
            className="rounded p-0.5 text-cx-fg-4 opacity-0 transition-opacity hover:text-cx-fg group-hover/agent:opacity-100 focus-visible:opacity-100 [@media(hover:none)]:opacity-100"
            aria-label={`查看 ${name} 详情`}
            onClick={() => onOpenDrawer(agentDrawerPayload(agent))}
          >
            <Icon name="maximize" size={13} />
          </button>
        </span>
      </div>
      {open ? (
        <div className="mb-1 ml-[30px] flex flex-col gap-1.5 border-l border-cx-border pl-3">
          {agent.request ? (
            <div className="text-[12px]">
              <div className="mb-0.5 text-cx-fg-4">任务</div>
              <p className="line-clamp-4 whitespace-pre-wrap text-cx-fg-2">{agent.request}</p>
            </div>
          ) : null}
          {appOwned && agent.worktreePath ? (
            <div className="text-[12px]">
              <div className="mb-0.5 text-cx-fg-4">Worktree</div>
              <p className="line-clamp-2 break-all font-cx-mono text-cx-fg-2">{agent.worktreePath}</p>
            </div>
          ) : null}
          {appOwned && agent.errorCode ? (
            <div className="text-[12px]">
              <div className="mb-0.5 text-cx-fg-4">错误码</div>
              <p className="line-clamp-2 break-all font-cx-mono text-cx-danger">{agent.errorCode}</p>
            </div>
          ) : null}
          {tools.length ? (
            <div className="-ml-1.5 flex flex-col">
              {tools.map((tool) => (
                <ToolEntry
                  key={tool.id}
                  tool={terminal && (tool.status === "running" || tool.status === "pending")
                    ? { ...tool, status: status === "failed" ? "failed" : status === "completed" ? "completed" : "cancelled" }
                    : tool}
                  threadId={threadId}
                  onSelect={() => onOpenDrawer(toolDrawerPayload(tool))}
                />
              ))}
            </div>
          ) : null}
          {agent.result || agent.error ? (
            <div className="text-[12px]">
              <div className="mb-0.5 text-cx-fg-4">{agent.error ? "错误" : "结果"}</div>
              <p className={cn("line-clamp-6 whitespace-pre-wrap", agent.error ? "text-cx-danger" : "text-cx-fg-2")}>
                {agent.error || agent.result}
              </p>
            </div>
          ) : null}
        </div>
      ) : null}
    </li>
  );
}

function flatten(roots: ConversationAgentNodeView[]): ConversationAgentNodeView[] {
  const out: ConversationAgentNodeView[] = [];
  const walk = (nodes: ConversationAgentNodeView[]) => {
    for (const node of nodes) {
      out.push(node);
      walk(node.children);
    }
  };
  walk(roots);
  return out;
}

/**
 * One collapsible card per turn summarising every subagent that turn spawned
 * ("2 个运行中 · 11 个完成"), replacing the raw delegation tool rows.
 */
export function ConversationSubagentGroup({
  roots,
  toolsByAgent,
  threadId,
  onOpenDrawer,
  onOpenThread,
  onCancelSubagent,
}: {
  roots: ConversationAgentNodeView[];
  toolsByAgent: Record<string, ConversationToolRecord[]>;
  threadId?: string;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  onOpenThread?: (threadId: string) => void;
  onCancelSubagent?: (subagentId: string) => void | Promise<void>;
}) {
  const agents = useMemo(() => flatten(roots), [roots]);
  const counts = useMemo(() => countAgentStatuses(agents), [agents]);
  const active = counts.running + counts.pending > 0;
  const [manualOpen, setManualOpen] = useState<boolean | null>(null);
  const open = manualOpen ?? active;
  const earliestRunningStart = useMemo(() => {
    const starts = agents
      .filter((agent) => !isAgentTerminal(String(agent.status)) && agent.startedAt)
      .map((agent) => Date.parse(agent.startedAt!))
      .filter(Number.isFinite);
    return starts.length ? Math.min(...starts) : null;
  }, [agents]);
  if (!agents.length) return null;

  return (
    <div
      className="my-1 rounded-xl border border-cx-border bg-cx-bg-subtle"
      data-testid="subagent-group"
      data-agent-count={agents.length}
      data-running-count={counts.running}
    >
      <div className="flex min-w-0 items-center gap-2 px-2.5 py-2">
        <button
          type="button"
          className="flex min-w-0 flex-1 items-center gap-2 text-left"
          aria-expanded={open}
          onClick={() => setManualOpen(!open)}
        >
          <AgentAvatarStack agents={agents} />
          <span className="shrink-0 text-[13px] font-medium text-cx-fg">
            {agents.length === 1 ? "子智能体" : `${agents.length} 个子智能体`}
          </span>
          <span className="min-w-0 truncate text-[12px] text-cx-fg-3" data-testid="subagent-group-counts">
            {formatAgentCounts(counts)}
          </span>
          {earliestRunningStart != null ? (
            <ElapsedTimer startMs={earliestRunningStart} className="shrink-0 text-[12px] text-cx-fg-4" />
          ) : null}
          <Icon name={open ? "chevronUp" : "chevronDown"} size={14} className="ml-auto shrink-0 text-cx-fg-4" />
        </button>
        {threadId ? (
          <button
            type="button"
            className="shrink-0 rounded-md px-1.5 py-0.5 text-[12px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg"
            onClick={() => chatPanel.open(threadId, "agents")}
          >
            面板
          </button>
        ) : null}
      </div>
      {open ? (
        <ul className="m-0 flex list-none flex-col gap-px border-t border-cx-border px-1 py-1">
          {agents.map((agent) => (
            <SubagentRow
              key={agent.agentId}
              agent={agent}
              tools={toolsByAgent[agent.agentId] || []}
              threadId={threadId}
              onOpenDrawer={onOpenDrawer}
              onOpenThread={onOpenThread}
              onCancelSubagent={onCancelSubagent}
            />
          ))}
        </ul>
      ) : null}
    </div>
  );
}
