"use client";
import { lazy, Suspense, useEffect, useMemo, useState, type ComponentType } from "react";
import { Spinner } from "@muteki/ui";
import { OverlayVisibilityContext } from "@muteki/ui";
import { SETTINGS_PAGES, type SettingsClient, type SettingsPageId } from "./catalog";
import { useSettingsHost } from "./SettingsHost";

type PageModule = { default: ComponentType };
function settingsPage(load: () => Promise<PageModule>) {
  let pending: Promise<PageModule> | undefined;
  const preload = () => pending ??= load().catch(error => { pending = undefined; throw error; });
  return { Component: lazy(preload), preload };
}

const SETTINGS_LOADERS: Record<SettingsPageId, ReturnType<typeof settingsPage>> = {
  appearance: settingsPage(async () => { const { AppearanceSettings } = await import("./AppearanceSettings"); return {default: function Appearance() { return <AppearanceSettings />; }}; }),
  chat: settingsPage(async () => { const { ChatSettings } = await import("./ChatSettings"); return {default: function Chat() { return <ChatSettings />; }}; }),
  notifications: settingsPage(() => import("./NotificationsSettings").then(m => ({default: m.NotificationsSettings}))),
  shortcuts: settingsPage(() => import("./ShortcutsSettings").then(m => ({default: m.ShortcutsSettings}))),
  agents: settingsPage(async () => { const { AgentsSettings } = await import("./agents/AgentsSettings"); return {default: function Agents() { const host = useSettingsHost(); return <div className="cx-settings-agents"><AgentsSettings navigation={host.navigation} /></div>; }}; }),
  "agent-extensions": settingsPage(async () => { const { AgentExtensionsSettings } = await import("../AgentExtensionsSettings"); return {default: function Extensions() { const host = useSettingsHost(); return <div className="cx-settings-agent-ext"><AgentExtensionsSettings navigation={host.navigation} /></div>; }}; }),
  capabilities: settingsPage(() => import("../CapabilityManagement").then(m => ({default: () => <div className="cx-settings-capabilities"><m.CapabilityManagement hideIntro /></div>}))),
  "source-control": settingsPage(() => import("./ControlPolicySettings").then(m => ({default: m.SourceControlSettings}))),
  control: settingsPage(() => import("./ControlSettings").then(m => ({default: m.ControlSettings}))),
  orchestration: settingsPage(() => import("./OrchestrationSettings").then(m => ({default: m.OrchestrationSettings}))),
  archives: settingsPage(() => import("./ArchivesSettings")),
  import: settingsPage(() => import("./ImportSettings")),
  extensions: settingsPage(() => import("../ExtensionSettings").then(m => ({default: () => <div className="cx-settings-extensions"><m.ExtensionSettings /></div>}))),
  operations: settingsPage(() => import("../OperationsSettings").then(m => ({default: () => <div className="cx-settings-operations"><m.OperationsSettings /></div>}))),
  update: settingsPage(() => import("./UpdateSettings").then(m => ({default: m.UpdateSettings}))),
  access: settingsPage(() => import("./AccessSettings").then(m => ({default: m.AccessSettings}))),
  "network-proxy": settingsPage(() => import("./NetworkProxySettings").then(m => ({default: m.NetworkProxySettings}))),
  desktop: settingsPage(async () => { const { DesktopPage } = await import("./DesktopConnectionSettings"); return {default: function Desktop() { const host = useSettingsHost(); return host.desktop ? <DesktopPage {...host.desktop} /> : null; }}; }),
};

function preloadSettingsPages(client: SettingsClient) {
  for (const id of Object.keys(SETTINGS_LOADERS) as SettingsPageId[]) {
    if (SETTINGS_PAGES[id].platforms.includes(client)) void SETTINGS_LOADERS[id].preload().catch(() => {});
  }
}

export function SettingsContent({page}: {page: SettingsPageId}) {
  const host = useSettingsHost();
  if (!SETTINGS_PAGES[page].platforms.includes(host.client)) return <p role="status">此设置只适用于桌面客户端。</p>;
  const Page = SETTINGS_LOADERS[page].Component;
  return <Page />;
}

export function SettingsPages({active}: {active: SettingsPageId}) {
  const host = useSettingsHost();
  const [visited, setVisited] = useState<SettingsPageId[]>([active]);
  const pages = useMemo(() => visited.includes(active) ? visited : [...visited, active], [visited, active]);
  useEffect(() => { if (pages !== visited) setVisited(pages); }, [pages, visited]);
  useEffect(() => {
    const idle = window.requestIdleCallback?.(() => preloadSettingsPages(host.client), {timeout: 2000});
    const timer = idle === undefined ? window.setTimeout(() => preloadSettingsPages(host.client), 300) : undefined;
    return () => { if (idle !== undefined) window.cancelIdleCallback(idle); if (timer !== undefined) window.clearTimeout(timer); };
  }, [host.client]);
  return <>{pages.map(id => <OverlayVisibilityContext.Provider key={id} value={id === active}><div hidden={id !== active} className={id === active ? "contents" : "hidden"}>
    <Suspense fallback={<div role="status" aria-label="加载设置" className="grid h-40 place-items-center"><Spinner /></div>}>
      <SettingsContent page={id} />
    </Suspense>
  </div></OverlayVisibilityContext.Provider>)}</>;
}
