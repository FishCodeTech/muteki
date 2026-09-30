import { conversationStorageKey, conversationStorageScope } from "./conversationStorageScope";
import { clearSendIntent, readSendIntent, type SendIntent } from "./sendIntentStore";

export interface ArchivedSendIntent { archivedAt: number; reason: "user_started_new_logical_send"; intent: SendIntent }
const STORAGE_KEY = "muteki:unconfirmed-send-archive:v1";
export function listArchivedSendIntents(): ArchivedSendIntent[] {
  if (typeof window === "undefined" || !conversationStorageScope()) return [];
  const raw = window.localStorage.getItem(conversationStorageKey(STORAGE_KEY));
  if (!raw) return [];
  const rows: unknown = JSON.parse(raw);
  if (!Array.isArray(rows)) throw new Error("send.archive.invalid: 待核对记录格式无效，未丢弃原数据");
  return rows as ArchivedSendIntent[];
}
/** This preserves the old request; it does not cancel it or prove non-acceptance. */
export function archiveUnconfirmedSend(draftKey: string): ArchivedSendIntent {
  const intent = readSendIntent(draftKey);
  if (!intent || !["unknown", "sending", "creating_thread"].includes(intent.phase)) throw new Error("send.archive.not_pending: 当前没有未确认的请求");
  if (typeof window === "undefined" || !conversationStorageScope()) throw new Error("send.archive.scope_unverified: 请先确认服务身份");
  const entry: ArchivedSendIntent = { archivedAt: Date.now(), reason: "user_started_new_logical_send", intent };
  const records = listArchivedSendIntents().filter((row) => row.intent.intentId !== intent.intentId);
  records.push(entry);
  // Persist the full original identity and payload before unlocking this draft.
  window.localStorage.setItem(conversationStorageKey(STORAGE_KEY), JSON.stringify(records));
  clearSendIntent(draftKey);
  return entry;
}
