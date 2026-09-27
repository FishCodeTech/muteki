"use client";

import { useSyncExternalStore } from "react";

// Missing preference means solve-only. Keep the existing key so saved choices survive upgrades.
const KEY = "muteki.workspace.solveOnly";
const CHANGE_EVENT = "muteki:workspace-mode-change";
let memoryEnabled = true;

function snapshot(): boolean {
  try { return window.localStorage.getItem(KEY) !== "0"; } catch { return memoryEnabled; }
}

function subscribe(callback: () => void): () => void {
  const onStorage = (event: StorageEvent) => {
    if (event.key === KEY || event.key === null) callback();
  };
  window.addEventListener("storage", onStorage);
  window.addEventListener(CHANGE_EVENT, callback);
  return () => {
    window.removeEventListener("storage", onStorage);
    window.removeEventListener(CHANGE_EVENT, callback);
  };
}

export function useSolveOnlyMode(): boolean {
  return useSyncExternalStore(subscribe, snapshot, () => true);
}

export function setSolveOnlyMode(enabled: boolean): void {
  memoryEnabled = enabled;
  try { window.localStorage.setItem(KEY, enabled ? "1" : "0"); } catch { /* available for this page only */ }
  window.dispatchEvent(new Event(CHANGE_EVENT));
}
