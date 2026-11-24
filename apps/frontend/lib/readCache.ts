import { currentAuthGeneration, currentAuthScope, currentServiceOrigin, onAuthRequired } from "./serviceAuth";

import { subscribeConversationStorageScope } from "./conversationStorageScope";

export function subscribeReadCacheIdentity(listener: () => void): () => void {
  const offScope = subscribeConversationStorageScope(listener);
  const offAuth = onAuthRequired(listener);
  return () => { offScope(); offAuth(); };
}

export function readCacheIdentity(): string {
  return JSON.stringify([currentServiceOrigin(), currentAuthScope(), currentAuthGeneration()]);
}

export function createReadCache<T>(ttlMs: number, maxEntries = 32) {
  type Entry = { data?: T; updatedAt: number; pending?: Promise<T>; forced?: boolean };
  let identity = "";
  const entries = new Map<string, Entry>();
  const scopedEntries = () => {
    const next = readCacheIdentity();
    if (next !== identity) { entries.clear(); identity = next; }
    return entries;
  };
  return {
    snapshot(key: string): T | null { return scopedEntries().get(key)?.data ?? null; },
    invalidate(key: string) { scopedEntries().delete(key); },
    read(key: string, loader: () => Promise<T>, force = false): Promise<T> {
      const cache = scopedEntries();
      const previous = cache.get(key);
      if (previous?.pending && (!force || previous.forced)) return previous.pending;
      if (!force && previous?.data !== undefined && Date.now() - previous.updatedAt < ttlMs) return Promise.resolve(previous.data);
      const entry: Entry = { data: previous?.data, updatedAt: previous?.updatedAt ?? 0, forced: force };
      const requestIdentity = identity;
      const pending = loader().then(data => {
        if (requestIdentity === readCacheIdentity() && cache.get(key) === entry) {
          entry.data = data;
          entry.updatedAt = Date.now();
        }
        return data;
      }).finally(() => { if (cache.get(key) === entry) entry.pending = undefined; });
      entry.pending = pending;
      cache.delete(key);
      cache.set(key, entry);
      while (cache.size > maxEntries) cache.delete(cache.keys().next().value!);
      return pending;
    },
  };
}
