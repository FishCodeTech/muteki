
export { isImeComposingKeyEvent } from "./imeKeyboard";

export function resolveActiveIndexAfterListChange(
  length: number,
  preferredIndex: number,
): number {
  if (length <= 0) return -1;
  if (preferredIndex >= 0 && preferredIndex < length) return preferredIndex;
  return 0;
}

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

export function isFavoriteToggleKey(event: {
  key: string;
  code?: string;
  altKey?: boolean;
  ctrlKey?: boolean;
  metaKey?: boolean;
  shiftKey?: boolean;
}): boolean {
  if (!event.altKey || event.ctrlKey || event.metaKey) return false;
  return event.code === "KeyS" || event.key === "s" || event.key === "S";
}

export function favoriteToggleAnnouncement(
  modelLabel: string,
  nextFavorited: boolean,
): string {
  const label = modelLabel.trim() || "模型";
  return nextFavorited ? `已收藏 ${label}` : `已取消收藏 ${label}`;
}
