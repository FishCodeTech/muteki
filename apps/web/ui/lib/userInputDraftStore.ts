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
let storageOverride: StorageLike | null = null;

function draftKey(threadId: string, requestId: string): string {
  return `${threadId}::${requestId}`;
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

function readBucket(): DraftBucket {
  const storage = getStorage();
  if (!storage) return { ...memoryBucket };
  try {
    const raw = storage.getItem(USER_INPUT_DRAFT_STORAGE_KEY);
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

function writeBucket(bucket: DraftBucket): void {
  memoryBucket = { ...bucket };
  const storage = getStorage();
  if (!storage) return;
  try {
    storage.setItem(
      USER_INPUT_DRAFT_STORAGE_KEY,
      JSON.stringify({ drafts: memoryBucket }),
    );
  } catch {
    // Quota / private mode — keep memory copy only.
  }
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
}
