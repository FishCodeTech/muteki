"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
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

export function useThreadScopedNotice(
  scopeKey: string,
): [string, (value: string, kind?: ThreadNoticeKind) => void, ThreadNoticeControls] {
  const [store, setStore] = useState<Record<string, { text: string; kind: ThreadNoticeKind }>>({});
  const currentNotice = store[scopeKey];
  const value = currentNotice?.text ?? "";
  const scopeRef = useRef(scopeKey);
  scopeRef.current = scopeKey;
  const timerScopeRef = useRef(scopeKey);
  const controllerRef = useRef<ReturnType<typeof createNoticeAutoDismissController> | null>(null);
  if (controllerRef.current == null) {
    controllerRef.current = createNoticeAutoDismissController(() => {
      const key = timerScopeRef.current;
      setStore((prev) => {
        const current = prev[key];
        if (!current || !shouldScheduleNoticeAutoClear(current.text, current.kind)) return prev;
        const next = { ...prev };
        delete next[key];
        return next;
      });
    });
  }
  const setValue = useCallback((text: string, kind: ThreadNoticeKind = "sticky") => {
    setStore((prev) => {
      if (!text) {
        if (!(scopeKey in prev)) return prev;
        const next = { ...prev };
        delete next[scopeKey];
        return next;
      }
      if (prev[scopeKey]?.text === text && prev[scopeKey]?.kind === kind) return prev;
      return { ...prev, [scopeKey]: { text, kind } };
    });
    if (scopeRef.current === scopeKey) {
      timerScopeRef.current = scopeKey;
      controllerRef.current?.onNoticeChange(text, kind);
    }
  }, [scopeKey]);

  useEffect(() => {
    timerScopeRef.current = scopeKey;
    controllerRef.current?.onNoticeChange(currentNotice?.text ?? "", currentNotice?.kind ?? "sticky");
    // setValue manages visible updates; navigation starts a fresh visible window.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scopeKey]);
  useEffect(() => () => { controllerRef.current?.dispose(); }, []);
  const pauseAutoDismiss = useCallback(() => { controllerRef.current?.pause(); }, []);
  const resumeAutoDismiss = useCallback(() => { controllerRef.current?.resume(); }, []);
  return [value, setValue, { kind: currentNotice?.kind ?? "sticky", pauseAutoDismiss, resumeAutoDismiss }];
}
