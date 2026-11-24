"use client";

import { usePathname } from "next/navigation";
import { useCallback, useEffect, useState } from "react";
import { Icon } from "@muteki/ui";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { APP_NAME } from "@/lib/brand";
import { isConversationLayoutPath, isSrcConversationPath } from "@/lib/workspaceSkipTarget";
import { readThemePreference, subscribeThemePreference } from "@/lib/themePreference";

export type DesktopHeaderSlotKey = "chat" | "src" | "default";
export type DesktopHeaderSlots = Record<DesktopHeaderSlotKey, HTMLDivElement | null>;

const TITLES: Record<string, string> = { chat: "对话", ctf: "CTF 工作台", run: "CTF 工作台", solve: "CTF 工作台", pentest: "渗透测试", src: "SRC 赏金", competitions: "比赛", usage: "全局用量", settings: "设置" };

export function activeDesktopHeaderSlotKey(pathname: string): DesktopHeaderSlotKey | null {
  if (pathname === "/chat" || pathname.startsWith("/chat/")) return "chat";
  if (isSrcConversationPath(pathname)) return "src";
  if (isConversationLayoutPath(pathname)) return "default";
  return null;
}

export function DesktopTitlebar({ setHeaderSlot, configuring, sidebarExpanded }: {
  setHeaderSlot: (key: DesktopHeaderSlotKey, element: HTMLDivElement | null) => void;
  configuring: boolean;
  sidebarExpanded: boolean;
}) {
  const pathname = usePathname();
  const [history, setHistory] = useState({ canGoBack: false, canGoForward: false });
  const title = TITLES[pathname.split("/")[1]] || APP_NAME;
  const activeSlot = activeDesktopHeaderSlotKey(pathname);
  const conversation = activeSlot !== null;
  const setChatSlot = useCallback((el: HTMLDivElement | null) => setHeaderSlot("chat", el), [setHeaderSlot]);
  const setSrcSlot = useCallback((el: HTMLDivElement | null) => setHeaderSlot("src", el), [setHeaderSlot]);
  const setDefaultSlot = useCallback((el: HTMLDivElement | null) => setHeaderSlot("default", el), [setHeaderSlot]);
  useEffect(() => {
    const bridge = desktopChatBridge();
    let active = true;
    const sync = () => {
      const theme = document.documentElement.dataset.theme === "light" ? "light" : "dark";
      const mode = readThemePreference();
      void bridge?.setWindowChrome?.({ theme, mode, title }).then(state => { if (active) setHistory(state); }).catch(console.error);
    };
    sync();
    const observer = new MutationObserver(sync);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    const stopThemePref = subscribeThemePreference(sync);
    return () => { active = false; observer.disconnect(); stopThemePref(); };
  }, [title, pathname]);
  return <header className="desktop-titlebar" onMouseDown={event => {
    if (event.button !== 0 || (event.target as Element).closest("button, a, input, [role='button']")) return;
    void desktopChatBridge()?.windowAction?.(event.detail === 2 ? "zoom" : "drag").catch(console.error);
  }}>
    <nav className="desktop-window-navigation" aria-label="窗口导航">
      <button type="button" aria-label="返回" title="返回" disabled={configuring || !history.canGoBack} onClick={() => void desktopChatBridge()?.windowAction?.("back")}><Icon name="arrowLeft" size={17} /></button>
      <button type="button" aria-label="前进" title="前进" disabled={configuring || !history.canGoForward} onClick={() => void desktopChatBridge()?.windowAction?.("forward")}><Icon name="arrowRight" size={17} /></button>
      <button type="button" aria-label="切换会话侧栏" title="切换会话侧栏" aria-expanded={sidebarExpanded} disabled={configuring || !conversation} onClick={() => window.dispatchEvent(new CustomEvent("muteki:command", { detail: { name: "sidebar" } }))}><Icon name="panelLeft" size={17} /></button>
    </nav>
    <h1>{title}</h1>
    <div className="desktop-titlebar-thread" data-titlebar-slot="chat" ref={setChatSlot} hidden={configuring || activeSlot !== "chat"} />
    <div className="desktop-titlebar-thread" data-titlebar-slot="src" ref={setSrcSlot} hidden={configuring || activeSlot !== "src"} />
    <div className="desktop-titlebar-thread" data-titlebar-slot="default" ref={setDefaultSlot} hidden={configuring || activeSlot !== "default"} />
  </header>;
}
