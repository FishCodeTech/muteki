import { conversationStorageTransaction, conversationStorageVersion } from "./conversationStorageTransaction";
import { classifyCommandReceipt } from "./commandReceiptState";
import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope, subscribeBeforeConversationStorageScope } from "./conversationStorageScope";

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
  artifactId?: string;
};

export type SendDelivery = { expectedTurnId: string };

export type SendIntent = {
  delivery?: SendDelivery;
  intentId: string;
  draftKey: string;
  phase: SendIntentPhase;
  text: string;
  capabilityRefs?: Array<Record<string, unknown>>;
  runtime?: Record<string, unknown>;
  runtimeInvocation?: Record<string, unknown>;
  threadContext?: Record<string, unknown>;
  createCommandId?: string;
  bindCommandId?: string;
  boundWorkspaceId?: string;
  boundBranch?: string;
  pendingCommandId?: string;
  threadId?: string;
  uploads: SendUploadIntent[];
  sendCommandId: string;
  previousCommandId?: string;
  clientMessageId: string;
  lastReceiptState?: string;
  pendingPhase?: "creating_thread" | "sending";
  lastError?: string;
  updatedAt: number;
};

export type RetryIntent = {
  threadId: string;
  turnId: string;
  commandId: string;
  previousCommandId?: string;
  phase: "prepared" | "sending" | "accepted" | "unknown" | "failed";
  lastReceiptState?: string;
  lastError?: string;
  request?: { fileMode: "keep_files" | "restore_files"; interactionMode: string };
  updatedAt: number;
};

type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

type IntentBucket = {
  sends: Record<string, SendIntent>;
  retries: Record<string, RetryIntent>;
};

let memoryBucket: IntentBucket = { sends: {}, retries: {} };
let memoryDirty = false;
type IntentWrites = { sends: Map<string, { base?: string; value?: SendIntent }>; retries: Map<string, { base?: string; value?: RetryIntent }> };
const emptyWrites = (): IntentWrites => ({ sends: new Map(), retries: new Map() });
let intentWrites = emptyWrites();
const storedIntentVersions = new WeakMap<SendIntent | RetryIntent, string>();
function storedIntentVersion(intent: SendIntent | RetryIntent | undefined): string | undefined {
  return intent ? storedIntentVersions.get(intent) ?? conversationStorageVersion(intent) : undefined;
}
function commitIntentWrites(storage: StorageLike, scope: string, local: IntentBucket, writes: IntentWrites): IntentBucket {
  const raw = JSON.parse(storage.getItem(conversationStorageKey(SEND_INTENT_STORAGE_KEY, scope)) || "{}");
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new Error("send-intent.storage_invalid: 请求恢复记录格式无效");
  const merged: IntentBucket = { sends: { ...(raw.sends || {}) }, retries: { ...(raw.retries || {}) } };
  const conflicts: string[] = [];
  const committed: Array<["sends" | "retries", string]> = [];
  for (const kind of ["sends", "retries"] as const) for (const [key, change] of writes[kind]) {
    if (conversationStorageVersion(merged[kind][key]) !== change.base && conversationStorageVersion(merged[kind][key]) !== conversationStorageVersion(change.value)) { conflicts.push(key); continue; }
    if (change.value) (merged[kind] as Record<string, SendIntent | RetryIntent>)[key] = change.value;
    else delete merged[kind][key];
    committed.push([kind, key]);
  }
  storage.setItem(conversationStorageKey(SEND_INTENT_STORAGE_KEY, scope), JSON.stringify(merged));
  for (const [kind, key] of committed) writes[kind].delete(key);
  for (const kind of ["sends", "retries"] as const) for (const [key, change] of writes[kind]) {
    if (change.value) (merged[kind] as Record<string, SendIntent | RetryIntent>)[key] = change.value;
    else delete merged[kind][key];
  }
  local.sends = merged.sends; local.retries = merged.retries;
  if (conflicts.length) throw new Error(`send-intent.storage_conflict: ${conflicts.join(", ")} 已由其他窗口更新，原请求身份仍保留，尚未投递`);
  return merged;
}

function getStorage(): StorageLike | null {
  if (typeof window === "undefined" || !conversationStorageScope()) return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function currentIntentTransaction<T>(run: () => T): Promise<T> {
  const ownerScope = conversationStorageScope();
  return conversationStorageTransaction(ownerScope, () => {
    if (!ownerScope || ownerScope !== conversationStorageScope()) throw new Error("conversation.send.scope_changed: 请求发起时的工作台身份已改变");
    return run();
  });
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
    const raw = storage.getItem(conversationStorageKey(SEND_INTENT_STORAGE_KEY));
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
      if (intent) {
        storedIntentVersions.set(intent, conversationStorageVersion(value)!);
        sends[key] = intent;
      }
    }
    const retries: Record<string, RetryIntent> = {};
    for (const [key, value] of Object.entries(retriesRaw)) {
      const intent = normalizeRetryIntent(value);
      if (intent) {
        storedIntentVersions.set(intent, conversationStorageVersion(value)!);
        retries[key] = intent;
      }
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
  for (const kind of ["sends", "retries"] as const) for (const key of new Set([...Object.keys(memoryBucket[kind]), ...Object.keys(bucket[kind])])) {
    if (conversationStorageVersion(memoryBucket[kind][key]) === conversationStorageVersion(bucket[kind][key])) continue;
    const changes = intentWrites[kind] as Map<string, { base?: string; value?: SendIntent | RetryIntent }>;
    changes.set(key, { base: changes.has(key) ? changes.get(key)!.base : storedIntentVersion(memoryBucket[kind][key]), value: bucket[kind][key] });
  }
  memoryBucket = {
    sends: { ...bucket.sends },
    retries: { ...bucket.retries },
  };
  const storage = getStorage();
  if (!storage) {
    memoryDirty = true;
    throw new Error("send-intent.storage_unavailable: 请求身份仅保留在当前窗口，尚未投递");
  }
  try {
    memoryBucket = commitIntentWrites(storage, conversationStorageScope(), memoryBucket, intentWrites);
    memoryDirty = false;
  } catch (error) {
    memoryDirty = true;
    throw error;
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
    const artifactId = String(upload.artifactId || "").trim();
    uploads.push({
      attachmentId,
      commandId,
      ...(artifactId ? { artifactId } : {}),
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
    ...(row.delivery && typeof row.delivery === "object" && typeof (row.delivery as SendDelivery).expectedTurnId === "string"
      ? { delivery: { expectedTurnId: (row.delivery as SendDelivery).expectedTurnId } } : {}),
    ...(capabilityRefs?.length ? { capabilityRefs } : {}),
    ...(runtime ? { runtime } : {}),
    ...(normalizeRuntimeRecord(row.runtimeInvocation as Record<string, unknown>) ? { runtimeInvocation: normalizeRuntimeRecord(row.runtimeInvocation as Record<string, unknown>) } : {}),
    ...(threadContext ? { threadContext } : {}),
    ...(createCommandId ? { createCommandId } : {}),
    ...(typeof row.bindCommandId === "string" ? { bindCommandId: row.bindCommandId } : {}),
    ...(typeof row.boundWorkspaceId === "string" ? { boundWorkspaceId: row.boundWorkspaceId } : {}),
    ...(typeof row.boundBranch === "string" ? { boundBranch: row.boundBranch } : {}),
    ...(typeof row.pendingCommandId === "string" ? { pendingCommandId: row.pendingCommandId } : {}),
    ...(threadId ? { threadId } : {}),
    uploads,
    sendCommandId,
    ...(typeof row.previousCommandId === "string" && row.previousCommandId.trim() ? { previousCommandId: row.previousCommandId.trim() } : {}),
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
  const previousCommandId = String(row.previousCommandId || "").trim();
  const phase = String(row.phase || "").trim();
  if (!threadId || !turnId || !commandId) return null;
  if (
    phase !== "prepared"
    && phase !== "sending"
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
    ...(previousCommandId ? { previousCommandId } : {}),
    phase,
    ...(lastReceiptState ? { lastReceiptState } : {}),
    ...(lastError ? { lastError } : {}),
    ...(row.request && typeof row.request === "object"
      && ["keep_files", "restore_files"].includes(String((row.request as Record<string, unknown>).fileMode))
      ? { request: { fileMode: (row.request as RetryIntent["request"])!.fileMode, interactionMode: String((row.request as Record<string, unknown>).interactionMode || "default") } } : {}),
    updatedAt,
  };
}

export function retryIntentKey(threadId: string, turnId: string): string {
  return `${String(threadId || "").trim()}::${String(turnId || "").trim()}`;
}

export function readSendIntent(draftKey: string): SendIntent | null {
  const key = String(draftKey || "").trim();
  if (!key) return null;
  return readBucket().sends[key] || null;
}


export function fingerprintAttachmentIds(attachmentIds: string[] = []): string {
  return JSON.stringify(
    (attachmentIds || [])
      .map((id) => String(id || "").trim())
      .filter(Boolean),
  );
}

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

export function sendIntentMatchesPayload(
  intent: SendIntent,
  text: string,
  attachmentIds: string[] = [],
  capabilityRefs: Array<Record<string, unknown>> = [],
  runtime: Record<string, unknown> = {},
  threadContext: Record<string, unknown> = {},
  runtimeInvocation?: Record<string, unknown>,
  delivery?: SendDelivery,
): boolean {
  return intent.text === String(text || "")
    && fingerprintCapabilityRefs(intent.capabilityRefs) === fingerprintCapabilityRefs(capabilityRefs)
    && fingerprintAttachmentIds(intent.uploads.map((row) => row.attachmentId)) === fingerprintAttachmentIds(attachmentIds)
    && fingerprintRuntime(intent.runtime) === fingerprintRuntime(runtime)
    && fingerprintRuntime(intent.threadContext) === fingerprintRuntime(threadContext)
    && fingerprintRuntime(intent.runtimeInvocation) === fingerprintRuntime(runtimeInvocation)
    && intent.delivery?.expectedTurnId === delivery?.expectedTurnId;
}

type RecoverableSendReceipt = {
  command_id?: string;
  state?: string;
  error?: unknown;
  aggregate?: { id?: string };
  output?: Record<string, unknown>;
};

export async function prepareSendIntent(
  draftKey: string,
  text: string,
  attachmentIds: string[],
  capabilityRefs: Array<Record<string, unknown>>,
  runtime: Record<string, unknown>,
  recover: (commandId: string) => Promise<RecoverableSendReceipt | null>,
  threadContext: Record<string, unknown> = {},
  runtimeInvocation?: Record<string, unknown>,
  delivery?: SendDelivery,
): Promise<SendIntent> {
  const ownerScope = conversationStorageScope();
  const existing = readSendIntent(draftKey);
  if (existing && (existing.phase === "unknown" || existing.pendingCommandId || !sendIntentMatchesPayload(existing, text, attachmentIds, capabilityRefs, runtime, threadContext, runtimeInvocation, delivery))) {
    const pending = existing.phase === "unknown" ? existing.pendingPhase : existing.phase;
    if (pending === "sending" || pending === "creating_thread") {
      const commandId = existing.pendingCommandId || (pending === "creating_thread" ? existing.createCommandId : existing.sendCommandId);
      const receipt = commandId ? await recover(commandId) : null;
      if (ownerScope !== conversationStorageScope()) throw new Error("conversation.send.scope_changed");
      if (!receipt) throw new ConversationCommandError({ code: "conversation.send.pending", category: "state", retryable: false, message: "原请求尚无可确认回执，草稿与原命令身份已保留；可核对原请求或显式归档待核对记录后开始新的发送", detail: { command_id: commandId, outcome_unknown: true } });
      if (receipt.command_id && receipt.command_id !== commandId) throw new Error("conversation.send.receipt_owner_mismatch: 回执与原命令身份不匹配");
      const rejected = classifyCommandReceipt(receipt) === "failed";
      if (!rejected && classifyCommandReceipt(receipt) !== "completed") throw new ConversationCommandError({ code: "conversation.send.pending", category: "state", retryable: false, message: `原请求仍为 ${receipt.state || "unknown"}，不能把计划中的资源当作已保存结果`, detail: { command_id: commandId, receipt_state: receipt.state, outcome_unknown: true } });
      const binding = pending === "creating_thread" && Boolean(existing.bindCommandId && commandId === existing.bindCommandId);
      const resourceId = !rejected ? (binding ? String(receipt.output?.workspace_id || receipt.aggregate?.id || "") : receipt.aggregate?.id) : undefined;
      if (pending === "creating_thread" && !rejected && !resourceId) throw new ConversationCommandError({ code: "conversation.send.invalid_receipt", category: "protocol", retryable: false, message: "完成回执缺少已保存资源标识，草稿与原命令身份已保留", detail: { command_id: commandId, outcome_unknown: true } });
      await transitionSendIntent(draftKey, { type: "recovered", commandId: commandId!, state: receipt.state, failed: rejected, resourceId, binding }, existing.intentId);
      if (!rejected && pending === "sending") return readSendIntent(draftKey)!;
    }
  }
  return ensureSendIntent(draftKey, text, attachmentIds, capabilityRefs, runtime, threadContext, runtimeInvocation, delivery);
}

export async function ensureSendIntent(
  draftKey: string,
  text: string,
  attachmentIds: string[] = [],
  capabilityRefs: Array<Record<string, unknown>> = [],
  runtime: Record<string, unknown> = {},
  threadContext: Record<string, unknown> = {},
  runtimeInvocation?: Record<string, unknown>,
  delivery?: SendDelivery,
): Promise<SendIntent> {
  return currentIntentTransaction(() => {
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
  const reusable = Boolean(
    existing
    && existing.text === normalizedText
    && existingRefsFingerprint === refsFingerprint
    && existingAttachmentFingerprint === attachmentFingerprint
    && existingRuntimeFingerprint === runtimeFingerprint
    && sameThreadContext
    && fingerprintRuntime(existing.runtimeInvocation) === fingerprintRuntime(runtimeInvocation)
    && existing.delivery?.expectedTurnId === delivery?.expectedTurnId
    && existing.phase !== "accepted"
    && existing.phase !== "failed"
    && !["failed", "rejected", "conflict", "cancelled"].includes(existing.lastReceiptState || ""),
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
      runtimeInvocation: normalizeRuntimeRecord(runtimeInvocation),
      delivery,
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

  const knownUploads = new Map((existing?.uploads || []).map((row) => [row.attachmentId, row]));
  const uploads: SendUploadIntent[] = attachmentIds.map((attachmentId) => {
    const prior = knownUploads.get(attachmentId);
    return {
      attachmentId,
      commandId: prior?.artifactId ? prior.commandId : newStableId("cmd_upload"),
      ...(prior?.artifactId ? { artifactId: prior.artifactId } : {}),
    };
  });
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
    runtimeInvocation: normalizeRuntimeRecord(runtimeInvocation),
    delivery,
    createCommandId,
    ...(sameThreadContext && existing?.threadId ? { threadId: existing.threadId } : {}),
    ...(sameThreadContext && existing?.boundWorkspaceId ? { boundWorkspaceId: existing.boundWorkspaceId, boundBranch: existing.boundBranch } : {}),
    uploads,
    sendCommandId,
    ...(existing?.sendCommandId ? { previousCommandId: existing.sendCommandId } : {}),
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
  });
}

export type SendIntentTransition =
  | { type: "creating" }
  | { type: "binding_started"; commandId: string; branch?: string }
  | { type: "binding_confirmed"; workspaceId: string }
  | { type: "create_started" }
  | { type: "thread_confirmed"; threadId: string }
  | { type: "uploading" }
  | { type: "upload_confirmed"; attachmentId: string; artifactId: string }
  | { type: "send_started" }
  | { type: "accepted"; state: string }
  | { type: "failure"; uncertain: boolean; error?: string; state?: string }
  | { type: "recovered"; commandId: string; state?: string; failed: boolean; resourceId?: string; binding: boolean };

export async function transitionSendIntent(draftKey: string, event: SendIntentTransition, expectedIntentId: string): Promise<SendIntent | null> {
  return currentIntentTransaction(() => {
  const key = String(draftKey || "").trim();
  const bucket = readBucket();
  const existing = bucket.sends[key];
  if (!existing) return null;
  if (existing.intentId !== expectedIntentId) throw new Error("conversation.send.intent_owner_changed: 原发送身份已被另一窗口替换，迟到结果未写入当前请求");
  if (existing.phase === "accepted" || existing.phase === "failed") {
    if (event.type === "failure") return { ...existing, uploads: existing.uploads.map((row) => ({ ...row })) };
    throw new Error("conversation.send.intent_terminal: 原请求已有明确终态，请核对当前请求");
  }
  if (existing.phase === "unknown" && event.type !== "recovered" && event.type !== "failure") {
    throw new Error("conversation.send.recovery_required: 原命令结果未知，必须先核对回执");
  }
  let patch: Partial<SendIntent>;
  switch (event.type) {
    case "creating": patch = { phase: "creating_thread" }; break;
    case "binding_started": patch = { phase: "creating_thread", bindCommandId: event.commandId, boundBranch: event.branch ?? existing.boundBranch, pendingCommandId: event.commandId, pendingPhase: "creating_thread" }; break;
    case "binding_confirmed":
      if (!existing.bindCommandId || existing.pendingCommandId !== existing.bindCommandId) throw new Error("conversation.send.binding_owner_mismatch");
      patch = { boundWorkspaceId: event.workspaceId, pendingCommandId: undefined, pendingPhase: undefined }; break;
    case "create_started":
      if (!existing.createCommandId) throw new Error("conversation.send.create_identity_missing");
      patch = { phase: "creating_thread", pendingCommandId: existing.createCommandId, pendingPhase: "creating_thread" }; break;
    case "thread_confirmed": patch = { phase: "uploading", threadId: event.threadId, pendingCommandId: undefined, pendingPhase: undefined }; break;
    case "uploading": patch = { phase: "uploading" }; break;
    case "upload_confirmed":
      if (!existing.uploads.some((row) => row.attachmentId === event.attachmentId)) throw new Error("conversation.send.upload_owner_mismatch");
      patch = { phase: "uploading", uploads: existing.uploads.map((row) => row.attachmentId === event.attachmentId ? { ...row, artifactId: event.artifactId } : row) }; break;
    case "send_started":
      if (!existing.threadId) throw new Error("conversation.send.thread_missing");
      patch = { phase: "sending", pendingCommandId: existing.sendCommandId, pendingPhase: "sending" }; break;
    case "accepted":
      if (existing.pendingCommandId !== existing.sendCommandId) throw new Error("conversation.send.receipt_owner_mismatch");
      if (classifyCommandReceipt({ state: event.state }) === "unknown" || classifyCommandReceipt({ state: event.state }) === "failed") throw new Error("conversation.send.invalid_acceptance");
      patch = { phase: "accepted", pendingCommandId: undefined, pendingPhase: undefined, lastReceiptState: event.state, lastError: "" }; break;
    case "failure": {
      const uncertain = event.uncertain && Boolean(existing.pendingCommandId);
      patch = { phase: uncertain ? "unknown" : "failed", pendingPhase: uncertain ? existing.pendingPhase : undefined, pendingCommandId: uncertain ? existing.pendingCommandId : undefined, lastError: event.error, lastReceiptState: event.state ?? existing.lastReceiptState }; break;
    }
    case "recovered": {
      const commandId = existing.pendingCommandId || (existing.pendingPhase === "creating_thread" ? existing.createCommandId : existing.sendCommandId);
      if (commandId !== event.commandId) throw new Error("conversation.send.receipt_owner_mismatch");
      patch = { phase: event.failed ? "failed" : event.binding ? "creating_thread" : existing.pendingPhase === "creating_thread" || existing.phase === "creating_thread" ? "uploading" : "accepted", pendingCommandId: undefined, pendingPhase: undefined, lastReceiptState: event.state, ...(event.resourceId ? event.binding ? { boundWorkspaceId: event.resourceId } : existing.pendingPhase === "creating_thread" || existing.phase === "creating_thread" ? { threadId: event.resourceId } : {} : {}) }; break;
    }
  }
  const next = { ...existing, ...patch, updatedAt: Date.now() };
  bucket.sends[key] = next;
  writeBucket(bucket);
  return { ...next, uploads: next.uploads.map((row) => ({ ...row })) };
  });
}

export async function markSendUploadRef(
  draftKey: string,
  attachmentId: string,
  artifactRef: string,
  expectedIntentId: string,
): Promise<SendIntent | null> {
  const key = String(draftKey || "").trim();
  const id = String(attachmentId || "").trim();
  const reference = String(artifactRef || "").trim();
  if (!key || !id || !reference) return null;
  const existing = readSendIntent(key);
  if (!existing) return null;
  return transitionSendIntent(key, { type: "upload_confirmed", attachmentId: id, artifactId: reference }, expectedIntentId);
}

export async function clearSendIntent(draftKey: string, expectedIntentId?: string): Promise<void> {
  return currentIntentTransaction(() => {
  const key = String(draftKey || "").trim();
  if (!key) return;
  const bucket = readBucket();
  if (expectedIntentId && bucket.sends[key]?.intentId !== expectedIntentId) throw new Error("conversation.send.intent_owner_changed");
  delete bucket.sends[key];
  writeBucket(bucket);
  });
}

export async function ensureRetryIntent(threadId: string, turnId: string): Promise<RetryIntent> {
  return currentIntentTransaction(() => {
  const key = retryIntentKey(threadId, turnId);
  if (!key.includes("::") || key.startsWith("::") || key.endsWith("::")) {
    throw new Error("retry intent requires thread and turn ids");
  }
  const bucket = readBucket();
  const existing = bucket.retries[key];
  if (existing && existing.phase !== "accepted" && existing.phase !== "failed") {
    return { ...existing };
  }
  const next: RetryIntent = {
    threadId: String(threadId || "").trim(),
    turnId: String(turnId || "").trim(),
    commandId: newStableId("cmd_retry"),
    ...(existing?.commandId ? { previousCommandId: existing.commandId } : {}),
    phase: "prepared",
    updatedAt: Date.now(),
  };
  bucket.retries[key] = next;
  writeBucket(bucket);
  return { ...next };
  });
}

export function readRetryIntent(threadId: string, turnId: string): RetryIntent | null {
  const intent = readBucket().retries[retryIntentKey(threadId, turnId)];
  return intent ? { ...intent, ...(intent.request ? { request: { ...intent.request } } : {}) } : null;
}

export async function updateRetryIntent(
  threadId: string,
  turnId: string,
  patch: Partial<Omit<RetryIntent, "threadId" | "turnId" | "commandId">>,
  expectedCommandId?: string,
): Promise<RetryIntent | null> {
  return currentIntentTransaction(() => {
  const key = retryIntentKey(threadId, turnId);
  const bucket = readBucket();
  const existing = bucket.retries[key];
  if (!existing) return null;
  if (expectedCommandId && existing.commandId !== expectedCommandId) throw new Error("conversation.retry.command_owner_changed");
  if (existing.phase === "unknown" && patch.phase === "sending") throw new Error("conversation.retry.recovery_required: 原重试命令结果未知，请先核对回执");
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
  });
}

export async function clearRetryIntent(threadId: string, turnId: string, expectedCommandId?: string): Promise<void> {
  return currentIntentTransaction(() => {
  const key = retryIntentKey(threadId, turnId);
  const bucket = readBucket();
  if (expectedCommandId && bucket.retries[key]?.commandId !== expectedCommandId) throw new Error("conversation.retry.command_owner_changed");
  delete bucket.retries[key];
  writeBucket(bucket);
  });
}

export type SendFailureKind = "connection" | "server" | "validation" | "unknown";

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
  detail?: Record<string, unknown>;
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
    detail: row.detail && typeof row.detail === "object" ? row.detail as Record<string, unknown> : undefined,
  };
}

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

export function classifySendFailure(exc: unknown): SendFailureKind {
  const info = readStructured(exc);
  const raw = info.message;
  const lower = raw.toLowerCase();
  const httpStatus = info.httpStatus;

  if (info.detail?.outcome_unknown === true || info.detail?.delivery_unknown === true) return "unknown";
  if (exc && typeof exc === "object" && ["AbortError", "TimeoutError"].includes(String((exc as { name?: string }).name))) return "connection";
  if (info.category === "validation") return "validation";
  if (
    info.category === "permission"
    || info.category === "not_found"
    || info.category === "state"
    || info.category === "conflict"
  ) {
    return "validation";
  }

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
  if (/请选择|不可用|校验|无效|不能|无法|已过期|未通过|已归档/.test(raw)) {
    return "validation";
  }
  return "unknown";
}

export function shouldOfferSendRetry(exc: unknown): boolean {
  const info = readStructured(exc);
  if (info.retryable === false) return false;
  if (info.deduplicated && info.retryable !== true) return false;
  const kind = classifySendFailure(exc);
  if (kind === "validation") return false;
  if (kind === "connection" || kind === "server") return true;
  return info.retryable === true;
}

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
    case "prepared":
      return "重试已准备，等待确认…";
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

const scopedIntents = new Map<string, { bucket: IntentBucket; dirty: boolean; writes: IntentWrites }>();
subscribeBeforeConversationStorageScope(() => {
  if (memoryDirty) void flushSendIntentStore();
  scopedIntents.set(conversationStorageScope(), { bucket: memoryBucket, dirty: memoryDirty, writes: intentWrites });
});
subscribeConversationStorageScope(() => {
  const old = scopedIntents.get(conversationStorageScope()); memoryBucket = old?.bucket || emptyBucket(); memoryDirty = old?.dirty || false; intentWrites = old?.writes || emptyWrites();
});

export async function flushSendIntentStore(): Promise<{ persisted: boolean; error?: string }> {
  const states = new Map(scopedIntents);
  states.set(conversationStorageScope(), { bucket: memoryBucket, dirty: memoryDirty, writes: intentWrites });
  const errors: string[] = [];
  for (const [scope, state] of states) {
    if (!state.dirty) continue;
    try { await conversationStorageTransaction(scope, () => {
      const storage = typeof window !== "undefined" && scope ? window.localStorage : null;
      if (!storage) throw new Error("send-intent.storage_unavailable: 请求身份仅保留在当前窗口");
      state.bucket = commitIntentWrites(storage, scope, state.bucket, state.writes);
      state.dirty = false;
      if (scope === conversationStorageScope()) { memoryBucket = state.bucket; memoryDirty = false; }
    }); } catch (error) { errors.push(error instanceof Error ? `${error.name}: ${error.message}` : String(error)); }
  }
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}
