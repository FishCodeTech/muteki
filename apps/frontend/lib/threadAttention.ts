
import type { ConversationState, ConversationThread } from "./useConversation";

function isActionableRequest(row: Record<string, unknown> | null | undefined): boolean {
  if (!row || typeof row !== "object") return false;
  const capability = row.response_capability;
  if (capability && typeof capability === "object" && (capability as { answerable?: unknown }).answerable === false) return false;
  return String(row.status || "pending") === "pending";
}

export function threadHasActionableApproval(thread: ConversationThread): boolean {
  const approvals = thread.state.pending_approvals;
  if (approvals) {
    for (const row of Object.values(approvals)) {
      if (isActionableRequest(row as Record<string, unknown>)) return true;
    }
  }
  return isActionableRequest(thread.state.pending_approval ?? null);
}

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
  if (thread.state.unread && thread.thread_id !== activeThreadId) return true;
  return false;
}

export type SubagentPendingKind = "approval" | "input";

export interface SubagentPending {
  thread: ConversationThread;
  kind: SubagentPendingKind;
}

export function attentionOwnerId(
  thread: ConversationThread,
  byId: Map<string, ConversationThread>,
): string {
  let rootId = thread.state.lineage?.root_thread_id || thread.state.lineage?.parent_thread_id || "";
  const seen = new Set([thread.thread_id]);
  while (rootId && !seen.has(rootId)) {
    seen.add(rootId);
    const parent = byId.get(rootId)?.state.lineage?.parent_thread_id;
    if (!parent) break;
    rootId = parent;
  }
  if (!rootId || rootId === thread.thread_id) return "";
  const root = byId.get(rootId);
  if (!root || root.state.status === "archived") return "";
  return rootId;
}

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
  active: number;
  total: number;
}

const ACTIVE_SUBAGENT_NODE = new Set(["pending", "running"]);

function threadHasLiveWork(thread: ConversationThread): boolean {
  if (thread.state.running_turn_id || thread.state.queued_turn) return true;
  return Boolean(
    Number(thread.state.queue_count || 0) > 0
    && !thread.state.queue_paused
    && !thread.state.queue_failed_item_id,
  );
}

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
