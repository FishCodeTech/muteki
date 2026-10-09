"use client";

import { useSyncExternalStore } from "react";
import { readUiPreference, writeUiPreferences, subscribeUiPreferences, type UiPatch } from "./uiPreferences";

export type SendKey = "enter" | "mod-enter";
export type RunningSendDefault = "queue" | "steer";
export type DiffViewDefault = "unified" | "split";
export type UiFontSize = 13 | 14 | 15;
/** Desktop "open in editor" target; "auto" tries VS Code, Cursor, Windsurf, Zed in order. */
export type EditorChoice = "auto" | "code" | "cursor" | "windsurf" | "zed" | "system";
export const EDITOR_CHOICES: EditorChoice[] = ["auto", "code", "cursor", "windsurf", "zed", "system"];

export interface ChatPreferences {
  sendKey: SendKey;
  runningSend: RunningSendDefault;
  /** Empty means the built-in default (`supervised`). */
  defaultAccessMode: string;
  diffView: DiffViewDefault;
  diffWrap: boolean;
  diffCollapseUnchanged: boolean;
  uiFontSize: UiFontSize;
  /** Empty means the built-in interface stack. */
  uiFont: string;
  /** Empty means the built-in monospace stack. */
  codeFont: string;
  editor: EditorChoice;
}

export const CHAT_PREFERENCES_KEY = "muteki.chatPrefs.v1";
const CHANGE_EVENT = "muteki:chat-prefs-change";

export const DEFAULT_CHAT_PREFERENCES: ChatPreferences = {
  sendKey: "enter",
  runningSend: "queue",
  defaultAccessMode: "",
  diffView: "unified",
  diffWrap: false,
  diffCollapseUnchanged: true,
  uiFontSize: 14,
  uiFont: "",
  codeFont: "",
  editor: "auto",
};

export const UI_FONT_SIZES: UiFontSize[] = [13, 14, 15];
const MONO_FALLBACK = "ui-monospace, \"SF Mono\", SFMono-Regular, Menlo, Consolas, \"Liberation Mono\", monospace";
// Keeps CJK glyphs on a matching system font when the chosen family lacks them.
const SANS_FALLBACK = "-apple-system, BlinkMacSystemFont, \"SF Pro Text\", \"Inter\", \"Segoe UI\", \"PingFang SC\", "
  + "\"Hiragino Sans GB\", \"Microsoft YaHei\", \"Noto Sans CJK SC\", \"Helvetica Neue\", Arial, sans-serif";

let cached: ChatPreferences | null = null;
let memory: ChatPreferences | null = null;

/** Strips characters that could end the declaration; the value only ever lands in a font-family property. */
export function sanitizeCodeFont(value: string): string {
  return value.replace(/[;{}<>\\\n\r]/g, "").trim().slice(0, 200);
}

function normalize(raw: unknown): ChatPreferences {
  const row = raw && typeof raw === "object" ? raw as Record<string, unknown> : {};
  const size = Number(row.uiFontSize);
  return {
    sendKey: row.sendKey === "mod-enter" ? "mod-enter" : "enter",
    runningSend: row.runningSend === "steer" ? "steer" : "queue",
    defaultAccessMode: typeof row.defaultAccessMode === "string" ? row.defaultAccessMode.trim().slice(0, 64) : "",
    diffView: row.diffView === "split" ? "split" : "unified",
    diffWrap: row.diffWrap === true,
    diffCollapseUnchanged: row.diffCollapseUnchanged !== false,
    uiFontSize: UI_FONT_SIZES.includes(size as UiFontSize) ? size as UiFontSize : 14,
    uiFont: typeof row.uiFont === "string" ? sanitizeCodeFont(row.uiFont) : "",
    codeFont: typeof row.codeFont === "string" ? sanitizeCodeFont(row.codeFont) : "",
    editor: EDITOR_CHOICES.includes(row.editor as EditorChoice) ? row.editor as EditorChoice : "auto",
  };
}

export function readChatPreferences(): ChatPreferences {
  if (cached) return cached;
  if (typeof window === "undefined") return DEFAULT_CHAT_PREFERENCES;
  try {
    const raw = window.localStorage.getItem(CHAT_PREFERENCES_KEY);
    cached = memory || (raw ? normalize(JSON.parse(raw)) : { ...DEFAULT_CHAT_PREFERENCES });
  } catch {
    cached = { ...DEFAULT_CHAT_PREFERENCES };
  }
  cached = {...cached,
    sendKey: readUiPreference("sendKey", DEFAULT_CHAT_PREFERENCES.sendKey), runningSend: readUiPreference("runningSend", DEFAULT_CHAT_PREFERENCES.runningSend),
    defaultAccessMode: readUiPreference("defaultAccessMode", DEFAULT_CHAT_PREFERENCES.defaultAccessMode), diffView: readUiPreference("diffView", DEFAULT_CHAT_PREFERENCES.diffView),
    diffWrap: readUiPreference("diffWrap", DEFAULT_CHAT_PREFERENCES.diffWrap), diffCollapseUnchanged: readUiPreference("diffCollapseUnchanged", DEFAULT_CHAT_PREFERENCES.diffCollapseUnchanged)};
  return cached;
}
subscribeUiPreferences(() => { cached = null; });

/** Returns false when storage refused the write; the change still applies to this window. */
export function writeChatPreferences(patch: Partial<ChatPreferences>): boolean {
  const shared: UiPatch = {};
  for (const key of ["sendKey", "runningSend", "defaultAccessMode", "diffView", "diffWrap", "diffCollapseUnchanged"] as const) {
    if (Object.hasOwn(patch, key)) Object.assign(shared, {[key]: patch[key]});
  }
  let persisted = !Object.keys(shared).length || writeUiPreferences(shared);
  const next = normalize({...readChatPreferences(), ...patch});
  if (["uiFontSize", "uiFont", "codeFont", "editor"].some(key => Object.hasOwn(patch, key))) {
    try {
      const old = JSON.parse(window.localStorage.getItem(CHAT_PREFERENCES_KEY) || "{}");
      for (const key of ["uiFontSize", "uiFont", "codeFont", "editor"] as const) if (Object.hasOwn(patch, key)) old[key] = next[key];
      window.localStorage.setItem(CHAT_PREFERENCES_KEY, JSON.stringify(old)); memory = null;
    } catch { memory = next; persisted = false; }
  }
  cached = null; applyFontPreferences(next); window.dispatchEvent(new Event(CHANGE_EVENT));
  return persisted;
}

export function subscribeChatPreferences(onChange: () => void): () => void {
  const onStorage = (event: StorageEvent) => {
    if (event.key !== CHAT_PREFERENCES_KEY && event.key !== null) return;
    cached = null; memory = null;
    applyFontPreferences(readChatPreferences());
    onChange();
  };
  const offUi = subscribeUiPreferences(() => { cached = null; applyFontPreferences(readChatPreferences()); onChange(); });
  window.addEventListener(CHANGE_EVENT, onChange);
  window.addEventListener("storage", onStorage);
  return () => {
    offUi();
    window.removeEventListener(CHANGE_EVENT, onChange);
    window.removeEventListener("storage", onStorage);
  };
}

export function useChatPreferences(): ChatPreferences {
  return useSyncExternalStore(subscribeChatPreferences, readChatPreferences, () => DEFAULT_CHAT_PREFERENCES);
}

export function applyFontPreferences(prefs: ChatPreferences = readChatPreferences()): void {
  if (typeof document === "undefined") return;
  const root = document.documentElement.style;
  if (prefs.uiFontSize === 14) root.removeProperty("--cx-ui-fs");
  else root.setProperty("--cx-ui-fs", `${prefs.uiFontSize}px`);
  if (prefs.uiFont) {
    const stack = `${prefs.uiFont}, ${SANS_FALLBACK}`;
    root.setProperty("--cx-font-sans", stack);
    root.setProperty("--font-sans", stack);
  } else {
    root.removeProperty("--cx-font-sans");
    root.removeProperty("--font-sans");
  }
  if (prefs.codeFont) {
    const stack = `${prefs.codeFont}, ${MONO_FALLBACK}`;
    root.setProperty("--cx-font-mono", stack);
    root.setProperty("--font-mono", stack);
  } else {
    root.removeProperty("--cx-font-mono");
    root.removeProperty("--font-mono");
  }
}

/** Whether a composer keydown should submit, given the configured send key. */
export function isSendChord(event: { key: string; shiftKey: boolean; metaKey: boolean; ctrlKey: boolean; altKey: boolean }, sendKey: SendKey): boolean {
  if (event.key !== "Enter" || event.shiftKey) return false;
  return sendKey === "enter" ? true : event.metaKey || event.ctrlKey;
}

export function sendKeyHint(sendKey: SendKey, mac: boolean): string {
  return sendKey === "enter" ? "Enter" : mac ? "⌘Enter" : "Ctrl+Enter";
}
