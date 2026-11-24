
"use client";

import { useSyncExternalStore } from "react";
import { conversationStorageScope, subscribeConversationStorageScope } from "./conversationStorageScope";
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

const DEFAULT_SCOPE = "conversation-sidebar";
const handlersByScope = new Map<string, Handler>();
const statesByScope = new Map<string, ReadonlyMap<string, SidebarThreadState>>();
let states: ReadonlyMap<string, SidebarThreadState> = new Map();
let activeStorageScope = "";
const listeners = new Set<() => void>();

function rebuildMergedStates(): void {
  const merged = new Map<string, SidebarThreadState>();
  for (const map of statesByScope.values()) {
    for (const [id, entry] of map) merged.set(id, entry);
  }
  states = merged;
}

function syncStorageScope(): void {
  const nextScope = conversationStorageScope();
  if (nextScope === activeStorageScope) return;
  activeStorageScope = nextScope;
  statesByScope.clear();
  states = new Map();
}

if (typeof window !== "undefined") {
  activeStorageScope = conversationStorageScope();
  subscribeConversationStorageScope(() => {
    const hadStates = states.size > 0;
    syncStorageScope();
    if (hadStates) {
      for (const listener of listeners) listener();
    }
  });
}

export function registerSidebarCommandHandler(next: Handler, scopeId = DEFAULT_SCOPE): () => void {
  handlersByScope.set(scopeId, next);
  return () => {
    if (handlersByScope.get(scopeId) === next) handlersByScope.delete(scopeId);
  };
}

export function sendSidebarCommand(threadId: string, command: SidebarThreadCommand, scopeId?: string): boolean {
  const handler = scopeId
    ? handlersByScope.get(scopeId)
    : handlersByScope.get(DEFAULT_SCOPE) ?? handlersByScope.values().next().value;
  if (!handler) return false;
  handler(threadId, command);
  return true;
}

export function publishSidebarThreadStates(next: ReadonlyMap<string, SidebarThreadState>, scopeId = DEFAULT_SCOPE): void {
  syncStorageScope();
  statesByScope.set(scopeId, next);
  rebuildMergedStates();
  for (const listener of listeners) listener();
}

export function currentSidebarThreadStates(): ReadonlyMap<string, SidebarThreadState> {
  syncStorageScope();
  return states;
}

export function subscribeSidebarThreadStates(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useSidebarThreadState(threadId: string | undefined): SidebarThreadState | null {
  return useSyncExternalStore(
    subscribeSidebarThreadStates,
    () => (threadId ? currentSidebarThreadStates().get(threadId) ?? null : null),
    () => null,
  );
}
