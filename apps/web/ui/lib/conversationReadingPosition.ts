/**
 * Per-thread conversation reading position (C08).
 *
 * C07 owns `?message=` deep links; restore runs only when that param is absent.
 * Search / cite jumps push a jump-back stack so the reader can return.
 */

export const READING_POSITION_STORAGE_KEY = "muteki:conversation-reading-position:v1";
const PERSIST_DEBOUNCE_MS = 200;
const MAX_THREADS = 80;
const MAX_JUMP_BACK = 20;

export type TurnExpandState = Record<string, { processOpen?: boolean }>;

export type ConversationReadingPosition = {
  threadId: string;
  messageId: string;
  offsetPx: number;
  stickToBottom: boolean;
  expandByTurn: TurnExpandState;
  updatedAt: number;
};

export type JumpBackEntry = {
  threadId: string;
  messageId: string;
  offsetPx: number;
};

export type VisibleAnchorBox = {
  messageId: string;
  top: number;
  bottom: number;
};

type Bucket = {
  byThread: Record<string, ConversationReadingPosition>;
  jumpBack: JumpBackEntry[];
};

type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

let memory: Bucket = { byThread: {}, jumpBack: [] };
let memoryDirty = false;
let persistTimer: ReturnType<typeof setTimeout> | null = null;
let storageOverride: StorageLike | null = null;
const pendingExpand: Record<string, TurnExpandState> = {};
const jumpBackListeners = new Set<() => void>();

function emptyBucket(): Bucket {
  return { byThread: {}, jumpBack: [] };
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

function normalizeExpand(value: unknown): TurnExpandState {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  const next: TurnExpandState = {};
  for (const [turnId, row] of Object.entries(value as Record<string, unknown>)) {
    const id = String(turnId || "").trim();
    if (!id) continue;
    if (!row || typeof row !== "object" || Array.isArray(row)) continue;
    const processOpen = (row as { processOpen?: unknown }).processOpen;
    if (typeof processOpen === "boolean") next[id] = { processOpen };
  }
  return next;
}

function normalizePosition(value: unknown, fallbackThreadId = ""): ConversationReadingPosition | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const row = value as Record<string, unknown>;
  const threadId = String(row.threadId || fallbackThreadId || "").trim();
  const messageId = String(row.messageId || "").trim();
  if (!threadId || !messageId) return null;
  const offsetPx = Number(row.offsetPx);
  const updatedAt = Number(row.updatedAt);
  return {
    threadId,
    messageId,
    offsetPx: Number.isFinite(offsetPx) ? offsetPx : 0,
    stickToBottom: Boolean(row.stickToBottom),
    expandByTurn: normalizeExpand(row.expandByTurn),
    updatedAt: Number.isFinite(updatedAt) ? updatedAt : 0,
  };
}

function normalizeJumpBack(value: unknown): JumpBackEntry | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const row = value as Record<string, unknown>;
  const threadId = String(row.threadId || "").trim();
  const messageId = String(row.messageId || "").trim();
  if (!threadId || !messageId) return null;
  const offsetPx = Number(row.offsetPx);
  return {
    threadId,
    messageId,
    offsetPx: Number.isFinite(offsetPx) ? offsetPx : 0,
  };
}

function readBucketFromStorage(): Bucket {
  if (memoryDirty) {
    return {
      byThread: { ...memory.byThread },
      jumpBack: [...memory.jumpBack],
    };
  }
  const storage = getStorage();
  if (!storage) {
    return {
      byThread: { ...memory.byThread },
      jumpBack: [...memory.jumpBack],
    };
  }
  try {
    const raw = storage.getItem(READING_POSITION_STORAGE_KEY);
    if (!raw) {
      memory = emptyBucket();
      return emptyBucket();
    }
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      memory = emptyBucket();
      return emptyBucket();
    }
    const row = parsed as Record<string, unknown>;
    const byThreadRaw = (
      row.byThread && typeof row.byThread === "object" && !Array.isArray(row.byThread)
        ? row.byThread
        : {}
    ) as Record<string, unknown>;
    const byThread: Record<string, ConversationReadingPosition> = {};
    for (const [threadId, value] of Object.entries(byThreadRaw)) {
      const position = normalizePosition(value, threadId);
      if (position) byThread[position.threadId] = position;
    }
    const jumpBack = Array.isArray(row.jumpBack)
      ? row.jumpBack.map(normalizeJumpBack).filter((item): item is JumpBackEntry => Boolean(item))
      : [];
    memory = { byThread, jumpBack };
    return {
      byThread: { ...byThread },
      jumpBack: [...jumpBack],
    };
  } catch {
    return {
      byThread: { ...memory.byThread },
      jumpBack: [...memory.jumpBack],
    };
  }
}

function pruneThreads(byThread: Record<string, ConversationReadingPosition>): Record<string, ConversationReadingPosition> {
  const rows = Object.values(byThread).sort((a, b) => b.updatedAt - a.updatedAt);
  if (rows.length <= MAX_THREADS) return { ...byThread };
  const next: Record<string, ConversationReadingPosition> = {};
  for (const row of rows.slice(0, MAX_THREADS)) next[row.threadId] = row;
  return next;
}

function writeBucketToStorage(bucket: Bucket): void {
  memory = {
    byThread: pruneThreads(bucket.byThread),
    jumpBack: bucket.jumpBack.slice(-MAX_JUMP_BACK),
  };
  memoryDirty = false;
  const storage = getStorage();
  if (!storage) return;
  try {
    storage.setItem(READING_POSITION_STORAGE_KEY, JSON.stringify(memory));
  } catch {
    // Quota / private mode — keep memory copy only.
  }
}

function schedulePersist(bucket: Bucket): void {
  memory = {
    byThread: { ...bucket.byThread },
    jumpBack: [...bucket.jumpBack],
  };
  memoryDirty = true;
  if (typeof window === "undefined" && !storageOverride) return;
  if (persistTimer) clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    persistTimer = null;
    writeBucketToStorage(memory);
  }, PERSIST_DEBOUNCE_MS);
}

function emitJumpBack(): void {
  for (const listener of jumpBackListeners) listener();
}

export function flushConversationReadingPositionStore(): void {
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  if (!memoryDirty) return;
  writeBucketToStorage(memory);
}

export function __resetConversationReadingPositionStoreForTests(storage?: StorageLike | null): void {
  if (persistTimer) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  storageOverride = storage ?? null;
  memory = emptyBucket();
  memoryDirty = false;
  for (const key of Object.keys(pendingExpand)) delete pendingExpand[key];
  jumpBackListeners.clear();
}

export function shouldRestoreReadingPosition(messageDeepLink: string): boolean {
  return !String(messageDeepLink || "").trim();
}

export function pickVisibleAnchor(
  viewportTop: number,
  items: VisibleAnchorBox[],
): { messageId: string; offsetPx: number } | null {
  if (!items.length) return null;
  const visible = items.find((item) => item.bottom > viewportTop + 8);
  const chosen = visible || items[items.length - 1];
  if (!chosen?.messageId) return null;
  return {
    messageId: chosen.messageId,
    offsetPx: viewportTop - chosen.top,
  };
}

export function captureVisibleAnchor(stream: HTMLElement | null): { messageId: string; offsetPx: number } | null {
  if (!stream) return null;
  const viewportTop = stream.getBoundingClientRect().top;
  const nodes = Array.from(stream.querySelectorAll<HTMLElement>("[data-message-id]"));
  const items: VisibleAnchorBox[] = [];
  const seen = new Set<string>();
  for (const node of nodes) {
    const messageId = String(node.getAttribute("data-message-id") || "").trim();
    if (!messageId || seen.has(messageId)) continue;
    seen.add(messageId);
    const rect = node.getBoundingClientRect();
    items.push({ messageId, top: rect.top, bottom: rect.bottom });
  }
  return pickVisibleAnchor(viewportTop, items);
}

export function applyReadingOffset(
  stream: HTMLElement | null,
  messageId: string,
  offsetPx: number,
): boolean {
  const target = String(messageId || "").trim();
  if (!stream || !target) return false;
  const el = stream.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(target)}"]`);
  if (!el) return false;
  const streamRect = stream.getBoundingClientRect();
  const elRect = el.getBoundingClientRect();
  stream.scrollTop += (elRect.top + offsetPx) - streamRect.top;
  return true;
}

/** Restore against committed, measured rows, not the pre-fetch virtual indexes. */
export async function restoreMeasuredReadingAnchor(options: {
  stream: () => HTMLElement | null;
  messageId: string;
  offsetPx: number;
  reveal: () => void;
  isCurrent: () => boolean;
  nextFrame?: () => Promise<void>;
}): Promise<boolean> {
  const nextFrame = options.nextFrame ?? (() => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())));
  let previousGeometry = "";
  let stableFrames = 0;
  for (let frame = 0; frame < 40; frame += 1) {
    await nextFrame();
    if (!options.isCurrent()) return false;
    const stream = options.stream();
    const target = stream?.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(options.messageId)}"]`);
    if (!stream || !target) {
      options.reveal();
      stableFrames = 0;
      continue;
    }
    applyReadingOffset(stream, options.messageId, options.offsetPx);
    const rect = target.getBoundingClientRect();
    const geometry = [stream.scrollHeight, stream.clientHeight, rect.height, stream.scrollTop].join(":");
    const delta = rect.top + options.offsetPx - stream.getBoundingClientRect().top;
    const atBoundary = (stream.scrollTop <= 1 && delta < 0)
      || (stream.scrollTop >= stream.scrollHeight - stream.clientHeight - 1 && delta > 0);
    const aligned = Math.abs(delta) <= 1 || atBoundary;
    stableFrames = aligned && geometry === previousGeometry ? stableFrames + 1 : 0;
    previousGeometry = geometry;
    if (stableFrames >= 3) return true;
  }
  return false;
}

export function userTurnMessageIds(
  messages: Array<{ message_id?: string; role?: string; turn_id?: string | null }>,
): string[] {
  const ids: string[] = [];
  const seenTurns = new Set<string>();
  for (const message of messages) {
    const messageId = String(message.message_id || "").trim();
    if (!messageId) continue;
    if (message.role === "assistant") continue;
    const turnId = String(message.turn_id || "").trim();
    if (turnId) {
      if (seenTurns.has(turnId)) continue;
      seenTurns.add(turnId);
    }
    ids.push(messageId);
  }
  return ids;
}

export function neighboringTurnMessageId(
  messages: Array<{ message_id?: string; role?: string; turn_id?: string | null }>,
  currentMessageId: string,
  direction: -1 | 1,
): string {
  const turns = userTurnMessageIds(messages);
  if (!turns.length) return "";
  const current = String(currentMessageId || "").trim();
  const turnIndex = turns.indexOf(current);
  if (turnIndex >= 0) {
    const next = turnIndex + direction;
    if (next < 0 || next >= turns.length) return "";
    return turns[next];
  }
  // Current id is missing or is an assistant/orphan row — walk the full list
  // for the nearest user turn instead of snapping to the first/last turn.
  if (!current) return "";
  const messageIndex = messages.findIndex(
    (message) => String(message.message_id || "").trim() === current,
  );
  if (messageIndex < 0) return "";
  const turnPositions = turns.map((turnId) => (
    messages.findIndex((message) => String(message.message_id || "").trim() === turnId)
  ));
  if (direction < 0) {
    let best = "";
    for (let i = 0; i < turnPositions.length; i += 1) {
      const pos = turnPositions[i];
      if (pos >= 0 && pos < messageIndex) best = turns[i];
    }
    return best;
  }
  for (let i = 0; i < turnPositions.length; i += 1) {
    const pos = turnPositions[i];
    if (pos > messageIndex) return turns[i];
  }
  return "";
}

export function readReadingPosition(threadId: string): ConversationReadingPosition | null {
  const id = String(threadId || "").trim();
  if (!id) return null;
  const saved = readBucketFromStorage().byThread[id] || null;
  if (!saved) return null;
  const pending = pendingExpand[id];
  if (!pending) return saved;
  return { ...saved, expandByTurn: { ...pending, ...saved.expandByTurn } };
}

export function readExpandState(threadId: string): TurnExpandState {
  const id = String(threadId || "").trim();
  return {
    ...(pendingExpand[id] || {}),
    ...(readBucketFromStorage().byThread[id]?.expandByTurn || {}),
  };
}

export function writeReadingPosition(
  position: Omit<ConversationReadingPosition, "updatedAt"> & { updatedAt?: number },
): ConversationReadingPosition | null {
  const threadId = String(position.threadId || "").trim();
  const messageId = String(position.messageId || "").trim();
  if (!threadId || !messageId) return null;
  const bucket = readBucketFromStorage();
  const expandByTurn = {
    ...(pendingExpand[threadId] || {}),
    ...normalizeExpand(position.expandByTurn),
  };
  delete pendingExpand[threadId];
  const next: ConversationReadingPosition = {
    threadId,
    messageId,
    offsetPx: Number.isFinite(position.offsetPx) ? position.offsetPx : 0,
    stickToBottom: Boolean(position.stickToBottom),
    expandByTurn,
    updatedAt: position.updatedAt || Date.now(),
  };
  bucket.byThread[threadId] = next;
  schedulePersist(bucket);
  return next;
}

export function writeExpandState(threadId: string, expandByTurn: TurnExpandState): void {
  const id = String(threadId || "").trim();
  if (!id) return;
  const current = readReadingPosition(id);
  if (!current) {
    pendingExpand[id] = normalizeExpand(expandByTurn);
    return;
  }
  writeReadingPosition({
    ...current,
    expandByTurn,
  });
}

export function pushJumpBack(entry: JumpBackEntry): JumpBackEntry | null {
  const threadId = String(entry.threadId || "").trim();
  const messageId = String(entry.messageId || "").trim();
  if (!threadId || !messageId) return null;
  const next: JumpBackEntry = {
    threadId,
    messageId,
    offsetPx: Number.isFinite(entry.offsetPx) ? entry.offsetPx : 0,
  };
  const bucket = readBucketFromStorage();
  const last = bucket.jumpBack.at(-1);
  if (
    last
    && last.threadId === next.threadId
    && last.messageId === next.messageId
    && Math.abs(last.offsetPx - next.offsetPx) < 2
  ) {
    return last;
  }
  bucket.jumpBack = [...bucket.jumpBack, next].slice(-MAX_JUMP_BACK);
  schedulePersist(bucket);
  emitJumpBack();
  return next;
}

export function pushJumpBackFromThread(threadId: string, stream?: HTMLElement | null): JumpBackEntry | null {
  const id = String(threadId || "").trim();
  if (!id) return null;
  const captured = captureVisibleAnchor(stream || null);
  const saved = readReadingPosition(id);
  const messageId = captured?.messageId || saved?.messageId || "";
  if (!messageId) return null;
  return pushJumpBack({
    threadId: id,
    messageId,
    offsetPx: captured?.offsetPx ?? saved?.offsetPx ?? 0,
  });
}

export function peekJumpBack(): JumpBackEntry | null {
  return readBucketFromStorage().jumpBack.at(-1) || null;
}

export function popJumpBack(): JumpBackEntry | null {
  const bucket = readBucketFromStorage();
  const entry = bucket.jumpBack.pop() || null;
  if (!entry) return null;
  const current = bucket.byThread[entry.threadId];
  bucket.byThread[entry.threadId] = {
    threadId: entry.threadId,
    messageId: entry.messageId,
    offsetPx: entry.offsetPx,
    stickToBottom: false,
    expandByTurn: {
      ...(pendingExpand[entry.threadId] || {}),
      ...(current?.expandByTurn || {}),
    },
    updatedAt: Date.now(),
  };
  schedulePersist(bucket);
  emitJumpBack();
  return entry;
}

export function subscribeJumpBack(listener: () => void): () => void {
  jumpBackListeners.add(listener);
  return () => {
    jumpBackListeners.delete(listener);
  };
}

export function isTypingTarget(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  if (!el) return false;
  if (el.isContentEditable) return true;
  const tag = el.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}
