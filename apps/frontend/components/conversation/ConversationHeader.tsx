"use client";

import React, { useEffect, useId, useRef, useState } from "react";
import { fetchConversationView, type ConversationView } from "@/lib/useConversation";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { useSharedGitStatus } from "@/lib/threadGitStatusStore";
import { useThreadPullRequests } from "@/lib/threadGitActions";
import { chatPanel } from "@/lib/chatPanelStore";
import { useThreadEditor } from "./threadEditorContext";
import {
  resolveWorkspaceBranchChip,
  type LiveGitBranchStatus,
} from "@/lib/workspaceBranchDisplay";
import { copyToClipboard } from "@/lib/clipboard";
import { formatWakeTime, snoozePresets } from "@/lib/sidebarInbox";
import { sendSidebarCommand, useSidebarThreadState } from "@/lib/sidebarThreadBridge";
import { Icon, type IconName } from "@muteki/ui";
import { SnoozeDialog } from "@/components/chat/sidebar/SnoozeDialog";
import { ThreadGitActions } from "./ThreadGitActions";
import { ConversationControlShield } from "./ConversationControlShield";
import { ConversationRouteLink } from "./ConversationNavigation";
import {
  Badge,
  Button,
  IconButton,
  Menu,
  MenuItem,
  MenuSeparator,
  MenuSub,
  Spinner,
  Tooltip,
  toast,
  type Tone,
} from "@muteki/ui";

export interface ConversationHeaderProps {
  view: ConversationView | null;
  projectName?: string;
  panelControls?: React.ReactNode;
  outputControl?: React.ReactNode;
  bottomPanelOpen?: boolean;
  rightPanelOpen?: boolean;
  outputCardOpen?: boolean;
  onToggleBottomPanel?: () => void;
  onToggleRightPanel?: () => void;
  onToggleOutput?: () => void;
  onOpenInfo: () => void;
  onOpenReadingPrefs?: () => void;
  readingPrefsOpen?: boolean;
  onOpenShortcuts?: () => void;
  sidebarToggle?: React.ReactNode;
  sidebarId?: string;
  threadHref?: (threadId: string) => string;
  onOpenPendingSend?: () => void;
  onNewInProject?: () => void;
  onFork?: () => void;
  onRename?: (title: string) => void;
  onArchive?: () => void;
  onExport?: () => void;
  busy?: boolean;
  className?: string;
}

type HeaderStatus = { label: string; tone: Tone; running: boolean };

function headerStatus(view: ConversationView): HeaderStatus {
  if (view.state.running_turn_id) return { label: "执行中", tone: "running", running: true };
  if (view.state.last_error && Object.keys(view.state.last_error).length > 0) {
    return { label: "失败", tone: "danger", running: false };
  }
  const lastStatus = String(view.turns.at(-1)?.status || "").toLowerCase();
  if (lastStatus === "failed") return { label: "失败", tone: "danger", running: false };
  if (["cancelled", "canceled"].includes(lastStatus)) {
    return { label: "已取消", tone: "neutral", running: false };
  }
  if (["interrupted", "aborted"].includes(lastStatus)) {
    return { label: "已中断", tone: "neutral", running: false };
  }
  if (lastStatus === "completed") return { label: "已完成", tone: "success", running: false };
  return { label: "就绪", tone: "neutral", running: false };
}

function workspaceChip(
  view: ConversationView,
  git: {
    status: LiveGitBranchStatus | null;
    error: string;
    loading: boolean;
  },
): { icon: IconName; label: string; title: string } | null {
  const workspace = view.workspace;
  if (!workspace) return null;
  const settings = workspace.settings || {};
  const settingsBranch = typeof settings.branch === "string" ? settings.branch.trim() : "";
  const mode = String(settings.mode || "");
  const root = workspace.root_path || "";
  const chip = resolveWorkspaceBranchChip({
    git: git.status,
    gitError: git.error || null,
    gitLoading: git.loading,
    settingsBranch,
    mode,
    rootPath: root,
    kind: workspace.kind,
  });
  if (!chip) return null;
  return { icon: chip.icon, label: chip.label, title: chip.title };
}

function Chip({ children, title }: { children: React.ReactNode; title?: string }) {
  return (
    <span title={title} className="inline-flex h-6 max-w-[200px] flex-none items-center text-cx-sm leading-none text-cx-fg-3">
      <span className="truncate">{children}</span>
    </span>
  );
}

function TitleEditor({
  initial,
  onCommit,
  onCancel,
}: {
  initial: string;
  onCommit: (title: string, refocus: boolean) => void;
  onCancel: (refocus: boolean) => void;
}) {
  const [value, setValue] = useState(initial);
  const settledRef = useRef(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const titleInputId = useId();

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  const commit = (refocus: boolean) => {
    if (settledRef.current) return;
    settledRef.current = true;
    const next = value.trim();
    if (next && next !== initial) onCommit(next, refocus);
    else onCancel(refocus);
  };

  return (
    <form
      noValidate
      className="flex min-w-0 flex-1 items-center"
      onSubmit={(event) => {
        event.preventDefault();
        commit(true);
      }}
    >
      <label className="sr-only" htmlFor={titleInputId}>对话标题</label>
      <input
        id={titleInputId}
        ref={inputRef}
        value={value}
        maxLength={200}
        onChange={(event) => setValue(event.target.value)}
        onBlur={() => commit(false)}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            settledRef.current = true;
            onCancel(true);
          }
        }}
        className={cn(
          "h-8 w-full min-w-0 max-w-[520px] rounded-xl border border-cx-border-strong bg-cx-elevated px-2.5",
          "text-cx-base font-semibold text-cx-fg outline-none",
        )}
      />
    </form>
  );
}

function copyText(value: string, label: string) {
  void copyToClipboard(value).then((ok) => {
    toast({ title: ok ? `已复制${label}` : "复制失败，请手动复制", tone: ok ? "success" : "danger", duration: 1800 });
  });
}

const PR_STATE_LABEL: Record<string, string> = {
  open: "打开",
  draft: "草稿",
  merged: "已合并",
  closed: "已关闭",
};

function useForkSource(sourceThreadId: string): { id: string; title: string } | null {
  const [source, setSource] = useState<{ id: string; title: string } | null>(null);
  useEffect(() => {
    setSource(sourceThreadId ? { id: sourceThreadId, title: "" } : null);
    if (!sourceThreadId) return;
    let cancelled = false;
    fetchConversationView(sourceThreadId, false, 1)
      .then((view) => { if (!cancelled) setSource({ id: sourceThreadId, title: view.thread.title || "" }); })
      .catch(() => { if (!cancelled) setSource({ id: sourceThreadId, title: "" }); });
    return () => { cancelled = true; };
  }, [sourceThreadId]);
  return source;
}

export function ConversationHeader({
  view,
  projectName,
  panelControls,
  outputControl,
  bottomPanelOpen = false,
  rightPanelOpen = false,
  outputCardOpen = false,
  onToggleBottomPanel,
  onToggleRightPanel,
  onToggleOutput,
  onOpenInfo,
  onOpenReadingPrefs,
  readingPrefsOpen = false,
  onOpenShortcuts,
  sidebarToggle,
  sidebarId,
  threadHref,
  onOpenPendingSend,
  onNewInProject,
  onFork,
  onRename,
  onArchive,
  onExport,
  busy = false,
  className = "",
}: ConversationHeaderProps) {
  const t = useT();
  const [editing, setEditing] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const [snoozeOpen, setSnoozeOpen] = useState(false);
  const [controlOpen, setControlOpen] = useState(false);
  const titleButtonRef = useRef<HTMLButtonElement>(null);
  const threadId = view?.thread.thread_id;
  const projectId = view?.thread.project_id || view?.workspace?.project_id || undefined;
  const git = useSharedGitStatus(threadId, projectId || undefined);
  const sidebarState = useSidebarThreadState(threadId);
  const forkSource = useForkSource(
    typeof view?.thread.source_thread_id === "string" ? view.thread.source_thread_id : "",
  );
  const threadEditor = useThreadEditor();
  const pullRequests = useThreadPullRequests(threadId, false);

  useEffect(() => {
    setEditing(false);
    setMenuOpen(false);
    setControlOpen(false);
  }, [threadId]);

  if (!view || !threadId) return null;

  const title = view.thread.title || "未命名对话";
  const isArchived = view.state.status === "archived";
  const status = headerStatus(view);
  const workspace = workspaceChip(view, {
    status: git.status,
    error: git.error,
    loading: git.loading,
  });
  const projectLabel = projectName?.trim() || (view.thread.project_id ? "未知项目" : "");
  const parentId = (view.lineage ?? view.state.lineage)?.parent_thread_id || "";
  const parentTitle = view.lineage?.parent_title || "父对话";
  const inbox = parentId ? undefined : sidebarState;
  const rootPath = view.workspace?.root_path || "";
  const backgroundCount = view.state.background_tasks || 0;
  const prList = pullRequests.result?.state === "ok" ? pullRequests.result.pull_requests : [];
  const prPrimary = prList.find((pr) => pr.state === "open" || pr.state === "draft") || prList[0];
  const headerPillClass = "cx-press hidden h-6 max-w-[220px] flex-none items-center gap-1 rounded-full border border-cx-border px-2 text-cx-xs leading-none text-cx-fg-3 no-underline outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)] @[600px]/header:inline-flex";
  const legacyDock = panelControls === undefined;
  const outputLabel = outputCardOpen ? "关闭任务输出" : "打开任务输出";
  const bottomLabel = bottomPanelOpen ? "关闭 Agent 执行日志" : "打开 Agent 执行日志";
  const rightLabel = rightPanelOpen ? "隐藏右侧面板" : "显示右侧面板";
  const command = (next: Parameters<typeof sendSidebarCommand>[1]) => {
    if (!sendSidebarCommand(threadId, next, sidebarId)) toast({ title: "侧栏未加载，暂时无法更改列表位置", tone: "warning" });
  };

  const startEditing = () => {
    if (onRename) setEditing(true);
  };
  const stopEditing = (refocus: boolean) => {
    setEditing(false);
    if (refocus) window.requestAnimationFrame(() => titleButtonRef.current?.focus({ preventScroll: true }));
  };

  return (
    <header
      className={cn(
        "cx-conv-header @container/header relative z-10 flex h-[var(--cx-header-h,52px)] flex-none items-center gap-3 border-b border-cx-border-subtle bg-cx-bg px-3",
        className,
      )}
    >
      {sidebarToggle ? <div className="-ml-1 flex flex-none items-center">{sidebarToggle}</div> : null}
      <div className="flex min-w-0 flex-1 items-center gap-1">
        {!editing && parentId ? (
          <nav aria-label="对话位置" className="flex min-w-0 flex-none items-center gap-1">
            <Tooltip content={`返回父对话「${parentTitle}」`} placement="bottom">
              <ConversationRouteLink
                href={`/chat/${encodeURIComponent(parentId)}`}
                aria-label={`返回父对话：${parentTitle}`}
                data-testid="conversation-parent-link"
                className="cx-press flex h-7 max-w-[200px] items-center gap-1.5 rounded-xl px-1.5 text-cx-sm text-cx-fg-3 no-underline outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
              >
                <Icon name="arrowLeft" size={13} className="shrink-0" />
                <span className="hidden truncate @[520px]/header:inline">{parentTitle}</span>
              </ConversationRouteLink>
            </Tooltip>
            <span aria-hidden="true" className="text-cx-sm text-cx-fg-4">/</span>
          </nav>
        ) : !editing && projectLabel ? (
          <nav aria-label="对话位置" className="hidden min-w-0 flex-none items-center gap-1 @[520px]/header:flex">
            <Tooltip content={projectId ? `侧栏只看「${projectLabel}」，再次点击显示全部` : projectLabel} placement="bottom">
              <button
                type="button"
                disabled={!projectId}
                onClick={() => { if (projectId) command({ kind: "filter-project", projectId }); }}
                className="cx-press flex h-7 max-w-[180px] items-center gap-1.5 rounded-xl px-1.5 text-cx-sm text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)] disabled:hover:bg-transparent"
              >
                <Icon name="folder" size={13} className="shrink-0 text-cx-fg-4" />
                <span className="truncate">{projectLabel}</span>
              </button>
            </Tooltip>
            <span aria-hidden="true" className="text-cx-sm text-cx-fg-4">/</span>
          </nav>
        ) : null}
        {editing ? (
          <TitleEditor
            initial={view.thread.title || ""}
            onCommit={(next, refocus) => {
              onRename?.(next);
              stopEditing(refocus);
            }}
            onCancel={stopEditing}
          />
        ) : (
          <h1 className="flex min-w-0 items-center text-cx-base font-semibold leading-5 text-cx-fg">
            <Menu
              open={menuOpen}
              onOpenChange={setMenuOpen}
              placement="bottom-start"
              ariaLabel="对话操作"
              className="min-w-[230px]"
              trigger={(
                <button
                  ref={titleButtonRef}
                  type="button"
                  title={onRename ? `${title} · 双击重命名` : title}
                  aria-label={`${title}，对话操作`}
                  data-testid="conversation-title-menu-trigger"
                  onDoubleClick={(event) => {
                    event.preventDefault();
                    setMenuOpen(false);
                    startEditing();
                  }}
                  className={cn(
                    "cx-press flex h-8 min-w-0 max-w-full items-center gap-1 rounded-xl px-2 text-left outline-none",
                    "hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)] data-[state=open]:bg-cx-hover",
                  )}
                >
                  {inbox?.pinned ? <Icon name="pin" size={12} className="shrink-0 text-cx-fg-4" /> : null}
                  <span className="min-w-0 truncate">{title}</span>
                  <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
                </button>
              )}
            >
              {onNewInProject ? (
                <MenuItem icon="newChat" onSelect={onNewInProject}>
                  {projectLabel ? `在「${projectLabel}」中新建对话` : "新建对话"}
                </MenuItem>
              ) : null}
              {inbox ? (
                <>
                  <MenuItem icon={inbox.pinned ? "pinOff" : "pin"} shortcut="mod+shift+p" onSelect={() => command({ kind: inbox.pinned ? "unpin" : "pin" })}>
                    {inbox.pinned ? "取消置顶" : "置顶对话"}
                  </MenuItem>
                  <MenuItem icon="checkCheck" shortcut="mod+shift+s" onSelect={() => command({ kind: inbox.placement === "settled" ? "unsettle" : "settle" })}>
                    {inbox.placement === "settled" ? "移回列表" : "归置对话"}
                  </MenuItem>
                  {inbox.placement === "snoozed" ? (
                    <MenuItem icon="bell" onSelect={() => command({ kind: "wake" })}>立即唤醒</MenuItem>
                  ) : null}
                  <MenuSub icon="clock" label={inbox.placement === "snoozed" ? "更改提醒时间" : "稍后提醒"}>
                    {snoozePresets().map((preset) => (
                      <MenuItem key={preset.id} hint={formatWakeTime(preset.until)} onSelect={() => command({ kind: "snooze", until: preset.until })}>
                        {preset.label}
                      </MenuItem>
                    ))}
                    <MenuSeparator />
                    <MenuItem icon="pencilLine" onSelect={() => setSnoozeOpen(true)}>自定义时间…</MenuItem>
                  </MenuSub>
                </>
              ) : null}
              {onNewInProject || inbox ? <MenuSeparator /> : null}
              {onRename ? <MenuItem icon="pencil" hint="双击标题" onSelect={startEditing}>重命名</MenuItem> : null}
              {onFork ? <MenuItem icon="gitFork" disabled={busy} onSelect={onFork}>分叉对话</MenuItem> : null}
              {onExport ? <MenuItem icon="download" onSelect={onExport}>导出对话</MenuItem> : null}
              {threadEditor ? (
                <MenuItem icon="code" disabled={threadEditor.busy} onSelect={() => void threadEditor.open()}>
                  在编辑器中打开
                </MenuItem>
              ) : null}
              <MenuSub icon="copy" label="复制">
                <MenuItem icon="link" onSelect={() => copyText(new URL(threadHref ? threadHref(threadId) : `/chat/${encodeURIComponent(threadId)}`, window.location.origin).toString(), "对话链接")}>对话链接</MenuItem>
                <MenuItem icon="hash" onSelect={() => copyText(threadId, "对话 ID")}>对话 ID</MenuItem>
                <MenuItem icon="quote" onSelect={() => copyText(title, "对话标题")}>对话标题</MenuItem>
                {rootPath ? <MenuItem icon="folder" onSelect={() => copyText(rootPath, "工作区路径")}>工作区路径</MenuItem> : null}
              </MenuSub>
              <MenuSeparator />
              <MenuItem icon="info" onSelect={onOpenInfo}>对话信息与记忆</MenuItem>
              <MenuItem icon="shield" onSelect={() => setControlOpen(true)}>协同与生命周期…</MenuItem>
              {onOpenReadingPrefs ? (
                <MenuItem icon="eye" checked={readingPrefsOpen} onSelect={onOpenReadingPrefs}>
                  <span data-testid="menu-open-reading-prefs">{t("readingPrefs.open")}</span>
                </MenuItem>
              ) : null}
              {onOpenShortcuts ? <MenuItem icon="keyboard" shortcut="?" onSelect={onOpenShortcuts}>键盘快捷键</MenuItem> : null}
              {onOpenPendingSend ? <MenuItem icon="history" onSelect={onOpenPendingSend}>待核对发送记录</MenuItem> : null}
              {onToggleBottomPanel || (legacyDock && (onToggleOutput || onToggleRightPanel)) ? (
                <>
                  <MenuSeparator />
                  {legacyDock && onToggleOutput ? (
                    <MenuItem icon="board" checked={outputCardOpen} onSelect={onToggleOutput}>
                      <span data-testid="menu-open-task-output">{outputLabel}</span>
                    </MenuItem>
                  ) : null}
                  {onToggleBottomPanel ? (
                    <MenuItem icon="terminal" shortcut="mod+j" checked={bottomPanelOpen} onSelect={onToggleBottomPanel}>
                      <span data-testid="menu-open-agent-log">{bottomLabel}</span>
                    </MenuItem>
                  ) : null}
                  {legacyDock && onToggleRightPanel ? (
                    <MenuItem icon="panel" checked={rightPanelOpen} onSelect={onToggleRightPanel}>
                      <span data-testid="menu-open-workspace-panel">{rightLabel}</span>
                    </MenuItem>
                  ) : null}
                </>
              ) : null}
              {onArchive ? (
                <>
                  <MenuSeparator />
                  <MenuItem icon="archive" danger={!isArchived} disabled={busy} onSelect={onArchive}>
                    {isArchived ? "取消归档" : "归档对话"}
                  </MenuItem>
                </>
              ) : null}
            </Menu>
          </h1>
        )}

        {!editing && workspace ? (
          <div className="hidden min-w-0 flex-none items-center gap-1 text-cx-fg-4 @[760px]/header:flex">
            <Icon name={workspace.icon} size={12} className="shrink-0" />
            <Chip title={workspace.title}>{workspace.label}</Chip>
          </div>
        ) : null}

        {!editing && parentId ? (
          <span className="ml-1 hidden flex-none @[420px]/header:inline-flex" title="由父对话委派的子代理会话">
            <Badge tone="neutral" icon="bot">子代理</Badge>
          </span>
        ) : null}

        {!editing && forkSource ? (
          <Tooltip content={forkSource.title ? `打开来源对话「${forkSource.title}」` : "打开来源对话"} placement="bottom">
            <ConversationRouteLink
              href={threadHref ? threadHref(forkSource.id) : `/chat/${encodeURIComponent(forkSource.id)}`}
              aria-label={`派生自《${forkSource.title || "原对话"}》，点击打开`}
              data-testid="conversation-fork-source"
              className={cn("ml-1", headerPillClass)}
            >
              <Icon name="gitFork" size={11} className="shrink-0" />
              <span className="truncate">派生自《{forkSource.title || "原对话"}》</span>
            </ConversationRouteLink>
          </Tooltip>
        ) : null}

        {!editing && backgroundCount > 0 ? (
          <Tooltip content="仍有后台任务在运行，点击查看子代理与后台工作" placement="bottom">
            <button
              type="button"
              data-testid="header-background-pill"
              onClick={() => chatPanel.open(threadId, "agents")}
              className={cn("ml-1", headerPillClass)}
            >
              <Icon name="layers" size={11} className="shrink-0" />
              <span className="cx-tabular">后台 {backgroundCount}</span>
            </button>
          </Tooltip>
        ) : null}

        {!editing && prPrimary ? (
          <Tooltip
            content={prList.map((pr) => `#${pr.number} ${pr.title}（${PR_STATE_LABEL[pr.state] || "等待状态"}）`).join("\n")}
            placement="bottom"
          >
            <button
              type="button"
              data-testid="header-pr-pill"
              onClick={() => chatPanel.open(threadId, "pull-request")}
              className={cn("ml-1", headerPillClass)}
            >
              <Icon name="gitPullRequest" size={11} className="shrink-0" />
              <span className="cx-tabular">#{prPrimary.number}</span>
              <span>{PR_STATE_LABEL[prPrimary.state] || "等待状态"}</span>
              {prList.length > 1 ? <span className="cx-tabular text-cx-fg-4">+{prList.length - 1}</span> : null}
            </button>
          </Tooltip>
        ) : null}

        {!editing && (isArchived || status.running || status.tone === "danger" || ["已中断", "已取消"].includes(status.label)) ? (
          <span className="ml-1 flex flex-none items-center gap-1" role="status" aria-live="polite" title="轮次执行状态；对话可继续接收新消息">
            {isArchived ? <Badge tone="warning">已归档</Badge> : null}
            {status.running || status.tone === "danger" || ["已中断", "已取消"].includes(status.label) ? (
              <Badge tone={status.tone}>
                {status.running ? <Spinner size={11} /> : null}
                {status.running ? "执行中" : status.label}
              </Badge>
            ) : null}
          </span>
        ) : null}
      </div>

      <div className="flex flex-none items-center gap-0.5" aria-label="对话工作区显示设置">
        {legacyDock ? (
          <>
            {onOpenReadingPrefs ? (
              <Button
                size="sm"
                variant="ghost"
                active={readingPrefsOpen}
                data-testid="c39-reading-prefs-toggle"
                aria-label={t("readingPrefs.aria")}
                tooltip={t("readingPrefs.aria")}
                onClick={onOpenReadingPrefs}
                className="hidden md:inline-flex"
              >
                {t("readingPrefs.short")}
              </Button>
            ) : null}
            {outputControl}
            {onToggleBottomPanel ? (
              <IconButton
                icon="panelBottom"
                label={bottomLabel}
                active={bottomPanelOpen}
                onClick={onToggleBottomPanel}
                className="hidden md:inline-flex"
              />
            ) : null}
            {onToggleRightPanel ? (
              <IconButton
                icon={rightPanelOpen ? "panelRightClose" : "panelRightOpen"}
                label={rightLabel}
                active={rightPanelOpen}
                onClick={onToggleRightPanel}
                className="hidden md:inline-flex"
              />
            ) : null}
          </>
        ) : null}
        {!editing && view?.thread?.thread_id ? (
          <ConversationControlShield
            threadId={view.thread.thread_id}
            watermark={view.event_watermark}
            open={controlOpen}
            onOpenChange={setControlOpen}
          />
        ) : null}
        {!editing ? <ThreadGitActions view={view} /> : null}
        {panelControls}
      </div>
      <SnoozeDialog
        open={snoozeOpen}
        subject={`「${title}」`}
        onOpenChange={setSnoozeOpen}
        onConfirm={(until) => command({ kind: "snooze", until })}
      />
    </header>
  );
}
