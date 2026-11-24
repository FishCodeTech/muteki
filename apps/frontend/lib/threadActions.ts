"use client";


import { apiFetch, currentAuthScope } from "./serviceAuth";
import { conversationExportFilename, conversationExportMetadata } from "./conversationExport";
import type { ThreadControlLifecycle } from "./useConversation";

export class ThreadActionError extends Error {
  readonly code: string;
  readonly status: number;
  constructor(code: string, message: string, status: number) {
    super(message);
    this.name = "ThreadActionError";
    this.code = code;
    this.status = status;
  }
}

async function failureOf(res: Response, fallback: string): Promise<ThreadActionError> {
  const body = await res.json().catch(() => null) as Record<string, unknown> | null;
  const error = body?.error;
  if (error && typeof error === "object") {
    const row = error as Record<string, unknown>;
    return new ThreadActionError(
      typeof row.code === "string" ? row.code : `http.${res.status}`,
      typeof row.message === "string" && row.message ? row.message : fallback,
      res.status,
    );
  }
  return new ThreadActionError(
    `http.${res.status}`,
    typeof error === "string" && error ? error : typeof body?.detail === "string" ? body.detail : `${fallback}（HTTP ${res.status}）`,
    res.status,
  );
}

export function threadActionMessage(error: unknown): string {
  if (error instanceof ThreadActionError) return `${error.message}（${error.code}）`;
  return error instanceof Error ? error.message : String(error);
}

export type ThreadLifecycleAction =
  | "settle" | "unsettle" | "pin" | "unpin" | "mark_read" | "mark_unread" | "follow_up" | "park" | "keep";

export interface ThreadLifecycleResult {
  thread_id: string;
  action: ThreadLifecycleAction;
  lifecycle: ThreadControlLifecycle;
  unread: boolean;
  auto_settle_disabled: boolean;
}

export interface ThreadLifecyclePatch {
  threadId: string;
  lifecycle: ThreadControlLifecycle;
  unread: boolean;
  autoSettleDisabled: boolean;
}

export const THREAD_LIFECYCLE_EVENT = "muteki:thread-lifecycle";

export async function changeThreadLifecycle(
  threadId: string,
  action: ThreadLifecycleAction,
  options: { on?: boolean; note?: string } = {},
): Promise<ThreadLifecycleResult> {
  const res = await apiFetch(`/api/control/threads/${encodeURIComponent(threadId)}/lifecycle`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ action, ...options }),
  });
  if (!res.ok) throw await failureOf(res, "对话状态更新失败");
  const result = await res.json() as ThreadLifecycleResult;
  if (typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent<ThreadLifecyclePatch>(THREAD_LIFECYCLE_EVENT, {
      detail: { threadId, lifecycle: result.lifecycle, unread: result.unread, autoSettleDisabled: result.auto_settle_disabled },
    }));
  }
  return result;
}

export interface ThreadPullRequestBadge {
  host: string;
  repository: string;
  number: number;
  url: string;
  state?: "open" | "closed" | "merged" | null;
  title?: string | null;
  is_draft?: boolean | null;
}

export interface ThreadAnnotation {
  pull_requests: ThreadPullRequestBadge[];
  auto_settle_disabled: boolean;
}

export async function fetchThreadAnnotations(signal?: AbortSignal): Promise<Record<string, ThreadAnnotation>> {
  const res = await apiFetch("/api/control/thread-annotations", { cache: "no-store", signal });
  if (!res.ok) throw await failureOf(res, "对话标注读取失败");
  const body = await res.json() as { threads?: Record<string, ThreadAnnotation> };
  return body.threads ?? {};
}

export async function deleteConversationThread(threadId: string): Promise<void> {
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}`, { method: "DELETE" });
  if (!res.ok) throw await failureOf(res, "删除对话失败");
  if (typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent<{ threadId: string }>("muteki:thread-removed", { detail: { threadId } }));
    window.dispatchEvent(new Event("muteki:threads-changed"));
  }
}

export async function mergeThreadWorktree(threadId: string): Promise<{ outcome: string; message: string }> {
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/git/merge-worktree`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  if (!res.ok) throw await failureOf(res, "合并回项目目录失败");
  return await res.json() as { outcome: string; message: string };
}

export async function regenerateThreadTitle(threadId: string): Promise<string> {
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/title/regenerate`, { method: "POST" });
  if (!res.ok) throw await failureOf(res, "重新生成标题失败");
  const body = await res.json() as { title?: string };
  const title = body.title ?? "";
  if (title && typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent("muteki:thread-patched", { detail: { threadId, patch: { title } } }));
  }
  return title;
}

export async function moveThreadToProject(threadId: string, projectId: string): Promise<{ project_name?: string; changed: boolean }> {
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/move`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ project_id: projectId }),
  });
  if (!res.ok) throw await failureOf(res, "移动对话失败");
  if (typeof window !== "undefined") {
    window.dispatchEvent(new CustomEvent("muteki:thread-patched", { detail: { threadId, patch: { project_id: projectId } } }));
    window.dispatchEvent(new Event("muteki:threads-changed"));
  }
  return await res.json() as { project_name?: string; changed: boolean };
}

export type ThreadExportFormat = "markdown" | "jsonl";

export async function downloadThreadExport(threadId: string, title: string, format: ThreadExportFormat): Promise<void> {
  const scope = currentAuthScope();
  const params = new URLSearchParams({ format, exclude_tools: "0", exclude_paths: "0" });
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/export?${params}`);
  if (!res.ok) throw await failureOf(res, "导出失败");
  conversationExportMetadata(res.headers);
  const blob = await res.blob();
  if (!blob.size) throw new ThreadActionError("conversation.export.empty", "导出文件为空，未保存。", res.status);
  if (currentAuthScope() !== scope) throw new ThreadActionError("conversation.export.scope_changed", "服务范围已改变，旧导出未保存。", 0);
  const filename = conversationExportFilename({
    contentDisposition: res.headers.get("Content-Disposition"), title: title || "conversation", format, excludePaths: false, date: new Date(),
  });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  anchor.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
}
