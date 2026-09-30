import { conversationStorageKey, conversationStorageScope, reportConversationPersistence } from "./conversationStorageScope";

export type ControlIntent = { threadId: string; requestId: string; kind: "approval" | "input"; commandId: string; payload: Record<string, unknown>; state: "sending" | "accepted" | "unknown" | "completed"; error?: string };
const STORAGE_KEY = "muteki:conversation-control:v1";
const EVENT = "muteki:conversation-control";
const memory = new Map<string, Map<string, ControlIntent>>();
const mutations = new Map<string, Map<string, ControlIntent | null>>();
const reportedErrors = new Map<string, string>();
const keyOf = (thread: string, request: string, kind: ControlIntent["kind"]) => JSON.stringify([thread, request, kind]);
function diskRows(scope: string): Map<string, ControlIntent> {
  const raw: unknown = JSON.parse(window.localStorage.getItem(conversationStorageKey(STORAGE_KEY, scope)) || "[]");
  if (!Array.isArray(raw)) throw new Error("conversation.control.invalid: 控制动作恢复记录格式无效");
  const rows = new Map<string, ControlIntent>();
  for (const row of raw) if (row && typeof row.threadId === "string" && typeof row.requestId === "string" && typeof row.commandId === "string" && ["approval", "input"].includes(row.kind)) rows.set(keyOf(row.threadId, row.requestId, row.kind), row);
  return rows;
}
function report(scope: string, error: unknown): void {
  const text = error instanceof Error ? error.message : String(error);
  if (reportedErrors.get(scope) === text) return;
  reportedErrors.set(scope, text); reportConversationPersistence("control", text);
}
function read(scope = conversationStorageScope()): Map<string, ControlIntent> {
  let rows = memory.get(scope) || new Map<string, ControlIntent>();
  if (scope && typeof window !== "undefined") {
    try {
      rows = diskRows(scope); reportedErrors.delete(scope);
    } catch (error) { report(scope, error); }
  }
  for (const [key, row] of mutations.get(scope) || []) { if (row) rows.set(key, row); else rows.delete(key); }
  memory.set(scope, rows); return rows;
}
function write(scope: string, key: string, row: ControlIntent | null): void {
  const pending = mutations.get(scope) || new Map<string, ControlIntent | null>();
  pending.set(key, row); mutations.set(scope, pending);
  const rows = read(scope);
  try {
    if (scope && typeof window !== "undefined") { window.localStorage.setItem(conversationStorageKey(STORAGE_KEY, scope), JSON.stringify(Array.from(rows.values()))); pending.clear(); reportedErrors.delete(scope); }
  } catch (error) { report(scope, error); }
  if (typeof window !== "undefined") window.dispatchEvent(new CustomEvent(EVENT));
}
export function listConversationControls(threadId: string): ControlIntent[] { return Array.from(read().values()).filter((row) => row.threadId === threadId); }
export function beginConversationControl(threadId: string, requestId: string, kind: ControlIntent["kind"], payload: Record<string, unknown>): ControlIntent | null {
  const scope = conversationStorageScope();
  if (!scope) throw new Error("conversation.control.scope_unverified: 当前工作台身份尚未确认");
  const rows = read(scope), key = keyOf(threadId, requestId, kind);
  if (rows.has(key)) return null;
  const intent: ControlIntent = { threadId, requestId, kind, payload: JSON.parse(JSON.stringify(payload)), commandId: crypto.randomUUID(), state: "sending" };
  write(scope, key, intent); return intent;
}
export function updateConversationControl(intent: ControlIntent, patch: Partial<Pick<ControlIntent, "state" | "error">>, scope = conversationStorageScope()): void {
  const rows = read(scope), key = keyOf(intent.threadId, intent.requestId, intent.kind);
  if (rows.get(key)?.commandId !== intent.commandId) return;
  write(scope, key, { ...intent, ...patch });
}
/** Remove only after a definite failed receipt or authoritative request consumption. */
export function clearConversationControl(intent: ControlIntent, scope = conversationStorageScope()): void {
  const rows = read(scope), key = keyOf(intent.threadId, intent.requestId, intent.kind);
  if (rows.get(key)?.commandId !== intent.commandId) return;
  write(scope, key, null);
}
export function subscribeConversationControls(listener: () => void): () => void {
  if (typeof window === "undefined") return () => {};
  const changed = (event: Event) => { if (event.type === EVENT || (event as StorageEvent).key === conversationStorageKey(STORAGE_KEY)) listener(); };
  window.addEventListener(EVENT, changed); window.addEventListener("storage", changed);
  return () => { window.removeEventListener(EVENT, changed); window.removeEventListener("storage", changed); };
}
