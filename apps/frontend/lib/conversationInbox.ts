
import type { ConversationInboxEvent, ThreadAttentionSummary } from "./threadNotifications";
import type { ConversationThread } from "./useConversation";

export type ConversationInboxSnapshot = {
  inbox_seq: number;
  broker_epoch: string;
  threads: ConversationThread[];
  summaries: ThreadAttentionSummary[];
};

export type ConversationInboxPatch = ConversationInboxSnapshot & {
  base_seq: number;
  removed_thread_ids: string[];
};

export function isInternalCtfThread(
  thread: Partial<Pick<ConversationThread, "thread_id" | "ctf_run_id">> | null | undefined,
): boolean {
  if (!thread) return false;
  const runId = typeof thread.ctf_run_id === "string" ? thread.ctf_run_id.trim() : "";
  if (runId.length > 0) return true;
  const threadId = typeof thread.thread_id === "string" ? thread.thread_id : "";
  return threadId.startsWith("ctf-pi-");
}

export function sanitizeConversationInboxSnapshot(
  snapshot: ConversationInboxSnapshot,
): ConversationInboxSnapshot {
  const excludedIds = new Set<string>();
  const threads = snapshot.threads.filter((thread) => {
    if (isInternalCtfThread(thread)) {
      if (thread.thread_id) excludedIds.add(thread.thread_id);
      return false;
    }
    return true;
  });
  const summaries = snapshot.summaries.filter(
    (summary) => !excludedIds.has(summary.thread_id) && !isInternalCtfThread({ thread_id: summary.thread_id }),
  );
  if (threads.length === snapshot.threads.length && summaries.length === snapshot.summaries.length) {
    return snapshot;
  }
  return {
    ...snapshot,
    threads,
    summaries,
  };
}

function patchInboxRows<T extends { thread_id: string }>(rows: T[], changed: T[], removed: string[]): T[] {
  if (!changed.length && !removed.length) return rows;
  const byId = new Map(rows.map((row) => [row.thread_id, row]));
  for (const id of removed) byId.delete(id);
  for (const row of changed) byId.set(row.thread_id, row);
  return [...byId.values()].sort((a, b) => a.thread_id < b.thread_id ? -1 : Number(a.thread_id > b.thread_id));
}

export function applyConversationInboxPatch(
  snapshot: ConversationInboxSnapshot | null,
  patch: ConversationInboxPatch,
): ConversationInboxSnapshot | null {
  if (!snapshot || snapshot.broker_epoch !== patch.broker_epoch || snapshot.inbox_seq !== patch.base_seq) return null;
  return sanitizeConversationInboxSnapshot({
    inbox_seq: patch.inbox_seq,
    broker_epoch: patch.broker_epoch,
    threads: patchInboxRows(snapshot.threads, patch.threads, patch.removed_thread_ids),
    summaries: patchInboxRows(snapshot.summaries, patch.summaries, patch.removed_thread_ids),
  });
}

export type ConversationInboxStreamState = {
  appliedSeq: number;
  brokerEpoch: string;
  seenEventIds: Set<string>;
};

export function emptyConversationInboxState(): ConversationInboxStreamState {
  return { appliedSeq: 0, brokerEpoch: "", seenEventIds: new Set() };
}

export function acceptConversationInboxEvent(
  state: ConversationInboxStreamState,
  event: ConversationInboxEvent,
): { accepted: boolean; state: ConversationInboxStreamState } {
  const epoch = String(event.broker_epoch || "");
  if (epoch && epoch !== state.brokerEpoch) state = { ...emptyConversationInboxState(), brokerEpoch: epoch };
  const seq = Number(event.seq || 0);
  const eventId = String(event.event_id || "");
  if (eventId && state.seenEventIds.has(eventId)) {
    return { accepted: false, state };
  }
  if (seq > 0 && seq <= state.appliedSeq) {
    return { accepted: false, state };
  }
  const seenEventIds = new Set(state.seenEventIds);
  if (eventId) {
    seenEventIds.add(eventId);
    if (seenEventIds.size > 500) {
      const trimmed = [...seenEventIds].slice(-400);
      seenEventIds.clear();
      for (const id of trimmed) seenEventIds.add(id);
    }
  }
  return {
    accepted: true,
    state: {
      appliedSeq: Math.max(state.appliedSeq, seq),
      brokerEpoch: epoch || state.brokerEpoch,
      seenEventIds,
    },
  };
}

export function upsertConversationThread(
  threads: ConversationThread[],
  next: ConversationThread,
): ConversationThread[] {
  const index = threads.findIndex((thread) => thread.thread_id === next.thread_id);
  if (index < 0) return [next, ...threads];
  const merged = {
    ...threads[index],
    ...next,
    state: {
      ...threads[index].state,
      ...next.state,
    },
  };
  if (sameJsonValue(threads[index], merged)) return threads;
  const copy = threads.slice();
  copy[index] = merged;
  return copy;
}

function sameJsonValue(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (!a || !b || typeof a !== "object" || typeof b !== "object") return false;
  if (Array.isArray(a)) {
    return Array.isArray(b) && a.length === b.length && a.every((value, index) => sameJsonValue(value, b[index]));
  }
  if (Array.isArray(b)) return false;
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const leftKeys = Object.keys(left);
  return leftKeys.length === Object.keys(right).length
    && leftKeys.every((key) => Object.hasOwn(right, key) && sameJsonValue(left[key], right[key]));
}

export function reconcileConversationThreads(
  previous: ConversationThread[],
  next: ConversationThread[],
): ConversationThread[] {
  if (previous === next) return previous;
  const byId = new Map(previous.map((thread) => [thread.thread_id, thread]));
  let changed = previous.length !== next.length;
  const result = next.map((thread, index) => {
    const prior = byId.get(thread.thread_id);
    const kept = prior && sameJsonValue(prior, thread) ? prior : thread;
    if (kept !== previous[index]) changed = true;
    return kept;
  });
  return changed ? result : previous;
}
