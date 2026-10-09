import type { ConversationInboxStreamState } from './conversationInbox';
import type { ConversationThread } from './useConversation';
import type { ConversationInboxEvent, ThreadAttentionSummary } from './threadNotifications';

export type NotificationInboxCursor = { seq: number; epoch: string; summaries: Record<string, ThreadAttentionSummary> };
export const emptyNotificationInboxCursor = (): NotificationInboxCursor => ({ seq: 0, epoch: '', summaries: {} });
export function readNotificationInboxCursor(key: string): NotificationInboxCursor {
  const raw = window.localStorage.getItem(key);
  if (!raw) return emptyNotificationInboxCursor();
  const value = JSON.parse(raw) as NotificationInboxCursor;
  if (!Number.isSafeInteger(value.seq) || value.seq < 0 || typeof value.epoch !== 'string' || !value.summaries || typeof value.summaries !== 'object') throw new Error('notification.cursor_invalid: Saved inbox cursor is invalid.');
  return value;
}
export function writeNotificationInboxCursor(key: string, state: ConversationInboxStreamState, summaries: Record<string, ThreadAttentionSummary>): void {
  window.localStorage.setItem(key, JSON.stringify({ seq: state.appliedSeq, epoch: state.brokerEpoch, summaries }));
}
/** A reset snapshot recovers changed attention only; the initial baseline never blasts history. */
export function recoverNotificationInboxSnapshot(previous: NotificationInboxCursor, epoch: string, seq: number, summaries: ThreadAttentionSummary[], threads: ConversationThread[] = []): ConversationInboxEvent[] {
  if (!previous.epoch) return [];
  return summaries.filter(summary => {
    const old = previous.summaries[summary.thread_id];
    const interesting = summary.pending_kind === 'approval' || summary.pending_kind === 'user_input' || summary.pending_kind === 'failed' || (summary.unread && !summary.running);
    return interesting && (!old || old.revision !== summary.revision || old.pending_kind !== summary.pending_kind || old.pending_id !== summary.pending_id || old.unread !== summary.unread || old.running !== summary.running);
  }).map(summary => ({ event_id: `recovery:${summary.thread_id}:${summary.revision}:${summary.pending_kind}:${summary.pending_id || ''}`, broker_epoch: epoch, seq, kind: 'attention.updated', summary, thread: threads.find(thread => thread.thread_id === summary.thread_id) }));
}
