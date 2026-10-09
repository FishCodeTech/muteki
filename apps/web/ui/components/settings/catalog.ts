import type { IconName } from "@/components/Icon";
import registry from "./registry.json";

export type SettingsPageId = keyof typeof registry.pages;
export type SettingsClient = "web" | "desktop";
export type SettingsStorage = "ui" | "service" | "device";
export type SettingsGroupId = keyof typeof registry.groups;
export interface SettingsPageMeta {
  id: SettingsPageId; icon: IconName; label: string[]; description: string[]; keywords: string;
  group: SettingsGroupId; platforms: string[]; wide?: boolean;
  requiredApis: Array<{ path: string; match: string; methods: string[] }>;
  requiredFeatures: string[];
}
export const SETTINGS_PAGES = registry.pages as Record<SettingsPageId, SettingsPageMeta>;
export const SETTINGS_GROUP_LABELS = registry.groups;
export const DEFAULT_SETTINGS_PAGE = registry.defaultPage as SettingsPageId;
export const SETTINGS_ALIASES: Readonly<Record<string, string>> = registry.aliases;
export function settingsGroups(client: SettingsClient) {
  return (Object.keys(registry.groups) as SettingsGroupId[]).map(id => ({
    id, pages: (Object.keys(SETTINGS_PAGES) as SettingsPageId[]).filter(page => SETTINGS_PAGES[page].group === id && SETTINGS_PAGES[page].platforms.includes(client)),
  })).filter(group => group.pages.length);
}
export function settingsPageFromPath(pathname: string): SettingsPageId | null {
  const parts = pathname.split("/");
  if (parts[1] !== "settings" || parts.length > 3) return null;
  const id = parts[2] || DEFAULT_SETTINGS_PAGE;
  return Object.hasOwn(SETTINGS_PAGES, id) ? id as SettingsPageId : null;
}
/** Aliases may leave settings; preserve explicit query values and the one-shot anchor. */
export function settingsRedirect(pathname: string, search: URLSearchParams): string | null {
  const segment = pathname.split("/")[2] || "";
  const target = !segment ? `/settings/${DEFAULT_SETTINGS_PAGE}` : SETTINGS_ALIASES[segment];
  if (!target) return null;
  const url = new URL(target, "https://settings.invalid");
  search.forEach((value, key) => { if (!url.searchParams.has(key)) url.searchParams.append(key, value); });
  return url.pathname + url.search;
}
export function settingsStorageDescription(page: SettingsPageId, en: boolean): string {
  const scopes = [...new Set(registry.items.filter(item => item.page === page).map(item => item.storage))];
  const names: Record<string, string> = en
    ? {ui: "Shared preferences on this service", service: "Service configuration", device: "Settings on this device"}
    : {ui: "当前服务共享偏好", service: "服务配置", device: "本机设置"};
  return scopes.map(scope => names[scope]).join(en ? " · " : " · ");
}
