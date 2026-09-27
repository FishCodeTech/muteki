import { COORDINATOR_ID, type CollaborationScope } from "./agentCollaboration";

/** Query keys for a shareable / refreshable collaboration deep link. */
export const COLLAB_URL_KEYS = {
  view: "view",
  agent: "agent",
  knowledge: "k",
  relation: "rel",
  scope: "scope",
} as const;

export const COLLAB_URL_VIEW = "collaboration";

const WRITE_MS = 200;

export type CollabUrlState = {
  view: typeof COLLAB_URL_VIEW | null;
  agentId: string | null;
  knowledgeId: string | null;
  relationId: string | null;
  scope: CollaborationScope;
};

export type CollabUrlWrite = {
  view: typeof COLLAB_URL_VIEW;
  agentId: string | null;
  knowledgeId: string | null;
  relationId: string | null;
  scope: CollaborationScope;
};

let writeTimer = 0;

/** Path of the standalone collaboration page for a run. */
export function collabPagePath(runId: string): string {
  return `/run/${encodeURIComponent(runId)}/collaboration`;
}

/** On the standalone page the path already says "collaboration"; no view key needed. */
function onCollabPage(): boolean {
  return typeof window !== "undefined" && /\/collaboration\/?$/.test(window.location.pathname);
}

function param(value: string | null): string | null {
  return value ? value : null;
}

export function readCollabUrlState(search?: string): CollabUrlState {
  const raw = search ?? (typeof window === "undefined" ? "" : window.location.search);
  const params = new URLSearchParams(raw.startsWith("?") ? raw.slice(1) : raw);
  const scope = params.get(COLLAB_URL_KEYS.scope);
  return {
    view: params.get(COLLAB_URL_KEYS.view) === COLLAB_URL_VIEW ? COLLAB_URL_VIEW : null,
    agentId: param(params.get(COLLAB_URL_KEYS.agent)),
    knowledgeId: param(params.get(COLLAB_URL_KEYS.knowledge)),
    relationId: param(params.get(COLLAB_URL_KEYS.relation)),
    // A plain graph link must include earlier executions. Scope is local to
    // this URL, never inherited from another run’s saved filters.
    scope: scope === "current" ? "current" : "all",
  };
}

function replaceSearch(params: URLSearchParams): void {
  if (typeof window === "undefined") return;
  const query = params.toString();
  const next = `${window.location.pathname}${query ? `?${query}` : ""}${window.location.hash}`;
  const current = `${window.location.pathname}${window.location.search}${window.location.hash}`;
  if (next === current) return;
  window.history.replaceState(window.history.state ?? {}, "", next);
}

function setOptional(params: URLSearchParams, key: string, value: string | null): void {
  if (value) params.set(key, value);
  else params.delete(key);
}

/** Write collaboration fields; any other query key (e.g. return) is left in place. */
export function writeCollabUrlState(state: CollabUrlWrite): void {
  if (typeof window === "undefined") return;
  const params = new URLSearchParams(window.location.search);
  if (onCollabPage()) params.delete(COLLAB_URL_KEYS.view);
  else params.set(COLLAB_URL_KEYS.view, COLLAB_URL_VIEW);
  setOptional(
    params,
    COLLAB_URL_KEYS.agent,
    state.agentId && state.agentId !== COORDINATOR_ID ? state.agentId : null,
  );
  setOptional(params, COLLAB_URL_KEYS.knowledge, state.knowledgeId);
  setOptional(params, COLLAB_URL_KEYS.relation, state.relationId);
  setOptional(params, COLLAB_URL_KEYS.scope, state.scope === "current" ? "current" : null);
  replaceSearch(params);
}

export function scheduleCollabUrlWrite(state: CollabUrlWrite): void {
  if (typeof window === "undefined") return;
  window.clearTimeout(writeTimer);
  writeTimer = window.setTimeout(() => {
    writeTimer = 0;
    writeCollabUrlState(state);
  }, WRITE_MS);
}

export function cancelCollabUrlWrite(): void {
  if (writeTimer) {
    window.clearTimeout(writeTimer);
    writeTimer = 0;
  }
}
