"use client";

import type { ReactNode } from "react";
import { useEffect, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Select } from "@/components/chat/ui";
import { stripPillClass } from "@/components/chat/composer/stripPill";
import { Icon } from "../Icon";
import { ConversationContextPicker } from "./ConversationContextPicker";
import { ComposerBranchPicker } from "./ComposerBranchPicker";
import {
  fetchProjectWorktrees,
  type ConversationProject,
  type GitWorktreeRow,
  type WorkspaceBindMode,
} from "@/lib/useConversation";
import {
  canBindExistingWorktree,
  isMainCheckoutPath,
  linkedWorktrees,
  normalizeWorktreePath,
  reconcileSelectedWorktreePath,
  worktreeEmptyCopy,
  worktreeListPhase,
  worktreeLoadFailCopy,
  worktreeMissingSelectionCopy,
  worktreeOccupiedCopy,
  worktreeOptionLabel,
} from "@/lib/existingWorktreePicker";

export interface ComposerContextStripProps {
  projects: ConversationProject[];
  selectedProjectId: string;
  onProjectChange: (id: string) => void;
  onCreateProject?: () => Promise<string | null>;
  onCreateFromPath?: (path: string) => Promise<string | null>;
  creatingProject?: boolean;
  projectsLoading?: boolean;
  projectsError?: string;
  onRetryProjects?: () => void;
  preferPathInput?: boolean;
  requestPathInput?: boolean;
  recentPaths?: string[];
  /** 会话已绑定工作区时覆盖展示用根路径（优先于 project.root_path）。 */
  workspaceRootPath?: string;
  threadId?: string;
  workspaceMode?: WorkspaceBindMode;
  onWorkspaceModeChange?: (mode: WorkspaceBindMode) => void;
  worktreeBranch?: string;
  onWorktreeBranchChange?: (branch: string) => void;
  /** Selected existing worktree absolute path (lifted for first-send bind). */
  existingWorktreePath?: string;
  onExistingWorktreePathChange?: (path: string) => void;
  projectDisabled?: boolean;
  branchDisabled?: boolean;
  modeDisabled?: boolean;
  className?: string;
  /** Right-aligned slot (usage stats, context meter). */
  trailing?: ReactNode;
}

const MODE_OPTIONS: Array<{ value: WorkspaceBindMode; label: string; description: string }> = [
  { value: "shared_checkout", label: "使用现有检出", description: "直接在项目目录中工作" },
  { value: "new_worktree", label: "新建 worktree", description: "隔离的新分支，互不干扰" },
  { value: "existing_worktree", label: "已有 worktree", description: "复用之前创建的 worktree" },
];

function pathBasename(path: string): string {
  const trimmed = path.replace(/\/$/, "");
  if (!trimmed) return "";
  return trimmed.split("/").filter(Boolean).at(-1) || trimmed;
}

function modeLabel(mode: WorkspaceBindMode | undefined, bound: boolean): string | undefined {
  if (bound) {
    if (mode === "new_worktree") return "新建 worktree";
    if (mode === "existing_worktree") return "已有 worktree";
    if (mode === "shared_checkout") return "主检出";
    return "本地检出";
  }
  if (mode === "new_worktree") return "将新建 worktree";
  if (mode === "existing_worktree") return "将绑定 worktree";
  if (mode === "shared_checkout") return "使用主检出";
  return undefined;
}

/**
 * t3code BranchToolbar / chat-composer-context-strip 的 Muteki 对应物：
 * 贴在 PromptBar 下方，左侧为本会话工作目录，右侧为 Git 分支。
 */
export function ComposerContextStrip({
  projects,
  selectedProjectId,
  onProjectChange,
  onCreateProject,
  onCreateFromPath,
  creatingProject = false,
  projectsLoading = false,
  projectsError = "",
  onRetryProjects,
  preferPathInput = false,
  requestPathInput = false,
  recentPaths = [],
  workspaceRootPath,
  threadId = "",
  workspaceMode,
  onWorkspaceModeChange,
  worktreeBranch = "",
  onWorktreeBranchChange,
  existingWorktreePath = "",
  onExistingWorktreePathChange,
  projectDisabled = false,
  branchDisabled = false,
  modeDisabled = false,
  className = "",
  trailing,
}: ComposerContextStripProps) {
  const currentProject = projects.find((project) => project.project_id === selectedProjectId);
  const rootPath = (workspaceRootPath || currentProject?.root_path || "").trim();
  const directoryLabel = rootPath
    ? pathBasename(rootPath)
    : (currentProject?.name || "");
  const bound = Boolean(threadId && workspaceRootPath);
  const showModeControls = Boolean(selectedProjectId && onWorkspaceModeChange && !modeDisabled);
  const mode = workspaceMode || "shared_checkout";
  const pickingExisting = workspaceMode === "existing_worktree" && !bound;

  const [worktrees, setWorktrees] = useState<GitWorktreeRow[]>([]);
  const [worktreesLoadedFor, setWorktreesLoadedFor] = useState("");
  const [worktreesLoading, setWorktreesLoading] = useState(false);
  const [worktreesError, setWorktreesError] = useState("");
  const [worktreesNonce, setWorktreesNonce] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setWorktrees([]);
    setWorktreesLoadedFor("");
    setWorktreesError("");
    if (!pickingExisting || !selectedProjectId) {
      setWorktreesLoading(false);
      return;
    }
    setWorktreesLoading(true);
    // An older project's response must never replace the current path options.
    void fetchProjectWorktrees(selectedProjectId).then(
      (rows) => {
        if (cancelled) return;
        setWorktrees(rows);
        setWorktreesLoadedFor(selectedProjectId);
        setWorktreesLoading(false);
      },
      (err: unknown) => {
        if (cancelled) return;
        setWorktreesError(err instanceof Error ? err.message : "读取 worktree 列表失败");
        setWorktreesLoading(false);
      },
    );
    return () => { cancelled = true; };
  }, [pickingExisting, selectedProjectId, worktreesNonce]);

  const projectRoot = currentProject?.root_path || "";
  const linked = useMemo(
    () => linkedWorktrees(worktrees, projectRoot),
    [worktrees, projectRoot],
  );
  const hasMainOnly = useMemo(() => {
    if (worktrees.length === 0) return false;
    return worktrees.every((row) => isMainCheckoutPath(row.path, projectRoot))
      || (worktrees.some((row) => isMainCheckoutPath(row.path, projectRoot)) && linked.length === 0);
  }, [worktrees, projectRoot, linked.length]);

  const phase = worktreeListPhase({
    modeIsExisting: pickingExisting,
    hasProject: Boolean(selectedProjectId),
    loading: worktreesLoading,
    error: worktreesError,
    linkedCount: linked.length,
  });

  // Drop stale selection when list changes or project/mode no longer applies.
  useEffect(() => {
    if (!onExistingWorktreePathChange) return;
    if (!pickingExisting) return;
    // Restored paths are valid candidates until this project's list has loaded.
    // On first mount the loading effect has not committed its state update yet.
    if (worktreesLoadedFor !== selectedProjectId) return;
    if (phase === "loading") return;
    if (phase === "error") return;
    const next = reconcileSelectedWorktreePath(existingWorktreePath, linked);
    if (next !== normalizeWorktreePath(existingWorktreePath)) {
      onExistingWorktreePathChange(next);
    }
  }, [
    pickingExisting,
    selectedProjectId,
    worktreesLoadedFor,
    phase,
    linked,
    existingWorktreePath,
    onExistingWorktreePathChange,
  ]);

  const selectedRow = linked.find(
    (row) => normalizeWorktreePath(row.path) === normalizeWorktreePath(existingWorktreePath),
  );
  const selectionValid = canBindExistingWorktree(existingWorktreePath) && Boolean(selectedRow);
  const selectedPath = normalizeWorktreePath(existingWorktreePath);
  const pathOptions = useMemo(
    () => linked.map((row) => {
      const path = normalizeWorktreePath(row.path);
      return {
        value: path,
        label: worktreeOptionLabel(row),
        textValue: worktreeOptionLabel(row),
        description: path,
        icon: "gitFork" as const,
      };
    }),
    [linked],
  );

  return (
    <div
      className={cn("cx-context-strip flex min-w-0 flex-col gap-1 px-1.5 pt-1.5", className)}
      data-has-project={selectedProjectId ? "true" : "false"}
    >
      <div className="flex min-w-0 flex-wrap items-center gap-x-1 gap-y-1">
        <ConversationContextPicker
          projects={projects}
          selectedProjectId={selectedProjectId}
          onProjectChange={onProjectChange}
          onCreateProject={projectDisabled ? undefined : onCreateProject}
          onCreateFromPath={projectDisabled ? undefined : onCreateFromPath}
          creatingProject={creatingProject}
          loading={projectsLoading}
          error={projectsError}
          onRetry={onRetryProjects}
          preferPathInput={preferPathInput}
          requestPathInput={requestPathInput}
          recentPaths={recentPaths}
          disabled={projectDisabled}

          directoryLabel={directoryLabel || undefined}
          directoryTitle={rootPath || undefined}
          modeLabel={modeLabel(workspaceMode, bound) || (currentProject || rootPath ? "本地检出" : undefined)}
        />
        {showModeControls ? (
          <>
            <Select<WorkspaceBindMode>
              value={mode}
              onChange={(next) => onWorkspaceModeChange?.(next)}
              disabled={bound}
              ariaLabel="工作区模式"
              placement="top-start"
              popoverClassName="w-[260px]"
              options={MODE_OPTIONS.map((item) => ({
                ...item,
                icon: item.value === "shared_checkout" ? "folder" : "gitFork",
              }))}
              trigger={(
                <button type="button" disabled={bound} className={stripPillClass(true)} aria-label="工作区模式">
                  <Icon name={mode === "shared_checkout" ? "folder" : "gitFork"} size={13} />
                  <span className="truncate">{MODE_OPTIONS.find((item) => item.value === mode)?.label}</span>
                  <Icon name="chevronDown" size={12} className="text-cx-fg-4" />
                </button>
              )}
            />
            {workspaceMode === "new_worktree" && !bound ? (
              <input
                aria-label="新 worktree 分支名"
                className="h-7 w-[180px] min-w-0 rounded-full border border-cx-border bg-transparent px-3 font-cx-mono text-[12px] text-cx-fg outline-none transition-[border-color,box-shadow] placeholder:font-cx-sans placeholder:text-cx-fg-4 focus:border-cx-border-strong"
                placeholder="新分支名，如 feat-login"
                value={worktreeBranch}
                onChange={(event) => onWorktreeBranchChange?.(event.target.value)}
              />
            ) : null}
            {pickingExisting && phase === "ready" ? (
              <Select<string>
                value={selectedPath || null}
                onChange={(next) => onExistingWorktreePathChange?.(next)}
                ariaLabel="已有 worktree 路径"
                placement="top-start"
                popoverClassName="w-[min(360px,calc(100vw-24px))]"
                searchable
                searchPlaceholder="搜索 worktree…"
                placeholder="选择 worktree…"
                options={pathOptions}
                trigger={(
                  <button
                    type="button"
                    id="composer-existing-worktree-path"
                    className={stripPillClass(true)}
                    aria-label="已有 worktree 路径"
                    title={selectedPath || undefined}
                  >
                    <Icon name="gitFork" size={13} />
                    <span className="truncate">
                      {selectedRow ? worktreeOptionLabel(selectedRow) : "选择 worktree…"}
                    </span>
                    <Icon name="chevronDown" size={12} className="text-cx-fg-4" />
                  </button>
                )}
              />
            ) : null}
          </>
        ) : null}
        <ComposerBranchPicker
          projectId={selectedProjectId}
          threadId={threadId}
          disabled={branchDisabled}

        />
        {trailing ? <div className="ml-auto flex min-w-0 items-center gap-1.5">{trailing}</div> : null}
      </div>
      {pickingExisting ? (
        <div
          className="px-0.5 text-[11px] leading-snug text-cx-fg-3"
          data-testid="existing-worktree-status"
          data-phase={phase}
        >
          {phase === "loading" ? (
            <span>正在加载 worktree 列表…</span>
          ) : null}
          {phase === "error" ? (
            <span className="inline-flex flex-wrap items-center gap-2 text-cx-warning">
              <span>{worktreeLoadFailCopy(worktreesError)}</span>
              <button
                type="button"
                className="underline hover:text-cx-fg"
                onClick={() => setWorktreesNonce((n) => n + 1)}
              >
                重试
              </button>
            </span>
          ) : null}
          {phase === "empty" ? (
            <span>{worktreeEmptyCopy(hasMainOnly)}</span>
          ) : null}
          {phase === "ready" && !selectionValid ? (
            <span>{worktreeMissingSelectionCopy()}</span>
          ) : null}
          {phase === "ready" && selectedRow?.occupied ? (
            <span className="text-cx-warning">{worktreeOccupiedCopy(selectedRow)}</span>
          ) : null}
          {phase === "ready" && selectionValid && selectedRow && !selectedRow.occupied ? (
            <span title={normalizeWorktreePath(selectedRow.path)}>
              将绑定 {normalizeWorktreePath(selectedRow.path)}
              {selectedRow.branch ? `（${selectedRow.branch}）` : selectedRow.detached ? "（detached）" : ""}
            </span>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
