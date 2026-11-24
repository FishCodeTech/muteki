"use client";

import { usePathname, useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { DEFAULT_SETTINGS_PAGE } from "@/components/settings/catalog";
import { desktopChatBridge, type DesktopCommand, type DesktopNavigateEvent } from "@/lib/desktopChatBridge";
import { flushConversationPersistence } from "@/lib/conversationPersistence";
import { refreshUiPreferences } from "@/lib/uiPreferences";

export interface DesktopBeforeCloseDetail { token: string; handled: boolean }

const visibleThreadIds = new Set<string>();

export function reportVisibleThread(threadId: string): () => void {
  const id = String(threadId || "").trim();
  if (!id) return () => {};
  visibleThreadIds.add(id);
  void desktopChatBridge()?.setVisibleThreads?.([...visibleThreadIds]);
  return () => {
    visibleThreadIds.delete(id);
    void desktopChatBridge()?.setVisibleThreads?.([...visibleThreadIds]);
  };
}

export function isThreadVisibleInShell(threadId: string): boolean {
  return visibleThreadIds.has(String(threadId || "").trim());
}

function conversationRoute(pathname: string): boolean {
  return pathname === "/chat" || pathname.startsWith("/chat/") || pathname === "/src" || pathname.startsWith("/src/");
}

function isSafeInternalRoute(route: unknown): route is string {
  return typeof route === "string" && route.startsWith("/") && !route.startsWith("//") && !route.includes("\\");
}

export function useDesktopShell(): number {
  const router = useRouter();
  const pathname = usePathname();
  const [reloadKey, setReloadKey] = useState(0);
  const latest = useRef({ router, pathname });
  latest.current = { router, pathname };
  useEffect(() => {
    const bridge = desktopChatBridge();
    if (!bridge?.onCommand || !bridge.onBeforeClose) return;
    const offCommand = bridge.onCommand(({ name }: { name: DesktopCommand }) => {
      const { router, pathname } = latest.current;
      if (name === "settings") router.push(`/settings/${DEFAULT_SETTINGS_PAGE}`);
      else if (name === "shortcuts") router.push("/settings/shortcuts");
      else if (name === "reload") { void refreshUiPreferences(); setReloadKey((value) => value + 1); }
      else if (name === "sidebar" || conversationRoute(pathname)) window.dispatchEvent(new CustomEvent("muteki:command", { detail: { name } }));
      else router.push("/chat");
    });
    const offNavigate = bridge.onNavigate?.((event: DesktopNavigateEvent) => {
      if (!isSafeInternalRoute(event?.route)) return false;
      event.handled = true;
      const current = `${window.location.pathname}${window.location.search}${window.location.hash}`;
      if (current !== event.route) latest.current.router.push(event.route);
      return true;
    });
    const reload = () => { void refreshUiPreferences(); setReloadKey(value => value + 1); };
    window.addEventListener("muteki:reload", reload);
    const offClose = bridge.onBeforeClose(({ token }) => {
      const detail: DesktopBeforeCloseDetail = { token, handled: false };
      window.dispatchEvent(new CustomEvent("muteki:before-close", { detail }));
      if (detail.handled || !bridge.acknowledgeClose) return;
      void flushConversationPersistence()
        .catch((error: unknown) => ({ persisted: false, error: error instanceof Error ? error.message : String(error) }))
        .then((result) => bridge.acknowledgeClose!({ token, ...result }))
        .catch((error: unknown) => console.error("desktop close acknowledgement failed", error));
    });
    return () => { offCommand(); offNavigate?.(); offClose(); window.removeEventListener("muteki:reload", reload); };
  }, []);
  return reloadKey;
}
