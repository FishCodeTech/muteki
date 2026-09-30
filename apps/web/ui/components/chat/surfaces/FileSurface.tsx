"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { Button, CopyButton, EmptyState, IconButton, Skeleton, toast } from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { chatPanel, type ChatSurface } from "@/lib/chatPanelStore";
import { type WorkspaceFilePreview } from "@/lib/resourcePreview";
import {
  downloadAuthenticatedWorkspaceRaw,
  fetchWorkspaceRaw,
  openAuthenticatedWorkspaceRaw,
  authenticatedResourceHttpError,
} from "@/lib/authenticatedResource";
import { TypedResourcePreview, workspacePreviewToProps } from "@/components/conversation/TypedResourcePreview";
import { formatLineCitation } from "@/components/chat/preview/CodeViewer";
import { PathBreadcrumb } from "./PathBreadcrumb";
import { SurfaceToolbar, surfaceJson } from "./shared";
import { NativeWorkspaceFileActions } from "@/components/NativeWorkspaceFileActions";

type FileSurfaceModel = Extract<ChatSurface, { kind: "file" }>;

const STALE_AFTER_MS = 8_000;

export function FileLoadingSkeleton() {
  return (
    <div className="flex flex-col gap-2.5 px-4 py-4" aria-busy>
      {["w-[72%]", "w-[54%]", "w-[88%]", "w-[40%]", "w-[66%]", "w-[80%]", "w-[48%]", "w-[60%]"].map((width, index) => (
        <Skeleton key={index} className={`h-3 ${width}`} />
      ))}
    </div>
  );
}

function needsBlobPreview(preview: WorkspaceFilePreview | null): boolean {
  const kind = String(preview?.preview_kind || "");
  return kind === "image" || kind === "pdf";
}

export function FileSurface({ surface, threadId, view, active, hasWorkspace, onCiteToComposer }: SurfaceProps<FileSurfaceModel>) {
  const { path, line } = surface;
  const [preview, setPreview] = useState<WorkspaceFilePreview | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [openingRaw, setOpeningRaw] = useState(false);
  const [blobUrl, setBlobUrl] = useState<string | null>(null);
  const fetchedAt = useRef(0);
  const seq = useRef(0);
  const blobUrlRef = useRef<string | null>(null);

  const load = useCallback(async () => {
    const id = ++seq.current;
    setLoading(true);
    setError("");
    try {
      const data = await surfaceJson<WorkspaceFilePreview>(
        `/api/threads/${threadId}/workspace/file?${new URLSearchParams({ path }).toString()}`,
      );
      if (id !== seq.current) return;
      setPreview(data);
      fetchedAt.current = Date.now();
    } catch (exc) {
      if (id !== seq.current) return;
      setPreview(null);
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      if (id === seq.current) setLoading(false);
    }
  }, [path, threadId]);

  useEffect(() => {
    if (hasWorkspace) void load();
  }, [hasWorkspace, load]);

  useEffect(() => {
    if (active && hasWorkspace && fetchedAt.current && Date.now() - fetchedAt.current > STALE_AFTER_MS) void load();
    // Refresh stale content when the tab is shown again.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active]);

  // Image/PDF need bytes with Bearer; bare /api/.../raw URLs 401 in <img>/<iframe>.
  useEffect(() => {
    let cancelled = false;
    if (blobUrlRef.current) {
      URL.revokeObjectURL(blobUrlRef.current);
      blobUrlRef.current = null;
    }
    setBlobUrl(null);
    if (!preview || !needsBlobPreview(preview)) return;

    void (async () => {
      try {
        const response = await fetchWorkspaceRaw(threadId, path);
        if (!response.ok) {
          if (!cancelled) {
            setError(authenticatedResourceHttpError(response.status, "read"));
          }
          return;
        }
        const blob = await response.blob();
        if (cancelled) return;
        const url = URL.createObjectURL(blob);
        blobUrlRef.current = url;
        setBlobUrl(url);
      } catch (exc) {
        if (!cancelled) {
          setError(exc instanceof Error ? exc.message : "读取文件失败");
        }
      }
    })();

    return () => {
      cancelled = true;
      if (blobUrlRef.current) {
        URL.revokeObjectURL(blobUrlRef.current);
        blobUrlRef.current = null;
      }
    };
  }, [preview, threadId, path]);

  const handleOpenRaw = useCallback(async () => {
    setOpeningRaw(true);
    try {
      const result = desktopChatBridge()
        ? await downloadAuthenticatedWorkspaceRaw(threadId, path)
        : await openAuthenticatedWorkspaceRaw(threadId, path);
      if (!result.ok) {
        toast({ title: result.message, tone: "danger", icon: "circleAlert" });
      }
    } finally {
      setOpeningRaw(false);
    }
  }, [threadId, path]);

  const handleDownload = useCallback(() => {
    void (async () => {
      const result = await downloadAuthenticatedWorkspaceRaw(threadId, path);
      if (!result.ok) {
        toast({ title: result.message, tone: "danger", icon: "circleAlert" });
      }
    })();
  }, [threadId, path]);

  if (!hasWorkspace) {
    return <EmptyState icon="fileCode" title="文件不可用" description="当前会话未绑定工作区。" />;
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="file-surface" data-path={path}>
      <SurfaceToolbar className="gap-0.5 pl-1.5 pr-1.5">
        <PathBreadcrumb
          path={path}
          lastIsFile
          onSelect={(dir) => chatPanel.revealInFiles(threadId, { kind: "directory", path: dir })}
        />
        <div className="flex shrink-0 items-center gap-0.5 pl-1">
          <IconButton icon="refresh" label="重新读取" loading={loading && Boolean(preview)} onClick={() => void load()} />
          <CopyButton text={path} label="复制路径" size="sm" />
          <IconButton icon="folderTree" label="在文件中显示" onClick={() => chatPanel.revealInFiles(threadId, { kind: "file", path, line })} />
          <IconButton
            icon={desktopChatBridge() ? "download" : "externalLink"}
            label={desktopChatBridge() ? "下载原文件（保留文件名）" : "打开原始文件"}
            loading={openingRaw}
            data-testid="file-surface-open-raw"
            onClick={() => void handleOpenRaw()}
          />
        </div>
      </SurfaceToolbar>
      {view.workspace && <NativeWorkspaceFileActions threadId={threadId} workspaceId={view.workspace.workspace_id} serviceRoot={view.workspace.root_path} relativePath={preview?.path || path} />}
      <div className="flex min-h-0 flex-1 flex-col">
        {loading && !preview ? (
          <FileLoadingSkeleton />
        ) : error ? (
          <EmptyState
            icon="circleAlert"
            title="文件读取失败"
            description={error}
            action={<Button size="sm" variant="secondary" icon="refresh" onClick={() => void load()}>重试</Button>}
          />
        ) : preview ? (
          <TypedResourcePreview
            {...workspacePreviewToProps(preview, {
              objectUrl: blobUrl,
              onDownload: handleDownload,
            })}
            focusLine={line ?? preview.line}
            fill
            testId="workspace-typed-preview"
            onCiteLines={onCiteToComposer ? (range, excerpt) => onCiteToComposer(formatLineCitation(preview.path, range, excerpt)) : undefined}
          />
        ) : null}
      </div>
    </div>
  );
}
