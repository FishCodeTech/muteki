"use client";

import { useEffect, useRef } from "react";
import { extensionSurface, type ExtensionSurface } from "@/components/chat/panel/extensions";
import { chatPanel } from "@/lib/chatPanelStore";
import { randomUuid } from "@/lib/browserCrypto";
import { sendConversationCommand, type ConversationView } from "@/lib/useConversation";
import type { ConversationMessageProps } from "./ConversationMessage";

export type SideChatQuote = (Parameters<NonNullable<ConversationMessageProps["onCiteSelection"]>>[0] & { sourceThreadId: string; key: string }) | { plainText: string; key: string; send?: boolean };

export const SIDE_CHAT_KEY = "side-chat";
const TAB_PREFIX = `${SIDE_CHAT_KEY}:`;

export function sideChatSurface(sideId: string): ExtensionSurface {
  return extensionSurface(`${TAB_PREFIX}${sideId}`, "侧边聊天");
}

export function sideChatIdOf(key: string): string {
  return key.startsWith(TAB_PREFIX) ? key.slice(TAB_PREFIX.length) : "";
}

export function openSideChatIds(parentId: string): string[] {
  return chatPanel.thread(parentId).surfaces
    .map((surface) => surface.kind === "extension" ? sideChatIdOf(surface.key) : "")
    .filter(Boolean);
}

export function visibleSideChatId(parentId: string): string {
  const panel = chatPanel.thread(parentId);
  const active = panel.isOpen ? panel.surfaces.find((surface) => surface.id === panel.activeId) : undefined;
  return active?.kind === "extension" ? sideChatIdOf(active.key) : "";
}

export async function createSideChat(parentId: string): Promise<string> {
  const id = randomUuid();
  const receipt = await sendConversationCommand(parentId, "conversation.thread.fork", { from_turn_id: "", side_chat: true }, { commandId: id, idempotencyKey: id });
  const sideId = receipt.aggregate?.id;
  if (!sideId) throw new Error("侧边聊天回执缺少会话编号");
  return sideId;
}

export async function sendSideChatQuestion(sideId: string, text: string): Promise<void> {
  const id = randomUuid();
  await sendConversationCommand(sideId, "conversation.turn.send", { text, client_message_id: id, capability_refs: [] }, { commandId: id, idempotencyKey: id });
}

export async function discardSideChat(sideId: string): Promise<void> {
  await sendConversationCommand(sideId, "conversation.thread.delete", {});
}

const pendingFocus = new Set<string>();

export function requestSideChatFocus(sideId: string) {
  pendingFocus.add(sideId);
}

export function takeSideChatFocus(sideId: string): boolean {
  return pendingFocus.delete(sideId);
}

const pendingQuotes = new Map<string, SideChatQuote[]>();
const quoteListeners = new Set<() => void>();

export function queueSideChatQuote(sideId: string, quote: SideChatQuote) {
  pendingQuotes.set(sideId, [...(pendingQuotes.get(sideId) || []), quote]);
  quoteListeners.forEach((listener) => listener());
}

export function useSideChatQuotes(sideId: string, ready: boolean, consume: (quote: SideChatQuote) => void) {
  const consumeRef = useRef(consume);
  consumeRef.current = consume;
  useEffect(() => {
    if (!sideId || !ready) return;
    const drain = () => {
      const rows = pendingQuotes.get(sideId);
      if (!rows?.length) return;
      pendingQuotes.delete(sideId);
      rows.forEach((quote) => consumeRef.current(quote));
    };
    drain();
    quoteListeners.add(drain);
    return () => { quoteListeners.delete(drain); };
  }, [sideId, ready]);
}

export type SideChatParentStatus = { label: string; tone: "running" | "attention" | "failed" | "idle" };

export function sideChatParentStatus(view: ConversationView | null | undefined): SideChatParentStatus {
  if (!view) return { label: "主对话", tone: "idle" };
  const state = view.state;
  const approvals = Object.values(state.pending_approvals || {}).filter((row) => !["resolved", "expired"].includes(String((row as { status?: unknown })?.status)));
  if (approvals.length || (state.pending_approval && !["resolved", "expired"].includes(String(state.pending_approval.status)))) return { label: "主对话需要审批", tone: "attention" };
  if (state.pending_user_input && !["resolved", "expired"].includes(String(state.pending_user_input.status))) return { label: "主对话等待回复", tone: "attention" };
  if (state.running_turn_id) return { label: "主对话进行中", tone: "running" };
  const last = view.turns.filter((turn) => turn.status !== "superseded").at(-1);
  if (!last) return { label: "主对话", tone: "idle" };
  if (last.status === "failed") return { label: "主对话失败", tone: "failed" };
  if (["interrupted", "aborted", "cancelled", "canceled"].includes(last.status)) return { label: "主对话已中断", tone: "idle" };
  return { label: "主对话已完成", tone: "idle" };
}
