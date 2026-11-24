import { conversationStorageTransaction, conversationStorageVersion } from "./conversationStorageTransaction";
import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope, subscribeBeforeConversationStorageScope, reportConversationPersistence } from "./conversationStorageScope";

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
  artifactId?: string;
  cacheId?: string;
  needsReselect?: boolean;
};

export type ComposerDraft = {
  prompt: string;
  promptSegments?: Array<
    | { type: "text"; text: string }
    | { type: "ref"; nodeId: string }
  >;
  capabilityRefs: ComposerCapabilityRef[];
  attachments: ComposerDraftAttachment[];
  credentialId?: string;
  runtimeKey?: string;
  model?: string;
  effort?: string;
  accessMode?: string;
  interactionMode?: string;
  projectId?: string;
  updatedAt: number;
  runtimeUpdatedAt?: number;
};

export type ComposerDraftBucket = Record<string, ComposerDraft>;

type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

type AttachmentCacheJob = {
  file: File; draftKey: string; attachmentId: string;
  promise: Promise<ComposerPersistenceResult>; status: "pending" | "failed";
};

interface DraftMemoryState {
  bucket: ComposerDraftBucket; dirty: boolean; timer: ReturnType<typeof setTimeout> | null;
  pendingKeys: Set<string>; baseVersions: Map<string, string | undefined>; observedVersions: Map<string, string | undefined>; journalSequence: number;
  files: Map<string, File>; jobs: Map<string, AttachmentCacheJob>;
  cleanup: Map<string, { id: string; draftId: string }>;
  cleaned: Set<string>; cleanupPromise: Promise<ComposerPersistenceResult> | null;
}
const emptyMemory = (): DraftMemoryState => ({ bucket: {}, dirty: false, timer: null, pendingKeys: new Set(), baseVersions: new Map(), observedVersions: new Map(), journalSequence: 0, files: new Map(), jobs: new Map(), cleanup: new Map(), cleaned: new Set(), cleanupPromise: null });
let draftMemory = emptyMemory();
const scopeMemories = new Map<string, DraftMemoryState>();
const journalOwner = newAttachmentId();
let journalSequence = 0;
const journalPrefix = (scope: string) => `${conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY, scope)}:pending:`;
const retiredKey = (scope: string, id: string) => `${conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY, scope)}:retired:${encodeURIComponent(id)}`;
function persistDraftJournal(state: DraftMemoryState, scope: string): void {
  const storage = getStorage(scope);
  if (!storage) return;
  const sequence = ++journalSequence;
  state.journalSequence = sequence;
  const key = `${journalPrefix(scope)}${journalOwner}:${sequence}`;
  const drafts = Object.fromEntries([...state.pendingKeys].flatMap(id => state.bucket[id] ? [[id, state.bucket[id]]] : []));
  storage.setItem(key, JSON.stringify({ owner: journalOwner, sequence, drafts, keys: [...state.pendingKeys], bases: Object.fromEntries(state.baseVersions), cacheCleanup: [...state.cleanup.values()] }));
  if (typeof window !== "undefined") {
    const old = Object.keys(window.localStorage).filter(candidate => candidate.startsWith(`${journalPrefix(scope)}${journalOwner}`) && candidate !== key);
    for (const candidate of old) storage.removeItem(candidate);
  }
}
type DraftJournal = { key: string; owner: string; sequence: number; sourceKeys: string[]; drafts: ComposerDraftBucket; keys: string[]; bases: Record<string, string | undefined>; cacheCleanup: Array<{ id: string; draftId: string }> };
function draftJournals(scope: string): DraftJournal[] {
  if (typeof window === "undefined") return [];
  const storage = window.localStorage;
  const owners = new Map<string, DraftJournal>();
  for (const key of Object.keys(storage)) {
    if (!key.startsWith(journalPrefix(scope))) continue;
    const raw = storage.getItem(key);
    if (raw === null) continue;
    const row = JSON.parse(raw);
    if (!row || !Array.isArray(row.keys) || !row.drafts || !row.bases) throw new Error("draft.storage.journal_invalid: 草稿恢复日志格式无效");
    const owner = typeof row.owner === "string" ? row.owner : key.slice(journalPrefix(scope).length);
    const sequence = typeof row.sequence === "number" ? row.sequence : 0;
    const previous = owners.get(owner);
    const sourceKeys = [...(previous?.sourceKeys || []), key];
    owners.set(owner, previous && previous.sequence > sequence ? { ...previous, sourceKeys } : { ...row, key, owner, sequence, sourceKeys });
  }
  return [...owners.values()];
}

type DraftWriter = { owner: string; sequence: number };
type DraftWriters = Record<string, DraftWriter>;
function parseDraftWriters(raw: string | null): DraftWriters {
  return raw ? (JSON.parse(raw)?.draftWriters || {}) as DraftWriters : {};
}
function sameDraftWriter(writers: DraftWriters, key: string, owner: string): DraftWriter | undefined {
  const writer = writers[key];
  return writer?.owner === owner ? writer : undefined;
}
function applyDraftRevision(drafts: ComposerDraftBucket, writers: DraftWriters, key: string, draft: ComposerDraft | undefined, owner: string, sequence: number): void {
  const previousWriter = sameDraftWriter(writers, key, owner);
  if (sameDraftContent(drafts[key], draft) && !(previousWriter && previousWriter.sequence < sequence)) return;
  if (draft) drafts[key] = draft; else delete drafts[key];
  writers[key] = { owner, sequence };
}

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
      const bucket = ownerScope === conversationStorageScope() ? readBucketFromStorage() : { ...state.bucket };
      const draft = bucket[draftKey];
      const attachment = draft?.attachments.find((item) => item.id === attachmentId);
      if (attachment && state.files.get(key) === file && state.jobs.get(key) === job) {
        bucket[draftKey] = { ...draft, attachments: draft.attachments.map((item) => item.id === attachmentId ? { ...item, cacheId: cached.id } : item) };
        schedulePersist(bucket, state, ownerScope);
        notifyCache(draftKey, attachmentId);
      } else {
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
  outcomes.push(await flushComposerDraftStore());
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
      if (!item.cacheId || draftMemory.files.has(key) || item.artifactId) return;
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
  state.cleanupPromise = conversationStorageTransaction(ownerScope, async (assertActive) => {
    const errors: string[] = [];
    for (const [id, entry] of Array.from(state.cleanup)) {
      assertActive();
      if (ownerScope !== conversationStorageScope() || state.dirty) break;
      try {
        const storage = getStorage(ownerScope);
        if (!storage) throw new Error("draft.storage.unavailable: 无法核对缓存元数据");
        const persisted = parseDraftBucket(storage.getItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY, ownerScope)));
        const journalReferences = draftJournals(ownerScope).some((journal) => journal.keys.some((key) => journal.drafts[key]?.attachments.some((item) => item.cacheId === id)));
        if (journalReferences || Object.values(persisted).some((draft) => draft.attachments.some((item) => item.cacheId === id))) { state.cleanup.delete(id); state.cleaned.add(id); continue; }
        storage.setItem(retiredKey(ownerScope, id), "1");
        await bridge.removeAttachment!({ id, draftId: entry.draftId });
        assertActive();
        state.cleanup.delete(id); state.cleaned.add(id);
      } catch (error) { errors.push(error instanceof Error ? `${error.name}: ${error.message}` : String(error)); }
    }
    assertActive();
    if (state.cleaned.size && !state.dirty) {
      const result = writeBucketUnlocked(state.bucket, state, ownerScope);
      if (!result.persisted) errors.push(result.error || "附件清理记录保存失败");
    }
    if (errors.length) reportConversationPersistence("attachment-cache", errors.join("\n"));
    return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
  }).catch((error) => ({ persisted: false, error: error instanceof Error ? error.message : String(error) }));
  try { return await state.cleanupPromise; } finally { state.cleanupPromise = null; }
}

function migrateV1Drafts(storage: StorageLike): ComposerDraftBucket {
  try {
    const v1 = parseDraftBucket(storage.getItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY_V1)));
    if (!Object.keys(v1).length) return {};
    return v1;
  } catch {
    return {};
  }
}

function readBucketFromStorage(): ComposerDraftBucket {
  if (draftMemory.dirty) return { ...draftMemory.bucket };
  const storage = getStorage();
  if (!storage) return { ...draftMemory.bucket };
  try {
    const raw = storage.getItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY));
    const next = raw === null ? migrateV1Drafts(storage) : parseDraftBucket(raw);
    if (raw === null && Object.keys(next).length) {
      for (const key of Object.keys(next)) { draftMemory.pendingKeys.add(key); draftMemory.baseVersions.set(key, undefined); }
      draftMemory.dirty = true;
      draftMemory.bucket = next;
      persistDraftJournal(draftMemory, conversationStorageScope());
      void flushComposerDraftStore();
    }
    draftMemory.bucket = next;
    const writers = parseDraftWriters(raw);
    for (const journal of draftJournals(conversationStorageScope())) for (const key of journal.keys) {
      const previousWriter = sameDraftWriter(writers, key, journal.owner);
      if (previousWriter && previousWriter.sequence >= journal.sequence) continue;
      if (!previousWriter && draftMatchesBase(journal.drafts[key], journal.bases[key])) continue;
      if (draftMatchesBase(next[key], journal.bases[key]) || previousWriter) {
        applyDraftRevision(draftMemory.bucket, writers, key, journal.drafts[key], journal.owner, journal.sequence);
      }
    }
    for (const entry of parseCacheCleanup(raw)) if (!draftMemory.cleaned.has(entry.id)) draftMemory.cleanup.set(entry.id, entry);
    void drainAttachmentCleanup(draftMemory, conversationStorageScope());
    return { ...draftMemory.bucket };
  } catch {
    return { ...draftMemory.bucket };
  }
}

export type ComposerPersistenceResult = { persisted: boolean; error?: string };
function sameDraftContent(a: ComposerDraft | undefined, b: ComposerDraft | undefined): boolean {
  if (a === b) return true;
  if (a?.prompt !== b?.prompt) return false;
  const content = (draft: ComposerDraft | undefined) => {
    if (!draft) return undefined;
    const { updatedAt: _updated, runtimeUpdatedAt: _runtimeUpdated, ...value } = draft;
    return { ...value, attachments: value.attachments.map(({ needsReselect: _needsReselect, ...attachment }) => attachment) };
  };
  return conversationStorageVersion(content(a)) === conversationStorageVersion(content(b));
}

function draftMatchesBase(draft: ComposerDraft | undefined, base: string | undefined): boolean {
  if (conversationStorageVersion(draft) === base) return true;
  if (typeof base !== "string") return false;
  try {
    const previous = normalizeComposerDraft(JSON.parse(base));
    return previous !== null && sameDraftContent(draft, previous);
  } catch { return false; }
}

async function writeBucketToStorage(_bucket: ComposerDraftBucket, state = draftMemory, ownerScope = conversationStorageScope()): Promise<ComposerPersistenceResult> {
  try { return await conversationStorageTransaction(ownerScope, () => writeBucketUnlocked(state.bucket, state, ownerScope)); }
  catch (error) { return { persisted: false, error: error instanceof Error ? error.message : String(error) }; }
}
function writeBucketUnlocked(bucket: ComposerDraftBucket, state: DraftMemoryState, ownerScope: string): ComposerPersistenceResult {
  state.bucket = { ...bucket };
  state.dirty = true;
  const storage = getStorage(ownerScope);
  if (!storage) return { persisted: false, error: "draft.storage.unavailable: 本地草稿存储不可用" };
  try {
    const raw = storage.getItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY, ownerScope));
    const merged = parseDraftBucket(raw);
    const writers = parseDraftWriters(raw);
    for (const entry of parseCacheCleanup(raw)) if (!state.cleaned.has(entry.id)) state.cleanup.set(entry.id, entry);
    const journals = draftJournals(ownerScope);
    const recoveredConflicts: string[] = [];
    const recovered: string[] = [];
    for (const journal of journals) {
      if (journal.owner === journalOwner) continue;
      let conflict = false;
      for (const key of journal.keys) {
        const previousWriter = sameDraftWriter(writers, key, journal.owner);
        if (previousWriter && previousWriter.sequence >= journal.sequence) continue;
        if (!previousWriter && draftMatchesBase(journal.drafts[key], journal.bases[key])) continue;
        if (journal.drafts[key]?.attachments.some((item) => item.cacheId && storage.getItem(retiredKey(ownerScope, item.cacheId)))) { conflict = true; recoveredConflicts.push(key); continue; }
        if (!draftMatchesBase(merged[key], journal.bases[key]) && !previousWriter && !sameDraftContent(merged[key], journal.drafts[key])) { conflict = true; recoveredConflicts.push(key); continue; }
        applyDraftRevision(merged, writers, key, journal.drafts[key], journal.owner, journal.sequence);
      }
      for (const entry of journal.cacheCleanup || []) if (!state.cleaned.has(entry.id)) state.cleanup.set(entry.id, entry);
      if (!conflict) recovered.push(...journal.sourceKeys);
    }
    const conflicts: string[] = [];
    const retiredConflicts: string[] = [];
    const conflictDrafts = new Map<string, unknown>();
    for (const key of state.pendingKeys) {
      const previousWriter = sameDraftWriter(writers, key, journalOwner);
      if (!previousWriter && draftMatchesBase(state.bucket[key], state.baseVersions.get(key))) continue;
      const ancestor = previousWriter && previousWriter.sequence < state.journalSequence;
      const superseded = previousWriter && previousWriter.sequence >= state.journalSequence;
      if ((!ancestor && !draftMatchesBase(merged[key], state.baseVersions.get(key)) || superseded) && !sameDraftContent(merged[key], state.bucket[key])) {
        conflicts.push(key);
        conflictDrafts.set(key, state.bucket[key]);
        continue;
      }
      if (state.bucket[key]?.attachments.some((item) => item.cacheId && storage.getItem(retiredKey(ownerScope, item.cacheId)))) {
        conflicts.push(key); retiredConflicts.push(key); conflictDrafts.set(key, state.bucket[key]); continue;
      }
      applyDraftRevision(merged, writers, key, state.bucket[key], journalOwner, state.journalSequence);
    }
    storage.setItem(conversationStorageKey(COMPOSER_DRAFT_STORAGE_KEY, ownerScope), JSON.stringify({ drafts: merged, draftWriters: writers, cacheCleanup: Array.from(state.cleanup.values()) }));
    state.bucket = merged;
    for (const key of conflicts) {
      const draft = conflictDrafts.get(key);
      if (draft) state.bucket[key] = draft as typeof state.bucket[typeof key];
      else delete state.bucket[key];
    }
    state.dirty = conflicts.length > 0;
    for (const key of state.pendingKeys) if (!conflicts.includes(key)) state.observedVersions.set(key, conversationStorageVersion(merged[key]));
    state.pendingKeys = new Set(conflicts);
    for (const key of [...state.baseVersions.keys()]) if (!conflicts.includes(key)) state.baseVersions.delete(key);
    state.cleaned.clear();
    for (const key of recovered) storage.removeItem(key);
    if (conflicts.length) persistDraftJournal(state, ownerScope);
    else for (const journal of journals) if (journal.owner === journalOwner) for (const key of journal.sourceKeys) storage.removeItem(key);
    if (recoveredConflicts.length) throw new Error(`draft.storage.conflict: 待恢复草稿 ${recoveredConflicts.join(", ")} 与另一窗口冲突；恢复日志已保留`);
    if (retiredConflicts.length) throw new Error(`desktop.attachment.cache_retired: 草稿 ${retiredConflicts.join(", ")} 引用的本地附件正在清理；编辑已保留，请重新选择附件后保存`);
    if (conflicts.length) throw new Error(`draft.storage.conflict: 草稿 ${conflicts.join(", ")} 已在另一窗口更新；当前编辑保留在内存，请先暂存或复制后另存`);
    if (!state.cleanupPromise) void drainAttachmentCleanup(state, ownerScope);
    return { persisted: true };
  } catch (error) {
    return { persisted: false, error: error instanceof Error ? `${error.name}: ${error.message}` : String(error) };
  }
}

function schedulePersist(bucket: ComposerDraftBucket, state = draftMemory, ownerScope = conversationStorageScope()): void {
  for (const key of new Set([...Object.keys(state.bucket), ...Object.keys(bucket)])) {
    const previous = state.bucket[key];
    const next = bucket[key];
    if (previous === next) continue;
    const previousVersion = conversationStorageVersion(previous);
    if (previousVersion === conversationStorageVersion(next)) continue;
    for (const item of previous?.attachments || []) {
      if (item.cacheId && !next?.attachments.some((entry) => entry.cacheId === item.cacheId)) state.cleanup.set(item.cacheId, { id: item.cacheId, draftId: key });
    }
    if (!state.pendingKeys.has(key)) {
      state.baseVersions.set(key, state.observedVersions.has(key) ? state.observedVersions.get(key) : previousVersion);
    }
    state.pendingKeys.add(key);
  }
  state.bucket = { ...bucket };
  state.dirty = true;
  if (typeof window === "undefined") return;
  try { persistDraftJournal(state, ownerScope); }
  catch (error) { reportConversationPersistence("composer", error instanceof Error ? error.message : String(error)); }
  if (state.timer) clearTimeout(state.timer);
  state.timer = setTimeout(() => {
    state.timer = null;
    void writeBucketToStorage(state.bucket, state, ownerScope).then((result) => {
      if (!result.persisted) reportConversationPersistence("composer", result.error || "草稿保存失败");
    });
  }, PERSIST_DEBOUNCE_MS);
}

export async function flushComposerDraftStore(): Promise<ComposerPersistenceResult> {
  const states = new Map(scopeMemories);
  states.set(conversationStorageScope(), draftMemory);
  const errors: string[] = [];
  for (const [scope, state] of states) {
    if (state.timer) { clearTimeout(state.timer); state.timer = null; }
    if (!state.dirty && !draftJournals(scope).length) continue;
    const result = await writeBucketToStorage(state.bucket, state, scope);
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
    const comment = typeof row.comment === "string" ? row.comment.trim() : "";
    if (comment) ref.comment = comment;
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
    const artifactId = String(row.artifactId || "").trim();
    const sizeRaw = row.size;
    const size = typeof sizeRaw === "number" && Number.isFinite(sizeRaw) ? sizeRaw : undefined;
    const mimeType = String(row.mimeType || "").trim() || undefined;
    const cacheId = typeof row.cacheId === "string" ? row.cacheId : "";
    const needsReselect = !artifactId && (Boolean(row.needsReselect) || typeof row.sha256 === "string");
    out.push({
      id,
      name,
      ...(size !== undefined ? { size } : {}),
      ...(mimeType ? { mimeType } : {}),
      ...(artifactId ? { artifactId } : {}),
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
  const runtimeUpdatedAt = Number(row.runtimeUpdatedAt) || 0;
  if (runtimeUpdatedAt) draft.runtimeUpdatedAt = runtimeUpdatedAt;
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

function observeDraft(key: string, draft: ComposerDraft | undefined): ComposerDraft | null {
  if (!draftMemory.observedVersions.has(key)) draftMemory.observedVersions.set(key, conversationStorageVersion(draft));
  return normalizeComposerDraft(draft);
}

export function readComposerDraft(draftKey: string): ComposerDraft | null {
  const key = String(draftKey || "").trim();
  if (!key) return null;
  return observeDraft(key, readBucketFromStorage()[key]);
}

export function latestNewComposerDraftId(options: { scope?: string; projectId?: string } = {}): string | null {
  const prefix = options.scope ? `experience:${encodeURIComponent(options.scope)}:draft:` : "draft:";
  const candidates = Object.entries(readBucketFromStorage()).filter(([key, draft]) => {
    if (!key.startsWith(prefix)) return false;
    if (options.projectId !== undefined && (draft.projectId || "") !== options.projectId) return false;
    return Boolean(draft.prompt.trim() || draft.capabilityRefs.length || draft.attachments.length
      || draft.promptSegments?.some((segment) => segment.type === "ref" || segment.text.trim()));
  });
  candidates.sort((a, b) => b[1].updatedAt - a[1].updatedAt);
  const key = candidates[0]?.[0];
  if (!key) return null;
  const id = key.slice(prefix.length);
  return id === "home" ? "" : id;
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
    artifactId?: string;
    cacheId?: string;
    needsReselect?: boolean;
    file?: File;
  }>;
  credentialId?: string;
  runtimeKey?: string;
  model?: string;
  effort?: string;
  accessMode?: string;
  interactionMode?: string;
  projectId?: string;
};

function newAttachmentId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `att_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

export function writeComposerDraft(draftKey: string, input: ComposerDraftWriteInput): ComposerDraft | null {
  if (!conversationStorageScope()) return null;
  const key = String(draftKey || "").trim();
  if (!key) return null;

  const bucket = readBucketFromStorage();
  const oldDraft = observeDraft(key, bucket[key]);
  const attachments: ComposerDraftAttachment[] = input.attachments.map((item) => {
    const id = String(item.id || "").trim() || newAttachmentId();
    const artifactId = String(item.artifactId || "").trim();
    const mimeType = String(item.mimeType || item.type || item.file?.type || "").trim();
    const oldFile = draftMemory.files.get(fileBagKey(key, id));
    const cacheId = item.cacheId || ((!item.file || oldFile === item.file) ? oldDraft?.attachments.find((row) => row.id === id)?.cacheId : undefined);
    if (item.file) {
      draftMemory.files.set(fileBagKey(key, id), item.file);
    }
    const hasFile = draftMemory.files.has(fileBagKey(key, id)) || Boolean(item.file);
    const needsReselect = !artifactId && !hasFile;
    return {
      id,
      name: item.name,
      ...(typeof item.size === "number" ? { size: item.size } : item.file ? { size: item.file.size } : {}),
      ...(mimeType ? { mimeType } : {}),
      ...(artifactId ? { artifactId } : {}),
      ...(cacheId ? { cacheId } : {}),
      ...(needsReselect ? { needsReselect: true } : {}),
    };
  });

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
  if (credentialId || runtimeKey || model || effort || accessMode) {
    const runtimeUnchanged = Boolean(oldDraft)
      && (oldDraft?.credentialId || "") === credentialId
      && (oldDraft?.runtimeKey || "") === runtimeKey
      && (oldDraft?.model || "") === model
      && (oldDraft?.effort || "") === effort
      && (oldDraft?.accessMode || "") === accessMode;
    draft.runtimeUpdatedAt = runtimeUnchanged
      ? (oldDraft?.runtimeUpdatedAt || oldDraft?.updatedAt || draft.updatedAt)
      : draft.updatedAt;
  }

  if (oldDraft && sameDraftContent(oldDraft, draft)) {
    for (const [index, item] of input.attachments.entries()) {
      if (item.file && !attachments[index].cacheId) cacheDraftFile(key, attachments[index].id, item.file);
    }
    return oldDraft;
  }
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

export type HydratedComposerAttachment = ComposerDraftAttachment & {
  file?: File;
  type?: string;
};

export function hydrateComposerDraft(draftKey: string): {
  draft: ComposerDraft | null;
  attachments: HydratedComposerAttachment[];
  restoredNeedsReselect: boolean;
} {
  const draft = readComposerDraft(draftKey);
  if (!draftMemory.pendingKeys.has(draftKey)) draftMemory.observedVersions.set(draftKey, conversationStorageVersion(draft ?? undefined));
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
    if (item.artifactId) {
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

subscribeBeforeConversationStorageScope(() => {
  flushComposerDraftStore();
  scopeMemories.set(conversationStorageScope(), draftMemory);
});
subscribeConversationStorageScope(() => {
  draftMemory = scopeMemories.get(conversationStorageScope()) || emptyMemory();
});
