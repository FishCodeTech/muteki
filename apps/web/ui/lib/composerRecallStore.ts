import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope, subscribeBeforeConversationStorageScope, reportConversationPersistence } from "./conversationStorageScope";
/**
 * C10: per-session sent-prompt history and named composer stashes.
 *
 * Draft persistence stays in composerDraftStore (C01/C09). This module uses
 * separate localStorage keys so C04 attachment upload and C09 structured refs
 * keep owning their files. Snapshot shape matches ComposerDraftWriteInput.
 */

import type { ComposerCapabilityRef } from "./composerCapabilities";

export const COMPOSER_HISTORY_STORAGE_KEY = "muteki:composer-prompt-history:v1";
export const COMPOSER_STASH_STORAGE_KEY = "muteki:composer-prompt-stash:v1";

const MAX_HISTORY_PER_SESSION = 50;
const PERSIST_DEBOUNCE_MS = 200;

export type RecallPromptSegment =
  | { type: "text"; text: string }
  | { type: "ref"; nodeId: string };

export type RecallAttachment = {
  id: string;
  name: string;
  size?: number;
  mimeType?: string;
  sha256?: string;
  needsReselect?: boolean;
};

export type RecallSnapshot = {
  prompt: string;
  promptSegments?: RecallPromptSegment[];
  capabilityRefs: ComposerCapabilityRef[];
  attachments: RecallAttachment[];
  projectId?: string;
  credentialId?: string;
  /** Exact adapter:instance identity; optional for legacy saved entries. */
  runtimeKey?: string;
  model?: string;
  effort?: string;
  accessMode?: string;
  updatedAt: number;
};

export type ComposerHistoryEntry = {
  id: string;
  sessionKey: string;
  sentAt: number;
  prompt: string;
  promptSegments?: RecallPromptSegment[];
  capabilityRefs: ComposerCapabilityRef[];
  projectId?: string;
};

export type ComposerStashEntry = {
  id: string;
  name: string;
  createdAt: number;
  draftKey: string;
  projectId?: string;
  snapshot: RecallSnapshot;
};

export type RecallWriteInput = {
  prompt: string;
  promptSegments?: RecallPromptSegment[];
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
  projectId?: string;
  credentialId?: string;
  /** Exact adapter:instance identity; optional for legacy saved entries. */
  runtimeKey?: string;
  model?: string;
  effort?: string;
  accessMode?: string;
};

type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

const historyFile = { bucket: {} as Record<string, ComposerHistoryEntry[]>, dirty: false };
const stashFile = { items: [] as ComposerStashEntry[], dirty: false };
const stashFileBag = new Map<string, File>();
const pendingHistory = new Map<string, ComposerHistoryEntry[]>();
const pendingStashes = new Map<string, ComposerStashEntry | null>();

let persistTimer: ReturnType<typeof setTimeout> | null = null;
let storageOverride: StorageLike | null = null;

function getStorage(ownerScope = conversationStorageScope()): StorageLike | null {
  if (storageOverride) return storageOverride;
  if (typeof window === "undefined" || !ownerScope) return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function newId(prefix: string): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return `${prefix}_${crypto.randomUUID()}`;
  }
  return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

function stashFileKey(stashId: string, attachmentId: string): string {
  return `${stashId}::${attachmentId}`;
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

function normalizePromptSegments(value: unknown): RecallPromptSegment[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const out: RecallPromptSegment[] = [];
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

function normalizeAttachments(value: unknown): RecallAttachment[] {
  if (!Array.isArray(value)) return [];
  const out: RecallAttachment[] = [];
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

function snapshotFromUnknown(value: unknown): RecallSnapshot | null {
  if (!value || typeof value !== "object") return null;
  const row = value as Record<string, unknown>;
  const snapshot: RecallSnapshot = {
    prompt: typeof row.prompt === "string" ? row.prompt : "",
    capabilityRefs: normalizeCapabilityRefs(row.capabilityRefs),
    attachments: normalizeAttachments(row.attachments),
    updatedAt: Number(row.updatedAt) || 0,
  };
  const segments = normalizePromptSegments(row.promptSegments);
  if (segments) snapshot.promptSegments = segments;
  const projectId = String(row.projectId || "").trim();
  const credentialId = String(row.credentialId || "").trim();
  const runtimeKey = String(row.runtimeKey || "").trim();
  const model = String(row.model || "").trim();
  const effort = String(row.effort || "").trim();
  const accessMode = String(row.accessMode || "").trim();
  if (projectId) snapshot.projectId = projectId;
  if (credentialId) snapshot.credentialId = credentialId;
  if (runtimeKey) snapshot.runtimeKey = runtimeKey;
  if (model) snapshot.model = model;
  if (row.effort !== undefined) snapshot.effort = effort;
  if (accessMode) snapshot.accessMode = accessMode;
  return snapshot;
}

function snapshotIsEmpty(snapshot: RecallSnapshot | null | undefined): boolean {
  if (!snapshot) return true;
  return composerRecallBodyEmpty(snapshot)
    && !snapshot.credentialId
    && !snapshot.model
    && !snapshot.effort
    && (!snapshot.accessMode || snapshot.accessMode === "supervised");
}

function fingerprintHistory(entry: Pick<ComposerHistoryEntry, "prompt" | "promptSegments" | "capabilityRefs">): string {
  const refs = entry.capabilityRefs.map((ref) => ref.node_id || ref.id).join(",");
  const segs = (entry.promptSegments || [])
    .map((segment) => (segment.type === "text" ? `t:${segment.text}` : `r:${segment.nodeId}`))
    .join("|");
  return `${entry.prompt}\n${segs}\n${refs}`;
}

function normalizeHistoryEntry(value: unknown): ComposerHistoryEntry | null {
  if (!value || typeof value !== "object") return null;
  const row = value as Record<string, unknown>;
  const snapshot = snapshotFromUnknown({
    prompt: row.prompt,
    promptSegments: row.promptSegments,
    capabilityRefs: row.capabilityRefs,
    attachments: [],
    projectId: row.projectId,
    updatedAt: row.sentAt,
  });
  if (!snapshot) return null;
  const id = String(row.id || "").trim() || newId("hist");
  const sessionKey = String(row.sessionKey || "").trim();
  if (!sessionKey) return null;
  if (!snapshot.prompt.trim() && !snapshot.promptSegments?.length && !snapshot.capabilityRefs.length) {
    return null;
  }
  const projectId = String(row.projectId || snapshot.projectId || "").trim();
  return {
    id,
    sessionKey,
    sentAt: Number(row.sentAt) || snapshot.updatedAt || 0,
    prompt: snapshot.prompt,
    ...(snapshot.promptSegments ? { promptSegments: snapshot.promptSegments } : {}),
    capabilityRefs: snapshot.capabilityRefs,
    ...(projectId ? { projectId } : {}),
  };
}

function normalizeStashEntry(value: unknown): ComposerStashEntry | null {
  if (!value || typeof value !== "object") return null;
  const row = value as Record<string, unknown>;
  const snapshot = snapshotFromUnknown(row.snapshot ?? row);
  if (!snapshot || snapshotIsEmpty(snapshot)) return null;
  const id = String(row.id || "").trim() || newId("stash");
  const name = String(row.name || "").trim() || defaultStashName(snapshot.prompt);
  const draftKey = String(row.draftKey || "").trim();
  const projectId = String(row.projectId || snapshot.projectId || "").trim();
  return {
    id,
    name,
    createdAt: Number(row.createdAt) || snapshot.updatedAt || 0,
    draftKey,
    ...(projectId ? { projectId } : {}),
    snapshot,
  };
}

export function defaultStashName(prompt: string): string {
  const line = String(prompt || "").trim().split(/\r?\n/)[0] || "";
  const clipped = line.slice(0, 40);
  return clipped || "未命名暂存";
}

function readHistoryBucket(): Record<string, ComposerHistoryEntry[]> {
  if (historyFile.dirty) return historyFile.bucket;
  const storage = getStorage();
  if (!storage) return historyFile.bucket;
  try {
    const raw = storage.getItem(conversationStorageKey(COMPOSER_HISTORY_STORAGE_KEY));
    if (!raw) {
      historyFile.bucket = {};
      return historyFile.bucket;
    }
    const parsed: unknown = JSON.parse(raw);
    const rows = (
      parsed && typeof parsed === "object" && !Array.isArray(parsed)
        && (parsed as Record<string, unknown>).sessions
        && typeof (parsed as Record<string, unknown>).sessions === "object"
        ? (parsed as { sessions: Record<string, unknown> }).sessions
        : parsed
    ) as Record<string, unknown>;
    const next: Record<string, ComposerHistoryEntry[]> = {};
    if (rows && typeof rows === "object" && !Array.isArray(rows)) {
      for (const [key, value] of Object.entries(rows)) {
        if (!Array.isArray(value)) continue;
        const items = value.map(normalizeHistoryEntry).filter(Boolean) as ComposerHistoryEntry[];
        if (items.length) next[key] = items.slice(0, MAX_HISTORY_PER_SESSION);
      }
    }
    historyFile.bucket = next;
    return historyFile.bucket;
  } catch {
    return historyFile.bucket;
  }
}

function readStashes(): ComposerStashEntry[] {
  if (stashFile.dirty) return stashFile.items;
  const storage = getStorage();
  if (!storage) return stashFile.items;
  try {
    const raw = storage.getItem(conversationStorageKey(COMPOSER_STASH_STORAGE_KEY));
    if (!raw) {
      stashFile.items = [];
      return stashFile.items;
    }
    const parsed: unknown = JSON.parse(raw);
    const list = (
      parsed && typeof parsed === "object" && !Array.isArray(parsed)
        && Array.isArray((parsed as Record<string, unknown>).stashes)
        ? (parsed as { stashes: unknown[] }).stashes
        : Array.isArray(parsed) ? parsed : []
    );
    stashFile.items = list.map(normalizeStashEntry).filter(Boolean) as ComposerStashEntry[];
    return stashFile.items;
  } catch {
    return stashFile.items;
  }
}

function flushNow(ownerScope = conversationStorageScope(), memory = { history: historyFile, stash: stashFile, historyEdits: pendingHistory, stashEdits: pendingStashes }): { persisted: boolean; error?: string } {
  if (!memory.history.dirty && !memory.stash.dirty) return { persisted: true };
  const storage = getStorage(ownerScope);
  if (!storage) return { persisted: false, error: "recall.storage.unavailable: 历史与暂存存储不可用" };
  const errors: string[] = [];
  if (memory.history.dirty) {
    try {
      const raw = storage.getItem(conversationStorageKey(COMPOSER_HISTORY_STORAGE_KEY, ownerScope));
      const parsed = raw ? JSON.parse(raw) : {};
      const sessions = parsed.sessions && typeof parsed.sessions === "object" ? parsed.sessions : {};
      for (const [key, edits] of memory.historyEdits) {
        const existing = Array.isArray(sessions[key]) ? sessions[key].map(normalizeHistoryEntry).filter(Boolean) as ComposerHistoryEntry[] : [];
        sessions[key] = [...new Map([...edits, ...existing].map((entry) => [entry.id, entry])).values()]
          .sort((a, b) => b.sentAt - a.sentAt).slice(0, MAX_HISTORY_PER_SESSION);
      }
      storage.setItem(conversationStorageKey(COMPOSER_HISTORY_STORAGE_KEY, ownerScope), JSON.stringify({ sessions }));
      memory.history.bucket = sessions;
      memory.history.dirty = false;
      memory.historyEdits.clear();
    } catch (error) { errors.push(error instanceof Error ? `${error.name}: ${error.message}` : String(error)); }
  }
  if (memory.stash.dirty) {
    try {
      const raw = storage.getItem(conversationStorageKey(COMPOSER_STASH_STORAGE_KEY, ownerScope));
      const parsed = raw ? JSON.parse(raw) : {};
      const existing = Array.isArray(parsed.stashes) ? parsed.stashes.map(normalizeStashEntry).filter(Boolean) as ComposerStashEntry[] : [];
      const merged = new Map(existing.map((entry) => [entry.id, entry]));
      for (const [id, entry] of memory.stashEdits) { if (entry) merged.set(id, entry); else merged.delete(id); }
      const stashes = [...merged.values()].sort((a, b) => b.createdAt - a.createdAt);
      storage.setItem(conversationStorageKey(COMPOSER_STASH_STORAGE_KEY, ownerScope), JSON.stringify({ stashes }));
      memory.stash.items = stashes;
      memory.stash.dirty = false;
      memory.stashEdits.clear();
    } catch (error) { errors.push(error instanceof Error ? `${error.name}: ${error.message}` : String(error)); }
  }
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}

function schedulePersist(): void {
  if (typeof window === "undefined" && !storageOverride) return;
  if (persistTimer) clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    persistTimer = null;
    const result = flushNow();
    if (!result.persisted) reportConversationPersistence("recall", result.error || "历史与暂存保存失败");
  }, PERSIST_DEBOUNCE_MS);
}

export function flushComposerRecallStore(): { persisted: boolean; error?: string } {
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  const results = [flushNow()];
  for (const [scope, snapshot] of scopedRecall) if (scope !== conversationStorageScope()) results.push(flushNow(scope, snapshot));
  const errors = results.filter((row) => !row.persisted).map((row) => row.error || "历史与暂存保存失败");
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}

export function listPromptHistory(sessionKey: string): ComposerHistoryEntry[] {
  const key = String(sessionKey || "").trim();
  if (!key) return [];
  const bucket = readHistoryBucket();
  return [...(bucket[key] || [])];
}

export function recordSentPrompt(
  sessionKey: string,
  input: {
    prompt: string;
    promptSegments?: RecallPromptSegment[];
    capabilityRefs: unknown[];
    projectId?: string;
  },
): ComposerHistoryEntry | null {
  const key = String(sessionKey || "").trim();
  if (!key) return null;
  const snapshot = snapshotFromUnknown({
    prompt: input.prompt,
    promptSegments: input.promptSegments,
    capabilityRefs: input.capabilityRefs,
    attachments: [],
    projectId: input.projectId,
    updatedAt: Date.now(),
  });
  if (!snapshot) return null;
  if (!snapshot.prompt.trim() && !snapshot.promptSegments?.length && !snapshot.capabilityRefs.length) {
    return null;
  }
  const entry: ComposerHistoryEntry = {
    id: newId("hist"),
    sessionKey: key,
    sentAt: Date.now(),
    prompt: snapshot.prompt,
    ...(snapshot.promptSegments ? { promptSegments: snapshot.promptSegments } : {}),
    capabilityRefs: snapshot.capabilityRefs,
    ...(snapshot.projectId ? { projectId: snapshot.projectId } : {}),
  };
  const bucket = readHistoryBucket();
  const current = [...(bucket[key] || [])];
  const fp = fingerprintHistory(entry);
  if (current[0] && fingerprintHistory(current[0]) === fp) {
    return current[0];
  }
  current.unshift(entry);
  bucket[key] = current.slice(0, MAX_HISTORY_PER_SESSION);
  historyFile.bucket = { ...bucket };
  historyFile.dirty = true;
  pendingHistory.set(key, [entry, ...(pendingHistory.get(key) || [])]);
  schedulePersist();
  return entry;
}

export function listComposerStashes(): ComposerStashEntry[] {
  return [...readStashes()].sort((a, b) => b.createdAt - a.createdAt);
}

export function saveComposerStash(input: {
  name?: string;
  draftKey: string;
  projectId?: string;
  snapshot: RecallWriteInput;
}): ComposerStashEntry | null {
  const snapshot = snapshotFromUnknown({
    ...input.snapshot,
    updatedAt: Date.now(),
  });
  if (!snapshot || snapshotIsEmpty(snapshot)) return null;

  const attachments: RecallAttachment[] = snapshot.attachments.map((item) => ({ ...item }));
  const id = newId("stash");
  for (const raw of input.snapshot.attachments) {
    const match = attachments.find((item) => item.id === raw.id || item.name === raw.name);
    if (match && raw.file) {
      stashFileBag.set(stashFileKey(id, match.id), raw.file);
      match.needsReselect = false;
    }
  }

  const entry: ComposerStashEntry = {
    id,
    name: String(input.name || "").trim() || defaultStashName(snapshot.prompt),
    createdAt: Date.now(),
    draftKey: String(input.draftKey || "").trim(),
    ...(String(input.projectId || snapshot.projectId || "").trim()
      ? { projectId: String(input.projectId || snapshot.projectId || "").trim() }
      : {}),
    snapshot: { ...snapshot, attachments },
  };

  const items = readStashes();
  items.unshift(entry);
  stashFile.items = items;
  pendingStashes.set(id, entry);
  stashFile.dirty = true;
  schedulePersist();
  return entry;
}

export function deleteComposerStash(stashId: string): void {
  const id = String(stashId || "").trim();
  if (!id) return;
  stashFile.items = readStashes().filter((item) => item.id !== id);
  for (const key of Array.from(stashFileBag.keys())) {
    if (key.startsWith(`${id}::`)) stashFileBag.delete(key);
  }
  stashFile.dirty = true;
  pendingStashes.set(id, null);
  schedulePersist();
}

export type HydratedStashRestore = {
  entry: ComposerStashEntry;
  attachments: Array<RecallAttachment & { file?: File; type?: string }>;
  restoredNeedsReselect: boolean;
  environmentMismatch: boolean;
};

export function hydrateComposerStash(
  stashId: string,
  current: { draftKey: string; projectId: string },
): HydratedStashRestore | null {
  const entry = readStashes().find((item) => item.id === stashId);
  if (!entry) return null;
  let restoredNeedsReselect = false;
  const attachments = entry.snapshot.attachments.map((item) => {
    const file = stashFileBag.get(stashFileKey(entry.id, item.id));
    if (file) {
      return { ...item, needsReselect: false, file, type: item.mimeType || file.type };
    }
    if (item.sha256) {
      return { ...item, needsReselect: false, type: item.mimeType };
    }
    restoredNeedsReselect = true;
    return { ...item, needsReselect: true, type: item.mimeType };
  });
  const currentProject = String(current.projectId || "").trim();
  const stashProject = String(entry.projectId || entry.snapshot.projectId || "").trim();
  const environmentMismatch = Boolean(stashProject && stashProject !== currentProject);
  return {
    entry,
    attachments,
    restoredNeedsReselect,
    environmentMismatch,
  };
}

export type HistoryDirection = "older" | "newer";

export function stepHistoryIndex(
  direction: HistoryDirection,
  currentIndex: number | null,
  length: number,
  canStart: boolean,
): { index: number | null; handled: boolean } {
  if (length <= 0) return { index: currentIndex, handled: false };
  if (currentIndex === null) {
    if (direction === "newer" || !canStart) return { index: null, handled: false };
    return { index: 0, handled: true };
  }
  if (direction === "older") {
    return { index: Math.min(length - 1, currentIndex + 1), handled: true };
  }
  if (direction === "newer") {
    if (currentIndex <= 0) return { index: null, handled: true };
    return { index: currentIndex - 1, handled: true };
  }
  const _never: never = direction;
  return _never;
}

export function composerRecallBodyEmpty(input: {
  prompt?: string;
  promptSegments?: RecallPromptSegment[];
  capabilityRefs?: ComposerCapabilityRef[];
  attachments?: { length: number };
}): boolean {
  if (String(input.prompt || "").trim()) return false;
  if (input.promptSegments?.some((segment) => (
    segment.type === "ref" || (segment.type === "text" && segment.text.trim())
  ))) return false;
  if (input.capabilityRefs?.length) return false;
  if (input.attachments?.length) return false;
  return true;
}

/** Clear history + stash (logout / auth lock). */
export function clearAllComposerRecall(): void {
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  historyFile.bucket = {};
  historyFile.dirty = false;
  stashFile.items = [];
  stashFile.dirty = false;
  stashFileBag.clear();
  pendingHistory.clear();
  pendingStashes.clear();
  const storage = getStorage();
  if (!storage) return;
  try {
    storage.removeItem(conversationStorageKey(COMPOSER_HISTORY_STORAGE_KEY));
    storage.removeItem(conversationStorageKey(COMPOSER_STASH_STORAGE_KEY));
  } catch {
    // ignore
  }
}

export function __resetComposerRecallStoreForTests(storage?: StorageLike | null): void {
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  historyFile.bucket = {};
  historyFile.dirty = false;
  stashFile.items = [];
  stashFile.dirty = false;
  stashFileBag.clear();
  pendingHistory.clear();
  pendingStashes.clear();
  storageOverride = storage === undefined ? null : storage;
}

/** Test helper: in-memory File bag size (Bugbot Low — trimmed stashes must release Files). */
export function __stashFileBagSizeForTests(): number {
  return stashFileBag.size;
}

const scopedRecall = new Map<string, { history: typeof historyFile; stash: typeof stashFile; historyEdits: typeof pendingHistory; stashEdits: typeof pendingStashes; files: typeof stashFileBag }>();
subscribeBeforeConversationStorageScope(() => {
  flushComposerRecallStore();
  scopedRecall.set(conversationStorageScope(), { history: { ...historyFile }, stash: { ...stashFile }, historyEdits: new Map(pendingHistory), stashEdits: new Map(pendingStashes), files: new Map(stashFileBag) });
});
subscribeConversationStorageScope(() => {
  const old = scopedRecall.get(conversationStorageScope());
  historyFile.bucket = old?.history.bucket || {}; historyFile.dirty = old?.history.dirty || false;
  stashFile.items = old?.stash.items || []; stashFile.dirty = old?.stash.dirty || false;
  pendingHistory.clear(); pendingStashes.clear(); stashFileBag.clear();
  for (const [key, value] of old?.historyEdits || []) pendingHistory.set(key, value);
  for (const [key, value] of old?.stashEdits || []) pendingStashes.set(key, value);
  for (const [key, value] of old?.files || []) stashFileBag.set(key, value);
});
