"use client";

import { createContext, useContext } from "react";

export const ConversationLocalPreviewContext = createContext(false);

export function useConversationLocalPreview(): boolean {
  return useContext(ConversationLocalPreviewContext);
}

export function isLocalPreviewPath(pathname: string | null | undefined): boolean {
  return pathname === "/studio-prototype" || Boolean(pathname?.startsWith("/studio-prototype/"));
}
