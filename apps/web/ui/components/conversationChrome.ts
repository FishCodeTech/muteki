"use client";

import { createContext, useContext, type Dispatch, type SetStateAction } from "react";

export type ConversationChrome = {
  sidebarCollapsed: boolean;
  toggleSidebarCollapsed: () => void;
  sidebarWidth: number;
  setSidebarWidth: (width: number) => void;
  mobileSidebarOpen: boolean;
  setMobileSidebarOpen: Dispatch<SetStateAction<boolean>>;
};

export const ConversationChromeContext = createContext<ConversationChrome | null>(null);

export function useConversationChrome(): ConversationChrome {
  const value = useContext(ConversationChromeContext);
  if (!value) throw new Error("ConversationChrome 需要 WorkspaceFrame Provider");
  return value;
}
