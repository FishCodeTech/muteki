"use client";

import { createContext, useContext } from "react";

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
