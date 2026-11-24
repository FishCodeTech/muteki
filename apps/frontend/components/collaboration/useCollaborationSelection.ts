"use client";

import { useReducer, type Dispatch } from "react";

export type InspectorTab = "overview" | "knowledge" | "activity" | "decisions" | "relations";
export const INSPECTOR_TABS: InspectorTab[] = ["overview", "knowledge", "activity", "relations"];
export const COORDINATOR_INSPECTOR_TABS: InspectorTab[] = ["overview", "knowledge", "activity", "decisions", "relations"];

export type CollaborationSelection = {
  agentId: string | null;
  knowledgeId: string | null;
  relationId: string | null;
  tab: InspectorTab;
};

export type CollaborationSelectionAction =
  | { type: "selectAgent"; agentId: string }
  | { type: "selectKnowledge"; agentId: string; knowledgeId: string }
  | { type: "selectRelation"; relationId: string }
  | { type: "clearRelation" }
  | { type: "clear" }
  | { type: "clearKnowledge" }
  | { type: "setTab"; tab: InspectorTab }
  | { type: "dropUnknown"; known: boolean }
  | { type: "hydrate"; state: CollaborationSelection };

export const INITIAL_SELECTION: CollaborationSelection = {
  agentId: null,
  knowledgeId: null,
  relationId: null,
  tab: "overview",
};

export function collaborationSelectionReducer(
  state: CollaborationSelection,
  action: CollaborationSelectionAction,
): CollaborationSelection {
  switch (action.type) {
    case "selectAgent":
      return { ...state, agentId: action.agentId, knowledgeId: null, relationId: null };
    case "selectKnowledge":
      return { agentId: action.agentId, knowledgeId: action.knowledgeId, relationId: null, tab: "knowledge" };
    case "selectRelation":
      return { ...state, knowledgeId: null, relationId: action.relationId };
    case "clearRelation":
      return state.relationId === null ? state : { ...state, relationId: null };
    case "clear":
      if (state.agentId === null && state.knowledgeId === null && state.relationId === null) return state;
      return { ...state, agentId: null, knowledgeId: null, relationId: null };
    case "clearKnowledge":
      return state.knowledgeId === null ? state : { ...state, knowledgeId: null };
    case "setTab": {
      const knowledgeId = action.tab === "knowledge" ? state.knowledgeId : null;
      if (state.tab === action.tab && state.knowledgeId === knowledgeId) return state;
      return { ...state, tab: action.tab, knowledgeId };
    }
    case "dropUnknown":
      if (action.known) return state;
      return { ...state, agentId: null, knowledgeId: null, relationId: null };
    case "hydrate":
      if (
        state.agentId === action.state.agentId
        && state.knowledgeId === action.state.knowledgeId
        && state.relationId === action.state.relationId
        && state.tab === action.state.tab
      ) return state;
      return action.state;
    default: {
      const _exhaustive: never = action;
      return _exhaustive;
    }
  }
}

export function useCollaborationSelection(
  init?: () => CollaborationSelection,
): [CollaborationSelection, Dispatch<CollaborationSelectionAction>] {
  return useReducer(
    collaborationSelectionReducer,
    INITIAL_SELECTION,
    (fallback) => (init ? init() : fallback),
  );
}
