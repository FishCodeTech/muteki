"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Button, DiffStat, IconButton, Skeleton } from "@/components/chat/ui";
import { chatPanel } from "@/lib/chatPanelStore";
import {
  fetchArtifactDiffText,
  splitUnifiedDiffFiles,
  type DiffArtifactRef,
  type ParsedDiffFile,
} from "@/lib/conversationDiff";

const VISIBLE_FILES = 8;

/** Parsed patches survive virtualized remounts (keyed by thread + sha). */
const resolvedDiffs = new Map<string, ParsedDiffFile[]>();
const pendingDiffs = new Map<string, Promise<ParsedDiffFile[]>>();

function loadDiff(threadId: string, sha256: string): Promise<ParsedDiffFile[]> {
  const key = `${threadId}:${sha256}`;
  const pending = pendingDiffs.get(key);
  if (pending) return pending;
  const next = fetchArtifactDiffText(threadId, sha256)
    .then((text) => {
      const files = splitUnifiedDiffFiles(text);
      resolvedDiffs.set(key, files);
      return files;
    })
    .finally(() => pendingDiffs.delete(key));
  pendingDiffs.set(key, next);
  return next;
}

export interface ChangedFilesOpenRequest {
  kind: "turn";
  turnId?: string;
  artifactSha256: string;
  filePath?: string;
}

interface TreeDir {
  name: string;
  path: string;
  dirs: Map<string, TreeDir>;
  files: ParsedDiffFile[];
}

type TreeRow =
  | { type: "dir"; key: string; name: string; depth: number; fileCount: number }
  | { type: "file"; key: string; name: string; depth: number; file: ParsedDiffFile };

function buildTree(files: ParsedDiffFile[]): TreeDir {
  const root: TreeDir = { name: "", path: "", dirs: new Map(), files: [] };
  for (const file of files) {
    const parts = file.path.split("/").filter(Boolean);
    let dir = root;
    for (const part of parts.slice(0, -1)) {
      const path = dir.path ? `${dir.path}/${part}` : part;
      let child = dir.dirs.get(part);
      if (!child) {
        child = { name: part, path, dirs: new Map(), files: [] };
        dir.dirs.set(part, child);
      }
      dir = child;
    }
    dir.files.push(file);
  }
  return root;
}

function compress(dir: TreeDir): TreeDir {
  let current = dir;
  let name = dir.name;
  while (current.files.length === 0 && current.dirs.size === 1) {
    const only = [...current.dirs.values()][0];
    name = name ? `${name}/${only.name}` : only.name;
    current = only;
  }
  return { ...current, name, dirs: new Map([...current.dirs].map(([key, child]) => [key, compress(child)])) };
}

function countFiles(dir: TreeDir): number {
  let total = dir.files.length;
  for (const child of dir.dirs.values()) total += countFiles(child);
  return total;
}

function flatten(dir: TreeDir, depth: number, closed: Set<string>, rows: TreeRow[]) {
  const dirs = [...dir.dirs.values()].sort((a, b) => a.name.localeCompare(b.name));
  for (const child of dirs) {
    rows.push({ type: "dir", key: `d:${child.path}`, name: child.name, depth, fileCount: countFiles(child) });
    if (!closed.has(child.path)) flatten(child, depth + 1, closed, rows);
  }
  const files = [...dir.files].sort((a, b) => a.path.localeCompare(b.path));
  for (const file of files) {
    rows.push({ type: "file", key: `f:${file.path}`, name: file.path.split("/").pop() || file.path, depth, file });
  }
}

const STATUS_META: Record<string, { letter: string; label: string; className: string }> = {
  A: { letter: "A", label: "新增", className: "text-cx-add" },
  M: { letter: "M", label: "修改", className: "text-cx-warning" },
  D: { letter: "D", label: "删除", className: "text-cx-del" },
  R: { letter: "R", label: "重命名", className: "text-cx-accent" },
};

function statusMeta(status: string) {
  return STATUS_META[status] ?? STATUS_META.M;
}

function isHtml(path: string): boolean {
  return /\.html?$/i.test(path);
}

export function ChangedFilesCard({
  threadId,
  turnId,
  artifact,
  onOpenDiff,
  className,
}: {
  threadId: string;
  turnId?: string;
  artifact: DiffArtifactRef;
  /** Used when no thread id is available for the right-panel store. */
  onOpenDiff?: (request: ChangedFilesOpenRequest) => void;
  className?: string;
}) {
  const cacheKey = `${threadId}:${artifact.sha256}`;
  const rootRef = useRef<HTMLElement>(null);
  const [files, setFiles] = useState<ParsedDiffFile[] | null>(() => resolvedDiffs.get(cacheKey) ?? null);
  const [error, setError] = useState("");
  const [inView, setInView] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [showAll, setShowAll] = useState(false);
  const [closedDirs, setClosedDirs] = useState<Set<string>>(() => new Set());

  useEffect(() => {
    setFiles(resolvedDiffs.get(cacheKey) ?? null);
    setError("");
  }, [cacheKey]);

  useEffect(() => {
    const element = rootRef.current;
    if (!element || inView) return;
    if (typeof IntersectionObserver === "undefined") {
      setInView(true);
      return;
    }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        setInView(true);
        observer.disconnect();
      }
    }, { rootMargin: "240px 0px" });
    observer.observe(element);
    return () => observer.disconnect();
  }, [inView]);

  useEffect(() => {
    if (!inView || files || !threadId) return;
    let cancelled = false;
    setError("");
    loadDiff(threadId, artifact.sha256)
      .then((next) => {
        if (!cancelled) setFiles(next);
      })
      .catch((exc: unknown) => {
        if (!cancelled) setError(exc instanceof Error ? exc.message : String(exc));
      });
    return () => {
      cancelled = true;
    };
  }, [artifact.sha256, attempt, files, inView, threadId]);

  const open = useCallback((filePath?: string) => {
    const request: ChangedFilesOpenRequest = { kind: "turn", turnId, artifactSha256: artifact.sha256, filePath };
    if (threadId) chatPanel.openDiff(threadId, request);
    else onOpenDiff?.(request);
  }, [artifact.sha256, onOpenDiff, threadId, turnId]);

  const totals = useMemo(() => {
    let additions = 0;
    let deletions = 0;
    for (const file of files ?? []) {
      additions += file.additions;
      deletions += file.deletions;
    }
    return { additions, deletions };
  }, [files]);

  const rows = useMemo(() => {
    if (!files?.length) return [];
    const tree = buildTree(files);
    const compressed: TreeDir = { ...tree, dirs: new Map([...tree.dirs].map(([key, child]) => [key, compress(child)])) };
    const out: TreeRow[] = [];
    flatten(compressed, 0, closedDirs, out);
    return out;
  }, [closedDirs, files]);

  const visibleRows = useMemo(() => {
    if (showAll) return rows;
    const out: TreeRow[] = [];
    let fileCount = 0;
    for (const row of rows) {
      if (row.type === "file") {
        if (fileCount >= VISIBLE_FILES) break;
        fileCount += 1;
      }
      out.push(row);
    }
    while (out.length && out[out.length - 1].type === "dir") out.pop();
    return out;
  }, [rows, showAll]);

  if (files && files.length === 0) return null;

  const fileTotal = files?.length ?? 0;
  const hiddenFiles = fileTotal - visibleRows.filter((row) => row.type === "file").length;
  const loading = !files && !error;

  return (
    <section
      ref={rootRef}
      aria-label={files ? `${fileTotal} 个文件已更改` : "文件变更"}
      data-testid="cx-changed-files"
      data-turn-id={turnId}
      className={cn("overflow-hidden rounded-xl border border-cx-border bg-cx-elevated", className)}
    >
      <header className="flex h-11 items-center gap-2.5 pl-3.5 pr-2">
        <Icon name="fileDiff" size={15} className="shrink-0 text-cx-fg-3" />
        {files ? (
          <>
            <span className="text-[13px] font-medium text-cx-fg">
              <span className="cx-tabular">{fileTotal}</span> 个文件已更改
            </span>
            <DiffStat additions={totals.additions} deletions={totals.deletions} bar />
          </>
        ) : (
          <span className="text-[13px] font-medium text-cx-fg-3">{error ? "变更读取失败" : "正在读取变更…"}</span>
        )}
        <span className="flex-1" />
        <Button size="xs" variant="secondary" iconRight="arrowUpRight" onClick={() => open()}>
          查看变更
        </Button>
      </header>
      <div className="border-t border-cx-border-subtle px-1.5 py-1.5">
        {loading ? (
          <div className="flex flex-col gap-2.5 px-2 py-1.5" aria-busy>
            <Skeleton className="h-3 w-2/5" />
            <Skeleton className="h-3 w-3/5" />
            <Skeleton className="h-3 w-1/3" />
          </div>
        ) : error ? (
          <div className="flex items-center gap-2 px-2 py-1 text-[13px] text-cx-fg-3" role="alert">
            <Icon name="circleAlert" size={14} className="shrink-0 text-cx-danger" />
            <span className="min-w-0 flex-1 truncate" title={error}>{error}</span>
            <Button size="xs" variant="ghost" icon="retry" onClick={() => setAttempt((value) => value + 1)}>
              重试
            </Button>
          </div>
        ) : (
          <ul className="m-0 flex list-none flex-col p-0" aria-label="变更文件">
            {visibleRows.map((row) => {
              const indent = { paddingLeft: `${row.depth * 14 + 8}px` };
              if (row.type === "dir") {
                const dirPath = row.key.slice(2);
                const isOpen = !closedDirs.has(dirPath);
                return (
                  <li key={row.key}>
                    <button
                      type="button"
                      aria-expanded={isOpen}
                      onClick={() => setClosedDirs((current) => {
                        const next = new Set(current);
                        if (next.has(dirPath)) next.delete(dirPath);
                        else next.add(dirPath);
                        return next;
                      })}
                      style={indent}
                      className="flex h-7 w-full items-center gap-1.5 rounded-md pr-2 text-left text-[13px] text-cx-fg-3 transition-colors hover:bg-cx-hover hover:text-cx-fg-2"
                    >
                      <Icon
                        name="chevronRight"
                        size={12}
                        className={cn("shrink-0 transition-transform duration-150 ease-cx-out", isOpen && "rotate-90")}
                      />
                      <Icon name={isOpen ? "folderOpen" : "folder"} size={13} className="shrink-0 text-cx-fg-4" />
                      <span className="min-w-0 truncate">{row.name}</span>
                      {!isOpen ? <span className="cx-tabular text-[12px] text-cx-fg-4">{row.fileCount}</span> : null}
                    </button>
                  </li>
                );
              }
              const meta = statusMeta(row.file.status);
              return (
                <li key={row.key} className="group/file relative flex items-center">
                  <button
                    type="button"
                    onClick={() => open(row.file.path)}
                    style={{ paddingLeft: `${row.depth * 14 + 26}px` }}
                    title={row.file.oldPath ? `${row.file.oldPath} → ${row.file.path}` : row.file.path}
                    className="flex h-7 min-w-0 flex-1 items-center gap-2 rounded-md pr-2 text-left transition-colors hover:bg-cx-hover"
                  >
                    <span
                      className={cn("w-3 shrink-0 text-center font-cx-mono text-[12px] font-semibold", meta.className)}
                      title={meta.label}
                      aria-label={meta.label}
                    >
                      {meta.letter}
                    </span>
                    <span className={cn("min-w-0 flex-1 truncate text-[13px]", row.file.status === "D" ? "text-cx-fg-3 line-through decoration-cx-fg-4" : "text-cx-fg")}>
                      {row.name}
                    </span>
                    {row.file.binary ? (
                      <span className="shrink-0 text-[12px] text-cx-fg-4">二进制</span>
                    ) : (
                      <DiffStat additions={row.file.additions} deletions={row.file.deletions} className="shrink-0 text-[12px]" />
                    )}
                  </button>
                  {isHtml(row.file.path) && row.file.status !== "D" && threadId ? (
                    <IconButton
                      size="xs"
                      icon="eye"
                      label="预览页面"
                      onClick={() => chatPanel.openFile(threadId, row.file.path)}
                      className="ml-0.5 shrink-0 opacity-0 transition-opacity group-hover/file:opacity-100 focus-visible:opacity-100 [@media(hover:none)]:opacity-100"
                    />
                  ) : null}
                </li>
              );
            })}
          </ul>
        )}
        {files && hiddenFiles > 0 ? (
          <button
            type="button"
            onClick={() => setShowAll(true)}
            className="mt-0.5 flex h-7 w-full items-center gap-1.5 rounded-md px-2 text-[12px] font-medium text-cx-fg-3 transition-colors hover:bg-cx-hover hover:text-cx-fg"
          >
            <Icon name="chevronsUpDown" size={12} />
            展开全部 {fileTotal} 个文件
          </button>
        ) : files && showAll && fileTotal > VISIBLE_FILES ? (
          <button
            type="button"
            onClick={() => setShowAll(false)}
            className="mt-0.5 flex h-7 w-full items-center gap-1.5 rounded-md px-2 text-[12px] font-medium text-cx-fg-3 transition-colors hover:bg-cx-hover hover:text-cx-fg"
          >
            <Icon name="chevronsDownUp" size={12} />
            收起
          </button>
        ) : null}
      </div>
    </section>
  );
}
