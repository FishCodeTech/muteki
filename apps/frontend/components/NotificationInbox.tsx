"use client";
import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { currentAuthGeneration } from "@/lib/serviceAuth";
import { desktopChatBridge, type DesktopNativeState } from '@/lib/desktopChatBridge';
import { conversationStorageScope, subscribeConversationStorageScope } from '@/lib/conversationStorageScope';
import { collectFinishedUnreadThreads } from '@/lib/dockBadge';
import { subscribeSidebarThreadStates } from '@/lib/sidebarThreadBridge';
import { dispatchThreadNotification, type ConversationInboxEvent } from '@/lib/threadNotifications';
import { useConversationThreads } from '@/lib/useConversation';
import { usePathname, useRouter } from 'next/navigation';
import { isLocalPreviewPath } from '@/lib/conversationPreviewMode';
import { isThreadVisibleInShell } from '@/lib/desktopShell';

export function NotificationInbox() {
  const router = useRouter();
  const routerRef = useRef(router); routerRef.current = router;
  const bridge = desktopChatBridge();
  const scope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => '');
  const [stateError, setStateError] = useState('');
  const [state, setState] = useState<DesktopNativeState>({});
  const stateRef = useRef(state); stateRef.current = state;
  const [inboxRevision, setInboxRevision] = useState(0);
  const lastDockPayloadRef = useRef('');
  useEffect(() => {
    if (!bridge) return;
    let live = true, updates = 0;
    const off = bridge.onState?.(next => { updates++; if (live) { stateRef.current = next; setState(next); setStateError(''); } });
    if (bridge.getState) void bridge.getState().then(next => { if (live && !updates) { stateRef.current = next; setState(next); setStateError(''); } })
      .catch(error => { if (live) setStateError(error instanceof Error ? error.stack || error.message : String(error)); });
    else setStateError('desktop.notifications_unavailable: Native workspace state is unavailable.');
    return () => { live = false; off?.(); };
  }, [bridge]);
  useEffect(() => {
    if (!bridge?.setDockBadge) return;
    const bump = () => setInboxRevision(value => value + 1);
    const offSidebar = subscribeSidebarThreadStates(bump);
    window.addEventListener('storage', bump);
    return () => {
      offSidebar();
      window.removeEventListener('storage', bump);
    };
  }, [bridge]);
  const handleEvent = useCallback(async (event: ConversationInboxEvent) => {
    const targetThreadId = event.summary.pending_thread_id || event.summary.thread_id;
    const route = bridge ? stateRef.current.route || '' : window.location.pathname;
    const routeThreadId = route.startsWith('/chat/') ? decodeURIComponent(route.split('/')[2] || '') : '';
    const activeThreadId = isThreadVisibleInShell(targetThreadId) ? targetThreadId : routeThreadId;
    const visibleElsewhere = bridge && (
      stateRef.current.notificationVisibleThreadIds?.includes(targetThreadId)
      || isThreadVisibleInShell(targetThreadId)
    );
    const delivery = dispatchThreadNotification(event, {
      activeThreadId: visibleElsewhere ? targetThreadId : bridge ? '' : activeThreadId,
      visibilityState: visibleElsewhere ? 'visible' : document.visibilityState,
      onOpenThread: id => {
        const target = `/chat/${encodeURIComponent(id)}`;
        const current = `${window.location.pathname}${window.location.search}${window.location.hash}`;
        if (current !== target) routerRef.current.push(target);
      },
    });
    if (bridge) console.info('muteki.notification.dispatch', JSON.stringify({ threadId: event.summary.thread_id, eventId: event.event_id, activeThreadId, ...delivery }));
    return delivery.admitted ? await delivery.admitted : true;
  }, [bridge]);
  const identity = bridge ? state.notificationWorkspaceKey || '' : scope;
  const admissionScopeKey = bridge ? JSON.stringify([state.connectionVersion, state.transportOrigin]) : String(currentAuthGeneration());
  const localPreview = isLocalPreviewPath(usePathname());
  const { threads, loading, error } = useConversationThreads({ enabled: !localPreview && Boolean(identity) && (!bridge || state.notificationOwner === true),
    admissionScopeKey,
    canAdmitInboxEvent: (key, generation) => bridge
      ? stateRef.current.notificationOwner === true && key === `muteki.notification-inbox.v1:${stateRef.current.notificationWorkspaceKey}`
        && generation === JSON.stringify([stateRef.current.connectionVersion, stateRef.current.transportOrigin])
      : key === `muteki.notification-inbox.v1:${conversationStorageScope()}` && generation === String(currentAuthGeneration()),
    cursorStorageKey: identity ? `muteki.notification-inbox.v1:${identity}` : '', onInboxEvent: handleEvent });
  useEffect(() => {
    if (!bridge?.setDockBadge) return;
    if (
      localPreview
      || state.configuring
      || state.notificationOwner !== true
      || !state.serviceId
      || !state.identityId
      || !Number.isSafeInteger(state.connectionVersion)
      || (state.connectionVersion || 0) <= 0
    ) {
      lastDockPayloadRef.current = '';
      return;
    }
    if (loading) return;
    const nowMs = Date.now();
    const { threads: unreadThreads, nextWakeAt } = collectFinishedUnreadThreads(threads, nowMs);
    const payload = {
      connectionVersion: state.connectionVersion as number,
      serviceId: state.serviceId,
      identityId: state.identityId,
      threads: unreadThreads,
    };
    const serialized = JSON.stringify(payload);
    if (serialized !== lastDockPayloadRef.current) {
      lastDockPayloadRef.current = serialized;
      void bridge.setDockBadge(payload).catch(() => {
        if (lastDockPayloadRef.current === serialized) lastDockPayloadRef.current = '';
      });
    }
    if (nextWakeAt === null) return;
    const delayMs = Math.min(Math.max(nextWakeAt - nowMs + 25, 25), 2_147_483_647);
    const timer = window.setTimeout(() => setInboxRevision(value => value + 1), delayMs);
    return () => window.clearTimeout(timer);
  }, [
    bridge,
    localPreview,
    loading,
    threads,
    scope,
    inboxRevision,
    state.configuring,
    state.notificationOwner,
    state.connectionVersion,
    state.serviceId,
    state.identityId,
  ]);
  const notice = stateError || error;
  return notice ? <div role="status" className="px-4 py-2 text-cx-sm text-cx-fg-2">通知订阅：{notice}</div> : null;
}
