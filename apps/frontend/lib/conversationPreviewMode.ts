"use client";

import { createContext, useContext } from "react";

export const ConversationLocalPreviewContext = createContext(false);

export function useConversationLocalPreview(): boolean {
  return useContext(ConversationLocalPreviewContext);
}

export function isLocalPreviewPath(_pathname: string | null | undefined): boolean {
  return false;
}
