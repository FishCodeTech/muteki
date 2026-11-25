
export const WORKSPACE_MAIN_CONTENT_ID = "main-content";
export const CONVERSATION_MAIN_CONTENT_ID = "conversation-main-content";
export const CHAT_MAIN_CONTENT_ID = "chat-main-content";
export const SRC_MAIN_CONTENT_ID = "src-main-content";

export function isSrcConversationPath(pathname: string): boolean {
  return pathname === "/src" || pathname.startsWith("/src/tasks/");
}

export function isConversationLayoutPath(pathname: string): boolean {
  if (pathname === "/chat" || pathname.startsWith("/chat/")) return true;
  return isSrcConversationPath(pathname);
}

export function workspaceSkipTargetId(pathname: string): string {
  if (pathname === "/chat" || pathname.startsWith("/chat/")) return CHAT_MAIN_CONTENT_ID;
  if (isSrcConversationPath(pathname)) return SRC_MAIN_CONTENT_ID;
  return isConversationLayoutPath(pathname)
    ? CONVERSATION_MAIN_CONTENT_ID
    : WORKSPACE_MAIN_CONTENT_ID;
}

export function workspaceFrameContentId(pathname: string): string | undefined {
  return isConversationLayoutPath(pathname) ? undefined : WORKSPACE_MAIN_CONTENT_ID;
}
