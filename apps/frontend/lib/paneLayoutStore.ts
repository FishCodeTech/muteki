"use client";

import { useSyncExternalStore } from "react";

export const MAX_PANES = 6;

export type PaneAxis = "horizontal" | "vertical";

export type PaneLayoutPreset =
  | "single"
  | "columns"
  | "one_beside_two"
  | "grid"
  | "spread_agents";

export type PaneLeaf = {
  kind: "leaf";
  id: string;
  threadId: string;
};

export type PaneSplit = {
  kind: "split";
  id: string;
  axis: PaneAxis;
  ratio: number;
  first: PaneNode;
  second: PaneNode;
};

export type PaneNode = PaneLeaf | PaneSplit;

export interface PaneLayoutState {
  root: PaneNode;
  activePaneId: string;
  zoomedPaneId: string | null;
}

let seq = 1;
function nextId(prefix: string): string {
  seq += 1;
  return `${prefix}-${seq}`;
}

const PRIMARY_PANE_ID = "pane-primary";

function createInitialState(): PaneLayoutState {
  return {
    root: { kind: "leaf", id: PRIMARY_PANE_ID, threadId: "" },
    activePaneId: PRIMARY_PANE_ID,
    zoomedPaneId: null,
  };
}

let state: PaneLayoutState = createInitialState();
const listeners = new Set<() => void>();

function emit(next: PaneLayoutState) {
  state = next;
  for (const listener of listeners) listener();
}

export function listPaneLeaves(node: PaneNode): PaneLeaf[] {
  if (node.kind === "leaf") return [node];
  return [...listPaneLeaves(node.first), ...listPaneLeaves(node.second)];
}

export function countPanes(node: PaneNode): number {
  if (node.kind === "leaf") return 1;
  return countPanes(node.first) + countPanes(node.second);
}

export function findPaneLeaf(node: PaneNode, paneId: string): PaneLeaf | null {
  if (node.kind === "leaf") return node.id === paneId ? node : null;
  return findPaneLeaf(node.first, paneId) || findPaneLeaf(node.second, paneId);
}

function mapLeaves(node: PaneNode, fn: (leaf: PaneLeaf) => PaneLeaf): PaneNode {
  if (node.kind === "leaf") return fn(node);
  return {
    ...node,
    first: mapLeaves(node.first, fn),
    second: mapLeaves(node.second, fn),
  };
}

function splitLeafNode(
  node: PaneNode,
  targetPaneId: string,
  axis: PaneAxis,
  newLeaf: PaneLeaf,
): PaneNode {
  if (node.kind === "leaf") {
    if (node.id !== targetPaneId) return node;
    return {
      kind: "split",
      id: nextId("split"),
      axis,
      ratio: 0.5,
      first: node,
      second: newLeaf,
    };
  }
  return {
    ...node,
    first: splitLeafNode(node.first, targetPaneId, axis, newLeaf),
    second: splitLeafNode(node.second, targetPaneId, axis, newLeaf),
  };
}

function removeLeafNode(node: PaneNode, targetPaneId: string): PaneNode | null {
  if (node.kind === "leaf") {
    return node.id === targetPaneId ? null : node;
  }
  const first = removeLeafNode(node.first, targetPaneId);
  const second = removeLeafNode(node.second, targetPaneId);
  if (!first && !second) return null;
  if (!first) return second;
  if (!second) return first;
  return { ...node, first, second };
}

function updateSplitRatioNode(node: PaneNode, splitId: string, ratio: number): PaneNode {
  if (node.kind === "leaf") return node;
  const clamped = Math.min(0.85, Math.max(0.15, ratio));
  if (node.id === splitId) {
    return { ...node, ratio: clamped };
  }
  return {
    ...node,
    first: updateSplitRatioNode(node.first, splitId, ratio),
    second: updateSplitRatioNode(node.second, splitId, ratio),
  };
}

export const paneLayoutStore = {
  getSnapshot(): PaneLayoutState {
    return state;
  },

  subscribe(listener: () => void): () => void {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  },

  isPrimaryPane(paneId: string): boolean {
    return paneId === PRIMARY_PANE_ID;
  },

  setPrimaryThread(threadId: string) {
    const current = findPaneLeaf(state.root, PRIMARY_PANE_ID);
    if (current && current.threadId === threadId) return;
    emit({
      ...state,
      root: mapLeaves(state.root, (leaf) =>
        leaf.id === PRIMARY_PANE_ID ? { ...leaf, threadId } : leaf,
      ),
    });
  },

  setActivePane(paneId: string) {
    if (state.activePaneId === paneId) return;
    if (!findPaneLeaf(state.root, paneId)) return;
    emit({ ...state, activePaneId: paneId });
  },

  setPaneThread(paneId: string, threadId: string) {
    const target = findPaneLeaf(state.root, paneId);
    if (!target || target.threadId === threadId) return;
    emit({
      ...state,
      root: mapLeaves(state.root, (leaf) =>
        leaf.id === paneId ? { ...leaf, threadId } : leaf,
      ),
    });
  },

  splitPane(paneId: string, axis: PaneAxis, newThreadId = ""): string | null {
    if (countPanes(state.root) >= MAX_PANES) return null;
    const targetId = findPaneLeaf(state.root, paneId) ? paneId : state.activePaneId;
    const newLeaf: PaneLeaf = {
      kind: "leaf",
      id: nextId("pane"),
      threadId: newThreadId,
    };
    emit({
      root: splitLeafNode(state.root, targetId, axis, newLeaf),
      activePaneId: newLeaf.id,
      zoomedPaneId: null,
    });
    return newLeaf.id;
  },

  closePane(paneId: string) {
    if (countPanes(state.root) <= 1) return;
    const nextRoot = removeLeafNode(state.root, paneId);
    if (!nextRoot) return;
    const leaves = listPaneLeaves(nextRoot);
    const nextActive = findPaneLeaf(nextRoot, state.activePaneId)
      ? state.activePaneId
      : leaves[0]?.id || PRIMARY_PANE_ID;
    const nextZoomed = state.zoomedPaneId === paneId ? null : state.zoomedPaneId;
    emit({
      root: nextRoot,
      activePaneId: nextActive,
      zoomedPaneId: leaves.length <= 1 ? null : nextZoomed,
    });
  },

  closeOtherPanes(keepPaneId: string) {
    const kept = findPaneLeaf(state.root, keepPaneId);
    const threadId = kept?.threadId ?? "";
    emit({
      root: { kind: "leaf", id: PRIMARY_PANE_ID, threadId },
      activePaneId: PRIMARY_PANE_ID,
      zoomedPaneId: null,
    });
  },

  toggleZoomPane(paneId?: string) {
    if (countPanes(state.root) <= 1) return;
    const target = paneId && findPaneLeaf(state.root, paneId) ? paneId : state.activePaneId;
    emit({
      ...state,
      zoomedPaneId: state.zoomedPaneId === target ? null : target,
    });
  },

  unzoom() {
    if (!state.zoomedPaneId) return;
    emit({ ...state, zoomedPaneId: null });
  },

  focusDirection(dir: "prev" | "next") {
    const leaves = listPaneLeaves(state.root);
    if (leaves.length <= 1) return;
    const idx = leaves.findIndex((leaf) => leaf.id === state.activePaneId);
    const current = idx >= 0 ? idx : 0;
    const nextIdx = dir === "next"
      ? (current + 1) % leaves.length
      : (current - 1 + leaves.length) % leaves.length;
    emit({ ...state, activePaneId: leaves[nextIdx].id });
  },

  setSplitRatio(splitId: string, ratio: number) {
    emit({
      ...state,
      root: updateSplitRatioNode(state.root, splitId, ratio),
    });
  },

  applyPreset(
    preset: PaneLayoutPreset,
    currentThreadId: string,
    delegatedThreadIds: string[] = [],
  ) {
    const primary: PaneLeaf = {
      kind: "leaf",
      id: PRIMARY_PANE_ID,
      threadId: currentThreadId,
    };
    const existingExtras = listPaneLeaves(state.root)
      .filter((leaf) => leaf.id !== PRIMARY_PANE_ID && leaf.threadId)
      .map((leaf) => leaf.threadId);
    const takeExtra = (index: number) => existingExtras[index] || "";

    let root: PaneNode = primary;
    if (preset === "single") {
      root = primary;
    } else if (preset === "columns") {
      root = {
        kind: "split",
        id: nextId("split"),
        axis: "horizontal",
        ratio: 0.5,
        first: primary,
        second: { kind: "leaf", id: nextId("pane"), threadId: takeExtra(0) },
      };
    } else if (preset === "one_beside_two") {
      root = {
        kind: "split",
        id: nextId("split"),
        axis: "horizontal",
        ratio: 0.5,
        first: primary,
        second: {
          kind: "split",
          id: nextId("split"),
          axis: "vertical",
          ratio: 0.5,
          first: { kind: "leaf", id: nextId("pane"), threadId: takeExtra(0) },
          second: { kind: "leaf", id: nextId("pane"), threadId: takeExtra(1) },
        },
      };
    } else if (preset === "grid") {
      root = {
        kind: "split",
        id: nextId("split"),
        axis: "horizontal",
        ratio: 0.5,
        first: {
          kind: "split",
          id: nextId("split"),
          axis: "vertical",
          ratio: 0.5,
          first: primary,
          second: { kind: "leaf", id: nextId("pane"), threadId: takeExtra(0) },
        },
        second: {
          kind: "split",
          id: nextId("split"),
          axis: "vertical",
          ratio: 0.5,
          first: { kind: "leaf", id: nextId("pane"), threadId: takeExtra(1) },
          second: { kind: "leaf", id: nextId("pane"), threadId: takeExtra(2) },
        },
      };
    } else if (preset === "spread_agents") {
      const agents = delegatedThreadIds.slice(0, MAX_PANES - 1);
      if (agents.length === 0) {
        root = {
          kind: "split",
          id: nextId("split"),
          axis: "horizontal",
          ratio: 0.5,
          first: primary,
          second: { kind: "leaf", id: nextId("pane"), threadId: "" },
        };
      } else {
        let rightNode: PaneNode = {
          kind: "leaf",
          id: nextId("pane"),
          threadId: agents[agents.length - 1],
        };
        for (let i = agents.length - 2; i >= 0; i -= 1) {
          rightNode = {
            kind: "split",
            id: nextId("split"),
            axis: "vertical",
            ratio: 1 / (agents.length - i),
            first: { kind: "leaf", id: nextId("pane"), threadId: agents[i] },
            second: rightNode,
          };
        }
        root = {
          kind: "split",
          id: nextId("split"),
          axis: "horizontal",
          ratio: 0.5,
          first: primary,
          second: rightNode,
        };
      }
    }

    emit({
      root,
      activePaneId: PRIMARY_PANE_ID,
      zoomedPaneId: null,
    });
  },
};

export function usePaneLayout(): PaneLayoutState {
  return useSyncExternalStore(
    paneLayoutStore.subscribe,
    paneLayoutStore.getSnapshot,
    paneLayoutStore.getSnapshot,
  );
}
