"use client";

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";
import { queueSideChatQuote, requestSideChatFocus } from "@/components/conversation/ConversationSideChat";
import { apiFetch } from "@/lib/serviceAuth";
import { randomUuid } from "@/lib/browserCrypto";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { sendConversationCommand, type ConversationThread } from "@/lib/useConversation";

export const RUN_SIDE_WIDTH_DEFAULT = 480;
export const RUN_SIDE_WIDTH_MIN = 360;

export type RunSideChatLayout = {
  open: boolean;
  tabs: string[];
  active: string;
  width: number;
  side: "left" | "right";
  maximized: boolean;
};

const DEFAULT_LAYOUT: RunSideChatLayout = { open: false, tabs: [], active: "", width: RUN_SIDE_WIDTH_DEFAULT, side: "right", maximized: false };
const PREFIX = "muteki.runSideChat.v1:";
const layouts = new Map<string, RunSideChatLayout>();
const listeners = new Set<() => void>();

function readLayout(runId: string): RunSideChatLayout {
  const cached = layouts.get(runId);
  if (cached) return cached;
  let value = DEFAULT_LAYOUT;
  try {
    const raw = JSON.parse(window.localStorage.getItem(PREFIX + runId) || "null");
    if (raw && typeof raw === "object") {
      const tabs = Array.isArray(raw.tabs) ? raw.tabs.filter((id: unknown): id is string => typeof id === "string" && Boolean(id)) : [];
      value = {
        open: raw.open === true,
        tabs,
        active: typeof raw.active === "string" && tabs.includes(raw.active) ? raw.active : tabs[0] || "",
        width: Number.isFinite(raw.width) ? Math.max(RUN_SIDE_WIDTH_MIN, Math.round(raw.width)) : RUN_SIDE_WIDTH_DEFAULT,
        side: raw.side === "left" ? "left" : "right",
        maximized: raw.maximized === true,
      };
    }
  } catch {
  }
  layouts.set(runId, value);
  return value;
}

export function updateRunSideChatLayout(runId: string, patch: Partial<RunSideChatLayout> | ((layout: RunSideChatLayout) => Partial<RunSideChatLayout>)) {
  if (!runId) return;
  const current = readLayout(runId);
  const next = { ...current, ...(typeof patch === "function" ? patch(current) : patch) };
  if (!next.tabs.includes(next.active)) next.active = next.tabs[0] || "";
  layouts.set(runId, next);
  try { window.localStorage.setItem(PREFIX + runId, JSON.stringify(next)); } catch {  }
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  const onStorage = (event: StorageEvent) => {
    if (!event.key?.startsWith(PREFIX)) return;
    layouts.delete(event.key.slice(PREFIX.length));
    listener();
  };
  window.addEventListener("storage", onStorage);
  return () => { listeners.delete(listener); window.removeEventListener("storage", onStorage); };
}

export function useRunSideChatLayout(runId: string): RunSideChatLayout {
  return useSyncExternalStore(subscribe, () => (runId ? readLayout(runId) : DEFAULT_LAYOUT), () => DEFAULT_LAYOUT);
}

export function showRunSideChat(runId: string, threadId: string) {
  updateRunSideChatLayout(runId, (layout) => ({
    open: true,
    tabs: layout.tabs.includes(threadId) ? layout.tabs : [...layout.tabs, threadId],
    active: threadId,
  }));
}

export function closeRunSideChatTab(runId: string, threadId: string) {
  updateRunSideChatLayout(runId, (layout) => {
    const index = layout.tabs.indexOf(threadId);
    const tabs = layout.tabs.filter((id) => id !== threadId);
    const active = layout.active === threadId ? tabs[Math.min(index, tabs.length - 1)] || "" : layout.active;
    return { tabs, active, open: tabs.length ? layout.open : false, maximized: tabs.length ? layout.maximized : false };
  });
}

async function failure(response: Response, fallback: string): Promise<Error> {
  const body = await response.json().catch(() => ({})) as { error?: { message?: string } };
  return new Error(body.error?.message || `${fallback}（HTTP ${response.status}）`);
}

export type RunSideChatTarget = {
  target_id: string;
  kind: "worker" | "agent";
  worker_id?: string | null;
  seat_id?: string | null;
  label: string;
  engine: string;
  adapter_id: string;
  instance_id: string;
  credential_id: string;
  model: string;
  effort?: string | null;
  status: string;
  online: boolean;
  solved: boolean;
  generation: number;
  current_generation: number;
  intent_id?: string | null;
  intent_goal?: string | null;
  tool_calls: number;
  facts_count: number;
  native_fork_available: boolean;
  backend?: string;
};

export type RunSideChatsPayload = {
  threads: ConversationThread[];
  targets: RunSideChatTarget[];
  defaultTargetId: string;
};

export async function createRunSideChat(runId: string, targetId?: string): Promise<string> {
  const id = randomUuid();
  const response = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/side-chats`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      command_id: id,
      idempotency_key: id,
      ...(targetId ? { target_id: targetId } : {}),
    }),
  });
  if (!response.ok) throw await failure(response, "侧边聊天创建失败");
  const body = await response.json() as { receipt?: { aggregate?: { id?: string } } };
  const threadId = body.receipt?.aggregate?.id;
  if (!threadId) throw new Error("侧边聊天回执缺少会话编号");
  return threadId;
}

export async function fetchRunSideChats(runId: string): Promise<RunSideChatsPayload> {
  const response = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/side-chats`);
  if (!response.ok) throw await failure(response, "侧边聊天列表读取失败");
  const body = await response.json() as {
    threads?: ConversationThread[];
    targets?: RunSideChatTarget[];
    default_target_id?: string | null;
  };
  return {
    threads: body.threads || [],
    targets: body.targets || [],
    defaultTargetId: body.default_target_id || "",
  };
}

export async function listRunSideChats(runId: string): Promise<ConversationThread[]> {
  return (await fetchRunSideChats(runId)).threads;
}

export async function selectRunSideChatTarget(
  runId: string,
  threadId: string,
  targetId: string,
): Promise<{ thread?: ConversationThread; runtime?: Record<string, unknown> }> {
  const response = await apiFetch(
    `/api/runs/${encodeURIComponent(runId)}/side-chats/${encodeURIComponent(threadId)}/target`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ target_id: targetId }),
    },
  );
  if (!response.ok) throw await failure(response, "切换出战目标失败");
  return await response.json() as { thread?: ConversationThread; runtime?: Record<string, unknown> };
}

export async function deleteRunSideChat(runId: string, threadId: string): Promise<void> {
  await sendConversationCommand(threadId, "conversation.thread.delete", {});
  closeRunSideChatTab(runId, threadId);
}

export function runSideChatRoute(runId: string, threadId: string) {
  return `/run/${encodeURIComponent(runId)}/side/${encodeURIComponent(threadId)}?popout=1`;
}

type PopoutMessage = { type: "open" | "closed" | "ping" | "reclaim" | "quote"; runId: string; threadId?: string; text?: string; send?: boolean };
const CHANNEL = "muteki.run-side-chat";

function channel(): BroadcastChannel | null {
  return typeof BroadcastChannel === "undefined" ? null : new BroadcastChannel(CHANNEL);
}

export function useRunSideChatPopout(runId: string) {
  const [popped, setPopped] = useState(false);
  useEffect(() => {
    setPopped(false);
    const port = channel();
    if (!port || !runId) return;
    port.onmessage = (event: MessageEvent<PopoutMessage>) => {
      const message = event.data;
      if (message?.runId !== runId) return;
      if (message.type === "open") setPopped(true);
      else if (message.type === "closed" || message.type === "reclaim") setPopped(false);
    };
    port.postMessage({ type: "ping", runId } satisfies PopoutMessage);
    return () => port.close();
  }, [runId]);

  const popOut = useCallback(async (threadId: string) => {
    const route = runSideChatRoute(runId, threadId);
    const bridge = desktopChatBridge();
    if (bridge?.openWindow) {
      await bridge.openWindow({ route });
    } else {
      const opened = window.open(route, `muteki-run-side-${runId}`, "popup=yes,width=620,height=880");
      if (!opened) throw new Error("浏览器拦截了弹出窗口，请允许本站弹窗后重试");
    }
    updateRunSideChatLayout(runId, { open: true });
    setPopped(true);
  }, [runId]);

  const reclaim = useCallback(() => {
    const port = channel();
    port?.postMessage({ type: "reclaim", runId } satisfies PopoutMessage);
    port?.close();
    updateRunSideChatLayout(runId, { open: true });
    setPopped(false);
  }, [runId]);

  const sendQuote = useCallback((threadId: string, text: string, send = false) => {
    if (!popped) return;
    const port = channel();
    port?.postMessage({ type: "quote", runId, threadId, text, send } satisfies PopoutMessage);
    port?.close();
  }, [runId, popped]);

  return { popped, popOut, reclaim, sendQuote };
}

export function useRunSideChatWindow(runId: string, enabled: boolean, onReclaimed: () => void) {
  const portRef = useRef<BroadcastChannel | null>(null);
  useEffect(() => {
    const port = channel();
    if (!port || !runId || !enabled) return;
    portRef.current = port;
    const announce = () => port.postMessage({ type: "open", runId } satisfies PopoutMessage);
    port.onmessage = (event: MessageEvent<PopoutMessage>) => {
      const message = event.data;
      if (message?.runId !== runId) return;
      if (message.type === "ping") announce();
      else if (message.type === "reclaim") onReclaimed();
      else if (message.type === "quote" && message.threadId && message.text) {
        queueSideChatQuote(message.threadId, { plainText: message.text, key: randomUuid(), send: message.send });
        requestSideChatFocus(message.threadId);
      }
    };
    const visible = () => { if (document.visibilityState === "visible") announce(); };
    const leave = () => port.postMessage({ type: "closed", runId } satisfies PopoutMessage);
    announce();
    document.addEventListener("visibilitychange", visible);
    window.addEventListener("pagehide", leave);
    return () => {
      document.removeEventListener("visibilitychange", visible);
      window.removeEventListener("pagehide", leave);
      leave();
      portRef.current = null;
      port.close();
    };
  }, [runId, enabled, onReclaimed]);

  return useCallback(() => {
    portRef.current?.postMessage({ type: "reclaim", runId } satisfies PopoutMessage);
    onReclaimed();
  }, [runId, onReclaimed]);
}

export async function closeRunSideChatWindow(): Promise<boolean> {
  const bridge = desktopChatBridge();
  if (bridge?.windowAction) {
    await bridge.windowAction("close");
    return true;
  }
  window.close();
  return window.closed;
}
