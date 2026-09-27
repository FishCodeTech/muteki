/**
 * Single admission path for Conversation SSE events.
 *
 * Events are accepted once (by event_id, else seq). liveText and appliedSeq
 * update only for accepted events so reconnect/replay cannot double-append
 * streaming assistant text.
 */

export interface ConversationStreamEvent {
  event_type: string;
  payload: Record<string, unknown>;
  seq: number;
  occurred_at?: string;
  event_id?: string;
}

export interface ConversationStreamState {
  events: ConversationStreamEvent[];
  liveText: string;
  appliedSeq: number;
}

export interface ConversationStreamResult {
  state: ConversationStreamState;
  accepted: boolean;
  scheduleRefresh: boolean;
}

const TRANSIENT_EVENT_TYPES = new Set([
  "core.message.delta",
  "core.reasoning.summary",
  "core.tool.started",
  "core.tool.progress",
  "core.tool.completed",
  "core.usage.updated",
]);

const LIVE_TEXT_CLEAR_TYPES = new Set([
  "core.turn.started",
  "core.message.completed",
  "core.turn.retried",
  "core.turn.edit_resent",
  "core.turn.rewound",
]);

/** Retry / edit-resend / native rewind all publish superseded_turn_ids. */
const TURN_SUPERSESSION_EVENT_TYPES = new Set([
  "core.turn.retried",
  "core.turn.edit_resent",
  "core.turn.rewound",
]);

export function emptyConversationStreamState(): ConversationStreamState {
  return { events: [], liveText: "", appliedSeq: 0 };
}

function isDuplicate(
  previous: ConversationStreamEvent[],
  event: ConversationStreamEvent,
): boolean {
  const incomingId = String(event.event_id || "");
  if (incomingId) {
    return previous.some((row) => row.event_id === incomingId);
  }
  return previous.some((row) => row.seq === event.seq);
}

function appendLiveDelta(
  liveText: string,
  event: ConversationStreamEvent,
): string {
  if (event.event_type !== "core.message.delta") return liveText;
  if (event.payload.thinking === true) return liveText;
  const role = String(event.payload.role ?? "").toLowerCase();
  if (!["assistant", "agent"].includes(role)) return liveText;
  return `${liveText}${String(event.payload.text ?? "")}`;
}

function nextEvents(
  previous: ConversationStreamEvent[],
  event: ConversationStreamEvent,
): ConversationStreamEvent[] {
  if (TURN_SUPERSESSION_EVENT_TYPES.has(event.event_type)) {
    const superseded = new Set(
      (Array.isArray(event.payload.superseded_turn_ids)
        ? event.payload.superseded_turn_ids
        : [])
        .map(String)
        .filter(Boolean),
    );
    return [
      ...previous.filter((row) => {
        const rowTurnId = String(row.payload.turn_id || "");
        return !rowTurnId || !superseded.has(rowTurnId);
      }),
      event,
    ].slice(-4000);
  }
  return [...previous, event].slice(-4000);
}

/**
 * Admit one Conversation SSE event into stream state.
 * Rejects duplicates and already-applied seq values (reconnect replay).
 *
 * The 4000-event ring buffer is a *live* admission window only (C05/C06).
 * Full conversation history comes from server-paged messages; do not treat
 * this slice as proof that older history was deleted.
 */
export function acceptConversationStreamEvent(
  state: ConversationStreamState,
  event: ConversationStreamEvent,
): ConversationStreamResult {
  const seq = Number(event.seq) || 0;
  if (seq > 0 && seq <= state.appliedSeq) {
    return { state, accepted: false, scheduleRefresh: false };
  }
  if (isDuplicate(state.events, event)) {
    return { state, accepted: false, scheduleRefresh: false };
  }

  let liveText = appendLiveDelta(state.liveText, event);
  if (LIVE_TEXT_CLEAR_TYPES.has(event.event_type)) {
    liveText = "";
  }

  const next: ConversationStreamState = {
    events: nextEvents(state.events, event),
    liveText,
    appliedSeq: Math.max(state.appliedSeq, seq),
  };

  return {
    state: next,
    accepted: true,
    scheduleRefresh: !TRANSIENT_EVENT_TYPES.has(event.event_type),
  };
}
