/** Filter / selection persistence for the collaboration canvas (P1-22). */

import { type CollaborationRelationKind } from "./agentCollaboration";
import type { CollabCanvasLayout, CollaborationPosition } from "./agentCollaborationLayout";

export const COLLAB_PREFS_KEY = "muteki.collab.prefs";
export const COLLAB_LAYOUT_KEY = "muteki.collab.layout";

export function collabSessionKey(runId: string): string {
  return `muteki.collab.sel.${runId}`;
}

export type CollabInspectorTab = "overview" | "knowledge" | "activity" | "decisions" | "relations";

export type CollabPrefs = {
  relationKinds?: CollaborationRelationKind[];
  onlyAnomalies: boolean;
  recentOnly: boolean;
  indexOpen?: boolean;
  detailOpen?: boolean;
};

export type CollabSession = {
  selectedAgentId: string;
  selectedKnowledgeId: string | null;
  selectedRelationId: string | null;
  inspectorTab: CollabInspectorTab;
  query: string;
};

export type CollabViewport = { x: number; y: number; zoom: number };

export type CollabRunMemory = {
  layoutVersion: number;
  positions: Record<string, CollaborationPosition>;
  viewport?: CollabViewport;
  fitted: boolean;
};

export type CollabViewLoad = {
  prefs: CollabPrefs;
  session: CollabSession;
  memory: CollabRunMemory;
  fromMemory: boolean;
};

type RunCache = { session: CollabSession; memory: CollabRunMemory };

const runCache = new Map<string, RunCache>();

const DEFAULT_PREFS: CollabPrefs = { onlyAnomalies: false, recentOnly: false };
const DEFAULT_SESSION: CollabSession = {
  selectedAgentId: "",
  selectedKnowledgeId: null,
  selectedRelationId: null,
  inspectorTab: "overview",
  query: "",
};

function emptyMemory(): CollabRunMemory {
  return { layoutVersion: 0, positions: {}, fitted: false };
}

function readJson(storage: Storage, key: string): unknown {
  try {
    const raw = storage.getItem(key);
    return raw ? JSON.parse(raw) : undefined;
  } catch {
    return undefined;
  }
}

function writeJson(storage: Storage, key: string, value: unknown): void {
  try {
    storage.setItem(key, JSON.stringify(value));
  } catch { /* storage unavailable */ }
}

function isRelationKind(value: unknown): value is CollaborationRelationKind {
  switch (value) {
    case "dispatch":
    case "model":
    case "handoff":
    case "review":
    case "verify":
    case "report":
    case "directive":
    case "lock":
      return true;
    default:
      return false;
  }
}

function asTab(value: unknown): CollabInspectorTab {
  switch (value) {
    case "knowledge":
    case "activity":
    case "decisions":
    case "relations":
      return value;
    default:
      return "overview";
  }
}

function readPrefs(): CollabPrefs {
  if (typeof window === "undefined") return DEFAULT_PREFS;
  const raw = readJson(window.localStorage, COLLAB_PREFS_KEY);
  if (!raw || typeof raw !== "object") return DEFAULT_PREFS;
  const o = raw as Record<string, unknown>;
  return {
    relationKinds: Array.isArray(o.relationKinds) ? o.relationKinds.filter(isRelationKind) : undefined,
    onlyAnomalies: o.onlyAnomalies === true,
    recentOnly: o.recentOnly === true,
    indexOpen: typeof o.indexOpen === "boolean" ? o.indexOpen : undefined,
    detailOpen: typeof o.detailOpen === "boolean" ? o.detailOpen : undefined,
  };
}

function readSession(runId: string): CollabSession | undefined {
  if (typeof window === "undefined" || !runId) return undefined;
  const raw = readJson(window.sessionStorage, collabSessionKey(runId));
  if (!raw || typeof raw !== "object") return undefined;
  const o = raw as Record<string, unknown>;
  if (typeof o.selectedAgentId !== "string") return undefined;
  return {
    selectedAgentId: o.selectedAgentId,
    selectedKnowledgeId: typeof o.selectedKnowledgeId === "string" ? o.selectedKnowledgeId : null,
    selectedRelationId: typeof o.selectedRelationId === "string" ? o.selectedRelationId : null,
    inspectorTab: asTab(o.inspectorTab),
    query: typeof o.query === "string" ? o.query : "",
  };
}

export function loadCollabView(runId: string): CollabViewLoad {
  const prefs = readPrefs();
  const cached = runCache.get(runId);
  if (cached) return { prefs, session: cached.session, memory: cached.memory, fromMemory: true };
  return { prefs, session: readSession(runId) ?? DEFAULT_SESSION, memory: emptyMemory(), fromMemory: false };
}

export function writeCollabPrefs(prefs: CollabPrefs): void {
  if (typeof window === "undefined") return;
  writeJson(window.localStorage, COLLAB_PREFS_KEY, prefs);
}

export function readCollabLayout(): CollabCanvasLayout {
  if (typeof window === "undefined") return "hierarchy";
  return window.localStorage.getItem(COLLAB_LAYOUT_KEY) === "timeline" ? "timeline" : "hierarchy";
}

export function writeCollabLayout(layout: CollabCanvasLayout): void {
  if (typeof window === "undefined") return;
  window.localStorage.setItem(COLLAB_LAYOUT_KEY, layout);
}

export function writeCollabSession(runId: string, session: CollabSession): void {
  if (!runId) return;
  const current = runCache.get(runId);
  runCache.set(runId, { session, memory: current?.memory ?? emptyMemory() });
  if (typeof window === "undefined") return;
  writeJson(window.sessionStorage, collabSessionKey(runId), session);
}

export function patchCollabMemory(runId: string, patch: Partial<CollabRunMemory>): void {
  if (!runId) return;
  const current = runCache.get(runId);
  runCache.set(runId, {
    session: current?.session ?? DEFAULT_SESSION,
    memory: { ...(current?.memory ?? emptyMemory()), ...patch },
  });
}

export function collabSelectionFromSession(
  session: CollabSession,
  known: (id: string) => boolean,
): { agentId: string | null; knowledgeId: string | null; relationId: string | null; tab: CollabInspectorTab } {
  if (!session.selectedAgentId || !known(session.selectedAgentId)) {
    return { agentId: null, knowledgeId: null, relationId: null, tab: session.inspectorTab };
  }
  return {
    agentId: session.selectedAgentId,
    knowledgeId: session.selectedKnowledgeId,
    relationId: session.selectedRelationId,
    tab: session.inspectorTab,
  };
}
