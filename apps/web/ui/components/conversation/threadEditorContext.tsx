"use client";

import { createContext, useCallback, useContext, useLayoutEffect, useMemo, useRef, type ReactNode } from "react";
import type { ConversationView } from "@/lib/useConversation";
import { useOpenThreadInEditor } from "./ThreadGitActions";

export interface ThreadEditorOpener {
  /** Desktop build with a bound workspace: files can open in the local editor. */
  available: boolean;
  busy: boolean;
  /** Workspace-relative path; `line` is 1-based. */
  open: (relativePath?: string, line?: number) => Promise<void>;
}

const ThreadEditorContext = createContext<ThreadEditorOpener | null>(null);

/** Shares the active thread's "open in editor" action with deep views (diff rows, file previews). */
export function ThreadEditorProvider({ view, children }: { view: ConversationView | null; children: ReactNode }) {
  const editor = useOpenThreadInEditor(view);
  const available = editor.available && editor.desktop;
  const { busy } = editor;
  // `editor.open` is recreated on every render; a changing context value would
  // make React scan the whole conversation tree for consumers on each frame.
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
