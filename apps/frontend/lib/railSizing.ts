export const RAIL_WIDTH_DEFAULT = 264;
export const RAIL_WIDTH_MIN = 220;
export const RAIL_WIDTH_MAX = 420;
export const RAIL_WIDTH_STORAGE_KEY = "muteki.threadRail.width";
export const CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY = "muteki.conversationSidebar.width";
export const CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY = "muteki.conversationSidebar.collapsed";

export function railWidthMax(viewportWidth?: number): number {
  if (!viewportWidth || viewportWidth <= 0) return RAIL_WIDTH_MAX;
  return Math.max(RAIL_WIDTH_MIN, Math.min(RAIL_WIDTH_MAX, Math.round(viewportWidth * 0.4)));
}

export function clampRailWidth(width: number, viewportWidth?: number): number {
  const next = Number.isFinite(width) ? width : RAIL_WIDTH_DEFAULT;
  return Math.round(Math.min(railWidthMax(viewportWidth), Math.max(RAIL_WIDTH_MIN, next)));
}
