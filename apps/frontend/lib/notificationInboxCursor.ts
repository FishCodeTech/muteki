import type { ConversationInboxSnapshot } from './conversationInbox';
import type { ConversationThread } from './useConversation';
import type { ConversationInboxEvent, ThreadAttentionSummary } from './threadNotifications';

export type NotificationInboxCursor = { seq: number; epoch: string; summaries: Record<string, ThreadAttentionSummary> };
export const emptyNotificationInboxCursor = (): NotificationInboxCursor => ({ seq: 0, epoch: '', summaries: {} });
export function readNotificationInboxCursor(key: string): NotificationInboxCursor {
  const raw = window.localStorage.getItem(key);
  if (!raw) return emptyNotificationInboxCursor();
  try {
    const value = JSON.parse(raw) as NotificationInboxCursor;
    if (!Number.isSafeInteger(value.seq) || value.seq < 0 || typeof value.epoch !== 'string' || !value.summaries || typeof value.summaries !== 'object') throw new Error('bad shape');
    return value;
  } catch {
    console.warn('notification.cursor_invalid: discarded corrupt saved cursor; subscribing from current position');
    try { window.localStorage.removeItem(key); } catch {  }
    return emptyNotificationInboxCursor();
  }
}
export function writeNotificationInboxCursor(key: string, cursor: NotificationInboxCursor): void {
  window.localStorage.setItem(key, JSON.stringify(cursor));
}
export function recoverNotificationInboxSnapshot(previous: NotificationInboxCursor, epoch: string, seq: number, summaries: ThreadAttentionSummary[], threads: ConversationThread[] = []): ConversationInboxEvent[] {
  if (!previous.epoch) return [];
  const readSeq = new Map(threads.map(thread => [thread.thread_id, thread.state.read_stream_seq]));
  return summaries.filter(summary => {
    const old = previous.summaries[summary.thread_id];
    const interesting = summary.pending_kind === 'approval' || summary.pending_kind === 'user_input' || summary.pending_kind === 'failed' || (summary.unread && !summary.running);
    return interesting && (!old || old.revision !== summary.revision || old.pending_kind !== summary.pending_kind || old.pending_id !== summary.pending_id || old.unread !== summary.unread || old.running !== summary.running);
  }).map(summary => ({ event_id: `recovery:${summary.thread_id}:${summary.revision}:${summary.pending_kind}:${summary.pending_id || ''}`, broker_epoch: epoch, seq, kind: 'attention.updated', summary, thread: { state: { read_stream_seq: readSeq.get(summary.thread_id) } } }));
}

export function createNotificationInboxDrain(consume: (snapshot: ConversationInboxSnapshot) => Promise<void>) {
  let running = false;
  let pending: ConversationInboxSnapshot | null = null;

  async function drain() {
    running = true;
    try {
      while (pending) {
        const snapshot = pending;
        pending = null;
        await consume(snapshot);
      }
    } finally {
      running = false;
    }
  }

  return {
    offer(snapshot: ConversationInboxSnapshot) {
      pending = snapshot;
      if (!running) void drain();
    },
    clear() { pending = null; },
  };
}
