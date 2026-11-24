
export const READING_POSITION_STORAGE_KEY = "muteki:conversation-reading-position:v1";
const PERSIST_DEBOUNCE_MS = 200;
const MAX_THREADS = 80;
const MAX_JUMP_BACK = 20;

export type TurnExpandState = Record<string, { processOpen?: boolean }>;

export type ReadingAnchor = {
  messageId: string;
  offsetPx: number;
  elementMessageId?: string;
};

export type ConversationReadingPosition = ReadingAnchor & {
  threadId: string;
  stickToBottom: boolean;
  expandByTurn: TurnExpandState;
  updatedAt: number;
};

export type JumpBackEntry = ReadingAnchor & {
  threadId: string;
};

type Bucket = {
  byThread: Record<string, ConversationReadingPosition>;
  jumpBack: JumpBackEntry[];
};

type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

let memory: Bucket = { byThread: {}, jumpBack: [] };
let memoryDirty = false;
let persistTimer: ReturnType<typeof setTimeout> | null = null;
const pendingExpand: Record<string, TurnExpandState> = {};
const jumpBackListeners = new Set<() => void>();

function emptyBucket(): Bucket {
  return { byThread: {}, jumpBack: [] };
}

function getStorage(): StorageLike | null {
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
    elementMessageId: typeof row.elementMessageId === "string" ? row.elementMessageId.trim() || undefined : undefined,
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
    elementMessageId: typeof row.elementMessageId === "string" ? row.elementMessageId.trim() || undefined : undefined,
  };
}

function readBucketFromStorage(): Bucket {
  if (memoryDirty) return memory;
  const storage = getStorage();
  if (!storage) return memory;
  try {
    const raw = storage.getItem(READING_POSITION_STORAGE_KEY);
    if (!raw) {
      memory = emptyBucket();
      return memory;
    }
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      memory = emptyBucket();
      return memory;
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
  } catch {  }
  return memory;
}

function pruneThreads(byThread: Record<string, ConversationReadingPosition>): Record<string, ConversationReadingPosition> {
  const rows = Object.values(byThread);
  if (rows.length <= MAX_THREADS) return byThread;
  rows.sort((a, b) => b.updatedAt - a.updatedAt);
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
  }
}

function schedulePersist(): void {
  memoryDirty = true;
  if (typeof window === "undefined") return;
  if (persistTimer) clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    persistTimer = null;
    writeBucketToStorage(memory);
  }, PERSIST_DEBOUNCE_MS);
}

function emitJumpBack(): void {
  for (const listener of jumpBackListeners) listener();
}

export function shouldRestoreReadingPosition(messageDeepLink: string): boolean {
  return !String(messageDeepLink || "").trim();
}

export function captureVisibleAnchor(stream: HTMLElement | null): ReadingAnchor | null {
  if (!stream) return null;
  const { top: viewportTop, bottom: viewportBottom } = stream.getBoundingClientRect();
  const seen = new Set<string>();
  for (const node of stream.querySelectorAll<HTMLElement>("[data-message-id]:not([data-index])")) {
    const elementMessageId = node.dataset.messageId?.trim();
    const messageId = node.dataset.readingMessageId?.trim() ?? elementMessageId;
    if (!messageId || !elementMessageId || seen.has(elementMessageId)) continue;
    seen.add(elementMessageId);
    const { top, bottom } = node.getBoundingClientRect();
    if (bottom > viewportTop + 8 && top < viewportBottom) {
      return { messageId, offsetPx: viewportTop - top, elementMessageId };
    }
  }
  const nodes = stream.querySelectorAll<HTMLElement>("[data-message-id]");
  seen.clear();
  let lastMessageId = "";
  let lastTop = 0;
  for (const node of nodes) {
    const messageId = String(node.getAttribute("data-message-id") || "").trim();
    if (!messageId || seen.has(messageId)) continue;
    seen.add(messageId);
    const rect = node.getBoundingClientRect();
    if (rect.bottom > viewportTop + 8) {
      return { messageId, offsetPx: viewportTop - rect.top };
    }
    lastMessageId = messageId;
    lastTop = rect.top;
  }
  return lastMessageId ? { messageId: lastMessageId, offsetPx: viewportTop - lastTop } : null;
}

export function findAnchorElement(stream: HTMLElement, { messageId, elementMessageId }: ReadingAnchor): HTMLElement | null {
  const selector = `[data-message-id="${CSS.escape(elementMessageId || messageId)}"]`;
  return stream.querySelector<HTMLElement>(elementMessageId ? `${selector}:not([data-index]):not([data-reading-message-id=""])` : selector);
}

export function applyReadingOffset(
  stream: HTMLElement | null,
  anchor: ReadingAnchor,
): boolean {
  if (!stream || !anchor.messageId) return false;
  const el = findAnchorElement(stream, anchor);
  if (!el) return false;
  const streamRect = stream.getBoundingClientRect();
  const elRect = el.getBoundingClientRect();
  stream.scrollTop += (elRect.top + anchor.offsetPx) - streamRect.top;
  return true;
}

export async function restoreMeasuredReadingAnchor(options: ReadingAnchor & {
  stream: () => HTMLElement | null;
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
    const target = stream && findAnchorElement(stream, options);
    if (!stream || !target) {
      options.reveal();
      stableFrames = 0;
      continue;
    }
    applyReadingOffset(stream, options);
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
    elementMessageId: position.elementMessageId,
    stickToBottom: Boolean(position.stickToBottom),
    expandByTurn,
    updatedAt: position.updatedAt || Date.now(),
  };
  bucket.byThread[threadId] = next;
  schedulePersist();
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
    elementMessageId: entry.elementMessageId,
  };
  const bucket = readBucketFromStorage();
  const last = bucket.jumpBack.at(-1);
  if (
    last
    && last.threadId === next.threadId
    && last.messageId === next.messageId
    && last.elementMessageId === next.elementMessageId
    && Math.abs(last.offsetPx - next.offsetPx) < 2
  ) {
    return last;
  }
  bucket.jumpBack = [...bucket.jumpBack, next].slice(-MAX_JUMP_BACK);
  schedulePersist();
  emitJumpBack();
  return next;
}

export function pushJumpBackFromThread(threadId: string, stream?: HTMLElement | null): JumpBackEntry | null {
  const id = String(threadId || "").trim();
  if (!id) return null;
  const captured = captureVisibleAnchor(stream || null);
  const saved = readReadingPosition(id);
  const anchor = captured ?? saved;
  if (!anchor) return null;
  return pushJumpBack({
    threadId: id,
    messageId: anchor.messageId,
    offsetPx: anchor.offsetPx,
    elementMessageId: anchor.elementMessageId,
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
  schedulePersist();
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
