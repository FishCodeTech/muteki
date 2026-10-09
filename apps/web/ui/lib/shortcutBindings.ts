"use client";

import { useEffect, useSyncExternalStore } from "react";
import { desktopChatBridge } from "@/lib/desktopChatBridge";

/**
 * Rebindable chat shortcuts. Bindings use the `mod+shift+k` notation shared
 * with `<Shortcut>`; `mod` is ⌘ on macOS and Ctrl elsewhere.
 */
export type ShortcutActionId =
  | "newChat" | "search" | "toggleSidebar" | "focusComposer" | "help"
  | "togglePanel" | "panelDiff" | "panelPreview" | "panelFiles" | "panelTerminal" | "panelOverview"
  | "toggleLog" | "modelPicker" | "effortPicker" | "accessPicker" | "interactionMode";

export interface ShortcutActionMeta {
  id: ShortcutActionId;
  label: [string, string];
  defaultBinding: string;
  /** The desktop menu owns this default accelerator, so the renderer must not handle it a second time. */
  desktopNative?: boolean;
}

export const SHORTCUT_ACTIONS: ShortcutActionMeta[] = [
  { id: "newChat", label: ["新建对话", "New chat"], defaultBinding: "mod+shift+n", desktopNative: true },
  { id: "search", label: ["搜索 / 跳转会话", "Search conversations"], defaultBinding: "mod+k", desktopNative: true },
  { id: "toggleSidebar", label: ["切换会话侧栏", "Toggle sidebar"], defaultBinding: "mod+b", desktopNative: true },
  { id: "focusComposer", label: ["聚焦输入框", "Focus composer"], defaultBinding: "/" },
  { id: "help", label: ["打开快捷键帮助", "Open shortcut help"], defaultBinding: "?" },
  { id: "togglePanel", label: ["打开 / 关闭工作面板", "Toggle work panel"], defaultBinding: "mod+alt+b" },
  { id: "panelDiff", label: ["变更 (Diff) 面板", "Changes panel"], defaultBinding: "mod+alt+d" },
  { id: "panelPreview", label: ["预览面板", "Preview panel"], defaultBinding: "mod+alt+p" },
  { id: "panelFiles", label: ["文件面板", "Files panel"], defaultBinding: "mod+alt+f" },
  { id: "panelTerminal", label: ["终端面板", "Terminal panel"], defaultBinding: "mod+alt+t" },
  { id: "panelOverview", label: ["概览面板", "Overview panel"], defaultBinding: "mod+alt+o" },
  { id: "toggleLog", label: ["执行日志", "Execution log"], defaultBinding: "mod+j" },
  { id: "modelPicker", label: ["选择模型", "Select model"], defaultBinding: "mod+shift+m" },
  { id: "effortPicker", label: ["思考强度", "Reasoning effort"], defaultBinding: "mod+shift+e" },
  { id: "accessPicker", label: ["Agent 操作权限", "Agent permissions"], defaultBinding: "mod+shift+a" },
  { id: "interactionMode", label: ["切换规划模式", "Toggle plan mode"], defaultBinding: "shift+tab" },
];

/** Fixed bindings that a rebind must not collide with. */
export const RESERVED_BINDINGS: Array<{ binding: string; label: [string, string] }> = [
  { binding: "enter", label: ["发送", "Send"] },
  { binding: "shift+enter", label: ["换行", "New line"] },
  { binding: "mod+enter", label: ["发送 / 提交表单", "Send / submit form"] },
  { binding: "esc", label: ["关闭面板", "Close panel"] },
  { binding: "mod+z", label: ["撤销侧栏操作", "Undo sidebar action"] },
  { binding: "mod+s", label: ["暂存草稿", "Stash draft"] },
  { binding: "mod+shift+s", label: ["归置当前对话", "Settle conversation"] },
  { binding: "mod+shift+p", label: ["置顶当前对话", "Pin conversation"] },
  { binding: "mod+shift+[", label: ["上一个对话", "Previous conversation"] },
  { binding: "mod+shift+]", label: ["下一个对话", "Next conversation"] },
  { binding: "alt+up", label: ["上一轮", "Previous turn"] },
  { binding: "alt+down", label: ["下一轮", "Next turn"] },
  { binding: "mod+,", label: ["打开设置", "Open settings"] },
  { binding: "mod+r", label: ["刷新", "Reload"] },
  { binding: "mod+c", label: ["复制", "Copy"] },
  { binding: "mod+v", label: ["粘贴", "Paste"] },
  { binding: "mod+x", label: ["剪切", "Cut"] },
  { binding: "mod+a", label: ["全选", "Select all"] },
  ...Array.from({ length: 9 }, (_, index) => ({ binding: `mod+${index + 1}`, label: [`跳到第 ${index + 1} 个对话`, `Jump to conversation ${index + 1}`] as [string, string] })),
];

export const SHORTCUT_BINDINGS_KEY = "muteki.shortcuts.v1";
const CHANGE_EVENT = "muteki:shortcuts-change";
const MODIFIERS = ["mod", "ctrl", "alt", "shift"] as const;
const ACTION_IDS = new Set(SHORTCUT_ACTIONS.map(action => action.id));

let cached: Record<ShortcutActionId, string> | null = null;
let memory: Partial<Record<ShortcutActionId, string>> | null = null;

const DEFAULT_BINDINGS = Object.fromEntries(SHORTCUT_ACTIONS.map(action => [action.id, action.defaultBinding])) as Record<ShortcutActionId, string>;
const defaults = () => DEFAULT_BINDINGS;

function readOverrides(): Partial<Record<ShortcutActionId, string>> {
  if (memory) return memory;
  try {
    const raw = window.localStorage.getItem(SHORTCUT_BINDINGS_KEY);
    const parsed = raw ? JSON.parse(raw) as Record<string, unknown> : {};
    const out: Partial<Record<ShortcutActionId, string>> = {};
    for (const [id, value] of Object.entries(parsed || {})) {
      if (ACTION_IDS.has(id as ShortcutActionId) && typeof value === "string" && normalizeBinding(value)) out[id as ShortcutActionId] = normalizeBinding(value);
    }
    return out;
  } catch {
    return {};
  }
}

export function readShortcutBindings(): Record<ShortcutActionId, string> {
  if (cached) return cached;
  if (typeof window === "undefined") return DEFAULT_BINDINGS;
  cached = { ...DEFAULT_BINDINGS, ...readOverrides() };
  return cached;
}

export function shortcutBinding(id: ShortcutActionId): string {
  return readShortcutBindings()[id];
}

/** Canonical order `mod+ctrl+alt+shift+key`; returns "" for an unusable binding. */
export function normalizeBinding(value: string): string {
  const parts = value.toLowerCase().split("+").map(part => part.trim());
  // "mod++" style bindings for the plus key are not supported.
  if (parts.some(part => !part)) return "";
  const key = parts[parts.length - 1];
  const mods = new Set(parts.slice(0, -1));
  if (!key || (MODIFIERS as readonly string[]).includes(key) || [...mods].some(mod => !(MODIFIERS as readonly string[]).includes(mod))) return "";
  return [...MODIFIERS.filter(mod => mods.has(mod)), key].join("+");
}

function persist(overrides: Partial<Record<ShortcutActionId, string>>): boolean {
  cached = null;
  let ok = true;
  try {
    if (Object.keys(overrides).length) window.localStorage.setItem(SHORTCUT_BINDINGS_KEY, JSON.stringify(overrides));
    else window.localStorage.removeItem(SHORTCUT_BINDINGS_KEY);
    memory = null;
  } catch {
    memory = overrides;
    ok = false;
  }
  window.dispatchEvent(new Event(CHANGE_EVENT));
  return ok;
}

export interface ShortcutConflict { kind: "action" | "reserved"; label: [string, string] }

export function findShortcutConflict(id: ShortcutActionId, binding: string, mac: boolean): ShortcutConflict | null {
  const target = canonicalForPlatform(normalizeBinding(binding), mac);
  if (!target) return null;
  const bindings = readShortcutBindings();
  for (const action of SHORTCUT_ACTIONS) {
    if (action.id !== id && canonicalForPlatform(bindings[action.id], mac) === target) return { kind: "action", label: action.label };
  }
  const reserved = RESERVED_BINDINGS.find(row => canonicalForPlatform(row.binding, mac) === target);
  return reserved ? { kind: "reserved", label: reserved.label } : null;
}

/** Saves a binding; returns an error message (Chinese / English pair) when rejected. */
export function setShortcutBinding(id: ShortcutActionId, binding: string, mac: boolean): { ok: true; persisted: boolean } | { ok: false; error: [string, string] } {
  const normalized = normalizeBinding(binding);
  if (!normalized) return { ok: false, error: ["无效的组合键", "Invalid key combination"] };
  const parts = normalized.split("+");
  const key = parts[parts.length - 1];
  const modified = parts.some(part => part === "mod" || part === "ctrl" || part === "alt");
  if (!modified && (/^[a-z0-9]$/.test(key) || key.length > 1)) {
    return { ok: false, error: ["单键或功能键需要搭配 ⌘/Ctrl 或 ⌥/Alt，以免影响输入", "Letters and named keys need ⌘/Ctrl or ⌥/Alt so typing is not affected"] };
  }
  const conflict = findShortcutConflict(id, normalized, mac);
  if (conflict) return { ok: false, error: [`与“${conflict.label[0]}”冲突`, `Conflicts with “${conflict.label[1]}”`] };
  const overrides = { ...readOverrides() };
  const meta = SHORTCUT_ACTIONS.find(action => action.id === id);
  if (meta?.defaultBinding === normalized) delete overrides[id];
  else overrides[id] = normalized;
  return { ok: true, persisted: persist(overrides) };
}

export function resetShortcutBinding(id?: ShortcutActionId): boolean {
  if (!id) return persist({});
  const overrides = { ...readOverrides() };
  delete overrides[id];
  return persist(overrides);
}

/** `mod` and the platform's primary modifier are the same physical key. */
function canonicalForPlatform(binding: string, mac: boolean): string {
  if (!binding) return "";
  const parts = binding.split("+");
  const key = parts.pop() || "";
  const mods = new Set(parts.map(part => (part === "ctrl" && !mac ? "mod" : part)));
  return [...MODIFIERS.filter(mod => mods.has(mod)), key].join("+");
}

const CODE_KEYS: Record<string, string> = {
  BracketLeft: "[", BracketRight: "]", Comma: ",", Period: ".", Slash: "/", Backslash: "\\", Semicolon: ";", Quote: "'",
  Backquote: "`", Minus: "-", Equal: "=", ArrowUp: "up", ArrowDown: "down", ArrowLeft: "left", ArrowRight: "right",
  Enter: "enter", Escape: "esc", Space: "space", Tab: "tab", Backspace: "backspace", Delete: "delete",
};

/** Physical key name for an event. Option/Alt changes `event.key` on macOS, so letters and digits use `event.code`. */
function eventKey(event: KeyboardEvent): string {
  const code = event.code || "";
  if (/^Key[A-Z]$/.test(code)) return code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F[0-9]{1,2}$/.test(code)) return code.toLowerCase();
  return CODE_KEYS[code] || (event.key.length === 1 ? event.key.toLowerCase() : event.key.toLowerCase());
}

export function bindingFromEvent(event: KeyboardEvent, mac: boolean): string {
  if (["Meta", "Control", "Alt", "Shift"].includes(event.key)) return "";
  const parts: string[] = [];
  if (mac ? event.metaKey : event.ctrlKey) parts.push("mod");
  if (mac && event.ctrlKey) parts.push("ctrl");
  if (event.altKey) parts.push("alt");
  // Shifted punctuation (`?`) is recorded as the produced character.
  const printable = event.key.length === 1 && !/[a-z0-9]/i.test(event.key) && !event.altKey && !(mac ? event.metaKey : event.ctrlKey);
  if (event.shiftKey && !printable) parts.push("shift");
  parts.push(printable ? event.key : eventKey(event));
  return normalizeBinding(parts.join("+"));
}

export function matchesBinding(event: KeyboardEvent, binding: string, mac: boolean): boolean {
  if (!binding || event.isComposing) return false;
  const parts = binding.split("+");
  const key = parts[parts.length - 1];
  const mods = new Set(parts.slice(0, -1));
  const primary = mac ? event.metaKey : event.ctrlKey;
  if ((mods.has("mod") || (!mac && mods.has("ctrl"))) !== primary) return false;
  if (mac && mods.has("ctrl") !== event.ctrlKey) return false;
  if (mods.has("alt") !== event.altKey) return false;
  const printable = key.length === 1 && !/[a-z0-9]/.test(key);
  // A bare punctuation binding such as `?` or `/` matches the produced character, whatever Shift state made it.
  if (printable && !mods.has("mod") && !mods.has("alt") && !mods.has("ctrl") && event.key === key) return true;
  if (mods.has("shift") !== event.shiftKey) return false;
  return eventKey(event) === key;
}

export function isMacPlatform(): boolean {
  if (typeof navigator === "undefined") return false;
  return /mac|iphone|ipad/i.test(navigator.platform || navigator.userAgent);
}

export const DESKTOP_MENU_COMMANDS = { newChat: "new-chat", search: "search", toggleSidebar: "sidebar" } as const satisfies Partial<Record<ShortcutActionId, string>>;
type DesktopMenuAction = keyof typeof DESKTOP_MENU_COMMANDS;

const ACCELERATOR_KEYS: Record<string, string> = {
  up: "Up", down: "Down", left: "Left", right: "Right", enter: "Enter", esc: "Escape", space: "Space",
  tab: "Tab", backspace: "Backspace", delete: "Delete",
};

/** Electron accelerator for a binding, or "" when Electron cannot express it (shifted punctuation such as `?`). */
export function bindingToAccelerator(binding: string, mac: boolean): string {
  const normalized = normalizeBinding(binding);
  if (!normalized) return "";
  const parts = normalized.split("+");
  const key = parts.pop() || "";
  let name = "";
  if (/^[a-z0-9]$/.test(key)) name = key.toUpperCase();
  else if (/^f([1-9]|1[0-2])$/.test(key)) name = key.toUpperCase();
  else if (ACCELERATOR_KEYS[key]) name = ACCELERATOR_KEYS[key];
  else if (/^[[\],./\\;'`=-]$/.test(key)) name = key;
  if (!name) return "";
  const mods = new Set(parts);
  const out: string[] = [];
  if (mods.has("mod") || (!mac && mods.has("ctrl"))) out.push("CmdOrCtrl");
  if (mac && mods.has("ctrl")) out.push("Ctrl");
  if (mods.has("alt")) out.push("Alt");
  if (mods.has("shift")) out.push("Shift");
  return [...out, name].join("+");
}

/** Bindings most recently applied to the desktop menu; `null` until a sync succeeds. */
let desktopMenuApplied: Partial<Record<DesktopMenuAction, string>> | null = null;

export function desktopMenuAccelerators(mac: boolean): Record<string, string> {
  const bindings = readShortcutBindings();
  return Object.fromEntries((Object.keys(DESKTOP_MENU_COMMANDS) as DesktopMenuAction[]).map(id => [DESKTOP_MENU_COMMANDS[id], bindingToAccelerator(bindings[id], mac)]));
}

export function markDesktopMenuApplied(bindings: Record<ShortcutActionId, string>): void {
  desktopMenuApplied = Object.fromEntries((Object.keys(DESKTOP_MENU_COMMANDS) as DesktopMenuAction[]).map(id => [id, bindings[id]]));
}

/** True when the desktop menu dispatches this action, so the renderer must not handle the same key a second time. */
export function handledByDesktopMenu(id: ShortcutActionId, desktop: boolean): boolean {
  if (!desktop) return false;
  const meta = SHORTCUT_ACTIONS.find(action => action.id === id);
  if (!meta?.desktopNative) return false;
  const binding = shortcutBinding(id);
  if (desktopMenuApplied && id in DESKTOP_MENU_COMMANDS) {
    return desktopMenuApplied[id as DesktopMenuAction] === binding && Boolean(bindingToAccelerator(binding, isMacPlatform()));
  }
  // Older desktop builds keep their built-in accelerators.
  return binding === meta.defaultBinding;
}

function subscribe(onChange: () => void): () => void {
  const onStorage = (event: StorageEvent) => {
    if (event.key !== SHORTCUT_BINDINGS_KEY && event.key !== null) return;
    cached = null; memory = null; onChange();
  };
  window.addEventListener(CHANGE_EVENT, onChange);
  window.addEventListener("storage", onStorage);
  return () => {
    window.removeEventListener(CHANGE_EVENT, onChange);
    window.removeEventListener("storage", onStorage);
  };
}

export function useShortcutBindings(): Record<ShortcutActionId, string> {
  return useSyncExternalStore(subscribe, readShortcutBindings, defaults);
}

/** Pushes the rebindable menu shortcuts to the desktop app menu whenever they change. */
export function useDesktopMenuAcceleratorSync(): void {
  const bindings = useShortcutBindings();
  useEffect(() => {
    const bridge = desktopChatBridge();
    if (!bridge?.setMenuAccelerators) return;
    let cancelled = false;
    bridge.setMenuAccelerators(desktopMenuAccelerators(isMacPlatform()))
      .then(() => { if (!cancelled) markDesktopMenuApplied(bindings); })
      .catch(error => { console.warn("desktop menu accelerators were not applied", error); });
    return () => { cancelled = true; };
  }, [bindings]);
}
