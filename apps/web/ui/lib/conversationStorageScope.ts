/** Verified service + identity own local conversation recovery state. */
let scope = "";
const listeners = new Set<() => void>();
const beforeListeners = new Set<() => void>();

export function conversationStorageScope(): string { return scope; }
export function conversationStorageKey(base: string, ownerScope = scope): string {
  return ownerScope ? `${base}:scope:${encodeURIComponent(ownerScope)}` : base;
}
export function setConversationStorageScope(next: string): void {
  const value = String(next || "").trim();
  if (value === scope) return;
  for (const listener of beforeListeners) { try { listener(); } catch (error) { console.error("conversation.storage.before_scope_failed", error); } }
  scope = value;
  for (const listener of listeners) {
    try { listener(); } catch (error) { console.error("conversation.storage.scope_cleanup_failed", error); }
  }
}
export function subscribeConversationStorageScope(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function reportConversationPersistence(source: string, error: string, key?: string): void {
  if (typeof window !== "undefined") window.dispatchEvent(new CustomEvent("muteki:persistence-error", { detail: { source, error, key } }));
}

export function subscribeBeforeConversationStorageScope(listener: () => void): () => void {
  beforeListeners.add(listener);
  return () => { beforeListeners.delete(listener); };
}
