"use client";

import { useEffect } from "react";
import { applySelection, readSavedSelection, readSavedTheme } from "../lib/palette-engine";
import { readAppearanceOverride } from "@/lib/desktopEnvironment";
import { readMotionPreference, useMotionPreference } from "@/lib/motionPreference";
import { applyFontPreferences, subscribeChatPreferences } from "@/lib/chatPreferences";
import { subscribeThemePreference, watchSystemTheme, persistLocalThemePreference, readThemePreference, THEME_PREFERENCE_KEY } from "@/lib/themePreference";
import { hasLoadedUiPreferences } from "@/lib/uiPreferences";

export default function SchemeBoot() {
  const motion = useMotionPreference();
  useEffect(() => {
    if (location.pathname !== "/native-report-preview") document.documentElement.dataset.motion = readMotionPreference();
  }, [motion]);
  useEffect(() => {
    if (location.pathname === "/native-report-preview") return;
    const apply = () => {
      if (hasLoadedUiPreferences()) persistLocalThemePreference(readThemePreference());
      applySelection(readSavedSelection(), readSavedTheme());
      const override = readAppearanceOverride();
      if (override) document.documentElement.dataset.desktopAppearanceApplied = JSON.stringify(override);
    };
    apply();
    applyFontPreferences();
    const onStorage = (event: StorageEvent) => {
      if (event.key === "muteki.theme" || event.key === THEME_PREFERENCE_KEY || event.key === "muteki.scheme" || event.key === "muteki.schemeHue") apply();
    };
    window.addEventListener("storage", onStorage);
    const stopFonts = subscribeChatPreferences(() => {});
    const stopSystem = watchSystemTheme();
    const stopShared = subscribeThemePreference(apply);
    return () => { window.removeEventListener("storage", onStorage); stopFonts(); stopSystem(); stopShared(); };
  }, []);
  return null;
}
