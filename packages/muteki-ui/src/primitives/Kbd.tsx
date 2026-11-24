import type { ReactNode } from "react";
import { cn } from "../utils/cn";

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

const GLYPHS: Record<string, string> = {
  mod: isMac ? "⌘" : "Ctrl",
  cmd: "⌘",
  ctrl: isMac ? "⌃" : "Ctrl",
  alt: isMac ? "⌥" : "Alt",
  shift: "⇧",
  enter: "↵",
  esc: "Esc",
  up: "↑",
  down: "↓",
  left: "←",
  right: "→",
  backspace: "⌫",
  tab: "Tab",
};

export function formatKey(key: string): string {
  return GLYPHS[key.toLowerCase()] ?? (key.length === 1 ? key.toUpperCase() : key);
}

/** "mod+shift+k" -> ["⌘", "⇧", "K"]; a trailing "+" is the plus key itself ("+", "mod++"). */
export function splitShortcut(shortcut: string): string[] {
  const trimmed = shortcut.trim();
  if (!trimmed) return [];
  if (trimmed === "+") return ["+"];
  const hasTrailingPlus = trimmed.endsWith("++");
  const base = hasTrailingPlus ? trimmed.slice(0, -1) : trimmed;
  const parts = base.split("+").map((part) => part.trim()).filter(Boolean);
  if (hasTrailingPlus) parts.push("+");
  return parts.map((part) => formatKey(part));
}

export function Kbd({
  children,
  tone = "default",
  className,
}: {
  children: ReactNode;
  tone?: "default" | "inverse" | "subtle";
  className?: string;
}) {
  const label = typeof children === "string" ? formatKey(children) : children;
  const isModifierGlyph = typeof label === "string" && /^[⌘⇧⌥⌃⎇⌫⏎⇥←→↑↓⌂⌤]+$/.test(label);
  return (
    <kbd
      data-slot="kbd"
      className={cn(
        "inline-flex h-[22px] min-w-[22px] items-center justify-center rounded-[5px] px-1.5 font-cx-sans text-cx-xs font-medium leading-none",
        tone === "default" && "ui-kbd-bevel border border-cx-border-strong bg-cx-elevated text-cx-fg-2",
        tone === "subtle" && "border-0 bg-cx-hover text-cx-fg-3",
        tone === "inverse" && "border-0 bg-[color-mix(in_srgb,var(--page)_16%,transparent)] text-[color-mix(in_srgb,var(--page)_90%,transparent)]",
        className,
      )}
    >
      {isModifierGlyph ? <span className="text-cx-base leading-none" style={{ fontFamily: "-apple-system, system-ui, sans-serif" }}>{label}</span> : label}
    </kbd>
  );
}

export function Shortcut({ keys, tone, className }: { keys: string; tone?: "default" | "inverse" | "subtle"; className?: string }) {
  return (
    <span data-slot="shortcut" className={cn("inline-flex items-center gap-0.5", className)}>
      {splitShortcut(keys).map((key, index) => <Kbd key={`${key}-${index}`} tone={tone}>{key}</Kbd>)}
    </span>
  );
}
