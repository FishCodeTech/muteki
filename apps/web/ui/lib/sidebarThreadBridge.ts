/**
 * Lets surfaces outside the sidebar (thread header, details card) read and
 * change a thread's inbox placement. The sidebar owns the preferences and the
 * undo window, so it registers the command handler and publishes snapshots.
 */

"use client";

import { useSyncExternalStore } from "react";
import type { InboxPlacement } from "./sidebarInbox";

export type SidebarThreadCommand =
  | { kind: "pin" | "unpin" | "settle" | "unsettle" | "wake" }
  | { kind: "snooze"; until: number }
  | { kind: "filter-project"; projectId: string | null };

export interface SidebarThreadState {
  pinned: boolean;
  placement: InboxPlacement;
  snoozedUntil?: number;
}

type Handler = (threadId: string, command: SidebarThreadCommand) => void;

let handler: Handler | null = null;
let states: ReadonlyMap<string, SidebarThreadState> = new Map();
const listeners = new Set<() => void>();

export function registerSidebarCommandHandler(next: Handler): () => void {
  handler = next;
  return () => {
    if (handler === next) handler = null;
  };
}

/** False when no sidebar is mounted to carry the command out. */
export function sendSidebarCommand(threadId: string, command: SidebarThreadCommand): boolean {
  if (!handler) return false;
  handler(threadId, command);
  return true;
}

export function publishSidebarThreadStates(next: ReadonlyMap<string, SidebarThreadState>): void {
  states = next;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useSidebarThreadState(threadId: string | undefined): SidebarThreadState | null {
  return useSyncExternalStore(
    subscribe,
    () => (threadId ? states.get(threadId) ?? null : null),
    () => null,
  );
}
