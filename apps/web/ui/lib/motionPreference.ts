"use client";

import { useSyncExternalStore } from "react";

export type MotionPreference = "system" | "reduce";
const STORAGE_KEY = "muteki.motion";
const CHANGE_EVENT = "muteki:motion-change";
let sessionPreference: MotionPreference | undefined;

export function readMotionPreference(): MotionPreference {
  if (sessionPreference) return sessionPreference;
  try {
    return window.localStorage.getItem(STORAGE_KEY) === "reduce" ? "reduce" : "system";
  } catch {
    return "system";
  }
}

export function setMotionPreference(preference: MotionPreference) {
  sessionPreference = preference;
  try { window.localStorage.setItem(STORAGE_KEY, preference); } catch { /* Session preference still works. */ }
  document.documentElement.dataset.motion = preference;
  window.dispatchEvent(new Event(CHANGE_EVENT));
}

export function subscribeMotionPreference(onChange: () => void) {
  const onStorage = (event: StorageEvent) => {
    if (event.key !== STORAGE_KEY && event.key !== null) return;
    sessionPreference = undefined;
    document.documentElement.dataset.motion = readMotionPreference();
    onChange();
  };
  window.addEventListener(CHANGE_EVENT, onChange);
  window.addEventListener("storage", onStorage);
  return () => {
    window.removeEventListener(CHANGE_EVENT, onChange);
    window.removeEventListener("storage", onStorage);
  };
}

export function useMotionPreference() {
  return useSyncExternalStore(subscribeMotionPreference, readMotionPreference, () => "system" as const);
}
