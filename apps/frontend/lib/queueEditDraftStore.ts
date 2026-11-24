import { conversationStorageKey, conversationStorageScope, reportConversationPersistence, subscribeConversationStorageScope, subscribeBeforeConversationStorageScope } from "./conversationStorageScope";

export interface QueueEditDraft { queueId: string; text: string; updatedAt: number }
const memory = new Map<string, QueueEditDraft | null>();
const pending = new Set<string>();
const key = (threadId: string, ownerScope = conversationStorageScope()) => conversationStorageKey(`muteki:queue-edit:v1:${encodeURIComponent(threadId)}`, ownerScope);
export function loadQueueEditDraft(threadId: string): QueueEditDraft | null {
  if (pending.has(threadId)) return memory.get(threadId) || null;
  try {
    if (typeof window === "undefined" || !conversationStorageScope()) return memory.get(threadId) || null;
    const raw = window.localStorage.getItem(key(threadId));
    const row = raw ? JSON.parse(raw) : null;
    const draft = row && typeof row.queueId === "string" && typeof row.text === "string" ? row as QueueEditDraft : null;
    memory.set(threadId, draft);
    return draft;
  } catch { return memory.get(threadId) || null; }
}
function flushQueueMemory(ownerScope: string, drafts: typeof memory, edits: typeof pending): { persisted: boolean; error?: string } {
  if (!edits.size) return { persisted: true };
  const errors: string[] = [];
  for (const threadId of edits) {
    try {
      if (typeof window === "undefined" || !ownerScope) throw new Error("queue.edit.storage_unavailable: 未提交队列编辑仅保留在当前窗口");
      const draft = drafts.get(threadId);
      if (draft) window.localStorage.setItem(key(threadId, ownerScope), JSON.stringify(draft));
      else window.localStorage.removeItem(key(threadId, ownerScope));
      edits.delete(threadId);
    } catch (failure) { errors.push(failure instanceof Error ? `${failure.name}: ${failure.message}` : String(failure)); }
  }
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}
export function flushQueueEditDraftStore(): { persisted: boolean; error?: string } {
  const results = [flushQueueMemory(conversationStorageScope(), memory, pending)];
  for (const [scope, state] of scopedEdits) if (scope !== conversationStorageScope()) results.push(flushQueueMemory(scope, state.drafts, state.pending));
  const errors = results.filter((row) => !row.persisted).map((row) => row.error || "队列编辑保存失败");
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}
export function saveQueueEditDraft(threadId: string, draft: QueueEditDraft | null): { persisted: boolean; error?: string } {
  memory.set(threadId, draft); pending.add(threadId);
  const result = flushQueueEditDraftStore();
  if (!result.persisted) reportConversationPersistence("queue-edit", result.error || "未提交队列编辑保存失败");
  return result;
}
const scopedEdits = new Map<string, { drafts: typeof memory; pending: typeof pending }>();
subscribeBeforeConversationStorageScope(() => {
  flushQueueEditDraftStore();
  scopedEdits.set(conversationStorageScope(), { drafts: new Map(memory), pending: new Set(pending) });
});
subscribeConversationStorageScope(() => {
  const old = scopedEdits.get(conversationStorageScope()); memory.clear(); pending.clear();
  for (const [key, value] of old?.drafts || []) memory.set(key, value);
  for (const key of old?.pending || []) pending.add(key);
});
