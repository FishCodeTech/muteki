export const DESKTOP_APPEARANCE_KEY = "muteki.desktop.appearanceOverride";
let workspaceActive = typeof navigator === "undefined" || !/\bMutekiWarmup\/1\b/.test(navigator.userAgent);
export function isDesktopWorkspaceActive(): boolean { return workspaceActive; }
export function setDesktopWorkspaceActive(active: boolean): void {
  const changed = workspaceActive !== active;
  workspaceActive = active;
  if (changed && active) window.dispatchEvent(new Event("muteki:workspace-activated"));
}
export function isDesktopRemote(): boolean {
  return typeof navigator !== "undefined" && /(?:^|\s)MutekiDesktop\/[\w.+-]+(?:\s|$)/.test(navigator.userAgent);
}
export type AppearanceOverride = {
  preference: "system" | "light" | "dark";
  resolvedTheme: "light" | "dark";
  selection: {kind: "preset"; id: string} | {kind: "custom"; hue: number; hex?: string};
};
export function readAppearanceOverride(): AppearanceOverride | null {
  if (!isDesktopRemote()) return null;
  try {
    const value = JSON.parse(localStorage.getItem(DESKTOP_APPEARANCE_KEY) || "null");
    if (!value || !["system", "light", "dark"].includes(value.preference) || !["light", "dark"].includes(value.resolvedTheme)) return null;
    if (value.selection?.kind === "preset" && ["azure", "violet", "teal", "ember"].includes(value.selection.id)) return value;
    if (value.selection?.kind === "custom" && Number.isFinite(value.selection.hue) && value.selection.hue >= 0 && value.selection.hue < 360 && (value.selection.hex === undefined || (typeof value.selection.hex === "string" && /^#[0-9a-f]{6}$/i.test(value.selection.hex)))) return value;
  } catch {  }
  return null;
}
