"use client";
import { apiFetch } from "./serviceAuth";
import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope } from "./conversationStorageScope";
import { isDesktopRemote } from "./desktopEnvironment";

export interface UiValues {
  language: "zh" | "en";
  theme: "light" | "dark" | "system";
  accent: {kind: "preset"; id: string} | {kind: "custom"; hue: number};
  sendKey: "enter" | "mod-enter"; runningSend: "queue" | "steer";
  defaultAccessMode: string; diffView: "unified" | "split"; diffWrap: boolean; diffCollapseUnchanged: boolean;
  readingFontScale: "sm" | "md" | "lg"; readingDensity: "comfortable" | "compact"; readingContentWidth: "narrow" | "default" | "wide";
  defaultModel: {credentialId: string; modelId: string} | null;
  hiddenModels: string[];
}
export type UiPatch = Partial<UiValues>;
export interface UiDocument { version: number; values: UiPatch }
export interface UiPreferenceSnapshot extends UiDocument {
  scope: string; loaded: boolean; status: "idle" | "loading" | "ready" | "saving" | "error";
  error: string; cacheError: string;
}
const EMPTY: UiDocument = {version: 0, values: {}};
const INITIAL: UiPreferenceSnapshot = {...EMPTY, scope: "", loaded: false, status: "idle", error: "", cacheError: ""};
const CACHE = "muteki.ui.cache.v1";
const MIGRATION = "muteki.ui.migrated.v1";
const listeners = new Set<() => void>();
let snapshot = INITIAL;
let confirmed: UiDocument = EMPTY;
let pending: UiPatch = {};
let generation = 0;
let refreshTask: Promise<void> | null = null;
let saveTask: Promise<void> | null = null;
let abort = new AbortController();
let users = 0;
let stopSync: (() => void) | null = null;

const object = (value: unknown): value is Record<string, unknown> => Boolean(value && typeof value === "object" && !Array.isArray(value));
const oneOf = (...values: unknown[]) => (value: unknown) => values.includes(value);
const validators: Record<keyof UiValues, (value: unknown) => boolean> = {
  language: oneOf("zh", "en"), theme: oneOf("light", "dark", "system"),
  accent: value => object(value) && (value.kind === "preset" ? ["azure", "violet", "teal", "ember"].includes(String(value.id)) : value.kind === "custom" && typeof value.hue === "number" && Number.isFinite(value.hue) && value.hue >= 0 && value.hue < 360),
  sendKey: oneOf("enter", "mod-enter"), runningSend: oneOf("queue", "steer"), defaultAccessMode: oneOf("", "supervised", "auto-accept-edits", "auto", "full-access"),
  diffView: oneOf("unified", "split"), diffWrap: value => typeof value === "boolean", diffCollapseUnchanged: value => typeof value === "boolean",
  readingFontScale: oneOf("sm", "md", "lg"), readingDensity: oneOf("comfortable", "compact"), readingContentWidth: oneOf("narrow", "default", "wide"),
  defaultModel: value => value === null || (object(value) && ["credentialId", "modelId"].every(key => typeof value[key] === "string" && value[key].length > 0 && value[key].length <= 512)),
  hiddenModels: value => Array.isArray(value) && value.length <= 512 && value.every(item => typeof item === "string" && item.length <= 2048),
};
export function validateUiPatch(value: unknown): UiPatch {
  if (!object(value) || Object.entries(value).some(([key, item]) => !Object.hasOwn(validators, key) || !validators[key as keyof UiValues](item))) throw new Error("ui.preferences.invalid_values: 偏好字段或类型不符合契约");
  return value as UiPatch;
}
export function validateUiDocument(value: unknown): UiDocument {
  if (!object(value) || !Number.isSafeInteger(value.version) || Number(value.version) < 0) throw new Error("ui.preferences.invalid_version: 偏好版本无效");
  return {version: Number(value.version), values: validateUiPatch(value.values)};
}
export const uiPreferenceSnapshot = () => snapshot;
export const serverUiPreferenceSnapshot = () => INITIAL;
export function subscribeUiPreferences(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; }
export function readUiPreference<K extends keyof UiValues>(key: K, fallback: UiValues[K]): UiValues[K] {
  return snapshot.scope === conversationStorageScope() && Object.hasOwn(snapshot.values, key) ? snapshot.values[key] as UiValues[K] : fallback;
}
export function hasLoadedUiPreferences(): boolean { return snapshot.loaded && snapshot.scope === conversationStorageScope(); }
function publish(patch: Partial<UiPreferenceSnapshot> = {}) {
  snapshot = {...snapshot, ...patch, version: confirmed.version, values: {...confirmed.values, ...pending}};
  listeners.forEach(listener => listener());
}
function cache() {
  if (!snapshot.scope) return;
  try { localStorage.setItem(conversationStorageKey(CACHE, snapshot.scope), JSON.stringify({document: confirmed, pending})); publish({cacheError: ""}); }
  catch (cause) { publish({cacheError: `本机偏好缓存未保存：${String(cause)}`}); }
}
async function responseDocument(response: Response): Promise<UiDocument> {
  if (!response.ok && response.status !== 409) {
    const raw = await response.text();
    throw new Error(`ui.preferences.http_error · HTTP ${response.status}\n${raw}`);
  }
  return validateUiDocument(await response.json());
}

/** Read persisted values, not normalized getters that manufacture defaults. */
export function legacyUiPreferences(scope: string): UiPatch {
  if (isDesktopRemote()) return {};
  const result: Record<string, unknown> = {};
  const add = (key: keyof UiValues, value: unknown) => { if (value !== undefined && validators[key](value)) result[key] = value; };
  const json = (key: string): unknown => {
    const raw = localStorage.getItem(key); if (raw === null) return undefined;
    try { return JSON.parse(raw); } catch { throw new Error(`ui.preferences.legacy_invalid: ${key}`); }
  };
  add("language", localStorage.getItem("muteki.lang"));
  add("theme", localStorage.getItem("muteki.themePreference") ?? localStorage.getItem("muteki.theme"));
  const scheme = localStorage.getItem("muteki.scheme");
  const hue = localStorage.getItem("muteki.schemeHue");
  if (scheme !== null && (scheme !== "custom" || hue !== null)) add("accent", scheme === "custom" ? {kind: "custom", hue: Number(hue)} : {kind: "preset", id: scheme});
  const chat = json("muteki.chatPrefs.v1");
  if (object(chat)) for (const key of ["sendKey", "runningSend", "defaultAccessMode", "diffView", "diffWrap", "diffCollapseUnchanged"] as const) if (Object.hasOwn(chat, key)) add(key, chat[key]);
  const reading = json(conversationStorageKey("muteki:conversation-reading:v1", scope));
  if (object(reading)) { add("readingFontScale", reading.fontScale); add("readingDensity", reading.density); add("readingContentWidth", reading.contentWidth); }
  add("defaultModel", json(conversationStorageKey("muteki.conversation.default-model.v1", scope)));
  add("hiddenModels", json("muteki.chat.model-visibility.v1"));
  return result as UiPatch;
}
function current(gen: number, scope: string) { return gen === generation && scope === conversationStorageScope() && scope === snapshot.scope; }

async function putPatch(patch: UiPatch, gen: number, scope: string, migration = false): Promise<void> {
  for (let attempt = 0; attempt < 3; attempt++) {
    if (!current(gen, scope)) return;
    // Recompute on every conflict: another client may have filled previously absent fields.
    const changes = migration ? Object.fromEntries(Object.entries(patch).filter(([key]) => !Object.hasOwn(confirmed.values, key))) : patch;
    if (!Object.keys(changes).length) return;
    const response = await apiFetch("/api/settings/ui", {method: "PUT", signal: abort.signal, headers: {"Content-Type": "application/json"},
      body: JSON.stringify({version: confirmed.version, values: {...confirmed.values, ...changes}})});
    const document = await responseDocument(response);
    if (!current(gen, scope)) return;
    confirmed = document;
    if (response.status !== 409) {
      if (Object.entries(changes).some(([key, value]) => JSON.stringify(document.values[key as keyof UiValues]) !== JSON.stringify(value))) throw new Error("ui.preferences.readback_mismatch: 服务响应尚未确认本次设置，请重试。");
      return;
    }
  }
  throw new Error("ui.preferences.conflict: 设置同时被其他窗口修改，请重试保存。");
}

export function refreshUiPreferences(): Promise<void> {
  if (refreshTask) return refreshTask;
  if (saveTask) return saveTask;
  const scope = conversationStorageScope(); if (!scope) return Promise.resolve();
  const gen = generation;
  publish({status: "loading", error: ""});
  const task = (async () => {
    try {
      const healthResponse = await apiFetch("/api/health", {signal: abort.signal});
      if (!healthResponse.ok) throw new Error(`读取服务功能失败：HTTP ${healthResponse.status}`);
      const health = await healthResponse.json();
      if (!current(gen, scope)) return;
      if (health.features?.["ui.preferences"] !== 1) throw new Error("ui.preferences.unsupported: 当前服务未声明共享偏好契约，请更新服务。");
      const document = await responseDocument(await apiFetch("/api/settings/ui", {signal: abort.signal, cache: "no-store"}));
      if (!current(gen, scope)) return;
      confirmed = document;
      if (!isDesktopRemote() && localStorage.getItem(conversationStorageKey(MIGRATION, scope)) !== "1") {
        await putPatch(legacyUiPreferences(scope), gen, scope, true);
        if (!current(gen, scope)) return;
        localStorage.setItem(conversationStorageKey(MIGRATION, scope), "1");
      }
      if (current(gen, scope)) { publish({loaded: true, status: "ready", error: ""}); cache(); }
    } catch (cause) { if (current(gen, scope)) publish({status: "error", error: cause instanceof Error ? cause.message : String(cause)}); }
  })();
  refreshTask = task;
  void task.finally(() => { if (refreshTask === task) { refreshTask = null; if (snapshot.status === "ready" && Object.keys(pending).length) void saveUiPreferences(); } });
  return task;
}
export function saveUiPreferences(): Promise<void> {
  if (saveTask) return saveTask;
  if (!snapshot.loaded) return refreshUiPreferences();
  const scope = snapshot.scope, gen = generation;
  const task = (async () => {
    try {
      if (refreshTask) await refreshTask;
      while (current(gen, scope) && Object.keys(pending).length) {
        const patch = {...pending}; publish({status: "saving", error: ""});
        await putPatch(patch, gen, scope);
        if (!current(gen, scope)) return;
        for (const key of Object.keys(patch) as Array<keyof UiValues>) if (JSON.stringify(pending[key]) === JSON.stringify(patch[key])) delete pending[key];
        publish({status: "ready"}); cache();
      }
    } catch (cause) { if (current(gen, scope)) { publish({status: "error", error: cause instanceof Error ? cause.message : String(cause)}); cache(); } }
  })();
  saveTask = task;
  void task.finally(() => { if (saveTask === task) saveTask = null; });
  return task;
}
/** Queues an explicit edit. The shared status reports asynchronous persistence failures. */
export function writeUiPreferences(patch: UiPatch): boolean {
  try {
    validateUiPatch(patch);
    if (!Object.keys(patch).length) return true;
    if (!snapshot.scope || snapshot.scope !== conversationStorageScope()) throw new Error("请连接并验证服务身份后保存共享偏好。");
    pending = {...pending, ...patch}; publish({status: "saving", error: ""}); cache();
    void saveUiPreferences(); return true;
  } catch (cause) { publish({status: "error", error: String(cause)}); return false; }
}
function changeScope() {
  abort.abort(); abort = new AbortController(); generation++; refreshTask = null; saveTask = null;
  const scope = conversationStorageScope(); confirmed = EMPTY; pending = {};
  snapshot = {...INITIAL, scope};
  if (scope) {
    try {
      const raw = localStorage.getItem(conversationStorageKey(CACHE, scope));
      if (raw) { const value = JSON.parse(raw); confirmed = validateUiDocument(value.document); pending = validateUiPatch(value.pending || {}); }
    } catch (cause) { snapshot = {...snapshot, cacheError: `本机偏好缓存无法读取：${String(cause)}`}; }
  }
  publish(); if (scope) void refreshUiPreferences();
}
export function startUiPreferencesSync(): () => void {
  users++;
  if (users === 1) {
    const unsubscribe = subscribeConversationStorageScope(changeScope);
    const focus = () => { void refreshUiPreferences(); };
    window.addEventListener("focus", focus); changeScope();
    stopSync = () => { unsubscribe(); window.removeEventListener("focus", focus); abort.abort(); };
  }
  return () => { if (--users === 0) { stopSync?.(); stopSync = null; } };
}
