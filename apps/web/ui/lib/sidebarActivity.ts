import type { ConversationThread } from "./useConversation";
import type { InboxState } from "./sidebarInbox";
import { attentionOwnerId, threadHasAttention, threadNeedsAction, type SubagentPending } from "./threadAttention";

export interface ActivityPrefs {
  showPriority: boolean;
  showRunning: boolean;
  showPinned: boolean;
}

export function buildSidebarActivity({
  threads, activeThreadId, pinnedIds, inboxStates, subagentPending, prefs,
  clearedAt, searchQuery, projectId, retainedPriorityIds,
}: {
  threads: ConversationThread[];
  activeThreadId: string;
  pinnedIds: string[];
  inboxStates: Map<string, InboxState>;
  subagentPending: Map<string, SubagentPending[]>;
  prefs: ActivityPrefs;
  clearedAt: number;
  searchQuery: string;
  projectId: string | null;
  retainedPriorityIds: string[];
}) {
  const byId = new Map(threads.map((thread) => [thread.thread_id, thread]));
  const pins = new Set(pinnedIds);
  const retained = new Set(retainedPriorityIds);
  const active = byId.get(activeThreadId);
  const activeOwner = active ? attentionOwnerId(active, byId) || activeThreadId : activeThreadId;
  const needsAction = (thread: ConversationThread) => threadNeedsAction(thread) || subagentPending.has(thread.thread_id);
  const unread = (thread: ConversationThread) => Boolean(thread.state.unread) && thread.thread_id !== activeThreadId;
  const running = (thread: ConversationThread) => Boolean(thread.state.running_turn_id);
  const timestamp = (thread: ConversationThread) => Date.parse(thread.updated_at || thread.created_at || "") || 0;
  const candidates = threads.filter((thread) => (
    thread.state.status !== "archived"
    && !attentionOwnerId(thread, byId)
    && (inboxStates.get(thread.thread_id)?.placement ?? "active") === "active"
  )).sort((a, b) => timestamp(b) - timestamp(a) || a.thread_id.localeCompare(b.thread_id));

  // Display switches change grouping, never whether a live request or running
  // turn is safe to clear. Keep the open chat in place after its read receipt.
  const clearable = (thread: ConversationThread) => (
    !needsAction(thread) && !running(thread) && !unread(thread)
    && thread.thread_id !== activeOwner && !pins.has(thread.thread_id)
  );
  const query = searchQuery.trim().toLowerCase();
  const visible = candidates.filter((thread) => (
    (!projectId || thread.project_id === projectId)
    && (!query || `${thread.title} ${thread.summary || ""} ${thread.thread_id}`.toLowerCase().includes(query))
    && (!clearedAt || query || !clearable(thread) || timestamp(thread) > clearedAt)
  ));
  const priorityRank = (thread: ConversationThread) => (
    needsAction(thread) ? 0 : prefs.showRunning && running(thread) ? 1
      : unread(thread) ? 2 : (retained.has(thread.thread_id) && !running(thread)) || thread.thread_id === activeOwner ? 3 : 4
  );
  const priority = prefs.showPriority
    ? visible.filter((thread) => priorityRank(thread) < 4).sort((a, b) => priorityRank(a) - priorityRank(b) || timestamp(b) - timestamp(a))
    : [];
  const grouped = new Set(priority.map((thread) => thread.thread_id));
  const pinned = prefs.showPinned ? visible.filter((thread) => pins.has(thread.thread_id) && !grouped.has(thread.thread_id)) : [];
  for (const thread of pinned) grouped.add(thread.thread_id);
  return {
    priority,
    pinned,
    recent: visible.filter((thread) => !grouped.has(thread.thread_id)),
    count: candidates.filter((thread) => needsAction(thread) || threadHasAttention(thread, activeThreadId)).length,
    unreadIds: visible.filter(unread).map((thread) => thread.thread_id),
    readCount: query ? 0 : visible.filter(clearable).length,
    clearableIds: candidates.filter(clearable).map((thread) => thread.thread_id),
  };
}
