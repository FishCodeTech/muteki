"use client";

/**
 * C17 typed resource preview — image / PDF / markdown / code / HTML sandbox /
 * binary+download. Shared by DetailsDrawer and the chat File/Files surfaces.
 * Not the live-URL preview surface (iframe to a running app).
 */

import React, { useEffect, useMemo, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Button, IconButton, languageLabel } from "@/components/chat/ui";
import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";
import { CodeViewer, type LineRange } from "@/components/chat/preview/CodeViewer";
import { languageFromPath } from "@/lib/chatHighlighter";
import {
  type PreviewKind,
  previewKindFromName,
  type WorkspaceFilePreview,
} from "@/lib/resourcePreview";

export interface TypedResourcePreviewProps {
  name: string;
  mediaType?: string | null;
  previewKind?: PreviewKind | string | null;
  content?: string | null;
  message?: string | null;
  size?: number | null;
  maxPreviewBytes?: number | null;
  /** Blob / data URL for image, PDF preview (must already be auth-safe). */
  objectUrl?: string | null;
  /** Blob URL for <a download>; prefer onDownload when the source needs Bearer. */
  downloadUrl?: string | null;
  /** Authenticated download path (apiFetch → blob). Wins over bare downloadUrl. */
  onDownload?: (() => void) | null;
  focusLine?: number | null;
  className?: string;
  testId?: string;
  /** Fill the parent's height and scroll internally (panel surfaces). */
  fill?: boolean;
  /** Enables line selection in code views; called with the chosen range. */
  onCiteLines?: (range: LineRange, excerpt: string) => void;
  /** Extra controls appended to the preview toolbar. */
  toolbarActions?: ReactNode;
}

type ViewMode = "render" | "source";

function triggerDownload(url: string, filename: string) {
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  anchor.rel = "noopener";
  anchor.click();
}

function formatSize(size?: number | null): string | null {
  if (typeof size !== "number" || size < 0) return null;
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
}

const KNOWN_KINDS = new Set<string>(["image", "pdf", "markdown", "html", "code", "text", "binary", "too_large", "missing"]);

/** Backend kinds are open strings; unknown ones degrade to text/binary. */
function normalizeKind(kind: string, content: string | null | undefined): PreviewKind {
  if (KNOWN_KINDS.has(kind)) return kind as PreviewKind;
  return content != null ? "text" : "binary";
}

function kindLabel(kind: PreviewKind, language: string | null): string {
  switch (kind) {
    case "image":
      return "图片";
    case "pdf":
      return "PDF";
    case "markdown":
      return "Markdown";
    case "html":
      return "HTML";
    case "code":
      return languageLabel(language);
    case "text":
      return "纯文本";
    case "binary":
      return "二进制";
    case "too_large":
      return "文件过大";
    case "missing":
      return "不可用";
    default: {
      const exhaustive: never = kind;
      return String(exhaustive);
    }
  }
}

function kindIcon(kind: PreviewKind): IconName {
  switch (kind) {
    case "image":
      return "image";
    case "pdf":
      return "book";
    case "markdown":
      return "pilcrow";
    case "html":
      return "globe";
    case "code":
      return "fileCode";
    case "text":
      return "file";
    case "binary":
      return "package";
    case "too_large":
      return "alert";
    case "missing":
      return "circleAlert";
    default: {
      const exhaustive: never = kind;
      return exhaustive;
    }
  }
}

function ModeToggle({ mode, onChange }: { mode: ViewMode; onChange: (mode: ViewMode) => void }) {
  const option = (value: ViewMode, label: string, testId: string) => (
    <button
      type="button"
      role="radio"
      aria-checked={mode === value}
      data-active={mode === value}
      data-testid={testId}
      onClick={() => onChange(value)}
      className={cn(
        "inline-flex h-5 items-center rounded-md px-2 text-[12px] font-medium transition-colors duration-150",
        mode === value
          ? "bg-cx-elevated text-cx-fg shadow-[0_1px_2px_hsl(var(--cx-shadow-color)/0.12),0_0_0_1px_var(--cx-border-subtle)]"
          : "text-cx-fg-3 hover:text-cx-fg",
      )}
    >
      {label}
    </button>
  );
  return (
    <div role="radiogroup" aria-label="预览模式" className="inline-flex h-6 items-center gap-0.5 rounded-lg bg-cx-hover p-0.5">
      {option("render", "预览", "typed-preview-mode-render")}
      {option("source", "源码", "typed-preview-mode-source")}
    </div>
  );
}

function Placeholder({
  icon,
  tone = "neutral",
  title,
  message,
  action,
  testId,
}: {
  icon: IconName;
  tone?: "neutral" | "warning";
  title: string;
  message?: string | null;
  action?: ReactNode;
  testId: string;
}) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-3 px-6 py-12 text-center" data-testid={testId}>
      <span className={cn(
        "grid size-11 place-items-center rounded-2xl",
        tone === "warning" ? "bg-cx-warning-soft text-cx-warning" : "bg-cx-hover text-cx-fg-3",
      )}>
        <Icon name={icon} size={20} />
      </span>
      <div className="flex max-w-[340px] flex-col gap-1">
        <p className="text-[14px] font-medium text-cx-fg">{title}</p>
        {message ? <p className="text-[13px] leading-5 text-cx-fg-3">{message}</p> : null}
      </div>
      {action ? <div className="mt-1">{action}</div> : null}
    </div>
  );
}

function ImagePreview({ src, name, onMeasure }: { src: string; name: string; onMeasure: (size: string) => void }) {
  const [actual, setActual] = useState(false);
  return (
    <div className="cx-checker cx-scroll flex min-h-[240px] flex-1 overflow-auto" data-testid="typed-preview-image">
      <div className={cn("m-auto p-6", actual ? "w-max" : "flex max-h-full max-w-full items-center justify-center")}>
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img
          src={src}
          alt={name}
          onLoad={(event) => onMeasure(`${event.currentTarget.naturalWidth} × ${event.currentTarget.naturalHeight}`)}
          onClick={() => setActual((value) => !value)}
          className={cn(
            "rounded-md shadow-cx-md ring-1 ring-cx-border-subtle",
            actual ? "max-w-none cursor-zoom-out" : "max-h-full max-w-full cursor-zoom-in object-contain",
          )}
        />
      </div>
    </div>
  );
}

export function TypedResourcePreview({
  name,
  mediaType,
  previewKind,
  content,
  message,
  size,
  maxPreviewBytes,
  objectUrl,
  downloadUrl,
  onDownload = null,
  focusLine,
  className = "",
  testId = "typed-resource-preview",
  fill = false,
  onCiteLines,
  toolbarActions,
}: TypedResourcePreviewProps) {
  const kind = normalizeKind(previewKind || previewKindFromName(name, mediaType), content);
  const renderable = kind === "markdown" || kind === "html";
  const [mode, setMode] = useState<ViewMode>(renderable && !focusLine ? "render" : "source");
  const [wrap, setWrap] = useState(kind === "text" || kind === "markdown");
  const [imageSize, setImageSize] = useState("");

  useEffect(() => {
    setMode(renderable && !focusLine ? "render" : "source");
    setImageSize("");
    // Reset only when the resource identity changes, not on focus-line hops.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, name]);

  const language = useMemo(() => languageFromPath(name), [name]);
  const sizeLabel = formatSize(size);
  const showSource = content != null && (!renderable || mode === "source");
  const codeHeight = fill ? undefined : 560;

  const canDownload = Boolean(onDownload || downloadUrl);
  const download = () => {
    if (onDownload) {
      onDownload();
      return;
    }
    if (!downloadUrl) return;
    triggerDownload(downloadUrl, name.split("/").pop() || "download");
  };

  const meta = [
    mediaType && kind !== "code" && kind !== "text" ? mediaType : null,
    sizeLabel,
    imageSize || null,
    typeof maxPreviewBytes === "number" && (kind === "too_large" || kind === "binary") ? `预览上限 ${(maxPreviewBytes / 1024).toFixed(0)} KB` : null,
  ].filter(Boolean);

  const toolbar = (
    <div className="flex h-9 shrink-0 items-center gap-2 border-b border-cx-border-subtle pl-3 pr-1.5" data-testid="typed-preview-toolbar">
      <span className="flex min-w-0 flex-1 items-center gap-2 text-[12px]">
        <span className="inline-flex shrink-0 items-center gap-1.5 font-medium text-cx-fg-2">
          <Icon name={kindIcon(kind)} size={13} className="text-cx-fg-3" />
          {kindLabel(kind, language)}
        </span>
        {meta.length ? <span className="cx-tabular truncate text-cx-fg-4">{meta.join(" · ")}</span> : null}
      </span>
      <span className="flex shrink-0 items-center gap-1">
        {renderable && content != null ? <ModeToggle mode={mode} onChange={setMode} /> : null}
        {showSource ? (
          <IconButton size="xs" icon="wrapText" label={wrap ? "不换行" : "自动换行"} active={wrap} onClick={() => setWrap((value) => !value)} />
        ) : null}
        {toolbarActions}
        {canDownload ? (
          <IconButton size="xs" icon="download" label="下载原文件" data-testid="typed-preview-download" onClick={download} />
        ) : null}
      </span>
    </div>
  );

  const downloadButton = (testIdOverride?: string) => canDownload ? (
    <Button size="sm" variant="secondary" icon="download" data-testid={testIdOverride} onClick={download}>下载原文件</Button>
  ) : null;

  let body: React.ReactNode;
  if (kind === "missing") {
    body = <Placeholder icon="circleAlert" title="文件不可用" message={message || "文件不存在或已删除"} testId="typed-preview-missing" />;
  } else if (kind === "too_large") {
    body = (
      <Placeholder
        icon="alert"
        tone="warning"
        title="文件过大，无法内联预览"
        message={message || "文件超过预览大小上限"}
        action={downloadButton()}
        testId="typed-preview-too-large"
      />
    );
  } else if (kind === "binary") {
    body = (
      <Placeholder
        icon="package"
        title="不支持内联预览"
        message={message || "此类型不支持内联预览，避免以文本显示乱码。"}
        action={downloadButton("typed-preview-binary-download")}
        testId="typed-preview-binary"
      />
    );
  } else if (kind === "image" && objectUrl) {
    body = <ImagePreview src={objectUrl} name={name} onMeasure={setImageSize} />;
  } else if (kind === "pdf" && objectUrl) {
    body = (
      <iframe
        title={name}
        src={objectUrl}
        data-testid="typed-preview-pdf"
        className={cn("block w-full flex-1 border-0 bg-cx-sunken", fill ? "min-h-0" : "h-[640px]")}
      />
    );
  } else if (kind === "html" && content != null && mode === "render") {
    body = (
      <div className={cn("flex flex-1 flex-col bg-cx-sunken p-3", fill ? "min-h-0" : "h-[560px]")}>
        <iframe
          title={name}
          sandbox=""
          srcDoc={content}
          data-testid="typed-preview-html"
          className="block min-h-0 w-full flex-1 rounded-lg border border-cx-border-subtle bg-white shadow-cx-sm"
        />
      </div>
    );
  } else if (kind === "markdown" && content != null && mode === "render") {
    body = (
      <div className={cn("cx-scroll flex-1 overflow-y-auto", fill && "min-h-0")} data-testid="typed-preview-markdown">
        <article className="cx-doc mx-auto w-full max-w-[760px] px-6 py-6">
          <ChatMarkdown text={content} />
        </article>
      </div>
    );
  } else if (content != null) {
    body = (
      <CodeViewer
        code={content}
        path={name}
        language={kind === "code" || kind === "html" || kind === "markdown" ? null : "text"}
        focusLine={focusLine}
        wrap={wrap}
        onCiteLines={onCiteLines}
        maxHeight={codeHeight}
        className="flex-1"
      />
    );
  } else {
    body = (
      <Placeholder
        icon="file"
        title="暂无预览内容"
        message={message}
        action={downloadButton()}
        testId="typed-preview-empty"
      />
    );
  }

  return (
    <div
      className={cn(
        "flex min-w-0 flex-col overflow-hidden bg-cx-bg",
        fill ? "h-full min-h-0 flex-1" : "rounded-xl border border-cx-border-subtle",
        className,
      )}
      data-testid={testId}
      data-preview-kind={kind}
    >
      {toolbar}
      <div className={cn("flex min-w-0 flex-col", fill && "min-h-0 flex-1")}>{body}</div>
    </div>
  );
}

export function workspacePreviewToProps(
  preview: WorkspaceFilePreview,
  options?: {
    /** Auth-safe blob URL for image/PDF. Do not pass bare /api/... raw URLs. */
    objectUrl?: string | null;
    downloadUrl?: string | null;
    onDownload?: (() => void) | null;
  },
): TypedResourcePreviewProps {
  const kind = String(preview.preview_kind || "text") as PreviewKind;
  const needsObject = kind === "image" || kind === "pdf";
  return {
    name: preview.path,
    mediaType: preview.media_type,
    previewKind: kind,
    content: preview.content,
    message: preview.message,
    size: preview.size,
    maxPreviewBytes: preview.max_preview_bytes,
    objectUrl: needsObject ? (options?.objectUrl ?? null) : null,
    downloadUrl: options?.downloadUrl ?? null,
    onDownload: options?.onDownload ?? null,
    focusLine: preview.line,
  };
}
