export const RAIL_WIDTH_DEFAULT = 264;
export const RAIL_WIDTH_MIN = 220;
export const RAIL_WIDTH_MAX = 420;
export const RAIL_WIDTH_STORAGE_KEY = "muteki.threadRail.width";
/** Conversation shell left thread list — same clamp bounds as ThreadRail. */
export const CONVERSATION_SIDEBAR_WIDTH_STORAGE_KEY = "muteki.conversationSidebar.width";
/** Conversation shell left thread list drawer open/closed. Values: "0" | "1". */
export const CONVERSATION_SIDEBAR_COLLAPSED_STORAGE_KEY = "muteki.conversationSidebar.collapsed";

/** Conversation shell right workspace panel (browser / terminal / files / …). */
export const WORKSPACE_WIDTH_DEFAULT = 420;
export const WORKSPACE_WIDTH_MIN = 320;
export const WORKSPACE_WIDTH_MAX = 620;
export const CONVERSATION_WORKSPACE_WIDTH_STORAGE_KEY = "muteki.conversationWorkspace.width";

export function railWidthMax(viewportWidth?: number): number {
  if (!viewportWidth || viewportWidth <= 0) return RAIL_WIDTH_MAX;
  return Math.max(RAIL_WIDTH_MIN, Math.min(RAIL_WIDTH_MAX, Math.round(viewportWidth * 0.4)));
}

export function clampRailWidth(width: number, viewportWidth?: number): number {
  const next = Number.isFinite(width) ? width : RAIL_WIDTH_DEFAULT;
  return Math.round(Math.min(railWidthMax(viewportWidth), Math.max(RAIL_WIDTH_MIN, next)));
}

export function workspaceWidthMax(viewportWidth?: number): number {
  if (!viewportWidth || viewportWidth <= 0) return WORKSPACE_WIDTH_MAX;
  return Math.max(
    WORKSPACE_WIDTH_MIN,
    Math.min(WORKSPACE_WIDTH_MAX, Math.round(viewportWidth * 0.45)),
  );
}

export function clampWorkspaceWidth(width: number, viewportWidth?: number): number {
  const next = Number.isFinite(width) ? width : WORKSPACE_WIDTH_DEFAULT;
  return Math.round(
    Math.min(workspaceWidthMax(viewportWidth), Math.max(WORKSPACE_WIDTH_MIN, next)),
  );
}
