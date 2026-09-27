/**
 * The runtime panel tab registry: order, grouping, icon, and the single-key
 * shortcut each panel answers to. page.tsx (key handler + tab strip),
 * RunInspector (panel buttons) and CommandPalette (kbd hints) all derive from
 * this table so a shortcut is only ever declared once.
 */

import type { ArtifactView } from "./events";
import type { IconName } from "./iconNames";

export type RuntimeGroup = "observe" | "investigate" | "assets";
export type RuntimeTab = { view: ArtifactView; key: string; group: RuntimeGroup; icon: IconName; hotkey?: string; alert?: boolean };

export const RUNTIME_TABS: RuntimeTab[] = [
  { view: "timeline", key: "panelbtn.timeline", group: "observe", icon: "rows", hotkey: "t" },
  { view: "workers", key: "panelbtn.workers", group: "observe", icon: "cpu", hotkey: "w" },
  { view: "usage", key: "panelbtn.usage", group: "observe", icon: "rows" },
  { view: "evidence", key: "panelbtn.evidence", group: "investigate", icon: "layers", hotkey: "e" },
  { view: "findings", key: "panelbtn.findings", group: "investigate", icon: "alert", hotkey: "f" },
  { view: "reports", key: "panelbtn.reports", group: "assets", icon: "list", hotkey: "o" },
  { view: "credentials", key: "panelbtn.credentials", group: "assets", icon: "lock", hotkey: "c" },
  { view: "pocs", key: "panelbtn.pocs", group: "assets", icon: "terminal", hotkey: "p" },
  { view: "routes", key: "panelbtn.routes", group: "assets", icon: "network", hotkey: "r" },
  { view: "directives", key: "panelbtn.directives", group: "assets", icon: "send", hotkey: "d" },
];

// The collaboration map is its own page (/run/<id>/collaboration), not a runtime
// panel; `g` (and the retired blackboard `b`) still route to it.
const LEGACY_PANEL_HOTKEYS: Record<string, ArtifactView> = { b: "collaboration", g: "collaboration" };

/** key → view for the global single-key handler. */
export const PANEL_HOTKEYS: Record<string, ArtifactView> = {
  ...LEGACY_PANEL_HOTKEYS,
  ...Object.fromEntries(RUNTIME_TABS.flatMap((tab) => (tab.hotkey ? [[tab.hotkey, tab.view]] : []))),
};

export function panelHotkey(view: ArtifactView): string | undefined {
  return RUNTIME_TABS.find((tab) => tab.view === view)?.hotkey;
}
