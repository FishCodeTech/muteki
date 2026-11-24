import { conversationStorageTransaction } from "./conversationStorageTransaction";
import { conversationStorageKey, conversationStorageScope, reportConversationPersistence } from "./conversationStorageScope";

export type ControlIntent = { ownerScope?: string; threadId: string; requestId: string; kind: "approval" | "input" | "steer"; commandId: string; payload: Record<string, unknown>; state: "sending" | "accepted" | "unknown" | "completed"; error?: string; composer?: { draftKey: string; documentFingerprint: string; projectId: string } };
const STORAGE_KEY = "muteki:conversation-control:v1";
const EVENT = "muteki:conversation-control";
const memory = new Map<string, Map<string, ControlIntent>>();
const mutations = new Map<string, Map<string, ControlIntent | null>>();
const deletedOwners = new Map<string, Map<string, string>>();
const reportedErrors = new Map<string, string>();
const keyOf = (thread: string, request: string, kind: ControlIntent["kind"]) => JSON.stringify([thread, request, kind]);
function diskRows(scope: string): Map<string, ControlIntent> {
  const raw: unknown = JSON.parse(window.localStorage.getItem(conversationStorageKey(STORAGE_KEY, scope)) || "[]");
  if (!Array.isArray(raw)) throw new Error("conversation.control.invalid: 控制动作恢复记录格式无效");
  const rows = new Map<string, ControlIntent>();
  for (const row of raw) if (row && typeof row.threadId === "string" && typeof row.requestId === "string" && typeof row.commandId === "string" && ["approval", "input", "steer"].includes(row.kind)) rows.set(keyOf(row.threadId, row.requestId, row.kind), row);
  return rows;
}
function report(scope: string, error: unknown): void {
  const text = error instanceof Error ? error.message : String(error);
  if (reportedErrors.get(scope) === text) return;
  reportedErrors.set(scope, text); reportConversationPersistence("control", text);
}
function applyMutation(rows: Map<string, ControlIntent>, scope: string, key: string, row: ControlIntent | null): void {
  const current = rows.get(key);
  if (row) {
    if (current && (current.commandId !== row.commandId || (current.state === "completed" && row.state !== "completed"))) return;
    rows.set(key, row);
  } else if (current?.commandId === deletedOwners.get(scope)?.get(key)) rows.delete(key);
}
function read(scope = conversationStorageScope()): Map<string, ControlIntent> {
  let rows = memory.get(scope) || new Map<string, ControlIntent>();
  if (scope && typeof window !== "undefined") {
    try {
      rows = diskRows(scope); reportedErrors.delete(scope);
    } catch (error) { report(scope, error); }
  }
  for (const [key, row] of mutations.get(scope) || []) applyMutation(rows, scope, key, row);
  memory.set(scope, rows); return rows;
}
function write(scope: string, key: string, row: ControlIntent | null): void {
  const pending = mutations.get(scope) || new Map<string, ControlIntent | null>();
  if (!row) {
    const owners = deletedOwners.get(scope) || new Map<string, string>();
    const commandId = read(scope).get(key)?.commandId;
    if (commandId) owners.set(key, commandId);
    deletedOwners.set(scope, owners);
  }
  pending.set(key, row); mutations.set(scope, pending);
  const rows = read(scope);
  void rows;
  void flushConversationControlStore();
  if (typeof window !== "undefined") window.dispatchEvent(new CustomEvent(EVENT));
}
export function listConversationControls(threadId: string): ControlIntent[] { return Array.from(read().values()).filter((row) => row.threadId === threadId); }
export async function beginConversationControl(threadId: string, requestId: string, kind: ControlIntent["kind"], payload: Record<string, unknown>, composer?: ControlIntent["composer"]): Promise<ControlIntent | null> {
  const scope = conversationStorageScope();
  if (!scope) throw new Error("conversation.control.scope_unverified: 当前工作台身份尚未确认");
  return conversationStorageTransaction(scope, () => {
  if (scope !== conversationStorageScope()) throw new Error("conversation.control.scope_changed: 原工作台身份已改变");
  const rows = read(scope), key = keyOf(threadId, requestId, kind);
  if (rows.has(key)) return null;
  const intent: ControlIntent = { ownerScope: scope, threadId, requestId, kind, payload: JSON.parse(JSON.stringify(payload)), commandId: typeof crypto !== "undefined" && typeof crypto.randomUUID === "function" ? crypto.randomUUID() : `cmd_control_${Date.now().toString(36)}_${Math.random().toString(36).slice(2)}`, state: "sending", ...(composer ? { composer } : {}) };
  rows.set(key, intent);
  try { window.localStorage.setItem(conversationStorageKey(STORAGE_KEY, scope), JSON.stringify(Array.from(rows.values()))); }
  catch (error) { report(scope, error); throw error; }
  memory.set(scope, rows);
  window.dispatchEvent(new CustomEvent(EVENT));
  return intent;
  });
}
export function updateConversationControl(intent: ControlIntent, patch: Partial<Pick<ControlIntent, "state" | "error">>, scope = conversationStorageScope()): void {
  const rows = read(scope), key = keyOf(intent.threadId, intent.requestId, intent.kind);
  if (rows.get(key)?.commandId !== intent.commandId) return;
  const current = rows.get(key)!;
  if (current.state === "completed" && patch.state && patch.state !== "completed") return;
  write(scope, key, { ...current, ...patch });
}
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

export async function flushConversationControlStore(): Promise<{ persisted: boolean; error?: string }> {
  const errors: string[] = [];
  for (const [scope, pending] of mutations) {
    if (!scope || !pending.size) continue;
    try { await conversationStorageTransaction(scope, () => {
      const rows = diskRows(scope);
      for (const [key, row] of pending) applyMutation(rows, scope, key, row);
      window.localStorage.setItem(conversationStorageKey(STORAGE_KEY, scope), JSON.stringify(Array.from(rows.values())));
      memory.set(scope, rows); pending.clear(); deletedOwners.delete(scope); reportedErrors.delete(scope);
    }); } catch (error) { report(scope, error); errors.push(error instanceof Error ? error.message : String(error)); }
  }
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}
