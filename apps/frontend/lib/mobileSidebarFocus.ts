
export const MOBILE_SIDEBAR_FOCUSABLE = [
  "button:not([disabled])",
  "input:not([disabled])",
  "textarea:not([disabled])",
  "select:not([disabled])",
  "a[href]",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

export function nextFocusableIndex(
  length: number,
  current: number,
  shiftKey: boolean,
): number {
  if (length <= 0) return -1;
  if (shiftKey) return current <= 0 ? length - 1 : current - 1;
  return current < 0 || current >= length - 1 ? 0 : current + 1;
}

export function isTabCycleCandidate(el: HTMLElement): boolean {
  if (el.closest("[inert]")) return false;
  if (typeof el.checkVisibility === "function") {
    try {
      return el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true });
    } catch {
    }
  }
  return el.getClientRects().length > 0;
}

export function listMobileSidebarFocusables(root: ParentNode | null): HTMLElement[] {
  if (!root) return [];
  return Array.from(root.querySelectorAll<HTMLElement>(MOBILE_SIDEBAR_FOCUSABLE)).filter(
    isTabCycleCandidate,
  );
}

export function mobileSidebarTabRoot(sidebar: HTMLElement | null, active: HTMLElement | null): HTMLElement | null {
  const layer = active?.closest<HTMLElement>(".cx-portal [data-cx-layer]");
  return layer && isTabCycleCandidate(layer) ? layer : sidebar;
}
