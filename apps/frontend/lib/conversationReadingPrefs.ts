import { readUiPreference, writeUiPreferences, subscribeUiPreferences } from "./uiPreferences";
import { subscribeConversationStorageScope } from "./conversationStorageScope";

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
  sm: "14px",
  md: "15px",
  lg: "17px",
};

const CONTENT_WIDTH_PX: Record<ConversationContentWidth, { chat: string; composer: string }> = {
  narrow: { chat: "672px", composer: "656px" },
  default: { chat: "768px", composer: "752px" },
  wide: { chat: "960px", composer: "944px" },
};

const listeners = new Set<(prefs: ConversationReadingPrefs) => void>();

function normalizePrefs(raw: unknown): ConversationReadingPrefs {
  if (!raw || typeof raw !== "object") return { ...DEFAULT_CONVERSATION_READING_PREFS };
  const row = raw as Record<string, unknown>;
  const fontScale = row.fontScale === "sm" || row.fontScale === "lg" ? row.fontScale : "md";
  const density = row.density === "compact" ? "compact" : "comfortable";
  const contentWidth =
    row.contentWidth === "narrow" || row.contentWidth === "wide" ? row.contentWidth : "default";
  return { fontScale, density, contentWidth };
}

export function readConversationReadingPrefs(): ConversationReadingPrefs {
  return {
    fontScale: readUiPreference("readingFontScale", "md"),
    density: readUiPreference("readingDensity", "comfortable"),
    contentWidth: readUiPreference("readingContentWidth", "default"),
  };
}

export function writeConversationReadingPrefs(
  patch: Partial<ConversationReadingPrefs>,
): ConversationReadingPrefs {
  const next = normalizePrefs({ ...readConversationReadingPrefs(), ...patch });
  writeUiPreferences({
    ...(patch.fontScale !== undefined ? { readingFontScale: patch.fontScale } : {}),
    ...(patch.density !== undefined ? { readingDensity: patch.density } : {}),
    ...(patch.contentWidth !== undefined ? { readingContentWidth: patch.contentWidth } : {}),
  });
  return next;
}

export function subscribeConversationReadingPrefs(
  listener: (prefs: ConversationReadingPrefs) => void,
): () => void {
  listeners.add(listener);
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
    "--cx-msg-fs": FONT_SCALE_PX[prefs.fontScale],
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

subscribeConversationStorageScope(() => { for (const listener of listeners) listener({ ...DEFAULT_CONVERSATION_READING_PREFS }); });

subscribeUiPreferences(() => { for (const listener of listeners) listener(readConversationReadingPrefs()); });
