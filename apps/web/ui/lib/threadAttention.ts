/**
 * Thread attention helpers for Conversation sidebar / activity badge (C29).
 */

import type { ConversationThread } from "./useConversation";

function isActionableApproval(row: Record<string, unknown> | null | undefined): boolean {
  if (!row || typeof row !== "object") return false;
  return String(row.status || "pending") === "pending";
}

/** True when the thread still has a decidable approval (expired ≠ actionable). */
export function threadHasActionableApproval(thread: ConversationThread): boolean {
  const approvals = thread.state.pending_approvals;
  if (approvals) {
    for (const row of Object.values(approvals)) {
      if (isActionableApproval(row as Record<string, unknown>)) return true;
    }
  }
  return isActionableApproval(thread.state.pending_approval ?? null);
}

/** Pending approval / user input counts as 待办 even without running_turn_id. */
export function threadNeedsAction(thread: ConversationThread): boolean {
  return Boolean(
    threadHasActionableApproval(thread)
    || thread.state.pending_user_input,
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

export function countThreadAttention(
  threads: ConversationThread[],
  activeThreadId = "",
): number {
  return threads.reduce(
    (total, thread) => total + (threadHasAttention(thread, activeThreadId) ? 1 : 0),
    0,
  );
}
