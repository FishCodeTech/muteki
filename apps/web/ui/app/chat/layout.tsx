import type { ReactNode } from "react";

// The shell lives in RetainedWorkspaces inside WorkspaceFrame so it survives
// navigation; this layout only keeps the route mounted.
export default function ConversationLayout({ children }: { children: ReactNode }) {
  return children;
}
