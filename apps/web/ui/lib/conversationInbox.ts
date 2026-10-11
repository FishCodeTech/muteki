/**
 * Inbox SSE admission helpers (C29).
 *
 * Mirrors the per-thread stream habit: advance only on unseen event_id / seq.
 */

import type { ConversationInboxEvent } from "./threadNotifications";
import type { ConversationThread } from "./useConversation";

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
  if (sameConversationThread(threads[index], merged)) return threads;
  const copy = threads.slice();
  copy[index] = merged;
  return copy;
}

function sameConversationThread(a: ConversationThread, b: ConversationThread): boolean {
  return a === b || JSON.stringify(a) === JSON.stringify(b);
}

/**
 * Adopt a freshly fetched thread list while keeping the previous objects for
 * threads whose content did not change. Returns `previous` itself when the
 * list is unchanged, so memoized sidebar rows and selectors can bail out.
 */
export function reconcileConversationThreads(
  previous: ConversationThread[],
  next: ConversationThread[],
): ConversationThread[] {
  if (previous === next) return previous;
  const byId = new Map(previous.map((thread) => [thread.thread_id, thread]));
  let changed = previous.length !== next.length;
  const result = next.map((thread, index) => {
    const prior = byId.get(thread.thread_id);
    const kept = prior && sameConversationThread(prior, thread) ? prior : thread;
    if (kept !== previous[index]) changed = true;
    return kept;
  });
  return changed ? result : previous;
}
