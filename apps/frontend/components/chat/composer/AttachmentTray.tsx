"use client";

import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import { Popover, SPRING_LAYOUT, Tooltip, useReducedMotion } from "@muteki/ui";
import { excerptPreview } from "@/lib/composerContextDoc";
import { ImageLightbox } from "@/components/chat/media/ImageViewer";
import type { ComposerCapabilityRef } from "@/lib/composerCapabilities";
import { composerAttachmentRecoveryAction } from "@/lib/composerAttachmentRecovery";
import { isComposerImageAttachment } from "@/lib/composerAttachments";
import type { PromptBarAttachment } from "./MutekiPromptBar";
import { capabilityIcon, refStatusLabel, refStatusTone } from "./capabilityMeta";
import { fileExtension, formatBytes } from "./format";
import { ProgressRing } from "./ProgressRing";

const RESELECT_HINT = "刷新后无法恢复该文件，请重新选择后再发送";

function useObjectUrl(file?: File): string | null {
  const [src, setSrc] = useState<string | null>(null);
  useEffect(() => {
    if (!file) {
      setSrc(null);
      return;
    }
    const url = URL.createObjectURL(file);
    setSrc(url);
    return () => URL.revokeObjectURL(url);
  }, [file]);
  return src;
}

function RemoveButton({ name, onRemove }: { name: string; onRemove: () => void }) {
  return (
    <button
      type="button"
      aria-label={`移除 ${name}`}
      onClick={onRemove}
      className={cn(
        "cx-press absolute -right-1.5 -top-1.5 z-10 grid size-[18px] place-items-center rounded-full bg-cx-fg text-cx-bg shadow-cx-sm",
        "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 focus-visible:opacity-100",
        "[@media(hover:none)]:opacity-100",
      )}
    >
      <Icon name="x" size={10} />
    </button>
  );
}

function uploadProgress(attachment: PromptBarAttachment): number | null {
  const value = attachment.uploadProgress;
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function ImageThumb({
  attachment,
  onRemove,
  onRetry,
}: {
  attachment: PromptBarAttachment;
  onRemove?: () => void;
  onRetry?: () => void;
}) {
  const src = useObjectUrl(attachment.file);
  const [previewOpen, setPreviewOpen] = useState(false);
  const recovery = composerAttachmentRecoveryAction(attachment);
  const uploading = attachment.uploadStatus === "uploading";
  const failed = attachment.uploadStatus === "error";
  const needsPick = recovery === "reselect";
  const hint = needsPick
    ? RESELECT_HINT
    : failed
      ? attachment.uploadError || "上传失败，点击重试"
      : uploading
        ? "正在上传…"
        : [attachment.name, formatBytes(attachment.size), attachment.scaledNote].filter(Boolean).join(" · ");

  return (
    <>
      <Tooltip content={hint}>
        <div
          className={cn(
            "group relative size-14 shrink-0 rounded-lg bg-cx-sunken",
            "shadow-[0_0_0_1px_var(--cx-border)]",
            needsPick && "shadow-[0_0_0_1.5px_var(--cx-warning)]",
            failed && !needsPick && "shadow-[0_0_0_1.5px_var(--cx-danger)]",
          )}
          data-attachment-kind="image"
          data-upload-status={attachment.uploadStatus || "done"}
        >
          <button
            type="button"
            aria-label={`预览 ${attachment.name}`}
            disabled={!src}
            onClick={() => setPreviewOpen(true)}
            className="block size-full cursor-zoom-in overflow-hidden rounded-lg focus-visible:outline focus-visible:outline-2 focus-visible:outline-cx-focus disabled:cursor-default"
          >
            {src ? (
              // eslint-disable-next-line @next/next/no-img-element
              <img src={src} alt={attachment.name} draggable={false} className="size-full object-cover" />
            ) : (
              <div className="grid size-full place-items-center text-cx-fg-4">
                <Icon name="image" size={18} />
              </div>
            )}
          </button>
          {uploading ? (
            <div className="pointer-events-none absolute inset-0 grid place-items-center rounded-lg bg-[color-mix(in_srgb,black_38%,transparent)] text-white">
              <ProgressRing value={uploadProgress(attachment)} size={22} stroke={2.25} trackOpacity={0.3} label="正在上传" />
            </div>
          ) : null}
          {(needsPick || (recovery === "retry" && failed)) && onRetry ? (
            <button
              type="button"
              aria-label={needsPick ? `重新选择 ${attachment.name}` : `重试上传 ${attachment.name}`}
              onClick={onRetry}
              className={cn(
                "absolute inset-0 grid place-items-center rounded-lg text-white transition-colors",
                needsPick
                  ? "bg-[color-mix(in_srgb,var(--amber)_58%,transparent)] hover:bg-[color-mix(in_srgb,var(--amber)_70%,transparent)]"
                  : "bg-[color-mix(in_srgb,var(--red)_55%,transparent)] hover:bg-[color-mix(in_srgb,var(--red)_68%,transparent)]",
              )}
            >
              <Icon name="retry" size={16} />
            </button>
          ) : null}
          {onRemove ? <RemoveButton name={attachment.name} onRemove={onRemove} /> : null}
        </div>
      </Tooltip>
      {src ? (
        <ImageLightbox images={[{ src, name: attachment.name }]} index={0} open={previewOpen} onOpenChange={setPreviewOpen} />
      ) : null}
    </>
  );
}

function FileCard({
  attachment,
  onRemove,
  onRetry,
}: {
  attachment: PromptBarAttachment;
  onRemove?: () => void;
  onRetry?: () => void;
}) {
  const recovery = composerAttachmentRecoveryAction(attachment);
  const uploading = attachment.uploadStatus === "uploading";
  const failed = attachment.uploadStatus === "error";
  const needsPick = recovery === "reselect";
  const meta = needsPick
    ? "需重新选择"
    : failed
      ? attachment.uploadError || "上传失败"
      : uploading
        ? "上传中…"
        : [fileExtension(attachment.name), formatBytes(attachment.size)].filter(Boolean).join(" · ") || "文件";
  const hint = needsPick ? RESELECT_HINT : failed ? attachment.uploadError || "上传失败，点击重试" : attachment.name;

  return (
    <div
      className={cn(
        "group relative flex h-14 w-[208px] max-w-full shrink-0 items-center gap-2.5 rounded-xl bg-cx-bg-subtle pl-2 pr-2.5",
        "shadow-[0_0_0_1px_var(--cx-border)]",
        needsPick && "shadow-[0_0_0_1px_var(--cx-warning)]",
        failed && !needsPick && "shadow-[0_0_0_1px_var(--cx-danger)]",
      )}
      title={hint}
      data-attachment-kind="file"
      data-upload-status={attachment.uploadStatus || "done"}
    >
      <span
        className={cn(
          "grid size-10 shrink-0 place-items-center rounded-lg",
          needsPick
            ? "bg-cx-warning-soft text-cx-warning"
            : failed
              ? "bg-cx-danger-soft text-cx-danger"
              : "bg-cx-accent-soft text-cx-accent",
        )}
      >
        {uploading ? (
          <ProgressRing value={uploadProgress(attachment)} size={18} label="正在上传" />
        ) : (
          <Icon name={failed ? "alert" : "file"} size={17} />
        )}
      </span>
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate text-cx-sm font-medium leading-5 text-cx-fg">{attachment.name}</span>
        <span
          className={cn(
            "cx-tabular truncate text-cx-xs leading-4",
            needsPick ? "text-cx-warning" : failed ? "text-cx-danger" : "text-cx-fg-3",
          )}
        >
          {meta}
        </span>
      </span>
      {(needsPick || (recovery === "retry" && failed)) && onRetry ? (
        <button
          type="button"
          aria-label={needsPick ? `重新选择 ${attachment.name}` : `重试上传 ${attachment.name}`}
          title={needsPick ? "重新选择文件" : "重试上传"}
          onClick={onRetry}
          className={cn(
            "cx-press grid size-7 shrink-0 place-items-center rounded-lg hover:bg-cx-hover",
            needsPick ? "text-cx-warning" : "text-cx-danger",
          )}
        >
          <Icon name="retry" size={14} />
        </button>
      ) : null}
      {onRemove ? <RemoveButton name={attachment.name} onRemove={onRemove} /> : null}
    </div>
  );
}

export function CapabilityRefPill({
  kind,
  refId,
  label,
  status,
  title,
  onRemove,
  removeLabel,
}: {
  kind: string;
  refId?: string;
  label: string;
  status?: string;
  title?: string;
  onRemove?: () => void;
  removeLabel?: string;
}) {
  const tone = refStatusTone(status);
  const badge = refStatusLabel(status);
  return (
    <span
      title={title}
      data-ref-status={tone}
      className={cn(
        "cx-ref-pill group inline-flex h-7 max-w-[240px] items-center gap-1.5 rounded-full pl-2 text-cx-sm font-medium",
        onRemove ? "pr-1" : "pr-2.5",
        tone === "ok" && "bg-cx-accent-soft text-cx-accent",
        tone === "stale" && "bg-cx-warning-soft text-cx-warning",
        tone === "missing" && "bg-cx-danger-soft text-cx-danger",
      )}
    >
      <Icon name={capabilityIcon(kind, refId)} size={12} className="shrink-0" />
      <span className="min-w-0 truncate">{label}</span>
      {badge ? <span className="shrink-0 text-cx-xs font-normal opacity-85">{badge}</span> : null}
      {onRemove ? (
        <button
          type="button"
          aria-label={removeLabel}
          onClick={onRemove}
          className="grid size-5 shrink-0 place-items-center rounded-full opacity-60 transition-opacity hover:bg-[color-mix(in_srgb,currentColor_14%,transparent)] hover:opacity-100"
        >
          <Icon name="x" size={10} />
        </button>
      ) : null}
    </span>
  );
}

function AnnotationRow({
  annotation,
  index,
  onRemove,
  onUpdate,
  onJump,
}: {
  annotation: ComposerCapabilityRef;
  index: number;
  onRemove?: () => void;
  onUpdate?: (comment: string) => void;
  onJump?: () => void;
}) {
  const excerpt = excerptPreview(annotation.snapshot?.text || "") || annotation.name;
  const commit = (value: string) => {
    if (value.trim() !== (annotation.comment || "")) onUpdate?.(value.trim());
  };
  return (
    <li className="group flex items-start gap-2 rounded-lg px-2 py-1.5 hover:bg-cx-hover" data-annotation-node={annotation.node_id}>
      <span className="cx-annotation-index mt-[3px]">{index}</span>
      <div className="min-w-0 flex-1">
        <button
          type="button"
          disabled={!onJump}
          onClick={onJump}
          title={onJump ? "定位到原文" : undefined}
          className="line-clamp-2 w-full whitespace-pre-wrap break-words text-left text-cx-xs leading-[18px] text-cx-fg-3 enabled:hover:text-cx-fg disabled:cursor-default"
        >
          {excerpt}
        </button>
        <input
          key={`${annotation.node_id}:${annotation.comment || ""}`}
          defaultValue={annotation.comment || ""}
          readOnly={!onUpdate}
          placeholder="添加可选评论…"
          aria-label={`注释 ${index} 的评论`}
          onBlur={(event) => commit(event.currentTarget.value)}
          onKeyDown={(event) => {
            if (event.key !== "Enter" || event.nativeEvent.isComposing || event.keyCode === 229) return;
            event.preventDefault();
            event.currentTarget.blur();
          }}
          className="-mx-1 mt-0.5 h-7 w-[calc(100%+0.5rem)] rounded-md bg-transparent px-1 text-cx-sm text-cx-fg outline-none placeholder:text-cx-fg-4 focus:bg-cx-bg-subtle"
        />
        {annotation.status && annotation.status !== "ok" && annotation.status_reason ? (
          <p className="text-cx-xs text-cx-warning">{annotation.status_reason}</p>
        ) : null}
      </div>
      {onRemove ? (
        <button
          type="button"
          aria-label={`移除注释 ${index}`}
          onClick={onRemove}
          className="cx-press grid size-6 shrink-0 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-active hover:text-cx-fg"
        >
          <Icon name="x" size={12} />
        </button>
      ) : null}
    </li>
  );
}

function AnnotationsPill({
  annotations,
  onRemove,
  onUpdate,
  onJump,
}: {
  annotations: ComposerCapabilityRef[];
  onRemove?: (nodeIds: string[]) => void;
  onUpdate?: (nodeId: string, comment: string) => void;
  onJump?: (annotation: ComposerCapabilityRef) => void;
}) {
  const [open, setOpen] = useState(false);
  const count = annotations.length;
  useEffect(() => {
    if (!count) setOpen(false);
  }, [count]);
  return (
    <Popover
      open={open}
      onOpenChange={setOpen}
      placement="top-start"
      initialFocus="container"
      ariaLabel="注释"
      className="w-[min(380px,calc(100vw-24px))] p-1.5"
      trigger={(
        <button
          type="button"
          data-testid="composer-annotations-pill"
          title="查看或编辑注释"
          className="cx-press inline-flex h-7 max-w-[220px] items-center gap-1.5 rounded-full bg-cx-accent-soft px-2.5 text-cx-sm font-medium text-cx-accent hover:brightness-95"
        >
          <Icon name="quote" size={12} className="shrink-0" />
          <span className="truncate">{count} 条注释</span>
        </button>
      )}
    >
      <div className="flex items-center justify-between px-2 pb-1 pt-0.5 text-cx-xs text-cx-fg-3">
        <span>注释会随下一条消息发送</span>
        {onRemove ? (
          <button
            type="button"
            className="rounded px-1 hover:bg-cx-hover hover:text-cx-fg"
            onClick={() => onRemove(annotations.flatMap((annotation) => (annotation.node_id ? [annotation.node_id] : [])))}
          >
            全部移除
          </button>
        ) : null}
      </div>
      <ul className="flex max-h-[min(360px,50vh)] flex-col gap-0.5 overflow-auto" data-testid="composer-annotations-list">
        {annotations.map((annotation, index) => (
          <AnnotationRow
            key={annotation.node_id || annotation.id}
            annotation={annotation}
            index={index + 1}
            onRemove={onRemove && annotation.node_id ? () => onRemove([annotation.node_id!]) : undefined}
            onUpdate={onUpdate && annotation.node_id ? (comment) => onUpdate(annotation.node_id!, comment) : undefined}
            onJump={onJump && annotation.locator?.message_id ? () => { setOpen(false); onJump(annotation); } : undefined}
          />
        ))}
      </ul>
    </Popover>
  );
}

export interface AttachmentTrayProps {
  attachments: PromptBarAttachment[];
  refs: ComposerCapabilityRef[];
  annotations?: ComposerCapabilityRef[];
  onRemoveAnnotation?: (nodeIds: string[]) => void;
  onUpdateAnnotation?: (nodeId: string, comment: string) => void;
  onJumpAnnotation?: (annotation: ComposerCapabilityRef) => void;
  contextPill?: { label: string; tooltip?: string; onClick?: () => void };
  onRemoveAttachment?: (index: number) => void;
  onRetryAttachment?: (index: number) => void;
  onRemoveRef?: (ref: ComposerCapabilityRef) => void;
}

export function AttachmentTray({
  attachments,
  refs,
  annotations = [],
  onRemoveAnnotation,
  onUpdateAnnotation,
  onJumpAnnotation,
  contextPill,
  onRemoveAttachment,
  onRetryAttachment,
  onRemoveRef,
}: AttachmentTrayProps) {
  const reduced = useReducedMotion();
  const indexed = attachments.map((attachment, index) => ({ attachment, index }));
  const images = indexed.filter(({ attachment }) => isComposerImageAttachment(attachment));
  const files = indexed.filter(({ attachment }) => !isComposerImageAttachment(attachment));
  const itemMotion = reduced
    ? { initial: { opacity: 0 }, animate: { opacity: 1 }, exit: { opacity: 0 } }
    : {
      initial: { opacity: 0, scale: 0.9 },
      animate: { opacity: 1, scale: 1, transition: SPRING_LAYOUT },
      exit: { opacity: 0, scale: 0.9, transition: { duration: 0.12 } },
    };
  const retryFor = (attachment: PromptBarAttachment, index: number) => (
    composerAttachmentRecoveryAction(attachment) && onRetryAttachment ? () => onRetryAttachment(index) : undefined
  );

  const hasMedia = attachments.length > 0;
  const hasPills = Boolean(contextPill) || refs.length > 0 || annotations.length > 0;
  if (!hasMedia && !hasPills) return null;

  return (
    <div className="flex flex-col gap-2 px-3 pt-3" data-testid="composer-attachment-tray">
      {hasMedia ? (
        <div className="flex flex-wrap items-center gap-2.5 pr-1.5 pt-1.5">
          <AnimatePresence initial={false} mode="popLayout">
            {images.map(({ attachment, index }) => (
              <motion.div key={`img-${attachment.id || index}-${attachment.name}`} layout={!reduced} {...itemMotion}>
                <ImageThumb
                  attachment={attachment}
                  onRemove={onRemoveAttachment ? () => onRemoveAttachment(index) : undefined}
                  onRetry={retryFor(attachment, index)}
                />
              </motion.div>
            ))}
            {files.map(({ attachment, index }) => (
              <motion.div key={`file-${attachment.id || index}-${attachment.name}`} layout={!reduced} {...itemMotion}>
                <FileCard
                  attachment={attachment}
                  onRemove={onRemoveAttachment ? () => onRemoveAttachment(index) : undefined}
                  onRetry={retryFor(attachment, index)}
                />
              </motion.div>
            ))}
          </AnimatePresence>
        </div>
      ) : null}
      {hasPills ? (
        <div className="flex flex-wrap items-center gap-1.5">
          {contextPill ? (
            <button
              type="button"
              onClick={contextPill.onClick}
              title={contextPill.tooltip}
              className="cx-press inline-flex h-7 max-w-[220px] items-center gap-1.5 rounded-full bg-cx-hover px-2.5 text-cx-sm font-medium text-cx-fg-2 hover:bg-cx-active hover:text-cx-fg"
            >
              <span className="size-1.5 shrink-0 rounded-full bg-cx-accent" />
              <span className="truncate">{contextPill.label}</span>
            </button>
          ) : null}
          {annotations.length ? (
            <AnnotationsPill
              annotations={annotations}
              onRemove={onRemoveAnnotation}
              onUpdate={onUpdateAnnotation}
              onJump={onJumpAnnotation}
            />
          ) : null}
          {refs.map((ref) => {
            const label = ref.kind === "skill"
              ? `$${ref.name}`
              : ref.kind === "council"
                ? `圆桌 · ${ref.name}`
                : ref.kind === "roster"
                  ? `编队 · ${ref.name}`
                  : ref.boundary_notice
                    ? `@${ref.name} · 跨边界`
                    : `@${ref.name}`;
            const titleParts = [
              ref.source,
              ref.status_reason || ref.description || ref.snapshot?.text || "",
              ref.boundary_notice ? `[边界提示] ${ref.boundary_notice}` : "",
            ].filter(Boolean);
            return (
              <CapabilityRefPill
                key={ref.node_id || ref.id}
                kind={String(ref.kind)}
                refId={ref.id}
                label={label}
                status={ref.status}
                title={titleParts.join(" · ")}
                removeLabel={`移除 ${ref.name}`}
                onRemove={onRemoveRef ? () => onRemoveRef(ref) : undefined}
              />
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
