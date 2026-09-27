"use client";

import { createContext, useContext } from "react";

/**
 * Card-local actions that must not live on AgentNodeData. Node objects are
 * reused by signature; a callback on the data object would go stale the
 * moment the signature matched a previous commit.
 */
export type CollaborationActions = {
  toggleExpand: (agentId: string) => void;
  showAllWorkItems: (agentId: string) => void;
  selectPreviewKnowledge: (knowledgeId: string) => void;
};

const CollaborationActionsContext = createContext<CollaborationActions>({
  toggleExpand() {},
  showAllWorkItems() {},
  selectPreviewKnowledge() {},
});

export const CollaborationActionsProvider = CollaborationActionsContext.Provider;

export function useCollaborationActions(): CollaborationActions {
  return useContext(CollaborationActionsContext);
}
