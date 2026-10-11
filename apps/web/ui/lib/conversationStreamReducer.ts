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
  liveTextRuns: ConversationLiveTextRun[];
  appliedSeq: number;
  /**
   * Membership index for `events`. It is shared by successive states of one
   * stream, so only the newest state of that stream may be passed back in.
   */
  index?: ConversationStreamIndex;
}

export interface ConversationStreamIndex {
  ids: Set<string>;
  seqs: Set<number>;
}

export interface ConversationLiveTextRun {
  start: number;
  end: number;
  phase: "commentary" | "final_answer" | "unknown";
}

export interface ConversationStreamResult {
  state: ConversationStreamState;
  accepted: boolean;
  scheduleRefresh: boolean;
}

export interface ConversationStreamBatchResult {
  state: ConversationStreamState;
  acceptedCount: number;
  /** Accepted, non-transient events in arrival order. */
  refreshEvents: ConversationStreamEvent[];
}

/**
 * The *live* admission window (C05/C06). Full conversation history comes from
 * server-paged messages; do not treat this slice as proof that older history
 * was deleted.
 */
export const CONVERSATION_STREAM_WINDOW = 4000;
const CONVERSATION_STREAM_TRIM_SLACK = 500;

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
  return {
    events: [],
    liveText: "",
    liveTextRuns: [],
    appliedSeq: 0,
    index: { ids: new Set(), seqs: new Set() },
  };
}

function buildIndex(events: ConversationStreamEvent[]): ConversationStreamIndex {
  const index: ConversationStreamIndex = { ids: new Set(), seqs: new Set() };
  for (const row of events) indexAdd(index, row);
  return index;
}

function indexAdd(index: ConversationStreamIndex, row: ConversationStreamEvent) {
  const id = String(row.event_id || "");
  if (id) index.ids.add(id);
  index.seqs.add(row.seq);
}

function indexDelete(index: ConversationStreamIndex, row: ConversationStreamEvent) {
  const id = String(row.event_id || "");
  if (id) index.ids.delete(id);
  index.seqs.delete(row.seq);
}

function isDuplicate(index: ConversationStreamIndex, event: ConversationStreamEvent): boolean {
  const incomingId = String(event.event_id || "");
  return incomingId ? index.ids.has(incomingId) : index.seqs.has(event.seq);
}

function liveDelta(event: ConversationStreamEvent): string {
  if (event.event_type !== "core.message.delta") return "";
  if (event.payload.thinking === true) return "";
  const role = String(event.payload.role ?? "").toLowerCase();
  if (!["assistant", "agent"].includes(role)) return "";
  return String(event.payload.text ?? "");
}

function liveDeltaPhase(event: ConversationStreamEvent): ConversationLiveTextRun["phase"] {
  const rawPhase = String(event.payload.phase || event.payload.message_phase || event.payload.channel || "").toLowerCase();
  return rawPhase === "commentary" ? "commentary"
    : rawPhase === "final_answer" || rawPhase === "final" ? "final_answer" : "unknown";
}

function supersededTurnIds(event: ConversationStreamEvent): Set<string> {
  return new Set(
    (Array.isArray(event.payload.superseded_turn_ids)
      ? event.payload.superseded_turn_ids
      : [])
      .map(String)
      .filter(Boolean),
  );
}

/**
 * Admit a batch of Conversation SSE events in arrival order.
 * Rejects duplicates and already-applied seq values (reconnect replay).
 *
 * Admission is O(1) per event and the window is copied at most once per
 * batch, so a long replay costs time linear in the replayed events.
 */
export function acceptConversationStreamEvents(
  state: ConversationStreamState,
  incoming: readonly ConversationStreamEvent[],
): ConversationStreamBatchResult {
  const index = state.index ?? buildIndex(state.events);
  let events: ConversationStreamEvent[] | null = null;
  let liveText = state.liveText;
  let liveTextRuns: ConversationLiveTextRun[] | null = null;
  let appliedSeq = state.appliedSeq;
  let acceptedCount = 0;
  const refreshEvents: ConversationStreamEvent[] = [];

  for (const event of incoming) {
    const seq = Number(event.seq) || 0;
    if (seq > 0 && seq <= appliedSeq) continue;
    if (isDuplicate(index, event)) continue;
    if (!events) events = state.events.slice();
    if (!liveTextRuns) liveTextRuns = (state.liveTextRuns || []).slice();

    const delta = liveDelta(event);
    if (delta) {
      const start = liveText.length;
      liveText += delta;
      const phase = liveDeltaPhase(event);
      const last = liveTextRuns.at(-1);
      if (last?.phase === phase && last.end === start) {
        liveTextRuns[liveTextRuns.length - 1] = { ...last, end: liveText.length };
      } else {
        liveTextRuns.push({ start, end: liveText.length, phase });
      }
    }
    if (LIVE_TEXT_CLEAR_TYPES.has(event.event_type)) {
      liveText = "";
      liveTextRuns = [];
    }

    if (TURN_SUPERSESSION_EVENT_TYPES.has(event.event_type)) {
      const superseded = supersededTurnIds(event);
      if (superseded.size) {
        events = events.filter((row) => {
          const rowTurnId = String(row.payload.turn_id || "");
          const keep = !rowTurnId || !superseded.has(rowTurnId);
          if (!keep) indexDelete(index, row);
          return keep;
        });
      }
    }
    events.push(event);
    indexAdd(index, event);
    appliedSeq = Math.max(appliedSeq, seq);
    acceptedCount += 1;
    if (!TRANSIENT_EVENT_TYPES.has(event.event_type)) refreshEvents.push(event);
  }

  if (!events) return { state, acceptedCount: 0, refreshEvents };
  // Trim in chunks: while the window only grows, consumers can extend their
  // folds incrementally instead of refolding after every dropped event.
  const overflow = events.length > CONVERSATION_STREAM_WINDOW + CONVERSATION_STREAM_TRIM_SLACK
    ? events.length - CONVERSATION_STREAM_WINDOW
    : 0;
  if (overflow > 0) {
    for (let i = 0; i < overflow; i += 1) indexDelete(index, events[i]);
    events = events.slice(overflow);
  }
  return {
    state: {
      events,
      liveText,
      liveTextRuns: liveTextRuns ?? state.liveTextRuns,
      appliedSeq,
      index,
    },
    acceptedCount,
    refreshEvents,
  };
}

/** Admit one Conversation SSE event; see acceptConversationStreamEvents. */
export function acceptConversationStreamEvent(
  state: ConversationStreamState,
  event: ConversationStreamEvent,
): ConversationStreamResult {
  const result = acceptConversationStreamEvents(state, [event]);
  return {
    state: result.state,
    accepted: result.acceptedCount > 0,
    scheduleRefresh: result.refreshEvents.length > 0,
  };
}
