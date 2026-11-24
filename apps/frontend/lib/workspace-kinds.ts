
import { apiFetch } from "./useRun";

export type WorkspaceKindState = "registered" | "ready" | "unavailable" | "disabled";

export interface WorkspaceKindInfo {
  schema_version: number;
  id: string;
  module_id: string;
  title: string;
  description: string;
  icon: string;
  route: string;
  create_entry: string;
  aggregate_type: string;
  state: WorkspaceKindState;
  task_kinds: string[];
}

export interface WorkspaceKindEntry {
  id: string;
  moduleId: string;
  title: string;
  description: string;
  icon: string;
  route: string;
  createEntry: string;
  aggregateType: string;
  state: WorkspaceKindState;
  taskKinds: string[];
  ready: boolean;
}

async function fetchWorkspaceKinds(): Promise<WorkspaceKindInfo[]> {
  const res = await apiFetch("/api/workspace-kinds");
  if (!res.ok) {
    throw new Error(`failed to load workspace kinds: HTTP ${res.status}`);
  }
  const data = (await res.json()) as unknown;
  if (!Array.isArray(data)) {
    throw new Error("failed to load workspace kinds: unexpected response shape");
  }
  return data as WorkspaceKindInfo[];
}

function toWorkspaceKindEntry(kind: WorkspaceKindInfo): WorkspaceKindEntry | null {
  if (!kind.create_entry) return null;
  return {
    id: kind.id,
    moduleId: kind.module_id,
    title: kind.title,
    description: kind.description,
    icon: kind.icon,
    route: kind.route,
    createEntry: kind.create_entry,
    aggregateType: kind.aggregate_type,
    state: kind.state,
    taskKinds: kind.task_kinds,
    ready: kind.state === "ready",
  };
}

function workspaceKindEntries(kinds: WorkspaceKindInfo[]): WorkspaceKindEntry[] {
  const entries: WorkspaceKindEntry[] = [];
  for (const kind of kinds) {
    const entry = toWorkspaceKindEntry(kind);
    if (entry) entries.push(entry);
  }
  return entries;
}

export async function fetchWorkspaceKindEntries(): Promise<WorkspaceKindEntry[]> {
  return workspaceKindEntries(await fetchWorkspaceKinds());
}
