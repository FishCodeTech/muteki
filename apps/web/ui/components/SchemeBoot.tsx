"use client";

import { useEffect } from "react";
import { applySelection, readSavedSelection, readSavedTheme } from "../lib/palette-engine";
import { readAppearanceOverride } from "@/lib/desktopEnvironment";
import { readMotionPreference, useMotionPreference } from "@/lib/motionPreference";
import { applyFontPreferences, subscribeChatPreferences } from "@/lib/chatPreferences";
import { subscribeThemePreference, syncSystemTheme, watchSystemTheme } from "@/lib/themePreference";

/**
 * Applies the persisted color scheme on routes that don't own scheme state
 * (e.g. /ctf/workers). The main shell (app/page.tsx) manages scheme
 * interactively and re-applies on every change; this boot pass only needs to
 * run once per mount so a direct visit to a secondary route still gets the
 * saved scheme instead of the static globals.css fallback.
 */
export default function SchemeBoot() {
  const motion = useMotionPreference();
  useEffect(() => { document.documentElement.dataset.motion = readMotionPreference(); }, [motion]);
  useEffect(() => {
    const apply = () => {
      const mode = readSavedTheme();
      document.documentElement.dataset.theme = mode;
      document.documentElement.classList.toggle("dark", mode === "dark");
      document.documentElement.classList.toggle("light", mode === "light");
      applySelection(readSavedSelection(), mode);
      const override = readAppearanceOverride();
      if (override) document.documentElement.dataset.desktopAppearanceApplied = JSON.stringify(override);
    };
    syncSystemTheme();
    apply();
    applyFontPreferences();
    // Keeps other tabs and the desktop shell's embedded pages in step with appearance changes made elsewhere.
    const onStorage = (event: StorageEvent) => {
      if (event.key === "muteki.theme" || event.key === "muteki.scheme" || event.key === "muteki.schemeHue") apply();
    };
    window.addEventListener("storage", onStorage);
    const stopFonts = subscribeChatPreferences(() => {});
    const stopSystem = watchSystemTheme();
    const stopShared = subscribeThemePreference(apply);
    return () => { window.removeEventListener("storage", onStorage); stopFonts(); stopSystem(); stopShared(); };
  }, []);
  return null;
}
