
"use client";

import { useCallback, useEffect, useSyncExternalStore } from "react";
import {
  checkoutProjectBranch,
  checkoutThreadBranch,
  fetchProjectGitStatus,
  fetchThreadGitStatus,
  type ProjectGitStatus,
} from "./useConversation";
import { readCacheIdentity, subscribeReadCacheIdentity } from "./readCache";
import { useConversationLocalPreview } from "./conversationPreviewMode";

export type SharedGitStatusEntry = {
  status: ProjectGitStatus | null;
  error: string;
  loading: boolean;
  revision: number;
};

const EMPTY_ENTRY: SharedGitStatusEntry = {
  status: null,
  error: "",
  loading: false,
  revision: 0,
};

const entries = new Map<string, SharedGitStatusEntry>();
const listeners = new Map<string, Set<() => void>>();
const inflight = new Map<string, Promise<void>>();
const epochs = new Map<string, number>();
let ownerIdentity = readCacheIdentity();
function currentOwner(): string {
  const next = readCacheIdentity();
  if (next !== ownerIdentity) {
    ownerIdentity = next;
    entries.clear(); inflight.clear(); epochs.clear();
  }
  return next;
}
subscribeReadCacheIdentity(() => {
  currentOwner();
  for (const set of listeners.values()) for (const listener of set) listener();
});

export function gitStatusCacheKey(threadId?: string, projectId?: string): string {
  currentOwner();
  const thread = (threadId || "").trim();
  if (thread) return `${ownerIdentity}:thread:${thread}`;
  const project = (projectId || "").trim();
  if (project) return `${ownerIdentity}:project:${project}`;
  return "";
}

function emit(key: string): void {
  const set = listeners.get(key);
  if (!set) return;
  for (const listener of set) listener();
}

export function readSharedGitStatus(key: string): SharedGitStatusEntry {
  currentOwner();
  if (!key) return EMPTY_ENTRY;
  return entries.get(key) ?? EMPTY_ENTRY;
}

function writeSharedGitStatus(key: string, patch: Partial<SharedGitStatusEntry>): void {
  if (!key || !key.startsWith(`${currentOwner()}:`)) return;
  const prev = readSharedGitStatus(key);
  const next: SharedGitStatusEntry = {
    status: patch.status !== undefined ? patch.status : prev.status,
    error: patch.error !== undefined ? patch.error : prev.error,
    loading: patch.loading !== undefined ? patch.loading : prev.loading,
    revision: prev.revision + 1,
  };
  entries.set(key, next);
  emit(key);
}

export function applySharedGitStatus(key: string, status: ProjectGitStatus): void {
  epochs.set(key, (epochs.get(key) ?? 0) + 1);
  writeSharedGitStatus(key, { status, error: "", loading: false });
}

export function subscribeSharedGitStatus(key: string, listener: () => void): () => void {
  if (!key) return () => {};
  let set = listeners.get(key);
  if (!set) {
    set = new Set();
    listeners.set(key, set);
  }
  set.add(listener);
  return () => {
    set!.delete(listener);
    if (set!.size === 0) listeners.delete(key);
  };
}

export async function refreshSharedGitStatus(
  threadId?: string,
  projectId?: string,
): Promise<ProjectGitStatus | null> {
  const key = gitStatusCacheKey(threadId, projectId);
  if (!key) return null;

  const pending = inflight.get(key);
  if (pending) {
    await pending;
    return readSharedGitStatus(key).status;
  }

  const requestOwner = currentOwner();
  const epoch = epochs.get(key) ?? 0;
  writeSharedGitStatus(key, { loading: true, error: "" });
  const work: Promise<void> = (async () => {
    try {
      const status = (threadId || "").trim()
        ? await fetchThreadGitStatus(threadId!.trim())
        : await fetchProjectGitStatus(projectId!.trim());
      if (requestOwner !== currentOwner() || (epochs.get(key) ?? 0) !== epoch) return;
      writeSharedGitStatus(key, { status, error: "", loading: false });
    } catch (err) {
      if (requestOwner !== currentOwner() || (epochs.get(key) ?? 0) !== epoch) return;
      writeSharedGitStatus(key, {
        status: null,
        error: err instanceof Error ? err.message : "读取分支失败",
        loading: false,
      });
    }
  })();
  inflight.set(key, work);
  try {
    await work;
  } finally {
    if (inflight.get(key) === work) inflight.delete(key);
  }
  return readSharedGitStatus(key).status;
}

export async function checkoutSharedBranch(
  threadId: string | undefined,
  projectId: string | undefined,
  branch: string,
  options: { create?: boolean } = {},
): Promise<ProjectGitStatus> {
  const key = gitStatusCacheKey(threadId, projectId);
  const owner = currentOwner();
  const checkoutEpoch = (epochs.get(key) ?? 0) + 1;
  epochs.set(key, checkoutEpoch);
  const next = (threadId || "").trim()
    ? await checkoutThreadBranch(threadId!.trim(), branch, options)
    : await checkoutProjectBranch(projectId!.trim(), branch, options);
  if (key && owner === currentOwner() && epochs.get(key) === checkoutEpoch) applySharedGitStatus(key, next);
  return next;
}

export function useSharedGitStatus(threadId?: string, projectId?: string) {
  const localPreview = useConversationLocalPreview();
  const key = localPreview ? "" : gitStatusCacheKey(threadId, projectId);
  const subscribe = useCallback(
    (onStoreChange: () => void) => subscribeSharedGitStatus(key, onStoreChange),
    [key],
  );
  const getSnapshot = useCallback(() => readSharedGitStatus(key), [key]);
  const entry = useSyncExternalStore(subscribe, getSnapshot, () => EMPTY_ENTRY);

  useEffect(() => {
    if (!key) return;
    void refreshSharedGitStatus(threadId, projectId);
  }, [key, threadId, projectId]);

  const refresh = useCallback(
    (): Promise<ProjectGitStatus | null> => (
      localPreview ? Promise.resolve(null) : refreshSharedGitStatus(threadId, projectId)
    ),
    [localPreview, threadId, projectId],
  );

  return {
    status: entry.status,
    error: entry.error,
    loading: entry.loading,
    revision: entry.revision,
    cacheKey: key,
    refresh,
  };
}
