import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope, subscribeBeforeConversationStorageScope, reportConversationPersistence } from "./conversationStorageScope";
/**
 * Per-composer-session draft persistence for Conversation.
 *
 * Keys drafts by thread id or ephemeral new-chat draft id so switching threads
 * does not leak or lose unsent prompt text, capability refs, model selection,
 * or attachment metadata. File blobs stay in an in-memory bag for same-tab
 * switches; after refresh, local-only files are marked needsReselect while
 * server-confirmed sha256 refs remain sendable.
 */

import { desktopChatBridge } from "./desktopChatBridge";
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
  /** Opaque native cache reference; never a server upload hash or filesystem path. */
  cacheId?: string;
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
  /** Per-thread interaction-mode preference: "default" | "plan". */
  interactionMode?: string;
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

type AttachmentCacheJob = {
  file: File; draftKey: string; attachmentId: string;
  promise: Promise<ComposerPersistenceResult>; status: "pending" | "failed";
};

interface DraftMemoryState {
  bucket: ComposerDraftBucket; dirty: boolean; timer: ReturnType<typeof setTimeout> | null;
  pendingKeys: Set<string>; baseVersions: Map<string, string | undefined>;
  files: Map<string, File>; jobs: Map<string, AttachmentCacheJob>;
  cleanup: Map<string, { id: string; draftId: string }>;
  cleaned: Set<string>; cleanupPromise: Promise<ComposerPersistenceResult> | null;
}
const emptyMemory = (): DraftMemoryState => ({ bucket: {}, dirty: false, timer: null, pendingKeys: new Set(), baseVersions: new Map(), files: new Map(), jobs: new Map(), cleanup: new Map(), cleaned: new Set(), cleanupPromise: null });
let draftMemory = emptyMemory();
const scopeMemories = new Map<string, DraftMemoryState>();
let storageOverride: StorageLike | null = null;

function notifyCache(draftKey: string, attachmentId: string, error?: string) {
  if (typeof window !== "undefined") window.dispatchEvent(new CustomEvent("muteki:attachment-cache", { detail: { draftKey, attachmentId, error } }));
}

function cacheDraftFile(draftKey: string, attachmentId: string, file: File): void {
  const state = draftMemory;
  const bridge = desktopChatBridge();
  if (!bridge) return;
  const key = fileBagKey(draftKey, attachmentId);
  const ownerScope = conversationStorageScope();
  const previous = state.jobs.get(key);
  if (previous?.file === file && previous.status !== "failed") return;
  const job: AttachmentCacheJob = {
    file, draftKey, attachmentId, promise: Promise.resolve({ persisted: true }), status: "pending",
  };
  state.jobs.set(key, job);
  job.promise = (async (): Promise<ComposerPersistenceResult> => {
    try {
      if (!bridge.cacheAttachment) throw new Error("desktop.attachment.cache_unavailable: 本地附件缓存未配置");
      if (file.size > 256 * 1024 * 1024) throw new Error("desktop.attachment.quota: 单个附件超过 256 MiB，请保留原文件并重新选择");
      const cached = await bridge.cacheAttachment({ data: await file.arrayBuffer(), name: file.name, type: file.type, lastModified: file.lastModified, draftId: draftKey });
      if (!cached || typeof cached.id !== "string" || !cached.id) throw new Error("desktop.attachment.invalid_reply: 附件缓存缺少身份");
      // Late cache replies cannot restore a removed/reselected file or mutate a new draft.
      const bucket = ownerScope === conversationStorageScope() ? readBucketFromStorage() : { ...state.bucket };
      const draft = bucket[draftKey];
      const attachment = draft?.attachments.find((item) => item.id === attachmentId);
      if (attachment && state.files.get(key) === file && state.jobs.get(key) === job) {
        bucket[draftKey] = { ...draft, attachments: draft.attachments.map((item) => item.id === attachmentId ? { ...item, cacheId: cached.id } : item) };
        schedulePersist(bucket, state, ownerScope);
        notifyCache(draftKey, attachmentId);
      } else {
        // This cache was never attached to persisted metadata. Keep cleanup durable.
        state.cleanup.set(cached.id, { id: cached.id, draftId: draftKey });
        schedulePersist(bucket, state, ownerScope);
      }
      if (state.jobs.get(key) === job) state.jobs.delete(key);
      return { persisted: true };
    } catch (error) {
      const message = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
      if (ownerScope === conversationStorageScope() && state.files.get(key) === file && state.jobs.get(key) === job) notifyCache(draftKey, attachmentId, message);
      if (state.jobs.get(key) === job) job.status = "failed";
      return { persisted: false, error: message };
    }
  })();
}

export async function flushComposerAttachmentCache(draftKey?: string): Promise<ComposerPersistenceResult> {
  // A quota/permission failure is retryable without making the user reselect identical bytes.
  for (const [key, job] of draftMemory.jobs) {
    if (job.status !== "failed" || draftMemory.files.get(key) !== job.file) continue;
    const ownerDraft = job.draftKey, attachmentId = job.attachmentId;
    if ((!draftKey || ownerDraft === draftKey) && readComposerDraft(ownerDraft)?.attachments.some((item) => item.id === attachmentId && !item.cacheId)) cacheDraftFile(ownerDraft, attachmentId, job.file);
  }
  const states = new Set([...scopeMemories.values(), draftMemory]);
  const promises: Promise<ComposerPersistenceResult>[] = [];
  for (const state of states) for (const [key, job] of state.jobs) {
    if ((!draftKey || job.draftKey === draftKey) && state.files.get(key) === job.file) promises.push(job.promise);
  }
  const outcomes = await Promise.all(promises);
  outcomes.push(flushComposerDraftStore());
  const cleanups = new Map(scopeMemories); cleanups.set(conversationStorageScope(), draftMemory);
  for (const [scope, state] of cleanups) if (scope === conversationStorageScope()) outcomes.push(await drainAttachmentCleanup(state, scope));
  const errors = outcomes.filter((outcome) => !outcome.persisted).map((outcome) => outcome.error || "desktop.attachment.cache_failed");
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}

export async function restoreComposerDraftAttachments(draftKey: string): Promise<ReturnType<typeof hydrateComposerDraft>> {
  const bridge = desktopChatBridge();
  const draft = readComposerDraft(draftKey);
  const ownerScope = conversationStorageScope();
  if (bridge?.restoreAttachment && draft) {
    await Promise.all(draft.attachments.map(async (item) => {
      const key = fileBagKey(draftKey, item.id);
      if (!item.cacheId || draftMemory.files.has(key) || item.sha256) return;
      try {
        const cached = await bridge.restoreAttachment!({ id: item.cacheId, draftId: draftKey });
        if (ownerScope !== conversationStorageScope()) return;
        const current = readComposerDraft(draftKey)?.attachments.find((entry) => entry.id === item.id);
        if (current?.cacheId !== item.cacheId || draftMemory.files.has(key)) return;
        draftMemory.files.set(key, new File([cached.data], cached.name, { type: cached.type, lastModified: cached.lastModified }));
        notifyCache(draftKey, item.id);
      } catch (error) { notifyCache(draftKey, item.id, error instanceof Error ? error.message : String(error)); }
    }));
  }
  return hydrateComposerDraft(draftKey);
}

function fileBagKey(draftKey: string, attachmentId: string): string {
  return `${draftKey}::${attachmentId}`;
}

function getStorage(ownerScope = conversationStorageScope()): StorageLike | null {
  if (storageOverride) return storageOverride;
  if (typeof window === "undefined" || !ownerScope) return null;
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

function parseCacheCleanup(raw: string | null): Array<{ id: string; draftId: string }> {
  if (!raw) return [];
  const row: unknown = JSON.parse(raw);
  const entries = row && typeof row === "object" ? (row as Record<string, unknown>).cacheCleanup : null;
  return Array.isArray(entries) ? entries.filter((entry): entry is { id: string; draftId: string } => Boolean(entry && typeof entry.id === "string" && entry.id && typeof entry.draftId === "string" && entry.draftId)) : [];
}

async function drainAttachmentCleanup(state: DraftMemoryState, ownerScope: string): Promise<ComposerPersistenceResult> {
  if (state.cleanupPromise) return state.cleanupPromise;
  if (state.dirty || !state.cleanup.size || ownerScope !== conversationStorageScope()) return { persisted: true };
  const bridge = desktopChatBridge();
  if (!bridge?.removeAttachment) return { persisted: false, error: "desktop.attachment.cleanup_unavailable: 本地附件缓存清理未配置" };
  state.cleanupPromise = (async () => {
    const errors: string[] = [];
    for (const [id, entry] of Array.from(state.cleanup)) {
      if (ownerScope !== conversationStorageScope() || state.dirty) break;
      // Metadata owns liveness. Re-added references cancel deletion, including other windows.
      try {
        const storage = getStorage(ownerScope);
        if (!storage) throw new Error("draft.storage.unavailable: 无法核对缓存元数据");
        const persisted = parseDraftBucket(storage.getItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY, ownerScope)));
        if (persisted[entry.draftId]?.attachments.some((item) => item.cacheId === id)) { state.cleanup.delete(id); state.cleaned.add(id); continue; }
        await bridge.removeAttachment!({ id, draftId: entry.draftId });
        state.cleanup.delete(id); state.cleaned.add(id);
      } catch (error) { errors.push(error instanceof Error ? `${error.name}: ${error.message}` : String(error)); }
    }
    if (state.cleaned.size && !state.dirty) {
      const result = writeBucketToStorage(state.bucket, state, ownerScope);
      if (!result.persisted) errors.push(result.error || "附件清理记录保存失败");
    }
    if (errors.length) reportConversationPersistence("attachment-cache", errors.join("\n"));
    return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
  })();
  try { return await state.cleanupPromise; } finally { state.cleanupPromise = null; }
}

function migrateV1Drafts(storage: StorageLike): ComposerDraftBucket {
  try {
    const v1 = parseDraftBucket(storage.getItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY_V1)));
    if (!Object.keys(v1).length) return {};
    // Promote v1 → v2 once, then leave v1 in place for older tabs.
    storage.setItem(
      conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY),
      JSON.stringify({ drafts: v1 }),
    );
    return v1;
  } catch {
    return {};
  }
}

function readBucketFromStorage(): ComposerDraftBucket {
  // In-memory bucket may be ahead of the debounced disk write.
  if (draftMemory.dirty) return { ...draftMemory.bucket };
  const storage = getStorage();
  if (!storage) return { ...draftMemory.bucket };
  try {
    const raw = storage.getItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY));
    const next = raw === null ? migrateV1Drafts(storage) : parseDraftBucket(raw);
    draftMemory.bucket = next;
    for (const entry of parseCacheCleanup(raw)) if (!draftMemory.cleaned.has(entry.id)) draftMemory.cleanup.set(entry.id, entry);
    void drainAttachmentCleanup(draftMemory, conversationStorageScope());
    return { ...draftMemory.bucket };
  } catch {
    return { ...draftMemory.bucket };
  }
}

export type ComposerPersistenceResult = { persisted: boolean; error?: string };

function writeBucketToStorage(bucket: ComposerDraftBucket, state = draftMemory, ownerScope = conversationStorageScope()): ComposerPersistenceResult {
  state.bucket = { ...bucket };
  state.dirty = true;
  const storage = getStorage(ownerScope);
  if (!storage) return { persisted: false, error: "draft.storage.unavailable: 本地草稿存储不可用" };
  try {
    // Merge only keys edited by this renderer into the latest persisted bucket.
    // A delayed flush must not overwrite drafts saved by another window.
    const raw = storage.getItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY, ownerScope));
    const merged = parseDraftBucket(raw);
    for (const entry of parseCacheCleanup(raw)) if (!state.cleaned.has(entry.id)) state.cleanup.set(entry.id, entry);
    for (const key of state.pendingKeys) {
      if (JSON.stringify(merged[key]) !== state.baseVersions.get(key)) throw new Error(`draft.storage.conflict: 草稿 ${key} 已在另一窗口更新；当前编辑保留在内存，请先暂存或复制后另存`);
      if (state.bucket[key]) merged[key] = state.bucket[key];
      else delete merged[key];
    }
    storage.setItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY, ownerScope), JSON.stringify({ drafts: merged, cacheCleanup: Array.from(state.cleanup.values()) }));
    state.bucket = merged;
    state.dirty = false;
    state.pendingKeys.clear();
    state.baseVersions.clear();
    state.cleaned.clear();
    if (!state.cleanupPromise) void drainAttachmentCleanup(state, ownerScope);
    return { persisted: true };
  } catch (error) {
    return { persisted: false, error: error instanceof Error ? `${error.name}: ${error.message}` : String(error) };
  }
}

function schedulePersist(bucket: ComposerDraftBucket, state = draftMemory, ownerScope = conversationStorageScope()): void {
  for (const key of new Set([...Object.keys(state.bucket), ...Object.keys(bucket)])) {
    if (JSON.stringify(state.bucket[key]) !== JSON.stringify(bucket[key])) {
      for (const item of state.bucket[key]?.attachments || []) {
        if (item.cacheId && !bucket[key]?.attachments.some((next) => next.cacheId === item.cacheId)) state.cleanup.set(item.cacheId, { id: item.cacheId, draftId: key });
      }
      if (!state.pendingKeys.has(key)) state.baseVersions.set(key, JSON.stringify(state.bucket[key]));
      state.pendingKeys.add(key);
    }
  }
  state.bucket = { ...bucket };
  state.dirty = true;
  if (typeof window === "undefined" && !storageOverride) return;
  if (state.timer) clearTimeout(state.timer);
  state.timer = setTimeout(() => {
    state.timer = null;
    const result = writeBucketToStorage(state.bucket, state, ownerScope);
    if (!result.persisted) reportConversationPersistence("composer", result.error || "草稿保存失败");
  }, PERSIST_DEBOUNCE_MS);
}

/** Flush pending debounced writes (call on beforeunload / tests). */
export function flushComposerDraftStore(): ComposerPersistenceResult {
  const states = new Map(scopeMemories);
  states.set(conversationStorageScope(), draftMemory);
  const errors: string[] = [];
  for (const [scope, state] of states) {
    if (state.timer) { clearTimeout(state.timer); state.timer = null; }
    if (!state.dirty) continue;
    const result = writeBucketToStorage(state.bucket, state, scope);
    if (!result.persisted) errors.push(result.error || "草稿保存失败");
  }
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
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
  if (draft.interactionMode && draft.interactionMode !== "default") return false;
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
    const cacheId = typeof row.cacheId === "string" ? row.cacheId : "";
    const needsReselect = Boolean(row.needsReselect) && !sha256;
    out.push({
      id,
      name,
      ...(size !== undefined ? { size } : {}),
      ...(mimeType ? { mimeType } : {}),
      ...(sha256 ? { sha256 } : {}),
      ...(cacheId ? { cacheId } : {}),
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
  const interactionMode = String(row.interactionMode || "").trim();
  if (interactionMode) draft.interactionMode = interactionMode;
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
    cacheId?: string;
    needsReselect?: boolean;
    file?: File;
  }>;
  credentialId?: string;
  /** Exact adapter:instance identity; optional for legacy saved entries. */
  runtimeKey?: string;
  model?: string;
  effort?: string;
  accessMode?: string;
  /** Per-thread interaction-mode preference: "default" | "plan". */
  interactionMode?: string;
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
  if (!storageOverride && !conversationStorageScope()) return null;
  const key = String(draftKey || "").trim();
  if (!key) return null;

  const oldDraft = readComposerDraft(key);
  const attachments: ComposerDraftAttachment[] = input.attachments.map((item) => {
    const id = String(item.id || "").trim() || newAttachmentId();
    const sha256 = String(item.sha256 || "").trim();
    const mimeType = String(item.mimeType || item.type || item.file?.type || "").trim();
    const oldFile = draftMemory.files.get(fileBagKey(key, id));
    const cacheId = item.cacheId || ((!item.file || oldFile === item.file) ? oldDraft?.attachments.find((row) => row.id === id)?.cacheId : undefined);
    if (item.file) {
      draftMemory.files.set(fileBagKey(key, id), item.file);
    }
    const hasFile = draftMemory.files.has(fileBagKey(key, id)) || Boolean(item.file);
    const needsReselect = !sha256 && !hasFile;
    return {
      id,
      name: item.name,
      ...(typeof item.size === "number" ? { size: item.size } : item.file ? { size: item.file.size } : {}),
      ...(mimeType ? { mimeType } : {}),
      ...(sha256 ? { sha256 } : {}),
      ...(cacheId ? { cacheId } : {}),
      ...(needsReselect ? { needsReselect: true } : {}),
    };
  });

  // Drop file-bag entries removed from this draft.
  const keep = new Set(attachments.map((item) => fileBagKey(key, item.id)));
  for (const bagKey of Array.from(draftMemory.files.keys())) {
    if (bagKey.startsWith(`${key}::`) && !keep.has(bagKey)) {
      draftMemory.files.delete(bagKey);
      draftMemory.jobs.delete(bagKey);
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
  const interactionMode = String(input.interactionMode || "").trim();
  if (interactionMode) draft.interactionMode = interactionMode;
  if (projectId) draft.projectId = projectId;

  const bucket = readBucketFromStorage();
  if (isComposerDraftEmpty(draft)) {
    delete bucket[key];
    for (const bagKey of Array.from(draftMemory.files.keys())) {
      if (bagKey.startsWith(`${key}::`)) { draftMemory.files.delete(bagKey); draftMemory.jobs.delete(bagKey); }
    }
    schedulePersist(bucket);
    return null;
  }

  bucket[key] = draft;
  schedulePersist(bucket);
  for (const [index, item] of input.attachments.entries()) {
    if (item.file && !attachments[index].cacheId) cacheDraftFile(key, attachments[index].id, item.file);
  }
  return draft;
}

export function clearComposerDraft(draftKey: string): void {
  const key = String(draftKey || "").trim();
  if (!key) return;
  const bucket = readBucketFromStorage();
  delete bucket[key];
  for (const bagKey of Array.from(draftMemory.files.keys())) {
    if (bagKey.startsWith(`${key}::`)) { draftMemory.files.delete(bagKey); draftMemory.jobs.delete(bagKey); }
  }
  schedulePersist(bucket);
}

/** Clear every composer draft (logout / auth lock). */
export function clearAllComposerDrafts(): void {
  if (draftMemory.timer) {
    clearTimeout(draftMemory.timer);
    draftMemory.timer = null;
  }
  draftMemory.files.clear();
  draftMemory.jobs.clear();
  schedulePersist({});
  const result = flushComposerDraftStore();
  if (!result.persisted) { reportConversationPersistence("composer", result.error || "草稿清空保存失败"); return; }
  const storage = getStorage();
  if (!storage) return;
  try {
    storage.removeItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY_V1));
  } catch (error) { reportConversationPersistence("composer", error instanceof Error ? error.message : String(error)); }
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
    const file = draftMemory.files.get(fileBagKey(draftKey, item.id));
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
  if (draftMemory.timer) {
    clearTimeout(draftMemory.timer);
    draftMemory.timer = null;
  }
  draftMemory.bucket = {};
  draftMemory.dirty = false;
  draftMemory.pendingKeys.clear();
  draftMemory.baseVersions.clear();
  draftMemory.files.clear();
  draftMemory.jobs.clear();
  draftMemory.cleanup.clear(); draftMemory.cleaned.clear(); draftMemory.cleanupPromise = null;
  storageOverride = storage === undefined ? null : storage;
}

subscribeBeforeConversationStorageScope(() => {
  flushComposerDraftStore();
  scopeMemories.set(conversationStorageScope(), draftMemory);
});
subscribeConversationStorageScope(() => {
  draftMemory = scopeMemories.get(conversationStorageScope()) || emptyMemory();
});
