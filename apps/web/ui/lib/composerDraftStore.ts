/**
 * Per-composer-session draft persistence for Conversation.
 *
 * Keys drafts by thread id or ephemeral new-chat draft id so switching threads
 * does not leak or lose unsent prompt text, capability refs, model selection,
 * or attachment metadata. File blobs stay in an in-memory bag for same-tab
 * switches; after refresh, local-only files are marked needsReselect while
 * server-confirmed sha256 refs remain sendable.
 */

import type { ComposerCapabilityRef } from "./composerCapabilities";

export const COMPOSER_DRAFT_STORAGE_KEY = "muteki:composer-drafts:v2";
export const COMPOSER_DRAFT_STORAGE_KEY_V1 = "muteki:composer-drafts:v1";
const PERSIST_DEBOUNCE_MS = 300;

export type ComposerDraftAttachment = {
  id: string;
  name: string;
  size?: number;
  mimeType?: string;
  /** Present after the file was uploaded and the server returned sha256. */
  sha256?: string;
  /**
   * True when the File handle is gone (e.g. after refresh) and there is no
   * sha256 yet — the operator must pick the file again before send.
   */
  needsReselect?: boolean;
};

export type ComposerDraft = {
  prompt: string;
  /** Ordered prompt document segments (text + inline ref nodes). */
  promptSegments?: Array<
    | { type: "text"; text: string }
    | { type: "ref"; nodeId: string }
  >;
  capabilityRefs: ComposerCapabilityRef[];
  attachments: ComposerDraftAttachment[];
  credentialId?: string;
  /** Exact adapter:instance identity; optional for legacy saved entries. */
  runtimeKey?: string;
  model?: string;
  effort?: string;
  accessMode?: string;
  projectId?: string;
  updatedAt: number;
};

export type ComposerDraftBucket = Record<string, ComposerDraft>;

type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

const emptyDraft = (): ComposerDraft => ({
  prompt: "",
  capabilityRefs: [],
  attachments: [],
  updatedAt: 0,
});

/** In-memory File handles keyed by `${draftKey}::${attachmentId}`. */
const fileBag = new Map<string, File>();

let memoryBucket: ComposerDraftBucket = {};
let memoryDirty = false;
let persistTimer: ReturnType<typeof setTimeout> | null = null;
let storageOverride: StorageLike | null = null;

function fileBagKey(draftKey: string, attachmentId: string): string {
  return `${draftKey}::${attachmentId}`;
}

function getStorage(): StorageLike | null {
  if (storageOverride) return storageOverride;
  if (typeof window === "undefined") return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function parseDraftBucket(raw: string | null): ComposerDraftBucket {
  if (!raw) return {};
  const parsed: unknown = JSON.parse(raw);
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
  const row = parsed as Record<string, unknown>;
  const draftsRaw = (
    row.drafts && typeof row.drafts === "object" && !Array.isArray(row.drafts)
      ? row.drafts
      : parsed
  ) as Record<string, unknown>;
  const next: ComposerDraftBucket = {};
  for (const [key, value] of Object.entries(draftsRaw)) {
    const draft = normalizeComposerDraft(value);
    if (draft && !isComposerDraftEmpty(draft)) next[key] = draft;
  }
  return next;
}

function migrateV1Drafts(storage: StorageLike): ComposerDraftBucket {
  try {
    const v1 = parseDraftBucket(storage.getItem(COMPOSER_DRAFT_STORAGE_KEY_V1));
    if (!Object.keys(v1).length) return {};
    // Promote v1 → v2 once, then leave v1 in place for older tabs.
    storage.setItem(
      COMPOSER_DRAFT_STORAGE_KEY,
      JSON.stringify({ drafts: v1 }),
    );
    return v1;
  } catch {
    return {};
  }
}

function readBucketFromStorage(): ComposerDraftBucket {
  // In-memory bucket may be ahead of the debounced disk write.
  if (memoryDirty) return { ...memoryBucket };
  const storage = getStorage();
  if (!storage) return { ...memoryBucket };
  try {
    let next = parseDraftBucket(storage.getItem(COMPOSER_DRAFT_STORAGE_KEY));
    if (!Object.keys(next).length) {
      next = migrateV1Drafts(storage);
    }
    memoryBucket = next;
    return { ...memoryBucket };
  } catch {
    return { ...memoryBucket };
  }
}

function writeBucketToStorage(bucket: ComposerDraftBucket): void {
  memoryBucket = { ...bucket };
  memoryDirty = false;
  const storage = getStorage();
  if (!storage) return;
  try {
    storage.setItem(
      COMPOSER_DRAFT_STORAGE_KEY,
      JSON.stringify({ drafts: memoryBucket }),
    );
  } catch {
    // Quota / private mode — keep memory copy only.
  }
}

function schedulePersist(bucket: ComposerDraftBucket): void {
  memoryBucket = { ...bucket };
  memoryDirty = true;
  if (typeof window === "undefined" && !storageOverride) return;
  if (persistTimer) clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    persistTimer = null;
    writeBucketToStorage(memoryBucket);
  }, PERSIST_DEBOUNCE_MS);
}

/** Flush pending debounced writes (call on beforeunload / tests). */
export function flushComposerDraftStore(): void {
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  // Avoid clobbering disk when this JS realm never wrote a draft (e.g. empty
  // module state flushing on unload after an external localStorage update).
  if (!memoryDirty) return;
  writeBucketToStorage(memoryBucket);
}

export function composerDraftKey(threadId: string, draftParam = ""): string {
  const thread = String(threadId || "").trim();
  if (thread) return `thread:${thread}`;
  const draft = String(draftParam || "").trim();
  return draft ? `draft:${draft}` : "draft:home";
}

export function isComposerDraftEmpty(draft: ComposerDraft | null | undefined): boolean {
  if (!draft) return true;
  if (draft.prompt.trim()) return false;
  if (draft.promptSegments?.some((segment) => (
    segment.type === "ref" || (segment.type === "text" && segment.text.trim())
  ))) return false;
  if (draft.capabilityRefs.length) return false;
  if (draft.attachments.length) return false;
  // Model / project selection alone is still a meaningful unsent draft.
  if (draft.credentialId || draft.model || draft.effort || draft.projectId) return false;
  if (draft.accessMode && draft.accessMode !== "supervised") return false;
  return true;
}

function normalizeCapabilityRefs(value: unknown): ComposerCapabilityRef[] {
  if (!Array.isArray(value)) return [];
  const out: ComposerCapabilityRef[] = [];
  for (const item of value) {
    if (!item || typeof item !== "object") continue;
    const row = item as Record<string, unknown>;
    const id = String(row.id || row.node_id || "").trim();
    const kind = String(row.kind || "").trim();
    const name = String(row.name || "").trim();
    if (!id || !kind || !name) continue;
    const ref: ComposerCapabilityRef = {
      id,
      kind: kind as ComposerCapabilityRef["kind"],
      name,
      description: String(row.description || ""),
      source: String(row.source || ""),
      scope: String(row.scope || ""),
    };
    const nodeId = String(row.node_id || "").trim();
    if (nodeId) ref.node_id = nodeId;
    if (row.locator && typeof row.locator === "object") {
      ref.locator = row.locator as ComposerCapabilityRef["locator"];
    }
    if (row.snapshot && typeof row.snapshot === "object") {
      ref.snapshot = row.snapshot as ComposerCapabilityRef["snapshot"];
    }
    const status = String(row.status || "").trim();
    if (status === "ok" || status === "stale" || status === "missing" || status === "forbidden") {
      ref.status = status;
    }
    const statusReason = String(row.status_reason || "").trim();
    if (statusReason) ref.status_reason = statusReason;
    const legacy = String(row.legacy_capability_id || "").trim();
    if (legacy) ref.legacy_capability_id = legacy;
    if (Number(row.context_schema) === 2) ref.context_schema = 2;
    out.push(ref);
  }
  return out;
}

function normalizePromptSegments(value: unknown): ComposerDraft["promptSegments"] {
  if (!Array.isArray(value)) return undefined;
  const out: NonNullable<ComposerDraft["promptSegments"]> = [];
  for (const item of value) {
    if (!item || typeof item !== "object") continue;
    const row = item as Record<string, unknown>;
    if (row.type === "ref") {
      const nodeId = String(row.nodeId || "").trim();
      if (nodeId) out.push({ type: "ref", nodeId });
      continue;
    }
    if (row.type === "text") {
      out.push({ type: "text", text: String(row.text || "") });
    }
  }
  return out.length ? out : undefined;
}

function normalizeAttachments(value: unknown): ComposerDraftAttachment[] {
  if (!Array.isArray(value)) return [];
  const out: ComposerDraftAttachment[] = [];
  for (const item of value) {
    if (!item || typeof item !== "object") continue;
    const row = item as Record<string, unknown>;
    const id = String(row.id || "").trim();
    const name = String(row.name || "").trim();
    if (!id || !name) continue;
    const sha256 = String(row.sha256 || "").trim();
    const sizeRaw = row.size;
    const size = typeof sizeRaw === "number" && Number.isFinite(sizeRaw) ? sizeRaw : undefined;
    const mimeType = String(row.mimeType || "").trim() || undefined;
    const needsReselect = Boolean(row.needsReselect) && !sha256;
    out.push({
      id,
      name,
      ...(size !== undefined ? { size } : {}),
      ...(mimeType ? { mimeType } : {}),
      ...(sha256 ? { sha256 } : {}),
      ...(needsReselect ? { needsReselect: true } : {}),
    });
  }
  return out;
}

export function normalizeComposerDraft(value: unknown): ComposerDraft | null {
  if (!value || typeof value !== "object") return null;
  const row = value as Record<string, unknown>;
  const capabilityRefs = normalizeCapabilityRefs(row.capabilityRefs);
  const promptSegments = normalizePromptSegments(row.promptSegments);
  const prompt = typeof row.prompt === "string" ? row.prompt : "";
  const draft: ComposerDraft = {
    prompt,
    capabilityRefs,
    attachments: normalizeAttachments(row.attachments),
    updatedAt: Number(row.updatedAt) || 0,
  };
  if (promptSegments) draft.promptSegments = promptSegments;
  const credentialId = String(row.credentialId || "").trim();
  const runtimeKey = String(row.runtimeKey || "").trim();
  const model = String(row.model || "").trim();
  const effort = String(row.effort || "").trim();
  const accessMode = String(row.accessMode || "").trim();
  const projectId = String(row.projectId || "").trim();
  if (credentialId) draft.credentialId = credentialId;
  if (runtimeKey) draft.runtimeKey = runtimeKey;
  if (model) draft.model = model;
  if (row.effort !== undefined) draft.effort = effort;
  if (accessMode) draft.accessMode = accessMode;
  if (projectId) draft.projectId = projectId;
  return draft;
}

export function readComposerDraft(draftKey: string): ComposerDraft | null {
  const key = String(draftKey || "").trim();
  if (!key) return null;
  const bucket = readBucketFromStorage();
  return normalizeComposerDraft(bucket[key]);
}

export type ComposerDraftWriteInput = {
  prompt: string;
  promptSegments?: NonNullable<ComposerDraft["promptSegments"]>;
  capabilityRefs: ComposerCapabilityRef[];
  attachments: Array<{
    id?: string;
    name: string;
    size?: number;
    mimeType?: string;
    type?: string;
    sha256?: string;
    needsReselect?: boolean;
    file?: File;
  }>;
  credentialId?: string;
  /** Exact adapter:instance identity; optional for legacy saved entries. */
  runtimeKey?: string;
  model?: string;
  effort?: string;
  accessMode?: string;
  projectId?: string;
};

function newAttachmentId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `att_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

/**
 * Persist a draft snapshot. Empty drafts are removed. File handles are kept in
 * the in-memory bag for same-tab thread switches.
 */
export function writeComposerDraft(draftKey: string, input: ComposerDraftWriteInput): ComposerDraft | null {
  const key = String(draftKey || "").trim();
  if (!key) return null;

  const attachments: ComposerDraftAttachment[] = input.attachments.map((item) => {
    const id = String(item.id || "").trim() || newAttachmentId();
    const sha256 = String(item.sha256 || "").trim();
    const mimeType = String(item.mimeType || item.type || item.file?.type || "").trim();
    if (item.file) {
      fileBag.set(fileBagKey(key, id), item.file);
    }
    const hasFile = fileBag.has(fileBagKey(key, id)) || Boolean(item.file);
    const needsReselect = !sha256 && !hasFile;
    return {
      id,
      name: item.name,
      ...(typeof item.size === "number" ? { size: item.size } : item.file ? { size: item.file.size } : {}),
      ...(mimeType ? { mimeType } : {}),
      ...(sha256 ? { sha256 } : {}),
      ...(needsReselect ? { needsReselect: true } : {}),
    };
  });

  // Drop file-bag entries removed from this draft.
  const keep = new Set(attachments.map((item) => fileBagKey(key, item.id)));
  for (const bagKey of Array.from(fileBag.keys())) {
    if (bagKey.startsWith(`${key}::`) && !keep.has(bagKey)) {
      fileBag.delete(bagKey);
    }
  }

  const draft: ComposerDraft = {
    prompt: input.prompt,
    capabilityRefs: input.capabilityRefs.map((ref) => ({ ...ref })),
    attachments,
    updatedAt: Date.now(),
  };
  if (input.promptSegments?.length) {
    draft.promptSegments = input.promptSegments.map((segment) => (
      segment.type === "text"
        ? { type: "text", text: segment.text }
        : { type: "ref", nodeId: segment.nodeId }
    ));
  }
  const credentialId = String(input.credentialId || "").trim();
  const runtimeKey = String(input.runtimeKey || "").trim();
  const model = String(input.model || "").trim();
  const effort = String(input.effort || "").trim();
  const accessMode = String(input.accessMode || "").trim();
  const projectId = String(input.projectId || "").trim();
  if (credentialId) draft.credentialId = credentialId;
  if (runtimeKey) draft.runtimeKey = runtimeKey;
  if (model) draft.model = model;
  if (input.effort !== undefined) draft.effort = effort;
  if (accessMode) draft.accessMode = accessMode;
  if (projectId) draft.projectId = projectId;

  const bucket = readBucketFromStorage();
  if (isComposerDraftEmpty(draft)) {
    delete bucket[key];
    for (const bagKey of Array.from(fileBag.keys())) {
      if (bagKey.startsWith(`${key}::`)) fileBag.delete(bagKey);
    }
    schedulePersist(bucket);
    return null;
  }

  bucket[key] = draft;
  schedulePersist(bucket);
  return draft;
}

export function clearComposerDraft(draftKey: string): void {
  const key = String(draftKey || "").trim();
  if (!key) return;
  const bucket = readBucketFromStorage();
  delete bucket[key];
  for (const bagKey of Array.from(fileBag.keys())) {
    if (bagKey.startsWith(`${key}::`)) fileBag.delete(bagKey);
  }
  schedulePersist(bucket);
}

/** Clear every composer draft (logout / auth lock). */
export function clearAllComposerDrafts(): void {
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  memoryBucket = {};
  memoryDirty = false;
  fileBag.clear();
  const storage = getStorage();
  if (!storage) return;
  try {
    storage.removeItem(COMPOSER_DRAFT_STORAGE_KEY);
  } catch {
    // ignore
  }
}

export type HydratedComposerAttachment = ComposerDraftAttachment & {
  file?: File;
  type?: string;
};

/** Load a draft and reattach in-memory File handles when available. */
export function hydrateComposerDraft(draftKey: string): {
  draft: ComposerDraft | null;
  attachments: HydratedComposerAttachment[];
  restoredNeedsReselect: boolean;
} {
  const draft = readComposerDraft(draftKey);
  if (!draft) {
    return { draft: null, attachments: [], restoredNeedsReselect: false };
  }
  let restoredNeedsReselect = false;
  const attachments: HydratedComposerAttachment[] = draft.attachments.map((item) => {
    const file = fileBag.get(fileBagKey(draftKey, item.id));
    if (file) {
      return {
        ...item,
        needsReselect: false,
        file,
        type: item.mimeType || file.type,
      };
    }
    if (item.sha256) {
      return {
        ...item,
        needsReselect: false,
        type: item.mimeType,
      };
    }
    restoredNeedsReselect = true;
    return {
      ...item,
      needsReselect: true,
      type: item.mimeType,
    };
  });
  return {
    draft: {
      ...draft,
      attachments: attachments.map(({ file: _file, type: _type, ...rest }) => rest),
    },
    attachments,
    restoredNeedsReselect,
  };
}

/** Test helper: inject storage and reset memory. */
export function __resetComposerDraftStoreForTests(storage?: StorageLike | null): void {
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  memoryBucket = {};
  memoryDirty = false;
  fileBag.clear();
  storageOverride = storage === undefined ? null : storage;
}
