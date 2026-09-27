"use client";

/* ─────────────────────────────────────────────────────────
 * CONVERSATION SIDEBAR — Left thread & project sidebar.
 * ───────────────────────────────────────────────────────── */

import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { SidebarNav, queryLooksSearchable, type NavItem, type NavSection } from "../ai-native/sidebar-nav";
import { ResizeHandle, toast } from "@/components/chat/ui";
import { TIME_BUCKETS, parseTimestamp, timeBucketLabel, timeBucketOf, type TimeBucket } from "@/components/chat/sidebar/time";
import type {
  ConversationThread,
  ConversationProject,
  ConversationSearchHit,
  SidebarPreferences,
} from "@/lib/useConversation";
import { fetchConversationSearch, fetchSidebarPreferences, saveSidebarPreferences } from "@/lib/useConversation";
import {
  clampRailWidth,
  railWidthMax,
  RAIL_WIDTH_DEFAULT,
  RAIL_WIDTH_MAX,
  RAIL_WIDTH_MIN,
} from "@/lib/railSizing";
import { threadHasAttention, threadNeedsAction } from "@/lib/threadAttention";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { rebaseSidebarPreferences, sameSidebarPreferences } from "@/lib/sidebarPreferenceMerge";

const PINNED_THREADS_KEY = "muteki.sidebar.pinned-threads.v1";
const THREAD_ORDER_KEY = "muteki.sidebar.thread-order.v1";
const PROJECT_ORDER_KEY = "muteki.sidebar.project-order.v1";
const PROJECT_THREAD_PREVIEW_LIMIT = 5;

function readStringList(key: string): string[] {
  try {
    const saved = JSON.parse(localStorage.getItem(key) || "[]");
    return Array.isArray(saved)
      ? saved.filter((id): id is string => typeof id === "string")
      : [];
  } catch {
    return [];
  }
}

function persistStringList(key: string, ids: string[]): void {
  try {
    localStorage.setItem(key, JSON.stringify(ids));
  } catch {
    // Sidebar preferences are optional when storage is unavailable.
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

export interface ConversationSidebarProps {
  threads: ConversationThread[];
  projects: ConversationProject[];
  activeThreadId?: string;
  /** Global pending/unread attention count for the activity bell badge. */
  attentionCount?: number;
  onSelectThread: (threadId: string) => void;
  onNewChat: () => void;
  onNewChatForProject?: (projectId: string) => void;
  onCreateFolder?: () => void;
  searchQuery: string;
  onSearchChange: (q: string) => void;
  onRenameThread?: (thread: ConversationThread) => void;
  onForkThread?: (thread: ConversationThread) => void;
  onArchiveThread?: (thread: ConversationThread) => void;
  /** Sequential batch archive for a project's threads (C31). */
  onArchiveProjectThreads?: (threads: ConversationThread[]) => void;
  onSelectMessageHit?: (threadId: string, messageId: string) => void;
  collapsed?: boolean;
  width: number;
  onWidthChange: (width: number) => void;
  /** First thread-list load in flight; shows skeleton rows while empty. */
  loading?: boolean;
  className?: string;
}

function threadHref(threadId: string): string {
  return `/chat/${encodeURIComponent(threadId)}`;
}

function threadTimestamp(thread: ConversationThread): number {
  return parseTimestamp(thread.updated_at || thread.created_at);
}

export function ConversationSidebar({
  threads,
  projects,
  activeThreadId,
  attentionCount = 0,
  onSelectThread,
  onNewChat,
  onNewChatForProject,
  onCreateFolder,
  searchQuery,
  onSearchChange,
  onRenameThread,
  onForkThread,
  onArchiveThread,
  onArchiveProjectThreads,
  onSelectMessageHit,
  collapsed = false,
  width,
  onWidthChange,
  loading = false,
  className = "",
}: ConversationSidebarProps) {
  const mobile = useMediaQuery("(max-width: 768px)");
  const [sortMode, setSortMode] = useState<"updated" | "priority" | "manual">("updated");
  const [pinnedSortMode, setPinnedSortMode] = useState<"updated" | "priority" | "manual">("manual");
  const [groupMode, setGroupMode] = useState<"project" | "list">("project");
  const [pinnedIds, setPinnedIds] = useState<string[]>([]);
  const [threadOrder, setThreadOrder] = useState<string[]>([]);
  const [projectOrder, setProjectOrder] = useState<string[]>([]);
  const [activityView, setActivityView] = useState(false);
  const [resizing, setResizing] = useState(false);
  const [bodyHits, setBodyHits] = useState<ConversationSearchHit[]>([]);
  const [bodyHitsLoading, setBodyHitsLoading] = useState(false);
  const [includeSuperseded, setIncludeSuperseded] = useState(false);
  const [maxWidth, setMaxWidth] = useState(RAIL_WIDTH_MAX);
  const [dayStamp, setDayStamp] = useState(() => new Date().toDateString());
  const searchAbortRef = useRef<AbortController | null>(null);

  // Server snapshot is the merge base; state changes made while it loads must
  // never be written back as if the user changed them.
  const serverSnapshotRef = useRef<SidebarPreferences | null>(null);
  const latestPreferencesRef = useRef<SidebarPreferences>({
    version: 0, pinned_ids: [], thread_order: [], project_order: [],
    sort_mode: "updated", pinned_sort_mode: "manual", group_mode: "project",
  });
  const failedPreferencesRef = useRef<SidebarPreferences | null>(null);
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
    persistStringList(PINNED_THREADS_KEY, prefs.pinned_ids);
    persistStringList(THREAD_ORDER_KEY, prefs.thread_order);
    persistStringList(PROJECT_ORDER_KEY, prefs.project_order);
  }, []);

  useEffect(() => {
    // 1. Immediately apply localStorage (instant visual feedback before server responds).
    const localPinned = readStringList(PINNED_THREADS_KEY);
    const localThreadOrder = readStringList(THREAD_ORDER_KEY);
    const localProjectOrder = readStringList(PROJECT_ORDER_KEY);
    setPinnedIds(localPinned);
    setThreadOrder(localThreadOrder);
    if (localThreadOrder.length) setSortMode("manual");
    setProjectOrder(localProjectOrder);

    // 2. Fetch from server and apply authoritative preferences.
    serverLoadInProgressRef.current = true;
    fetchSidebarPreferences()
      .then(async (remote: SidebarPreferences) => {
        serverSnapshotRef.current = remote;
        if (remote.version > 0) {
          applyPreferences(remote);
        } else if (localPinned.length || localThreadOrder.length || localProjectOrder.length) {
          // Server is empty — migrate from localStorage.
          const migrated: SidebarPreferences = {
            version: 0,
            pinned_ids: localPinned,
            thread_order: localThreadOrder,
            project_order: localProjectOrder,
            sort_mode: localThreadOrder.length ? "manual" : "updated",
            pinned_sort_mode: "manual",
            group_mode: "project",
          };
          const { prefs } = await saveSidebarPreferences(migrated);
          serverSnapshotRef.current = prefs;
          applyPreferences(prefs);
        }
      })
      .catch(() => { /* Server unavailable — localStorage is the fallback. */ })
      .finally(() => {
        serverLoadInProgressRef.current = false;
        prefsReadyRef.current = true;
      });
  }, [applyPreferences]);

  // Rebase only fields changed in this tab, so a concurrent tab's unrelated
  // edits survive. Serialize writes and preserve edits made during a request.
  const syncTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const flushPreferences = useCallback(async () => {
    if (syncInProgressRef.current) return;
    syncInProgressRef.current = true;
    try {
      for (let attempt = 0; attempt < 3; attempt += 1) {
        const base = serverSnapshotRef.current;
        const local = latestPreferencesRef.current;
        if (!base || sameSidebarPreferences(base, local)) {
          failedPreferencesRef.current = null;
          return;
        }
        const remote = await fetchSidebarPreferences();
        const merged = rebaseSidebarPreferences(base, local, remote);
        if (sameSidebarPreferences(merged, remote)) {
          serverSnapshotRef.current = remote;
          const next = rebaseSidebarPreferences(local, latestPreferencesRef.current, remote);
          applyPreferences(next);
          if (sameSidebarPreferences(next, remote)) return;
          continue;
        }
        const { ok, prefs } = await saveSidebarPreferences(merged);
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
      toast({ title: "侧栏偏好尚未保存，请再试一次", tone: "warning" });
    } catch {
      failedPreferencesRef.current = latestPreferencesRef.current;
      toast({ title: "侧栏偏好保存失败，请检查连接后重试", tone: "warning" });
    } finally {
      syncInProgressRef.current = false;
    }
  }, [applyPreferences]);

  const currentPreferences = useMemo<SidebarPreferences>(() => ({
    version: 0, pinned_ids: pinnedIds, thread_order: threadOrder,
    project_order: projectOrder, sort_mode: sortMode,
    pinned_sort_mode: pinnedSortMode, group_mode: groupMode,
  }), [pinnedIds, threadOrder, projectOrder, sortMode, pinnedSortMode, groupMode]);
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
  }, [currentPreferences, flushPreferences]);

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

  useEffect(() => () => document.body.classList.remove("rail-resizing"), []);

  useEffect(() => {
    searchAbortRef.current?.abort();
    searchAbortRef.current = null;
    if (!queryLooksSearchable(searchQuery)) {
      setBodyHits([]);
      setBodyHitsLoading(false);
      return;
    }
    const controller = new AbortController();
    searchAbortRef.current = controller;
    setBodyHitsLoading(true);
    const timer = window.setTimeout(() => {
      void fetchConversationSearch(searchQuery, {
        includeArchived: false,
        includeSuperseded,
        limit: 20,
        signal: controller.signal,
      }).then((result) => {
        if (controller.signal.aborted) return;
        setBodyHits(result.hits || []);
        setBodyHitsLoading(false);
      }).catch(() => {
        if (controller.signal.aborted) return;
        setBodyHits([]);
        setBodyHitsLoading(false);
      });
    }, 250);
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [searchQuery, includeSuperseded]);

  const resizeTo = useCallback((next: number) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    onWidthChange(clampRailWidth(next, viewport));
  }, [onWidthChange]);

  const onResizeDrag = useCallback((dragging: boolean) => {
    setResizing(dragging);
    document.body.classList.toggle("rail-resizing", dragging);
  }, []);

  const togglePinned = useCallback((threadId: string) => {
    setPinnedIds((current) => {
      const next = current.includes(threadId)
        ? current.filter((id) => id !== threadId)
        : [...current, threadId];
      persistStringList(PINNED_THREADS_KEY, next);
      return next;
    });
  }, []);

  const moveThread = useCallback((sourceId: string, targetId: string, visibleIds: string[]) => {
    setSortMode("manual");
    setThreadOrder((current) => {
      const base = sortMode === "manual" ? mergeOrder(current, visibleIds) : [...visibleIds];
      const next = moveBefore(base, sourceId, targetId);
      persistStringList(THREAD_ORDER_KEY, next);
      return next;
    });
  }, [sortMode]);

  const movePinnedThread = useCallback((sourceId: string, targetId: string, visibleIds: string[]) => {
    setPinnedSortMode("manual");
    setPinnedIds((current) => {
      const base = pinnedSortMode === "manual" ? mergeOrder(current, visibleIds) : [...visibleIds];
      const next = moveBefore(base, sourceId, targetId);
      persistStringList(PINNED_THREADS_KEY, next);
      return next;
    });
  }, [pinnedSortMode]);

  const orderedProjects = useMemo(() => (
    orderByIds(projects, projectOrder, (project) => project.project_id)
  ), [projects, projectOrder]);

  const moveProject = useCallback((sourceSectionId: string, targetSectionId: string) => {
    const sourceId = sourceSectionId.replace(/^project:/, "");
    const targetId = targetSectionId.replace(/^project:/, "");
    const visibleIds = orderedProjects.map((project) => project.project_id);
    setProjectOrder((current) => {
      const next = moveBefore(mergeOrder(current, visibleIds), sourceId, targetId);
      persistStringList(PROJECT_ORDER_KEY, next);
      return next;
    });
  }, [orderedProjects]);

  // Archived conversations are managed from Settings instead of this rail.
  const activeThreads = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    const matchingThreads = threads.filter((thread) => (
      !q || `${thread.title} ${thread.summary || ""} ${thread.thread_id}`.toLowerCase().includes(q)
    ));
    const sortThreads = (items: ConversationThread[]) => items.sort((a, b) => {
      if (sortMode === "priority") {
        const rank = (thread: ConversationThread) => (
          thread.state.running_turn_id ? 0
            : threadNeedsAction(thread) ? 1
              : thread.state.unread ? 2
                : 3
        );
        return rank(a) - rank(b);
      }
      if (sortMode === "manual") {
        const aRank = threadOrder.indexOf(a.thread_id);
        const bRank = threadOrder.indexOf(b.thread_id);
        return (aRank < 0 ? Number.MAX_SAFE_INTEGER : aRank) -
          (bRank < 0 ? Number.MAX_SAFE_INTEGER : bRank);
      }
      return String(b.updated_at || b.created_at || "").localeCompare(String(a.updated_at || a.created_at || ""));
    });

    return sortThreads(matchingThreads.filter((thread) => thread.state.status !== "archived"));
  }, [threads, searchQuery, sortMode, threadOrder]);

  // Group threads into sections (Recent, Running, or by Project)
  const sections: NavSection[] = useMemo(() => {
    const pinned = activeThreads.filter((t) => pinnedIds.includes(t.thread_id)).sort((a, b) => {
      if (pinnedSortMode === "manual") return pinnedIds.indexOf(a.thread_id) - pinnedIds.indexOf(b.thread_id);
      if (pinnedSortMode === "priority") return Number(Boolean(b.state.running_turn_id)) - Number(Boolean(a.state.running_turn_id));
      return String(b.updated_at || b.created_at || "").localeCompare(String(a.updated_at || a.created_at || ""));
    });
    const ordinary = activeThreads.filter((t) => !pinnedIds.includes(t.thread_id));

    const result: NavSection[] = [];
    const searching = Boolean(searchQuery.trim());

    const toItem = (t: ConversationThread): NavItem => ({
      id: t.thread_id,
      label: t.title || "未命名对话",
      href: threadHref(t.thread_id),
      updatedAt: t.updated_at || t.created_at,
      status: t.state.status,
      running: Boolean(t.state.running_turn_id),
      // 当前打开的对话视为已读；列表刷新前先去掉未读点。
      unread: Boolean(t.state.unread) && t.thread_id !== activeThreadId,
      needsAction: threadNeedsAction(t),
      archived: t.state.status === "archived",
      failed: Boolean(t.state.last_error && Object.keys(t.state.last_error).length),
      pinned: pinnedIds.includes(t.thread_id),
      onPin: () => togglePinned(t.thread_id),
      onRename: onRenameThread ? () => onRenameThread(t) : undefined,
      onFork: onForkThread ? () => onForkThread(t) : undefined,
      onArchive: onArchiveThread ? () => onArchiveThread(t) : undefined,
    });

    const visibleThreadIds = activeThreads.map((thread) => thread.thread_id);

    const toItems = (
      items: ConversationThread[],
      reorder: (sourceId: string, targetId: string, visibleIds: string[]) => void,
      orderIds: string[],
    ) => items.map((thread, index): NavItem => ({
      ...toItem(thread),
      onMoveUp: index > 0
        ? () => reorder(thread.thread_id, items[index - 1].thread_id, orderIds)
        : undefined,
      onMoveDown: index < items.length - 1
        ? () => reorder(thread.thread_id, items[index + 1].thread_id, orderIds)
        : undefined,
    }));

    // Recency buckets only make sense when the list is ordered by time.
    const chronological = (idPrefix: string, title: string, items: ConversationThread[]): NavSection[] => {
      const onReorderItems = (sourceId: string, targetId: string) => moveThread(sourceId, targetId, visibleThreadIds);
      if (sortMode !== "updated") {
        return [{ id: idPrefix, title, kind: "section", items: toItems(items, moveThread, visibleThreadIds), onReorderItems }];
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
        items: toItems(buckets.get(bucket) || [], moveThread, visibleThreadIds),
        onReorderItems,
      }));
    };

    if (groupMode === "list") {
      return [
        { id: "pinned", title: "置顶", kind: "pinned", items: toItems(pinned, movePinnedThread, pinned.map((thread) => thread.thread_id)), sortMode: pinnedSortMode, onSortChange: setPinnedSortMode, onReorderItems: (sourceId, targetId) => movePinnedThread(sourceId, targetId, pinned.map((thread) => thread.thread_id)) },
        ...chronological("all", "全部对话", ordinary),
      ];
    }

    result.push({
      id: "pinned",
      title: "置顶",
      kind: "pinned",
      items: toItems(pinned, movePinnedThread, pinned.map((thread) => thread.thread_id)),
      sortMode: pinnedSortMode,
      onSortChange: setPinnedSortMode,
      onReorderItems: (sourceId, targetId) => movePinnedThread(sourceId, targetId, pinned.map((thread) => thread.thread_id)),
    });

    for (const [projectIndex, project] of orderedProjects.entries()) {
      const projectThreads = ordinary.filter((thread) => thread.project_id === project.project_id);
      if (searching && !projectThreads.length) continue;
      result.push({
        id: `project:${project.project_id}`,
        title: project.name,
        kind: "folder",
        previewLimit: PROJECT_THREAD_PREVIEW_LIMIT,
        emptyLabel: "暂无对话",
        running: projectThreads.some((thread) => Boolean(thread.state.running_turn_id)),
        onNewChat: onNewChatForProject ? () => onNewChatForProject(project.project_id) : undefined,
        onArchiveAll: (
          onArchiveProjectThreads
            ? () => onArchiveProjectThreads(projectThreads)
            : onArchiveThread
              ? () => { projectThreads.forEach(onArchiveThread); }
              : undefined
        ),
        onMoveUp: projectIndex > 0
          ? () => moveProject(`project:${project.project_id}`, `project:${orderedProjects[projectIndex - 1].project_id}`)
          : undefined,
        onMoveDown: projectIndex < orderedProjects.length - 1
          ? () => moveProject(`project:${project.project_id}`, `project:${orderedProjects[projectIndex + 1].project_id}`)
          : undefined,
        onReorderSection: moveProject,
        items: toItems(projectThreads, moveThread, visibleThreadIds),
        onReorderItems: (sourceId, targetId) => moveThread(sourceId, targetId, visibleThreadIds),
      });
    }

    const looseThreads = ordinary.filter((thread) => (
      !thread.project_id || !orderedProjects.some((project) => project.project_id === thread.project_id)
    ));
    result.push(...chronological("recent", "最近对话", looseThreads));

    return result;
    // dayStamp re-buckets 今天/昨天 when the date rolls over.
  }, [activeThreads, activeThreadId, groupMode, sortMode, searchQuery, dayStamp, orderedProjects, onRenameThread, onForkThread, onArchiveThread, onArchiveProjectThreads, onNewChatForProject, pinnedIds, pinnedSortMode, togglePinned, moveThread, movePinnedThread, moveProject]);

  const activitySections: NavSection[] = useMemo(() => {
    const projectNames = new Map(projects.map((project) => [project.project_id, project.name]));
    const candidates = threads
      .filter((thread) => thread.state.status !== "archived")
      .sort((a, b) => String(b.updated_at || b.created_at || "").localeCompare(String(a.updated_at || a.created_at || "")));

    const toActivityItem = (thread: ConversationThread): NavItem => ({
      id: thread.thread_id,
      label: thread.title || "未命名对话",
      href: threadHref(thread.thread_id),
      updatedAt: thread.updated_at || thread.created_at,
      subtitle: thread.summary
        || (thread.project_id ? projectNames.get(thread.project_id) || "未知项目" : "未归入项目"),
      status: thread.state.status,
      running: Boolean(thread.state.running_turn_id),
      unread: Boolean(thread.state.unread) && thread.thread_id !== activeThreadId,
      needsAction: threadNeedsAction(thread),
      failed: Boolean(thread.state.last_error && Object.keys(thread.state.last_error).length),
      pinned: pinnedIds.includes(thread.thread_id),
      onPin: () => togglePinned(thread.thread_id),
      onRename: onRenameThread ? () => onRenameThread(thread) : undefined,
      onFork: onForkThread ? () => onForkThread(thread) : undefined,
      onArchive: onArchiveThread ? () => onArchiveThread(thread) : undefined,
    });

    const needsAttention = (thread: ConversationThread) => Boolean(
      thread.state.running_turn_id || threadHasAttention(thread, activeThreadId)
    );
    const attention = candidates.filter(needsAttention).sort((a, b) => {
      const rank = (thread: ConversationThread) => (
        thread.state.last_error && Object.keys(thread.state.last_error).length ? 0 :
        threadNeedsAction(thread) ? 1 :
        thread.state.unread ? 2 : 3
      );
      return rank(a) - rank(b) || String(b.updated_at || "").localeCompare(String(a.updated_at || ""));
    });
    const attentionIds = new Set(attention.map((thread) => thread.thread_id));
    const timeline = candidates.filter((thread) => !attentionIds.has(thread.thread_id));
    const now = new Date();
    const todayStart = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    const yesterdayStart = todayStart - 86_400_000;
    const timestamp = (thread: ConversationThread) => Date.parse(String(thread.updated_at || thread.created_at || "")) || 0;
    const today = timeline.filter((thread) => timestamp(thread) >= todayStart);
    const yesterday = timeline.filter((thread) => timestamp(thread) >= yesterdayStart && timestamp(thread) < todayStart);
    const earlier = timeline.filter((thread) => timestamp(thread) < yesterdayStart);

    const result: NavSection[] = [];
    if (attention.length) result.push({ id: "activity-priority", title: "需要关注", kind: "activity", items: attention.map(toActivityItem) });
    if (today.length) result.push({ id: "activity-today", title: "今天", kind: "activity", items: today.map(toActivityItem) });
    if (yesterday.length) result.push({ id: "activity-yesterday", title: "昨天", kind: "activity", items: yesterday.map(toActivityItem) });
    if (earlier.length) result.push({ id: "activity-earlier", title: "更早", kind: "activity", items: earlier.map(toActivityItem) });
    if (!result.length) result.push({ id: "activity-empty", title: "活动", kind: "activity", emptyLabel: "一切就绪，暂时没有需要关注的对话", items: [] });
    return result;
  }, [threads, projects, activeThreadId, pinnedIds, togglePinned, onRenameThread, onForkThread, onArchiveThread]);

  const mobileOpen = className.split(/\s+/).includes("open");
  const drawerHidden = mobile ? !mobileOpen : collapsed;

  // Mobile overlay open → dialog semantics for focus trap (#199); desktop rail stays non-modal.
  const mobileModal = mobile && mobileOpen;

  return (
    <aside
      className={`ai-conv-sidebar cx-sidebar flex h-full flex-col select-none ${
        resizing ? "resizing" : ""
      } ${className}`}
      id="conversation-sidebar"
      data-collapsed={collapsed ? "true" : "false"}
      role={mobileModal ? "dialog" : undefined}
      aria-modal={mobileModal ? true : undefined}
      tabIndex={mobileModal ? -1 : undefined}
      aria-label="对话导航"
      aria-hidden={drawerHidden || undefined}
      inert={drawerHidden ? true : undefined}
    >
      <SidebarNav
        sections={sections}
        activitySections={activitySections}
        activityView={activityView}
        activityBadge={attentionCount}
        onToggleActivityView={() => setActivityView((current) => !current)}
        activeId={activeThreadId}
        onSelect={onSelectThread}
        onNewChat={onNewChat}
        onCreateFolder={onCreateFolder}
        searchQuery={searchQuery}
        onSearchChange={onSearchChange}
        sortMode={sortMode}
        onSortChange={setSortMode}
        groupMode={groupMode}
        onGroupChange={setGroupMode}
        collapsed={false}
        bodyHits={bodyHits.map((hit) => ({
          threadId: hit.thread_id,
          messageId: hit.message_id,
          title: hit.thread_title || "未命名对话",
          snippet: hit.snippet || "",
          role: hit.role || "user",
          archived: hit.archived,
          superseded: hit.superseded,
        }))}
        bodyHitsLoading={bodyHitsLoading}
        includeSuperseded={includeSuperseded}
        onIncludeSupersededChange={setIncludeSuperseded}
        onSelectBodyHit={(hit) => {
          onSelectMessageHit?.(hit.threadId, hit.messageId);
        }}
        loading={loading}
      />
      {!collapsed ? (
        <ResizeHandle
          side="right"
          value={width}
          min={RAIL_WIDTH_MIN}
          max={maxWidth}
          onChange={resizeTo}
          onReset={() => resizeTo(RAIL_WIDTH_DEFAULT)}
          onDragStateChange={onResizeDrag}
          label="拖拽调整对话列表宽度"
          className="ai-conv-sidebar-resizer"
        />
      ) : null}
    </aside>
  );
}
