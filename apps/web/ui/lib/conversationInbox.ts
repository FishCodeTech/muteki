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
  const copy = threads.slice();
  copy[index] = {
    ...copy[index],
    ...next,
    state: {
      ...copy[index].state,
      ...next.state,
    },
  };
  return copy;
}
