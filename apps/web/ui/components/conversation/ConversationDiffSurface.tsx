"use client";

/**
 * Diff surface (T3 Code "Diff panel" parity): scope picker (working tree /
 * latest turn / any turn), every changed file in one virtualized scroll with
 * sticky headers, unified or split layout, file tree, and review comments that
 * flow into the composer.
 */

import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import {
  Button,
  Callout,
  DiffStat,
  EmptyState,
  IconButton,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  MenuSub,
  ScrollArea,
  SegmentedControl,
  Tooltip,
} from "@/components/chat/ui";
import type { ConversationView } from "@/lib/useConversation";
import {
  annotationBaselineId,
  fetchArtifactDiffText,
  fetchWorktreeDiff,
  latestDiffArtifact,
  listDiffArtifacts,
  turnIdFromDiffArtifactName,
  worktreeContentRevision,
  type DiffArtifactRef,
  type DiffBaselineKind,
  type DiffLineAnnotation,
  type DiffStaging,
  type WorktreeDiffResponse,
} from "@/lib/conversationDiff";
import { DiffList, type DiffListHandle } from "@/components/chat/diff/DiffList";
import { DiffSkeleton, StatusLetter } from "@/components/chat/diff/DiffRows";
import { buildFileTree, type DiffListFile, type DiffViewMode, type TreeNode } from "@/components/chat/diff/model";
import { filesFromMetas, filesFromPatch, totalStats } from "@/components/chat/diff/files";
import { DiffReviewBasket } from "./DiffReviewBasket";

export interface DiffBaselineRequest {
  kind: DiffBaselineKind;
  turnId?: string;
  artifactSha256?: string;
  /** Scroll to and flash this file once the diff renders. */
  filePath?: string;
}

type StagingFilter = "all" | "staged" | "unstaged" | "untracked";

const TREE_KEY = "muteki.chat.diff.tree";
const viewKey = (threadId: string) => `muteki:diff-view:${threadId}`;

function readBool(key: string, fallback: boolean): boolean {
  try {
    const raw = window.localStorage.getItem(key);
    return raw === null ? fallback : raw === "1";
  } catch {
    return fallback;
  }
}

function artifactTurnId(artifact: DiffArtifactRef | undefined): string | undefined {
  if (!artifact) return undefined;
  return artifact.turn_id || turnIdFromDiffArtifactName(artifact.name);
}

function shortTime(value?: string | null): string {
  if (!value) return "";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleString([], { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function worktreeFiles(data: WorktreeDiffResponse): DiffListFile[] {
  const out: DiffListFile[] = [];
  const seen = new Set<string>();
  const sections = data.sections || {};
  (["staged", "unstaged", "untracked"] as const).forEach((staging) => {
    const patch = sections[staging]?.patch;
    if (!patch) return;
    for (const file of filesFromPatch(patch, staging)) {
      out.push(file);
      seen.add(`${staging}:${file.meta.path}`);
    }
  });
  const rest = (data.files || [])
    .map((file) => ({ ...file, staging: (file.staging || "unstaged") as DiffStaging }))
    .filter((file) => !seen.has(`${file.staging}:${file.path}`));
  if (rest.length) out.push(...filesFromMetas(rest, out.length ? "" : data.patch));
  return out;
}

function TreeRows({
  nodes,
  depth,
  activeKey,
  closed,
  onToggleDir,
  onPick,
}: {
  nodes: TreeNode[];
  depth: number;
  activeKey: string;
  closed: ReadonlySet<string>;
  onToggleDir: (id: string) => void;
  onPick: (key: string) => void;
}) {
  return (
    <>
      {nodes.map((node) => {
        if (node.type === "dir") {
          const open = !closed.has(node.id);
          return (
            <div key={node.id}>
              <button
                type="button"
                onClick={() => onToggleDir(node.id)}
                className="flex h-7 w-full items-center gap-1 rounded-md pr-2 text-left text-[12px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg"
                style={{ paddingLeft: 6 + depth * 12 }}
              >
                <Icon name="chevronRight" size={11} className={cn("shrink-0 text-cx-fg-4 transition-transform duration-150", open && "rotate-90")} />
                <Icon name={open ? "folderOpen" : "folder"} size={13} className="shrink-0 text-cx-fg-4" />
                <span className="min-w-0 flex-1 truncate">{node.name}</span>
              </button>
              {open ? (
                <TreeRows nodes={node.children} depth={depth + 1} activeKey={activeKey} closed={closed} onToggleDir={onToggleDir} onPick={onPick} />
              ) : null}
            </div>
          );
        }
        const additions = node.file.meta.additions ?? node.file.parsed.additions;
        const deletions = node.file.meta.deletions ?? node.file.parsed.deletions;
        return (
          <button
            key={node.id}
            type="button"
            data-active={node.id === activeKey}
            onClick={() => onPick(node.id)}
            title={node.file.meta.path}
            className="cx-diff-tree-row flex h-7 w-full items-center gap-1.5 rounded-md pr-2 text-left text-[12px] text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg"
            style={{ paddingLeft: 20 + depth * 12 }}
          >
            <StatusLetter status={node.file.meta.status} />
            <span className="min-w-0 flex-1 truncate font-cx-mono">{node.name}</span>
            <DiffStat additions={additions} deletions={deletions} className="shrink-0 text-[10.5px]" />
          </button>
        );
      })}
    </>
  );
}

export function ConversationDiffSurface({
  threadId,
  view,
  requestedBaseline,
  onBaselineConsumed,
  onSendAnnotations,
  active = true,
}: {
  threadId: string;
  view: ConversationView;
  requestedBaseline?: DiffBaselineRequest | null;
  onBaselineConsumed?: () => void;
  onSendAnnotations?: (annotations: DiffLineAnnotation[]) => void;
  /** Visible in the panel; hidden surfaces skip focus-driven refreshes. */
  active?: boolean;
}) {
  const [kind, setKind] = useState<DiffBaselineKind>("worktree");
  const [selectedArtifactSha, setSelectedArtifactSha] = useState("");
  const [worktree, setWorktree] = useState<WorktreeDiffResponse | null>(null);
  const [files, setFiles] = useState<DiffListFile[]>([]);
  const [loading, setLoading] = useState(true);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState("");
  const [viewMode, setViewMode] = useState<DiffViewMode>("unified");
  const [wrap, setWrap] = useState(false);
  const [contextLines, setContextLines] = useState(3);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set());
  const [stagingFilter, setStagingFilter] = useState<StagingFilter>("all");
  const [treeOpen, setTreeOpen] = useState(false);
  const [closedDirs, setClosedDirs] = useState<ReadonlySet<string>>(() => new Set());
  const [activeKey, setActiveKey] = useState("");
  const [annotations, setAnnotations] = useState<DiffLineAnnotation[]>([]);
  const [pendingReveal, setPendingReveal] = useState("");
  const listRef = useRef<DiffListHandle | null>(null);
  const loadSeq = useRef(0);

  const diffArtifacts = useMemo(() => listDiffArtifacts(view.artifacts), [view.artifacts]);
  const runningTurnId = view.state.running_turn_id || "";
  const latestTurnId = runningTurnId || view.turns.at(-1)?.turn_id || "";
  const turnSeq = useMemo(() => new Map(view.turns.map((turn, index) => [turn.turn_id, turn.seq ?? index + 1])), [view.turns]);

  useEffect(() => {
    setAnnotations([]);
    setCollapsed(new Set());
  }, [threadId]);

  useEffect(() => {
    try {
      const raw = window.localStorage.getItem(viewKey(threadId));
      if (raw === "split" || raw === "unified") setViewMode(raw);
    } catch {
      // storage unavailable
    }
    setTreeOpen(readBool(TREE_KEY, false));
  }, [threadId]);

  const changeViewMode = (mode: DiffViewMode) => {
    setViewMode(mode);
    try { window.localStorage.setItem(viewKey(threadId), mode); } catch { /* ignore */ }
  };
  const toggleTree = () => {
    setTreeOpen((open) => {
      try { window.localStorage.setItem(TREE_KEY, open ? "0" : "1"); } catch { /* ignore */ }
      return !open;
    });
  };

  useEffect(() => {
    if (!requestedBaseline) return;
    setKind(requestedBaseline.kind);
    if (requestedBaseline.artifactSha256) {
      setSelectedArtifactSha(requestedBaseline.artifactSha256);
    } else if (requestedBaseline.kind !== "worktree") {
      const match = latestDiffArtifact(view.artifacts, requestedBaseline.turnId || latestTurnId);
      if (match) setSelectedArtifactSha(match.sha256);
    }
    if (requestedBaseline.filePath) setPendingReveal(requestedBaseline.filePath);
    onBaselineConsumed?.();
  }, [requestedBaseline, onBaselineConsumed, view.artifacts, latestTurnId]);

  const activeArtifact = useMemo(() => {
    if (kind === "worktree") return undefined;
    if (kind === "current_turn") return latestDiffArtifact(view.artifacts, latestTurnId);
    return diffArtifacts.find((row) => row.sha256 === selectedArtifactSha) || latestDiffArtifact(view.artifacts);
  }, [kind, view.artifacts, latestTurnId, diffArtifacts, selectedArtifactSha]);

  const worktreeRevision = useMemo(
    () => (kind === "worktree" ? worktreeContentRevision(worktree) : ""),
    [kind, worktree],
  );
  const currentBaselineId = useMemo(
    () => annotationBaselineId(
      kind,
      artifactTurnId(activeArtifact),
      activeArtifact?.sha256,
      kind === "worktree" ? worktreeRevision : undefined,
    ),
    [kind, activeArtifact, worktreeRevision],
  );
  useEffect(() => {
    setAnnotations((prev) => prev.map((a) => ({ ...a, stale: a.baselineId !== currentBaselineId })));
  }, [currentBaselineId]);

  const load = useCallback(async (options: { quiet?: boolean } = {}) => {
    const seq = ++loadSeq.current;
    const controller = new AbortController();
    if (!options.quiet) setLoading(true);
    setError("");
    try {
      if (kind === "worktree") {
        const data = await fetchWorktreeDiff(threadId, { signal: controller.signal });
        if (seq !== loadSeq.current) return;
        setWorktree(data);
        setFiles(data.is_repo === false ? [] : worktreeFiles(data));
      } else if (!activeArtifact) {
        if (seq !== loadSeq.current) return;
        setWorktree(null);
        setFiles([]);
      } else {
        const text = await fetchArtifactDiffText(threadId, activeArtifact.sha256, controller.signal);
        if (seq !== loadSeq.current) return;
        setWorktree(null);
        setFiles(filesFromPatch(text, "artifact"));
      }
      setLoaded(true);
    } catch (exc) {
      if ((exc as Error).name === "AbortError" || seq !== loadSeq.current) return;
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      if (seq === loadSeq.current) setLoading(false);
    }
  }, [kind, activeArtifact, threadId]);

  useEffect(() => {
    void load();
  }, [load]);

  // Working tree changes while the agent runs; refetch when a turn settles or the window regains focus.
  const wasRunning = useRef(Boolean(runningTurnId));
  useEffect(() => {
    const running = Boolean(runningTurnId);
    if (wasRunning.current && !running && kind === "worktree") {
      const timer = window.setTimeout(() => void load({ quiet: true }), 400);
      wasRunning.current = running;
      return () => window.clearTimeout(timer);
    }
    wasRunning.current = running;
  }, [runningTurnId, kind, load]);

  useEffect(() => {
    if (!active || kind !== "worktree") return;
    let timer = 0;
    const onFocus = () => {
      window.clearTimeout(timer);
      timer = window.setTimeout(() => void load({ quiet: true }), 300);
    };
    window.addEventListener("focus", onFocus);
    return () => {
      window.removeEventListener("focus", onFocus);
      window.clearTimeout(timer);
    };
  }, [active, kind, load]);

  const stagingCounts = useMemo(() => {
    const counts: Record<Exclude<StagingFilter, "all">, number> = { staged: 0, unstaged: 0, untracked: 0 };
    for (const file of files) {
      const staging = file.meta.staging;
      if (staging === "staged" || staging === "unstaged" || staging === "untracked") counts[staging] += 1;
    }
    return counts;
  }, [files]);
  const showStagingFilter = kind === "worktree" && Object.values(stagingCounts).filter(Boolean).length > 1;
  const visibleFiles = useMemo(
    () => (showStagingFilter && stagingFilter !== "all" ? files.filter((file) => file.meta.staging === stagingFilter) : files),
    [files, showStagingFilter, stagingFilter],
  );
  const totals = useMemo(() => totalStats(visibleFiles), [visibleFiles]);
  const tree = useMemo(() => buildFileTree(visibleFiles), [visibleFiles]);

  useEffect(() => {
    if (!pendingReveal || !visibleFiles.length) return;
    const target = visibleFiles.find((file) => file.meta.path === pendingReveal || file.meta.path.endsWith(`/${pendingReveal}`) || pendingReveal.endsWith(`/${file.meta.path}`));
    const frame = window.requestAnimationFrame(() => {
      if (target) {
        setCollapsed((current) => {
          if (!current.has(target.key)) return current;
          const next = new Set(current);
          next.delete(target.key);
          return next;
        });
        listRef.current?.scrollToFile(target.key, { flash: true });
      }
      setPendingReveal("");
    });
    return () => window.cancelAnimationFrame(frame);
  }, [pendingReveal, visibleFiles]);

  const handleAddAnnotation = useCallback((annotation: Omit<DiffLineAnnotation, "id" | "createdAt" | "stale">) => {
    setAnnotations((prev) => [
      ...prev,
      {
        ...annotation,
        id: `ann-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`,
        createdAt: new Date().toISOString(),
        stale: annotation.baselineId !== currentBaselineId,
      },
    ]);
  }, [currentBaselineId]);

  const handleSendAll = useCallback(() => {
    if (!onSendAnnotations || annotations.some((a) => a.stale)) return;
    onSendAnnotations(annotations);
    setAnnotations([]);
  }, [annotations, onSendAnnotations]);

  const toggleFile = useCallback((key: string) => {
    setCollapsed((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);
  const allCollapsed = visibleFiles.length > 0 && visibleFiles.every((file) => collapsed.has(file.key));

  const artifactsNewestFirst = useMemo(() => [...diffArtifacts].reverse(), [diffArtifacts]);
  const artifactLabel = (artifact: DiffArtifactRef | undefined) => {
    const turnId = artifactTurnId(artifact);
    const seq = turnId ? turnSeq.get(turnId) : undefined;
    return seq ? `回合 ${seq}` : turnId ? `回合 ${turnId.slice(0, 8)}` : "回合";
  };
  const scopeLabel = kind === "worktree" ? "工作树" : kind === "current_turn" ? "最新回合" : artifactLabel(activeArtifact);
  const scopeDetail = kind === "worktree"
    ? [worktree?.current_branch || (worktree?.detached_sha ? `detached ${worktree.detached_sha}` : ""), worktree?.head_sha?.slice(0, 7)].filter(Boolean).join(" · ")
    : shortTime(activeArtifact?.created_at);

  const notRepo = kind === "worktree" && worktree?.is_repo === false;
  const noArtifact = kind !== "worktree" && !activeArtifact;
  const showSkeleton = loading && !loaded;

  return (
    <div className="flex h-full min-h-0 flex-col bg-cx-bg" data-testid="conversation-diff-surface">
      <div className="flex h-10 shrink-0 items-center gap-1 border-b border-cx-border-subtle pl-2 pr-1.5">
        <Menu
          ariaLabel="Diff 范围"
          placement="bottom-start"
          trigger={(
            <button
              type="button"
              data-testid="diff-baseline-picker"
              className="cx-press flex h-7 min-w-0 items-center gap-1.5 rounded-lg px-2 text-[12.5px] font-medium text-cx-fg hover:bg-cx-hover data-[state=open]:bg-cx-active"
            >
              <Icon name={kind === "worktree" ? "gitBranch" : "history"} size={13} className="shrink-0 text-cx-fg-3" />
              <span className="truncate">{scopeLabel}</span>
              <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
            </button>
          )}
        >
          <MenuLabel>比较范围</MenuLabel>
          <div data-testid="diff-baseline-worktree">
            <MenuItem icon="gitBranch" checked={kind === "worktree"} description="当前未提交的全部改动" onSelect={() => setKind("worktree")}>
              工作树
            </MenuItem>
          </div>
          <div data-testid="diff-baseline-current-turn">
            <MenuItem icon="sparkles" checked={kind === "current_turn"} description="最近一个回合产生的改动" disabled={!diffArtifacts.length} onSelect={() => setKind("current_turn")}>
              最新回合
            </MenuItem>
          </div>
          {artifactsNewestFirst.length ? (
            <>
              <MenuSeparator />
              <MenuSub label="按回合查看" icon="history">
                {artifactsNewestFirst.map((artifact) => (
                  <MenuItem
                    key={artifact.sha256}
                    checked={kind === "turn" && activeArtifact?.sha256 === artifact.sha256}
                    hint={shortTime(artifact.created_at)}
                    onSelect={() => { setKind("turn"); setSelectedArtifactSha(artifact.sha256); }}
                  >
                    {artifactLabel(artifact)}
                  </MenuItem>
                ))}
              </MenuSub>
            </>
          ) : null}
        </Menu>
        {scopeDetail ? (
          <span className="hidden min-w-0 truncate font-cx-mono text-[11.5px] text-cx-fg-4 @[480px]/panel:inline" data-testid="diff-baseline-identity">
            {scopeDetail}
          </span>
        ) : null}
        <span className="flex-1" />
        {visibleFiles.length ? <DiffStat additions={totals.additions} deletions={totals.deletions} className="mr-1 hidden @[420px]/panel:inline-flex" /> : null}
        <span data-testid="diff-refresh">
          <IconButton icon="refresh" label="刷新" size="sm" loading={loading && loaded} onClick={() => void load({ quiet: true })} />
        </span>
        <IconButton
          icon={allCollapsed ? "chevronsUpDown" : "chevronsDownUp"}
          label={allCollapsed ? "全部展开" : "全部折叠"}
          size="sm"
          className="hidden @[500px]/panel:inline-flex"
          disabled={!visibleFiles.length}
          onClick={() => setCollapsed(allCollapsed ? new Set() : new Set(visibleFiles.map((file) => file.key)))}
        />
        <SegmentedControl
          size="xs"
          value={viewMode}
          onChange={changeViewMode}
          ariaLabel="Diff 布局"
          className="mx-0.5"
          options={[
            { value: "unified", icon: "alignJustify", ariaLabel: "合并视图" },
            { value: "split", icon: "columns", ariaLabel: "分栏视图" },
          ]}
        />
        <IconButton icon="wrapText" label={wrap ? "不换行" : "自动换行"} size="sm" active={wrap} onClick={() => setWrap((value) => !value)} />
        <IconButton
          icon={contextLines >= 999 ? "foldVertical" : "unfoldVertical"}
          label={contextLines >= 999 ? "折叠未变更行" : "展开全部上下文"}
          size="sm"
          className="hidden @[560px]/panel:inline-flex"
          onClick={() => setContextLines((value) => (value >= 999 ? 3 : 999))}
        />
        <IconButton icon="folderTree" label={treeOpen ? "隐藏文件树" : "显示文件树"} size="sm" active={treeOpen} onClick={toggleTree} />
      </div>

      {showStagingFilter ? (
        <div className="flex h-9 shrink-0 items-center gap-2 border-b border-cx-border-subtle px-2.5">
          <SegmentedControl<StagingFilter>
            size="xs"
            value={stagingFilter}
            onChange={setStagingFilter}
            ariaLabel="暂存区筛选"
            options={[
              { value: "all", label: `全部 ${files.length}` },
              { value: "staged", label: `已暂存 ${stagingCounts.staged}`, disabled: !stagingCounts.staged },
              { value: "unstaged", label: `未暂存 ${stagingCounts.unstaged}`, disabled: !stagingCounts.unstaged },
              { value: "untracked", label: `未跟踪 ${stagingCounts.untracked}`, disabled: !stagingCounts.untracked },
            ]}
          />
        </div>
      ) : null}

      {error && loaded ? (
        <div className="shrink-0 px-3 pt-2" data-testid="diff-error">
          <Callout tone="danger" onDismiss={() => setError("")}>{error}</Callout>
        </div>
      ) : null}

      <div className="flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          {showSkeleton ? (
            <DiffSkeleton />
          ) : error && !loaded ? (
            <EmptyState
              icon="circleAlert"
              title="读取变更失败"
              description={error}
              action={<Button variant="primary" size="sm" icon="refresh" onClick={() => void load()}>重试</Button>}
            />
          ) : notRepo ? (
            <EmptyState icon="gitBranch" title="当前目录不是 Git 仓库" description="绑定 Git 工作区后，这里会显示工作树改动。" />
          ) : noArtifact ? (
            <EmptyState icon="history" title="还没有回合变更" description="Agent 在工作区里修改文件后，每个回合的改动都能在这里回看。" />
          ) : !visibleFiles.length ? (
            <EmptyState
              icon="checkCheck"
              title={kind === "worktree" ? "工作树很干净" : "这个回合没有改动文件"}
              description={kind === "worktree" ? "当前分支与索引都没有未提交的改动。" : undefined}
            />
          ) : (
            <DiffList
              handleRef={listRef}
              files={visibleFiles}
              mode={viewMode}
              wrap={wrap}
              contextLines={contextLines}
              collapsed={collapsed}
              onToggleFile={toggleFile}
              threadId={threadId}
              annotations={annotations}
              baselineId={currentBaselineId}
              onAddAnnotation={onSendAnnotations ? handleAddAnnotation : undefined}
              onDeleteAnnotation={(id) => setAnnotations((prev) => prev.filter((a) => a.id !== id))}
              onActiveFileChange={setActiveKey}
            />
          )}
        </div>
        {treeOpen && visibleFiles.length ? (
          <aside className="flex w-[min(15rem,38%)] shrink-0 flex-col border-l border-cx-border-subtle bg-cx-bg-subtle" aria-label="变更文件">
            <div className="flex h-8 shrink-0 items-center gap-1.5 px-3 text-[11.5px] font-medium text-cx-fg-3">
              <span className="flex-1">{visibleFiles.length} 个文件</span>
              <Tooltip content="折叠全部目录">
                <button
                  type="button"
                  aria-label="折叠全部目录"
                  onClick={() => setClosedDirs(new Set(tree.filter((node) => node.type === "dir").map((node) => node.id)))}
                  className="grid size-5 place-items-center rounded text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
                >
                  <Icon name="chevronsDownUp" size={12} />
                </button>
              </Tooltip>
            </div>
            <ScrollArea className="flex-1 px-1 pb-2">
              <TreeRows
                nodes={tree}
                depth={0}
                activeKey={activeKey}
                closed={closedDirs}
                onToggleDir={(id) => setClosedDirs((current) => {
                  const next = new Set(current);
                  if (next.has(id)) next.delete(id);
                  else next.add(id);
                  return next;
                })}
                onPick={(key) => {
                  setCollapsed((current) => {
                    if (!current.has(key)) return current;
                    const next = new Set(current);
                    next.delete(key);
                    return next;
                  });
                  window.requestAnimationFrame(() => listRef.current?.scrollToFile(key, { flash: true }));
                }}
              />
            </ScrollArea>
          </aside>
        ) : null}
      </div>

      {annotations.length > 0 ? (
        <DiffReviewBasket
          annotations={annotations}
          onUpdate={(id, comment) => setAnnotations((prev) => prev.map((a) => (a.id === id ? { ...a, comment } : a)))}
          onDelete={(id) => setAnnotations((prev) => prev.filter((a) => a.id !== id))}
          onSendAll={handleSendAll}
        />
      ) : null}
    </div>
  );
}
