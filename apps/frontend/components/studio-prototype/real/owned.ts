import type { StudioState } from "../store";

export type OwnedFieldMerge<K extends keyof StudioState = keyof StudioState> = {
  replace: ReadonlyArray<K>;
  combines: Partial<Record<K, Record<string, (local: unknown, server: unknown) => unknown>>>;
};

const registry = new Map<keyof StudioState, { replace: boolean; combines?: Record<string, (local: unknown, server: unknown) => unknown> }>();

export function setOwnedFields(spec: OwnedFieldMerge | null) {
  registry.clear();
  if (!spec) return;
  for (const key of spec.replace) registry.set(key, { replace: true });
  for (const [key, byDomain] of Object.entries(spec.combines)) {
    registry.set(key as keyof StudioState, { replace: false, combines: byDomain });
  }
}

export function sliceValueForBackend(sliceFields: ReadonlyArray<keyof StudioState>, value: Record<keyof StudioState, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const field of sliceFields) {
    const owned = registry.get(field);
    if (owned?.replace || owned?.combines) continue;
    out[field] = value[field];
  }
  return out;
}

export function isBackendExcludedField(field: keyof StudioState): boolean {
  const owned = registry.get(field);
  return Boolean(owned?.replace || owned?.combines);
}

export function combineOwnedField(field: keyof StudioState, local: unknown, server: unknown, domainId: string): unknown {
  const owned = registry.get(field);
  const merge = owned?.combines?.[domainId];
  if (merge) return merge(local, server);
  return server;
}
