/**
 * Workspace kind 描述与首页入口（任务书 11.1、13.1，CORE-03）。
 *
 * 首页展示 workspace kind，而非扩大 "ctf" | "pentest" 联合类型。本库从后端
 * `GET /api/workspace-kinds`（muteki/platform/registry_api.py）拉取已注册的
 * workspace kind 描述，并派生首页创建入口数据；首页接线属于 UI-INTEG-01，
 * 这里只提供类型与 fetch/helper。
 *
 * 字段名与后端 pydantic 模型的 JSON（snake_case）保持一致。
 */

import { apiFetch } from "./useRun";

/** 模块/workspace kind 生命周期状态，对应后端 ModuleState。 */
export type WorkspaceKindState = "registered" | "ready" | "unavailable" | "disabled";

/** 后端 WorkspaceKindInfo 的 JSON 外形。 */
export interface WorkspaceKindInfo {
  schema_version: number;
  /** workspace kind 注册键，例如 conversation / single-security-task / competition */
  id: string;
  /** 提供该 kind 的 DomainModule id，例如 builtin.single-security-task */
  module_id: string;
  title: string;
  description: string;
  icon: string;
  /** 工作区路由，例如 /ctf、/pentest、/chat、/competitions */
  route: string;
  /** 首页创建入口路由 */
  create_entry: string;
  /** 后端聚合类型，例如 run / thread / competition */
  aggregate_type: string;
  state: WorkspaceKindState;
  task_kinds: string[];
}

/** 首页入口卡片数据（camelCase，供 UI 组件直接消费）。 */
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
  /** registered 表示依赖组件未全部就绪，入口可展示但应提示未就绪。 */
  ready: boolean;
}

/** 拉取已注册 workspace kind；禁用与校验失败的模块不会出现在列表里。 */
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

/** 由后端描述生成首页入口数据；缺少创建入口的 kind 不生成入口。 */
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

/** 批量派生首页入口；保持后端返回顺序。 */
function workspaceKindEntries(kinds: WorkspaceKindInfo[]): WorkspaceKindEntry[] {
  const entries: WorkspaceKindEntry[] = [];
  for (const kind of kinds) {
    const entry = toWorkspaceKindEntry(kind);
    if (entry) entries.push(entry);
  }
  return entries;
}

/** 拉取并派生首页入口，供 UI-INTEG-01 首页接线直接使用。 */
export async function fetchWorkspaceKindEntries(): Promise<WorkspaceKindEntry[]> {
  return workspaceKindEntries(await fetchWorkspaceKinds());
}
