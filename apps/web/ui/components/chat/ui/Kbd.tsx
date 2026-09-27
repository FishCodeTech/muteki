import type { ReactNode } from "react";
import { cn } from "@/lib/cn";

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

/** "mod+shift+k" → ["⌘", "⇧", "K"] */
export function splitShortcut(shortcut: string): string[] {
  return shortcut.split("+").map((part) => formatKey(part.trim()));
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
  return (
    <kbd
      className={cn(
        "inline-flex h-[18px] min-w-[18px] items-center justify-center rounded-[5px] px-1 font-cx-sans text-[10.5px] font-medium leading-none",
        tone === "default" && "border border-cx-border bg-cx-elevated text-cx-fg-3 shadow-[0_1px_0_var(--cx-border)]",
        tone === "subtle" && "bg-cx-hover text-cx-fg-3",
        tone === "inverse" && "bg-[color-mix(in_srgb,var(--page)_16%,transparent)] text-[color-mix(in_srgb,var(--page)_82%,transparent)]",
        className,
      )}
    >
      {label}
    </kbd>
  );
}

export function Shortcut({ keys, tone, className }: { keys: string; tone?: "default" | "inverse" | "subtle"; className?: string }) {
  return (
    <span className={cn("inline-flex items-center gap-0.5", className)}>
      {splitShortcut(keys).map((key, index) => <Kbd key={`${key}-${index}`} tone={tone}>{key}</Kbd>)}
    </span>
  );
}
