"use client";

import { useSyncExternalStore } from "react";

import type { RunWorkspaceMode } from "./runWorkspaceRoutes";

/** Collab (and other /run/… shells) publish deck.mode so WorkspaceNav can highlight the right workspace. */
type ActiveRunWorkspace = { runId: string; mode: RunWorkspaceMode };

let current: ActiveRunWorkspace | null = null;
const listeners = new Set<() => void>();

function emit(): void {
  listeners.forEach((listener) => listener());
}

export function publishActiveRunWorkspace(runId: string, mode: RunWorkspaceMode): void {
  if (current?.runId === runId && current.mode === mode) return;
  current = { runId, mode };
  emit();
}

export function clearActiveRunWorkspace(runId?: string): void {
  if (!current) return;
  if (runId && current.runId !== runId) return;
  current = null;
  emit();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useActiveRunWorkspace(): ActiveRunWorkspace | null {
  return useSyncExternalStore(subscribe, () => current, () => null);
}
