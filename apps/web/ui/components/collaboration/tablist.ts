import type { KeyboardEvent } from "react";

/** Validate an external DOM/store string while preserving the caller's literal tab-id union. */
export function isTabId<T extends string>(tabs: readonly T[], value: unknown): value is T {
  return typeof value === "string" && tabs.some((tab) => tab === value);
}

/** Resolve the next tab for Arrow/Home/End. Returns null when the key is unrelated or tabs are empty. */
export function nextTabFromKey<T extends string>(
  key: string,
  tabs: readonly T[],
  current: T,
): T | null {
  if (tabs.length === 0) return null;
  const index = tabs.indexOf(current);
  const safeIndex = index >= 0 ? index : 0;
  switch (key) {
    case "ArrowRight":
      return tabs[(safeIndex + 1) % tabs.length];
    case "ArrowLeft":
      return tabs[(safeIndex - 1 + tabs.length) % tabs.length];
    case "Home":
      return tabs[0];
    case "End":
      return tabs[tabs.length - 1];
    default:
      return null;
  }
}

/**
 * Roving tabindex: Left/Right/Home/End move among tabs, select (automatic activation), and focus the next one.
 * `tabIdPrefix` defaults to `collab-tab` for collaboration callers; workspace dock passes `workspace-surface-tab`.
 */
export function handleTabListKeyDown<T extends string>(
  event: KeyboardEvent<HTMLElement>,
  tabs: readonly T[],
  current: T,
  onSelect: (tab: T) => void,
  tabIdPrefix = "collab-tab",
) {
  const target = event.target as HTMLElement | null;
  // Ignore keys aimed at non-tab controls nested inside the tablist (e.g. close / add).
  if (target && target.getAttribute("role") !== "tab") return;

  const next = nextTabFromKey(event.key, tabs, current);
  if (next == null) return;

  event.preventDefault();
  onSelect(next);
  const root =
    event.currentTarget.getAttribute("role") === "tablist"
      ? event.currentTarget
      : (event.currentTarget.closest?.('[role="tablist"]') ?? event.currentTarget);
  // Surface ids may contain `:` / `/` (preview:/file:); CSS.escape keeps querySelector valid.
  root.querySelector<HTMLElement>(`#${CSS.escape(`${tabIdPrefix}-${next}`)}`)?.focus();
}

/**
 * After closing a tab, focus a surviving tab or the open-view control.
 * When `closedIndex` is provided and the closed tab was active, pick the neighbor by index
 * (matches chatPanel.closeSurface). Otherwise fall back to the last remaining tab.
 */
export function focusTargetAfterTabClose<T extends string>(
  closed: T,
  remaining: readonly T[],
  activeBeforeClose: T | string,
  closedIndex?: number,
): { kind: "tab"; tab: T } | { kind: "open-view" } {
  if (remaining.length === 0) return { kind: "open-view" };
  if (activeBeforeClose === closed) {
    if (closedIndex != null && closedIndex >= 0) {
      return { kind: "tab", tab: remaining[Math.min(closedIndex, remaining.length - 1)] };
    }
    return { kind: "tab", tab: remaining[remaining.length - 1] };
  }
  if (remaining.includes(activeBeforeClose as T)) {
    return { kind: "tab", tab: activeBeforeClose as T };
  }
  return { kind: "tab", tab: remaining[remaining.length - 1] };
}
