
import type { ArtifactView } from "./events";
import type { IconName } from "@muteki/ui";

export type RuntimeGroup = "observe" | "investigate" | "assets";
export type RuntimeTab = { view: ArtifactView; key: string; group: RuntimeGroup; icon: IconName; hotkey?: string; alert?: boolean };

export const RUNTIME_TABS: RuntimeTab[] = [
  { view: "timeline", key: "panelbtn.timeline", group: "observe", icon: "rows", hotkey: "t" },
  { view: "workers", key: "panelbtn.workers", group: "observe", icon: "cpu", hotkey: "w" },
  { view: "usage", key: "panelbtn.usage", group: "observe", icon: "rows" },
  { view: "evidence", key: "panelbtn.evidence", group: "investigate", icon: "layers", hotkey: "e" },
  { view: "findings", key: "panelbtn.findings", group: "investigate", icon: "alert", hotkey: "f" },
  { view: "credentials", key: "panelbtn.credentials", group: "assets", icon: "lock", hotkey: "c" },
  { view: "pocs", key: "panelbtn.pocs", group: "assets", icon: "terminal", hotkey: "p" },
  { view: "routes", key: "panelbtn.routes", group: "assets", icon: "network", hotkey: "r" },
  { view: "directives", key: "panelbtn.directives", group: "assets", icon: "send", hotkey: "d" },
];

const LEGACY_PANEL_HOTKEYS: Record<string, ArtifactView> = { b: "collaboration", g: "collaboration" };

export const PANEL_HOTKEYS: Record<string, ArtifactView> = {
  ...LEGACY_PANEL_HOTKEYS,
  ...Object.fromEntries(RUNTIME_TABS.flatMap((tab) => (tab.hotkey ? [[tab.hotkey, tab.view]] : []))),
};

export function panelHotkey(view: ArtifactView): string | undefined {
  return RUNTIME_TABS.find((tab) => tab.view === view)?.hotkey;
}
