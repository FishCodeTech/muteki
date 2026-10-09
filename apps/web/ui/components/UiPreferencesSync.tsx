"use client";
import { useEffect, useSyncExternalStore } from "react";
import { startUiPreferencesSync, subscribeUiPreferences, uiPreferenceSnapshot, serverUiPreferenceSnapshot, refreshUiPreferences } from "@/lib/uiPreferences";

export function UiPreferencesSync() {
  const state = useSyncExternalStore(subscribeUiPreferences, uiPreferenceSnapshot, serverUiPreferenceSnapshot);
  useEffect(startUiPreferencesSync, []);
  const message = state.error || state.cacheError;
  if (!message) return null;
  return <div role="alert" data-testid="ui-preferences-error" className="fixed bottom-3 left-1/2 z-[100] max-w-[min(90vw,42rem)] -translate-x-1/2 rounded-xl border border-cx-border bg-cx-elevated p-3 text-xs text-cx-fg shadow-lg">
    <p>共享偏好尚未同步 / Shared preferences are not synchronized</p><pre className="mt-1 whitespace-pre-wrap break-all">{message}</pre>
    <button type="button" className="mt-2 underline" onClick={() => void refreshUiPreferences()}>重试 / Retry</button>
  </div>;
}
