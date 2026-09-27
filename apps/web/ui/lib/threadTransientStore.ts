/** Pure thread-scoped string store helpers (no React). */

export type ThreadTransientStore = Record<string, string>;

export function readThreadScopedString(
  store: ThreadTransientStore,
  scopeKey: string,
): string {
  return store[scopeKey] ?? "";
}

export function writeThreadScopedString(
  store: ThreadTransientStore,
  scopeKey: string,
  value: string,
): ThreadTransientStore {
  if (!value) {
    if (!(scopeKey in store)) return store;
    const next = { ...store };
    delete next[scopeKey];
    return next;
  }
  if (store[scopeKey] === value) return store;
  return { ...store, [scopeKey]: value };
}
