"use client";

import { createContext, useCallback, useContext, useLayoutEffect, useMemo, useRef, type ReactNode } from "react";
import type { ConversationView } from "@/lib/useConversation";
import { useOpenThreadInEditor } from "./ThreadGitActions";

export interface ThreadEditorOpener {
  available: boolean;
  busy: boolean;
  open: (relativePath?: string, line?: number) => Promise<void>;
}

const ThreadEditorContext = createContext<ThreadEditorOpener | null>(null);

export function ThreadEditorProvider({ view, children }: { view: ConversationView | null; children: ReactNode }) {
  const editor = useOpenThreadInEditor(view);
  const available = editor.available && editor.desktop;
  const { busy } = editor;
  const openRef = useRef(editor.open);
  useLayoutEffect(() => {
    openRef.current = editor.open;
  });
  const open = useCallback<ThreadEditorOpener["open"]>((relativePath, line) => openRef.current(relativePath, line), []);
  const value = useMemo<ThreadEditorOpener>(() => ({ available, busy, open }), [available, busy, open]);
  return <ThreadEditorContext.Provider value={value}>{children}</ThreadEditorContext.Provider>;
}

export function useThreadEditor(): ThreadEditorOpener | null {
  const value = useContext(ThreadEditorContext);
  return value?.available ? value : null;
}
