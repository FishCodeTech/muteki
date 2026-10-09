"use client";
import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { currentAuthGeneration } from "@/lib/serviceAuth";
import { desktopChatBridge, type DesktopNativeState } from '@/lib/desktopChatBridge';
import { conversationStorageScope, subscribeConversationStorageScope } from '@/lib/conversationStorageScope';
import { dispatchThreadNotification, type ConversationInboxEvent } from '@/lib/threadNotifications';
import { useConversationThreads } from '@/lib/useConversation';

/** The authenticated workspace owns notification delivery independently of its current page. */
export function NotificationInbox() {
  const bridge = desktopChatBridge();
  const scope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => '');
  const [stateError, setStateError] = useState('');
  const [state, setState] = useState<DesktopNativeState>({});
  const stateRef = useRef(state); stateRef.current = state;
  useEffect(() => {
    if (!bridge) return;
    let live = true, updates = 0;
    const off = bridge.onState?.(next => { updates++; if (live) { stateRef.current = next; setState(next); setStateError(''); } });
    if (bridge.getState) void bridge.getState().then(next => { if (live && !updates) { stateRef.current = next; setState(next); setStateError(''); } })
      .catch(error => { if (live) setStateError(error instanceof Error ? error.stack || error.message : String(error)); });
    else setStateError('desktop.notifications_unavailable: Native workspace state is unavailable.');
    return () => { live = false; off?.(); };
  }, [bridge]);
  const handleEvent = useCallback(async (event: ConversationInboxEvent) => {
    const route = bridge ? stateRef.current.route || '' : window.location.pathname;
    const activeThreadId = route.startsWith('/chat/') ? decodeURIComponent(route.split('/')[2] || '') : '';
    const visibleElsewhere = bridge && stateRef.current.notificationVisibleThreadIds?.includes(event.summary.pending_thread_id || event.summary.thread_id);
    const delivery = dispatchThreadNotification(event, { activeThreadId: visibleElsewhere ? event.summary.pending_thread_id || event.summary.thread_id : bridge ? '' : activeThreadId, visibilityState: visibleElsewhere ? 'visible' : document.visibilityState, onOpenThread: id => { window.location.assign(`/chat/${encodeURIComponent(id)}`); } });
    if (bridge) console.info('muteki.notification.dispatch', JSON.stringify({ threadId: event.summary.thread_id, eventId: event.event_id, activeThreadId, ...delivery }));
    return delivery.admitted ? await delivery.admitted : true;
  }, [bridge]);
  const identity = bridge ? state.notificationWorkspaceKey || '' : scope;
  const admissionScopeKey = bridge ? JSON.stringify([state.connectionVersion, state.transportOrigin]) : String(currentAuthGeneration());
  const { error } = useConversationThreads({ enabled: Boolean(identity) && (!bridge || state.notificationOwner === true),
    admissionScopeKey,
    canAdmitInboxEvent: (key, generation) => bridge
      ? stateRef.current.notificationOwner === true && key === `muteki.notification-inbox.v1:${stateRef.current.notificationWorkspaceKey}`
        && generation === JSON.stringify([stateRef.current.connectionVersion, stateRef.current.transportOrigin])
      : key === `muteki.notification-inbox.v1:${conversationStorageScope()}` && generation === String(currentAuthGeneration()),
    cursorStorageKey: identity ? `muteki.notification-inbox.v1:${identity}` : '', onInboxEvent: handleEvent });
  const notice = stateError || error;
  return notice ? <div role="status" className="px-4 py-2 text-[13px] text-cx-fg-2">通知订阅：{notice}</div> : null;
}
