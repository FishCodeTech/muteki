import type { DesktopDockBadgeThread } from "./desktopChatBridge";
import type { ConversationThread } from "./useConversation";
import { conversationStorageKey, conversationStorageScope } from "./conversationStorageScope";
import { isInternalCtfThread } from "./conversationInbox";
import { inboxStateOf, nextWakeAt } from "./sidebarInbox";
import { currentSidebarThreadStates } from "./sidebarThreadBridge";
import {
  attentionOwnerId,
  subagentActivityByRoot,
  subagentPendingByRoot,
  threadNeedsAction,
} from "./threadAttention";

const SETTLED_KEY = "muteki.sidebar.settled-at.v1";
const SNOOZED_KEY = "muteki.sidebar.snoozed-until.v1";

function readTimeMap(key: string): Record<string, number> {
  if (typeof window === "undefined" || !conversationStorageScope()) return {};
  try {
    const saved = JSON.parse(
      window.localStorage.getItem(conversationStorageKey(key)) || "{}",
    ) as Record<string, unknown>;
    const result: Record<string, number> = {};
    for (const [id, value] of Object.entries(saved || {})) {
      if (typeof value === "number" && Number.isFinite(value) && value > 0) {
        result[id] = value;
      }
    }
    return result;
  } catch {
    return {};
  }
}

function isArchived(thread: ConversationThread): boolean {
  return (
    Boolean((thread as { archived?: boolean }).archived)
    || thread.state.status === "archived"
  );
}

export function collectFinishedUnreadThreads(
  threads: ConversationThread[],
  nowMs: number = Date.now(),
): {
  threads: DesktopDockBadgeThread[];
  nextWakeAt: number | null;
} {
  const chatThreads = threads.filter((thread) => !isInternalCtfThread(thread));
  const byId = new Map(chatThreads.map((thread) => [thread.thread_id, thread]));
  const subagentPending = subagentPendingByRoot(chatThreads);
  const subagentActivity = subagentActivityByRoot(chatThreads);

  const visibleIdsByRoot = new Map<string, string[]>();
  for (const thread of chatThreads) {
    if (isArchived(thread)) continue;
    const ownerId = attentionOwnerId(thread, byId) || thread.thread_id;
    let family = visibleIdsByRoot.get(ownerId);
    if (!family) {
      family = [ownerId];
      visibleIdsByRoot.set(ownerId, family);
    }
    if (thread.thread_id !== ownerId && !family.includes(thread.thread_id)) {
      family.push(thread.thread_id);
    }
  }

  const settledAt = readTimeMap(SETTLED_KEY);
  const snoozedUntil = readTimeMap(SNOOZED_KEY);
  const hasPersisted =
    Object.keys(settledAt).length > 0 || Object.keys(snoozedUntil).length > 0;
  const sidebarStates = currentSidebarThreadStates();

  const entries: DesktopDockBadgeThread[] = [];
  for (const thread of chatThreads) {
    if (isArchived(thread)) continue;
    if (attentionOwnerId(thread, byId)) continue;

    const inbox = inboxStateOf(
      thread,
      settledAt,
      snoozedUntil,
      nowMs,
      subagentPending.has(thread.thread_id),
    );
    const livePlacement = !hasPersisted
      ? sidebarStates.get(thread.thread_id)?.placement
      : undefined;
    const placement = livePlacement ?? inbox.placement;
    if (placement !== "active") continue;

    const running = Boolean(
      thread.state.running_turn_id
      || thread.state.status === "running"
      || subagentActivity.get(thread.thread_id)?.active,
    );
    const willContinue = Boolean(
      thread.state.queued_turn
      || thread.state.quota_resume
      || (
        Number(thread.state.queue_count || 0) > 0
        && !thread.state.queue_paused
        && !thread.state.queue_failed_item_id
      ),
    );
    const needsAction =
      threadNeedsAction(thread) || subagentPending.has(thread.thread_id);

    if (!thread.state.unread || running || willContinue || needsAction) {
      continue;
    }

    entries.push({
      threadId: thread.thread_id,
      visibleThreadIds: visibleIdsByRoot.get(thread.thread_id) ?? [thread.thread_id],
    });
  }

  entries.sort((a, b) => a.threadId.localeCompare(b.threadId));
  return {
    threads: entries,
    nextWakeAt: nextWakeAt(snoozedUntil, nowMs),
  };
}
