import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope, subscribeBeforeConversationStorageScope, reportConversationPersistence } from "./conversationStorageScope";
/**
 * Per-thread answer drafts for pending Conversation user-input cards (C22).
 *
 * Separate namespace from composer drafts so answer state never leaks into
 * prompt text when switching Threads.
 */

export const USER_INPUT_DRAFT_STORAGE_KEY = "muteki:user-input-drafts:v1";

export type UserInputAnswerEntry = {
  values: string[];
  text?: string;
};

export type UserInputAnswerMap = Record<string, UserInputAnswerEntry>;

export type UserInputDraft = {
  requestId: string;
  answers: UserInputAnswerMap;
  updatedAt: number;
};

type DraftBucket = Record<string, UserInputDraft>;
type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

let memoryBucket: DraftBucket = {};
const pending = new Map<string, UserInputDraft | null>();
let storageOverride: StorageLike | null = null;

function draftKey(threadId: string, requestId: string): string {
  return `${threadId}::${requestId}`;
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

function readBucket(): DraftBucket {
  if (pending.size) return { ...memoryBucket };
  const storage = getStorage();
  if (!storage) return { ...memoryBucket };
  try {
    const raw = storage.getItem(conversationStorageKey(USER_INPUT_DRAFT_STORAGE_KEY));
    if (!raw) return { ...memoryBucket };
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return { ...memoryBucket };
    }
    const row = parsed as Record<string, unknown>;
    const draftsRaw = (
      row.drafts && typeof row.drafts === "object" && !Array.isArray(row.drafts)
        ? row.drafts
        : parsed
    ) as Record<string, unknown>;
    const next: DraftBucket = {};
    for (const [key, value] of Object.entries(draftsRaw)) {
      if (!value || typeof value !== "object" || Array.isArray(value)) continue;
      const draft = value as Record<string, unknown>;
      const requestId = typeof draft.requestId === "string" ? draft.requestId : "";
      const answers =
        draft.answers && typeof draft.answers === "object" && !Array.isArray(draft.answers)
          ? (draft.answers as UserInputAnswerMap)
          : {};
      if (!requestId) continue;
      next[key] = {
        requestId,
        answers,
        updatedAt: typeof draft.updatedAt === "number" ? draft.updatedAt : Date.now(),
      };
    }
    memoryBucket = next;
    return { ...memoryBucket };
  } catch {
    return { ...memoryBucket };
  }
}

function writeBucket(bucket: DraftBucket, journal = pending, ownerScope = conversationStorageScope()): { persisted: boolean; error?: string } {
  if (journal === pending) for (const key of new Set([...Object.keys(memoryBucket), ...Object.keys(bucket)])) {
    if (JSON.stringify(memoryBucket[key]) !== JSON.stringify(bucket[key])) pending.set(key, bucket[key] || null);
  }
  if (journal === pending) memoryBucket = { ...bucket };
  const storage = getStorage(ownerScope);
  if (!storage) return { persisted: false, error: "answer.storage.unavailable: 回答草稿存储不可用" };
  try {
    const raw = storage.getItem(conversationStorageKey(USER_INPUT_DRAFT_STORAGE_KEY, ownerScope));
    const parsed = raw ? JSON.parse(raw) : {};
    const merged: DraftBucket = parsed.drafts && typeof parsed.drafts === "object" ? { ...parsed.drafts } : {};
    for (const [key, draft] of journal) { if (draft) merged[key] = draft; else delete merged[key]; }
    storage.setItem(conversationStorageKey(USER_INPUT_DRAFT_STORAGE_KEY, ownerScope), JSON.stringify({ drafts: merged }));
    if (journal === pending) memoryBucket = merged;
    journal.clear();
    return { persisted: true };
  } catch (error) {
    const message = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
    reportConversationPersistence("user-input", message);
    return { persisted: false, error: message };
  }
}

export function flushUserInputDraftStore(): { persisted: boolean; error?: string } {
  const results = [pending.size ? writeBucket(memoryBucket) : { persisted: true }];
  for (const [scope, snapshot] of scopedAnswers) if (scope !== conversationStorageScope() && snapshot.edits.size) results.push(writeBucket(snapshot.bucket, snapshot.edits, scope));
  const errors = results.filter((row) => !row.persisted).map((row) => row.error || "回答草稿保存失败");
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}

export function loadUserInputDraft(
  threadId: string,
  requestId: string,
): UserInputDraft | null {
  if (!threadId || !requestId) return null;
  const bucket = readBucket();
  const draft = bucket[draftKey(threadId, requestId)];
  if (!draft || draft.requestId !== requestId) return null;
  return { ...draft, answers: { ...draft.answers } };
}

export function saveUserInputDraft(
  threadId: string,
  requestId: string,
  answers: UserInputAnswerMap,
): void {
  if (!threadId || !requestId) return;
  const bucket = readBucket();
  const key = draftKey(threadId, requestId);
  const empty = Object.keys(answers).length === 0;
  if (empty) {
    if (key in bucket) {
      delete bucket[key];
      writeBucket(bucket);
    }
    return;
  }
  bucket[key] = {
    requestId,
    answers: { ...answers },
    updatedAt: Date.now(),
  };
  writeBucket(bucket);
}

export function clearUserInputDraft(threadId: string, requestId?: string): void {
  if (!threadId) return;
  const bucket = readBucket();
  if (requestId) {
    delete bucket[draftKey(threadId, requestId)];
  } else {
    const prefix = `${threadId}::`;
    for (const key of Object.keys(bucket)) {
      if (key.startsWith(prefix)) delete bucket[key];
    }
  }
  writeBucket(bucket);
}

/** Test helper: inject storage and reset memory. */
export function _resetUserInputDraftStoreForTests(storage: StorageLike | null = null): void {
  storageOverride = storage;
  memoryBucket = {};
  pending.clear();
}

const scopedAnswers = new Map<string, { bucket: DraftBucket; edits: typeof pending }>();
subscribeBeforeConversationStorageScope(() => {
  flushUserInputDraftStore();
  scopedAnswers.set(conversationStorageScope(), { bucket: memoryBucket, edits: new Map(pending) });
});
subscribeConversationStorageScope(() => {
  const old = scopedAnswers.get(conversationStorageScope()); memoryBucket = old?.bucket || {}; pending.clear();
  for (const [key, value] of old?.edits || []) pending.set(key, value);
});
