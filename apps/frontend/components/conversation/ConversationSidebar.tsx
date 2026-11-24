"use client";


import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { SidebarNav, queryLooksSearchable } from "@/components/chat/sidebar/SidebarNav";
import type { NavItem, NavSection } from "@/components/chat/sidebar/types";
import { Button, Dialog, ResizeHandle, dismissToast, toast } from "@muteki/ui";
import { readChatPreferences } from "@/lib/chatPreferences";
import {
  THREAD_LIFECYCLE_EVENT,
  changeThreadLifecycle,
  deleteConversationThread,
  downloadThreadExport,
  fetchThreadAnnotations,
  mergeThreadWorktree,
  moveThreadToProject,
  regenerateThreadTitle,
  threadActionMessage,
  type ThreadAnnotation,
  type ThreadLifecycleAction,
} from "@/lib/threadActions";
import { TIME_BUCKETS, parseTimestamp, timeBucketLabel, timeBucketOf, type TimeBucket } from "@/components/chat/sidebar/time";
import type {
  ConversationThread,
  ConversationProject,
  ConversationSearchHit,
  SidebarPreferences,
} from "@/lib/useConversation";
import { fetchConversationSearch, fetchSidebarPreferences, saveSidebarPreferences } from "@/lib/useConversation";
import { REFLOW_PANEL_ATTR } from "@muteki/ui/agent";
import {
  clampRailWidth,
  railWidthMax,
  RAIL_WIDTH_DEFAULT,
  RAIL_WIDTH_MAX,
  RAIL_WIDTH_MIN,
} from "@/lib/railSizing";
import { attentionOwnerId, subagentActivityByRoot, subagentPendingByRoot, threadHasActionableApproval, threadHasAttention, threadNeedsAction, type SubagentPending } from "@/lib/threadAttention";
import { buildSidebarActivity, type ActivityPrefs } from "@/lib/sidebarActivity";
import { subagentPendingLabel } from "@/lib/threadNotifications";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { useEngineLogoPreferences } from "@/lib/engineLogoPreference";
import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope } from "@/lib/conversationStorageScope";
import { useConversationLocalPreview } from "@/lib/conversationPreviewMode";
import { mergeSidebarSearchHits, sidebarSearchFailure } from "@/lib/sidebarSearch";
import { rebaseSidebarPreferences, sameSidebarPreferences } from "@/lib/sidebarPreferenceMerge";
import { formatWakeTime, inboxStateOf, nextWakeAt, pruneTimeMap, type InboxState } from "@/lib/sidebarInbox";
import { publishSidebarThreadStates, registerSidebarCommandHandler, type SidebarThreadState } from "@/lib/sidebarThreadBridge";
import type { SidebarBulkActions, SidebarLabels, SidebarProjectFilter } from "@/components/chat/sidebar/types";

const PINNED_THREADS_KEY = "muteki.sidebar.pinned-threads.v1";
const THREAD_ORDER_KEY = "muteki.sidebar.thread-order.v1";
const PROJECT_ORDER_KEY = "muteki.sidebar.project-order.v1";
const PROJECT_FILTER_KEY = "muteki.sidebar.project-filter.v1";

function scopedKey(base: string, scope: "chat" | "src"): string {
  return scope === "src" ? `${base}.src` : base;
}
const PROJECT_THREAD_PREVIEW_LIMIT = 5;
const SETTLED_PAGE_SIZE = 10;
const SETTLED_PAGE_STEP = 25;
const UNDO_WINDOW_MS = 10_000;

type InboxChange =
  | { kind: "settle" }
  | { kind: "unsettle" }
  | { kind: "snooze"; until: number }
  | { kind: "wake" }
  | { kind: "pin" }
  | { kind: "unpin" };

interface InboxSnapshot {
  pinned: boolean;
  pinIndex: number;
  settled?: number;
  snoozed?: number;
}

const EMPTY_PREFERENCES: SidebarPreferences = {
  version: 0, pinned_ids: [], thread_order: [], project_order: [],
  sort_mode: "updated", pinned_sort_mode: "manual", group_mode: "project",
  settled_at: {}, snoozed_until: {},
};

const SETTLED_KEY = "muteki.sidebar.settled-at.v1";
const SNOOZED_KEY = "muteki.sidebar.snoozed-until.v1";

function readTimeMap(key: string): Record<string, number> {
  if (!conversationStorageScope()) return {};
  try {
    const saved = JSON.parse(localStorage.getItem(conversationStorageKey(key)) || "{}") as Record<string, unknown>;
    const result: Record<string, number> = {};
    for (const [id, value] of Object.entries(saved || {})) {
      if (typeof value === "number" && Number.isFinite(value) && value > 0) result[id] = value;
    }
    return result;
  } catch {
    return {};
  }
}

function persistTimeMap(key: string, map: Record<string, number>): void {
  if (!conversationStorageScope()) return;
  try {
    localStorage.setItem(conversationStorageKey(key), JSON.stringify(map));
  } catch {
  }
}

function readProjectFilter(scope: "chat" | "src"): string | null {
  if (!conversationStorageScope()) return null;
  try {
    return localStorage.getItem(conversationStorageKey(scopedKey(PROJECT_FILTER_KEY, scope))) || null;
  } catch {
    return null;
  }
}

function persistProjectFilter(value: string | null, scope: "chat" | "src"): void {
  if (!conversationStorageScope()) return;
  try {
    const key = conversationStorageKey(scopedKey(PROJECT_FILTER_KEY, scope));
    if (value) localStorage.setItem(key, value);
    else localStorage.removeItem(key);
  } catch {
  }
}

function errorTextOf(thread: ConversationThread): string | undefined {
  const error = thread.state.last_error;
  if (!error || !Object.keys(error).length) return undefined;
  const message = error.message ?? error.detail ?? error.code;
  return typeof message === "string" && message.trim() ? message.trim() : "对话异常";
}

function subagentPendingNote(pending: SubagentPending[]): string {
  const [first] = pending;
  const label = subagentPendingLabel({
    title: first.thread.title,
    pending_kind: first.kind === "approval" ? "approval" : "user_input",
  });
  return pending.length > 1 ? `${label}（另有 ${pending.length - 1} 个子代理待处理）` : label;
}

function readStringList(key: string): string[] {
  if (!conversationStorageScope()) return [];
  try {
    const saved = JSON.parse(localStorage.getItem(conversationStorageKey(key)) || "[]");
    return Array.isArray(saved)
      ? saved.filter((id): id is string => typeof id === "string")
      : [];
  } catch {
    return [];
  }
}

function persistStringList(key: string, ids: string[]): void {
  if (!conversationStorageScope()) return;
  try {
    localStorage.setItem(conversationStorageKey(key), JSON.stringify(ids));
  } catch {
  }
}

function mergeOrder(savedIds: string[], visibleIds: string[]): string[] {
  const visible = new Set(visibleIds);
  const result = savedIds.filter((id) => visible.has(id));
  const seen = new Set(result);
  for (const id of visibleIds) {
    if (!seen.has(id)) result.push(id);
  }
  return result;
}

function moveBefore(ids: string[], sourceId: string, targetId: string): string[] {
  if (sourceId === targetId) return ids;
  const sourceIndex = ids.indexOf(sourceId);
  const targetIndex = ids.indexOf(targetId);
  if (sourceIndex < 0 || targetIndex < 0) return ids;
  const next = [...ids];
  const [source] = next.splice(sourceIndex, 1);
  next.splice(targetIndex, 0, source);
  return next;
}

function orderByIds<T>(items: T[], ids: string[], getId: (item: T) => string): T[] {
  const rank = new Map((ids || []).map((id, index) => [id, index]));
  return [...items].sort((a, b) => (
    (rank.get(getId(a)) ?? Number.MAX_SAFE_INTEGER) -
    (rank.get(getId(b)) ?? Number.MAX_SAFE_INTEGER)
  ));
}

interface Cached<T> {
  value: T;
  extra: string;
}

function sameRenderData(a: object, b: object): boolean {
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const keys = Object.keys(left);
  if (keys.length !== Object.keys(right).length) return false;
  for (const key of keys) {
    if (!(key in right)) return false;
    const x = left[key];
    const y = right[key];
    if (typeof x === "function" || typeof y === "function") {
      if (typeof x !== typeof y) return false;
      continue;
    }
    if (Array.isArray(x) && Array.isArray(y)) {
      if (x.length !== y.length || x.some((value, index) => value !== y[index])) return false;
      continue;
    }
    if (!Object.is(x, y)) return false;
  }
  return true;
}

function reuseStable<T extends object>(
  previous: Map<string, Cached<T>>,
  next: Map<string, Cached<T>>,
  key: string,
  value: T,
  extra = "",
): T {
  const cached = next.get(key) ?? previous.get(key);
  const stable = cached && cached.extra === extra && sameRenderData(cached.value, value) ? cached.value : value;
  next.set(key, { value: stable, extra });
  return stable;
}

export type ConversationSidebarLabels = SidebarLabels;

export interface ConversationSidebarProps {
  sidebarId?: string;
  threadHref?: (threadId: string) => string;
  labels?: ConversationSidebarLabels;
  preferencesScope?: "chat" | "src";
  restrictSearchToThreads?: boolean;
  includeSrcInSearch?: boolean;
  projectNavigationOnly?: boolean;
  threads: ConversationThread[];
  projects: ConversationProject[];
  activeThreadId?: string;
  onSelectThread: (threadId: string) => void;
  onNewChat: () => void;
  onNewChatForProject?: (projectId: string) => void;
  onCreateFolder?: () => void;
  onDeleteProject?: (project: ConversationProject) => void;
  onDeleteEmptyProjects?: (projects: ConversationProject[]) => void;
  searchQuery: string;
  onSearchChange: (q: string) => void;
  onRenameThread?: (thread: ConversationThread) => void;
  onForkThread?: (thread: ConversationThread) => void;
  onArchiveThread?: (thread: ConversationThread) => void;
  onArchiveProjectThreads?: (threads: ConversationThread[]) => void;
  onSelectMessageHit?: (threadId: string, messageId: string) => void;
  onMarkThreadsRead?: (threadIds: string[]) => void;
  onThreadDeleted?: (threadId: string) => void;
  onRenameThreadTitle?: (threadId: string, title: string) => void;
  collapsed?: boolean;
  onCollapse?: () => void;
  width: number;
  onWidthChange: (width: number) => void;
  onWidthPreview?: (width: number | null) => void;
  loading?: boolean;
  className?: string;
}

const ACTIVITY_PREFS_KEY = "muteki.sidebar.activity-view.v2";
const ACTIVITY_CLEARED_KEY = "muteki.sidebar.activity-cleared-at.v1";
const ACTIVITY_PRIORITY_KEY = "muteki.sidebar.activity-priority.v1";

const DEFAULT_ACTIVITY_PREFS: ActivityPrefs = { showPriority: true, showRunning: true, showPinned: true };

function readActivityPrefs(scope: "chat" | "src"): ActivityPrefs {
  if (!conversationStorageScope()) return DEFAULT_ACTIVITY_PREFS;
  try {
    const key = scopedKey(ACTIVITY_PREFS_KEY, scope);
    const raw = localStorage.getItem(conversationStorageKey(key))
      || (scope === "chat" ? localStorage.getItem(ACTIVITY_PREFS_KEY) : null)
      || "{}";
    const saved = JSON.parse(raw) as Partial<ActivityPrefs>;
    const pick = (k: keyof ActivityPrefs) => (typeof saved[k] === "boolean" ? saved[k] : DEFAULT_ACTIVITY_PREFS[k]);
    return { showPriority: pick("showPriority"), showRunning: pick("showRunning"), showPinned: pick("showPinned") };
  } catch {
    return DEFAULT_ACTIVITY_PREFS;
  }
}

function persistActivityPrefs(prefs: ActivityPrefs, scope: "chat" | "src"): void {
  if (!conversationStorageScope()) return;
  try {
    localStorage.setItem(conversationStorageKey(scopedKey(ACTIVITY_PREFS_KEY, scope)), JSON.stringify(prefs));
  } catch {
  }
}

function readActivityClearedAt(scope: "chat" | "src"): number {
  try {
    const value = Number(localStorage.getItem(conversationStorageKey(scopedKey(ACTIVITY_CLEARED_KEY, scope))));
    return Number.isFinite(value) && value > 0 ? value : 0;
  } catch {
    return 0;
  }
}

function persistActivityClearedAt(at: number, scope: "chat" | "src"): void {
  try {
    const key = conversationStorageKey(scopedKey(ACTIVITY_CLEARED_KEY, scope));
    if (at > 0) localStorage.setItem(key, String(at));
    else localStorage.removeItem(key);
  } catch {
  }
}

function defaultThreadHref(threadId: string): string {
  return `/chat/${encodeURIComponent(threadId)}`;
}

function threadTimestamp(thread: ConversationThread): number {
  return parseTimestamp(thread.updated_at || thread.created_at);
}

export const ConversationSidebar = memo(function ConversationSidebar({
  sidebarId = "conversation-sidebar",
  threadHref = defaultThreadHref,
  labels,
  preferencesScope = "chat",
  restrictSearchToThreads = false,
  includeSrcInSearch = false,
  projectNavigationOnly = false,
  threads,
  projects,
  activeThreadId,
  onSelectThread,
  onNewChat,
  onNewChatForProject,
  onCreateFolder,
  onDeleteProject,
  onDeleteEmptyProjects,
  searchQuery,
  onSearchChange,
  onRenameThread,
  onForkThread,
  onArchiveThread,
  onArchiveProjectThreads,
  onSelectMessageHit,
  onMarkThreadsRead,
  onThreadDeleted,
  onRenameThreadTitle,
  collapsed = false,
  onCollapse,
  width,
  onWidthChange,
  onWidthPreview,
  loading = false,
  className = "",
}: ConversationSidebarProps) {
  const mobile = useMediaQuery("(max-width: 768px)");
  const localPreview = useConversationLocalPreview();
  const [activityPrefs, setActivityPrefs] = useState<ActivityPrefs>(DEFAULT_ACTIVITY_PREFS);
  const [activityClearedAt, setActivityClearedAt] = useState(0);
  const [activityPriorityIds, setActivityPriorityIds] = useState<string[]>([]);
  const [sortMode, setSortMode] = useState<"updated" | "priority" | "manual">("updated");
  const [pinnedSortMode, setPinnedSortMode] = useState<"updated" | "priority" | "manual">("manual");
  const [groupMode, setGroupMode] = useState<"project" | "list">("project");
  const [pinnedIds, setPinnedIds] = useState<string[]>([]);
  const [threadOrder, setThreadOrder] = useState<string[]>([]);
  const [projectOrder, setProjectOrder] = useState<string[]>([]);
  const [settledAt, setSettledAt] = useState<Record<string, number>>({});
  const [snoozedUntil, setSnoozedUntil] = useState<Record<string, number>>({});
  const [projectFilter, setProjectFilter] = useState<string | null>(null);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [activityView, setActivityView] = useState(false);
  const [resizing, setResizing] = useState(false);
  const [bodyHits, setBodyHits] = useState<ConversationSearchHit[]>([]);
  const [bodyHitsLoading, setBodyHitsLoading] = useState(false);
  const [bodyHitsLoadingMore, setBodyHitsLoadingMore] = useState(false);
  const [bodyHitsError, setBodyHitsError] = useState("");
  const [bodyHitsNextOffset, setBodyHitsNextOffset] = useState<number | null>(null);
  const [searchAttempt, setSearchAttempt] = useState(0);
  const [preferenceScope, setPreferenceScope] = useState(conversationStorageScope);
  const [preferencesReady, setPreferencesReady] = useState(false);
  const [preferencesError, setPreferencesError] = useState("");
  const searchPageRef = useRef<{ query: string; includeSuperseded: boolean; offset: number | null }>({ query: "", includeSuperseded: false, offset: null });
  const [includeSuperseded, setIncludeSuperseded] = useState(false);
  const [maxWidth, setMaxWidth] = useState(RAIL_WIDTH_MAX);
  const [dayStamp, setDayStamp] = useState(() => new Date().toDateString());
  const searchAbortRef = useRef<AbortController | null>(null);
  const searchMoreRequestRef = useRef<AbortController | null>(null);

  const serverSnapshotRef = useRef<SidebarPreferences | null>(null);
  const latestPreferencesRef = useRef<SidebarPreferences>(EMPTY_PREFERENCES);
  const failedPreferencesRef = useRef<SidebarPreferences | null>(null);
  const initialPreferencesRef = useRef(latestPreferencesRef.current);
  const syncInProgressRef = useRef(false);
  const prefsReadyRef = useRef<boolean>(false);
  const serverLoadInProgressRef = useRef<boolean>(false);

  const applyPreferences = useCallback((prefs: SidebarPreferences) => {
    latestPreferencesRef.current = prefs;
    setPinnedIds(prefs.pinned_ids);
    setThreadOrder(prefs.thread_order);
    setProjectOrder(prefs.project_order);
    setSortMode(prefs.sort_mode);
    setPinnedSortMode(prefs.pinned_sort_mode);
    setGroupMode(prefs.group_mode);
    setSettledAt(prefs.settled_at);
    setSnoozedUntil(prefs.snoozed_until);
    persistStringList(scopedKey(PINNED_THREADS_KEY, preferencesScope), prefs.pinned_ids);
    persistStringList(scopedKey(THREAD_ORDER_KEY, preferencesScope), prefs.thread_order);
    persistStringList(scopedKey(PROJECT_ORDER_KEY, preferencesScope), prefs.project_order);
    persistTimeMap(scopedKey(SETTLED_KEY, preferencesScope), prefs.settled_at);
    persistTimeMap(scopedKey(SNOOZED_KEY, preferencesScope), prefs.snoozed_until);
  }, [preferencesScope]);

  const updatePreferences = useCallback((update: (current: SidebarPreferences) => SidebarPreferences) => {
    applyPreferences(update(latestPreferencesRef.current));
  }, [applyPreferences]);

  useEffect(() => subscribeConversationStorageScope(() => {
    setPreferenceScope(conversationStorageScope());
  }), []);

  useEffect(() => {
    setActivityPrefs(readActivityPrefs(preferencesScope));
    setActivityClearedAt(readActivityClearedAt(preferencesScope));
    setActivityPriorityIds(readStringList(scopedKey(ACTIVITY_PRIORITY_KEY, preferencesScope)));
  }, [preferenceScope, preferencesScope]);

  const [annotations, setAnnotations] = useState<Record<string, ThreadAnnotation>>({});
  const annotationsWarnedRef = useRef(false);
  useEffect(() => {
    if (!preferenceScope || localPreview) {
      setAnnotations({});
      return;
    }
    let cancelled = false;
    let loading = false;
    let refreshRequested = false;
    const controller = new AbortController();
    const load = () => {
      if (loading) {
        refreshRequested = true;
        return;
      }
      loading = true;
      void fetchThreadAnnotations(controller.signal).then((rows) => {
        if (!cancelled) setAnnotations(rows);
      }).catch((error) => {
        if (cancelled || annotationsWarnedRef.current) return;
        annotationsWarnedRef.current = true;
        toast({ title: `PR 与自动收尾标注读取失败：${threadActionMessage(error)}`, tone: "warning", duration: 4000 });
      }).finally(() => {
        loading = false;
        if (!cancelled && refreshRequested) {
          refreshRequested = false;
          load();
        }
      });
    };
    load();
    const timer = window.setInterval(load, 60_000);
    window.addEventListener("muteki:threads-changed", load);
    window.addEventListener(THREAD_LIFECYCLE_EVENT, load);
    return () => {
      cancelled = true;
      controller.abort();
      window.clearInterval(timer);
      window.removeEventListener("muteki:threads-changed", load);
      window.removeEventListener(THREAD_LIFECYCLE_EVENT, load);
    };
  }, [preferenceScope, localPreview]);

  useEffect(() => {
    let cancelled = false;
    const ownsScope = () => !cancelled && conversationStorageScope() === preferenceScope;
    const local: SidebarPreferences = {
      ...EMPTY_PREFERENCES,
      pinned_ids: readStringList(scopedKey(PINNED_THREADS_KEY, preferencesScope)),
      thread_order: readStringList(scopedKey(THREAD_ORDER_KEY, preferencesScope)),
      project_order: readStringList(scopedKey(PROJECT_ORDER_KEY, preferencesScope)),
      settled_at: readTimeMap(scopedKey(SETTLED_KEY, preferencesScope)),
      snoozed_until: readTimeMap(scopedKey(SNOOZED_KEY, preferencesScope)),
    };
    setProjectFilter(readProjectFilter(preferencesScope));
    if (local.thread_order.length) local.sort_mode = "manual";
    initialPreferencesRef.current = local;
    setPreferencesError("");
    serverSnapshotRef.current = null;
    failedPreferencesRef.current = null;
    prefsReadyRef.current = false;
    setPreferencesReady(false);
    applyPreferences(local);
    serverLoadInProgressRef.current = true;
    if (!preferenceScope || localPreview) {
      serverLoadInProgressRef.current = false;
      return () => { cancelled = true; };
    }
    void fetchSidebarPreferences(preferencesScope).then((remote) => {
      if (!ownsScope()) return;
      serverSnapshotRef.current = remote;
      const initial = remote.version === 0 ? { ...local, version: remote.version } : remote;
      applyPreferences(rebaseSidebarPreferences(local, latestPreferencesRef.current, initial));
    }).catch((error) => {
      if (!ownsScope()) return;
      setPreferencesError(`侧栏偏好读取失败：${error instanceof Error ? error.message : String(error)}。当前修改仍在此窗口，尚未同步到服务。`);
    }).finally(() => {
      if (!ownsScope()) return;
      serverLoadInProgressRef.current = false;
      prefsReadyRef.current = true;
      setPreferencesReady(true);
    });
    return () => { cancelled = true; };
  }, [applyPreferences, preferenceScope, preferencesScope, localPreview]);

  const syncTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const flushPreferences = useCallback(async () => {
    if (syncInProgressRef.current || !preferenceScope || conversationStorageScope() !== preferenceScope) return;
    syncInProgressRef.current = true;
    setPreferencesError("");
    try {
      for (let attempt = 0; attempt < 3; attempt += 1) {
        const base = serverSnapshotRef.current;
        const local = latestPreferencesRef.current;
        if (!base || sameSidebarPreferences(base, local)) {
          failedPreferencesRef.current = null;
          return;
        }
        const remote = await fetchSidebarPreferences(preferencesScope);
        if (conversationStorageScope() !== preferenceScope) return;
        const merged = rebaseSidebarPreferences(base, local, remote);
        if (sameSidebarPreferences(merged, remote)) {
          serverSnapshotRef.current = remote;
          const next = rebaseSidebarPreferences(local, latestPreferencesRef.current, remote);
          applyPreferences(next);
          if (sameSidebarPreferences(next, remote)) return;
          continue;
        }
        const { ok, prefs } = await saveSidebarPreferences(merged, preferencesScope);
        if (conversationStorageScope() !== preferenceScope) return;
        serverSnapshotRef.current = prefs;
        const next = rebaseSidebarPreferences(
          ok ? local : base, latestPreferencesRef.current, prefs,
        );
        applyPreferences(next);
        if (ok && sameSidebarPreferences(next, prefs)) {
          failedPreferencesRef.current = null;
          return;
        }
      }
      failedPreferencesRef.current = latestPreferencesRef.current;
      setPreferencesError("侧栏偏好尚未保存，请重试。当前操作仍保留在此窗口。");
    } catch (error) {
      if (conversationStorageScope() !== preferenceScope) return;
      failedPreferencesRef.current = latestPreferencesRef.current;
      setPreferencesError(`侧栏偏好保存失败：${error instanceof Error ? error.message : String(error)}。请检查连接后重试。`);
    } finally {
      syncInProgressRef.current = false;
    }
  }, [applyPreferences, preferenceScope, preferencesScope]);

  const retryPreferences = useCallback(async () => {
    if (!preferenceScope || conversationStorageScope() !== preferenceScope || serverLoadInProgressRef.current) return;
    failedPreferencesRef.current = null;
    if (serverSnapshotRef.current) { await flushPreferences(); return; }
    serverLoadInProgressRef.current = true;
    setPreferencesError("");
    try {
      const remote = await fetchSidebarPreferences(preferencesScope);
      if (conversationStorageScope() !== preferenceScope) return;
      serverSnapshotRef.current = remote;
      const base = initialPreferencesRef.current;
      const initial = remote.version === 0 ? { ...base, version: remote.version } : remote;
      applyPreferences(rebaseSidebarPreferences(base, latestPreferencesRef.current, initial));
    } catch (error) {
      if (conversationStorageScope() === preferenceScope) setPreferencesError(`侧栏偏好重试失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      if (conversationStorageScope() === preferenceScope) {
        serverLoadInProgressRef.current = false;
        setPreferencesReady(true);
      }
    }
  }, [applyPreferences, flushPreferences, preferenceScope, preferencesScope]);

  const currentPreferences = useMemo<SidebarPreferences>(() => ({
    version: 0, pinned_ids: pinnedIds, thread_order: threadOrder,
    project_order: projectOrder, sort_mode: sortMode,
    pinned_sort_mode: pinnedSortMode, group_mode: groupMode,
    settled_at: settledAt, snoozed_until: snoozedUntil,
  }), [pinnedIds, threadOrder, projectOrder, sortMode, pinnedSortMode, groupMode, settledAt, snoozedUntil]);
  latestPreferencesRef.current = currentPreferences;
  useEffect(() => {
    if (!prefsReadyRef.current || serverLoadInProgressRef.current) return;
    const base = serverSnapshotRef.current;
    if (!base || sameSidebarPreferences(base, currentPreferences)
        || (failedPreferencesRef.current
          && sameSidebarPreferences(failedPreferencesRef.current, currentPreferences))) return;
    if (syncTimerRef.current) clearTimeout(syncTimerRef.current);
    syncTimerRef.current = setTimeout(() => { void flushPreferences(); }, 400);
    return () => { if (syncTimerRef.current) clearTimeout(syncTimerRef.current); };
  }, [currentPreferences, flushPreferences, preferencesReady]);

  useEffect(() => {
    const measure = () => setMaxWidth(railWidthMax(window.innerWidth));
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, []);

  useEffect(() => {
    const timer = window.setInterval(() => setDayStamp(new Date().toDateString()), 60_000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    const next = nextWakeAt(snoozedUntil, Date.now());
    if (next === null) return;
    const timer = window.setTimeout(() => setNowMs(Date.now()), Math.min(next - Date.now() + 50, 2 ** 31 - 1));
    return () => window.clearTimeout(timer);
  }, [snoozedUntil, nowMs]);

  useEffect(() => {
    const onVisible = () => { if (document.visibilityState === "visible") setNowMs(Date.now()); };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  }, []);

  useEffect(() => {
    searchAbortRef.current?.abort();
    searchAbortRef.current = null;
    setBodyHits([]);
    setBodyHitsError("");
    setBodyHitsNextOffset(null);
    setBodyHitsLoadingMore(false);
    searchMoreRequestRef.current = null;
    searchPageRef.current = { query: searchQuery.trim(), includeSuperseded, offset: null };
    if (!queryLooksSearchable(searchQuery) || !preferenceScope || localPreview) {
      setBodyHitsLoading(false);
      return;
    }
    const controller = new AbortController();
    searchAbortRef.current = controller;
    setBodyHitsLoading(true);
    const timer = window.setTimeout(() => {
      void fetchConversationSearch(searchQuery, { includeArchived: false, includeSuperseded, includeSrc: includeSrcInSearch,
        limit: 20, offset: 0, signal: controller.signal }).then((result) => {
        if (controller.signal.aborted || searchAbortRef.current !== controller
          || conversationStorageScope() !== preferenceScope) return;
        setBodyHits(mergeSidebarSearchHits([], result.hits));
        const offset = result.has_more ? result.next_offset ?? null : null;
        searchPageRef.current.offset = offset;
        setBodyHitsNextOffset(offset);
      }).catch((error) => {
        if (controller.signal.aborted || searchAbortRef.current !== controller) return;
        setBodyHitsError(sidebarSearchFailure(error));
      }).finally(() => {
        if (!controller.signal.aborted && searchAbortRef.current === controller) setBodyHitsLoading(false);
      });
    }, 250);
    return () => { window.clearTimeout(timer); controller.abort(); };
  }, [searchQuery, includeSuperseded, includeSrcInSearch, searchAttempt, preferenceScope, localPreview]);

  const loadMoreSearchHits = useCallback(async () => {
    const controller = searchAbortRef.current;
    const snapshot = { ...searchPageRef.current };
    if (!controller || controller.signal.aborted || snapshot.offset == null || searchMoreRequestRef.current === controller) return;
    searchMoreRequestRef.current = controller;
    setBodyHitsLoadingMore(true);
    setBodyHitsError("");
    try {
      const result = await fetchConversationSearch(snapshot.query, { includeArchived: false,
        includeSuperseded: snapshot.includeSuperseded, includeSrc: includeSrcInSearch, limit: 20, offset: snapshot.offset, signal: controller.signal });
      if (controller.signal.aborted || searchAbortRef.current !== controller
        || conversationStorageScope() !== preferenceScope) return;
      setBodyHits((previous) => mergeSidebarSearchHits(previous, result.hits));
      const offset = result.has_more ? result.next_offset ?? null : null;
      searchPageRef.current.offset = offset;
      setBodyHitsNextOffset(offset);
    } catch (error) {
      if (!controller.signal.aborted && searchAbortRef.current === controller) setBodyHitsError(sidebarSearchFailure(error));
    } finally {
      if (searchMoreRequestRef.current === controller) searchMoreRequestRef.current = null;
      if (!controller.signal.aborted && searchAbortRef.current === controller) setBodyHitsLoadingMore(false);
    }
  }, [preferenceScope, includeSrcInSearch]);

  const asideRef = useRef<HTMLElement | null>(null);
  const resizeDragRef = useRef(false);
  const draftWidthRef = useRef<number | null>(null);
  const rawDragWidthRef = useRef<number | null>(null);

  const onResizeRaw = useCallback((raw: number) => {
    rawDragWidthRef.current = raw;
    if (!resizeDragRef.current) return;
    const aside = asideRef.current;
    const nav = aside?.querySelector<HTMLElement>(":scope > .cx-sidebar-nav");
    if (!aside) return;
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    const maxW = railWidthMax(viewport);
    const shown = Math.max(0, Math.min(maxW, raw));
    aside.style.width = `${shown}px`;
    if (shown < RAIL_WIDTH_MIN) {
      aside.dataset.scrub = "true";
      if (nav) {
        const progress = Math.max(0, Math.min(1, (shown - 88) / (RAIL_WIDTH_MIN - 88)));
        nav.style.width = `${Math.max(RAIL_WIDTH_MIN, width) - 1}px`;
        nav.style.opacity = progress.toFixed(3);
        nav.style.transform = `translateX(${((1 - progress) * -8).toFixed(2)}px)`;
      }
    } else {
      delete aside.dataset.scrub;
      if (nav) {
        nav.style.width = `${shown - 1}px`;
        nav.style.opacity = "";
        nav.style.transform = "";
      }
    }
    onWidthPreview?.(shown);
  }, [onWidthPreview, width]);

  const resizeTo = useCallback((next: number) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    const clamped = clampRailWidth(next, viewport);
    if (!resizeDragRef.current) {
      onWidthChange(clamped);
      return;
    }
    draftWidthRef.current = clamped;
  }, [onWidthChange]);

  const onResizeDrag = useCallback((dragging: boolean) => {
    resizeDragRef.current = dragging;
    if (!dragging) {
      const raw = rawDragWidthRef.current;
      const next = draftWidthRef.current;
      rawDragWidthRef.current = null;
      draftWidthRef.current = null;
      if (raw != null && raw < 140 && onCollapse) onCollapse();
      else if (next != null) onWidthChange(next);
    }
    setResizing(dragging);
  }, [onCollapse, onWidthChange]);

  useLayoutEffect(() => {
    if (resizing) return;
    const aside = asideRef.current;
    if (!aside) return;
    aside.style.width = "";
    delete aside.dataset.scrub;
    const nav = aside.querySelector<HTMLElement>(":scope > .cx-sidebar-nav");
    if (nav) {
      nav.style.removeProperty("width");
      nav.style.removeProperty("opacity");
      nav.style.removeProperty("transform");
    }
    onWidthPreview?.(null);
  }, [resizing, width, collapsed, onWidthPreview]);

  const threadsRef = useRef(threads);
  threadsRef.current = threads;
  const callbacks = {
    onSelectThread, onNewChat, onNewChatForProject, onCreateFolder, onDeleteProject, onDeleteEmptyProjects, onSearchChange,
    onRenameThread, onForkThread, onArchiveThread, onArchiveProjectThreads, onSelectMessageHit, onMarkThreadsRead,
    onThreadDeleted, onRenameThreadTitle,
  };
  const callbacksRef = useRef(callbacks);
  callbacksRef.current = callbacks;
  const hasRename = Boolean(onRenameThread);
  const hasRenameTitle = Boolean(onRenameThreadTitle);
  const hasFork = Boolean(onForkThread);
  const hasArchive = Boolean(onArchiveThread) && !projectNavigationOnly;
  const hasArchiveProject = Boolean(onArchiveProjectThreads) && !projectNavigationOnly;
  const hasNewInProject = Boolean(onNewChatForProject);
  const hasCreateFolder = Boolean(onCreateFolder);
  const hasDeleteProject = Boolean(onDeleteProject);
  const hasDeleteEmptyProjects = Boolean(onDeleteEmptyProjects);
  const serverActions = !localPreview;
  const allowSidebarThreadLifecycle = serverActions && !projectNavigationOnly;
  const liveThreadCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const thread of threads) {
      if (!thread.project_id || thread.state.status === "archived") continue;
      counts.set(thread.project_id, (counts.get(thread.project_id) || 0) + 1);
    }
    return counts;
  }, [threads]);
  const hasMarkRead = Boolean(onMarkThreadsRead);
  const findThread = useCallback((threadId: string) => threadsRef.current.find((thread) => thread.thread_id === threadId), []);
  const viewRef = useRef({
    effectiveProjectFilter: null as string | null,
    listedIds: [] as string[],
    pinnedOrderIds: [] as string[],
    threadsByProject: new Map<string, ConversationThread[]>(),
  });
  const mainItemCacheRef = useRef(new Map<string, Cached<NavItem>>());
  const mainSectionCacheRef = useRef(new Map<string, Cached<NavSection>>());
  const activityItemCacheRef = useRef(new Map<string, Cached<NavItem>>());
  const activitySectionCacheRef = useRef(new Map<string, Cached<NavSection>>());
  const undoRef = useRef<{ snapshot: Map<string, InboxSnapshot>; expires: number; toastId: number } | null>(null);

  const restoreSnapshot = useCallback((snapshot: Map<string, InboxSnapshot>) => {
    updatePreferences((current) => {
      const settled = { ...current.settled_at };
      const snoozed = { ...current.snoozed_until };
      let pinned = [...current.pinned_ids];
      for (const [id, before] of snapshot) {
        if (before.settled) settled[id] = before.settled;
        else delete settled[id];
        if (before.snoozed) snoozed[id] = before.snoozed;
        else delete snoozed[id];
        const has = pinned.includes(id);
        if (before.pinned && !has) pinned.splice(Math.min(before.pinIndex, pinned.length), 0, id);
        if (!before.pinned && has) pinned = pinned.filter((item) => item !== id);
      }
      return { ...current, pinned_ids: pinned, settled_at: settled, snoozed_until: snoozed };
    });
  }, [updatePreferences]);

  const undoLastInboxChange = useCallback((): boolean => {
    const entry = undoRef.current;
    if (!entry || entry.expires < Date.now()) return false;
    undoRef.current = null;
    restoreSnapshot(entry.snapshot);
    toast({ title: "已撤销", tone: "neutral", duration: 1800 });
    return true;
  }, [restoreSnapshot]);

  const applyInboxChange = useCallback((ids: string[], change: InboxChange, options: { announce?: boolean } = {}) => {
    if (!ids.length) return;
    const current = latestPreferencesRef.current;
    const snapshot = new Map<string, InboxSnapshot>();
    for (const id of ids) {
      snapshot.set(id, {
        pinned: current.pinned_ids.includes(id),
        pinIndex: current.pinned_ids.indexOf(id),
        settled: current.settled_at[id],
        snoozed: current.snoozed_until[id],
      });
    }
    const now = Date.now();
    const liveIds = new Set(threadsRef.current.map((thread) => thread.thread_id));
    updatePreferences((prefs) => {
      const settled = pruneTimeMap(prefs.settled_at, liveIds);
      const snoozed = pruneTimeMap(prefs.snoozed_until, liveIds);
      let pinned = prefs.pinned_ids;
      for (const id of ids) {
        switch (change.kind) {
          case "settle":
            settled[id] = now;
            delete snoozed[id];
            pinned = pinned.filter((item) => item !== id);
            break;
          case "unsettle":
            delete settled[id];
            break;
          case "snooze":
            snoozed[id] = change.until;
            delete settled[id];
            break;
          case "wake":
            delete snoozed[id];
            break;
          case "pin":
            if (!pinned.includes(id)) pinned = [...pinned, id];
            delete settled[id];
            delete snoozed[id];
            break;
          case "unpin":
            pinned = pinned.filter((item) => item !== id);
            break;
          default: {
            const exhaustive: never = change;
            return exhaustive;
          }
        }
      }
      return { ...prefs, pinned_ids: pinned, settled_at: settled, snoozed_until: snoozed };
    });
    setNowMs(now);
    if (options.announce === false) return;
    const subject = ids.length === 1
      ? `「${threadsRef.current.find((thread) => thread.thread_id === ids[0])?.title || "未命名对话"}」`
      : `${ids.length} 个对话`;
    const title = change.kind === "settle" ? `已归置${subject}`
      : change.kind === "unsettle" ? `已将${subject}移回列表`
        : change.kind === "snooze" ? `${subject}将在${formatWakeTime(change.until)}提醒`
          : change.kind === "wake" ? `已唤醒${subject}`
            : change.kind === "pin" ? `已置顶${subject}`
              : `已取消置顶${subject}`;
    if (undoRef.current) {
      dismissToast(undoRef.current.toastId);
      undoRef.current = null;
    }
    const toastId = toast({
      title,
      icon: change.kind === "snooze" ? "clock" : change.kind === "settle" ? "checkCheck" : undefined,
      duration: 5000,
      action: { label: "撤销", onClick: () => { undoLastInboxChange(); } },
    });
    undoRef.current = { snapshot, expires: Date.now() + UNDO_WINDOW_MS, toastId };
  }, [undoLastInboxChange, updatePreferences]);

  const snoozeThread = useCallback((threadId: string, until: number | null) => {
    applyInboxChange([threadId], until === null ? { kind: "wake" } : { kind: "snooze", until });
  }, [applyInboxChange]);


  const titleOf = useCallback((threadId: string) => (
    threadsRef.current.find((thread) => thread.thread_id === threadId)?.title || "未命名对话"
  ), []);
  const reportFailure = useCallback((prefix: string, error: unknown) => {
    toast({ title: `${prefix}：${threadActionMessage(error)}`, tone: "danger", duration: 6000 });
  }, []);

  const runLifecycleOps = useCallback(async (ops: { id: string; action: ThreadLifecycleAction; on?: boolean }[]) => {
    const failed: string[] = [];
    const done: { id: string; action: ThreadLifecycleAction }[] = [];
    for (const op of ops) {
      try {
        await changeThreadLifecycle(op.id, op.action, op.on === undefined ? {} : { on: op.on });
        done.push(op);
      } catch (error) {
        failed.push(`${titleOf(op.id)}：${threadActionMessage(error)}`);
      }
    }
    return { done, failed };
  }, [titleOf]);

  const serverUndoRef = useRef<{ ops: { id: string; action: ThreadLifecycleAction }[]; expires: number; toastId: number } | null>(null);
  const undoServerChange = useCallback(async (): Promise<boolean> => {
    const entry = serverUndoRef.current;
    if (!entry || entry.expires < Date.now()) return false;
    serverUndoRef.current = null;
    const { failed } = await runLifecycleOps(entry.ops);
    if (failed.length) {
      toast({ title: "撤销未完成", description: failed.join("\n"), tone: "danger", duration: 6000 });
    } else {
      toast({ title: "已撤销", tone: "neutral", duration: 1800 });
    }
    return true;
  }, [runLifecycleOps]);

  const applyServerChange = useCallback((ids: string[], change: "settle" | "unsettle" | "pin" | "unpin") => {
    if (!ids.length) return;
    updatePreferences((prefs) => {
      const settled = { ...prefs.settled_at };
      const snoozed = { ...prefs.snoozed_until };
      let pinned = prefs.pinned_ids;
      for (const id of ids) {
        delete settled[id];
        if (change !== "unpin") delete snoozed[id];
        if (change === "pin") {
          if (!pinned.includes(id)) pinned = [...pinned, id];
        } else {
          pinned = pinned.filter((item) => item !== id);
        }
      }
      return { ...prefs, pinned_ids: pinned, settled_at: settled, snoozed_until: snoozed };
    });
    setNowMs(Date.now());
    void runLifecycleOps(ids.map((id) => ({ id, action: change }))).then(({ done, failed }) => {
      if (done.length) {
        const subject = done.length === 1 ? `「${titleOf(done[0].id)}」` : `${done.length} 个对话`;
        const title = change === "settle" ? `已收尾${subject}`
          : change === "unsettle" ? `已将${subject}移回列表`
            : change === "pin" ? `已置顶${subject}`
              : `已取消置顶${subject}`;
        if (serverUndoRef.current) dismissToast(serverUndoRef.current.toastId);
        const reverse: ThreadLifecycleAction = change === "settle" ? "unsettle"
          : change === "unsettle" ? "settle"
            : change === "pin" ? "unpin" : "pin";
        const toastId = toast({
          title,
          description: change === "settle"
            ? "撤销只取消收尾，不会重启已停止的会话、已结束的 PR 关注、已丢弃的排队消息或已关闭的终端。"
            : undefined,
          icon: change === "settle" ? "checkCheck" : undefined,
          duration: 5000,
          action: { label: "撤销", onClick: () => { void undoServerChange(); } },
        });
        serverUndoRef.current = { ops: done.map((op) => ({ id: op.id, action: reverse })), expires: Date.now() + UNDO_WINDOW_MS, toastId };
      }
      if (failed.length) {
        toast({
          title: failed.length === 1 ? failed[0] : `${failed.length} 个对话未能${change === "settle" ? "收尾" : "更新"}`,
          description: failed.length > 1 ? failed.join("\n") : undefined,
          tone: "danger",
          duration: 6000,
        });
      }
    });
  }, [runLifecycleOps, titleOf, undoServerChange, updatePreferences]);

  const togglePinned = useCallback((threadId: string) => {
    const thread = threadsRef.current.find((item) => item.thread_id === threadId);
    const pinned = thread?.control_lifecycle
      ? Boolean(thread.control_lifecycle.pinned)
      : latestPreferencesRef.current.pinned_ids.includes(threadId);
    applyServerChange([threadId], pinned ? "unpin" : "pin");
  }, [applyServerChange]);

  const setThreadSettled = useCallback((threadId: string, settle: boolean) => {
    applyServerChange([threadId], settle ? "settle" : "unsettle");
  }, [applyServerChange]);

  const toggleReadState = useCallback((threadId: string) => {
    const thread = threadsRef.current.find((item) => item.thread_id === threadId);
    if (!thread) return;
    const unread = Boolean(thread.state.unread);
    void changeThreadLifecycle(threadId, unread ? "mark_read" : "mark_unread")
      .then(() => toast({ title: unread ? `已将「${titleOf(threadId)}」标为已读` : `已将「${titleOf(threadId)}」标记为未读`, duration: 2000 }))
      .catch((error) => reportFailure("更新未读状态失败", error));
  }, [reportFailure, titleOf]);

  const toggleFollowUp = useCallback((threadId: string) => {
    const lifecycle = threadsRef.current.find((item) => item.thread_id === threadId)?.control_lifecycle;
    const on = !lifecycle?.follow_up;
    void changeThreadLifecycle(threadId, "follow_up", { on })
      .then(() => toast({ title: on ? `已将「${titleOf(threadId)}」加入待跟进` : `已将「${titleOf(threadId)}」移出待跟进`, duration: 2000 }))
      .catch((error) => reportFailure("更新待跟进失败", error));
  }, [reportFailure, titleOf]);

  const toggleParked = useCallback((threadId: string) => {
    const lifecycle = threadsRef.current.find((item) => item.thread_id === threadId)?.control_lifecycle;
    const on = !lifecycle?.parked;
    void changeThreadLifecycle(threadId, "park", { on })
      .then(() => toast({ title: on ? `已搁置「${titleOf(threadId)}」` : `已取消搁置「${titleOf(threadId)}」`, duration: 2000 }))
      .catch((error) => reportFailure("更新搁置状态失败", error));
  }, [reportFailure, titleOf]);

  const changeAutoSettle = useCallback((threadId: string, disabled: boolean) => {
    void changeThreadLifecycle(threadId, "keep", { on: disabled })
      .then(() => toast({ title: disabled ? "已停用此对话的自动收尾" : "已启用此对话的自动收尾", duration: 2000 }))
      .catch((error) => reportFailure("更新自动收尾失败", error));
  }, [reportFailure]);

  const exportBusyRef = useRef(new Set<string>());
  const exportThread = useCallback((threadId: string, format: "markdown" | "jsonl") => {
    if (exportBusyRef.current.has(threadId)) return;
    exportBusyRef.current.add(threadId);
    void downloadThreadExport(threadId, titleOf(threadId), format)
      .then(() => toast({ title: format === "jsonl" ? "已导出 JSONL" : "已导出 Markdown", icon: "download", duration: 2500 }))
      .catch((error) => reportFailure("导出失败", error))
      .finally(() => exportBusyRef.current.delete(threadId));
  }, [reportFailure, titleOf]);

  const mergeWorktreeBack = useCallback((threadId: string) => {
    void mergeThreadWorktree(threadId)
      .then((result) => toast({
        title: result.message || "已合并回项目目录",
        tone: result.outcome === "merged" ? "success" : "neutral",
        duration: 5000,
      }))
      .catch((error) => reportFailure("合并回项目目录失败", error));
  }, [reportFailure]);

  const regenerateTitleFor = useCallback((threadId: string) => {
    void regenerateThreadTitle(threadId)
      .then((title) => toast({ title: title ? `已生成新标题：${title}` : "正在重新生成标题", duration: 3000 }))
      .catch((error) => reportFailure("重新生成标题失败", error));
  }, [reportFailure]);

  const moveToProject = useCallback((threadId: string, projectId: string) => {
    void moveThreadToProject(threadId, projectId)
      .then((result) => toast({
        title: result.changed ? `已移动到「${result.project_name || "项目"}」，下次运行在新目录继续` : "对话已在此项目中",
        tone: "success",
        duration: 3000,
      }))
      .catch((error) => reportFailure("移动对话失败", error));
  }, [reportFailure]);

  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const performDeleteThread = useCallback(async (threadId: string) => {
    setDeleting(true);
    try {
      await deleteConversationThread(threadId);
      toast({ title: `已删除「${titleOf(threadId)}」`, duration: 2500 });
      setDeleteTarget(null);
      callbacksRef.current.onThreadDeleted?.(threadId);
    } catch (error) {
      reportFailure("删除对话失败", error);
    } finally {
      setDeleting(false);
    }
  }, [reportFailure, titleOf]);
  const requestDeleteThread = useCallback((threadId: string) => {
    if ((readChatPreferences() as { skipDeleteConfirmation?: boolean }).skipDeleteConfirmation) {
      void performDeleteThread(threadId);
      return;
    }
    setDeleteTarget(threadId);
  }, [performDeleteThread]);

  const changeProjectFilter = useCallback((value: string | null) => {
    setProjectFilter(value);
    persistProjectFilter(value, preferencesScope);
  }, [preferencesScope]);

  const moveThread = useCallback((sourceId: string, targetId: string, visibleIds: string[]) => {
    updatePreferences((current) => ({ ...current, sort_mode: "manual", thread_order: moveBefore(
      [...visibleIds], sourceId, targetId) }));
  }, [updatePreferences]);

  const movePinnedThread = useCallback((sourceId: string, targetId: string, visibleIds: string[]) => {
    updatePreferences((current) => ({ ...current, pinned_sort_mode: "manual", pinned_ids: moveBefore(
      current.pinned_sort_mode === "manual" ? mergeOrder(current.pinned_ids, visibleIds) : [...visibleIds], sourceId, targetId) }));
  }, [updatePreferences]);

  const reorderListed = useCallback((sourceId: string, targetId: string) => {
    moveThread(sourceId, targetId, viewRef.current.listedIds);
  }, [moveThread]);

  const reorderPinned = useCallback((sourceId: string, targetId: string) => {
    movePinnedThread(sourceId, targetId, viewRef.current.pinnedOrderIds);
  }, [movePinnedThread]);

  const orderedProjects = useMemo(() => orderByIds(projects, projectOrder, (project) => project.project_id), [projects, projectOrder]);
  const orderedProjectsRef = useRef(orderedProjects);
  orderedProjectsRef.current = orderedProjects;
  const moveProject = useCallback((sourceSectionId: string, targetSectionId: string) => {
    const sourceId = sourceSectionId.replace(/^project:/, "");
    const targetId = targetSectionId.replace(/^project:/, "");
    const visibleIds = orderedProjectsRef.current.map((project) => project.project_id);
    updatePreferences((current) => ({ ...current, project_order: moveBefore(mergeOrder(current.project_order, visibleIds), sourceId, targetId) }));
  }, [updatePreferences]);
  const changePinnedSortMode = useCallback((next: SidebarPreferences["pinned_sort_mode"]) => updatePreferences((current) => ({ ...current, pinned_sort_mode: next })), [updatePreferences]);
  const changeSortMode = useCallback((next: SidebarPreferences["sort_mode"]) => updatePreferences((current) => ({ ...current, sort_mode: next })), [updatePreferences]);
  const changeGroupMode = useCallback((next: SidebarPreferences["group_mode"]) => updatePreferences((current) => ({ ...current, group_mode: next })), [updatePreferences]);

  const threadsById = useMemo(() => new Map(threads.map((thread) => [thread.thread_id, thread])), [threads]);
  const subagentActivity = useMemo(() => subagentActivityByRoot(threads), [threads]);

  const activeThreads = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    const matchingThreads = threads.filter((thread) => (
      !q || `${thread.title} ${thread.summary || ""} ${thread.thread_id}`.toLowerCase().includes(q)
    ));
    const manualRank = sortMode === "manual" ? new Map(threadOrder.map((id, index) => [id, index])) : null;
    const sortThreads = (items: ConversationThread[]) => items.sort((a, b) => {
      if (sortMode === "priority") {
        const rank = (thread: ConversationThread) => (
          thread.state.running_turn_id || subagentActivity.get(thread.thread_id)?.active ? 0
            : threadNeedsAction(thread) ? 1
              : thread.state.unread ? 2
                : 3
        );
        return rank(a) - rank(b);
      }
      if (manualRank) {
        return (manualRank.get(a.thread_id) ?? -1) -
          (manualRank.get(b.thread_id) ?? -1) || threadTimestamp(b) - threadTimestamp(a);
      }
      return String(b.updated_at || b.created_at || "").localeCompare(String(a.updated_at || a.created_at || ""));
    });

    return sortThreads(matchingThreads.filter((thread) => thread.state.status !== "archived"));
  }, [threads, searchQuery, sortMode, threadOrder, subagentActivity]);

  const projectsById = useMemo(() => new Map(projects.map((project) => [project.project_id, project])), [projects]);
  const effectiveProjectFilter = projectFilter && projectsById.has(projectFilter) ? projectFilter : null;

  const subagentPending = useMemo(() => subagentPendingByRoot(threads), [threads]);

  const inboxStates = useMemo(() => {
    const states = new Map<string, InboxState>();
    for (const thread of threads) {
      states.set(thread.thread_id, inboxStateOf(thread, settledAt, snoozedUntil, nowMs, subagentPending.has(thread.thread_id)));
    }
    return states;
  }, [threads, settledAt, snoozedUntil, nowMs, subagentPending]);

  useEffect(() => {
    if (!activeThreadId || !inboxStates.get(activeThreadId)?.woke) return;
    applyInboxChange([activeThreadId], { kind: "wake" }, { announce: false });
  }, [activeThreadId, inboxStates, applyInboxChange]);

  const effectivePinnedIds = useMemo(() => {
    const pinned = threads.filter((thread) => (
      thread.control_lifecycle ? Boolean(thread.control_lifecycle.pinned) : pinnedIds.includes(thread.thread_id)
    )).map((thread) => thread.thread_id);
    const ordered = pinnedIds.filter((id) => pinned.includes(id));
    for (const id of pinned) if (!ordered.includes(id)) ordered.push(id);
    return ordered;
  }, [threads, pinnedIds]);
  const pinnedSet = useMemo(() => new Set(effectivePinnedIds), [effectivePinnedIds]);

  useEffect(() => {
    const next = new Map<string, SidebarThreadState>();
    for (const [id, state] of inboxStates) {
      next.set(id, { pinned: pinnedSet.has(id), placement: state.placement, snoozedUntil: state.snoozedUntil });
    }
    publishSidebarThreadStates(next, sidebarId);
  }, [inboxStates, pinnedSet, sidebarId]);

  useEffect(() => registerSidebarCommandHandler((threadId, command) => {
    if (command.kind === "filter-project") changeProjectFilter(command.projectId === effectiveProjectFilter ? null : command.projectId);
    else if (command.kind === "snooze") applyInboxChange([threadId], { kind: "snooze", until: command.until });
    else if (command.kind === "wake") applyInboxChange([threadId], { kind: "wake" });
    else applyServerChange([threadId], command.kind);
  }, sidebarId), [applyInboxChange, applyServerChange, changeProjectFilter, effectiveProjectFilter, sidebarId]);

  const { sidebarModelIcon } = useEngineLogoPreferences();
  const toItem = useCallback((t: ConversationThread): NavItem => {
    const threadId = t.thread_id;
    const inbox = inboxStates.get(t.thread_id);
    const lifecycle = t.control_lifecycle;
    const project = t.project_id ? projectsById.get(t.project_id) : undefined;
    const placement = inbox?.placement ?? "active";
    const ownNeedsAction = threadNeedsAction(t);
    const delegated = ownNeedsAction ? undefined : subagentPending.get(t.thread_id);
    const needsAction = ownNeedsAction || Boolean(delegated?.length);
    return {
      id: t.thread_id,
      label: t.title || `未命名${labels?.thread ?? "对话"}`,
      href: threadHref(t.thread_id),
      updatedAt: t.updated_at || t.created_at,
      engine: sidebarModelIcon ? t.runtime?.adapter_id || "" : undefined,
      status: t.state.status,
      running: Boolean(t.state.running_turn_id),
      unread: Boolean(t.state.unread) && t.thread_id !== activeThreadId,
      needsAction,
      pending: ownNeedsAction ? (threadHasActionableApproval(t) ? "approval" : "input") : delegated?.[0]?.kind,
      pendingNote: delegated?.length ? subagentPendingNote(delegated) : undefined,
      archived: t.state.status === "archived",
      failed: Boolean(t.state.last_error && Object.keys(t.state.last_error).length),
      errorText: errorTextOf(t),
      queueCount: t.state.queue_count || undefined,
      agentsActive: subagentActivity.get(threadId)?.active || undefined,
      agentsTotal: subagentActivity.get(threadId)?.total || undefined,
      pinned: pinnedSet.has(t.thread_id),
      projectId: project?.project_id,
      projectName: project?.name,
      projectPath: project?.root_path || undefined,
      summary: t.summary || undefined,
      woke: Boolean(inbox?.woke),
      snoozedUntil: placement === "snoozed" ? inbox?.snoozedUntil : undefined,
      settled: placement === "settled",
      onPin: () => togglePinned(threadId),
      onSettle: () => setThreadSettled(threadId, placement !== "settled"),
      onSnooze: (until) => snoozeThread(threadId, until),
      onRename: hasRename ? () => {
        const thread = findThread(threadId);
        if (thread) callbacksRef.current.onRenameThread?.(thread);
      } : undefined,
      onRenameCommit: hasRenameTitle ? (title) => callbacksRef.current.onRenameThreadTitle?.(threadId, title) : undefined,
      onFork: hasFork ? () => {
        const thread = findThread(threadId);
        if (thread) callbacksRef.current.onForkThread?.(thread);
      } : undefined,
      onArchive: hasArchive ? () => {
        const thread = findThread(threadId);
        if (thread) callbacksRef.current.onArchiveThread?.(thread);
      } : undefined,
      followUp: Boolean(lifecycle?.follow_up),
      followUpNote: lifecycle?.follow_up_note || undefined,
      parked: Boolean(lifecycle?.parked),
      autoSettleDisabled: annotations[t.thread_id]?.auto_settle_disabled,
      pullRequests: annotations[t.thread_id]?.pull_requests?.length
        ? annotations[t.thread_id].pull_requests.map((link) => ({
          number: link.number, url: link.url, state: link.state, title: link.title, repository: link.repository,
        }))
        : undefined,
      worktree: t.state.lineage?.isolation === "worktree",
      onMarkReadToggle: serverActions ? () => toggleReadState(threadId) : undefined,
      onFollowUpToggle: serverActions ? () => toggleFollowUp(threadId) : undefined,
      onParkToggle: serverActions ? () => toggleParked(threadId) : undefined,
      onAutoSettleChange: serverActions ? (disabled) => changeAutoSettle(threadId, disabled) : undefined,
      onExport: serverActions ? (format) => exportThread(threadId, format) : undefined,
      onMergeWorktree: serverActions && t.state.lineage?.isolation === "worktree" ? () => mergeWorktreeBack(threadId) : undefined,
      onRegenerateTitle: serverActions ? () => regenerateTitleFor(threadId) : undefined,
      onMoveToProject: serverActions && !projectNavigationOnly && t.state.lineage?.isolation !== "worktree" && projects.length > 1
        ? (projectId) => moveToProject(threadId, projectId)
        : undefined,
      onDelete: allowSidebarThreadLifecycle ? () => requestDeleteThread(threadId) : undefined,
      onNewInProject: project && hasNewInProject ? () => callbacksRef.current.onNewChatForProject?.(project.project_id) : undefined,
      onFilterProject: project ? () => changeProjectFilter(viewRef.current.effectiveProjectFilter === project.project_id ? null : project.project_id) : undefined,
    };
  }, [inboxStates, subagentPending, subagentActivity, projectsById, activeThreadId, pinnedSet, togglePinned, setThreadSettled, snoozeThread, hasRename, hasRenameTitle, hasFork, hasArchive, hasNewInProject, findThread, changeProjectFilter, threadHref, labels?.thread, sidebarModelIcon, annotations, projects.length, serverActions, allowSidebarThreadLifecycle, projectNavigationOnly, toggleReadState, toggleFollowUp, toggleParked, changeAutoSettle, exportThread, mergeWorktreeBack, regenerateTitleFor, moveToProject, requestDeleteThread]);

  const mainView = useMemo(() => {
    const previousItems = mainItemCacheRef.current;
    const nextItems = new Map<string, Cached<NavItem>>();
    const previousSections = mainSectionCacheRef.current;
    const nextSections = new Map<string, Cached<NavSection>>();
    mainItemCacheRef.current = nextItems;
    mainSectionCacheRef.current = nextSections;
    const stableItem = (thread: ConversationThread) => reuseStable(previousItems, nextItems, thread.thread_id, toItem(thread));
    const stableSection = (section: NavSection, extra = "") => reuseStable(previousSections, nextSections, section.id || section.title, section, extra);

    const scoped = effectiveProjectFilter
      ? activeThreads.filter((thread) => thread.project_id === effectiveProjectFilter)
      : activeThreads;
    const searching = Boolean(searchQuery.trim());
    const busy = (thread: ConversationThread) => Boolean(thread.state.running_turn_id || subagentActivity.get(thread.thread_id)?.active);
    const topLevel = scoped.filter((thread) => !attentionOwnerId(thread, threadsById));
    const childMatches = searching ? scoped.filter((thread) => attentionOwnerId(thread, threadsById)) : [];
    const subagentMatches: NavSection[] = childMatches.length
      ? [{ id: "subagent-matches", title: `子代理 · ${childMatches.length}`, kind: "section", items: childMatches.map(stableItem) }]
      : [];
    const placementOf = (thread: ConversationThread) => inboxStates.get(thread.thread_id)?.placement ?? "active";
    const listed = topLevel.filter((thread) => placementOf(thread) === "active");
    const snoozed = topLevel.filter((thread) => placementOf(thread) === "snoozed")
      .sort((a, b) => (inboxStates.get(a.thread_id)?.snoozedUntil ?? 0) - (inboxStates.get(b.thread_id)?.snoozedUntil ?? 0));
    const settled = topLevel.filter((thread) => placementOf(thread) === "settled")
      .sort((a, b) => (inboxStates.get(b.thread_id)?.settledAt ?? 0) - (inboxStates.get(a.thread_id)?.settledAt ?? 0));
    const pinnedRank = new Map(effectivePinnedIds.map((id, index) => [id, index]));
    const pinned = listed.filter((t) => pinnedSet.has(t.thread_id)).sort((a, b) => {
      if (pinnedSortMode === "manual") return (pinnedRank.get(a.thread_id) ?? 0) - (pinnedRank.get(b.thread_id) ?? 0);
      if (pinnedSortMode === "priority") return Number(busy(b)) - Number(busy(a));
      return String(b.updated_at || b.created_at || "").localeCompare(String(a.updated_at || a.created_at || ""));
    });
    const ordinary = listed.filter((t) => !pinnedSet.has(t.thread_id));

    const result: NavSection[] = [];
    const sectionExtras = new Map<string, string>();

    const tail: NavSection[] = [];
    if (snoozed.length) {
      tail.push({ id: "snoozed", title: `稍后提醒 · ${snoozed.length}`, kind: "snoozed", defaultCollapsed: true, items: snoozed.map(stableItem) });
    }
    if (settled.length) {
      tail.push({
        id: "settled",
        title: `已归置 · ${settled.length}`,
        kind: "settled",
        defaultCollapsed: true,
        pageSize: SETTLED_PAGE_SIZE,
        pageStep: SETTLED_PAGE_STEP,
        items: settled.map(stableItem),
      });
    }

    const listedIds = listed.map((thread) => thread.thread_id);
    const pinnedOrderIds = pinned.map((thread) => thread.thread_id);

    const toItems = (items: ConversationThread[], group: "listed" | "pinned") => {
      const reorder = group === "pinned" ? reorderPinned : reorderListed;
      return items.map((thread, index): NavItem => {
        const threadId = thread.thread_id;
        const previousId = index > 0 ? items[index - 1].thread_id : undefined;
        const nextId = index < items.length - 1 ? items[index + 1].thread_id : undefined;
        return reuseStable(previousItems, nextItems, threadId, {
          ...toItem(thread),
          onMoveUp: previousId ? () => reorder(threadId, previousId) : undefined,
          onMoveDown: nextId ? () => reorder(threadId, nextId) : undefined,
        }, `${group}|${previousId ?? ""}|${nextId ?? ""}`);
      });
    };

    const chronological = (idPrefix: string, title: string, items: ConversationThread[]): NavSection[] => {
      if (sortMode !== "updated") {
        return [{ id: idPrefix, title, kind: "section", items: toItems(items, "listed"), onReorderItems: reorderListed }];
      }
      const now = new Date();
      const buckets = new Map<TimeBucket, ConversationThread[]>();
      for (const thread of items) {
        const bucket = timeBucketOf(threadTimestamp(thread), now);
        const list = buckets.get(bucket);
        if (list) list.push(thread);
        else buckets.set(bucket, [thread]);
      }
      return TIME_BUCKETS.filter((bucket) => buckets.has(bucket)).map((bucket) => ({
        id: `${idPrefix}:${bucket}`,
        title: timeBucketLabel(bucket),
        kind: "time",
        items: toItems(buckets.get(bucket) || [], "listed"),
        onReorderItems: reorderListed,
      }));
    };

    const pinnedSection: NavSection = {
      id: "pinned",
      title: "置顶",
      kind: "pinned",
      items: toItems(pinned, "pinned"),
      sortMode: pinnedSortMode,
      onSortChange: changePinnedSortMode,
      onReorderItems: reorderPinned,
    };

    const threadsByProject = new Map<string, ConversationThread[]>();

    if (groupMode === "list") {
      result.push(pinnedSection, ...chronological("all", `全部${labels?.thread ?? "对话"}`, ordinary), ...subagentMatches, ...tail);
    } else {
      result.push(pinnedSection);

      for (const thread of ordinary) {
        if (!thread.project_id) continue;
        const list = threadsByProject.get(thread.project_id);
        if (list) list.push(thread);
        else threadsByProject.set(thread.project_id, [thread]);
      }

      for (const [projectIndex, project] of orderedProjects.entries()) {
        if (effectiveProjectFilter && project.project_id !== effectiveProjectFilter) continue;
        const projectId = project.project_id;
        const projectThreads = threadsByProject.get(projectId) ?? [];
        if (searching && !projectThreads.length) continue;
        const previousProjectId = projectIndex > 0 ? orderedProjects[projectIndex - 1].project_id : undefined;
        const nextProjectId = projectIndex < orderedProjects.length - 1 ? orderedProjects[projectIndex + 1].project_id : undefined;
        const sectionId = `project:${projectId}`;
        sectionExtras.set(sectionId, `${previousProjectId ?? ""}|${nextProjectId ?? ""}`);
        result.push({
          id: sectionId,
          title: project.name,
          subtitle: project.root_path || (projectNavigationOnly ? "SRC 研究项目（无本地目录）" : "独立研究项目（无本地工作目录）"),
          kind: "folder",
          previewLimit: PROJECT_THREAD_PREVIEW_LIMIT,
          emptyLabel: `暂无${labels?.thread ?? "对话"}`,
          running: projectThreads.some(busy),
          onNewChat: hasNewInProject ? () => callbacksRef.current.onNewChatForProject?.(projectId) : undefined,
          onArchiveAll: hasArchiveProject || hasArchive
            ? () => {
              const current = viewRef.current.threadsByProject.get(projectId) ?? [];
              const { onArchiveProjectThreads: archiveAll, onArchiveThread: archiveOne } = callbacksRef.current;
              if (archiveAll) archiveAll(current);
              else if (archiveOne) current.forEach(archiveOne);
            }
            : undefined,
          onDelete: hasDeleteProject
            ? () => {
              const current = orderedProjectsRef.current.find((row) => row.project_id === projectId);
              if (current) callbacksRef.current.onDeleteProject?.(current);
            }
            : undefined,
          liveThreadCount: liveThreadCounts.get(projectId) || 0,
          onMoveUp: previousProjectId ? () => moveProject(sectionId, `project:${previousProjectId}`) : undefined,
          onMoveDown: nextProjectId ? () => moveProject(sectionId, `project:${nextProjectId}`) : undefined,
          onReorderSection: moveProject,
          items: toItems(projectThreads, "listed"),
          onReorderItems: reorderListed,
        });
      }

      const projectIds = new Set(orderedProjects.map((project) => project.project_id));
      const looseThreads = ordinary.filter((thread) => !thread.project_id || !projectIds.has(thread.project_id));
      if (!effectiveProjectFilter) result.push(...chronological("recent", `最近${labels?.thread ?? "对话"}`, looseThreads));
      result.push(...subagentMatches);
      result.push(...tail);
    }

    return {
      sections: result.map((section) => stableSection(section, sectionExtras.get(section.id || "") ?? "")),
      listedIds,
      pinnedOrderIds,
      threadsByProject,
      childRowIds: new Set(childMatches.map((thread) => thread.thread_id)),
    };
  }, [activeThreads, threadsById, subagentActivity, inboxStates, toItem, effectiveProjectFilter, groupMode, sortMode, searchQuery, dayStamp, orderedProjects, hasArchiveProject, hasArchive, hasNewInProject, hasDeleteProject, projectNavigationOnly, liveThreadCounts, effectivePinnedIds, pinnedSet, pinnedSortMode, reorderListed, reorderPinned, moveProject, changePinnedSortMode, labels?.thread]);
  const sections = mainView.sections;
  viewRef.current = {
    effectiveProjectFilter,
    listedIds: mainView.listedIds,
    pinnedOrderIds: mainView.pinnedOrderIds,
    threadsByProject: mainView.threadsByProject,
  };

  const navActiveId = useMemo(() => {
    const active = activeThreadId ? threadsById.get(activeThreadId) : undefined;
    if (!active || (!activityView && mainView.childRowIds.has(active.thread_id))) return activeThreadId;
    return attentionOwnerId(active, threadsById) || activeThreadId;
  }, [activeThreadId, threadsById, activityView, mainView.childRowIds]);

  useEffect(() => {
    if (!activityView || !preferenceScope || loading) return;
    const byId = new Map(threads.map((thread) => [thread.thread_id, thread]));
    const active = activeThreadId ? byId.get(activeThreadId) : undefined;
    const activeOwner = active ? attentionOwnerId(active, byId) || active.thread_id : "";
    const eligible = threads.filter((thread) => thread.state.status !== "archived" && !attentionOwnerId(thread, byId));
    const eligibleIds = new Set(eligible.map((thread) => thread.thread_id));
    setActivityPriorityIds((previous) => {
      const next = new Set(previous.filter((id) => eligibleIds.has(id)));
      for (const thread of eligible) {
        if (thread.thread_id === activeOwner || threadHasAttention(thread) || thread.state.running_turn_id || subagentPending.has(thread.thread_id) || subagentActivity.get(thread.thread_id)?.active) next.add(thread.thread_id);
      }
      if (next.size === previous.length && previous.every((id) => next.has(id))) return previous;
      const ids = [...next];
      persistStringList(scopedKey(ACTIVITY_PRIORITY_KEY, preferencesScope), ids);
      return ids;
    });
  }, [activityView, preferenceScope, preferencesScope, threads, activeThreadId, subagentPending, subagentActivity, loading]);

  const inbox = useMemo(() => {
    const previousItems = activityItemCacheRef.current;
    const nextItems = new Map<string, Cached<NavItem>>();
    const previousSections = activitySectionCacheRef.current;
    const nextSections = new Map<string, Cached<NavSection>>();
    activityItemCacheRef.current = nextItems;
    activitySectionCacheRef.current = nextSections;
    const projectNames = new Map(projects.map((project) => [project.project_id, project.name]));
    const activity = buildSidebarActivity({
      threads, activeThreadId: activeThreadId || "", pinnedIds, inboxStates, subagentPending, subagentActivity,
      prefs: activityPrefs, clearedAt: activityClearedAt, searchQuery, projectId: effectiveProjectFilter,
      retainedPriorityIds: activityPriorityIds,
    });

    const toActivityItem = (thread: ConversationThread): NavItem => reuseStable(previousItems, nextItems, thread.thread_id, {
      ...toItem(thread),
      subtitle: thread.summary
        || (thread.project_id ? projectNames.get(thread.project_id) || "未知项目" : "未归入项目"),
    });

    const result: NavSection[] = [];
    if (activityPrefs.showPriority) {
      result.push({
        id: "activity-priority", title: "优先级", kind: "activity", items: activity.priority.map(toActivityItem),
        emptyLabel: searchQuery.trim() ? "没有匹配的优先事项" : "暂无优先事项，待处理和未读动态会出现在这里",
      });
    }
    if (activity.pinned.length) result.push({ id: "activity-pinned", title: "置顶", kind: "activity", items: activity.pinned.map(toActivityItem) });
    const now = new Date();
    const buckets = new Map<TimeBucket, ConversationThread[]>();
    for (const thread of activity.recent) {
      const bucket = timeBucketOf(threadTimestamp(thread), now);
      const list = buckets.get(bucket);
      if (list) list.push(thread);
      else buckets.set(bucket, [thread]);
    }
    for (const bucket of TIME_BUCKETS) {
      const items = buckets.get(bucket);
      if (items?.length) result.push({ id: `activity-time:${bucket}`, title: timeBucketLabel(bucket), kind: "activity", items: items.map(toActivityItem) });
    }
    if (!result.length) {
      result.push({
        id: "activity-empty",
        title: "活动",
        kind: "activity",
        emptyLabel: searchQuery.trim() ? "没有匹配的对话" : activityClearedAt ? "已读对话已清除，新的动态会出现在这里" : "还没有对话",
        items: [],
      });
    }
    return {
      sections: result.map((section) => reuseStable(previousSections, nextSections, section.id || section.title, section)),
      count: activity.count,
      unreadIds: activity.unreadIds,
      readCount: activity.readCount,
      clearableIds: activity.clearableIds,
    };
  }, [threads, projects, activeThreadId, pinnedIds, inboxStates, subagentPending, subagentActivity, toItem, activityPrefs, activityClearedAt, dayStamp, searchQuery, effectiveProjectFilter, activityPriorityIds]);

  const updateActivityPrefs = useCallback((patch: Partial<ActivityPrefs>) => {
    setActivityPrefs((current) => {
      const next = { ...current, ...patch };
      persistActivityPrefs(next, preferencesScope);
      return next;
    });
  }, [preferencesScope]);

  const clearActivityRead = useCallback((at: number) => {
    setActivityClearedAt(at);
    persistActivityClearedAt(at, preferencesScope);
  }, [preferencesScope]);

  const unreadIds = inbox.unreadIds;
  const activityOptions = useMemo(() => ({
    ...activityPrefs,
    onShowChange: (key: keyof ActivityPrefs, value: boolean) => updateActivityPrefs({ [key]: value }),
    unreadCount: unreadIds.length,
    onMarkAllRead: hasMarkRead ? () => callbacksRef.current.onMarkThreadsRead?.(unreadIds) : undefined,
    readCount: inbox.readCount,
    onClearRead: () => {
      const cleared = new Set(inbox.clearableIds);
      setActivityPriorityIds((current) => {
        const next = current.filter((id) => !cleared.has(id));
        persistStringList(scopedKey(ACTIVITY_PRIORITY_KEY, preferencesScope), next);
        return next;
      });
      clearActivityRead(Date.now());
    },
    onRestoreDefaults: () => {
      updateActivityPrefs(DEFAULT_ACTIVITY_PREFS);
      clearActivityRead(0);
    },
  }), [activityPrefs, updateActivityPrefs, unreadIds, hasMarkRead, inbox.readCount, inbox.clearableIds, clearActivityRead, preferencesScope]);

  const projectFilterProp = useMemo<SidebarProjectFilter | undefined>(() => {
    if (!projects.length) return undefined;
    const counts = new Map<string, number>();
    for (const thread of threads) {
      if (thread.state.status === "archived" || !thread.project_id || attentionOwnerId(thread, threadsById)) continue;
      counts.set(thread.project_id, (counts.get(thread.project_id) || 0) + 1);
    }
    return {
      options: orderedProjects.map((project) => ({
        id: project.project_id,
        name: project.name,
        path: project.root_path || undefined,
        count: counts.get(project.project_id) || 0,
      })),
      value: effectiveProjectFilter,
      onChange: changeProjectFilter,
    };
  }, [projects.length, threads, threadsById, orderedProjects, effectiveProjectFilter, changeProjectFilter]);

  const bulkActions = useMemo<SidebarBulkActions>(() => ({
    pin: (ids) => {
      const allPinned = ids.every((id) => pinnedSet.has(id));
      applyServerChange(ids, allPinned ? "unpin" : "pin");
    },
    settle: (ids) => {
      const allSettled = ids.every((id) => inboxStates.get(id)?.placement === "settled");
      applyServerChange(ids, allSettled ? "unsettle" : "settle");
    },
    snooze: (ids, until) => applyInboxChange(ids, { kind: "snooze", until }),
    archive: hasArchiveProject
      ? (ids) => {
        const selected = new Set(ids);
        callbacksRef.current.onArchiveProjectThreads?.(threadsRef.current.filter((thread) => selected.has(thread.thread_id)));
      }
      : undefined,
  }), [applyInboxChange, applyServerChange, inboxStates, pinnedSet, hasArchiveProject]);

  const toggleActivityView = useCallback(() => setActivityView((current) => !current), []);
  const selectThread = useCallback((threadId: string) => callbacksRef.current.onSelectThread(threadId), []);
  const startNewChat = useCallback(() => callbacksRef.current.onNewChat(), []);
  const createFolder = useCallback(() => callbacksRef.current.onCreateFolder?.(), []);
  const emptyProjects = useMemo(
    () => orderedProjects.filter((project) => !liveThreadCounts.get(project.project_id)),
    [orderedProjects, liveThreadCounts],
  );
  const emptyProjectsRef = useRef(emptyProjects);
  emptyProjectsRef.current = emptyProjects;
  const deleteEmptyFolders = useCallback(() => callbacksRef.current.onDeleteEmptyProjects?.(emptyProjectsRef.current), []);
  const changeSearch = useCallback((q: string) => callbacksRef.current.onSearchChange(q), []);
  const visibleBodyHits = useMemo(() => restrictSearchToThreads
    ? bodyHits.filter((hit) => threadsById.has(hit.thread_id))
    : bodyHits, [bodyHits, restrictSearchToThreads, threadsById]);
  const navBodyHits = useMemo(() => visibleBodyHits.map((hit) => ({
    threadId: hit.thread_id,
    messageId: hit.message_id,
    title: hit.thread_title || "未命名对话",
    snippet: hit.snippet || "",
    role: hit.role || "user",
    archived: hit.archived,
    superseded: hit.superseded,
  })), [visibleBodyHits]);
  const loadMoreBodyHits = useCallback(() => { void loadMoreSearchHits(); }, [loadMoreSearchHits]);
  const canLoadMoreBodyHits = bodyHits.length > 0 && bodyHitsNextOffset != null;
  const retryBodyHits = useCallback(() => {
    if (canLoadMoreBodyHits) void loadMoreSearchHits();
    else setSearchAttempt((attempt) => attempt + 1);
  }, [canLoadMoreBodyHits, loadMoreSearchHits]);
  const selectBodyHit = useCallback((hit: { threadId: string; messageId: string }) => {
    if (restrictSearchToThreads && !threadsById.has(hit.threadId)) return;
    callbacksRef.current.onSelectMessageHit?.(hit.threadId, hit.messageId);
  }, [restrictSearchToThreads, threadsById]);

  useEffect(() => {
    const inTextField = (target: EventTarget | null) => target instanceof HTMLElement
      && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable || Boolean(target.closest("[data-c34-terminal]")));
    const onKey = (event: KeyboardEvent) => {
      if (event.isComposing || event.defaultPrevented) return;
      const mod = event.metaKey || event.ctrlKey;
      if (!mod || event.altKey) return;
      if (event.shiftKey && (event.code === "KeyS" || event.code === "KeyP") && navActiveId) {
        event.preventDefault();
        if (event.code === "KeyS") setThreadSettled(navActiveId, inboxStates.get(navActiveId)?.placement !== "settled");
        else togglePinned(navActiveId);
        return;
      }
      if (!event.shiftKey && event.code === "KeyZ" && !inTextField(event.target)) {
        if (serverUndoRef.current && serverUndoRef.current.expires >= Date.now()) {
          event.preventDefault();
          void undoServerChange();
        } else if (undoLastInboxChange()) event.preventDefault();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [navActiveId, inboxStates, setThreadSettled, togglePinned, undoLastInboxChange, undoServerChange]);

  const mobileOpen = className.split(/\s+/).includes("open");
  const drawerHidden = mobile ? !mobileOpen : collapsed;

  const mobileModal = mobile && mobileOpen;

  return (
    <aside
      ref={asideRef}
      {...{ [REFLOW_PANEL_ATTR]: "" }}
      className={`ai-conv-sidebar cx-sidebar flex h-full flex-col select-none ${
        resizing ? "resizing" : ""
      } ${className}`}
      id={sidebarId}
      data-collapsed={collapsed ? "true" : "false"}
      role={mobileModal ? "dialog" : undefined}
      aria-modal={mobileModal ? true : undefined}
      tabIndex={mobileModal ? -1 : undefined}
      aria-label={labels?.title ?? "对话导航"}
      aria-hidden={drawerHidden || undefined}
      inert={drawerHidden ? true : undefined}
    >
      {preferencesError ? <div role="alert" className="mx-2 mt-2 rounded-lg border border-cx-border p-2 text-cx-xs text-cx-warning">
        <details><summary>侧栏偏好尚未同步</summary><p className="mt-1 whitespace-pre-wrap break-words">{preferencesError}</p></details>
        <button type="button" className="mt-1 rounded px-2 py-1 text-cx-accent focus-visible:outline-2" onClick={() => void retryPreferences()}>重试保存</button>
      </div> : null}
      <SidebarNav
        labels={labels}
        storageNamespace={preferencesScope === "src" ? "src" : undefined}
        sections={sections}
        activitySections={inbox.sections}
        activityView={activityView}
        activityBadge={inbox.count}
        onToggleActivityView={toggleActivityView}
        activityOptions={activityOptions}
        projectFilter={projectFilterProp}
        bulkActions={bulkActions}
        activeId={navActiveId}
        onSelect={selectThread}
        onNewChat={startNewChat}
        onCreateFolder={hasCreateFolder ? createFolder : undefined}
        onDeleteEmptyFolders={hasDeleteEmptyProjects ? deleteEmptyFolders : undefined}
        emptyFolderCount={emptyProjects.length}
        searchQuery={searchQuery}
        onSearchChange={changeSearch}
        sortMode={sortMode}
        onSortChange={changeSortMode}
        groupMode={groupMode}
        onGroupChange={changeGroupMode}
        collapsed={false}
        bodyHits={navBodyHits}
        bodyHitsLoading={bodyHitsLoading}
        bodyHitsLoadingMore={bodyHitsLoadingMore}
        bodyHitsError={bodyHitsError}
        bodyHitsHasMore={bodyHitsNextOffset != null}
        onLoadMoreBodyHits={loadMoreBodyHits}
        onRetryBodyHits={retryBodyHits}
        includeSuperseded={includeSuperseded}
        onIncludeSupersededChange={setIncludeSuperseded}
        onSelectBodyHit={selectBodyHit}
        loading={loading}
      />
      {projectNavigationOnly ? (
        <div className="border-t border-cx-border-subtle px-3 py-2 text-cx-2xs text-cx-fg-4">
          仅保留研究导航、置顶与搜索；归档、删除等研究生命周期请在 SRC 任务工作台内操作。
        </div>
      ) : null}
      {!collapsed ? (
        <ResizeHandle
          side="right"
          value={width}
          min={RAIL_WIDTH_MIN}
          max={maxWidth}
          onChange={resizeTo}
          onRawChange={onResizeRaw}
          onReset={() => resizeTo(RAIL_WIDTH_DEFAULT)}
          onDragStateChange={onResizeDrag}
          label="拖拽调整对话列表宽度"
          className="ai-conv-sidebar-resizer"
        />
      ) : null}
      <Dialog
        open={deleteTarget !== null}
        onOpenChange={(open) => { if (!open && !deleting) setDeleteTarget(null); }}
        title="删除对话？"
        description={deleteTarget ? `将永久删除「${titleOf(deleteTarget)}」及其全部历史记录，无法恢复。` : undefined}
        tone="danger"
        icon="trash"
        size="sm"
        dismissable={!deleting}
        footer={(
          <>
            <Button variant="ghost" disabled={deleting} onClick={() => setDeleteTarget(null)}>取消</Button>
            <Button variant="danger" loading={deleting} onClick={() => { if (deleteTarget) void performDeleteThread(deleteTarget); }}>删除</Button>
          </>
        )}
      />
    </aside>
  );
});
