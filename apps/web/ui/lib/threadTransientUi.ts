"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  classifyThreadNotice,
  createNoticeAutoDismissController,
  shouldScheduleNoticeAutoClear,
  type ThreadNoticeKind,
} from "./threadTransientNotice";
import {
  readThreadScopedString,
  writeThreadScopedString,
  type ThreadTransientStore,
} from "./threadTransientStore";

export type { ThreadTransientStore };
export { readThreadScopedString, writeThreadScopedString };

export {
  SUCCESS_NOTICE_DISMISS_MS,
  classifyThreadNotice,
  createNoticeAutoDismissController,
  shouldScheduleNoticeAutoClear,
  type ThreadNoticeKind,
  type NoticeAutoDismissTimers,
} from "./threadTransientNotice";

/**
 * Transient banners (notice / command error / capability notice) that must
 * stay with the thread or new-chat draft that produced them. ConversationShell
 * lives in the /chat layout and does not remount on thread navigation.
 */
export function useThreadScopedString(
  scopeKey: string,
): [string, (value: string) => void] {
  const [store, setStore] = useState<ThreadTransientStore>({});
  const value = readThreadScopedString(store, scopeKey);
  const setValue = useCallback((next: string) => {
    setStore((prev) => writeThreadScopedString(prev, scopeKey, next));
  }, [scopeKey]);
  return [value, setValue];
}

export type ThreadErrorState = {
  message: string;
  recoveryAction: "send" | null;
};

const EMPTY_THREAD_ERROR: ThreadErrorState = { message: "", recoveryAction: null };

/** Keep an error and its recovery action together, owned by the initiating scope. */
export function useThreadScopedError(
  scopeKey: string,
): [ThreadErrorState, (message: string, recoveryAction?: ThreadErrorState["recoveryAction"]) => void] {
  const [store, setStore] = useState<Record<string, ThreadErrorState>>({});
  const setError = useCallback((message: string, recoveryAction: ThreadErrorState["recoveryAction"] = null) => {
    setStore((previous) => {
      if (!message) {
        if (!(scopeKey in previous)) return previous;
        const next = { ...previous };
        delete next[scopeKey];
        return next;
      }
      const current = previous[scopeKey];
      if (current?.message === message && current.recoveryAction === recoveryAction) return previous;
      return { ...previous, [scopeKey]: { message, recoveryAction } };
    });
  }, [scopeKey]);
  return [store[scopeKey] ?? EMPTY_THREAD_ERROR, setError];
}

export type ThreadNoticeControls = {
  kind: ThreadNoticeKind;
  pauseAutoDismiss: () => void;
  resumeAutoDismiss: () => void;
};

/**
 * Thread-scoped notice with success auto-dismiss. Progress/sticky stay until
 * overwritten or manually cleared; ownership remains per draftKey/thread.
 */
export function useThreadScopedNotice(
  scopeKey: string,
): [string, (value: string) => void, ThreadNoticeControls] {
  const [store, setStore] = useState<ThreadTransientStore>({});
  const value = readThreadScopedString(store, scopeKey);
  const scopeRef = useRef(scopeKey);
  scopeRef.current = scopeKey;
  const timerScopeRef = useRef(scopeKey);

  const controllerRef = useRef<ReturnType<typeof createNoticeAutoDismissController> | null>(null);
  if (controllerRef.current == null) {
    controllerRef.current = createNoticeAutoDismissController(() => {
      const key = timerScopeRef.current;
      setStore((prev) => {
        const current = readThreadScopedString(prev, key);
        if (!shouldScheduleNoticeAutoClear(current)) return prev;
        return writeThreadScopedString(prev, key, "");
      });
    });
  }

  const setValue = useCallback((next: string) => {
    setStore((prev) => writeThreadScopedString(prev, scopeKey, next));
    // An async completion owned by a hidden thread only updates its stored copy.
    if (scopeRef.current === scopeKey) {
      timerScopeRef.current = scopeKey;
      controllerRef.current?.onNoticeChange(next);
    }
  }, [scopeKey]);

  // Switching threads: cancel prior timer; if the newly visible scope already
  // holds a success notice, restart its short dismiss window.
  useEffect(() => {
    const ctrl = controllerRef.current;
    if (!ctrl) return;
    const current = readThreadScopedString(store, scopeKey);
    timerScopeRef.current = scopeKey;
    ctrl.onNoticeChange(current);
    // Only re-arm on scope change — not on every store write (setValue arms).
    // eslint-disable-next-line react-hooks/exhaustive-deps -- intentional
  }, [scopeKey]);

  useEffect(() => () => {
    controllerRef.current?.dispose();
  }, []);

  const pauseAutoDismiss = useCallback(() => {
    controllerRef.current?.pause();
  }, []);
  const resumeAutoDismiss = useCallback(() => {
    controllerRef.current?.resume();
  }, []);

  const kind = classifyThreadNotice(value);
  return [value, setValue, { kind, pauseAutoDismiss, resumeAutoDismiss }];
}
