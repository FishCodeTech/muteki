
"use client";

import { useCallback, useEffect, useSyncExternalStore } from "react";
import { readCacheIdentity, subscribeReadCacheIdentity } from "./readCache";
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
  const owner = readCacheIdentity();
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/${path}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload ?? {}),
  });
  const body = await res.json().catch(() => ({} as any));
  if (owner !== readCacheIdentity()) throw new ThreadGitError("conversation.git.scope_changed", "原 Git 操作属于此前的工作台身份，请到原工作台核对结果");
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
  if (result.git?.pull_requests_supported !== false) void refreshThreadPullRequests(threadId);
  return result;
}

export async function pullThreadGit(threadId: string, input: { rebase?: boolean } = {}): Promise<ThreadGitSyncResult> {
  return parseSync(threadId, await postThreadGit(threadId, "git/pull", { rebase: Boolean(input.rebase) }, "拉取失败"));
}

export async function checkoutThreadGit(
  threadId: string,
  input: { branch: string; create?: boolean },
): Promise<ProjectGitStatus | null> {
  const body = await postThreadGit(threadId, "git/checkout", { branch: input.branch, create: Boolean(input.create) }, "切换分支失败");
  return applyStatus(threadId, body);
}

export async function initThreadGit(threadId: string): Promise<ProjectGitStatus | null> {
  const body = await postThreadGit(threadId, "git/init", {}, "初始化 Git 失败");
  return applyStatus(threadId, body);
}

export type MergeWorktreeOutcome = "fast_forward" | "merged" | "squashed" | string;

export interface MergeWorktreeResult {
  outcome: MergeWorktreeOutcome;
  message: string;
  source_branch?: string;
  git: ProjectGitStatus | null;
}

export async function mergeWorktreeThreadGit(
  threadId: string,
  input: { squash?: boolean } = {},
): Promise<MergeWorktreeResult> {
  const body = await postThreadGit(threadId, "git/merge-worktree", { squash: Boolean(input.squash) }, "合并 worktree 失败");
  return {
    outcome: String(body.outcome || ""),
    message: String(body.message || ""),
    source_branch: body.source_branch ? String(body.source_branch) : undefined,
    git: applyStatus(threadId, body.git),
  };
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
let prOwner = readCacheIdentity();
function prKey(threadId: string): string {
  const owner = readCacheIdentity();
  if (owner !== prOwner) { prOwner = owner; prEntries.clear(); prInflight.clear(); }
  return `${owner}:${threadId}`;
}
subscribeReadCacheIdentity(() => {
  prKey("");
  for (const set of prListeners.values()) for (const listener of set) listener();
});

function writePr(threadId: string, patch: Partial<PullRequestEntry>): void {
  prEntries.set(threadId, { ...(prEntries.get(threadId) ?? EMPTY_PR), ...patch });
  for (const listener of prListeners.get(threadId) ?? []) listener();
}

export async function refreshThreadPullRequests(threadId: string): Promise<void> {
  const id = threadId.trim();
  if (!id) return;
  const key = prKey(id);
  const owner = prOwner;
  const pending = prInflight.get(key);
  if (pending) return pending;
  writePr(key, { loading: true, error: "" });
  const work = (async () => {
    try {
      const res = await apiFetch(`/api/threads/${encodeURIComponent(id)}/pull-requests`);
      const body = await res.json().catch(() => null);
      if (!res.ok || !body) {
        throw new Error(body?.error?.message || `读取 PR 失败（HTTP ${res.status}）`);
      }
      if (owner !== readCacheIdentity()) return;
      writePr(key, { result: body as ThreadPullRequestListing, error: "", loading: false });
    } catch (err) {
      if (owner !== readCacheIdentity()) return;
      writePr(key, { error: err instanceof Error ? err.message : "读取 PR 失败", loading: false });
    } finally {
      if (owner === readCacheIdentity()) prInflight.delete(key);
    }
  })();
  prInflight.set(key, work);
  return work;
}

export function useThreadPullRequests(threadId: string | undefined, autoload = true) {
  const id = (threadId || "").trim();
  const key = id ? prKey(id) : "";
  const subscribe = useCallback((listener: () => void) => {
    if (!id) return () => {};
    let set = prListeners.get(key);
    if (!set) {
      set = new Set();
      prListeners.set(key, set);
    }
    set.add(listener);
    return () => {
      set!.delete(listener);
      if (set!.size === 0) prListeners.delete(key);
    };
  }, [key]);
  const getSnapshot = useCallback(() => (key ? prEntries.get(key) ?? EMPTY_PR : EMPTY_PR), [key]);
  const entry = useSyncExternalStore(subscribe, getSnapshot, () => EMPTY_PR);
  useEffect(() => {
    if (!autoload || !id) return;
    const current = prEntries.get(key);
    if (!current || (!current.result && !current.error && !current.loading)) void refreshThreadPullRequests(id);
  }, [autoload, id, key]);
  const refresh = useCallback(() => refreshThreadPullRequests(id), [id]);
  return { ...entry, refresh };
}
