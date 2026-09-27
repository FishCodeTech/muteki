/**
 * Model picker listbox keyboard helpers (#196).
 * Keep focus on the search input; scroll the active option into view separately.
 * Redesign rows use `data-index` (see OptionList / ConversationModelPicker).
 */

export type ListNavKey = "ArrowUp" | "ArrowDown" | "Home" | "End";

export function isListNavKey(key: string): key is ListNavKey {
  return key === "ArrowUp" || key === "ArrowDown" || key === "Home" || key === "End";
}

/** True while CJK IME is composing — must not move/select list items. */
export function isImeComposingKeyEvent(event: {
  isComposing?: boolean;
  keyCode?: number;
  nativeEvent?: { isComposing?: boolean };
}): boolean {
  if (event.nativeEvent?.isComposing || event.isComposing) return true;
  // Some browsers still fire keyCode 229 during IME without isComposing on the React event.
  if (event.keyCode === 229) return true;
  return false;
}

export function clampListIndex(index: number, length: number): number {
  if (length <= 0) return -1;
  if (index < 0) return 0;
  if (index >= length) return length - 1;
  return index;
}

export function nextListIndex(current: number, key: ListNavKey, length: number): number {
  if (length <= 0) return -1;
  if (current < 0 || current >= length) {
    // No valid highlight yet — Down/Home → first, Up/End → last.
    if (key === "End" || key === "ArrowUp") return length - 1;
    return 0;
  }
  switch (key) {
    case "ArrowDown":
      return Math.min(length - 1, current + 1);
    case "ArrowUp":
      return Math.max(0, current - 1);
    case "Home":
      return 0;
    case "End":
      return length - 1;
  }
}

/** After search / endpoint switch: prefer selected row if still visible, else first. */
export function resolveActiveIndexAfterListChange(
  length: number,
  preferredIndex: number,
): number {
  if (length <= 0) return -1;
  if (preferredIndex >= 0 && preferredIndex < length) return preferredIndex;
  return 0;
}

/**
 * Scroll the active option into the nearest visible edge of its scrollport.
 * Returns true when an element was found and scrollIntoView was called.
 */
export function scrollActiveOptionIntoView(
  container: ParentNode | null | undefined,
  activeIndex: number,
  attr = "data-index",
): boolean {
  if (!container || activeIndex < 0) return false;
  const el = container.querySelector<HTMLElement>(`[${attr}="${activeIndex}"]`);
  if (!el) return false;
  el.scrollIntoView({ block: "nearest" });
  return true;
}

/** Alt+S toggles favorite on the aria-activedescendant row (#197). */
export function isFavoriteToggleKey(event: {
  key: string;
  code?: string;
  altKey?: boolean;
  ctrlKey?: boolean;
  metaKey?: boolean;
  shiftKey?: boolean;
}): boolean {
  if (!event.altKey || event.ctrlKey || event.metaKey) return false;
  // Shift+Alt+S is fine (same physical key); reject other chords.
  return event.code === "KeyS" || event.key === "s" || event.key === "S";
}

/** Polite live-region copy after a favorite toggle. */
export function favoriteToggleAnnouncement(
  modelLabel: string,
  nextFavorited: boolean,
): string {
  const label = modelLabel.trim() || "模型";
  return nextFavorited ? `已收藏 ${label}` : `已取消收藏 ${label}`;
}
