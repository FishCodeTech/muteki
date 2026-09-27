"use client";

import { useReducer, type Dispatch } from "react";

export type InspectorTab = "overview" | "knowledge" | "activity" | "decisions" | "relations";
export const INSPECTOR_TABS: InspectorTab[] = ["overview", "knowledge", "activity", "relations"];
export const COORDINATOR_INSPECTOR_TABS: InspectorTab[] = ["overview", "knowledge", "activity", "decisions", "relations"];

/** What the right-hand inspector is showing. Panel open state lives in useCollaborationLayout. */
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
      // Keep the operator's tab; drop knowledge / relation focus so the new agent
      // shows its own list (empty states stay on that tab).
      return { ...state, agentId: action.agentId, knowledgeId: null, relationId: null };
    case "selectKnowledge":
      // Knowledge opens under its producing agent on the knowledge tab.
      return { agentId: action.agentId, knowledgeId: action.knowledgeId, relationId: null, tab: "knowledge" };
    case "selectRelation":
      // Relation focus is independent of the agent: "back" returns to whoever
      // was already selected. The relation inspector never reads `tab`.
      return { ...state, knowledgeId: null, relationId: action.relationId };
    case "clearRelation":
      return state.relationId === null ? state : { ...state, relationId: null };
    case "clear":
      // Pane click / second click on the selected card: no agent, tab untouched.
      if (state.agentId === null && state.knowledgeId === null && state.relationId === null) return state;
      return { ...state, agentId: null, knowledgeId: null, relationId: null };
    case "clearKnowledge":
      return state.knowledgeId === null ? state : { ...state, knowledgeId: null };
    case "setTab": {
      // Leaving the knowledge tab drops the pinned detail so it cannot overlay overview / activity.
      const knowledgeId = action.tab === "knowledge" ? state.knowledgeId : null;
      if (state.tab === action.tab && state.knowledgeId === knowledgeId) return state;
      return { ...state, tab: action.tab, knowledgeId };
    }
    case "dropUnknown":
      // Filters may hide a card; keep the selection so the inspector can still
      // show it. Drop only once the agent has left the model.
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
