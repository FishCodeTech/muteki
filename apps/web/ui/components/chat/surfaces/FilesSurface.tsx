"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import {
  Button,
  Callout,
  EmptyState,
  IconButton,
  ScrollArea,
  SearchInput,
  SegmentedControl,
  Skeleton,
  Spinner,
  toast,
} from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { chatPanel, useFileRequest } from "@/lib/chatPanelStore";
import { planFileReveal } from "@/lib/fileRevealPlan";
import { type WorkspaceFilePreview } from "@/lib/resourcePreview";
import {
  authenticatedResourceHttpError,
  downloadAuthenticatedWorkspaceRaw,
  fetchWorkspaceRaw,
} from "@/lib/authenticatedResource";
import { TypedResourcePreview, workspacePreviewToProps } from "@/components/conversation/TypedResourcePreview";
import { formatLineCitation } from "@/components/chat/preview/CodeViewer";
import { PathBreadcrumb } from "./PathBreadcrumb";
import { SurfaceToolbar, fileIconFor, formatBytes, parentPath, surfaceJson } from "./shared";

type SearchMode = "browse" | "files" | "content";
type FileEntry = { name: string; path: string; kind: "directory" | "file"; size?: number | null };
type ContentHit = { path: string; line: number; snippet: string };

function Highlight({ text, query }: { text: string; query: string }) {
  const q = query.trim();
  if (!q) return <>{text}</>;
  const index = text.toLowerCase().indexOf(q.toLowerCase());
  if (index < 0) return <>{text}</>;
  return (
    <>
      {text.slice(0, index)}
      <mark className="rounded-[3px] bg-cx-warning-soft px-px text-cx-fg">{text.slice(index, index + q.length)}</mark>
      {text.slice(index + q.length)}
    </>
  );
}

function ListSkeleton() {
  return (
    <div className="flex flex-col gap-1 p-2" aria-busy>
      {["w-[46%]", "w-[62%]", "w-[38%]", "w-[54%]", "w-[70%]", "w-[42%]", "w-[58%]"].map((width, index) => (
        <div key={index} className="flex h-8 items-center gap-2.5 px-2">
          <Skeleton className="size-4 rounded" />
          <Skeleton className={cn("h-3", width)} />
        </div>
      ))}
    </div>
  );
}

export function FilesSurface({ threadId, hasWorkspace, onCiteToComposer }: SurfaceProps) {
  const [path, setPath] = useState("");
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [mode, setMode] = useState<SearchMode>("browse");
  const [query, setQuery] = useState("");
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState("");
  const [fileResults, setFileResults] = useState<FileEntry[]>([]);
  const [contentHits, setContentHits] = useState<ContentHit[]>([]);
  const [truncated, setTruncated] = useState(false);
  const [ignoredDirs, setIgnoredDirs] = useState<string[]>([]);
  const [preview, setPreview] = useState<WorkspaceFilePreview | null>(null);
  const [previewLoading, setPreviewLoading] = useState(false);
  const [previewError, setPreviewError] = useState("");
  const [previewBlobUrl, setPreviewBlobUrl] = useState<string | null>(null);
  const [focusIndex, setFocusIndex] = useState(0);
  const listRef = useRef<HTMLDivElement | null>(null);
  const previewBlobUrlRef = useRef<string | null>(null);
  const surfaceRef = useRef<HTMLDivElement | null>(null);
  const [selectedFilePath, setSelectedFilePath] = useState("");
  const previewEpochRef = useRef(0);
  const directoryEpochRef = useRef(0);
  const wasShowingPreviewRef = useRef(false);
  const showPreview = Boolean(preview || previewLoading || previewError);
  const request = useFileRequest(threadId);

  const loadDir = useCallback(async (nextPath: string) => {
    const epoch = ++directoryEpochRef.current;
    setSelectedFilePath("");
    setLoading(true);
    setError("");
    try {
      const data = await surfaceJson<{ path: string; entries: FileEntry[] }>(
        `/api/threads/${threadId}/workspace/files?path=${encodeURIComponent(nextPath)}`,
      );
      if (epoch !== directoryEpochRef.current) return;
      setPath(data.path);
      setEntries(data.entries);
      setFocusIndex(0);
    } catch (exc) {
      if (epoch !== directoryEpochRef.current) return;
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      if (epoch === directoryEpochRef.current) setLoading(false);
    }
  }, [threadId]);

  const openFile = useCallback(async (filePath: string, line?: number) => {
    const epoch = ++previewEpochRef.current;
    setSelectedFilePath(filePath);
    setPreviewLoading(true);
    setPreviewError("");
    try {
      const qs = new URLSearchParams({ path: filePath });
      if (line && line > 0) qs.set("line", String(line));
      const data = await surfaceJson<WorkspaceFilePreview>(`/api/threads/${threadId}/workspace/file?${qs.toString()}`);
      if (epoch !== previewEpochRef.current) return;
      setPreview({ ...data, line: line ?? data.line });
    } catch (exc) {
      if (epoch !== previewEpochRef.current) return;
      setPreview(null);
      setPreviewError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      if (epoch === previewEpochRef.current) setPreviewLoading(false);
    }
  }, [threadId]);

  const closePreview = useCallback(() => {
    previewEpochRef.current += 1;
    setPreview(null);
    setPreviewError("");
    setPreviewLoading(false);
  }, []);

  useEffect(() => {
    if (wasShowingPreviewRef.current === showPreview) return;
    wasShowingPreviewRef.current = showPreview;
    const root = surfaceRef.current;
    if (!root || root.getBoundingClientRect().width >= 720) return;
    if (showPreview) {
      root.querySelector<HTMLButtonElement>('[aria-label="返回文件列表"]')?.focus();
    } else {
      Array.from(root.querySelectorAll<HTMLButtonElement>("[data-file-row]"))
        .find((row) => row.dataset.filePath === selectedFilePath)?.focus();
    }
  }, [showPreview, selectedFilePath]);

  useEffect(() => {
    closePreview();
    setEntries([]);
    setPath("");
    setError("");
    if (hasWorkspace) void loadDir("");
    else setLoading(false);
    return () => {
      directoryEpochRef.current += 1;
      previewEpochRef.current += 1;
    };
  }, [hasWorkspace, loadDir, closePreview]);

  useEffect(() => {
    if (!request || !hasWorkspace) return;
    setMode("browse");
    setQuery("");
    const plan = planFileReveal(request);
    if (plan.mode === "directory") {
      closePreview();
      void loadDir(plan.loadPath);
    } else {
      void loadDir(plan.loadPath);
      void openFile(plan.openPath, plan.line);
    }
    chatPanel.consumeFileRequest(threadId, request.nonce);
  }, [request, hasWorkspace, loadDir, openFile, closePreview, threadId]);

  // Image/PDF preview bytes need Bearer; never point <img>/<iframe> at bare raw URLs (#223).
  useEffect(() => {
    let cancelled = false;
    if (previewBlobUrlRef.current) {
      URL.revokeObjectURL(previewBlobUrlRef.current);
      previewBlobUrlRef.current = null;
    }
    setPreviewBlobUrl(null);
    const kind = String(preview?.preview_kind || "");
    if (!preview || (kind !== "image" && kind !== "pdf")) return;

    void (async () => {
      try {
        const response = await fetchWorkspaceRaw(threadId, preview.path);
        if (!response.ok) {
          if (!cancelled) {
            setPreviewError(authenticatedResourceHttpError(response.status, "read"));
          }
          return;
        }
        const blob = await response.blob();
        if (cancelled) return;
        const url = URL.createObjectURL(blob);
        previewBlobUrlRef.current = url;
        setPreviewBlobUrl(url);
      } catch (exc) {
        if (!cancelled) {
          setPreviewError(exc instanceof Error ? exc.message : "读取文件失败");
        }
      }
    })();

    return () => {
      cancelled = true;
      if (previewBlobUrlRef.current) {
        URL.revokeObjectURL(previewBlobUrlRef.current);
        previewBlobUrlRef.current = null;
      }
    };
  }, [preview, threadId]);

  useEffect(() => {
    let cancelled = false;
    setSearching(false);
    setFileResults([]);
    setContentHits([]);
    setTruncated(false);
    setIgnoredDirs([]);
    setSearchError("");
    if (mode === "browse" || !hasWorkspace) return;
    const q = query.trim();
    if (!q) {
      setFileResults([]);
      setContentHits([]);
      setTruncated(false);
      setSearchError("");
      return;
    }
    const timer = window.setTimeout(async () => {
      setSearching(true);
      setSearchError("");
      try {
        if (mode === "files") {
          const data = await surfaceJson<{ results: FileEntry[]; truncated: boolean; ignored_dirs?: string[] }>(
            `/api/threads/${threadId}/workspace/search/files?q=${encodeURIComponent(q)}`,
          );
          if (cancelled) return;
          setFileResults(data.results);
          setTruncated(data.truncated);
          setIgnoredDirs(data.ignored_dirs ?? []);
        } else {
          const data = await surfaceJson<{ results: ContentHit[]; truncated: boolean }>(
            `/api/threads/${threadId}/workspace/search/content?q=${encodeURIComponent(q)}`,
          );
          if (cancelled) return;
          setContentHits(data.results);
          setTruncated(data.truncated);
        }
      } catch (exc) {
        if (cancelled) return;
        setSearchError(exc instanceof Error ? exc.message : String(exc));
      } finally {
        if (!cancelled) setSearching(false);
      }
    }, 280);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [query, mode, threadId, hasWorkspace]);

  const visibleEntries = useMemo(() => {
    if (mode !== "browse" || !query.trim()) return entries;
    const q = query.toLowerCase();
    return entries.filter((entry) => entry.name.toLowerCase().includes(q));
  }, [entries, mode, query]);

  const activateEntry = (entry: FileEntry) => {
    if (entry.kind === "directory") {
      setMode("browse");
      setQuery("");
      void loadDir(entry.path);
    } else {
      void openFile(entry.path);
    }
  };

  // Keep the row mounted for the OS's double-click interval. Narrow panels
  // use explicit preview/open actions; wide panels already have a stable list column.
  const onFileRowClick = useCallback((filePath: string) => {
    setSelectedFilePath(filePath);
    if ((surfaceRef.current?.getBoundingClientRect().width ?? 0) >= 720) {
      void openFile(filePath);
    }
  }, [openFile]);

  const onFileRowDoubleClick = useCallback((filePath: string) => {
    chatPanel.openFile(threadId, filePath);
  }, [threadId]);

  const onListKeyDown = (event: React.KeyboardEvent) => {
    const rows = listRef.current?.querySelectorAll<HTMLElement>("[data-file-row]");
    if (!rows?.length) return;
    let next = focusIndex;
    if (event.key === "ArrowDown") next = Math.min(rows.length - 1, focusIndex + 1);
    else if (event.key === "ArrowUp") next = Math.max(0, focusIndex - 1);
    else if (event.key === "Backspace" && mode === "browse" && path) {
      event.preventDefault();
      void loadDir(parentPath(path));
      return;
    } else return;
    event.preventDefault();
    setFocusIndex(next);
    rows[next]?.focus();
  };

  const handlePreviewDownload = useCallback(() => {
    if (!preview) return;
    void (async () => {
      const result = await downloadAuthenticatedWorkspaceRaw(threadId, preview.path);
      if (!result.ok) {
        toast({ title: result.message, tone: "danger", icon: "circleAlert" });
      }
    })();
  }, [preview, threadId]);

  if (!hasWorkspace) {
    return <EmptyState icon="folderTree" title="文件不可用" description="当前会话未绑定工作区，绑定后可在这里浏览和搜索文件。" />;
  }

  const row = (entry: FileEntry, index: number, subtitle?: string) => {
    const selected = selectedFilePath === entry.path;
    return (
      <button
        key={entry.path}
        type="button"
        data-file-row=""
        data-file-path={entry.path}
        data-testid={mode === "files" ? `workspace-search-${entry.name}` : `workspace-file-${entry.name}`}
        tabIndex={index === focusIndex ? 0 : -1}
        onFocus={() => setFocusIndex(index)}
        onClick={() => {
          if (entry.kind === "directory") activateEntry(entry);
          else onFileRowClick(entry.path);
        }}
        onDoubleClick={() => {
          if (entry.kind === "file") onFileRowDoubleClick(entry.path);
        }}
        title={entry.path}
        className={cn(
          "group flex min-h-8 w-full items-center gap-2.5 rounded-lg px-2 py-1 text-left outline-none transition-colors",
          selected ? "bg-cx-selected text-cx-fg" : "text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg focus-visible:bg-cx-hover",
        )}
      >
        <Icon
          name={entry.kind === "directory" ? "folder" : fileIconFor(entry.name)}
          size={15}
          className={cn("shrink-0", entry.kind === "directory" ? "text-cx-accent" : "text-cx-fg-4")}
        />
        <span className="flex min-w-0 flex-1 flex-col">
          <span className="truncate text-[13px]"><Highlight text={entry.name} query={mode === "browse" ? query : mode === "files" ? query : ""} /></span>
          {subtitle ? <span className="truncate font-cx-mono text-[11px] text-cx-fg-4">{subtitle}</span> : null}
        </span>
        {entry.kind === "directory" ? (
          <Icon name="chevronRight" size={13} className="shrink-0 text-cx-fg-4" />
        ) : (
          <span className="cx-tabular shrink-0 text-[11.5px] text-cx-fg-4">{formatBytes(entry.size)}</span>
        )}
      </button>
    );
  };

  const listBody = mode === "browse" ? (
    loading && !entries.length ? (
      <ListSkeleton />
    ) : error ? (
      <EmptyState
        compact
        icon="circleAlert"
        title="目录读取失败"
        description={error}
        action={<Button size="sm" variant="secondary" icon="refresh" onClick={() => void loadDir(path)}>重试</Button>}
      />
    ) : !visibleEntries.length ? (
      <EmptyState compact icon={query ? "search" : "folder"} title={query ? "没有匹配的文件" : "空目录"} />
    ) : (
      <div className="flex flex-col p-1.5">
        {path ? (
          <button
            type="button"
            onClick={() => void loadDir(parentPath(path))}
            className="flex h-8 items-center gap-2.5 rounded-lg px-2 text-[13px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg"
          >
            <Icon name="arrowUp" size={14} />
            上一级
          </button>
        ) : null}
        {visibleEntries.map((entry, index) => row(entry, index))}
      </div>
    )
  ) : searching ? (
    <div className="flex items-center justify-center gap-2 py-10 text-[12.5px] text-cx-fg-3"><Spinner size={13} />正在搜索…</div>
  ) : searchError ? (
    <div className="p-3"><Callout tone="danger">{searchError}</Callout></div>
  ) : !query.trim() ? (
    <EmptyState compact icon={mode === "files" ? "fileSearch" : "textSearch"} title={mode === "files" ? "跨目录搜索文件名" : "搜索文件正文"} description={mode === "content" ? "跳过二进制与超大文件。" : undefined} />
  ) : mode === "files" ? (
    fileResults.length ? (
      <div className="flex flex-col p-1.5">
        {fileResults.map((entry, index) => row(entry, index, parentPath(entry.path) || "工作区根目录"))}
      </div>
    ) : (
      <EmptyState compact icon="search" title="没有匹配的文件" description={`工作区中没有与「${query}」匹配的文件名。`} />
    )
  ) : contentHits.length ? (
    <div className="flex flex-col p-1.5">
      {contentHits.map((hit, index) => (
        <button
          key={`${hit.path}:${hit.line}:${index}`}
          type="button"
          data-file-row=""
          data-file-path={hit.path}
          tabIndex={index === focusIndex ? 0 : -1}
          onFocus={() => setFocusIndex(index)}
          onClick={() => void openFile(hit.path, hit.line)}
          className={cn(
            "flex w-full flex-col gap-0.5 rounded-lg px-2 py-1.5 text-left outline-none transition-colors hover:bg-cx-hover focus-visible:bg-cx-hover",
            preview?.path === hit.path && preview?.line === hit.line && "bg-cx-selected",
          )}
        >
          <span className="flex min-w-0 items-center gap-1.5 text-[12px]">
            <Icon name={fileIconFor(hit.path)} size={13} className="shrink-0 text-cx-fg-4" />
            <span className="min-w-0 truncate font-medium text-cx-fg-2">{hit.path}</span>
            <span className="cx-tabular shrink-0 font-cx-mono text-cx-fg-4">:{hit.line}</span>
          </span>
          <span className="truncate pl-5 font-cx-mono text-[12px] text-cx-fg-3">
            <Highlight text={hit.snippet.trim()} query={query} />
          </span>
        </button>
      ))}
    </div>
  ) : (
    <EmptyState compact icon="search" title="没有正文命中" description={`工作区中没有包含「${query}」的文本。`} />
  );

  return (
    <div ref={surfaceRef} className="flex min-h-0 flex-1 flex-col" data-testid="workspace-files-surface">
      <SurfaceToolbar className="pl-1.5">
        <PathBreadcrumb path={path} onSelect={(dir) => { setMode("browse"); setQuery(""); void loadDir(dir); }} />
        <span className="shrink-0 rounded-md bg-cx-hover px-1.5 text-[11px] leading-5 text-cx-fg-4" aria-label="文件来自运行环境">运行环境</span>
        <IconButton icon="refresh" label="刷新" loading={loading && entries.length > 0} onClick={() => void loadDir(path)} />
      </SurfaceToolbar>
      <div className="flex shrink-0 items-center gap-1.5 border-b border-cx-border-subtle px-2 py-1.5">
        <SearchInput
          value={query}
          onValueChange={setQuery}
          aria-label="文件搜索"
          placeholder={mode === "content" ? "搜索正文内容…" : mode === "files" ? "跨目录搜索文件名…" : "过滤当前目录"}
          className="min-w-0 flex-1"
        />
        <SegmentedControl<SearchMode>
          size="sm"
          value={mode}
          onChange={setMode}
          ariaLabel="搜索范围"
          options={[
            { value: "browse", icon: "folder", ariaLabel: "当前目录" },
            { value: "files", icon: "fileSearch", ariaLabel: "文件名搜索" },
            { value: "content", icon: "textSearch", ariaLabel: "正文搜索" },
          ]}
        />
      </div>
      {mode === "files" && ignoredDirs.length ? (
        <p className="shrink-0 truncate border-b border-cx-border-subtle px-3 py-1 text-[11.5px] text-cx-fg-4" title={`已忽略：${ignoredDirs.join(", ")}`}>
          已忽略 {ignoredDirs.length} 个目录（node_modules 等）{truncated ? " · 结果已截断" : ""}
        </p>
      ) : truncated ? (
        <p className="shrink-0 border-b border-cx-border-subtle px-3 py-1 text-[11.5px] text-cx-fg-4">结果已截断，请缩小搜索范围</p>
      ) : null}

      <div className="flex min-h-0 flex-1 flex-col @[720px]/panel:flex-row">
        <ScrollArea
          ref={listRef}
          onKeyDown={onListKeyDown}
          className={cn(
            "@[720px]/panel:w-[min(320px,42%)] @[720px]/panel:shrink-0 @[720px]/panel:border-r @[720px]/panel:border-cx-border-subtle",
            showPreview ? "hidden @[720px]/panel:block" : "flex-1 @[720px]/panel:flex-none",
          )}
        >
          {listBody}
        </ScrollArea>
        {!showPreview ? (
          <div className="flex h-10 shrink-0 items-center gap-1.5 border-t border-cx-border-subtle px-2 @[720px]/panel:hidden" data-testid="workspace-file-selection-actions">
            <span className="min-w-0 flex-1 truncate text-[11.5px] text-cx-fg-3" title={selectedFilePath || undefined}>
              {selectedFilePath.split("/").at(-1) || "双击文件可独立打开"}
            </span>
            <Button size="xs" variant="ghost" disabled={!selectedFilePath} aria-label="预览选中文件" onClick={() => void openFile(selectedFilePath)}>预览</Button>
            <Button size="xs" variant="secondary" disabled={!selectedFilePath} aria-label="在新标签中打开选中文件" onClick={() => chatPanel.openFile(threadId, selectedFilePath)}>新标签</Button>
          </div>
        ) : null}
        {showPreview ? (
          <div className="flex min-h-0 min-w-0 flex-1 flex-col" data-testid="workspace-file-preview">
            <div className="flex h-9 shrink-0 items-center gap-1 border-b border-cx-border-subtle pl-1.5 pr-1.5">
              <IconButton icon="arrowLeft" label="返回文件列表" size="xs" className="@[720px]/panel:hidden" onClick={closePreview} />
              <Icon name={fileIconFor(preview?.path || "")} size={13} className="ml-1 shrink-0 text-cx-fg-4" />
              <span className="min-w-0 flex-1 truncate font-cx-mono text-[12px] text-cx-fg-2" title={preview?.path}>{preview?.path}</span>
              {preview ? (
                <>
                  <IconButton icon="externalLink" label="在新标签中打开" size="xs" onClick={() => chatPanel.openFile(threadId, preview.path, preview.line ?? undefined)} />
                  <IconButton icon="x" label="关闭预览" size="xs" className="hidden @[720px]/panel:inline-flex" onClick={closePreview} />
                </>
              ) : null}
            </div>
            <div className="flex min-h-0 flex-1 flex-col">
              {previewLoading && !preview ? (
                <ListSkeleton />
              ) : previewError ? (
                <EmptyState compact icon="circleAlert" title="文件读取失败" description={previewError} />
              ) : preview ? (
                <TypedResourcePreview
                  {...workspacePreviewToProps(preview, {
                    objectUrl: previewBlobUrl,
                    onDownload: handlePreviewDownload,
                  })}
                  focusLine={preview.line}
                  fill
                  testId="workspace-typed-preview"
                  onCiteLines={onCiteToComposer ? (range, excerpt) => onCiteToComposer(formatLineCitation(preview.path, range, excerpt)) : undefined}
                />
              ) : null}
            </div>
          </div>
        ) : (
          <div className="hidden min-h-0 min-w-0 flex-1 items-center justify-center @[720px]/panel:flex">
            <EmptyState compact icon="fileCode" title="选择文件预览" description="双击文件可在独立标签中打开。" />
          </div>
        )}
      </div>
    </div>
  );
}
