"use client";
import { conversationStorageKey, conversationStorageScope, subscribeBeforeConversationStorageScope, reportConversationPersistence } from "./conversationStorageScope";

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
  /** Complete receipt body; commandId remains the authoritative server lookup reference. */
  rawReceipt?: unknown;
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

type ReceiptMemory = { rows: Map<string, TaskReceiptRecord>; pending: Map<string, TaskReceiptRecord> };
const memories = new Map<string, ReceiptMemory>();
function memory(scope: string): ReceiptMemory {
  let state = memories.get(scope);
  if (!state) { state = { rows: new Map(), pending: new Map() }; memories.set(scope, state); }
  return state;
}
function parseReceipts(raw: string | null): TaskReceiptRecord[] {
  const rows: unknown = JSON.parse(raw || "[]");
  if (!Array.isArray(rows)) throw new Error("task_receipt.storage.invalid: 本地操作回执格式无效");
  return rows.filter((row): row is TaskReceiptRecord => Boolean(row && typeof row.commandId === "string"));
}
/** Local display cache is secondary evidence; failure must never alter a server receipt. */
export function readTaskReceipts(): TaskReceiptRecord[] {
  const scope = conversationStorageScope();
  if (typeof window === "undefined" || !scope) return [];
  const state = memory(scope);
  try {
    const disk = parseReceipts(window.localStorage.getItem(conversationStorageKey(TASK_CENTER_STORAGE, scope)));
    state.rows = new Map(disk.map((row) => [row.commandId, row]));
  } catch (error) {
    reportConversationPersistence("task-receipt", error instanceof Error ? error.message : String(error));
  }
  for (const [id, row] of state.pending) state.rows.set(id, row);
  return Array.from(state.rows.values()).sort((a, b) => a.createdAt - b.createdAt || a.commandId.localeCompare(b.commandId));
}
export function flushTaskReceiptStore(): { persisted: boolean; error?: string } {
  if (typeof window === "undefined") return { persisted: true };
  const errors: string[] = [];
  for (const [scope, state] of memories) {
    if (!scope || !state.pending.size) continue;
    try {
      const rows = new Map(parseReceipts(window.localStorage.getItem(conversationStorageKey(TASK_CENTER_STORAGE, scope))).map((row) => [row.commandId, row]));
      for (const [id, row] of state.pending) rows.set(id, row);
      window.localStorage.setItem(conversationStorageKey(TASK_CENTER_STORAGE, scope), JSON.stringify(Array.from(rows.values())));
      state.rows = rows; state.pending.clear();
    } catch (error) { errors.push(error instanceof Error ? `${error.name}: ${error.message}` : String(error)); }
  }
  const result = errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
  if (result.error) reportConversationPersistence("task-receipt", result.error);
  return result;
}
function writeTaskReceipts(rows: TaskReceiptRecord[], changed: TaskReceiptRecord, scope: string): void {
  if (!scope) return;
  const state = memory(scope);
  state.rows = new Map(rows.map((row) => [row.commandId, row]));
  state.pending.set(changed.commandId, changed);
  flushTaskReceiptStore();
  window.dispatchEvent(new CustomEvent(TASK_CENTER_EVENT));
}
subscribeBeforeConversationStorageScope(() => { flushTaskReceiptStore(); });

export function recordTaskReceipt(
  receipt: ReceiptLike,
  label: string,
  domain = "platform",
  ownerScope?: string,
): void {
  const commandId = String(receipt.command_id || "").trim();
  if (!commandId || typeof window === "undefined") return;
  if (domain === "conversation" && ownerScope === undefined) {
    reportConversationPersistence("task-receipt", "task_receipt.scope_unverified: 操作已返回原回执；缓存调用缺少请求发起时的工作台身份，未把它写入当前工作台");
    return;
  }
  const scope = ownerScope ?? conversationStorageScope();
  if (!scope) return;
  const now = Date.now();
  const state = memory(scope);
  try {
    state.rows = new Map(parseReceipts(window.localStorage.getItem(conversationStorageKey(TASK_CENTER_STORAGE, scope))).map((row) => [row.commandId, row]));
  } catch (error) { reportConversationPersistence("task-receipt", error instanceof Error ? error.message : String(error)); }
  for (const [id, pending] of state.pending) state.rows.set(id, pending);
  const rows = Array.from(state.rows.values());
  let rawReceipt: unknown;
  try { rawReceipt = typeof structuredClone === "function" ? structuredClone(receipt) : JSON.parse(JSON.stringify(receipt)); }
  catch (error) { reportConversationPersistence("task-receipt", `task_receipt.snapshot_failed: 原回执可按命令 ${commandId} 查询；${error instanceof Error ? error.message : String(error)}`); }
  const index = rows.findIndex((row) => row.commandId === commandId);
  const previous = index >= 0 ? rows[index] : undefined;
  const next: TaskReceiptRecord = {
    commandId,
    receiptId: receipt.receipt_id || previous?.receiptId,
    label: label || previous?.label || commandId,
    domain: domain || previous?.domain || "platform",
    state: receipt.state || previous?.state || "accepted",
    aggregate: receipt.aggregate === undefined ? previous?.aggregate : receipt.aggregate,
    runId: receipt.run_id === undefined ? previous?.runId : receipt.run_id,
    effectIds: receipt.effect_ids || previous?.effectIds || [],
    error: receipt.error === undefined ? previous?.error : receipt.error,
    createdAt: previous?.createdAt || now,
    updatedAt: now,
    rawReceipt,
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
      rawReceipt: previous.rawReceipt,
    }) === JSON.stringify({
      receiptId: next.receiptId,
      label: next.label,
      domain: next.domain,
      state: next.state,
      aggregate: next.aggregate,
      runId: next.runId,
      effectIds: next.effectIds,
      error: next.error,
      rawReceipt: next.rawReceipt,
    });
    // 轮询返回同一回执时不改 localStorage、不广播事件。否则每次广播都会
    // 重新建立轮询 effect，并立即再发请求，形成无等待的请求循环。
    if (unchanged) { if (state.pending.has(commandId)) flushTaskReceiptStore(); return; }
  }
  if (index >= 0) rows[index] = next;
  else rows.push(next);
  writeTaskReceipts(rows, next, scope);
}
