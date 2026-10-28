"use client";

import { useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { Button, Callout, Dialog, Spinner, StatusDot, type Tone } from "@/components/chat/ui";
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
  agentPreviewText,
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

const STATUS_TEXT: Record<Tone, string> = {
  neutral: "text-cx-fg-4",
  accent: "text-cx-accent",
  running: "text-cx-accent",
  success: "text-cx-success",
  warning: "text-cx-warning",
  danger: "text-cx-danger",
};

/** Status label for one subagent; a requested cancel reads as stopping until it lands. */
export function AgentStatusLabel({ agent, className }: { agent: ConversationAgentNodeView; className?: string }) {
  const status = String(agent.status);
  if (agent.cancelRequested && !isAgentTerminal(status)) {
    return (
      <span className={cn("flex shrink-0 items-center gap-1 text-cx-warning", className)}>
        <Spinner size={11} />
        正在停止
      </span>
    );
  }
  const tone = agentTone(status);
  return (
    <span className={cn("flex shrink-0 items-center gap-1", STATUS_TEXT[tone], className)}>
      {status === "running" ? <Spinner size={11} /> : <StatusDot tone={tone} className="size-1.5" />}
      {agent.origin === "app_owned" && status === "completed" ? "执行完成" : agentStatusLabel(status)}
    </span>
  );
}

/** How long after a cancel lost focus is still put back (cancels normally land within seconds). */
const CANCEL_FOCUS_WINDOW_MS = 30_000;

/**
 * A cancelled row's cancel button first disables and then disappears (the row may also move to
 * "已结束" or its group may collapse), which drops keyboard focus to <body>. For a short window,
 * lost focus goes back to the row's main button inside `root`, or to the list's fallback control
 * when the row is gone. Any pointer press ends the window so mouse users are never pulled back.
 */
function recoverFocusAfterCancel(root: HTMLElement, agentId: string): () => void {
  let observer: MutationObserver | null = null;
  let timer = 0;
  const stop = () => {
    observer?.disconnect();
    observer = null;
    window.clearTimeout(timer);
    window.removeEventListener("pointerdown", stop, true);
  };
  const recover = () => {
    if (!root.isConnected) {
      stop();
      return;
    }
    const active = document.activeElement;
    const lost = !active || active === document.body || (active instanceof HTMLButtonElement && active.disabled);
    if (!lost) return;
    const target = root.querySelector<HTMLElement>(`[data-agent-id="${CSS.escape(agentId)}"] [data-agent-primary]`)
      ?? root.querySelector<HTMLElement>("[data-agent-focus-fallback]");
    target?.focus({ preventScroll: true });
  };
  observer = new MutationObserver(recover);
  observer.observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ["disabled"] });
  timer = window.setTimeout(stop, CANCEL_FOCUS_WINDOW_MS);
  window.addEventListener("pointerdown", stop, true);
  return stop;
}

/** fetch() reports an unreachable service as a TypeError with browser-specific English text. */
function cancelErrorMessage(reason: unknown): string {
  const message = reason instanceof Error ? reason.message : String(reason);
  if (reason instanceof TypeError && /Failed to fetch|NetworkError|Load failed|network request failed/i.test(message)) {
    return "网络不可用，取消请求可能没有送达。请检查连接后重试。";
  }
  return message;
}

/**
 * Confirms a subagent cancel. The request also stops that subagent's own
 * descendants; a failed request keeps the dialog open with the error and
 * focus on the confirm button. After a successful one, keyboard focus stays
 * on the row inside `focusRootRef` while it settles.
 */
export function SubagentCancelDialog({
  agent,
  open,
  onOpenChange,
  onConfirm,
  focusRootRef,
}: {
  agent: ConversationAgentNodeView | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onConfirm: (agent: ConversationAgentNodeView) => void | Promise<void>;
  focusRootRef?: RefObject<HTMLElement | null>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const confirmRef = useRef<HTMLButtonElement | null>(null);
  const stopFocusRecovery = useRef<(() => void) | null>(null);
  const agentId = agent?.agentId;
  useEffect(() => {
    if (open) setError("");
  }, [open, agentId]);
  // The confirm button disables while the request runs, which drops focus; a failure hands it back.
  useEffect(() => {
    if (open && error && !busy) confirmRef.current?.focus({ preventScroll: true });
  }, [open, error, busy]);
  useEffect(() => () => stopFocusRecovery.current?.(), []);
  const name = agent ? agentDisplayName(agent) : "子代理";
  const confirm = async () => {
    if (!agent || busy) return;
    setBusy(true);
    setError("");
    try {
      await onConfirm(agent);
      const root = focusRootRef?.current;
      if (root) {
        stopFocusRecovery.current?.();
        stopFocusRecovery.current = recoverFocusAfterCancel(root, agent.agentId);
      }
      onOpenChange(false);
    } catch (reason) {
      setError(cancelErrorMessage(reason));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => { if (!busy) onOpenChange(next); }}
      size="sm"
      icon="stopCircle"
      tone="danger"
      title="取消这个子代理？"
      description={`「${name}」会停止当前运行，它派生的子代理也会一起停止。已产生的会话记录会保留。`}
      dismissable={!busy}
      testId="subagent-cancel-dialog"
      footer={(
        <>
          <Button size="sm" variant="secondary" disabled={busy} data-autofocus onClick={() => onOpenChange(false)}>继续运行</Button>
          <Button ref={confirmRef} size="sm" variant="danger" loading={busy} data-testid="subagent-cancel-confirm" onClick={() => void confirm()}>取消子代理</Button>
        </>
      )}
    >
      {error ? <Callout tone="danger" role="alert" title="取消未完成"><span className="whitespace-pre-wrap break-words">{error}</span></Callout> : null}
    </Dialog>
  );
}

/** Composer-adjacent summary of subagents still waiting or running; opens the subagents panel. */
export function SubagentActivityPill({ active, total, onOpen }: { active: number; total: number; onOpen: () => void }) {
  if (active <= 0) return null;
  return (
    <div className="mb-2 flex">
      <button
        type="button"
        data-testid="subagent-activity-pill"
        aria-label={`${active} 个子代理进行中，打开子代理面板`}
        onClick={onOpen}
        className={cn(
          "cx-press inline-flex h-7 max-w-full items-center gap-1.5 rounded-full border border-cx-border bg-cx-elevated pl-2 pr-1.5 text-[12.5px] text-cx-fg-2 shadow-cx-xs outline-none",
          "hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
        )}
      >
        <Spinner size={12} className="text-cx-accent" />
        <span className="truncate">{active} 个子代理进行中</span>
        {total > active ? <span className="shrink-0 text-cx-fg-4">· 共 {total} 个</span> : null}
        <Icon name="chevronRight" size={13} className="shrink-0 text-cx-fg-4" />
      </button>
    </div>
  );
}

export function agentDrawerPayload(agent: ConversationAgentNodeView): DrawerDetailPayload {
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
      agent.effort ? `思考 ${agent.effort}` : null,
      agent.sessionRef,
      appOwned && agent.lineageDepth != null ? `深度 ${agent.lineageDepth}` : null,
      appOwned && agent.isolation === "worktree"
        ? (agent.worktreePath || worktreeStateLabel(agent.worktreeState))
        : null,
    ].filter(Boolean).join(" · "),
    status: ["running", "failed", "cancelled", "declined", "completed", "interrupted"].includes(status) ? status : "pending",
    toolName: agent.title,
    input: [
      agent.request || "子代理没有上报任务正文。",
      agent.acceptanceCriteria ? `验收条件\n${agent.acceptanceCriteria}` : null,
      agent.routingReason ? `模型选择依据\n${agent.routingReason}` : null,
    ].filter(Boolean).join("\n\n"),
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
  onRequestCancel,
}: {
  agent: ConversationAgentNodeView;
  tools: ConversationToolRecord[];
  threadId?: string;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  onOpenThread?: (threadId: string) => void;
  onRequestCancel?: (agent: ConversationAgentNodeView) => void;
}) {
  const [open, setOpen] = useState(false);
  const status = String(agent.status);
  const terminal = isAgentTerminal(status);
  const appOwned = agent.origin === "app_owned";
  const name = agentDisplayName(agent);
  const line = agentPreviewText(agent);
  const toolCount = agent.toolUses ?? (tools.length || null);
  const hasDetails = Boolean(agent.request || agent.result || agent.error || tools.length);
  const meta = appOwned
    ? [
        [agent.adapterId, agent.model].filter(Boolean).join(" / "),
        agent.effort ? `思考 ${agent.effort}` : null,
        agent.lineageDepth != null ? `深度 ${agent.lineageDepth}` : null,
        agent.isolation === "worktree"
          ? (worktreeStateLabel(agent.worktreeState) || "worktree 隔离")
          : null,
        agent.accessMode ? `权限 ${agent.accessMode}` : null,
      ].filter(Boolean).join(" · ")
    : "";
  const cancellable = appOwned && !terminal && Boolean(onRequestCancel);

  return (
    <li data-agent-id={agent.agentId} data-agent-status={status} data-agent-origin={agent.origin || "provider_native"} data-child-thread-id={agent.childThreadId || undefined} style={{ paddingLeft: agent.depth * 16 }}>
      <div className="group/agent flex min-w-0 items-start gap-2 rounded-lg px-1.5 py-1.5 hover:bg-cx-hover">
        <AgentAvatar agent={agent} size={20} className="mt-px" />
        <button
          type="button"
          data-agent-primary=""
          className="min-w-0 flex-1 text-left"
          aria-expanded={hasDetails ? open : undefined}
          onClick={() => hasDetails && setOpen((value) => !value)}
        >
          <span className="flex min-w-0 flex-wrap items-center gap-1.5">
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
            <AgentStatusLabel agent={agent} className="text-[12px]" />
          </span>
          {meta ? (
            <span className="mt-0.5 line-clamp-1 text-[12px] leading-5 text-cx-fg-4">{meta}</span>
          ) : null}
          {line ? (
            <span className={cn("mt-0.5 line-clamp-1 break-words text-[12px] leading-5", status === "failed" ? "text-cx-danger" : "text-cx-fg-3")}>
              {line}
            </span>
          ) : null}
        </button>
        <span className="mt-0.5 flex shrink-0 flex-wrap items-center justify-end gap-2">
          {toolCount ? <span className="text-[12px] text-cx-fg-4">{toolCount} 次工具</span> : null}
          <AgentDuration agent={agent} />
          {appOwned && agent.childThreadId && onOpenThread ? (
            <button
              type="button"
              className="rounded px-1 py-0.5 text-[12px] text-cx-accent hover:bg-cx-accent-soft"
              aria-label={`打开 ${name} 的子会话`}
              title="打开子会话"
              onClick={() => onOpenThread(agent.childThreadId!)}
            >
              子会话
            </button>
          ) : null}
          {cancellable ? (
            <button
              type="button"
              className="rounded p-0.5 text-cx-fg-4 transition-opacity hover:text-cx-danger [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover/agent:opacity-100 focus-visible:opacity-100 disabled:opacity-40"
              aria-label={`取消 ${name}`}
              title={agent.cancelRequested ? "已请求取消，等待停止" : "取消该子代理"}
              disabled={agent.cancelRequested}
              onClick={() => onRequestCancel?.(agent)}
            >
              <Icon name="stopCircle" size={13} />
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
          {appOwned && agent.acceptanceCriteria ? (
            <div className="text-[12px]">
              <div className="mb-0.5 text-cx-fg-4">验收条件</div>
              <p className="line-clamp-4 whitespace-pre-wrap break-words text-cx-fg-2">{agent.acceptanceCriteria}</p>
            </div>
          ) : null}
          {appOwned && agent.routingReason ? (
            <div className="text-[12px]">
              <div className="mb-0.5 text-cx-fg-4">模型选择依据</div>
              <p className="line-clamp-3 whitespace-pre-wrap break-words text-cx-fg-2">{agent.routingReason}</p>
            </div>
          ) : null}
          {appOwned && agent.activity ? <p className="text-[12px] text-cx-fg-3">{agent.activity}</p> : null}
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
          <button
            type="button"
            className="self-start rounded px-1 py-0.5 text-[12px] text-cx-accent hover:bg-cx-accent-soft"
            onClick={() => onOpenDrawer(agentDrawerPayload(agent))}
          >
            查看完整任务与结果
          </button>
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
  const [cancelTarget, setCancelTarget] = useState<ConversationAgentNodeView | null>(null);
  const [cancelOpen, setCancelOpen] = useState(false);
  const groupRef = useRef<HTMLDivElement | null>(null);
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
      ref={groupRef}
      className="my-1 rounded-xl border border-cx-border bg-cx-bg-subtle"
      data-testid="subagent-group"
      data-agent-count={agents.length}
      data-running-count={counts.running}
    >
      <div className="flex min-w-0 items-center gap-2 px-2.5 py-2">
        <button
          type="button"
          data-agent-focus-fallback=""
          className="flex min-w-0 flex-1 items-center gap-2 text-left"
          aria-expanded={open}
          onClick={() => setManualOpen(!open)}
        >
          <AgentAvatarStack agents={agents} />
          <span className="shrink-0 text-[13px] font-medium text-cx-fg">
            {agents.length === 1 ? "子代理" : `${agents.length} 个子代理`}
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
            title="在子代理面板中查看全部子代理"
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
              onRequestCancel={onCancelSubagent ? (target) => { setCancelTarget(target); setCancelOpen(true); } : undefined}
            />
          ))}
        </ul>
      ) : null}
      {onCancelSubagent ? (
        <SubagentCancelDialog
          agent={cancelTarget}
          open={cancelOpen}
          onOpenChange={setCancelOpen}
          onConfirm={(target) => onCancelSubagent(target.agentId)}
          focusRootRef={groupRef}
        />
      ) : null}
    </div>
  );
}
