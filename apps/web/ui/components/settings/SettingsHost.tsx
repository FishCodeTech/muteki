"use client";

import { createContext, useContext, useEffect, useMemo, useRef, type AnchorHTMLAttributes, type ReactNode } from "react";
import { useLang } from "@/lib/i18n";
import { conversationStorageScope, subscribeConversationStorageScope } from "@/lib/conversationStorageScope";
import { useSyncExternalStore } from "react";
import { SETTINGS_PAGES, SETTINGS_GROUP_LABELS, settingsGroups, settingsPageFromPath, settingsStorageDescription, type SettingsClient } from "./catalog";
import { localizedSettingsEntries, settingsAnchorFromHash } from "./settingsIndex";
import { SettingsNav, SettingsPage, revealSettingsAnchor, type SettingsNavLinkProps } from "./primitives";
import { SettingsCompatibility } from "./SettingsCompatibility";
import type { AgentsSettingsNavigation } from "./agents/shared";

export interface SettingsNavigation extends AgentsSettingsNavigation {
  hash?: string;
  anchorRequestId?: number;
  consumeAnchor?(id: number): void;
}
export interface SettingsHostValue {
  client: SettingsClient;
  navigation: SettingsNavigation;
  desktop?: { origin: string; connected: boolean; onConfigure(): void; onHelp(): void };
}
const Context = createContext<SettingsHostValue | null>(null);
export function useSettingsHost(): SettingsHostValue {
  const value = useContext(Context);
  if (!value) throw new Error("Settings host is missing");
  return value;
}
export function SettingsLink({ href = "", onClick, ...props }: AnchorHTMLAttributes<HTMLAnchorElement>) {
  const { navigation } = useSettingsHost();
  return <a {...props} href={href} onClick={event => {
    onClick?.(event);
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault(); navigation.router.push(href);
  }} />;
}
function NavLink({ href, children, ...props }: SettingsNavLinkProps) {
  const { client, navigation } = useSettingsHost();
  return <a {...props} href={href} onClick={event => {
    props.onClick?.(event); if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault(); navigation.router[client === "desktop" ? "replace" : "push"](href);
  }}>{children}</a>;
}

export function SettingsHost({ children, back, ...host }: SettingsHostValue & {
  children: ReactNode; back: { href?: string; onClick?: () => void };
}) {
  const { lang } = useLang(); const en = lang === "en";
  const active = settingsPageFromPath(host.navigation.pathname);
  const meta = active ? SETTINGS_PAGES[active] : null;
  const scope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => "");
  const {hash, anchorRequestId, consumeAnchor} = host.navigation;
  const scroller = useRef<HTMLDivElement>(null);
  useEffect(() => { scroller.current?.scrollTo({ top: 0 }); }, [active]);
  useEffect(() => {
    const anchor = settingsAnchorFromHash(hash || "");
    if (!anchor || !active) return;
    const controller = new AbortController();
    revealSettingsAnchor(anchor, controller.signal, () => { if (anchorRequestId !== undefined) consumeAnchor?.(anchorRequestId); });
    return () => controller.abort();
  }, [active, hash, anchorRequestId, consumeAnchor, scope]);
  const groups = useMemo(() => settingsGroups(host.client).map(group => ({
    id: group.id, label: SETTINGS_GROUP_LABELS[group.id][en ? 1 : 0],
    items: group.pages.map(id => {
      const page = SETTINGS_PAGES[id];
      return {id, href: `/settings/${id}`, label: page.label[en ? 1 : 0], icon: page.icon,
        keywords: `${page.label.join(" ")} ${page.description.join(" ")} ${page.keywords}`};
    }),
  })), [host.client, en]);
  const entries = useMemo(() => localizedSettingsEntries(en), [en]);
  return <Context.Provider value={host}>
    <div className={`cx-root cx-settings-shell ${host.client === "web" ? "cx-settings-web" : ""}`} data-testid={`${host.client}-settings`} data-page={active || "unknown"}>
      <SettingsNav title={en ? "Settings" : "设置"} back={{...back, label: en ? "Back to app" : "返回应用"}}
        groups={groups} entries={entries} activeId={active || undefined} link={NavLink}
        onNavigate={href => host.navigation.router[host.client === "desktop" ? "replace" : "push"](href)}
        labels={{search: en ? "Search settings" : "搜索设置", clear: en ? "Clear search" : "清除搜索", empty: en ? "No matching settings" : "没有匹配的设置", nav: en ? "Settings categories" : "设置分类", items: en ? "Settings" : "设置项"}} />
      <div ref={scroller} id="settings-main" className="cx-settings-content">
        <SettingsPage wide={meta?.wide} title={meta?.label[en ? 1 : 0] || (en ? "Page not found" : "设置页面不存在")}
          description={meta ? <>{meta.description[en ? 1 : 0]}<span className="mt-1 block text-[12px]">{settingsStorageDescription(meta.id, en)}</span></> : undefined}>
          <SettingsCompatibility page={active}>{children}</SettingsCompatibility>
        </SettingsPage>
      </div>
    </div>
  </Context.Provider>;
}
