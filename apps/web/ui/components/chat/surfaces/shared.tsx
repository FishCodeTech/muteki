"use client";

import { useEffect, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { apiFetch } from "@/lib/useRun";
import type { ConversationView } from "@/lib/useConversation";
import type { ConversationToolRecord } from "@/components/conversation/conversationEventViews";
import type { DrawerDetailPayload } from "@/components/conversation/ConversationDetailsDrawer";
import type { Tone } from "@/components/chat/ui";
import type { IconName } from "@/components/Icon";
import { previewKindFromName } from "@/lib/resourcePreview";

/** Slim per-surface toolbar that sits under the panel's tab strip. */
export function SurfaceToolbar({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div className={cn("flex h-10 shrink-0 items-center gap-1 border-b border-cx-border-subtle px-2", className)}>
      {children}
    </div>
  );
}

export function SectionHeader({ title, count, action, className }: { title: ReactNode; count?: number | string; action?: ReactNode; className?: string }) {
  return (
    <div className={cn("flex h-7 items-center justify-between gap-2 px-1", className)}>
      <h3 className="flex items-center gap-1.5 text-[11.5px] font-medium text-cx-fg-3">
        {title}
        {count !== undefined ? <span className="cx-tabular text-cx-fg-4">{count}</span> : null}
      </h3>
      {action}
    </div>
  );
}

export async function surfaceJson<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await apiFetch(path, init);
  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    const message = data?.error?.message || data?.detail || `请求失败（${response.status}）`;
    throw new Error(String(message));
  }
  return data as T;
}

export function formatBytes(size?: number | null): string {
  if (typeof size !== "number" || !Number.isFinite(size) || size < 0) return "";
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(size >= 10 * 1024 ? 0 : 1)} KB`;
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
}

export function relativeTime(input: string | number | null | undefined, now = Date.now()): string {
  if (input === null || input === undefined || input === "") return "";
  const time = typeof input === "number" ? input : new Date(input).getTime();
  if (!Number.isFinite(time)) return "";
  const diff = Math.max(0, (now - time) / 1000);
  if (diff < 45) return "刚刚";
  if (diff < 3_600) return `${Math.max(1, Math.floor(diff / 60))} 分钟前`;
  if (diff < 86_400) return `${Math.floor(diff / 3_600)} 小时前`;
  if (diff < 86_400 * 30) return `${Math.floor(diff / 86_400)} 天前`;
  return new Date(time).toLocaleDateString();
}

export function formatDuration(ms?: number | null): string {
  if (typeof ms !== "number" || !Number.isFinite(ms) || ms < 0) return "";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  const minutes = Math.floor(seconds / 60);
  const rest = Math.round(seconds % 60);
  if (minutes < 60) return rest ? `${minutes}m ${rest}s` : `${minutes}m`;
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

/** Re-render on an interval so relative timestamps stay fresh; paused while hidden. */
export function useNow(active: boolean, intervalMs = 30_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [active, intervalMs]);
  return now;
}

export function turnStatusMap(view: ConversationView): Record<string, string> {
  return Object.fromEntries((view.turns || []).map((turn) => [turn.turn_id, turn.status]));
}

export type ToolStatus = ConversationToolRecord["status"];

export function toolStatusLabel(status: ToolStatus): string {
  switch (status) {
    case "running":
      return "执行中";
    case "failed":
      return "异常";
    case "completed":
      return "完成";
    case "pending":
      return "等待中";
    case "cancelled":
      return "已取消";
    case "declined":
      return "已拒绝";
    default: {
      const exhaustive: never = status;
      return exhaustive;
    }
  }
}

export function toolStatusTone(status: ToolStatus): Tone {
  switch (status) {
    case "running":
      return "running";
    case "failed":
      return "danger";
    case "completed":
      return "success";
    case "pending":
      return "neutral";
    case "cancelled":
      return "warning";
    case "declined":
      return "warning";
    default: {
      const exhaustive: never = status;
      return exhaustive;
    }
  }
}

export function toolPayload(record: ConversationToolRecord): DrawerDetailPayload {
  const isTerminal = /^(shell|exec_command|terminal|bash)$/i.test(record.name);
  return {
    type: record.status === "failed" ? "error" : "tool",
    title: record.name,
    subtitle: record.turnId || record.occurredAt,
    status: record.status,
    toolName: record.name,
    input: record.argsSummary,
    output: isTerminal ? undefined : record.outputSummary,
    terminalOutput: isTerminal ? record.outputSummary : undefined,
    error: record.error,
  };
}

export type ConversationArtifact = NonNullable<ConversationView["artifacts"]>[number];

export function isDiffArtifact(artifact: ConversationArtifact): boolean {
  return artifact.kind === "conversation.diff" || Boolean(artifact.name?.endsWith(".diff"));
}

export function artifactPayload(view: ConversationView, artifact: ConversationArtifact): DrawerDetailPayload {
  const isDiff = isDiffArtifact(artifact);
  return {
    type: isDiff ? "diff" : "artifact",
    title: artifact.name || "Artifact",
    subtitle: artifact.sha256,
    artifact: {
      sha256: artifact.sha256,
      name: artifact.name,
      kind: artifact.kind,
      mediaType: artifact.media_type,
      size: artifact.size,
      threadId: view.thread.thread_id,
    },
    ...(isDiff ? { diff: { file: artifact.name || "conversation.diff" } } : {}),
  };
}

export function statusPresentation(view: ConversationView): { label: string; tone: Tone } {
  if (view.state.running_turn_id) return { label: "执行中", tone: "running" };
  const lastTurn = view.turns.at(-1);
  if (view.state.last_error && Object.keys(view.state.last_error).length > 0) return { label: "失败", tone: "danger" };
  const lastStatus = String(lastTurn?.status || "").toLowerCase();
  if (lastStatus === "failed") return { label: "失败", tone: "danger" };
  if (["interrupted", "aborted", "cancelled", "canceled"].includes(lastStatus)) return { label: "已中断", tone: "warning" };
  if (lastStatus === "completed") return { label: "已完成", tone: "success" };
  return { label: "就绪", tone: "neutral" };
}

export function workspaceLabel(view: ConversationView): string {
  const root = view.workspace?.root_path?.replace(/\/$/, "") || "";
  const mode = String(view.workspace?.settings?.mode || "");
  if (mode === "new_worktree" || mode === "existing_worktree") return root.split("/").filter(Boolean).at(-1) || "Git worktree";
  if (view.workspace?.kind === "isolated") return "沙箱工作区";
  if (root) return root.split("/").filter(Boolean).at(-1) || root;
  if (view.workspace?.kind === "git") return "Git 工作区";
  if (view.workspace?.kind === "local") return "本地工作区";
  return "未绑定工作区";
}

export function workspaceKindLabel(view: ConversationView): string {
  const mode = String(view.workspace?.settings?.mode || "");
  if (mode === "new_worktree") return "新建 worktree";
  if (mode === "existing_worktree") return "已有 worktree";
  if (mode === "shared_checkout") return "主检出";
  if (view.workspace?.kind === "isolated") return "独立";
  if (view.workspace?.kind === "git") return "Git";
  if (view.workspace?.kind === "local") return "本地";
  return "未绑定";
}

const IS_MAC = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

/**
 * Surfaces don't receive the Shell's bottom-panel setter. A listener may claim
 * `muteki:open-execution-log` (preventDefault); otherwise fall back to the
 * Shell's Mod+J shortcut handler.
 */
export function openExecutionLog() {
  if (typeof window === "undefined") return;
  const claimed = !window.dispatchEvent(new CustomEvent("muteki:open-execution-log", { cancelable: true }));
  if (claimed) return;
  document.dispatchEvent(new KeyboardEvent("keydown", {
    key: "j",
    code: "KeyJ",
    metaKey: IS_MAC,
    ctrlKey: !IS_MAC,
    bubbles: true,
    cancelable: true,
  }));
}

export function fileIconFor(name: string): IconName {
  const kind = previewKindFromName(name);
  switch (kind) {
    case "image":
      return "image";
    case "pdf":
      return "book";
    case "markdown":
      return "pilcrow";
    case "html":
    case "code":
      return "fileCode";
    case "text":
    case "binary":
    case "too_large":
    case "missing":
      return "file";
    default: {
      const exhaustive: never = kind;
      return exhaustive;
    }
  }
}

export function fileName(path: string): string {
  return path.split("/").filter(Boolean).pop() || path;
}

export function parentPath(path: string): string {
  return path.split("/").filter(Boolean).slice(0, -1).join("/");
}
