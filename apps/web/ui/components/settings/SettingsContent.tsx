"use client";
import { lazy, Suspense, type ComponentType } from "react";
import { Spinner } from "@/components/chat/ui";
import { SETTINGS_PAGES, type SettingsPageId } from "./catalog";
import { useSettingsHost } from "./SettingsHost";

// Every registered page has exactly one shared loader. No Next imports cross this boundary.
export const SETTINGS_LOADERS: Record<SettingsPageId, ComponentType> = {
  appearance: lazy(async () => { const { AppearanceSettings } = await import("./AppearanceSettings"); return {default: function Appearance() { return <AppearanceSettings />; }}; }),
  chat: lazy(async () => { const { ChatSettings } = await import("./ChatSettings"); return {default: function Chat() { const host = useSettingsHost(); return <ChatSettings navigate={href => host.navigation.router.push(href)} />; }}; }),
  notifications: lazy(() => import("./NotificationsSettings").then(m => ({default: m.NotificationsSettings}))),
  shortcuts: lazy(() => import("./ShortcutsSettings").then(m => ({default: m.ShortcutsSettings}))),
  agents: lazy(async () => { const { AgentsSettings } = await import("./agents/AgentsSettings"); return {default: function Agents() { const host = useSettingsHost(); return <div className="cx-settings-agents"><AgentsSettings navigation={host.navigation} /></div>; }}; }),
  "agent-extensions": lazy(async () => { const { AgentExtensionsSettings } = await import("../AgentExtensionsSettings"); return {default: function Extensions() { const host = useSettingsHost(); return <div className="cx-settings-agent-ext"><AgentExtensionsSettings navigation={host.navigation} /></div>; }}; }),
  capabilities: lazy(() => import("../CapabilityManagement").then(m => ({default: () => <div className="cx-settings-capabilities"><m.CapabilityManagement hideIntro /></div>}))),
  archives: lazy(() => import("./ArchivesSettings")),
  import: lazy(() => import("./ImportSettings")),
  extensions: lazy(() => import("../ExtensionSettings").then(m => ({default: () => <div className="cx-settings-extensions"><m.ExtensionSettings /></div>}))),
  operations: lazy(() => import("../OperationsSettings").then(m => ({default: () => <div className="cx-settings-operations"><m.OperationsSettings /></div>}))),
  update: lazy(() => import("./UpdateSettings").then(m => ({default: m.UpdateSettings}))),
  access: lazy(() => import("./AccessSettings").then(m => ({default: m.AccessSettings}))),
  desktop: lazy(async () => { const { DesktopPage } = await import("./DesktopConnectionSettings"); return {default: function Desktop() { const host = useSettingsHost(); return host.desktop ? <DesktopPage {...host.desktop} /> : null; }}; }),
};
export function SettingsContent({page}: {page: SettingsPageId}) {
  const host = useSettingsHost();
  if (!SETTINGS_PAGES[page].platforms.includes(host.client)) return <p role="status">此设置只适用于桌面客户端。</p>;
  const Page = SETTINGS_LOADERS[page];
  return <Suspense fallback={<div role="status" aria-label="加载设置" className="grid h-40 place-items-center"><Spinner /></div>}><Page /></Suspense>;
}
