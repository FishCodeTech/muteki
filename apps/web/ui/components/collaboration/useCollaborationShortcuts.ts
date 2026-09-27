"use client";

import { useCallback, type KeyboardEvent, type RefObject } from "react";

import { type CollaborationAgent } from "@/lib/agentCollaboration";
import type { CollaborationPosition } from "@/lib/agentCollaborationLayout";

import {
  type CollaborationSelection,
  type CollaborationSelectionAction,
  type InspectorTab,
} from "./useCollaborationSelection";

type NavDir = "up" | "down" | "left" | "right";

const KEY_DIR: Record<string, NavDir> = {
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowLeft: "left",
  ArrowRight: "right",
  k: "up",
  j: "down",
};

function isEditingField(target: EventTarget | null): boolean {
  const node = target as HTMLElement | null;
  if (!node || typeof node.tagName !== "string") return false;
  const tag = node.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || node.isContentEditable;
}

function ownsArrows(target: EventTarget | null): boolean {
  const node = target as Element | null;
  return !!node?.closest?.("[role='separator'], [role='radiogroup'], [role='tablist']");
}

/** Closest visible agent in one compass direction, using layout coordinates. */
export function nearestAgentId(
  fromId: string,
  dir: NavDir,
  agents: CollaborationAgent[],
  positions: Record<string, CollaborationPosition>,
): string | undefined {
  const from = positions[fromId];
  if (!from) return undefined;
  let bestId: string | undefined;
  let bestScore = Infinity;
  for (const agent of agents) {
    if (agent.id === fromId) continue;
    const point = positions[agent.id];
    if (!point) continue;
    const dx = point.x - from.x;
    const dy = point.y - from.y;
    if (dir === "left" && dx >= 0) continue;
    if (dir === "right" && dx <= 0) continue;
    if (dir === "up" && dy >= 0) continue;
    if (dir === "down" && dy <= 0) continue;
    const primary = dir === "left" || dir === "right" ? Math.abs(dx) : Math.abs(dy);
    const secondary = dir === "left" || dir === "right" ? Math.abs(dy) : Math.abs(dx);
    const score = primary + secondary * 0.25;
    if (score < bestScore) {
      bestScore = score;
      bestId = agent.id;
    }
  }
  return bestId;
}

export function useCollaborationShortcuts({
  contextMenu,
  onCloseContextMenu,
  selection,
  dispatch,
  query,
  setQuery,
  visibleAgents,
  positions,
  selectAgent,
  openDetail,
  focusInspectorHead,
  canvasRef,
  shellRef,
  inspectorTabs,
}: {
  contextMenu: unknown;
  onCloseContextMenu: () => void;
  selection: CollaborationSelection;
  dispatch: (action: CollaborationSelectionAction) => void;
  query: string;
  setQuery: (value: string) => void;
  visibleAgents: CollaborationAgent[];
  positions: Record<string, CollaborationPosition>;
  selectAgent: (id: string, center?: boolean) => void;
  openDetail: () => void;
  focusInspectorHead: () => void;
  canvasRef: RefObject<HTMLElement | null>;
  shellRef: RefObject<HTMLElement | null>;
  inspectorTabs: InspectorTab[];
}) {
  return useCallback((event: KeyboardEvent<HTMLDivElement>) => {
    // HeroUI menus render in a portal: their keys belong to the menu, not the graph.
    if (event.target instanceof Node && !event.currentTarget.contains(event.target)) return;
    if (event.key === "Escape") {
      if (contextMenu) {
        onCloseContextMenu();
        event.preventDefault();
        event.stopPropagation();
        return;
      }
      if (selection.relationId) dispatch({ type: "clearRelation" });
      else if (selection.knowledgeId && selection.agentId) dispatch({ type: "selectAgent", agentId: selection.agentId });
      else if (selection.knowledgeId) dispatch({ type: "clearKnowledge" });
      else if (query) setQuery("");
      else if (selection.agentId !== null) dispatch({ type: "clear" });
      else return;
      event.preventDefault();
      event.stopPropagation();
      return;
    }

    if (event.ctrlKey || event.metaKey || event.altKey || event.nativeEvent.isComposing) return;
    const editing = isEditingField(event.target);

    if (event.key === "/" && !event.shiftKey && !editing) {
      event.preventDefault();
      event.stopPropagation();
      shellRef.current?.querySelector<HTMLInputElement>(".collab-toolbar-search input")?.focus();
      return;
    }

    if (editing) return;

    const dirKey = event.key.length === 1 ? event.key.toLowerCase() : event.key;
    const dir = event.shiftKey ? undefined : KEY_DIR[dirKey];
    if (dir && !ownsArrows(event.target)) {
      const fromId = selection.agentId && visibleAgents.some((agent) => agent.id === selection.agentId)
        ? selection.agentId
        : visibleAgents[0]?.id;
      const nextId = fromId
        ? (nearestAgentId(fromId, dir, visibleAgents, positions) ?? (selection.agentId ? undefined : fromId))
        : undefined;
      if (!nextId) return;
      event.preventDefault();
      event.stopPropagation();
      selectAgent(nextId, true);
      requestAnimationFrame(() => {
        canvasRef.current
          ?.querySelector<HTMLElement>(`.react-flow__node[data-id="${CSS.escape(nextId)}"]`)
          ?.focus();
      });
      return;
    }

    if ((event.key === "[" || event.key === "]") && !event.shiftKey) {
      const current = inspectorTabs.includes(selection.tab) ? selection.tab : inspectorTabs[0];
      const index = inspectorTabs.indexOf(current);
      const step = event.key === "]" ? 1 : -1;
      const next = inspectorTabs[(index + step + inspectorTabs.length) % inspectorTabs.length];
      event.preventDefault();
      event.stopPropagation();
      if (selection.relationId) dispatch({ type: "clearRelation" });
      dispatch({ type: "setTab", tab: next });
      openDetail();
      return;
    }

    if (event.key !== "Enter" || event.shiftKey) return;
    const target = event.target as Element | null;
    const control = target?.closest?.("button, a, input, textarea, select");
    if (control && !control.classList.contains("collab-node-tip-trigger")) return;
    const node = target?.closest?.(".react-flow__node");
    const edge = target?.closest?.(".react-flow__edge");
    if (!node && !edge) return;
    if (node?.classList.contains("react-flow__node-group")) return;
    event.preventDefault();
    event.stopPropagation();
    const id = (node ?? edge)?.getAttribute("data-id");
    if (node && id) dispatch({ type: "selectAgent", agentId: id });
    else if (edge && id) dispatch({ type: "selectRelation", relationId: id });
    openDetail();
    focusInspectorHead();
  }, [
    canvasRef,
    contextMenu,
    dispatch,
    focusInspectorHead,
    inspectorTabs,
    onCloseContextMenu,
    positions,
    query,
    openDetail,
    selectAgent,
    selection.agentId,
    selection.knowledgeId,
    selection.relationId,
    selection.tab,
    setQuery,
    shellRef,
    visibleAgents,
  ]);
}
