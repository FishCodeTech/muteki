"use client";

import { useEffect, useState } from "react";

export type VisualizationTheme = { appearance: "light" | "dark"; variables: Record<string, string> };
const TOKENS: Record<string, string> = {
  background: "bg", foreground: "fg", card: "elevated", "card-foreground": "fg",
  muted: "sunken", "muted-foreground": "fg-3", popover: "overlay", "popover-foreground": "fg",
  secondary: "hover", "secondary-foreground": "fg", border: "border", input: "border-strong", ring: "focus",
  primary: "fg", "primary-foreground": "bg", accent: "accent", "accent-foreground": "accent-fg",
  "accent-surface": "accent-soft", "accent-surface-foreground": "accent",
  destructive: "danger", "destructive-foreground": "danger", "destructive-surface": "danger-soft",
  warning: "warning", "warning-foreground": "warning", "warning-surface": "warning-soft",
  success: "success", "success-foreground": "success", "code-background": "code", "code-foreground": "fg",
  "font-sans": "font-sans", "font-mono": "font-mono", "chart-1": "accent", "viz-series-1": "accent",
};

/** Resolve host values, including custom schemes and fonts, without reloading the page. */
export function useVisualizationTheme(): VisualizationTheme | null {
  const [theme, setTheme] = useState<VisualizationTheme | null>(null);
  useEffect(() => {
    const root = document.documentElement;
    let raf = 0;
    const read = () => {
      const style = getComputedStyle(root);
      const variables = Object.fromEntries(Object.entries(TOKENS).flatMap(([name, token]) => {
        const value = style.getPropertyValue(`--cx-${token}`).trim();
        return value ? [[`--${name}`, value]] : [];
      }));
      for (let i = 2; i <= 6; i++) variables[`--chart-${i}`] = `var(--viz-series-${i})`;
      const next: VisualizationTheme = { appearance: root.dataset.theme === "light" ? "light" : "dark", variables };
      setTheme((current) => JSON.stringify(current) === JSON.stringify(next) ? current : next);
    };
    const schedule = () => { cancelAnimationFrame(raf); raf = requestAnimationFrame(read); };
    const observer = new MutationObserver(schedule);
    observer.observe(root, { attributes: true });
    observer.observe(document.head, { childList: true, subtree: true, characterData: true });
    read();
    return () => { observer.disconnect(); cancelAnimationFrame(raf); };
  }, []);
  return theme;
}
