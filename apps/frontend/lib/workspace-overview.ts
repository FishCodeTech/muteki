"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { fetchWorkspaceKindEntries, type WorkspaceKindEntry } from "./workspace-kinds";
import { apiFetch, type RunSummary } from "./useRun";
import type { ConversationThread } from "./useConversation";
import type { CompetitionInfo, CompetitionSnapshotView } from "./competition-events";
import type { SrcTask } from "./srcTypes";
import { isDesktopWorkspaceActive } from "./desktopEnvironment";
import { attentionOwnerId, subagentActivityByRoot, subagentPendingByRoot, threadNeedsAction } from "./threadAttention";
import { inboxStateOf } from "./sidebarInbox";
import { isInternalCtfThread } from "./conversationInbox";
import { conversationStorageKey, conversationStorageScope } from "./conversationStorageScope";
import { currentSidebarThreadStates, subscribeSidebarThreadStates } from "./sidebarThreadBridge";

const SETTLED_KEY = "muteki.sidebar.settled-at.v1";
const SNOOZED_KEY = "muteki.sidebar.snoozed-until.v1";

function readTimeMap(key: string): Record<string, number> {
  if (typeof window === "undefined" || !conversationStorageScope()) return {};
  try {
    const saved = JSON.parse(window.localStorage.getItem(conversationStorageKey(key)) || "{}") as Record<string, unknown>;
    const result: Record<string, number> = {};
    for (const [id, value] of Object.entries(saved || {})) {
      if (typeof value === "number" && Number.isFinite(value) && value > 0) result[id] = value;
    }
    return result;
  } catch {
    return {};
  }
}

type OverviewSrcTask = SrcTask & {
  lifecycle?: { state?: string };
  created_at?: string;
  updated_at?: string;
};

interface OverviewSnapshot {
  kinds: WorkspaceKindEntry[];
  runs: RunSummary[];
  threads: ConversationThread[];
  competitions: CompetitionInfo[];
  snapshots: (CompetitionSnapshotView | null)[];
  srcTasks: OverviewSrcTask[];
}

export interface RecentWorkspace {
  id: string;
  kindId: string;
  title: string;
  href: string;
  status: string;
  updatedAt: number;
  running: boolean;
  unread: boolean;
  approval: boolean;
}

export interface WorkspaceKindActivity {
  running: number;
  unread: number;
  approvals: number;
  total: number;
}

export interface WorkspaceOverview {
  kinds: WorkspaceKindEntry[];
  recent: RecentWorkspace[];
  activity: Record<string, WorkspaceKindActivity>;
  loading: boolean;
  error: string;
  hasRefreshed: boolean;
  refresh: () => Promise<void>;
}

const EMPTY_ACTIVITY: WorkspaceKindActivity = {
  running: 0,
  unread: 0,
  approvals: 0,
  total: 0,
};

function timestamp(value: unknown): number {
  if (typeof value === "number") return value < 1e12 ? value * 1000 : value;
  const parsed = Date.parse(String(value || ""));
  return Number.isFinite(parsed) ? parsed : 0;
}

async function json<T>(path: string): Promise<T> {
  const response = await apiFetch(path, { signal: AbortSignal.timeout(8000) });
  if (!response.ok) throw new Error(`${path} 返回 HTTP ${response.status}`);
  return (await response.json()) as T;
}

function competitionApproval(snapshot: CompetitionSnapshotView | null): boolean {
  return Boolean(snapshot?.challenges.some((row) =>
    row.candidates.some((candidate) => candidate.state === "awaiting_approval"),
  ));
}

export function useWorkspaceOverview(pollMs = 10000, solveOnly = false, enabled = true): WorkspaceOverview {
  const [kinds, setKinds] = useState<WorkspaceKindEntry[]>([]);
  const [recent, setRecent] = useState<RecentWorkspace[]>([]);
  const [activity, setActivity] = useState<Record<string, WorkspaceKindActivity>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [hasRefreshed, setHasRefreshed] = useState(false);
  const enabledRef = useRef(enabled);
  enabledRef.current = enabled;

  const lastSources = useRef({
    kinds: [] as WorkspaceKindEntry[], runs: {} as { runs?: RunSummary[] },
    threads: {} as { threads?: ConversationThread[] },
    competitions: [] as CompetitionInfo[], src: [] as OverviewSrcTask[],
  });
  const loaded = useRef<OverviewSnapshot | null>(null);
  const lastStartedAt = useRef(Number.NEGATIVE_INFINITY);
  const activeRefresh = useRef<{ requestedAgain: boolean } | null>(null);

  const updateOverview = useCallback((snapshot: OverviewSnapshot) => {
    const { kinds: nextKinds, runs, threads, competitions: competitionRows, snapshots, srcTasks } = snapshot;
    const rows: RecentWorkspace[] = [];
    const threadsById = new Map(threads.map((thread) => [thread.thread_id, thread]));
    const subagentPending = subagentPendingByRoot(threads);
    const subagentActivity = subagentActivityByRoot(threads);
    const sidebarStates = currentSidebarThreadStates();
    const needsPersistedPlacement = threads.some(
      (thread) => !attentionOwnerId(thread, threadsById) && !sidebarStates.has(thread.thread_id),
    );
    const settledAt = needsPersistedPlacement ? readTimeMap(SETTLED_KEY) : {};
    const snoozedUntil = needsPersistedPlacement ? readTimeMap(SNOOZED_KEY) : {};
    const nowMs = Date.now();
    const pathname = typeof window !== "undefined" ? window.location.pathname : "";
    const activeThreadId = pathname.startsWith("/chat/") ? decodeURIComponent(pathname.split("/")[2] || "") : "";
    const activeThread = activeThreadId ? threadsById.get(activeThreadId) : undefined;
    const activeOwner = activeThread ? attentionOwnerId(activeThread, threadsById) || activeThreadId : activeThreadId;
    for (const thread of threads) {
      if (thread.mode === "src" || isInternalCtfThread(thread)) continue;
      if (attentionOwnerId(thread, threadsById)) continue;
      const archived = thread.state.status === "archived";
      const running = !archived && Boolean(
        thread.state.running_turn_id || subagentActivity.get(thread.thread_id)?.active,
      );
      const willContinue = !archived && Boolean(
        thread.state.queued_turn
        || thread.state.quota_resume
        || (
          Number(thread.state.queue_count || 0) > 0
          && !thread.state.queue_paused
          && !thread.state.queue_failed_item_id
        ),
      );
      const approval = !archived && (
        threadNeedsAction(thread) || subagentPending.has(thread.thread_id)
      );
      const placement = sidebarStates.get(thread.thread_id)?.placement
        ?? inboxStateOf(thread, settledAt, snoozedUntil, nowMs, subagentPending.has(thread.thread_id)).placement;
      const unread = !archived
        && !approval
        && placement === "active"
        && Boolean(thread.state.unread)
        && thread.thread_id !== activeOwner
        && !running
        && !willContinue;
      rows.push({
        id: thread.thread_id,
        kindId: "conversation",
        title: thread.title || thread.thread_id,
        href: `/chat/${encodeURIComponent(thread.thread_id)}`,
        status: running ? "运行中" : thread.state.status || "空闲",
        updatedAt: timestamp(thread.updated_at || thread.created_at),
        running,
        unread,
        approval,
      });
    }
    for (const run of runs) {
      rows.push({
        id: run.run_id,
        kindId: run.mode === "pentest" ? "pentest" : "single-security-task",
        title: run.name || run.run_id,
        href: run.mode === "pentest"
          ? `/pentest?run=${encodeURIComponent(run.run_id)}`
          : `/run/${encodeURIComponent(run.run_id)}`,
        status: run.status,
        updatedAt: timestamp(run.updated_at ?? run.updated),
        running: run.status === "running" || run.status === "paused",
        unread: false,
        approval: false,
      });
    }
    competitionRows.forEach((competition, index) => {
      const snapshot = snapshots[index];
      rows.push({
        id: competition.competition_id,
        kindId: "competition",
        title: competition.title || competition.competition_id,
        href: `/competitions/${encodeURIComponent(competition.competition_id)}`,
        status: competition.scheduler_state || "stopped",
        updatedAt: timestamp(competition.updated_at || competition.created_at),
        running: competition.scheduler_state === "running",
        unread: false,
        approval: competitionApproval(snapshot),
      });
    });
    for (const task of srcTasks) {
      const conversation = task.binding?.thread_id ? threadsById.get(task.binding.thread_id) : undefined;
      rows.push({
        id: task.task_id,
        kindId: "src",
        title: task.title || task.task_id,
        href: `/src/tasks/${encodeURIComponent(task.task_id)}`,
        status: task.lifecycle?.state || "active",
        updatedAt: timestamp(task.updated_at || task.created_at),
        running: Boolean(conversation?.state.running_turn_id),
        unread: false,
        approval: false,
      });
    }
    rows.sort((a, b) => b.updatedAt - a.updatedAt);

    const nextActivity: Record<string, WorkspaceKindActivity> = {};
    for (const kind of nextKinds) nextActivity[kind.id] = { ...EMPTY_ACTIVITY };
    for (const row of rows) {
      const current = nextActivity[row.kindId] ?? { ...EMPTY_ACTIVITY };
      current.total += 1;
      if (row.running) current.running += 1;
      if (row.unread) current.unread += 1;
      if (row.approval) current.approvals += 1;
      nextActivity[row.kindId] = current;
    }

    const nextRecent = rows.slice(0, 8);
    setRecent((current) => JSON.stringify(current) === JSON.stringify(nextRecent) ? current : nextRecent);
    setActivity((current) => JSON.stringify(current) === JSON.stringify(nextActivity) ? current : nextActivity);
  }, []);

  const refresh = useCallback(async function refresh() {
    if (!enabledRef.current) return;
    lastStartedAt.current = performance.now();
    if (activeRefresh.current) {
      activeRefresh.current.requestedAgain = true;
      return;
    }
    const request = { requestedAgain: false };
    activeRefresh.current = request;
    const ownsRefresh = () => enabledRef.current && activeRefresh.current === request;
    const isCurrent = () => ownsRefresh() && !request.requestedAgain;
    try {
      const failures: string[] = [];
      const read = async <K extends keyof typeof lastSources.current>(key: K, label: string, request: Promise<(typeof lastSources.current)[K]>) => {
        try {
          const value = await request;
          if (!isCurrent()) return lastSources.current[key];
          lastSources.current[key] = value;
          if (key === "kinds") {
            const nextKinds = value as WorkspaceKindEntry[];
            setKinds((current) => JSON.stringify(current) === JSON.stringify(nextKinds) ? current : nextKinds);
          }
          return value;
        } catch (error) {
          failures.push(`${label}暂不可用：${error instanceof Error ? error.message : String(error)}`);
          return lastSources.current[key];
        }
      };
      const [nextKinds, runBody, threadBody, competitions, srcTasks] = await Promise.all([
        read("kinds", "工作区入口", fetchWorkspaceKindEntries()),
        read("runs", "安全任务", json<{ runs?: RunSummary[] }>("/api/runs?archived=1")),
        read("threads", "聊天", solveOnly ? Promise.resolve({ threads: [] as ConversationThread[] }) : json<{ threads?: ConversationThread[] }>("/api/threads?view=workspace-overview")),
        read("competitions", "CTF 比赛", solveOnly ? Promise.resolve([] as CompetitionInfo[]) : json<CompetitionInfo[]>("/api/competitions")),
        read("src", "SRC", solveOnly ? Promise.resolve([] as OverviewSrcTask[]) : json<OverviewSrcTask[]>("/api/src/tasks")),
      ]);
      if (!isCurrent()) return;
      const runs = runBody.runs ?? [];
      const threads = threadBody.threads ?? [];
      const competitionRows = Array.isArray(competitions) ? competitions : [];
      const snapshots = await Promise.all(competitionRows.map(async (competition) => {
        try {
          return await json<CompetitionSnapshotView>(
            `/api/competitions/${encodeURIComponent(competition.competition_id)}`,
          );
        } catch {
          return null;
        }
      }));

      if (!isCurrent()) return;
      const snapshot = { kinds: nextKinds, runs, threads, competitions: competitionRows, snapshots, srcTasks };
      updateOverview(snapshot);
      loaded.current = snapshot;
      setError(failures.join("；"));
      setHasRefreshed(true);
    } catch (exc) {
      if (isCurrent()) setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      if (ownsRefresh()) {
        activeRefresh.current = null;
        if (request.requestedAgain && isDesktopWorkspaceActive()) void refresh();
        else setLoading(false);
      }
    }
  }, [solveOnly, updateOverview]);

  useEffect(() => {
    if (!enabled) {
      lastSources.current = { kinds: [], runs: {}, threads: {}, competitions: [], src: [] };
      loaded.current = null;
      setKinds([]);
      setRecent([]);
      setActivity({});
      setError("");
      setLoading(false);
      return;
    }
    const onFocus = () => { if (isDesktopWorkspaceActive()) void refresh(); };
    onFocus();
    window.addEventListener("focus", onFocus);
    window.addEventListener("muteki:workspace-activated", onFocus);
    window.addEventListener("muteki:threads-changed", onFocus);
    const unsubscribeSidebar = subscribeSidebarThreadStates(() => {
      if (enabledRef.current && loaded.current && isDesktopWorkspaceActive()) updateOverview(loaded.current);
    });
    return () => {
      activeRefresh.current = null;
      window.removeEventListener("focus", onFocus);
      window.removeEventListener("muteki:workspace-activated", onFocus);
      window.removeEventListener("muteki:threads-changed", onFocus);
      unsubscribeSidebar();
    };
  }, [enabled, refresh, updateOverview]);

  useEffect(() => {
    if (!enabled) return;
    let timer: number;
    const schedule = () => {
      const elapsed = performance.now() - lastStartedAt.current;
      const delay = isDesktopWorkspaceActive() ? Math.max(0, pollMs - elapsed) : pollMs;
      timer = window.setTimeout(() => {
        if (isDesktopWorkspaceActive() && performance.now() - lastStartedAt.current >= pollMs) void refresh();
        schedule();
      }, delay);
    };
    schedule();
    return () => window.clearTimeout(timer);
  }, [enabled, pollMs, refresh]);

  const pausedRefresh = useCallback(async () => undefined, []);
  return { kinds, recent, activity, loading, error, hasRefreshed, refresh: enabled ? refresh : pausedRefresh };
}
