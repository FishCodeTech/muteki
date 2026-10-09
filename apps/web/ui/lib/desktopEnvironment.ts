export const DESKTOP_APPEARANCE_KEY = "muteki.desktop.appearanceOverride";
export function isDesktopRemote(): boolean {
  return typeof navigator !== "undefined" && /(?:^|\s)MutekiDesktop\/[\w.+-]+(?:\s|$)/.test(navigator.userAgent);
}
export type AppearanceOverride = {
  preference: "system" | "light" | "dark";
  resolvedTheme: "light" | "dark";
  selection: {kind: "preset"; id: string} | {kind: "custom"; hue: number};
};
export function readAppearanceOverride(): AppearanceOverride | null {
  if (!isDesktopRemote()) return null;
  try {
    const value = JSON.parse(localStorage.getItem(DESKTOP_APPEARANCE_KEY) || "null");
    if (!value || !["system", "light", "dark"].includes(value.preference) || !["light", "dark"].includes(value.resolvedTheme)) return null;
    if (value.selection?.kind === "preset" && ["azure", "violet", "teal", "ember"].includes(value.selection.id)) return value;
    if (value.selection?.kind === "custom" && Number.isFinite(value.selection.hue) && value.selection.hue >= 0 && value.selection.hue < 360) return value;
  } catch { /* A corrupt display cache is ignored; it is never a preference migration source. */ }
  return null;
}
