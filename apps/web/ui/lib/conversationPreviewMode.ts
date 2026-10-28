"use client";

import { createContext, useContext } from "react";

/**
 * True below a ConversationShell that renders local preview data. Thread ids
 * there are not server ids, so thread-scoped reads must stay local.
 */
export const ConversationLocalPreviewContext = createContext(false);

export function useConversationLocalPreview(): boolean {
  return useContext(ConversationLocalPreviewContext);
}

/**
 * Routes that render only local demo data. The shared workspace frame skips
 * its service reads (overview, notification inbox, palette body search) here
 * so nothing from the real workspace is requested or shown.
 */
export function isLocalPreviewPath(pathname: string | null | undefined): boolean {
  return pathname === "/studio-prototype" || Boolean(pathname?.startsWith("/studio-prototype/"));
}
