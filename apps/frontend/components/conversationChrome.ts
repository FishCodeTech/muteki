"use client";

import { createContext, useContext, type Dispatch, type SetStateAction } from "react";

export type ConversationChrome = {
  sidebarCollapsed: boolean;
  setSidebarCollapsed: (collapsed: boolean) => void;
  toggleSidebarCollapsed: () => void;
  sidebarWidth: number;
  setSidebarWidth: (width: number) => void;
  compactScreen: boolean;
  mobileSidebarOpen: boolean;
  setMobileSidebarOpen: Dispatch<SetStateAction<boolean>>;
  headerSlot?: HTMLElement | null;
  previewSidebarWidth?: (width: number | null) => void;
};

export const ConversationChromeContext = createContext<ConversationChrome | null>(null);

export function useConversationChrome(): ConversationChrome {
  const value = useContext(ConversationChromeContext);
  if (!value) throw new Error("ConversationChrome 需要 WorkspaceFrame Provider");
  return value;
}
