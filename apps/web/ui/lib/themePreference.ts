"use client";
import { useSyncExternalStore } from "react";
import { applySelection, readSavedSelection, readSavedTheme, type SchemeSelection, type ThemeMode } from "./palette-engine";
import { readUiPreference, hasLoadedUiPreferences, writeUiPreferences, subscribeUiPreferences } from "./uiPreferences";
import { DESKTOP_APPEARANCE_KEY, readAppearanceOverride } from "./desktopEnvironment";
export type ThemePreference = ThemeMode | "system";
export const THEME_PREFERENCE_KEY = "muteki.themePreference";
export function readThemePreference(): ThemePreference {
  const override = readAppearanceOverride(); if (override) return override.preference;
  let fallback: ThemePreference = "system";
  if (!hasLoadedUiPreferences()) { try { const saved = localStorage.getItem(THEME_PREFERENCE_KEY); if (saved === "light" || saved === "dark" || saved === "system") fallback = saved; } catch { /* before a verified service loads */ } }
  return readUiPreference("theme", fallback);
}
export function systemTheme(): ThemeMode { return typeof window !== "undefined" && window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark"; }
export function resolveThemePreference(preference: ThemePreference): ThemeMode { return preference === "system" ? systemTheme() : preference; }
export function setThemePreference(preference: ThemePreference, selection?: SchemeSelection): void {
  writeUiPreferences({theme: preference, ...(selection ? {accent: selection} : {})});
  applySelection(selection || readSavedSelection(), resolveThemePreference(preference));
}
export function syncSystemTheme(): void { applySelection(readSavedSelection(), readSavedTheme()); }
export function watchSystemTheme(): () => void {
  const media = window.matchMedia?.("(prefers-color-scheme: light)");
  const apply = () => { if (readThemePreference() === "system") applySelection(readSavedSelection(), systemTheme()); };
  media?.addEventListener("change", apply); return () => media?.removeEventListener("change", apply);
}
export function subscribeThemePreference(listener: () => void): () => void {
  const off = subscribeUiPreferences(listener);
  const storage = (event: StorageEvent) => { if (event.key === DESKTOP_APPEARANCE_KEY) listener(); };
  window.addEventListener("storage", storage);
  return () => { off(); window.removeEventListener("storage", storage); };
}
export function useThemePreference() { return useSyncExternalStore(subscribeThemePreference, readThemePreference, () => "system" as const); }
