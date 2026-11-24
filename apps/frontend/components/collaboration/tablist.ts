import type { KeyboardEvent } from "react";

export function isTabId<T extends string>(tabs: readonly T[], value: unknown): value is T {
  return typeof value === "string" && tabs.some((tab) => tab === value);
}

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

export function handleTabListKeyDown<T extends string>(
  event: KeyboardEvent<HTMLElement>,
  tabs: readonly T[],
  current: T,
  onSelect: (tab: T) => void,
  tabIdPrefix = "collab-tab",
) {
  const target = event.target as HTMLElement | null;
  if (target && target.getAttribute("role") !== "tab") return;

  const next = nextTabFromKey(event.key, tabs, current);
  if (next == null) return;

  event.preventDefault();
  onSelect(next);
  const root =
    event.currentTarget.getAttribute("role") === "tablist"
      ? event.currentTarget
      : (event.currentTarget.closest?.('[role="tablist"]') ?? event.currentTarget);
  root.querySelector<HTMLElement>(`#${CSS.escape(`${tabIdPrefix}-${next}`)}`)?.focus();
}

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
