import { conversationStorageTransaction } from "./conversationStorageTransaction";
import { conversationStorageKey, conversationStorageScope } from "./conversationStorageScope";
import { clearSendIntent, readSendIntent, clearRetryIntent, readRetryIntent, type SendIntent, type RetryIntent } from "./sendIntentStore";

export type PendingReceiptCheck = { checkedAt: number; result: "missing" | "received" | "error"; receipt?: unknown; error?: string };
export interface ArchivedSendIntent { archivedAt: number; reason: "user_started_new_logical_send"; intent: SendIntent; receiptCheck?: PendingReceiptCheck }
export interface ArchivedRetryIntent {
  archivedAt: number; reason: "user_started_new_logical_retry"; intent: RetryIntent;
  request: { type: "conversation.turn.retry"; threadId: string; turnId: string; payloadCompleteness: "complete" | "legacy_partial"; fileMode?: string; interactionMode?: string };
  receiptCheck?: PendingReceiptCheck;
}
const STORAGE_KEY = "muteki:unconfirmed-send-archive:v1";
const RETRY_STORAGE_KEY = "muteki:unconfirmed-retry-archive:v1";
export function listArchivedSendIntents(): ArchivedSendIntent[] {
  if (typeof window === "undefined" || !conversationStorageScope()) return [];
  const raw = window.localStorage.getItem(conversationStorageKey(STORAGE_KEY));
  if (!raw) return [];
  const rows: unknown = JSON.parse(raw);
  if (!Array.isArray(rows)) throw new Error("send.archive.invalid: 待核对记录格式无效，未丢弃原数据");
  return rows as ArchivedSendIntent[];
}
export async function archiveUnconfirmedSend(draftKey: string, receiptCheck?: PendingReceiptCheck): Promise<ArchivedSendIntent> {
  const intent = readSendIntent(draftKey);
  if (!intent || !["unknown", "sending", "creating_thread"].includes(intent.phase)) throw new Error("send.archive.not_pending: 当前没有未确认的请求");
  if (typeof window === "undefined" || !conversationStorageScope()) throw new Error("send.archive.scope_unverified: 请先确认服务身份");
  const entry: ArchivedSendIntent = { archivedAt: Date.now(), reason: "user_started_new_logical_send", intent, ...(receiptCheck ? { receiptCheck } : {}) };
  const ownerScope = conversationStorageScope();
  await conversationStorageTransaction(ownerScope, () => {
    if (ownerScope !== conversationStorageScope()) throw new Error("send.archive.scope_changed");
    const records = listArchivedSendIntents().filter((row) => row.intent.intentId !== intent.intentId);
    records.push(entry);
    window.localStorage.setItem(conversationStorageKey(STORAGE_KEY, ownerScope), JSON.stringify(records));
  });
  if (ownerScope !== conversationStorageScope()) throw new Error("send.archive.scope_changed");
  await clearSendIntent(draftKey, intent.intentId);
  return entry;
}

export function listArchivedRetryIntents(): ArchivedRetryIntent[] {
  if (typeof window === "undefined" || !conversationStorageScope()) return [];
  const rows: unknown = JSON.parse(window.localStorage.getItem(conversationStorageKey(RETRY_STORAGE_KEY)) || "[]");
  if (!Array.isArray(rows)) throw new Error("retry.archive.invalid: 待核对重试记录格式无效，未丢弃原数据");
  return rows as ArchivedRetryIntent[];
}

export async function archiveUnconfirmedRetry(threadId: string, turnId: string, expectedCommandId: string, receiptCheck?: PendingReceiptCheck): Promise<ArchivedRetryIntent> {
  const intent = readRetryIntent(threadId, turnId);
  if (!intent || intent.commandId !== expectedCommandId || !["unknown", "sending"].includes(intent.phase)) throw new Error("retry.archive.not_pending: 原重试身份已改变，请重新核对");
  const scope = conversationStorageScope();
  if (!scope || typeof window === "undefined") throw new Error("retry.archive.scope_unverified: 请先确认服务身份");
  const entry: ArchivedRetryIntent = {
    archivedAt: Date.now(), reason: "user_started_new_logical_retry", intent,
    request: { type: "conversation.turn.retry", threadId, turnId, payloadCompleteness: intent.request ? "complete" : "legacy_partial", ...intent.request },
    ...(receiptCheck ? { receiptCheck } : {}),
  };
  await conversationStorageTransaction(scope, () => {
    if (scope !== conversationStorageScope()) throw new Error("retry.archive.scope_changed");
    const records = listArchivedRetryIntents().filter(row => row.intent.commandId !== intent.commandId);
    records.push(entry);
    window.localStorage.setItem(conversationStorageKey(RETRY_STORAGE_KEY, scope), JSON.stringify(records));
  });
  if (scope !== conversationStorageScope()) throw new Error("retry.archive.scope_changed");
  await clearRetryIntent(threadId, turnId, intent.commandId);
  return entry;
}
