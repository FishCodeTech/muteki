"use client";

import { createContext, lazy, Suspense, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";
import { usePathname, useRouter } from "next/navigation";
import type { ConversationExperience, ConversationNavigation } from "@/components/conversation/ConversationShell";
import { queueSideChatQuote, requestSideChatFocus, type SideChatParentStatus } from "@/components/conversation/ConversationSideChat";
import { Badge, Button, IconButton, Menu, MenuItem, MenuLabel, MenuSeparator, Popover, ResizeHandle, toast } from "@muteki/ui";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import { cn } from "@/lib/cn";
import { randomUuid } from "@/lib/browserCrypto";
import { isRunActive, swarmPhase, type DeckState } from "@/lib/events";
import { asRunWorkspaceMode, conversationHref } from "@/lib/runWorkspaceRoutes";
import { useRun } from "@/lib/useRun";
import type { ConversationThread } from "@/lib/useConversation";
import {
  RUN_SIDE_WIDTH_DEFAULT,
  RUN_SIDE_WIDTH_MIN,
  closeRunSideChatTab,
  createRunSideChat,
  deleteRunSideChat,
  fetchRunSideChats,
  selectRunSideChatTarget,
  showRunSideChat,
  updateRunSideChatLayout,
  useRunSideChatLayout,
  useRunSideChatPopout,
  useRunSideChatWindow,
  closeRunSideChatWindow,
  type RunSideChatLayout,
  type RunSideChatTarget,
} from "./runSideChatStore";

const ConversationShell = lazy(() => import("@/components/conversation/ConversationShell").then((module) => ({ default: module.ConversationShell })));

export type RunSideChatRun = { runId: string; mode: "ctf" | "pentest"; title: string; status: SideChatParentStatus };

export function runSideChatStatus(deck: DeckState, running: boolean): SideChatParentStatus {
  if (!deck.started) return { label: "读取中", tone: "idle" };
  const phase = swarmPhase(deck);
  if (phase === "paused") return { label: "已暂停", tone: "attention" };
  if (running) return { label: "运行中", tone: "running" };
  if (phase === "solved") return { label: "已解出", tone: "idle" };
  if (phase === "goal_met") return { label: "已达成目标", tone: "idle" };
  return { label: "已结束", tone: "idle" };
}

function failureText(failure: unknown) {
  return failure instanceof Error ? failure.message : String(failure);
}

function runExperience(mode: "ctf" | "pentest", sheetPanel: boolean): ConversationExperience {
  const evidence = mode === "pentest"
    ? "梳理当前发现和证据：哪些已有可复现的证据，哪些还需要验证？"
    : "检查当前证据和 Flag 候选：哪些已验证，哪些还缺证据？";
  return {
    allowFork: false,
    emptyThread: {
      icon: "messages",
      title: "Run 侧边聊天",
      hint: "仅限选择本 Run 已出战的 Worker（继承完整工作上下文）或出战 Agent（继承全局 Run 上下文），不会干扰主对话。关闭标签后可从 + 菜单重新打开。",
      suggestions: [
        { icon: "activity", label: "总结进展", prompt: "总结这个 Run 目前的进展：已确认的事实、正在进行的方向和下一步。" },
        { icon: "listTodo", label: "未完成方向", prompt: "列出这个 Run 当前还没完成的方向，以及各自卡在哪里。" },
        { icon: "history", label: "失败方向", prompt: "回顾已经失败或放弃的方向和原因，避免重复尝试。" },
        { icon: mode === "pentest" ? "shield" : "flag", label: "证据状态", prompt: evidence },
      ],
    },
    composerPlaceholder: "就这个 Run 提问，或让它查看、操作 Run…",
    panelExtensions: sheetPanel ? { extensions: [], sheetBreakpoint: 100000 } : undefined,
  };
}

async function startRunSideChat(
  runId: string,
  question = "",
  deliver?: (threadId: string, question: string) => void,
  targetId?: string,
): Promise<string> {
  try {
    const threadId = await createRunSideChat(runId, targetId);
    if (question && deliver) deliver(threadId, question);
    else requestSideChatFocus(threadId);
    showRunSideChat(runId, threadId);
    return threadId;
  } catch (failure) {
    toast({ title: "侧边聊天未能开启", description: failureText(failure), tone: "danger" });
    return "";
  }
}

export function useRunSideChatController(runId: string) {
  const layout = useRunSideChatLayout(runId);
  const popout = useRunSideChatPopout(runId);
  const creating = useRef(false);

  const newChat = useCallback(async (question = "", targetId?: string) => {
    if (!runId || creating.current) return "";
    creating.current = true;
    const deliver = (threadId: string, text: string) => {
      if (popout.popped) popout.sendQuote(threadId, text, true);
      else queueSideChatQuote(threadId, { plainText: text, key: randomUuid(), send: true });
    };
    try { return await startRunSideChat(runId, question, deliver, targetId); }
    finally { creating.current = false; }
  }, [runId, popout]);

  const open = useCallback(async (options: { question?: string; quote?: string } = {}) => {
    if (!runId) return;
    if (options.question) { await newChat(options.question); return; }
    let threadId = layout.active;
    if (!threadId) threadId = await newChat();
    if (!threadId) return;
    if (options.quote) {
      const text = options.quote.split("\n").map((line) => `> ${line}`).join("\n");
      if (popout.popped) popout.sendQuote(threadId, `${text}\n\n`);
      else queueSideChatQuote(threadId, { plainText: `${text}\n\n`, key: randomUuid() });
    }
    requestSideChatFocus(threadId);
    showRunSideChat(runId, threadId);
  }, [runId, layout.active, newChat, popout]);

  const toggle = useCallback(() => {
    if (layout.open && layout.tabs.length) updateRunSideChatLayout(runId, { open: false, maximized: false });
    else void open();
  }, [runId, layout.open, layout.tabs.length, open]);

  return { runId, layout, popout, newChat, open, toggle };
}

export type RunSideChatController = ReturnType<typeof useRunSideChatController>;

export type RunSideChatFrame = { narrow: boolean; view: "main" | "side"; setView: (view: "main" | "side") => void; maxWidth: number };
export const RunSideChatFrameContext = createContext<RunSideChatFrame>({ narrow: false, view: "side", setView: () => {}, maxWidth: 4000 });

function useIsolatedNavigation(): ConversationNavigation {
  const router = useRouter();
  const pathname = usePathname();
  const [params, setParams] = useState(() => new URLSearchParams());
  return useMemo(() => ({
    pathname,
    searchParams: params,
    router: {
      push: (href: string, options?: { scroll?: boolean }) => {
        const [path, query = ""] = href.split("?");
        if (path === pathname) { setParams(new URLSearchParams(query)); return; }
        router.push(href, options);
      },
      replace: (href: string, options?: { scroll?: boolean }) => {
        const [path, query = ""] = href.split("?");
        if (path === pathname) { setParams(new URLSearchParams(query)); return; }
        router.replace(href, options);
      },
    },
  }), [pathname, params, router]);
}

function useRunSideThreads(runId: string, tabs: string[], statusLabel: string) {
  const [threads, setThreads] = useState<ConversationThread[]>([]);
  const [targets, setTargets] = useState<RunSideChatTarget[]>([]);
  const [defaultTargetId, setDefaultTargetId] = useState("");
  const [error, setError] = useState("");
  const refresh = useCallback(async () => {
    if (!runId) return;
    try {
      const data = await fetchRunSideChats(runId);
      setThreads(data.threads);
      setTargets(data.targets);
      setDefaultTargetId(data.defaultTargetId);
      setError("");
    } catch (failure) {
      setError(failureText(failure));
    }
  }, [runId]);
  const tabKey = tabs.join(",");
  useEffect(() => { void refresh(); }, [refresh, tabKey, statusLabel]);
  useEffect(() => {
    const onFocus = () => void refresh();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refresh]);
  return { threads, targets, defaultTargetId, error, refresh };
}

function targetStatusMeta(item: RunSideChatTarget): { label: string; tone: "running" | "success" | "neutral" | "warning" } {
  if (item.solved) return { label: "已解出", tone: "success" };
  if (item.online) return { label: "在线", tone: "running" };
  const s = item.status.toLowerCase();
  if (s === "failed" || s === "killed" || s === "crashed") return { label: item.status, tone: "warning" };
  return { label: item.status || "已完成", tone: "neutral" };
}

function RunSideTargetPicker({
  runId,
  threadId,
  sideChat,
  targets,
  onRefreshTargets,
  open,
  onOpenChange,
  disabled,
}: {
  runId: string;
  threadId: string;
  sideChat?: ConversationThread["side_chat"];
  targets: RunSideChatTarget[];
  onRefreshTargets: () => Promise<void>;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  disabled?: boolean;
}) {
  const [switchingId, setSwitchingId] = useState("");
  const currentTargetId = sideChat?.target_id || "";
  const matched = targets.find((t) => t.target_id === currentTargetId);
  const currentEngine = matched?.engine || sideChat?.target_engine || "";
  const currentKind = matched?.kind || sideChat?.target_kind || "agent";
  const currentModel = matched?.model ?? sideChat?.target_model ?? "";
  const currentTitle = currentKind === "worker" && (matched?.worker_id || sideChat?.worker_id)
    ? `Worker ${matched?.worker_id || sideChat?.worker_id}`
    : matched?.label || sideChat?.target_label || currentEngine || "选择出战对象";
  const workerTargets = useMemo(() => targets.filter((t) => t.kind === "worker"), [targets]);
  const agentTargets = useMemo(() => targets.filter((t) => t.kind === "agent"), [targets]);

  const chooseTarget = async (item: RunSideChatTarget) => {
    if (switchingId) return;
    if (item.target_id === currentTargetId) {
      onOpenChange(false);
      requestSideChatFocus(threadId);
      return;
    }
    setSwitchingId(item.target_id);
    try {
      await selectRunSideChatTarget(runId, threadId, item.target_id);
      await onRefreshTargets();
      onOpenChange(false);
      requestSideChatFocus(threadId);
    } catch (failure) {
      toast({ title: "切换出战目标失败", description: failureText(failure), tone: "danger" });
    } finally {
      setSwitchingId("");
    }
  };

  const trigger = (
    <button
      type="button"
      disabled={disabled || !targets.length}
      data-testid="run-side-target-picker"
      title={targets.length ? "切换为该 Run 已出战的 Worker 或出战 Agent" : "正在加载可用出战对象"}
      className="inline-flex h-7 min-w-0 max-w-full items-center gap-1.5 rounded-lg border border-cx-border-subtle bg-cx-bg-subtle px-2 text-cx-xs text-cx-fg hover:bg-cx-hover disabled:cursor-not-allowed disabled:opacity-50"
    >
      {currentEngine ? <EngineLogo engine={currentEngine} size={14} className="shrink-0" /> : <Icon name="cpu" size={13} className="shrink-0 text-cx-fg-3" />}
      <span className="max-w-[140px] truncate font-medium">{currentTitle}</span>
      <span className="hidden max-w-[120px] truncate text-cx-fg-3 sm:inline">· {currentModel || "默认模型"}</span>
      <Badge tone={currentKind === "worker" ? "accent" : "neutral"} className="hidden md:inline-flex">
        {currentKind === "worker" ? "完整上下文" : "全局上下文"}
      </Badge>
      <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
    </button>
  );

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        onOpenChange(next);
        if (next) void onRefreshTargets();
      }}
      trigger={trigger}
      placement="top-start"
      offset={8}
      ariaLabel="选择出战 Worker 或 Agent"
      className="flex max-h-[min(460px,70vh)] w-[min(390px,calc(100vw-24px))] flex-col p-1.5"
    >
      <div className="cx-scroll flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto">
        {workerTargets.length > 0 ? (
          <div data-testid="run-side-target-group-workers" className="flex flex-col gap-0.5">
            <div className="px-2 pt-1 pb-0.5 text-cx-2xs font-medium text-cx-fg-3">
              出战 Worker · 继承完整工作上下文
            </div>
            {workerTargets.map((item) => {
              const selected = item.target_id === currentTargetId;
              const status = targetStatusMeta(item);
              const busy = switchingId === item.target_id;
              return (
                <button
                  key={item.target_id}
                  type="button"
                  disabled={Boolean(switchingId)}
                  aria-pressed={selected}
                  data-selected={selected ? "true" : undefined}
                  data-testid="run-side-target-item"
                  data-target-id={item.target_id}
                  onClick={() => void chooseTarget(item)}
                  className={cn(
                    "flex w-full items-start gap-2 rounded-lg px-2 py-2 text-left transition-colors hover:bg-cx-hover disabled:opacity-60",
                    selected && "bg-cx-active",
                  )}
                >
                  <EngineLogo engine={item.engine} size={15} className="mt-0.5 shrink-0" />
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-1.5">
                      <span className="truncate text-cx-xs font-medium text-cx-fg">{item.worker_id || item.label}</span>
                      <Badge tone={status.tone} dot>{status.label}</Badge>
                      {item.native_fork_available ? <Badge tone="accent">原生分叉</Badge> : null}
                    </div>
                    <div className="mt-0.5 truncate text-cx-xs text-cx-fg-3">
                      {item.engine} · {item.model || "默认模型"} · {item.tool_calls} 次调用 · {item.facts_count} 条发现
                    </div>
                    {item.intent_goal ? (
                      <div className="mt-0.5 truncate text-cx-2xs text-cx-fg-4" title={item.intent_goal}>
                        {item.intent_goal}
                      </div>
                    ) : null}
                  </div>
                  {busy ? (
                    <span className="mt-1 shrink-0 text-cx-2xs text-cx-fg-3">切换中…</span>
                  ) : selected ? (
                    <Icon name="check" size={14} className="mt-1 shrink-0 text-cx-accent" />
                  ) : null}
                </button>
              );
            })}
          </div>
        ) : null}
        {agentTargets.length > 0 ? (
          <div data-testid="run-side-target-group-agents" className={cn("flex flex-col gap-0.5", workerTargets.length > 0 && "mt-1 border-t border-cx-border-subtle pt-1")}>
            <div className="px-2 pt-1 pb-0.5 text-cx-2xs font-medium text-cx-fg-3">
              出战 Worker 的 Agent · 全局 Run 上下文
            </div>
            {agentTargets.map((item) => {
              const selected = item.target_id === currentTargetId;
              const busy = switchingId === item.target_id;
              return (
                <button
                  key={item.target_id}
                  type="button"
                  disabled={Boolean(switchingId)}
                  aria-pressed={selected}
                  data-selected={selected ? "true" : undefined}
                  data-testid="run-side-target-item"
                  data-target-id={item.target_id}
                  onClick={() => void chooseTarget(item)}
                  className={cn(
                    "flex w-full items-start gap-2 rounded-lg px-2 py-2 text-left transition-colors hover:bg-cx-hover disabled:opacity-60",
                    selected && "bg-cx-active",
                  )}
                >
                  <EngineLogo engine={item.engine} size={15} className="mt-0.5 shrink-0" />
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-1.5">
                      <span className="truncate text-cx-xs font-medium text-cx-fg">{item.label}</span>
                    </div>
                    <div className="mt-0.5 truncate text-cx-xs text-cx-fg-3">
                      {item.engine} · {item.model || "默认模型"} · {item.facts_count} 条发现
                    </div>
                  </div>
                  {busy ? (
                    <span className="mt-1 shrink-0 text-cx-2xs text-cx-fg-3">切换中…</span>
                  ) : selected ? (
                    <Icon name="check" size={14} className="mt-1 shrink-0 text-cx-accent" />
                  ) : null}
                </button>
              );
            })}
          </div>
        ) : null}
        {!targets.length ? (
          <div className="px-2 py-3 text-center text-cx-xs text-cx-fg-3">
            当前 Run 暂无可选的出战 Worker 或出战 Agent
          </div>
        ) : null}
      </div>
      <div className="mt-1 border-t border-cx-border-subtle px-2 pt-1.5 pb-0.5 text-cx-2xs leading-4 text-cx-fg-4">
        仅可选本 Run 的出战 Worker 与出战 Roster Agent（不含未出战全局模型及评审/验证席位）。
      </div>
    </Popover>
  );
}

function relativeTime(value?: string) {
  if (!value) return "";
  const at = Date.parse(value);
  if (!Number.isFinite(at)) return "";
  const minutes = Math.round((Date.now() - at) / 60000);
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  return new Date(at).toLocaleDateString();
}

export function RunSideChatPanel({ run, layout, onNewChat, controls, sheetPanel, testId }: {
  run: RunSideChatRun;
  layout: RunSideChatLayout;
  onNewChat: (targetId?: string) => Promise<string>;
  controls?: ReactNode;
  sheetPanel: boolean;
  testId?: string;
}) {
  const navigation = useIsolatedNavigation();
  const { threads, targets, error, refresh } = useRunSideThreads(run.runId, layout.tabs, run.status.label);
  const [menuOpen, setMenuOpen] = useState(false);
  const [pendingDelete, setPendingDelete] = useState("");
  const [creating, setCreating] = useState(false);
  const titles = useMemo(() => new Map(threads.map((thread) => [thread.thread_id, thread.title])), [threads]);
  const active = layout.active;
  const activeThread = useMemo(() => threads.find((thread) => thread.thread_id === active), [threads, active]);
  const experience = useMemo<ConversationExperience>(() => ({
    ...runExperience(run.mode, sheetPanel),
    composerControls: active ? ({ open, onOpenChange, disabled }) => (
      <RunSideTargetPicker
        runId={run.runId}
        threadId={active}
        sideChat={activeThread?.side_chat}
        targets={targets}
        onRefreshTargets={refresh}
        open={open}
        onOpenChange={onOpenChange}
        disabled={disabled}
      />
    ) : undefined,
  }), [run.mode, run.runId, sheetPanel, active, activeThread?.side_chat, targets, refresh]);
  const tablistRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    tablistRef.current?.querySelector<HTMLElement>("[role=tab][aria-selected=true]")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [active, layout.tabs.length]);

  const create = async (targetId?: string) => {
    setCreating(true);
    try { await onNewChat(targetId); } finally { setCreating(false); }
  };
  const remove = async (threadId: string) => {
    try {
      await deleteRunSideChat(run.runId, threadId);
      toast({ title: "已删除侧边聊天", tone: "success", duration: 1800 });
    } catch (failure) {
      toast({ title: "侧边聊天未能删除", description: failureText(failure), tone: "danger" });
    } finally {
      setPendingDelete("");
      void refresh();
    }
  };

  return (
    <div className="run-side-panel flex min-h-0 min-w-0 flex-1 flex-col bg-cx-bg" data-testid={testId}>
      <div className="flex h-10 shrink-0 items-center gap-1 border-b border-cx-border px-1.5" data-testid="run-side-tabs">
        <div ref={tablistRef} role="tablist" aria-label="侧边聊天标签" className="cx-scroll flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto">
          {layout.tabs.map((threadId) => {
            const selected = threadId === active;
            const title = titles.get(threadId) || "侧边聊天";
            return (
              <div key={threadId} className={cn("group flex h-7 min-w-[76px] max-w-[180px] shrink items-center rounded-md pl-2 pr-0.5 text-cx-xs",
                selected ? "bg-cx-active text-cx-fg" : "text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg-2")}>
                <button type="button" role="tab" aria-selected={selected} title={title} data-testid="run-side-tab"
                  className="min-w-0 flex-1 truncate text-left outline-none focus-visible:underline"
                  onClick={() => showRunSideChat(run.runId, threadId)}>{title}</button>
                <button type="button" aria-label={`关闭“${title}”`} title="关闭标签（保留对话，可从 + 重新打开）" data-testid="run-side-tab-close"
                  className={cn("ml-0.5 flex size-5 shrink-0 items-center justify-center rounded text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg", !selected && "opacity-0 group-hover:opacity-100 focus-visible:opacity-100")}
                  onClick={() => closeRunSideChatTab(run.runId, threadId)}><Icon name="x" size={12} /></button>
              </div>
            );
          })}
        </div>
        <Menu
          open={menuOpen}
          onOpenChange={(next) => { setMenuOpen(next); setPendingDelete(""); if (next) void refresh(); }}
          placement="bottom-end"
          ariaLabel="侧边聊天"
          className="w-[320px]"
          trigger={<IconButton icon="plus" label="新建或重新打开侧边聊天" data-testid="run-side-menu" />}
        >
          <MenuItem icon="plus" onSelect={() => void create()} disabled={creating}>新建侧边聊天</MenuItem>
          {targets.length > 0 ? (
            <>
              <MenuSeparator />
              <MenuLabel>按出战对象新建</MenuLabel>
              {targets.map((target) => (
                <MenuItem
                  key={target.target_id}
                  icon={target.kind === "worker" ? "cpu" : "sparkles"}
                  hint={target.model || "默认模型"}
                  onSelect={() => void create(target.target_id)}
                  disabled={creating}
                >
                  <span className="truncate">
                    {target.kind === "worker" && target.worker_id
                      ? `Worker ${target.worker_id}`
                      : `Agent · ${target.label}`}
                  </span>
                </MenuItem>
              ))}
            </>
          ) : null}
          <MenuSeparator />
          <MenuLabel>这个 Run 的侧边聊天</MenuLabel>
          {error ? <div className="px-2 py-1.5 text-cx-xs text-cx-danger">{error}</div> : null}
          {!error && !threads.length ? <div className="px-2 py-1.5 text-cx-xs text-cx-fg-3">还没有侧边聊天</div> : null}
          {threads.map((thread) => {
            const opened = layout.tabs.includes(thread.thread_id);
            const confirming = pendingDelete === thread.thread_id;
            const targetTag = thread.side_chat?.target_kind === "worker" && thread.side_chat?.worker_id
              ? `Worker ${thread.side_chat.worker_id}${thread.side_chat.target_model ? ` · ${thread.side_chat.target_model}` : ""}`
              : thread.side_chat?.target_label
                ? `${thread.side_chat.target_label}${thread.side_chat.target_model ? ` · ${thread.side_chat.target_model}` : ""}`
                : undefined;
            return (
              <div key={thread.thread_id} className="flex items-center gap-1">
                <MenuItem className="min-w-0 flex-1" icon={opened ? "checkCircle" : "messages"} onSelect={() => showRunSideChat(run.runId, thread.thread_id)}
                  description={targetTag}
                  hint={opened ? "已打开" : relativeTime(thread.updated_at)}>
                  <span className="truncate" data-testid="run-side-history-item">{thread.title || "侧边聊天"}</span>
                </MenuItem>
                <button type="button" data-testid="run-side-history-delete"
                  aria-label={confirming ? `确认删除“${thread.title}”` : `删除“${thread.title}”`}
                  title={confirming ? "再次点击确认删除" : "删除"}
                  className={cn("flex h-7 shrink-0 items-center justify-center rounded-md px-1.5 text-cx-xs", confirming ? "bg-cx-danger-soft text-cx-danger" : "text-cx-fg-4 hover:bg-cx-hover hover:text-cx-danger")}
                  onClick={(event) => {
                    event.stopPropagation();
                    if (confirming) void remove(thread.thread_id);
                    else setPendingDelete(thread.thread_id);
                  }}>
                  {confirming ? "确认删除" : <Icon name="trash" size={13} />}
                </button>
              </div>
            );
          })}
        </Menu>
        {controls}
      </div>
      <div className="relative flex min-h-0 flex-1">
        {active ? (
          <Suspense fallback={<div className="m-auto text-cx-sm text-cx-fg-3" role="status">正在打开对话…</div>}>
            <ConversationShell key={active} threadId={active} navigation={navigation} experience={experience}
              mainContentId={`run-side-main-${active}`} sidebarId={`run-side-sidebar-${active}`}
              sideOf={{ threadId: run.runId, title: run.title, overlay: false, onClose: () => closeRunSideChatTab(run.runId, active),
                sideChat: { parentStatus: run.status, runOwned: true, runTargets: targets } }} />
          </Suspense>
        ) : (
          <div className="flex flex-1 flex-col items-center justify-center gap-3 px-6 text-center">
            <Icon name="messages" size={22} className="text-cx-fg-3" />
            <p className="text-cx-sm leading-5 text-cx-fg-3">没有打开的侧边聊天。新建一个，或从 + 菜单重新打开之前的对话。</p>
            <Button size="md" onClick={() => void create()} disabled={creating} data-testid="run-side-empty-new">新建侧边聊天</Button>
          </div>
        )}
      </div>
    </div>
  );
}

export function RunSideChatColumn({ controller, run }: { controller: RunSideChatController; run: RunSideChatRun }) {
  const frame = useContext(RunSideChatFrameContext);
  const { layout, popout } = controller;
  const max = Math.max(RUN_SIDE_WIDTH_MIN, frame.maxWidth);
  const width = Math.min(layout.width, max);
  const collapse = () => updateRunSideChatLayout(run.runId, { open: false, maximized: false });
  const failure = (title: string) => (error: unknown) => toast({ title, description: failureText(error), tone: "danger" });

  if (popout.popped) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 px-6 text-center" data-testid="run-side-popped">
        <Icon name="externalLink" size={22} className="text-cx-fg-3" />
        <p className="text-cx-sm leading-5 text-cx-fg-2">侧边聊天已在独立窗口打开</p>
        <div className="flex gap-2">
          <Button size="md" onClick={popout.reclaim} data-testid="run-side-reclaim">收回到这里</Button>
          <Button size="md" variant="ghost" onClick={collapse}>收起</Button>
        </div>
      </div>
    );
  }

  const controls = (
    <div className="flex shrink-0 items-center gap-0.5 border-l border-cx-border-subtle pl-1">
      {!frame.narrow ? <>
        <IconButton icon="columns" label={layout.side === "right" ? "移到主对话左侧" : "移到主对话右侧"} data-testid="run-side-swap"
          onClick={() => updateRunSideChatLayout(run.runId, { side: layout.side === "right" ? "left" : "right" })} />
        <IconButton icon={layout.maximized ? "minimize" : "maximize"} label={layout.maximized ? "还原并排" : "最大化侧边聊天"} data-testid="run-side-maximize"
          onClick={() => updateRunSideChatLayout(run.runId, { maximized: !layout.maximized })} />
      </> : null}
      <IconButton icon="externalLink" label="在独立窗口打开" data-testid="run-side-popout" disabled={!layout.active}
        onClick={() => { if (layout.active) void popout.popOut(layout.active).catch(failure("无法打开独立窗口")); }} />
      <IconButton icon={layout.side === "right" ? "panelRightClose" : "panelLeftClose"} label="收起侧边聊天（⌘/Ctrl+Shift+/）" data-testid="run-side-collapse" onClick={collapse} />
    </div>
  );

  return (
    <>
      {!frame.narrow && !layout.maximized ? (
        <ResizeHandle side={layout.side === "right" ? "left" : "right"} value={width} min={RUN_SIDE_WIDTH_MIN} max={max}
          onChange={(next) => updateRunSideChatLayout(run.runId, { width: next })}
          onReset={() => updateRunSideChatLayout(run.runId, { width: RUN_SIDE_WIDTH_DEFAULT })}
          label="拖动调整侧边聊天宽度，双击还原" />
      ) : null}
      <RunSideChatPanel run={run} layout={layout} onNewChat={(targetId) => controller.newChat("", targetId)} controls={controls} sheetPanel testId="run-side-column" />
    </>
  );
}

export function RunSelectionAsk({ containerRef, onAsk }: { containerRef: RefObject<HTMLElement | null>; onAsk: (text: string) => void }) {
  const [anchor, setAnchor] = useState<{ x: number; y: number; text: string } | null>(null);
  useEffect(() => {
    const root = containerRef.current;
    if (!root) return;
    const read = () => {
      const selection = window.getSelection();
      const text = selection?.toString().trim() || "";
      if (!selection || selection.isCollapsed || !text || !selection.rangeCount) { setAnchor(null); return; }
      const range = selection.getRangeAt(0);
      if (!root.contains(range.commonAncestorContainer)) { setAnchor(null); return; }
      if ((range.commonAncestorContainer instanceof Element ? range.commonAncestorContainer : range.commonAncestorContainer.parentElement)?.closest("input, textarea, [contenteditable='true']")) { setAnchor(null); return; }
      const rect = range.getBoundingClientRect();
      setAnchor({ x: Math.min(window.innerWidth - 180, Math.max(8, rect.left + rect.width / 2 - 80)), y: Math.max(8, rect.top - 40), text });
    };
    const onUp = () => requestAnimationFrame(read);
    const clear = () => setAnchor(null);
    root.addEventListener("mouseup", onUp);
    root.addEventListener("keyup", onUp);
    root.addEventListener("scroll", clear, true);
    return () => { root.removeEventListener("mouseup", onUp); root.removeEventListener("keyup", onUp); root.removeEventListener("scroll", clear, true); };
  }, [containerRef]);
  if (!anchor) return null;
  return (
    <button type="button" data-testid="run-selection-ask"
      className="fixed z-(--z-popover) inline-flex h-8 items-center gap-1.5 rounded-lg ring-1 ring-cx-ring bg-cx-elevated px-2.5 text-cx-xs text-cx-fg shadow-cx-md hover:bg-cx-hover"
      style={{ left: anchor.x, top: anchor.y }}
      onMouseDown={(event) => event.preventDefault()}
      onClick={() => { const text = anchor.text; setAnchor(null); window.getSelection()?.removeAllRanges(); onAsk(text); }}>
      <Icon name="messages" size={13} />在侧边聊天中提问
    </button>
  );
}


export function RunSideChatWindow({ runId, threadId, detached }: { runId: string; threadId: string; detached: boolean }) {
  const live = useRun(runId);
  const deck = live.deck;
  const running = isRunActive(deck);
  const mode = asRunWorkspaceMode(deck.mode) ?? "ctf";
  const layout = useRunSideChatLayout(runId);
  const [reclaimed, setReclaimed] = useState(false);
  useEffect(() => { if (runId && threadId) showRunSideChat(runId, threadId); }, [runId, threadId]);
  const onReclaimed = useCallback(() => {
    void closeRunSideChatWindow().then((closed) => { if (!closed) setReclaimed(true); }, () => setReclaimed(true));
  }, []);
  const reclaim = useRunSideChatWindow(runId, detached && !reclaimed, onReclaimed);
  const title = deck.challengeName || "Run";
  const run: RunSideChatRun = { runId, mode, title, status: runSideChatStatus(deck, running) };
  const runHref = conversationHref(runId, mode);
  useEffect(() => { document.title = `侧边聊天 · ${title}`; }, [title]);

  return (
    <div className="run-side-window flex h-full min-h-0 w-full flex-col bg-cx-bg" data-detached={detached ? "true" : undefined} data-testid="run-side-window">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-cx-border px-3">
        <Icon name="messages" size={15} className="shrink-0 text-cx-fg-3" />
        <span className="min-w-0 truncate text-cx-sm font-medium text-cx-fg" title={title}>侧边聊天 · {title}</span>
        <span className={cn("shrink-0 text-cx-xs", run.status.tone === "running" ? "text-cx-accent" : run.status.tone === "attention" ? "text-cx-warning" : "text-cx-fg-3")}>{run.status.label}</span>
        <span className="flex-1" />
        {detached && !reclaimed ? (
          <Button size="sm" variant="ghost" icon="cornerDownLeft" onClick={reclaim} data-testid="run-side-window-reclaim">收回到 Run 页面</Button>
        ) : (
          <a href={runHref} className="inline-flex h-7 items-center gap-1 rounded-md px-2 text-cx-xs text-cx-fg-2 hover:bg-cx-hover">
            <Icon name="arrowLeft" size={13} />返回 Run
          </a>
        )}
      </header>
      {reclaimed ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center text-cx-sm text-cx-fg-3" data-testid="run-side-window-reclaimed">
          <p>侧边聊天已收回到 Run 页面，可以关闭这个窗口。</p>
        </div>
      ) : (
        <RunSideChatPanel run={run} layout={layout} onNewChat={(targetId) => startRunSideChat(runId, "", undefined, targetId)} sheetPanel={false} testId="run-side-window-panel" />
      )}
    </div>
  );
}
