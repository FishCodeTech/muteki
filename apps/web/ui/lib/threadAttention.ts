/**
 * Thread attention helpers for Conversation sidebar / activity badge (C29).
 */

import type { ConversationState, ConversationThread } from "./useConversation";

function isActionableRequest(row: Record<string, unknown> | null | undefined): boolean {
  if (!row || typeof row !== "object") return false;
  const capability = row.response_capability;
  if (capability && typeof capability === "object" && (capability as { answerable?: unknown }).answerable === false) return false;
  return String(row.status || "pending") === "pending";
}

/** True when the thread still has a decidable approval (expired ≠ actionable). */
export function threadHasActionableApproval(thread: ConversationThread): boolean {
  const approvals = thread.state.pending_approvals;
  if (approvals) {
    for (const row of Object.values(approvals)) {
      if (isActionableRequest(row as Record<string, unknown>)) return true;
    }
  }
  return isActionableRequest(thread.state.pending_approval ?? null);
}

/** Pending approval / user input counts as 待办 even without running_turn_id. */
export function threadNeedsAction(thread: ConversationThread): boolean {
  return Boolean(
    threadHasActionableApproval(thread)
    || isActionableRequest(thread.state.pending_user_input),
  );
}

export function threadHasAttention(
  thread: ConversationThread,
  activeThreadId = "",
): boolean {
  if (thread.state.status === "archived") return false;
  if (threadNeedsAction(thread)) return true;
  // A failure remains in the thread history after it has been viewed. The
  // unread watermark, rather than the persistent error record, owns its badge.
  if (thread.state.unread && thread.thread_id !== activeThreadId) return true;
  return false;
}

export type SubagentPendingKind = "approval" | "input";

export interface SubagentPending {
  thread: ConversationThread;
  kind: SubagentPendingKind;
}

/**
 * Root Thread that carries a subagent child's attention, or "" when the child
 * must keep its own (root not loaded or archived, so nothing would show it).
 * Mirrors ``collect_attention_rows`` on the server.
 */
export function attentionOwnerId(
  thread: ConversationThread,
  byId: Map<string, ConversationThread>,
): string {
  const rootId = thread.state.lineage?.root_thread_id || "";
  if (!rootId || rootId === thread.thread_id) return "";
  const root = byId.get(rootId);
  if (!root || root.state.status === "archived") return "";
  return rootId;
}

/** Blocking pending items of active subagent descendants, keyed by root Thread id. */
export function subagentPendingByRoot(
  threads: ConversationThread[],
): Map<string, SubagentPending[]> {
  const byId = new Map(threads.map((thread) => [thread.thread_id, thread]));
  const result = new Map<string, SubagentPending[]>();
  for (const thread of threads) {
    const rootId = attentionOwnerId(thread, byId);
    if (!rootId || thread.state.status === "archived" || !threadNeedsAction(thread)) continue;
    const entry: SubagentPending = {
      thread,
      kind: threadHasActionableApproval(thread) ? "approval" : "input",
    };
    const list = result.get(rootId);
    if (list) list.push(entry);
    else result.set(rootId, [entry]);
  }
  for (const list of result.values()) {
    list.sort((a, b) => Number(a.kind !== "approval") - Number(b.kind !== "approval"));
  }
  return result;
}

export interface SubagentActivity {
  /** Descendants still in progress: a running turn, or a spawn that has not finished. */
  active: number;
  /** Every non-archived descendant the root delegated to. */
  total: number;
}

const ACTIVE_SUBAGENT_NODE = new Set(["pending", "running"]);

/**
 * Mirrors the backend live status: a spawned child's first turn waits in
 * queued status until the runtime starts it, and follow-ups wait in the
 * queue, so both count as in progress unless the queue is paused or its
 * item failed.
 */
function threadHasLiveWork(thread: ConversationThread): boolean {
  if (thread.state.running_turn_id || thread.state.queued_turn) return true;
  return Boolean(
    Number(thread.state.queue_count || 0) > 0
    && !thread.state.queue_paused
    && !thread.state.queue_failed_item_id,
  );
}

/**
 * Delegation activity of the app-owned subagent descendants of each root
 * Thread. Child rows are the live source (their running/queue flags reach the
 * inbox stream); a Thread's own subagent nodes stand in for children whose
 * rows are not listed yet.
 */
export function subagentActivityByRoot(
  threads: ConversationThread[],
): Map<string, SubagentActivity> {
  const byId = new Map(threads.map((thread) => [thread.thread_id, thread]));
  const result = new Map<string, SubagentActivity>();
  const add = (rootId: string, active: boolean) => {
    const entry = result.get(rootId) ?? { active: 0, total: 0 };
    entry.total += 1;
    if (active) entry.active += 1;
    result.set(rootId, entry);
  };
  for (const thread of threads) {
    if (thread.state.status === "archived") continue;
    const ownerId = attentionOwnerId(thread, byId);
    if (ownerId) add(ownerId, threadHasLiveWork(thread));
    for (const node of thread.state.subagents || []) {
      if (node.origin && node.origin !== "app_owned") continue;
      const childId = node.thread_id || node.agent_id;
      if (!childId || byId.has(childId)) continue;
      add(ownerId || thread.thread_id, ACTIVE_SUBAGENT_NODE.has(String(node.status)));
    }
  }
  return result;
}

/** Direct subagents of one Thread (Muteki-owned and runtime-reported), for its own controls. */
export function threadSubagentCounts(state: ConversationState | null | undefined): SubagentActivity {
  const counts: SubagentActivity = { active: 0, total: 0 };
  for (const node of [...(state?.subagents || []), ...(state?.agents?.agents || [])]) {
    counts.total += 1;
    if (ACTIVE_SUBAGENT_NODE.has(String(node.status))) counts.active += 1;
  }
  return counts;
}

export function countThreadAttention(
  threads: ConversationThread[],
  activeThreadId = "",
): number {
  const byId = new Map(threads.map((thread) => [thread.thread_id, thread]));
  const delegated = subagentPendingByRoot(threads);
  return threads.reduce((total, thread) => {
    if (attentionOwnerId(thread, byId)) return total;
    const counted = threadHasAttention(thread, activeThreadId)
      || (thread.state.status !== "archived" && delegated.has(thread.thread_id));
    return total + (counted ? 1 : 0);
  }, 0);
}
