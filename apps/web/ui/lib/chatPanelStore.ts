"use client";

import { useMemo, useSyncExternalStore } from "react";
import type { DiffBaselineKind } from "@/lib/conversationDiff";
import type { FileRevealRequest, FileRevealTarget } from "@/lib/fileRevealPlan";

export type { FileRevealRequest, FileRevealTarget } from "@/lib/fileRevealPlan";

/**
 * Right-panel state for the chat workbench, keyed per thread (T3 Code style):
 * one panel, a strip of tab-like surfaces, and imperative entry points that
 * any component (timeline cards, links, header, shortcuts) can call without
 * prop drilling. Surfaces and width persist in localStorage; open requests
 * (diff reveal, file reveal, preview navigation) are transient.
 */

export type SurfaceKind =
  | "overview"
  | "diff"
  | "preview"
  | "file"
  | "files"
  | "terminal"
  | "agents"
  | "plan"
  | "pull-request";

export type ChatSurface =
  | { id: "overview"; kind: "overview" }
  | { id: "diff"; kind: "diff" }
  | { id: `preview:${string}`; kind: "preview"; url: string | null; title?: string }
  | { id: `file:${string}`; kind: "file"; path: string; line?: number }
  | { id: "files"; kind: "files" }
  | { id: "terminal"; kind: "terminal" }
  | { id: "agents"; kind: "agents" }
  | { id: "plan"; kind: "plan" }
  | { id: "pull-request"; kind: "pull-request" };

export type SingletonKind = Exclude<SurfaceKind, "preview" | "file">;

export interface ThreadPanelState {
  isOpen: boolean;
  activeId: string | null;
  surfaces: ChatSurface[];
  maximized: boolean;
}

export interface DiffOpenRequest {
  kind: DiffBaselineKind;
  turnId?: string;
  artifactSha256?: string;
  /** File to scroll to once the diff renders. */
  filePath?: string;
  nonce: number;
}

export interface PreviewNavigateRequest {
  surfaceId: string;
  url: string;
  nonce: number;
}

export interface DetectedServer {
  url: string;
  source: "terminal" | "tool" | "message";
  seenAt: number;
}

export const PANEL_WIDTH_MIN = 360;
export const PANEL_WIDTH_DEFAULT = 560;
export const PANEL_MAIN_MIN = 420;
export const PANEL_SHEET_BREAKPOINT = 980;

export function panelWidthMax(viewport: number, sidebar = 0): number {
  return Math.max(PANEL_WIDTH_MIN, Math.min(Math.round(viewport * 0.72), viewport - sidebar - PANEL_MAIN_MIN));
}

const STORAGE_KEY = "muteki.chat.panel.v1";
const WIDTH_KEY = "muteki.chat.panel.width.v1";
const RECENT_KEY = "muteki.chat.preview.recent.v1";
const MAX_THREADS = 60;
const MAX_RECENT = 8;

interface StoreState {
  threads: Record<string, ThreadPanelState>;
  order: string[];
  width: number;
  diffRequests: Record<string, DiffOpenRequest | undefined>;
  fileRequests: Record<string, FileRevealRequest | undefined>;
  previewRequests: Record<string, PreviewNavigateRequest | undefined>;
  recentUrls: Record<string, string[]>;
  detected: Record<string, DetectedServer[]>;
  /** Threads where the user explicitly closed the panel this session — proactive opens are suppressed. */
  userClosed: Record<string, boolean>;
}

const EMPTY_THREAD: ThreadPanelState = { isOpen: false, activeId: null, surfaces: [], maximized: false };

let state: StoreState = {
  threads: {},
  order: [],
  width: PANEL_WIDTH_DEFAULT,
  diffRequests: {},
  fileRequests: {},
  previewRequests: {},
  recentUrls: {},
  detected: {},
  userClosed: {},
};
let hydrated = false;
const listeners = new Set<() => void>();

function hydrate() {
  if (hydrated || typeof window === "undefined") return;
  hydrated = true;
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as { threads?: Record<string, ThreadPanelState>; order?: string[] };
      const threads: Record<string, ThreadPanelState> = {};
      for (const [id, value] of Object.entries(parsed.threads || {})) {
        if (!value || !Array.isArray(value.surfaces)) continue;
        threads[id] = {
          isOpen: Boolean(value.isOpen),
          activeId: value.activeId ?? null,
          surfaces: value.surfaces.filter((surface) => surface && typeof surface.id === "string"),
          maximized: false,
        };
      }
      state = { ...state, threads, order: (parsed.order || []).filter((id) => threads[id]) };
    }
    const width = Number(window.localStorage.getItem(WIDTH_KEY));
    if (Number.isFinite(width) && width >= PANEL_WIDTH_MIN) state = { ...state, width };
    const recent = window.localStorage.getItem(RECENT_KEY);
    if (recent) state = { ...state, recentUrls: JSON.parse(recent) as Record<string, string[]> };
  } catch {
    // Storage blocked or corrupted: start clean.
  }
}

let persistTimer: number | undefined;
function persist() {
  if (typeof window === "undefined") return;
  window.clearTimeout(persistTimer);
  persistTimer = window.setTimeout(() => {
    try {
      const threads: Record<string, ThreadPanelState> = {};
      for (const id of state.order.slice(-MAX_THREADS)) {
        const value = state.threads[id];
        if (value) threads[id] = { ...value, maximized: false };
      }
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify({ threads, order: Object.keys(threads) }));
      window.localStorage.setItem(WIDTH_KEY, String(state.width));
      window.localStorage.setItem(RECENT_KEY, JSON.stringify(state.recentUrls));
    } catch {
      // ignore quota errors
    }
  }, 120);
}

function emit() {
  persist();
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void) {
  hydrate();
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function getSnapshot(): StoreState {
  hydrate();
  return state;
}

const serverSnapshot = state;

function updateThread(threadId: string, fn: (current: ThreadPanelState) => ThreadPanelState) {
  if (!threadId) return;
  hydrate();
  const current = state.threads[threadId] ?? EMPTY_THREAD;
  const next = fn(current);
  if (next === current) return;
  const order = state.order.filter((id) => id !== threadId);
  order.push(threadId);
  state = { ...state, threads: { ...state.threads, [threadId]: next }, order };
  emit();
}

export function surfaceForKind(kind: SingletonKind): ChatSurface {
  return { id: kind, kind } as ChatSurface;
}

function withSurface(current: ThreadPanelState, surface: ChatSurface, activate = true): ThreadPanelState {
  const exists = current.surfaces.findIndex((item) => item.id === surface.id);
  const surfaces = exists >= 0
    ? current.surfaces.map((item, index) => (index === exists ? { ...item, ...surface } as ChatSurface : item))
    : [...current.surfaces, surface];
  return { ...current, isOpen: true, surfaces, activeId: activate ? surface.id : current.activeId ?? surface.id };
}

function randomId(): string {
  return Math.random().toString(36).slice(2, 9);
}

function markTouched(threadId: string, closed: boolean) {
  if (state.userClosed[threadId] === closed) return;
  state = { ...state, userClosed: { ...state.userClosed, [threadId]: closed } };
}

export const chatPanel = {
  open(threadId: string, target?: SingletonKind | ChatSurface) {
    markTouched(threadId, false);
    updateThread(threadId, (current) => {
      if (!target) return { ...current, isOpen: true };
      const surface = typeof target === "string" ? surfaceForKind(target) : target;
      return withSurface(current, surface);
    });
  },

  /** Opens only if the user hasn't closed the panel in this thread this session. */
  openProactive(threadId: string, target: SingletonKind | ChatSurface) {
    hydrate();
    if (state.userClosed[threadId]) return;
    const current = state.threads[threadId];
    if (current?.isOpen) return;
    updateThread(threadId, (thread) => withSurface(thread, typeof target === "string" ? surfaceForKind(target) : target));
  },

  close(threadId: string) {
    markTouched(threadId, true);
    updateThread(threadId, (current) => (current.isOpen ? { ...current, isOpen: false, maximized: false } : current));
  },

  /** Toggle the whole panel, or a specific surface (closing only if it is already the visible one). */
  toggle(threadId: string, kind?: SingletonKind) {
    hydrate();
    const current = state.threads[threadId] ?? EMPTY_THREAD;
    if (!kind) {
      if (current.isOpen) chatPanel.close(threadId);
      else chatPanel.open(threadId, current.surfaces.length ? undefined : "overview");
      return;
    }
    if (current.isOpen && current.activeId === kind) chatPanel.close(threadId);
    else chatPanel.open(threadId, kind);
  },

  activate(threadId: string, surfaceId: string) {
    updateThread(threadId, (current) => (current.activeId === surfaceId ? current : { ...current, activeId: surfaceId, isOpen: true }));
  },

  closeSurface(threadId: string, surfaceId: string) {
    updateThread(threadId, (current) => {
      const index = current.surfaces.findIndex((item) => item.id === surfaceId);
      if (index < 0) return current;
      const surfaces = current.surfaces.filter((item) => item.id !== surfaceId);
      let activeId = current.activeId;
      if (activeId === surfaceId) activeId = surfaces[Math.min(index, surfaces.length - 1)]?.id ?? null;
      return { ...current, surfaces, activeId };
    });
  },

  closeOthers(threadId: string, surfaceId: string) {
    updateThread(threadId, (current) => ({ ...current, surfaces: current.surfaces.filter((item) => item.id === surfaceId), activeId: surfaceId }));
  },

  closeToRight(threadId: string, surfaceId: string) {
    updateThread(threadId, (current) => {
      const index = current.surfaces.findIndex((item) => item.id === surfaceId);
      if (index < 0) return current;
      const surfaces = current.surfaces.slice(0, index + 1);
      const activeId = surfaces.some((item) => item.id === current.activeId) ? current.activeId : surfaceId;
      return { ...current, surfaces, activeId };
    });
  },

  closeAll(threadId: string) {
    updateThread(threadId, (current) => ({ ...current, surfaces: [], activeId: null }));
  },

  reorder(threadId: string, fromId: string, toId: string) {
    updateThread(threadId, (current) => {
      const surfaces = [...current.surfaces];
      const from = surfaces.findIndex((item) => item.id === fromId);
      const to = surfaces.findIndex((item) => item.id === toId);
      if (from < 0 || to < 0 || from === to) return current;
      const [moved] = surfaces.splice(from, 1);
      surfaces.splice(to, 0, moved);
      return { ...current, surfaces };
    });
  },

  setMaximized(threadId: string, maximized: boolean) {
    updateThread(threadId, (current) => (current.maximized === maximized ? current : { ...current, maximized }));
  },

  setWidth(width: number) {
    hydrate();
    if (state.width === width) return;
    state = { ...state, width };
    emit();
  },

  openDiff(threadId: string, request: Omit<DiffOpenRequest, "nonce"> = { kind: "worktree" }) {
    hydrate();
    state = { ...state, diffRequests: { ...state.diffRequests, [threadId]: { ...request, nonce: Date.now() + Math.random() } } };
    chatPanel.open(threadId, "diff");
  },

  consumeDiffRequest(threadId: string, nonce: number) {
    if (state.diffRequests[threadId]?.nonce !== nonce) return;
    state = { ...state, diffRequests: { ...state.diffRequests, [threadId]: undefined } };
    emit();
  },

  /** Reveal a directory or file inside the Files surface (kind-split; #207). */
  revealInFiles(threadId: string, target: FileRevealTarget) {
    hydrate();
    state = {
      ...state,
      fileRequests: {
        ...state.fileRequests,
        [threadId]: { ...target, nonce: Date.now() + Math.random() },
      },
    };
    chatPanel.open(threadId, "files");
  },

  consumeFileRequest(threadId: string, nonce: number) {
    if (state.fileRequests[threadId]?.nonce !== nonce) return;
    state = { ...state, fileRequests: { ...state.fileRequests, [threadId]: undefined } };
    emit();
  },

  /** Open a single file as its own tab (rich preview: markdown, html, image, pdf, code). */
  openFile(threadId: string, path: string, line?: number) {
    chatPanel.open(threadId, { id: `file:${path}`, kind: "file", path, line });
  },

  /**
   * Open a URL in the preview. Reuses a tab already showing that URL, else
   * navigates the active preview tab, else creates one. `newTab` forces a new tab.
   */
  openPreview(threadId: string, url?: string | null, options: { newTab?: boolean } = {}) {
    hydrate();
    const current = state.threads[threadId] ?? EMPTY_THREAD;
    const previews = current.surfaces.filter((item): item is Extract<ChatSurface, { kind: "preview" }> => item.kind === "preview");
    if (url) chatPanel.rememberUrl(threadId, url);
    if (url && !options.newTab) {
      const same = previews.find((item) => item.url === url);
      if (same) { chatPanel.open(threadId, same); return; }
    }
    const active = previews.find((item) => item.id === current.activeId) ?? previews.at(-1);
    if (url && active && !options.newTab) {
      state = { ...state, previewRequests: { ...state.previewRequests, [threadId]: { surfaceId: active.id, url, nonce: Date.now() + Math.random() } } };
      chatPanel.open(threadId, { ...active, url });
      return;
    }
    if (!url && active && !options.newTab) { chatPanel.open(threadId, active); return; }
    chatPanel.open(threadId, { id: `preview:${randomId()}`, kind: "preview", url: url ?? null });
  },

  updatePreview(threadId: string, surfaceId: string, patch: { url?: string | null; title?: string }) {
    updateThread(threadId, (current) => ({
      ...current,
      surfaces: current.surfaces.map((item) => (item.id === surfaceId && item.kind === "preview" ? { ...item, ...patch } : item)),
    }));
  },

  consumePreviewRequest(threadId: string, nonce: number) {
    if (state.previewRequests[threadId]?.nonce !== nonce) return;
    state = { ...state, previewRequests: { ...state.previewRequests, [threadId]: undefined } };
    emit();
  },

  rememberUrl(threadId: string, url: string) {
    hydrate();
    const list = [url, ...(state.recentUrls[threadId] || []).filter((item) => item !== url)].slice(0, MAX_RECENT);
    state = { ...state, recentUrls: { ...state.recentUrls, [threadId]: list } };
    emit();
  },

  forgetUrl(threadId: string, url: string) {
    state = { ...state, recentUrls: { ...state.recentUrls, [threadId]: (state.recentUrls[threadId] || []).filter((item) => item !== url) } };
    emit();
  },

  reportDetectedUrl(threadId: string, url: string, source: DetectedServer["source"]) {
    if (!threadId || !url) return;
    hydrate();
    const list = state.detected[threadId] || [];
    const existing = list.find((item) => item.url === url);
    if (existing && Date.now() - existing.seenAt < 5_000) return;
    const next = [{ url, source, seenAt: Date.now() }, ...list.filter((item) => item.url !== url)].slice(0, 12);
    state = { ...state, detected: { ...state.detected, [threadId]: next } };
    listeners.forEach((listener) => listener());
  },
};

function useStore<T>(selector: (snapshot: StoreState) => T): T {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, () => serverSnapshot);
  return selector(snapshot);
}

export function useChatPanel(threadId: string): ThreadPanelState {
  return useStore((snapshot) => snapshot.threads[threadId] ?? EMPTY_THREAD);
}

export function useChatPanelWidth(): number {
  return useStore((snapshot) => snapshot.width);
}

export function useDiffRequest(threadId: string): DiffOpenRequest | undefined {
  return useStore((snapshot) => snapshot.diffRequests[threadId]);
}

export function useFileRequest(threadId: string): FileRevealRequest | undefined {
  return useStore((snapshot) => snapshot.fileRequests[threadId]);
}

export function usePreviewRequest(threadId: string): PreviewNavigateRequest | undefined {
  return useStore((snapshot) => snapshot.previewRequests[threadId]);
}

export function useRecentUrls(threadId: string): string[] {
  const list = useStore((snapshot) => snapshot.recentUrls[threadId]);
  return useMemo(() => list ?? [], [list]);
}

export function useDetectedServers(threadId: string): DetectedServer[] {
  const list = useStore((snapshot) => snapshot.detected[threadId]);
  return useMemo(() => list ?? [], [list]);
}

const SURFACE_META: Record<SurfaceKind, { label: string; icon: string; shortcut?: string }> = {
  overview: { label: "概览", icon: "layers", shortcut: "O" },
  diff: { label: "变更", icon: "gitCompare", shortcut: "D" },
  preview: { label: "预览", icon: "globe", shortcut: "B" },
  file: { label: "文件", icon: "fileCode" },
  files: { label: "文件", icon: "folderTree", shortcut: "F" },
  terminal: { label: "终端", icon: "terminal", shortcut: "T" },
  agents: { label: "Agents", icon: "bot", shortcut: "A" },
  plan: { label: "计划", icon: "listTodo", shortcut: "P" },
  "pull-request": { label: "Pull request", icon: "gitPullRequest", shortcut: "R" },
};

export function surfaceMeta(kind: SurfaceKind) {
  return SURFACE_META[kind];
}

export function surfaceTitle(surface: ChatSurface): string {
  if (surface.kind === "preview") {
    if (surface.title) return surface.title;
    if (!surface.url) return "新预览";
    try {
      const parsed = new URL(surface.url);
      return parsed.host || surface.url;
    } catch {
      return surface.url;
    }
  }
  if (surface.kind === "file") return surface.path.split("/").pop() || surface.path;
  return SURFACE_META[surface.kind].label;
}
