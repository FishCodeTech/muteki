"use client";

/**
 * C11 — confirm dialog for Retry / Edit-resend / Fork / native rewind.
 * One composition: mode switch, guarantee sentence, impact list (superseded
 * turns, file policy, touched files, side effects), warnings, CTAs.
 */

import React, { useEffect, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "../Icon";
import {
  Badge,
  Button,
  Callout,
  Dialog,
  SegmentedControl,
  TextArea,
  type DialogProps,
  type SegmentOption,
} from "@/components/chat/ui";
import { chatPanel } from "@/lib/chatPanelStore";
import { mapExternalSideEffects } from "@/lib/externalSideEffects";

export type ImpactMode = "retry" | "edit_resend" | "fork" | "native_rewind";

export type ImpactPreview = {
  mode: ImpactMode | string;
  target_turn_id: string;
  target_seq?: number;
  target_text_preview?: string;
  edited?: boolean;
  superseded_turn_ids: string[];
  superseded_seqs?: number[];
  superseded_message_count?: number;
  attachments_affected?: Array<{ sha256: string; name: string }>;
  workspace?: {
    policy?: string;
    dirty?: boolean;
    files_touched_since_target?: string[] | "unknown";
    shares_workspace?: boolean;
  };
  external_side_effects?: string;
  provider?: {
    rewind_level?: string;
    reason?: string;
    alternative?: string;
    invocable?: boolean;
  };
  guarantees?: {
    honest_label?: string;
    files?: string;
    conversation?: string;
    provider_session?: string;
  };
};

const MODE_TITLE: Record<string, string> = {
  retry: "重新执行本轮",
  edit_resend: "编辑后重发",
  fork: "Fork 对话",
  native_rewind: "原生回退",
};

const MODE_OPTIONS: Array<SegmentOption<ImpactMode>> = [
  { value: "retry", label: "重新执行", icon: "retry" },
  { value: "edit_resend", label: "编辑重发", icon: "pencilLine" },
  { value: "fork", label: "Fork", icon: "gitFork" },
  { value: "native_rewind", label: "原生回退", icon: "undo" },
];

const MAX_FILES = 8;

function isImpactMode(value: string): value is ImpactMode {
  return value === "retry" || value === "edit_resend" || value === "fork" || value === "native_rewind";
}

function modeVisual(mode: ImpactMode, filePolicy: string): { icon: IconName; tone: NonNullable<DialogProps["tone"]>; confirm: string } {
  switch (mode) {
    case "retry":
      return { icon: "retry", tone: "default", confirm: "确认重新执行" };
    case "edit_resend":
      return { icon: "pencilLine", tone: "default", confirm: "确认编辑重发" };
    case "fork":
      return { icon: "gitFork", tone: "accent", confirm: "确认 Fork" };
    case "native_rewind":
      // Syncing files rewrites the workspace, which cannot be undone from here.
      return { icon: "undo", tone: filePolicy === "sync_files" ? "danger" : "warning", confirm: "确认原生回退" };
    default: {
      const exhaustive: never = mode;
      return exhaustive;
    }
  }
}

type Props = {
  open: boolean;
  preview: ImpactPreview | null;
  loading?: boolean;
  editedText?: string;
  onEditedTextChange?: (value: string) => void;
  rewindDisabled?: boolean;
  rewindReason?: string;
  /** Frozen fork source (Pane #209) — title / workspace shown while confirm is open. */
  forkSource?: {
    threadId: string;
    title: string;
    projectName?: string;
    rootPath?: string;
  } | null;
  onConfirm: () => void;
  onCancel: () => void;
  onSwitchMode?: (mode: ImpactMode) => void;
  /** Enables opening touched files in the diff panel; falls back to `preview.thread_id`. */
  threadId?: string;
};

function ImpactRow({
  icon,
  tone = "neutral",
  children,
  hint,
  testId,
}: {
  icon: IconName;
  tone?: "neutral" | "warning" | "danger" | "success";
  children: ReactNode;
  hint?: ReactNode;
  testId?: string;
}) {
  const iconTone = {
    neutral: "text-cx-fg-4",
    warning: "text-cx-warning",
    danger: "text-cx-danger",
    success: "text-cx-success",
  }[tone];
  return (
    <li className="flex items-start gap-3 px-3 py-2.5">
      <Icon name={icon} size={15} className={cn("mt-[3px] shrink-0", iconTone)} />
      <div className="min-w-0 flex-1">
        <p className="text-[13px] leading-5 text-cx-fg" data-testid={testId}>{children}</p>
        {hint ? <p className="mt-0.5 text-[12px] leading-[18px] text-cx-fg-3">{hint}</p> : null}
      </div>
    </li>
  );
}

export function ImpactConfirmModal({
  open,
  preview: incomingPreview,
  loading = false,
  editedText,
  onEditedTextChange,
  rewindDisabled = true,
  rewindReason = "",
  forkSource = null,
  onConfirm,
  onCancel,
  onSwitchMode,
  threadId: threadIdProp,
}: Props) {
  const [retained, setRetained] = useState(incomingPreview);
  useEffect(() => {
    if (incomingPreview) setRetained(incomingPreview);
  }, [incomingPreview]);
  const preview = incomingPreview ?? retained;
  if (!preview) return null;

  const mode = String(preview.mode || "retry");
  const knownMode = isImpactMode(mode) ? mode : null;
  const seqs = preview.superseded_seqs?.length
    ? preview.superseded_seqs
    : preview.superseded_turn_ids.map((_, i) => i + 1);
  const filePolicy = preview.workspace?.policy || "keep_files";
  const files = preview.workspace?.files_touched_since_target;
  const fileList = Array.isArray(files) ? files : [];
  const visual = knownMode ? modeVisual(knownMode, filePolicy) : { icon: "retry" as IconName, tone: "default" as const, confirm: "确认" };
  const threadId = threadIdProp || (preview as { thread_id?: string }).thread_id || "";
  const confirmDisabled = loading || (mode === "native_rewind" && rewindDisabled);
  const showRewind = mode === "native_rewind" || Boolean(onSwitchMode);
  const attachments = preview.attachments_affected || [];
  const rewindStatus = rewindDisabled
    ? (rewindReason || preview.provider?.reason || "当前 Runtime 未确认支持原生回退")
    : "Provider 已确认支持原生回退";
  const sideEffects = mapExternalSideEffects(preview.external_side_effects);

  const openFile = (filePath: string) => {
    if (!threadId) return;
    onCancel();
    chatPanel.openDiff(threadId, { kind: "worktree", filePath });
  };

  return (
    <Dialog
      open={open && Boolean(incomingPreview)}
      onOpenChange={(next) => { if (!next && !loading) onCancel(); }}
      size="lg"
      icon={visual.icon}
      tone={visual.tone}
      title={MODE_TITLE[mode] || mode}
      description={preview.guarantees?.honest_label
        || "明确区分文本重建、Fork 与 Provider 原生回退；默认保留工作区文件。"}
      dismissable={!loading}
      testId="c11-impact-modal"
      footer={(
        <>
          <Button variant="ghost" onClick={onCancel} disabled={loading}>取消</Button>
          <Button
            variant={visual.tone === "danger" ? "danger" : "primary"}
            onClick={onConfirm}
            disabled={confirmDisabled}
            loading={loading}
            data-testid="c11-confirm"
          >
            {loading ? "处理中…" : visual.confirm}
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-4 pb-2">
        {onSwitchMode && knownMode ? (
          <SegmentedControl<ImpactMode>
            value={knownMode}
            onChange={(next) => { if (next !== knownMode) onSwitchMode(next); }}
            ariaLabel="操作方式"
            size="md"
            className="w-full [&>button]:flex-1"
            options={MODE_OPTIONS.map((option) => (
              option.value === "native_rewind"
                ? { ...option, disabled: rewindDisabled && knownMode !== "native_rewind" }
                : { ...option, disabled: loading }
            ))}
          />
        ) : null}

        {mode === "fork" && forkSource ? (
          <section className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2.5" data-testid="c11-fork-source">
            <h3 className="text-[12px] font-medium text-cx-fg-3">分叉来源</h3>
            <p className="mt-1 text-[13px] text-cx-fg" data-testid="c11-fork-source-title">{forkSource.title}</p>
            <p className="mt-1 break-all text-[12px] text-cx-fg-3" data-testid="c11-fork-source-workspace">
              {forkSource.projectName || forkSource.rootPath
                ? `工作区：${forkSource.projectName || ""}${forkSource.rootPath ? `（${forkSource.rootPath}）` : ""}`
                : "未绑定工作区"}
            </p>
          </section>
        ) : null}

        {mode === "edit_resend" && onEditedTextChange ? (
          <TextArea
            label="编辑用户消息"
            value={editedText ?? ""}
            onChange={(e) => onEditedTextChange(e.target.value)}
            autoResize
            maxRows={14}
            rows={4}
            data-autofocus
            data-testid="c11-edit-textarea"
          />
        ) : null}

        <section className="flex flex-col gap-2">
          <h3 className="text-[12.5px] font-semibold text-cx-fg-2">影响范围</h3>
          <ul className="divide-y divide-cx-border-subtle overflow-hidden rounded-xl border border-cx-border-subtle bg-cx-elevated">
            {mode === "fork" ? (
              <ImpactRow icon="gitFork" tone="success">
                源 Thread 保持不变；新 Thread 复制截止到所选轮次的历史。
              </ImpactRow>
            ) : seqs.length ? (
              <ImpactRow
                icon="layers"
                tone="warning"
                testId="c11-superseded-list"
                hint="被替代的轮次保留在历史中，可随时展开查看。"
              >
                将替代第 {seqs.join("、")} 轮（共 {preview.superseded_message_count ?? seqs.length} 条）
              </ImpactRow>
            ) : (
              <ImpactRow icon="checkCircle" tone="success">没有后续轮次需要替代。</ImpactRow>
            )}
            {attachments.length ? (
              <ImpactRow icon="paperclip">附件：{attachments.map((a) => a.name).join("、")}</ImpactRow>
            ) : null}
            <ImpactRow icon="folder" testId="c11-files-policy" tone={filePolicy === "sync_files" ? "danger" : "neutral"}>
              {filePolicy === "keep_files" || filePolicy === "fork_shares_workspace"
                ? "工作区文件保持原状（不会自动还原）"
                : filePolicy === "sync_files"
                  ? "将尝试同步还原文件（仅当能力已验证）"
                  : `文件策略：${filePolicy}`}
            </ImpactRow>
            {mode === "fork" ? (
              <ImpactRow icon="link" testId="c11-fork-workspace">
                Fork 当前与源 Thread 共享同一工作区
              </ImpactRow>
            ) : null}
            <ImpactRow
              icon="globe"
              tone="warning"
              testId="c11-side-effects"
              hint={sideEffects.hint}
            >
              <span data-side-effects-diagnostic={sideEffects.diagnostic || undefined}>
                外部副作用（已执行的工具 / 网络调用）不能撤销
              </span>
            </ImpactRow>
            {showRewind ? (
              <ImpactRow
                icon="undo"
                tone={rewindDisabled ? "neutral" : "success"}
                testId="c11-rewind-status"
              >
                {rewindStatus}
              </ImpactRow>
            ) : null}
          </ul>
        </section>

        {fileList.length || files === "unknown" ? (
          <section className="flex flex-col gap-2">
            <h3 className="flex items-center gap-2 text-[12.5px] font-semibold text-cx-fg-2">
              自目标轮次以来变更的文件
              {fileList.length ? <Badge>{fileList.length}</Badge> : null}
            </h3>
            {files === "unknown" ? (
              <p className="text-[12.5px] text-cx-fg-3">无法确定变更文件（Runtime 未上报）。</p>
            ) : (
              <ul className="overflow-hidden rounded-xl border border-cx-border-subtle bg-cx-elevated py-1" data-testid="c11-files-list">
                {fileList.slice(0, MAX_FILES).map((file) => {
                  const name = file.split("/").pop() || file;
                  const dir = file.slice(0, file.length - name.length);
                  const content = (
                    <>
                      <Icon name="file" size={14} className="shrink-0 text-cx-fg-4" />
                      <span className="min-w-0 flex-1 truncate font-cx-mono text-[12px]">
                        <span className="text-cx-fg-4">{dir}</span>
                        <span className="text-cx-fg">{name}</span>
                      </span>
                    </>
                  );
                  return (
                    <li key={file}>
                      {threadId ? (
                        <button
                          type="button"
                          onClick={() => openFile(file)}
                          title="在变更面板中查看（将关闭此确认）"
                          className="group/file flex h-8 w-full items-center gap-2 px-3 text-left transition-colors hover:bg-cx-hover"
                        >
                          {content}
                          <Icon name="arrowUpRight" size={13} className="shrink-0 text-cx-fg-4 opacity-0 transition-opacity group-hover/file:opacity-100" />
                        </button>
                      ) : (
                        <div className="flex h-8 items-center gap-2 px-3">{content}</div>
                      )}
                    </li>
                  );
                })}
                {fileList.length > MAX_FILES ? (
                  <li className="px-3 py-1.5 text-[12px] text-cx-fg-4">另有 {fileList.length - MAX_FILES} 个文件</li>
                ) : null}
              </ul>
            )}
          </section>
        ) : null}

        {preview.workspace?.dirty ? (
          <Callout tone="warning" title="工作区当前有未提交改动">
            {filePolicy === "sync_files"
              ? "同步还原可能覆盖这些改动，请先确认或提交。"
              : "这些改动会原样保留，新一轮执行将在其基础上继续。"}
          </Callout>
        ) : null}

        {mode === "native_rewind" && rewindDisabled ? (
          <Callout
            tone="warning"
            title="原生回退不可用"
            action={onSwitchMode ? (
              <>
                <Button size="sm" variant="secondary" onClick={() => onSwitchMode("fork")} data-testid="c11-alt-fork">改用 Fork</Button>
                <Button size="sm" variant="secondary" onClick={() => onSwitchMode("retry")} data-testid="c11-alt-retry">改用重新执行</Button>
              </>
            ) : undefined}
          >
            替代：{preview.provider?.alternative || "可改用 Fork 或重新执行本轮（保留文件）"}
          </Callout>
        ) : null}
      </div>
    </Dialog>
  );
}
