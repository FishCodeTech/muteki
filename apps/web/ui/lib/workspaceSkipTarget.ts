/**
 * Skip-to-main-content target routing (#213).
 *
 * /chat nests ConversationSidebar inside WorkspaceFrame's content wrapper, so
 * the outer #main-content landmark still includes the sidebar. Route the skip
 * link to the conversation <main> instead; other workspaces keep the frame id.
 */

export const WORKSPACE_MAIN_CONTENT_ID = "main-content";
export const CONVERSATION_MAIN_CONTENT_ID = "conversation-main-content";

/** Fragment id the WorkspaceFrame skip link should target for this pathname. */
export function workspaceSkipTargetId(pathname: string): string {
  return pathname.startsWith("/chat")
    ? CONVERSATION_MAIN_CONTENT_ID
    : WORKSPACE_MAIN_CONTENT_ID;
}

/**
 * Id for the outer workspace-frame-content wrapper.
 * Undefined on /chat so the conversation <main> owns the unique skip target.
 */
export function workspaceFrameContentId(pathname: string): string | undefined {
  return pathname.startsWith("/chat") ? undefined : WORKSPACE_MAIN_CONTENT_ID;
}

/** Assert a document only exposes one of each landmark id (no duplicates). */
export function assertUniqueLandmarkIds(ids: Array<string | null | undefined>): void {
  const seen = new Map<string, number>();
  for (const id of ids) {
    if (!id) continue;
    seen.set(id, (seen.get(id) || 0) + 1);
  }
  for (const [id, count] of seen) {
    if (count > 1) {
      throw new Error(`duplicate landmark id "${id}" (count=${count})`);
    }
  }
}
