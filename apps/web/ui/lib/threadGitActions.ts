/**
 * Thread-scoped Git write operations (commit / push / pull / create PR) and
 * the shared pull-request listing they invalidate.
 *
 * Every write applies the returned status to the shared Git status store so
 * the header, details card and composer branch picker update together.
 */

"use client";

import { useCallback, useEffect, useSyncExternalStore } from "react";
import { apiFetch } from "./useRun";
import { parseGitStatus, type ProjectGitStatus } from "./useConversation";
import { applySharedGitStatus, gitStatusCacheKey, refreshSharedGitStatus } from "./threadGitStatusStore";

export class ThreadGitError extends Error {
  constructor(public code: string, message: string, public recoveryHint = "", public status = 0) {
    super(message);
    this.name = "ThreadGitError";
  }
}

async function postThreadGit(threadId: string, path: string, payload: unknown, fallback: string): Promise<any> {
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/${path}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload ?? {}),
  });
  const body = await res.json().catch(() => ({} as any));
  if (!res.ok) {
    const error = body?.error ?? {};
    const detail = typeof body?.detail === "string" ? body.detail : "";
    throw new ThreadGitError(
      String(error.code || "conversation.git.error"),
      String(error.message || detail || `${fallback}（HTTP ${res.status}）`),
      String(error.recovery_hint || ""),
      res.status,
    );
  }
  return body;
}

function applyStatus(threadId: string, raw: unknown): ProjectGitStatus | null {
  if (!raw || typeof raw !== "object") return null;
  const status = parseGitStatus(raw, { thread_id: threadId });
  applySharedGitStatus(gitStatusCacheKey(threadId), status);
  return status;
}

export interface ThreadGitCommitResult {
  sha: string;
  short_sha: string;
  summary: string;
  branch: string;
  amend: boolean;
  git: ProjectGitStatus | null;
}

export async function commitThreadGit(
  threadId: string,
  input: { message: string; paths?: string[]; amend?: boolean },
): Promise<ThreadGitCommitResult> {
  const body = await postThreadGit(threadId, "git/commit", {
    message: input.message,
    paths: input.paths && input.paths.length ? input.paths : undefined,
    amend: Boolean(input.amend),
  }, "提交失败");
  return {
    sha: String(body.sha || ""),
    short_sha: String(body.short_sha || ""),
    summary: String(body.summary || ""),
    branch: String(body.branch || ""),
    amend: Boolean(body.amend),
    git: applyStatus(threadId, body.git),
  };
}

export interface ThreadGitSyncResult {
  branch: string;
  remote?: string;
  upstream?: string | null;
  set_upstream?: boolean;
  output: string;
  ahead: number | null;
  behind: number | null;
  git: ProjectGitStatus | null;
}

function parseSync(threadId: string, body: any): ThreadGitSyncResult {
  return {
    branch: String(body.branch || ""),
    remote: body.remote ? String(body.remote) : undefined,
    upstream: body.upstream ? String(body.upstream) : null,
    set_upstream: Boolean(body.set_upstream),
    output: String(body.output || ""),
    ahead: typeof body.ahead === "number" ? body.ahead : null,
    behind: typeof body.behind === "number" ? body.behind : null,
    git: applyStatus(threadId, body.git),
  };
}

export async function pushThreadGit(threadId: string, input: { setUpstream?: boolean } = {}): Promise<ThreadGitSyncResult> {
  const payload = input.setUpstream === undefined ? {} : { set_upstream: input.setUpstream };
  const result = parseSync(threadId, await postThreadGit(threadId, "git/push", payload, "推送失败"));
  void refreshThreadPullRequests(threadId);
  return result;
}

export async function pullThreadGit(threadId: string, input: { rebase?: boolean } = {}): Promise<ThreadGitSyncResult> {
  return parseSync(threadId, await postThreadGit(threadId, "git/pull", { rebase: Boolean(input.rebase) }, "拉取失败"));
}

export interface ThreadPullRequestCreated {
  number: number;
  url: string;
  state: string;
  title: string;
  base: string;
  head: string;
  pushed: boolean;
}

export async function createThreadPullRequest(
  threadId: string,
  input: { title: string; body?: string; base?: string; draft?: boolean; pushFirst?: boolean },
): Promise<ThreadPullRequestCreated> {
  const body = await postThreadGit(threadId, "git/pull-request", {
    title: input.title,
    body: input.body || "",
    base: input.base || "",
    draft: Boolean(input.draft),
    push_first: Boolean(input.pushFirst),
  }, "创建 PR 失败");
  const created: ThreadPullRequestCreated = {
    number: Number(body.number || 0),
    url: String(body.url || ""),
    state: String(body.state || ""),
    title: String(body.title || input.title),
    base: String(body.base || ""),
    head: String(body.head || ""),
    pushed: Boolean(body.pushed),
  };
  if (created.pushed) void refreshSharedGitStatus(threadId);
  void refreshThreadPullRequests(threadId);
  return created;
}

// -- Shared pull-request listing ---------------------------------------------

export interface ThreadPullRequestRecord {
  number: number;
  title: string;
  state: "open" | "closed" | "draft" | "merged" | string;
  html_url: string;
  user_login: string;
  created_at: string;
  updated_at: string;
  base_ref: string;
  head_ref: string;
}

export interface ThreadPullRequestListing {
  state: "no_workspace" | "no_git" | "no_remote" | "not_github" | "error" | "no_pr" | "ok";
  branch: string | null;
  remote_url: string;
  owner: string;
  repo: string;
  error?: string;
  pull_requests: ThreadPullRequestRecord[];
}

interface PullRequestEntry {
  result: ThreadPullRequestListing | null;
  error: string;
  loading: boolean;
}

const EMPTY_PR: PullRequestEntry = { result: null, error: "", loading: false };
const prEntries = new Map<string, PullRequestEntry>();
const prListeners = new Map<string, Set<() => void>>();
const prInflight = new Map<string, Promise<void>>();

function writePr(threadId: string, patch: Partial<PullRequestEntry>): void {
  prEntries.set(threadId, { ...(prEntries.get(threadId) ?? EMPTY_PR), ...patch });
  for (const listener of prListeners.get(threadId) ?? []) listener();
}

export async function refreshThreadPullRequests(threadId: string): Promise<void> {
  const id = threadId.trim();
  if (!id) return;
  const pending = prInflight.get(id);
  if (pending) return pending;
  writePr(id, { loading: true, error: "" });
  const work = (async () => {
    try {
      const res = await apiFetch(`/api/threads/${encodeURIComponent(id)}/pull-requests`);
      const body = await res.json().catch(() => null);
      if (!res.ok || !body) {
        throw new Error(body?.error?.message || `读取 PR 失败（HTTP ${res.status}）`);
      }
      writePr(id, { result: body as ThreadPullRequestListing, error: "", loading: false });
    } catch (err) {
      writePr(id, { error: err instanceof Error ? err.message : "读取 PR 失败", loading: false });
    } finally {
      prInflight.delete(id);
    }
  })();
  prInflight.set(id, work);
  return work;
}

/** `autoload` fetches once when nothing is cached yet. */
export function useThreadPullRequests(threadId: string | undefined, autoload = true) {
  const id = (threadId || "").trim();
  const subscribe = useCallback((listener: () => void) => {
    if (!id) return () => {};
    let set = prListeners.get(id);
    if (!set) {
      set = new Set();
      prListeners.set(id, set);
    }
    set.add(listener);
    return () => {
      set!.delete(listener);
      if (set!.size === 0) prListeners.delete(id);
    };
  }, [id]);
  const getSnapshot = useCallback(() => (id ? prEntries.get(id) ?? EMPTY_PR : EMPTY_PR), [id]);
  const entry = useSyncExternalStore(subscribe, getSnapshot, () => EMPTY_PR);
  useEffect(() => {
    if (!autoload || !id) return;
    const current = prEntries.get(id);
    if (!current || (!current.result && !current.error && !current.loading)) void refreshThreadPullRequests(id);
  }, [autoload, id]);
  const refresh = useCallback(() => refreshThreadPullRequests(id), [id]);
  return { ...entry, refresh };
}
