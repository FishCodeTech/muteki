"use client";

import { useSyncExternalStore } from "react";
import { readUiPreference, subscribeUiPreferences, writeUiPreferences, type UiPatch } from "./uiPreferences";

export type EngineLogoColorMode = "brand" | "mono" | "custom";

export interface EngineLogoPreferences {
  color: EngineLogoColorMode;
  customColor: string;
  sidebarModelIcon: boolean;
}

export const DEFAULT_ENGINE_LOGO_PREFERENCES: EngineLogoPreferences = {
  color: "brand",
  customColor: "#8A8FA3",
  sidebarModelIcon: true,
};

const HEX = /^#[0-9a-f]{6}$/i;
const MODES: EngineLogoColorMode[] = ["brand", "mono", "custom"];

export function sanitizeEngineLogoColor(value: string): string {
  const hex = value.trim();
  return HEX.test(hex) ? hex : DEFAULT_ENGINE_LOGO_PREFERENCES.customColor;
}

let cached: EngineLogoPreferences | null = null;

export function readEngineLogoPreferences(): EngineLogoPreferences {
  if (cached) return cached;
  const mode = readUiPreference("engineLogoColor", DEFAULT_ENGINE_LOGO_PREFERENCES.color);
  cached = {
    color: MODES.includes(mode) ? mode : DEFAULT_ENGINE_LOGO_PREFERENCES.color,
    customColor: sanitizeEngineLogoColor(readUiPreference("engineLogoCustomColor", DEFAULT_ENGINE_LOGO_PREFERENCES.customColor)),
    sidebarModelIcon: readUiPreference("sidebarModelIcon", DEFAULT_ENGINE_LOGO_PREFERENCES.sidebarModelIcon) !== false,
  };
  return cached;
}
subscribeUiPreferences(() => { cached = null; });

export function writeEngineLogoPreferences(patch: Partial<EngineLogoPreferences>): boolean {
  const values: UiPatch = {};
  if (patch.color) values.engineLogoColor = patch.color;
  if (patch.customColor !== undefined) values.engineLogoCustomColor = sanitizeEngineLogoColor(patch.customColor);
  if (patch.sidebarModelIcon !== undefined) values.sidebarModelIcon = patch.sidebarModelIcon;
  cached = null;
  return !Object.keys(values).length || writeUiPreferences(values);
}

export function useEngineLogoPreferences(): EngineLogoPreferences {
  return useSyncExternalStore(subscribeUiPreferences, readEngineLogoPreferences, () => DEFAULT_ENGINE_LOGO_PREFERENCES);
}
