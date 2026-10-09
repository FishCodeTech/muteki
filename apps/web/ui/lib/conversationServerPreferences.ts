"use client";

import { useEffect, useSyncExternalStore } from "react";
import { CHAT_PREFERENCES_KEY } from "@/lib/chatPreferences";
import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope } from "./conversationStorageScope";
import { isDesktopRemote } from "./desktopEnvironment";
import { apiFetch } from "@/lib/useRun";

/** Preferences the Muteki service acts on itself, so they apply with no window open. */
export interface ConversationServerPreferences {
  autoResumeOnQuotaReset: boolean;
}

type Snapshot =
  | { status: "loading" }
  | { status: "ready"; prefs: ConversationServerPreferences }
  | { status: "error"; message: string };

const LOADING: Snapshot = { status: "loading" };
let snapshot: Snapshot = LOADING;
let inflight: Promise<void> | null = null;
let generation = 0;
const listeners = new Set<() => void>();

function publish(next: Snapshot) {
  snapshot = next;
  listeners.forEach((listener) => listener());
}

async function errorMessage(response: Response): Promise<string> {
  try {
    const body = await response.json() as { error?: { message?: string } };
    if (body.error?.message) return body.error.message;
  } catch { /* fall through to the status line */ }
  return `HTTP ${response.status}`;
}

async function put(prefs: ConversationServerPreferences, expectedVersion?: number): Promise<ConversationServerPreferences> {
  const response = await apiFetch("/api/conversation/preferences", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ auto_resume_on_quota_reset: prefs.autoResumeOnQuotaReset, ...(expectedVersion === undefined ? {} : {version: expectedVersion}) }),
  });
  if (!response.ok && !(expectedVersion !== undefined && response.status === 409)) throw new Error(await errorMessage(response));
  const body = await response.json() as { auto_resume_on_quota_reset?: boolean };
  return { autoResumeOnQuotaReset: body.auto_resume_on_quota_reset === true };
}

/** Absence and an explicitly disabled legacy value are different states. */
function legacyLocalValue(): boolean | undefined {
  const raw = window.localStorage.getItem(CHAT_PREFERENCES_KEY);
  if (!raw) return undefined;
  const row = JSON.parse(raw) as Record<string, unknown>;
  return typeof row.autoResumeOnQuotaReset === "boolean" ? row.autoResumeOnQuotaReset : undefined;
}

function load(): Promise<void> {
  if (inflight) return inflight;
  const gen = generation, scope = conversationStorageScope();
  const task = (async () => {
    try {
      const response = await apiFetch("/api/conversation/preferences");
      if (!response.ok) throw new Error(await errorMessage(response));
      const body = await response.json() as { version?: number; auto_resume_on_quota_reset?: boolean };
      if (gen !== generation || scope !== conversationStorageScope()) return;
      let prefs: ConversationServerPreferences = { autoResumeOnQuotaReset: body.auto_resume_on_quota_reset === true };
      const marker = conversationStorageKey("muteki.quota-resume.migrated.v1", scope);
      if (scope && !isDesktopRemote() && localStorage.getItem(marker) !== "1") {
        const legacy = legacyLocalValue();
        if (body.version === 0 && legacy !== undefined) prefs = await put({autoResumeOnQuotaReset: legacy}, 0);
        if (gen !== generation || scope !== conversationStorageScope()) return;
        if (body.version !== undefined) localStorage.setItem(marker, "1");
      }
      if (gen !== generation || scope !== conversationStorageScope()) return;
      publish({ status: "ready", prefs });
    } catch (cause) {
      if (gen === generation) publish({ status: "error", message: cause instanceof Error ? cause.message : String(cause) });
    }
  })();
  inflight = task;
  void task.finally(() => { if (inflight === task) inflight = null; });
  return task;
}

export async function writeConversationServerPreferences(patch: Partial<ConversationServerPreferences>): Promise<void> {
  const base = snapshot.status === "ready" ? snapshot.prefs : { autoResumeOnQuotaReset: false };
  const gen = generation;
  const prefs = await put({ ...base, ...patch });
  if (gen === generation) publish({ status: "ready", prefs });
}

/** Persist a thread's plan/default preference; the server rejects `plan` when the runtime cannot plan. */
export async function saveThreadInteractionMode(threadId: string, mode: "default" | "plan"): Promise<void> {
  const response = await apiFetch("/api/conversation/preferences", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ thread_id: threadId, interaction_mode: mode }),
  });
  if (!response.ok) throw new Error(await errorMessage(response));
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function useConversationServerPreferences(): Snapshot {
  const current = useSyncExternalStore(subscribe, () => snapshot, () => LOADING);
  useEffect(() => {
    if (snapshot.status !== "ready") void load();
  }, []);
  return current;
}

subscribeConversationStorageScope(() => { generation++; inflight = null; publish(LOADING); if (listeners.size && conversationStorageScope()) void load(); });
