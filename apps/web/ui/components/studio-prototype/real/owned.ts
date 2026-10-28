/**
 * Registry of StudioState fields owned by attached real domains. Server
 * records are authoritative for owned fields: the workbench_slice driver
 * excludes them from what it saves, and domain reloads replace them.
 *
 * A field may be owned only partially (e.g. `merges` holds both source-kind
 * merges owned by the asset domain and clue-kind merges Phase C will own).
 * `mergeLocal` describes how a reloaded server value combines with the
 * non-owned part that still lives in the persisted slice.
 */
import type { StudioState } from "../store";

export type OwnedFieldMerge<K extends keyof StudioState = keyof StudioState> = {
  /** Fields whose entire value a domain replaces on reload. */
  replace: ReadonlyArray<K>;
  /** field -> domainId -> merge fn; each domain declares how ITS reload merges. */
  combines: Partial<Record<K, Record<string, (local: unknown, server: unknown) => unknown>>>;
};

const registry = new Map<keyof StudioState, { replace: boolean; combines?: Record<string, (local: unknown, server: unknown) => unknown> }>();

/** Called by the store when domains attach; fields not listed stay locally persisted. */
export function setOwnedFields(spec: OwnedFieldMerge | null) {
  registry.clear();
  if (!spec) return;
  for (const key of spec.replace) registry.set(key, { replace: true });
  for (const [key, byDomain] of Object.entries(spec.combines)) {
    registry.set(key as keyof StudioState, { replace: false, combines: byDomain });
  }
}

/** The subset of a persisted slice's data the backend driver should still save. */
export function sliceValueForBackend(sliceFields: ReadonlyArray<keyof StudioState>, value: Record<keyof StudioState, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const field of sliceFields) {
    const owned = registry.get(field);
    if (owned?.replace) continue;
    out[field] = value[field];
  }
  return out;
}

/** Whether any attached domain owns (even partially) one of the slice's fields. */
export function sliceHasOwnedFields(sliceFields: ReadonlyArray<keyof StudioState>): boolean {
  return sliceFields.some((field) => registry.has(field));
}

/** Merge a reload for one domain; the domain's own combine keeps non-owned local parts. */
export function combineOwnedField(field: keyof StudioState, local: unknown, server: unknown, domainId: string): unknown {
  const owned = registry.get(field);
  const merge = owned?.combines?.[domainId];
  if (merge) return merge(local, server);
  return server;
}
