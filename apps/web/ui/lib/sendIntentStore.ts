/**
 * Persistent send / create / retry intent for Conversation (C02).
 *
 * One user send attempt keeps stable command_id / idempotency_key /
 * client_message_id values across network retries so the backend Command API
 * and Turn queue dedupe instead of minting a second logical message.
 */

export const SEND_INTENT_STORAGE_KEY = "muteki:send-intents:v1";

export type SendIntentPhase =
  | "creating_thread"
  | "uploading"
  | "sending"
  | "accepted"
  | "unknown"
  | "failed";

export type SendUploadIntent = {
  attachmentId: string;
  commandId: string;
  sha256?: string;
};

export type SendIntent = {
  intentId: string;
  draftKey: string;
  phase: SendIntentPhase;
  text: string;
  /** C09 structured context nodes carried with the stable send intent. */
  capabilityRefs?: Array<Record<string, unknown>>;
  /**
   * Runtime fields sent with turn.send / thread.create (adapter, model, …).
   * Part of the logical-send fingerprint (#185).
   */
  runtime?: Record<string, unknown>;
  /** Project/workspace choices frozen for thread creation, separate from runtime. */
  threadContext?: Record<string, unknown>;
  createCommandId?: string;
  threadId?: string;
  uploads: SendUploadIntent[];
  sendCommandId: string;
  clientMessageId: string;
  lastReceiptState?: string;
  /** Command whose HTTP outcome could not be recovered yet. */
  pendingPhase?: "creating_thread" | "sending";
  lastError?: string;
  updatedAt: number;
};

export type RetryIntent = {
  threadId: string;
  turnId: string;
  commandId: string;
  phase: "sending" | "accepted" | "unknown" | "failed";
  lastReceiptState?: string;
  lastError?: string;
  updatedAt: number;
};

type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

type IntentBucket = {
  sends: Record<string, SendIntent>;
  retries: Record<string, RetryIntent>;
};

let memoryBucket: IntentBucket = { sends: {}, retries: {} };
let memoryDirty = false;
let storageOverride: StorageLike | null = null;

function getStorage(): StorageLike | null {
  if (storageOverride) return storageOverride;
  if (typeof window === "undefined") return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function emptyBucket(): IntentBucket {
  return { sends: {}, retries: {} };
}

function readBucket(): IntentBucket {
  if (memoryDirty) {
    return {
      sends: { ...memoryBucket.sends },
      retries: { ...memoryBucket.retries },
    };
  }
  const storage = getStorage();
  if (!storage) {
    return {
      sends: { ...memoryBucket.sends },
      retries: { ...memoryBucket.retries },
    };
  }
  try {
    const raw = storage.getItem(SEND_INTENT_STORAGE_KEY);
    if (!raw) {
      memoryBucket = emptyBucket();
      return emptyBucket();
    }
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return emptyBucket();
    }
    const row = parsed as Record<string, unknown>;
    const sendsRaw = (
      row.sends && typeof row.sends === "object" && !Array.isArray(row.sends)
        ? row.sends
        : {}
    ) as Record<string, unknown>;
    const retriesRaw = (
      row.retries && typeof row.retries === "object" && !Array.isArray(row.retries)
        ? row.retries
        : {}
    ) as Record<string, unknown>;
    const sends: Record<string, SendIntent> = {};
    for (const [key, value] of Object.entries(sendsRaw)) {
      const intent = normalizeSendIntent(value);
      if (intent) sends[key] = intent;
    }
    const retries: Record<string, RetryIntent> = {};
    for (const [key, value] of Object.entries(retriesRaw)) {
      const intent = normalizeRetryIntent(value);
      if (intent) retries[key] = intent;
    }
    memoryBucket = { sends, retries };
    return {
      sends: { ...sends },
      retries: { ...retries },
    };
  } catch {
    return {
      sends: { ...memoryBucket.sends },
      retries: { ...memoryBucket.retries },
    };
  }
}

function writeBucket(bucket: IntentBucket): void {
  memoryBucket = {
    sends: { ...bucket.sends },
    retries: { ...bucket.retries },
  };
  const storage = getStorage();
  if (!storage) {
    // No backing store — keep memory as the latest source of truth.
    memoryDirty = true;
    return;
  }
  try {
    storage.setItem(SEND_INTENT_STORAGE_KEY, JSON.stringify(memoryBucket));
    // Only clear dirty after a successful persist.
    memoryDirty = false;
  } catch {
    // Quota / private mode / write-disabled — prefer in-memory copy on next read
    // so stable command IDs survive even when storage is empty or stale.
    memoryDirty = true;
  }
}

function newStableId(prefix: string): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return `${prefix}_${crypto.randomUUID()}`;
  }
  return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

function normalizeSendIntent(value: unknown): SendIntent | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const row = value as Record<string, unknown>;
  const intentId = String(row.intentId || "").trim();
  const draftKey = String(row.draftKey || "").trim();
  const sendCommandId = String(row.sendCommandId || "").trim();
  const clientMessageId = String(row.clientMessageId || "").trim();
  const text = String(row.text || "");
  const phase = String(row.phase || "").trim() as SendIntentPhase;
  if (!intentId || !draftKey || !sendCommandId || !clientMessageId) return null;
  if (
    phase !== "creating_thread"
    && phase !== "uploading"
    && phase !== "sending"
    && phase !== "accepted"
    && phase !== "unknown"
    && phase !== "failed"
  ) {
    return null;
  }
  const uploadsRaw = Array.isArray(row.uploads) ? row.uploads : [];
  const uploads: SendUploadIntent[] = [];
  for (const item of uploadsRaw) {
    if (!item || typeof item !== "object" || Array.isArray(item)) continue;
    const upload = item as Record<string, unknown>;
    const attachmentId = String(upload.attachmentId || "").trim();
    const commandId = String(upload.commandId || "").trim();
    if (!attachmentId || !commandId) continue;
    const sha256 = String(upload.sha256 || "").trim();
    uploads.push({
      attachmentId,
      commandId,
      ...(sha256 ? { sha256 } : {}),
    });
  }
  const createCommandId = String(row.createCommandId || "").trim();
  const threadId = String(row.threadId || "").trim();
  const lastReceiptState = String(row.lastReceiptState || "").trim();
  const lastError = String(row.lastError || "").trim();
  const updatedAt = typeof row.updatedAt === "number" ? row.updatedAt : Date.now();
  const capabilityRefs = Array.isArray(row.capabilityRefs)
    ? row.capabilityRefs.filter((item): item is Record<string, unknown> => (
      Boolean(item) && typeof item === "object" && !Array.isArray(item)
    )).map((item) => ({ ...item }))
    : undefined;
  const runtime = normalizeRuntimeRecord(
    row.runtime && typeof row.runtime === "object" && !Array.isArray(row.runtime)
      ? row.runtime as Record<string, unknown>
      : undefined,
  );
  const threadContext = normalizeRuntimeRecord(
    row.threadContext && typeof row.threadContext === "object" && !Array.isArray(row.threadContext)
      ? row.threadContext as Record<string, unknown>
      : undefined,
  );
  return {
    intentId,
    draftKey,
    phase,
    text,
    ...(capabilityRefs?.length ? { capabilityRefs } : {}),
    ...(runtime ? { runtime } : {}),
    ...(threadContext ? { threadContext } : {}),
    ...(createCommandId ? { createCommandId } : {}),
    ...(threadId ? { threadId } : {}),
    uploads,
    sendCommandId,
    clientMessageId,
    ...(lastReceiptState ? { lastReceiptState } : {}),
    ...(row.pendingPhase === "creating_thread" || row.pendingPhase === "sending"
      ? { pendingPhase: row.pendingPhase } : {}),
    ...(lastError ? { lastError } : {}),
    updatedAt,
  };
}

function normalizeRetryIntent(value: unknown): RetryIntent | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const row = value as Record<string, unknown>;
  const threadId = String(row.threadId || "").trim();
  const turnId = String(row.turnId || "").trim();
  const commandId = String(row.commandId || "").trim();
  const phase = String(row.phase || "").trim();
  if (!threadId || !turnId || !commandId) return null;
  if (
    phase !== "sending"
    && phase !== "accepted"
    && phase !== "unknown"
    && phase !== "failed"
  ) {
    return null;
  }
  const lastReceiptState = String(row.lastReceiptState || "").trim();
  const lastError = String(row.lastError || "").trim();
  const updatedAt = typeof row.updatedAt === "number" ? row.updatedAt : Date.now();
  return {
    threadId,
    turnId,
    commandId,
    phase,
    ...(lastReceiptState ? { lastReceiptState } : {}),
    ...(lastError ? { lastError } : {}),
    updatedAt,
  };
}

export function retryIntentKey(threadId: string, turnId: string): string {
  return `${String(threadId || "").trim()}::${String(turnId || "").trim()}`;
}

/** Read an in-flight send intent for a composer draft key. */
export function readSendIntent(draftKey: string): SendIntent | null {
  const key = String(draftKey || "").trim();
  if (!key) return null;
  return readBucket().sends[key] || null;
}


/** Stable JSON fingerprint for ordered attachment client ids. */
export function fingerprintAttachmentIds(attachmentIds: string[] = []): string {
  return JSON.stringify(
    (attachmentIds || [])
      .map((id) => String(id || "").trim())
      .filter(Boolean),
  );
}

/**
 * Canonical runtime fingerprint for logical-send identity.
 * Empty / blank values are dropped; keys are sorted for stable compare.
 */
export function fingerprintRuntime(
  runtime: Record<string, unknown> | null | undefined = {},
): string {
  if (!runtime || typeof runtime !== "object" || Array.isArray(runtime)) {
    return "{}";
  }
  const out: Record<string, unknown> = {};
  for (const key of Object.keys(runtime).sort()) {
    const raw = runtime[key];
    if (raw === undefined || raw === null) continue;
    if (typeof raw === "string") {
      const trimmed = raw.trim();
      if (!trimmed) continue;
      out[key] = trimmed;
      continue;
    }
    out[key] = raw;
  }
  return JSON.stringify(out);
}

export function fingerprintCapabilityRefs(
  capabilityRefs: Array<Record<string, unknown>> = [],
): string {
  return JSON.stringify(capabilityRefs || []);
}

function normalizeRuntimeRecord(
  runtime: Record<string, unknown> | null | undefined,
): Record<string, unknown> | undefined {
  if (!runtime || typeof runtime !== "object" || Array.isArray(runtime)) {
    return undefined;
  }
  const out: Record<string, unknown> = {};
  for (const key of Object.keys(runtime).sort()) {
    const raw = runtime[key];
    if (raw === undefined || raw === null) continue;
    if (typeof raw === "string") {
      const trimmed = raw.trim();
      if (!trimmed) continue;
      out[key] = trimmed;
      continue;
    }
    out[key] = raw;
  }
  return Object.keys(out).length ? out : undefined;
}

/**
 * Ensure a send intent for this draft + semantic payload.
 * Reuses the prior intent when text, capability refs, attachment ids, and
 * runtime fingerprint all match and the prior attempt is still recoverable;
 * otherwise mints fresh stable ids (#185).
 */
export function sendIntentMatchesPayload(
  intent: SendIntent,
  text: string,
  attachmentIds: string[] = [],
  capabilityRefs: Array<Record<string, unknown>> = [],
  runtime: Record<string, unknown> = {},
  threadContext: Record<string, unknown> = {},
): boolean {
  return intent.text === String(text || "")
    && fingerprintCapabilityRefs(intent.capabilityRefs) === fingerprintCapabilityRefs(capabilityRefs)
    && fingerprintAttachmentIds(intent.uploads.map((row) => row.attachmentId)) === fingerprintAttachmentIds(attachmentIds)
    && fingerprintRuntime(intent.runtime) === fingerprintRuntime(runtime)
    && fingerprintRuntime(intent.threadContext) === fingerprintRuntime(threadContext);
}

type RecoverableSendReceipt = {
  state?: string;
  error?: unknown;
  aggregate?: { id?: string };
};

/** Resolve an old uncertain command before semantic edits replace its identity. */
export async function prepareSendIntent(
  draftKey: string,
  text: string,
  attachmentIds: string[],
  capabilityRefs: Array<Record<string, unknown>>,
  runtime: Record<string, unknown>,
  recover: (commandId: string) => Promise<RecoverableSendReceipt | null>,
  threadContext: Record<string, unknown> = {},
): Promise<SendIntent> {
  const existing = readSendIntent(draftKey);
  if (existing && !sendIntentMatchesPayload(existing, text, attachmentIds, capabilityRefs, runtime, threadContext)) {
    const pending = existing.phase === "unknown" ? existing.pendingPhase : existing.phase;
    if (pending === "sending" || pending === "creating_thread") {
      const commandId = pending === "creating_thread" ? existing.createCommandId : existing.sendCommandId;
      let receipt: RecoverableSendReceipt | null = null;
      try { receipt = commandId ? await recover(commandId) : null; } catch { /* keep original identity */ }
      if (!receipt) {
        throw new Error("上一条请求的结果尚未确认，草稿已保留。请恢复连接后再次发送，系统会先查询原请求回执");
      }
      const rejected = Boolean(receipt.error) || ["failed", "rejected", "conflict", "cancelled"].includes(receipt.state || "");
      const createdThread = pending === "creating_thread" && !rejected ? receipt.aggregate?.id : undefined;
      if (pending === "creating_thread" && !rejected && !createdThread) {
        throw new Error("创建对话的回执尚未包含对话标识，草稿已保留，请稍后重试");
      }
      updateSendIntent(draftKey, {
        phase: rejected ? "failed" : pending === "creating_thread" ? "uploading" : "accepted",
        pendingPhase: undefined,
        lastReceiptState: receipt.state,
        ...(createdThread ? { threadId: createdThread } : {}),
      });
    }
  }
  return ensureSendIntent(draftKey, text, attachmentIds, capabilityRefs, runtime, threadContext);
}

export function ensureSendIntent(
  draftKey: string,
  text: string,
  attachmentIds: string[] = [],
  capabilityRefs: Array<Record<string, unknown>> = [],
  runtime: Record<string, unknown> = {},
  threadContext: Record<string, unknown> = {},
): SendIntent {
  const key = String(draftKey || "").trim();
  const normalizedText = String(text || "");
  const refsFingerprint = fingerprintCapabilityRefs(capabilityRefs);
  const attachmentFingerprint = fingerprintAttachmentIds(attachmentIds);
  const normalizedRuntime = normalizeRuntimeRecord(runtime);
  const runtimeFingerprint = fingerprintRuntime(normalizedRuntime);
  const normalizedThreadContext = normalizeRuntimeRecord(threadContext);
  const contextFingerprint = fingerprintRuntime(normalizedThreadContext);
  if (!key) {
    throw new Error("send intent requires a draft key");
  }
  const bucket = readBucket();
  const existing = bucket.sends[key];
  const existingRefsFingerprint = fingerprintCapabilityRefs(existing?.capabilityRefs || []);
  const existingAttachmentFingerprint = fingerprintAttachmentIds(
    (existing?.uploads || []).map((row) => row.attachmentId),
  );
  const existingRuntimeFingerprint = fingerprintRuntime(existing?.runtime);
  const sameThreadContext = fingerprintRuntime(existing?.threadContext) === contextFingerprint;
  // Full semantic payload identity: text + cites + attachment ids + runtime.
  // Same payload → reuse stable command/client ids (network retry / receipt recover).
  // Any semantic edit after a confirmed failure → mint a new logical send (#185).
  const reusable = Boolean(
    existing
    && existing.text === normalizedText
    && existingRefsFingerprint === refsFingerprint
    && existingAttachmentFingerprint === attachmentFingerprint
    && existingRuntimeFingerprint === runtimeFingerprint
    && sameThreadContext
    && existing.phase !== "accepted",
  );
  if (existing?.phase === "unknown" && !reusable) {
    throw new Error("原请求结果尚未确认，必须先恢复回执再更改发送身份");
  }
  if (reusable && existing) {
    const known = new Map(existing.uploads.map((row) => [row.attachmentId, row]));
    const uploads: SendUploadIntent[] = attachmentIds.map((attachmentId) => {
      const prior = known.get(attachmentId);
      if (prior) return { ...prior };
      return {
        attachmentId,
        commandId: newStableId("cmd_upload"),
      };
    });
    const next: SendIntent = {
      ...existing,
      uploads,
      ...(capabilityRefs.length
        ? { capabilityRefs: capabilityRefs.map((row) => ({ ...row })) }
        : { capabilityRefs: undefined }),
      ...(normalizedRuntime
        ? { runtime: { ...normalizedRuntime } }
        : { runtime: undefined }),
      threadContext: normalizedThreadContext,
      updatedAt: Date.now(),
    };
    bucket.sends[key] = next;
    writeBucket(bucket);
    return {
      ...next,
      uploads: uploads.map((row) => ({ ...row })),
      ...(next.capabilityRefs
        ? { capabilityRefs: next.capabilityRefs.map((row) => ({ ...row })) }
        : {}),
      ...(next.runtime ? { runtime: { ...next.runtime } } : {}),
    };
  }

  const uploads: SendUploadIntent[] = attachmentIds.map((attachmentId) => ({
    attachmentId,
    commandId: newStableId("cmd_upload"),
  }));
  const createCommandId = newStableId("cmd_create");
  const sendCommandId = newStableId("cmd_send");
  const next: SendIntent = {
    intentId: newStableId("intent"),
    draftKey: key,
    phase: "creating_thread",
    text: normalizedText,
    ...(capabilityRefs.length
      ? { capabilityRefs: capabilityRefs.map((row) => ({ ...row })) }
      : {}),
    ...(normalizedRuntime ? { runtime: { ...normalizedRuntime } } : {}),
    ...(normalizedThreadContext ? { threadContext: { ...normalizedThreadContext } } : {}),
    createCommandId,
    // A failed draft may already have created an unbound/different-project
    // thread. Changing its directory must create a new correctly bound thread.
    ...(sameThreadContext && existing?.threadId ? { threadId: existing.threadId } : {}),
    uploads,
    sendCommandId,
    clientMessageId: newStableId("msg"),
    updatedAt: Date.now(),
  };
  bucket.sends[key] = next;
  writeBucket(bucket);
  return {
    ...next,
    uploads: uploads.map((row) => ({ ...row })),
    ...(next.capabilityRefs
      ? { capabilityRefs: next.capabilityRefs.map((row) => ({ ...row })) }
      : {}),
    ...(next.runtime ? { runtime: { ...next.runtime } } : {}),
  };
}

export function updateSendIntent(
  draftKey: string,
  patch: Partial<Omit<SendIntent, "intentId" | "draftKey" | "sendCommandId" | "clientMessageId">>,
): SendIntent | null {
  const key = String(draftKey || "").trim();
  if (!key) return null;
  const bucket = readBucket();
  const existing = bucket.sends[key];
  if (!existing) return null;
  const next: SendIntent = {
    ...existing,
    ...patch,
    intentId: existing.intentId,
    draftKey: existing.draftKey,
    sendCommandId: existing.sendCommandId,
    clientMessageId: existing.clientMessageId,
    uploads: patch.uploads
      ? patch.uploads.map((row) => ({ ...row }))
      : existing.uploads.map((row) => ({ ...row })),
    updatedAt: Date.now(),
  };
  bucket.sends[key] = next;
  writeBucket(bucket);
  return {
    ...next,
    uploads: next.uploads.map((row) => ({ ...row })),
  };
}

export function markSendUploadSha(
  draftKey: string,
  attachmentId: string,
  sha256: string,
): SendIntent | null {
  const key = String(draftKey || "").trim();
  const id = String(attachmentId || "").trim();
  const hash = String(sha256 || "").trim();
  if (!key || !id || !hash) return null;
  const existing = readSendIntent(key);
  if (!existing) return null;
  const uploads = existing.uploads.map((row) => (
    row.attachmentId === id ? { ...row, sha256: hash } : { ...row }
  ));
  return updateSendIntent(key, { uploads, phase: "uploading" });
}

export function clearSendIntent(draftKey: string): void {
  const key = String(draftKey || "").trim();
  if (!key) return;
  const bucket = readBucket();
  delete bucket.sends[key];
  writeBucket(bucket);
}

export function ensureRetryIntent(threadId: string, turnId: string): RetryIntent {
  const key = retryIntentKey(threadId, turnId);
  if (!key.includes("::") || key.startsWith("::") || key.endsWith("::")) {
    throw new Error("retry intent requires thread and turn ids");
  }
  const bucket = readBucket();
  const existing = bucket.retries[key];
  if (existing && existing.phase !== "accepted") {
    return { ...existing };
  }
  const next: RetryIntent = {
    threadId: String(threadId || "").trim(),
    turnId: String(turnId || "").trim(),
    commandId: newStableId("cmd_retry"),
    phase: "sending",
    updatedAt: Date.now(),
  };
  bucket.retries[key] = next;
  writeBucket(bucket);
  return { ...next };
}

export function updateRetryIntent(
  threadId: string,
  turnId: string,
  patch: Partial<Omit<RetryIntent, "threadId" | "turnId" | "commandId">>,
): RetryIntent | null {
  const key = retryIntentKey(threadId, turnId);
  const bucket = readBucket();
  const existing = bucket.retries[key];
  if (!existing) return null;
  const next: RetryIntent = {
    ...existing,
    ...patch,
    threadId: existing.threadId,
    turnId: existing.turnId,
    commandId: existing.commandId,
    updatedAt: Date.now(),
  };
  bucket.retries[key] = next;
  writeBucket(bucket);
  return { ...next };
}

export function clearRetryIntent(threadId: string, turnId: string): void {
  const key = retryIntentKey(threadId, turnId);
  const bucket = readBucket();
  delete bucket.retries[key];
  writeBucket(bucket);
}

export type SendFailureKind = "connection" | "server" | "validation" | "unknown";

/** Structured Command API / receipt error preserved through the Chat send path (#191). */
export type ConversationCommandErrorInit = {
  message: string;
  code?: string;
  category?: string;
  retryable?: boolean;
  recoveryHint?: string;
  httpStatus?: number;
  correlationId?: string;
  deduplicated?: boolean;
  detail?: Record<string, unknown>;
};

export class ConversationCommandError extends Error {
  readonly code: string;
  readonly category: string;
  readonly retryable?: boolean;
  readonly recoveryHint: string;
  readonly httpStatus?: number;
  readonly correlationId?: string;
  readonly deduplicated?: boolean;
  readonly detail?: Record<string, unknown>;

  constructor(init: ConversationCommandErrorInit) {
    super(String(init.message || "").trim() || init.code || "命令执行失败");
    this.name = "ConversationCommandError";
    this.code = String(init.code || "").trim();
    this.category = String(init.category || "").trim().toLowerCase();
    if (typeof init.retryable === "boolean") this.retryable = init.retryable;
    this.recoveryHint = String(init.recoveryHint || "").trim();
    if (typeof init.httpStatus === "number" && Number.isFinite(init.httpStatus)) {
      this.httpStatus = init.httpStatus;
    }
    const correlationId = String(init.correlationId || "").trim();
    if (correlationId) this.correlationId = correlationId;
    if (init.deduplicated) this.deduplicated = true;
    if (init.detail && typeof init.detail === "object") this.detail = { ...init.detail };
  }
}

export type ReceiptErrorLike = {
  code?: string;
  message?: string;
  category?: string;
  recovery_hint?: string;
  retryable?: boolean;
  correlation_id?: string;
  detail?: Record<string, unknown>;
};

/** Build a structured error from a CommandReceipt.error (or top-level API error). */
export function conversationCommandErrorFrom(
  error: ReceiptErrorLike | null | undefined,
  fallbackMessage: string,
  options: { httpStatus?: number; deduplicated?: boolean } = {},
): ConversationCommandError {
  const code = String(error?.code || "").trim();
  const message = String(error?.message || "").trim()
    || code
    || String(fallbackMessage || "").trim()
    || "命令执行失败";
  return new ConversationCommandError({
    message,
    code,
    category: String(error?.category || "").trim(),
    retryable: error?.retryable,
    recoveryHint: String(error?.recovery_hint || "").trim(),
    httpStatus: options.httpStatus,
    correlationId: String(error?.correlation_id || "").trim(),
    deduplicated: Boolean(options.deduplicated),
    detail: error?.detail && typeof error.detail === "object"
      ? { ...error.detail }
      : undefined,
  });
}

function readStructured(exc: unknown): {
  code: string;
  category: string;
  retryable: boolean | undefined;
  recoveryHint: string;
  httpStatus: number | undefined;
  deduplicated: boolean;
  message: string;
} {
  const raw = exc instanceof Error ? exc.message : String(exc || "");
  const row = (exc && typeof exc === "object") ? exc as Record<string, unknown> : {};
  const httpStatus = typeof row.httpStatus === "number" && Number.isFinite(row.httpStatus)
    ? Number(row.httpStatus)
    : undefined;
  const retryable = typeof row.retryable === "boolean" ? row.retryable : undefined;
  return {
    code: String(row.code || "").trim(),
    category: String(row.category || "").trim().toLowerCase(),
    retryable,
    recoveryHint: String(row.recoveryHint || row.recovery_hint || "").trim(),
    httpStatus,
    deduplicated: Boolean(row.deduplicated),
    message: raw,
  };
}

/** Stable Chinese diagnostics for common Chat validation codes (#191). */
const SEND_ERROR_DIAGNOSTICS: Record<string, { message: string; fix?: string }> = {
  "conversation.turn.text_required": {
    message: "发送消息需要正文、附件或上下文引用",
    fix: "请补充正文，或添加附件/引用后再发送",
  },
  "conversation.queue.text_required": {
    message: "队列消息需要正文",
    fix: "请编辑队列项并填写正文后再试",
  },
  "conversation.thread.archived": {
    message: "已归档的对话不能发送消息，请先取消归档",
  },
  "conversation.composer.reference_invalid": {
    message: "上下文引用无效",
    fix: "请移除失效引用或按当前 Agent 重新选择后再发送",
  },
  "conversation.command.local_only": {
    message: "该输入是 Muteki 本地界面命令，不会发送给 Agent",
    fix: "请改用普通正文，或执行对应的本地操作",
  },
  "conversation.command.not_available": {
    message: "当前 Runtime Session 不支持该命令",
    fix: "请更换命令，或切换到已公布该能力的 Agent",
  },
  "command.idempotency_conflict": {
    message: "发送内容已变更，但沿用了旧的命令身份",
    fix: "请修改输入后重新发送，或关闭提示后再次提交以使用新的发送身份",
  },
};

function diagnosticFor(exc: unknown): { message: string; fix: string } {
  const info = readStructured(exc);
  const mapped = info.code ? SEND_ERROR_DIAGNOSTICS[info.code] : undefined;
  const serverMessage = info.message.trim();
  const serverIsChinese = /[\u4e00-\u9fff]/.test(serverMessage);
  const hint = info.recoveryHint;
  const hintIsChinese = /[\u4e00-\u9fff]/.test(hint);

  let message = "";
  if (mapped?.message) {
    // Prefer Chinese map; keep richer Chinese server copy when present.
    message = serverIsChinese && serverMessage && serverMessage !== mapped.message
      ? serverMessage
      : mapped.message;
  } else if (serverIsChinese) {
    message = serverMessage;
  } else if (info.code) {
    message = `发送未通过校验（${info.code}）`;
  } else if (serverMessage) {
    message = `发送未通过校验：${serverMessage}`;
  } else {
    message = "发送未通过校验";
  }

  let fix = "";
  if (mapped?.fix) fix = mapped.fix;
  else if (hintIsChinese) fix = hint;
  else if (hint) fix = hint;
  return { message, fix };
}

/** Classify a thrown send/create/retry failure for user-facing copy. */
export function classifySendFailure(exc: unknown): SendFailureKind {
  const info = readStructured(exc);
  const raw = info.message;
  const lower = raw.toLowerCase();
  const httpStatus = info.httpStatus;

  if (info.category === "validation") return "validation";
  if (
    info.category === "permission"
    || info.category === "not_found"
    || info.category === "state"
    || info.category === "conflict"
  ) {
    // User-correctable / non-transient command outcomes — no identical retry.
    return "validation";
  }

  // Next.js api proxy returns HTTP 502 when upstream is unreachable ("site down").
  if (
    !raw.trim()
    || raw === "Failed to fetch"
    || lower.includes("failed to fetch")
    || lower.includes("networkerror")
    || lower.includes("network request failed")
    || lower.includes("load failed")
    || lower.includes("err_connection")
    || lower.includes("econnrefused")
    || lower.includes("api proxy failed")
    || /HTTP\s*network/i.test(raw)
    || httpStatus === 0
    || httpStatus === 502
  ) {
    return "connection";
  }
  if (typeof httpStatus === "number") {
    if (httpStatus === 502) return "connection";
    if (httpStatus >= 500) return "server";
    if (httpStatus === 422 || httpStatus === 400 || httpStatus === 403 || httpStatus === 404 || httpStatus === 409) {
      return "validation";
    }
    if (httpStatus >= 400 && httpStatus < 500) return "validation";
  }
  const httpMatch = raw.match(/HTTP\s*(\d{3})/i);
  if (httpMatch) {
    const code = Number(httpMatch[1]);
    // 502 = proxy/upstream down → disconnect strip, not generic 5xx copy.
    if (code === 502) return "connection";
    if (code >= 500) return "server";
    if (code >= 400) return "validation";
  }
  if (info.code && (
    info.code.startsWith("conversation.")
    || info.code.startsWith("command.")
    || info.code.includes("text_required")
    || info.code.includes("validation")
  )) {
    return "validation";
  }
  // Local UI guards and Chinese API details are validation/user-correctable.
  if (/请选择|不可用|校验|无效|不能|无法|已过期|未通过|已归档/.test(raw)) {
    return "validation";
  }
  return "unknown";
}

/**
 * Whether the send-error banner should offer identical "重试发送".
 * Permanent validation / retryable=false / deduplicated failed receipts → no.
 */
export function shouldOfferSendRetry(exc: unknown): boolean {
  const info = readStructured(exc);
  if (info.retryable === false) return false;
  if (info.deduplicated && info.retryable !== true) return false;
  const kind = classifySendFailure(exc);
  if (kind === "validation") return false;
  if (kind === "connection" || kind === "server") return true;
  return info.retryable === true;
}

/**
 * Single Chinese failure line for Chat send/create/retry.
 * Connection / 5xx keep the draft and invite retry; validation shows reason + fix,
 * without inviting identical retry of a terminal failed command (#191).
 */
export function formatSendFailureMessage(
  exc: unknown,
  options: { keepDraft?: boolean } = {},
): string {
  const keepDraft = options.keepDraft !== false;
  const retryHint = shouldOfferSendRetry(exc) ? "，可重试" : "";
  const draftHint = `${keepDraft ? "，草稿已保留" : ""}${retryHint}`;
  const kind = classifySendFailure(exc);
  const info = readStructured(exc);
  const raw = info.message;
  switch (kind) {
    case "connection":
      return `连接已断开${draftHint}`;
    case "server": {
      const status = info.httpStatus;
      const m = raw.match(/HTTP\s*(\d{3})/i);
      const code = status && status >= 500 ? String(status) : (m ? m[1] : "");
      return code ? `服务暂时异常（${code}）${draftHint}` : `服务暂时异常${draftHint}`;
    }
    case "validation": {
      const { message, fix } = diagnosticFor(exc);
      if (fix) {
        const draftKeep = keepDraft ? "草稿已保留。" : "";
        return `${message}。${draftKeep}${fix}`.replace(/。。/g, "。");
      }
      // Pre-formatted Chinese UI/API details stay intact (no identical-retry invite).
      if (/[\u4e00-\u9fff]/.test(message)) return message;
      if (keepDraft) return `${message}（草稿已保留，请修正后重新发送）`;
      return message;
    }
    default:
      if (/[\u4e00-\u9fff]/.test(raw)) return raw;
      return `发送失败${draftHint}`;
  }
}

export function phaseNotice(phase: SendIntentPhase | RetryIntent["phase"]): string {
  switch (phase) {
    case "creating_thread":
      return "正在创建会话…";
    case "uploading":
      return "正在上传附件…";
    case "sending":
      return "正在发送…";
    case "accepted":
      return "消息已受理（Agent 仍可能在执行中）";
    case "unknown":
      return "网络结果未知，正在核对回执…";
    case "failed":
      return "发送失败，草稿已保留，可重试";
    default: {
      const _exhaustive: never = phase;
      return String(_exhaustive);
    }
  }
}

/** Test helper: inject storage and reset memory. */
export function __resetSendIntentStoreForTests(storage?: StorageLike | null): void {
  memoryBucket = emptyBucket();
  memoryDirty = false;
  storageOverride = storage === undefined ? null : storage;
}
