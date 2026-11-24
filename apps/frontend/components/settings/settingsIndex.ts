import registry from "./registry.json";
import type { SettingsPageId, SettingsStorage } from "./catalog";
export interface SettingsIndexEntry {
  page: SettingsPageId; anchor: string; section: string[]; label: string[]; keywords: string; storage: SettingsStorage;
}
export const SETTINGS_INDEX = registry.items as SettingsIndexEntry[];
export function localizedSettingsEntries(en: boolean) {
  const i = en ? 1 : 0;
  return SETTINGS_INDEX.map(entry => ({pageId: entry.page, anchor: entry.anchor, label: entry.label[i], section: entry.section[i],
    keywords: `${entry.label.join(" ")} ${entry.section.join(" ")} ${entry.keywords}`}));
}
export function settingsAnchorFromHash(hash: string): string | null {
  let anchor: string;
  try { anchor = decodeURIComponent(hash.replace(/^#/, "")).replace(/^setting-/, ""); } catch { return null; }
  return SETTINGS_INDEX.some(item => item.anchor === anchor) ? anchor : null;
}
