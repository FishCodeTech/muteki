"use client";

import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { SPRING_LAYOUT, Tooltip, useReducedMotion } from "@/components/chat/ui";
import type { ComposerCapabilityRef } from "@/lib/composerCapabilities";
import { composerAttachmentRecoveryAction } from "@/lib/composerAttachmentRecovery";
import { isComposerImageAttachment } from "@/lib/composerAttachments";
import type { PromptBarAttachment } from "../../ai-native/prompt-bar";
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
        : [attachment.name, formatBytes(attachment.size)].filter(Boolean).join(" · ");

  return (
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
        <div className="size-full overflow-hidden rounded-lg">
          {src ? (
            // eslint-disable-next-line @next/next/no-img-element
            <img src={src} alt={attachment.name} draggable={false} className="size-full object-cover" />
          ) : (
            <div className="grid size-full place-items-center text-cx-fg-4">
              <Icon name="image" size={18} />
            </div>
          )}
        </div>
        {uploading ? (
          <div className="absolute inset-0 grid place-items-center rounded-lg bg-[color-mix(in_srgb,black_38%,transparent)] text-white">
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
        <span className="truncate text-[12.5px] font-medium leading-5 text-cx-fg">{attachment.name}</span>
        <span
          className={cn(
            "cx-tabular truncate text-[11.5px] leading-4",
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
  label,
  status,
  title,
  onRemove,
  removeLabel,
}: {
  kind: string;
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
        "cx-ref-pill group inline-flex h-7 max-w-[240px] items-center gap-1.5 rounded-full pl-2 text-[12.5px] font-medium",
        onRemove ? "pr-1" : "pr-2.5",
        tone === "ok" && "bg-cx-accent-soft text-cx-accent",
        tone === "stale" && "bg-cx-warning-soft text-cx-warning",
        tone === "missing" && "bg-cx-danger-soft text-cx-danger",
      )}
    >
      <Icon name={capabilityIcon(kind)} size={12} className="shrink-0" />
      <span className="min-w-0 truncate">{label}</span>
      {badge ? <span className="shrink-0 text-[10.5px] font-normal opacity-85">{badge}</span> : null}
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

export interface AttachmentTrayProps {
  attachments: PromptBarAttachment[];
  refs: ComposerCapabilityRef[];
  contextPill?: { label: string; tooltip?: string; onClick?: () => void };
  onRemoveAttachment?: (index: number) => void;
  onRetryAttachment?: (index: number) => void;
  onRemoveRef?: (ref: ComposerCapabilityRef) => void;
}

export function AttachmentTray({
  attachments,
  refs,
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
  const hasPills = Boolean(contextPill) || refs.length > 0;
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
              className="cx-press inline-flex h-7 max-w-[220px] items-center gap-1.5 rounded-full bg-cx-hover px-2.5 text-[12.5px] font-medium text-cx-fg-2 hover:bg-cx-active hover:text-cx-fg"
            >
              <span className="size-1.5 shrink-0 rounded-full bg-cx-accent" />
              <span className="truncate">{contextPill.label}</span>
            </button>
          ) : null}
          {refs.map((ref) => (
            <CapabilityRefPill
              key={ref.node_id || ref.id}
              kind={String(ref.kind)}
              label={ref.kind === "skill" ? `$${ref.name}` : `@${ref.name}`}
              status={ref.status}
              title={`${ref.source} · ${ref.status_reason || ref.description || ref.snapshot?.text || ""}`}
              removeLabel={`移除 ${ref.name}`}
              onRemove={onRemoveRef ? () => onRemoveRef(ref) : undefined}
            />
          ))}
        </div>
      ) : null}
    </div>
  );
}
