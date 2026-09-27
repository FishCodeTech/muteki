"use client";

import React, { useEffect, useRef, useState } from "react";
import type { ConversationView } from "@/lib/useConversation";
import { cn } from "@/lib/cn";
import { useSharedGitStatus } from "@/lib/threadGitStatusStore";
import {
  resolveWorkspaceBranchChip,
  type LiveGitBranchStatus,
} from "@/lib/workspaceBranchDisplay";
import { Icon, type IconName } from "../Icon";
import {
  Badge,
  Button,
  IconButton,
  Menu,
  MenuItem,
  MenuSeparator,
  Spinner,
  Tooltip,
  type Tone,
} from "@/components/chat/ui";

export interface ConversationHeaderProps {
  view: ConversationView | null;
  /** Resolved project display name; falls back to Muteki when absent. */
  projectName?: string;
  /** Right-panel toggles owned by the Shell; replaces the legacy dock buttons when set. */
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
  if (["interrupted", "aborted", "cancelled", "canceled"].includes(lastStatus)) {
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

function Chip({ icon, children, title }: { icon: IconName; children: React.ReactNode; title?: string }) {
  return (
    <span
      title={title}
      className="inline-flex h-6 max-w-[180px] flex-none items-center gap-1 rounded-md px-1.5 text-[12px] leading-none text-cx-fg-3"
    >
      <Icon name={icon} size={13} className="shrink-0 text-cx-fg-4" />
      <span className="truncate">{children}</span>
    </span>
  );
}

/** `refocus` is false when editing ended because focus moved elsewhere (blur). */
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
      <label className="sr-only" htmlFor="conversation-title-input">对话标题</label>
      <input
        id="conversation-title-input"
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
          "h-8 w-full min-w-0 max-w-[520px] rounded-lg border border-cx-border-strong bg-cx-elevated px-2.5",
          "text-[14px] font-semibold tracking-[-0.01em] text-cx-fg outline-none",
        )}
      />
    </form>
  );
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
  onFork,
  onRename,
  onArchive,
  onExport,
  busy = false,
  className = "",
}: ConversationHeaderProps) {
  const [editing, setEditing] = useState(false);
  const titleButtonRef = useRef<HTMLButtonElement>(null);
  const threadId = view?.thread.thread_id;
  const projectId = view?.thread.project_id || view?.workspace?.project_id || undefined;
  const git = useSharedGitStatus(threadId, projectId || undefined);

  useEffect(() => {
    setEditing(false);
  }, [threadId]);

  if (!view) return null;

  const title = view.thread.title || "未命名对话";
  const isArchived = view.state.status === "archived";
  const status = headerStatus(view);
  const workspace = workspaceChip(view, {
    status: git.status,
    error: git.error,
    loading: git.loading,
  });
  const projectLabel = projectName?.trim() || (view.thread.project_id ? "未知项目" : "");
  const legacyDock = panelControls === undefined;
  const outputLabel = outputCardOpen ? "关闭任务输出" : "打开任务输出";
  const bottomLabel = bottomPanelOpen ? "关闭 Agent 执行日志" : "打开 Agent 执行日志";
  const rightLabel = rightPanelOpen ? "隐藏右侧面板" : "显示右侧面板";

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
        "cx-conv-header @container/header relative z-10 flex h-[var(--cx-header-h,52px)] flex-none items-center gap-3 border-b border-cx-border-subtle bg-cx-bg px-3 sm:px-4",
        className,
      )}
    >
      <div className="flex min-w-0 flex-1 items-center gap-1.5">
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
          <h1 className="min-w-0 shrink text-[14px] font-semibold leading-5 tracking-[-0.01em] text-cx-fg">
            {onRename ? (
              <Tooltip content="点击重命名" placement="bottom">
                <button
                  ref={titleButtonRef}
                  type="button"
                  onClick={startEditing}
                  className={cn(
                    "cx-press -mx-1.5 block h-8 max-w-full truncate rounded-lg px-1.5 text-left outline-none",
                    "hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
                  )}
                >
                  {title}
                </button>
              </Tooltip>
            ) : (
              <span className="block truncate">{title}</span>
            )}
          </h1>
        )}

        {!editing ? (
          <div className="hidden flex-none items-center gap-0.5 @[860px]/header:flex">
            {projectLabel ? <Chip icon="folder" title={projectLabel}>{projectLabel}</Chip> : null}
            {workspace ? <Chip icon={workspace.icon} title={workspace.title}>{workspace.label}</Chip> : null}
          </div>
        ) : null}

        {!editing ? (
          <span className="flex flex-none items-center gap-1" role="status" aria-live="polite">
            {isArchived ? <Badge tone="warning" icon="archive">已归档</Badge> : null}
            <Badge tone={status.tone} className={cn(status.tone === "neutral" && "bg-transparent text-cx-fg-4")}>
              {status.running ? <Spinner size={11} /> : null}
              {status.label}
            </Badge>
          </span>
        ) : null}
      </div>

      <div className="flex flex-none items-center gap-0.5" aria-label="会话工作区显示设置">
        {legacyDock ? (
          <>
            {onOpenReadingPrefs ? (
              <Button
                size="sm"
                variant="ghost"
                active={readingPrefsOpen}
                data-testid="c39-reading-prefs-toggle"
                aria-label="对话阅读偏好"
                tooltip="对话阅读偏好"
                onClick={onOpenReadingPrefs}
                className="hidden md:inline-flex"
              >
                阅读
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

        <Menu
          placement="bottom-end"
          ariaLabel="会话操作"
          trigger={(
            <IconButton
              icon="more"
              label="会话操作"
              tooltip="更多操作"
              data-testid="conversation-title-menu-trigger"
            />
          )}
        >
          {onRename ? <MenuItem icon="pencil" onSelect={startEditing}>重命名</MenuItem> : null}
          {onFork ? <MenuItem icon="gitFork" disabled={busy} onSelect={onFork}>分叉对话</MenuItem> : null}
          {onExport ? <MenuItem icon="download" onSelect={onExport}>导出对话</MenuItem> : null}
          <MenuItem icon="info" onSelect={onOpenInfo}>会话信息与记忆</MenuItem>
          {onOpenReadingPrefs ? (
            <MenuItem icon="eye" checked={readingPrefsOpen} onSelect={onOpenReadingPrefs}>
              <span data-testid="menu-open-reading-prefs">阅读偏好</span>
            </MenuItem>
          ) : null}
          {onOpenShortcuts ? <MenuItem icon="keyboard" shortcut="?" onSelect={onOpenShortcuts}>键盘快捷键</MenuItem> : null}
          {legacyDock && (onToggleOutput || onToggleBottomPanel || onToggleRightPanel) ? (
            <>
              <MenuSeparator />
              {onToggleOutput ? (
                <MenuItem icon="board" checked={outputCardOpen} onSelect={onToggleOutput}>
                  <span data-testid="menu-open-task-output">{outputLabel}</span>
                </MenuItem>
              ) : null}
              {onToggleBottomPanel ? (
                <MenuItem icon="terminal" checked={bottomPanelOpen} onSelect={onToggleBottomPanel}>
                  <span data-testid="menu-open-agent-log">{bottomLabel}</span>
                </MenuItem>
              ) : null}
              {onToggleRightPanel ? (
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

        {panelControls ? (
          <>
            <span aria-hidden className="mx-1 h-4 w-px bg-cx-border" />
            {panelControls}
          </>
        ) : null}
      </div>
    </header>
  );
}
