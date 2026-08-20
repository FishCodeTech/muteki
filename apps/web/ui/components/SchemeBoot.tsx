"use client";

import { useEffect } from "react";
import { applySelection, readSavedSelection, readSavedTheme } from "../lib/palette-engine";

/**
 * Applies the persisted color scheme on routes that don't own scheme state
 * (e.g. /settings/workers). The main shell (app/page.tsx) manages scheme
 * interactively and re-applies on every change; this boot pass only needs to
 * run once per mount so a direct visit to a secondary route still gets the
 * saved scheme instead of the static globals.css fallback.
 */
export default function SchemeBoot() {
  useEffect(() => {
    const mode = readSavedTheme();
    document.documentElement.dataset.theme = mode;
    applySelection(readSavedSelection(), mode);
  }, []);
  return null;
}
