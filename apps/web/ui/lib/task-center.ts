"use client";

export type TaskState =
  | "accepted"
  | "running"
  | "waiting"
  | "completed"
  | "failed"
  | "conflict"
  | "cancelled";

export interface TaskReceiptRecord {
  commandId: string;
  receiptId?: string;
  label: string;
  domain: string;
  state: TaskState | string;
  aggregate?: { type?: string; id?: string } | null;
  runId?: string | null;
  effectIds?: string[];
  error?: {
    code?: string;
    message?: string;
    recovery_hint?: string;
    correlation_id?: string;
  } | null;
  createdAt: number;
  updatedAt: number;
}

type ReceiptLike = {
  command_id?: string;
  receipt_id?: string;
  state?: string;
  aggregate?: { type?: string; id?: string } | null;
  run_id?: string | null;
  effect_ids?: string[];
  error?: TaskReceiptRecord["error"];
};

const TASK_CENTER_EVENT = "muteki:task-receipt";
const TASK_CENTER_STORAGE = "muteki.task-receipts.v1";

function readTaskReceipts(): TaskReceiptRecord[] {
  if (typeof window === "undefined") return [];
  try {
    const rows = JSON.parse(
      window.localStorage.getItem(TASK_CENTER_STORAGE) || "[]",
    ) as TaskReceiptRecord[];
    return Array.isArray(rows)
      ? rows
          .filter((row) => row && typeof row.commandId === "string")
          .slice(-200)
      : [];
  } catch {
    return [];
  }
}

function writeTaskReceipts(rows: TaskReceiptRecord[]): void {
  if (typeof window === "undefined") return;
  window.localStorage.setItem(
    TASK_CENTER_STORAGE,
    JSON.stringify(rows.slice(-200)),
  );
  window.dispatchEvent(new CustomEvent(TASK_CENTER_EVENT));
}

export function recordTaskReceipt(
  receipt: ReceiptLike,
  label: string,
  domain = "platform",
): void {
  const commandId = String(receipt.command_id || "").trim();
  if (!commandId || typeof window === "undefined") return;
  const now = Date.now();
  const rows = readTaskReceipts();
  const index = rows.findIndex((row) => row.commandId === commandId);
  const previous = index >= 0 ? rows[index] : undefined;
  const next: TaskReceiptRecord = {
    commandId,
    receiptId: receipt.receipt_id || previous?.receiptId,
    label: label || previous?.label || commandId,
    domain: domain || previous?.domain || "platform",
    state: receipt.state || previous?.state || "accepted",
    aggregate: receipt.aggregate || previous?.aggregate,
    runId: receipt.run_id ?? previous?.runId,
    effectIds: receipt.effect_ids || previous?.effectIds || [],
    error: receipt.error ?? previous?.error,
    createdAt: previous?.createdAt || now,
    updatedAt: now,
  };
  if (previous) {
    const unchanged = JSON.stringify({
      receiptId: previous.receiptId,
      label: previous.label,
      domain: previous.domain,
      state: previous.state,
      aggregate: previous.aggregate,
      runId: previous.runId,
      effectIds: previous.effectIds,
      error: previous.error,
    }) === JSON.stringify({
      receiptId: next.receiptId,
      label: next.label,
      domain: next.domain,
      state: next.state,
      aggregate: next.aggregate,
      runId: next.runId,
      effectIds: next.effectIds,
      error: next.error,
    });
    // 轮询返回同一回执时不改 localStorage、不广播事件。否则每次广播都会
    // 重新建立轮询 effect，并立即再发请求，形成无等待的请求循环。
    if (unchanged) return;
  }
  if (index >= 0) rows[index] = next;
  else rows.push(next);
  writeTaskReceipts(rows);
}
