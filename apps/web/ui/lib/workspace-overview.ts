"use client";

import { useCallback, useEffect, useState } from "react";
import { fetchWorkspaceKindEntries, type WorkspaceKindEntry } from "./workspace-kinds";
import { apiFetch, type RunSummary } from "./useRun";
import type { ConversationThread } from "./useConversation";
import type { CompetitionInfo, CompetitionSnapshotView } from "./competition-events";

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
  refreshedAt: number;
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
  const response = await apiFetch(path);
  if (!response.ok) throw new Error(`${path} 返回 HTTP ${response.status}`);
  return (await response.json()) as T;
}

function competitionApproval(snapshot: CompetitionSnapshotView | null): boolean {
  return Boolean(snapshot?.challenges.some((row) =>
    row.candidates.some((candidate) => candidate.state === "awaiting_approval"),
  ));
}

export function useWorkspaceOverview(pollMs = 10000, solveOnly = false): WorkspaceOverview {
  const [kinds, setKinds] = useState<WorkspaceKindEntry[]>([]);
  const [recent, setRecent] = useState<RecentWorkspace[]>([]);
  const [activity, setActivity] = useState<Record<string, WorkspaceKindActivity>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [refreshedAt, setRefreshedAt] = useState(0);

  const refresh = useCallback(async () => {
    try {
      const [nextKinds, runBody, threadBody, competitions] = await Promise.all([
        fetchWorkspaceKindEntries(),
        json<{ runs?: RunSummary[] }>("/api/runs?archived=1"),
        solveOnly ? Promise.resolve({ threads: [] as ConversationThread[] }) : json<{ threads?: ConversationThread[] }>("/api/threads"),
        solveOnly ? Promise.resolve([] as CompetitionInfo[]) : json<CompetitionInfo[]>("/api/competitions"),
      ]);
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

      const rows: RecentWorkspace[] = [];
      for (const thread of threads) {
        // 与对话侧栏默认列表一致：已归档不计入顶栏「待处理」角标。
        const archived = thread.state.status === "archived";
        rows.push({
          id: thread.thread_id,
          kindId: "conversation",
          title: thread.title || thread.thread_id,
          href: `/chat/${encodeURIComponent(thread.thread_id)}`,
          status: thread.state.running_turn_id ? "运行中" : thread.state.status || "空闲",
          updatedAt: timestamp(thread.updated_at || thread.created_at),
          running: !archived && Boolean(thread.state.running_turn_id),
          unread: !archived && Boolean(thread.state.unread),
          approval: !archived && Boolean(
            thread.state.running_turn_id
            && (
              thread.state.pending_user_input
              || (thread.state.pending_approval && String(thread.state.pending_approval.status || "pending") === "pending")
              || (thread.state.pending_approvals && Object.values(thread.state.pending_approvals).some(
                (row) => row && String((row as Record<string, unknown>).status || "pending") === "pending",
              ))
            ),
          ),
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

      setKinds(nextKinds);
      setRecent(rows.slice(0, 8));
      setActivity(nextActivity);
      setError("");
      setRefreshedAt(Date.now());
    } catch (exc) {
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      setLoading(false);
    }
  }, [solveOnly]);

  useEffect(() => {
    let active = true;
    const run = async () => {
      if (active) await refresh();
    };
    void run();
    const timer = window.setInterval(() => void run(), pollMs);
    const onFocus = () => void run();
    window.addEventListener("focus", onFocus);
    return () => {
      active = false;
      window.clearInterval(timer);
      window.removeEventListener("focus", onFocus);
    };
  }, [pollMs, refresh]);

  return { kinds, recent, activity, loading, error, refreshedAt, refresh };
}
