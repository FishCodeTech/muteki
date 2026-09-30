import { conversationStorageKey, subscribeConversationStorageScope } from "./conversationStorageScope";
/**
 * C39: conversation reading preferences (font scale, density, content width).
 * Client-only localStorage — separate from theme (`muteki.theme`) and C10 stash keys.
 */

export const CONVERSATION_READING_PREFS_KEY = "muteki:conversation-reading:v1";

export type ConversationFontScale = "sm" | "md" | "lg";
export type ConversationDensity = "comfortable" | "compact";
export type ConversationContentWidth = "narrow" | "default" | "wide";

export type ConversationReadingPrefs = {
  fontScale: ConversationFontScale;
  density: ConversationDensity;
  contentWidth: ConversationContentWidth;
};

export const DEFAULT_CONVERSATION_READING_PREFS: ConversationReadingPrefs = {
  fontScale: "md",
  density: "comfortable",
  contentWidth: "default",
};

const FONT_SCALE_PX: Record<ConversationFontScale, string> = {
  sm: "12.5px",
  md: "13.5px",
  lg: "15.5px",
};

const CONTENT_WIDTH_PX: Record<ConversationContentWidth, { chat: string; composer: string }> = {
  narrow: { chat: "640px", composer: "680px" },
  default: { chat: "748px", composer: "780px" },
  wide: { chat: "960px", composer: "1000px" },
};

type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

let storageOverride: StorageLike | null = null;
let memoryPrefs: ConversationReadingPrefs = { ...DEFAULT_CONVERSATION_READING_PREFS };
let dirty = false;
let storageListener = false;
const listeners = new Set<(prefs: ConversationReadingPrefs) => void>();

function getStorage(): StorageLike | null {
  if (storageOverride) return storageOverride;
  if (typeof window === "undefined") return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function normalizePrefs(raw: unknown): ConversationReadingPrefs {
  if (!raw || typeof raw !== "object") return { ...DEFAULT_CONVERSATION_READING_PREFS };
  const row = raw as Record<string, unknown>;
  const fontScale = row.fontScale === "sm" || row.fontScale === "lg" ? row.fontScale : "md";
  const density = row.density === "compact" ? "compact" : "comfortable";
  const contentWidth =
    row.contentWidth === "narrow" || row.contentWidth === "wide" ? row.contentWidth : "default";
  return { fontScale, density, contentWidth };
}

export function __resetConversationReadingPrefsForTests(storage?: StorageLike | null): void {
  storageOverride = storage === undefined ? null : storage;
  listeners.clear();
  memoryPrefs = { ...DEFAULT_CONVERSATION_READING_PREFS }; dirty = false;
}

export function readConversationReadingPrefs(): ConversationReadingPrefs {
  if (dirty) return { ...memoryPrefs };
  const storage = getStorage();
  if (!storage) return { ...memoryPrefs };
  try {
    const raw = storage.getItem(conversationStorageKey(CONVERSATION_READING_PREFS_KEY));
    memoryPrefs = raw ? normalizePrefs(JSON.parse(raw)) : { ...DEFAULT_CONVERSATION_READING_PREFS };
    return { ...memoryPrefs };
  } catch {
    return { ...memoryPrefs };
  }
}

export function writeConversationReadingPrefs(
  patch: Partial<ConversationReadingPrefs>,
): ConversationReadingPrefs {
  const next = normalizePrefs({ ...readConversationReadingPrefs(), ...patch });
  memoryPrefs = next; dirty = true;
  const storage = getStorage();
  if (storage) {
    try {
      storage.setItem(conversationStorageKey(CONVERSATION_READING_PREFS_KEY), JSON.stringify(next));
      dirty = false;
    } catch {
      /* ignore quota */
    }
  }
  for (const listener of listeners) listener(next);
  return next;
}

export function subscribeConversationReadingPrefs(
  listener: (prefs: ConversationReadingPrefs) => void,
): () => void {
  listeners.add(listener);
  if (typeof window !== "undefined" && !storageListener) {
    storageListener = true;
    window.addEventListener("storage", (event) => {
      if (event.key !== conversationStorageKey(CONVERSATION_READING_PREFS_KEY)) return;
      try { memoryPrefs = event.newValue ? normalizePrefs(JSON.parse(event.newValue)) : { ...DEFAULT_CONVERSATION_READING_PREFS }; dirty = false; }
      catch { return; }
      for (const callback of listeners) callback({ ...memoryPrefs });
    });
  }
  return () => {
    listeners.delete(listener);
  };
}

export function conversationReadingCssVars(
  prefs: ConversationReadingPrefs = readConversationReadingPrefs(),
): Record<string, string> {
  const width = CONTENT_WIDTH_PX[prefs.contentWidth];
  return {
    "--conv-fs-body": FONT_SCALE_PX[prefs.fontScale],
    "--dsh-chat-content-width": width.chat,
    "--dsh-composer-card-max-width": width.composer,
  };
}

export function applyConversationReadingPrefsToElement(
  el: HTMLElement | null,
  prefs: ConversationReadingPrefs = readConversationReadingPrefs(),
): void {
  if (!el) return;
  const vars = conversationReadingCssVars(prefs);
  for (const [key, value] of Object.entries(vars)) {
    el.style.setProperty(key, value);
  }
  el.dataset.convFontScale = prefs.fontScale;
  el.dataset.convDensity = prefs.density;
  el.dataset.convContentWidth = prefs.contentWidth;
}

export function runtimeIdentityKey(runtime: {
  endpoint?: string | null;
  model?: string | null;
} | null | undefined): string {
  if (!runtime) return "";
  return `${String(runtime.endpoint || "").trim()}::${String(runtime.model || "").trim()}`;
}

subscribeConversationStorageScope(() => { memoryPrefs = { ...DEFAULT_CONVERSATION_READING_PREFS }; dirty = false; for (const listener of listeners) listener({ ...memoryPrefs }); });
