/**
 * C21 — read-only Agents panel tree from delegation events / tools.
 *
 * Ordinary shell / file tools stay in TerminalLog; only Task/Agent/spawn-style
 * delegation (or explicit view.state.agents) become Agent nodes.
 */

import type {
  ConversationEvent,
  ConversationView,
  ThreadAgentTreeSnapshot,
} from "@/lib/useConversation";
import {
  collectConversationTools,
  type ConversationToolRecord,
} from "./conversationEventViews";

export type ConversationAgentStatus =
  | "pending"
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "declined"
  | "interrupted";

export interface ConversationAgentNodeView {
  agentId: string;
  parentId?: string | null;
  title: string;
  nickname?: string | null;
  role?: string | null;
  model?: string | null;
  turnId?: string | null;
  messageId?: string | null;
  callId?: string | null;
  sessionRef?: string | null;
  status: ConversationAgentStatus | string;
  request?: string | null;
  result?: string | null;
  error?: string | null;
  activity?: string | null;
  toolUses?: number | null;
  totalTokens?: number | null;
  durationMs?: number | null;
  startedAt?: string | null;
  completedAt?: string | null;
  /** provider_native (default) | app_owned (Muteki subagent Thread). */
  origin?: string;
  /** app_owned: the child conversation Thread; open it to inspect the run. */
  childThreadId?: string | null;
  /** app_owned: lineage depth (direct children of the viewed Thread are 1). */
  lineageDepth?: number | null;
  adapterId?: string | null;
  accessMode?: string | null;
  isolation?: string | null;
  worktreePath?: string | null;
  worktreeState?: string | null;
  spawnState?: string | null;
  cancelRequested?: boolean;
  errorCode?: string | null;
  depth: number;
  children: ConversationAgentNodeView[];
}

export interface ConversationAgentTreeView {
  agents: ConversationAgentNodeView[];
  roots: ConversationAgentNodeView[];
  unsupported: boolean;
  unsupportedReason?: string | null;
  toolActivitySummary?: string | null;
  source?: string;
  revision?: number;
}

const DELEGATION_NAME_MARKERS = [
  "task",
  "taskcreate",
  "task_create",
  "taskupdate",
  "task_update",
  "tasktool",
  "agent",
  "subagent",
  "spawn",
  "delegate",
  "run_subagent",
  "muteki_spawn_run_worker",
];

const SHELL_NAME_MARKERS = [
  "shell",
  "bash",
  "zsh",
  "terminal",
  "exec",
  "command",
  "run_terminal",
  "powershell",
];

function normalizeToolName(name: string): string {
  return name.toLowerCase().replace(/[\s./:-]+/g, "_");
}

export function isShellLikeTool(record: ConversationToolRecord): boolean {
  const name = normalizeToolName(record.name || "");
  return SHELL_NAME_MARKERS.some(
    (marker) => name === marker || name.includes(marker),
  );
}

export function isDelegationTool(record: ConversationToolRecord): boolean {
  if (record.isAgent) return true;
  if (isShellLikeTool(record)) return false;
  const name = normalizeToolName(record.name || "");
  // Muteki task CRUD is resource access. Only its worker-spawn tool delegates.
  // MCP adapters may prefix the tool with a server namespace separated by __.
  const toolName = name.split("__").at(-1) || name;
  if (toolName.startsWith("muteki_")) {
    return toolName === "muteki_spawn_run_worker";
  }
  if (
    DELEGATION_NAME_MARKERS.some(
      (marker) =>
        name === marker
        || name.endsWith(`_${marker}`)
        || name.includes(`_${marker}_`)
        || name.startsWith(`${marker}_`),
    )
  ) {
    return true;
  }
  return false;
}

function parseArgsRecord(value: string): Record<string, unknown> {
  const text = String(value || "").trim();
  if (!text) return {};
  try {
    const parsed = JSON.parse(text) as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      return parsed as Record<string, unknown>;
    }
    return { text: parsed };
  } catch {
    return { text };
  }
}

function requestFromTool(record: ConversationToolRecord): string {
  const args = parseArgsRecord(record.argsSummary || "");
  for (const key of [
    "prompt",
    "description",
    "task",
    "goal",
    "query",
    "instruction",
    "text",
    "command",
  ]) {
    const value = args[key];
    if (value != null && String(value).trim()) return String(value).trim();
  }
  return (record.argsSummary || "").trim();
}

function statusFromTool(
  record: ConversationToolRecord,
): ConversationAgentStatus {
  switch (record.status) {
    case "running":
      return "running";
    case "failed":
      return "failed";
    case "completed":
      return "completed";
    case "pending":
      return "pending";
    case "cancelled":
      return "cancelled";
    case "declined":
      return "declined";
    default: {
      const _exhaustive: never = record.status;
      return _exhaustive;
    }
  }
}

export function agentsFromTools(
  tools: ConversationToolRecord[],
): ConversationAgentNodeView[] {
  const agents = tools.filter(isDelegationTool).map((tool) => {
    const args = parseArgsRecord(tool.argsSummary || "");
    const parentId = tool.parentId
      || (args.parent_id != null ? String(args.parent_id) : null)
      || (args.parent_agent_id != null ? String(args.parent_agent_id) : null)
      || (args.parent_tool_use_id != null
        ? String(args.parent_tool_use_id)
        : null);
    return {
      agentId: tool.id,
      parentId: parentId || null,
      title: tool.name || "Agent",
      model: tool.model || null,
      turnId: tool.turnId || null,
      messageId: tool.messageId || null,
      callId: tool.id,
      status: statusFromTool(tool),
      request: requestFromTool(tool) || null,
      result: tool.error || tool.outputSummary || null,
      error: tool.error || null,
      durationMs: tool.durationMs || null,
      startedAt: tool.occurredAt || null,
      depth: 0,
      children: [] as ConversationAgentNodeView[],
    };
  });
  return nestAgents(agents);
}

function nestAgents(
  flat: ConversationAgentNodeView[],
): ConversationAgentNodeView[] {
  const byId = new Map(flat.map((agent) => [agent.agentId, { ...agent, children: [] as ConversationAgentNodeView[] }]));
  const roots: ConversationAgentNodeView[] = [];
  for (const agent of byId.values()) {
    const parent = agent.parentId ? byId.get(agent.parentId) : undefined;
    if (parent && parent.agentId !== agent.agentId) {
      agent.depth = (parent.depth || 0) + 1;
      parent.children.push(agent);
    } else {
      agent.parentId = agent.parentId && byId.has(agent.parentId)
        ? agent.parentId
        : null;
      agent.depth = 0;
      roots.push(agent);
    }
  }
  return roots;
}

function flattenAgents(
  roots: ConversationAgentNodeView[],
): ConversationAgentNodeView[] {
  const out: ConversationAgentNodeView[] = [];
  const walk = (nodes: ConversationAgentNodeView[]) => {
    for (const node of nodes) {
      out.push(node);
      if (node.children.length) walk(node.children);
    }
  };
  walk(roots);
  return out;
}

function snapshotToTree(
  snapshot: ThreadAgentTreeSnapshot,
): ConversationAgentTreeView {
  const flat = (snapshot.agents || []).map((agent) => ({
    agentId: agent.agent_id,
    parentId: agent.parent_id ?? null,
    title: agent.title || agent.agent_id,
    nickname: agent.nickname,
    role: agent.role,
    model: agent.model,
    turnId: agent.turn_id,
    messageId: agent.message_id,
    callId: agent.call_id,
    sessionRef: agent.session_ref,
    status: agent.status,
    request: agent.request,
    result: agent.result || agent.error,
    error: agent.error,
    activity: agent.activity,
    toolUses: agent.tool_uses,
    totalTokens: agent.total_tokens,
    durationMs: agent.duration_ms,
    startedAt: agent.started_at,
    completedAt: agent.completed_at,
    depth: 0,
    children: [] as ConversationAgentNodeView[],
  }));
  const roots = nestAgents(flat);
  return {
    agents: flattenAgents(roots),
    roots,
    unsupported: Boolean(snapshot.unsupported),
    unsupportedReason: snapshot.unsupported_reason,
    toolActivitySummary: snapshot.tool_activity_summary,
    source: snapshot.source,
    revision: snapshot.revision,
  };
}

export function summarizeToolActivity(
  tools: ConversationToolRecord[],
): string {
  if (!tools.length) return "当前没有工具活动。";
  const running = tools.filter((tool) => tool.status === "running");
  const failed = tools.filter((tool) => tool.status === "failed");
  const cancelled = tools.filter((tool) => tool.status === "cancelled");
  const declined = tools.filter((tool) => tool.status === "declined");
  const shell = tools.filter(isShellLikeTool);
  const parts = [`${tools.length} 条工具记录`];
  if (running.length) parts.push(`${running.length} 项执行中`);
  if (failed.length) parts.push(`${failed.length} 项异常`);
  if (declined.length) parts.push(`${declined.length} 项已拒绝`);
  if (cancelled.length) parts.push(`${cancelled.length} 项已取消`);
  if (shell.length) parts.push(`含 ${shell.length} 条 shell/命令（不是独立 Agent）`);
  return parts.join(" · ");
}

/** app_owned subagent Threads (ThreadState.subagents) as tree roots. */
function appOwnedSubagentRoots(view?: ConversationView | null): ConversationAgentNodeView[] {
  const nodes = view?.state?.subagents || [];
  return nodes.map((node) => ({
    agentId: node.agent_id,
    parentId: null,
    title: node.title || node.agent_id,
    nickname: node.nickname,
    role: node.role,
    model: node.model,
    turnId: node.turn_id,
    messageId: node.message_id,
    callId: node.call_id,
    sessionRef: node.session_ref,
    status: node.status,
    request: node.request,
    result: node.result || node.error,
    error: node.error,
    startedAt: node.started_at,
    completedAt: node.completed_at,
    origin: node.origin || "app_owned",
    childThreadId: node.thread_id || node.agent_id,
    lineageDepth: node.depth ?? null,
    adapterId: node.adapter_id,
    accessMode: node.access_mode,
    isolation: node.isolation,
    worktreePath: node.worktree_path,
    worktreeState: node.worktree_state,
    spawnState: node.spawn_state,
    cancelRequested: Boolean(node.cancel_requested),
    errorCode: node.error_code,
    depth: 0,
    children: [] as ConversationAgentNodeView[],
  }));
}

/**
 * Prefer durable ThreadState.agents when present; otherwise derive from
 * delegation tool events. Never invent agent counts from ordinary tools.
 * Muteki-owned subagent Threads (ThreadState.subagents) always merge in as
 * additional roots alongside the provider-native tree.
 */
export function buildConversationAgentTree(
  events: ConversationEvent[],
  view?: ConversationView | null,
): ConversationAgentTreeView {
  const snapshot = view?.state?.agents;
  const turnStatusById = Object.fromEntries(
    (view?.turns || []).map((turn) => [turn.turn_id, turn.status]),
  );
  const appRoots = appOwnedSubagentRoots(view);

  if (snapshot && (snapshot.agents?.length || snapshot.unsupported)) {
    const tree = snapshotToTree(snapshot);
    if (!tree.toolActivitySummary) {
      const tools = collectConversationTools(events, turnStatusById);
      tree.toolActivitySummary = summarizeToolActivity(tools);
    }
    if (appRoots.length) {
      tree.roots = [...tree.roots, ...appRoots];
      tree.agents = [...tree.agents, ...appRoots];
    }
    return tree;
  }

  const tools = collectConversationTools(events, turnStatusById);
  const roots = [...agentsFromTools(tools), ...appRoots];
  const agents = flattenAgents(roots);
  return {
    agents,
    roots,
    unsupported: false,
    unsupportedReason: agents.length
      ? null
      : "当前轨迹没有委派 Agent 事件；普通工具活动请查看 Agent 执行日志。",
    toolActivitySummary: summarizeToolActivity(tools),
    source: agents.length ? "derived" : "none",
    revision: 0,
  };
}

export function agentStatusLabel(status: string): string {
  switch (status) {
    case "pending":
      return "等待中";
    case "running":
      return "执行中";
    case "completed":
      return "完成";
    case "failed":
      return "异常";
    case "cancelled":
      return "已取消";
    case "declined":
      return "已拒绝";
    case "interrupted":
      return "已中断";
    default:
      return status || "未知";
  }
}

export function agentOriginLabel(agent: ConversationAgentNodeView): string {
  return agent.origin === "app_owned" ? "Muteki" : "引擎";
}

export function worktreeStateLabel(state?: string | null): string {
  switch (state) {
    case "active":
      return "worktree 使用中";
    case "removed":
      return "worktree 已清理";
    case "retained":
      return "worktree 已保留（有未提交改动）";
    default:
      return state || "";
  }
}

export function agentDisplayName(agent: ConversationAgentNodeView): string {
  return (agent.nickname || agent.title || agent.role || agent.agentId).trim();
}

const AGENT_TERMINAL = new Set(["completed", "failed", "cancelled", "declined", "interrupted"]);

export function isAgentTerminal(status: string): boolean {
  return AGENT_TERMINAL.has(status);
}

export interface AgentStatusCounts {
  total: number;
  running: number;
  pending: number;
  completed: number;
  failed: number;
  cancelled: number;
}

export function countAgentStatuses(agents: ConversationAgentNodeView[]): AgentStatusCounts {
  const counts: AgentStatusCounts = { total: agents.length, running: 0, pending: 0, completed: 0, failed: 0, cancelled: 0 };
  for (const agent of agents) {
    const status = String(agent.status);
    if (status === "running") counts.running += 1;
    else if (status === "pending") counts.pending += 1;
    else if (status === "completed") counts.completed += 1;
    else if (status === "failed") counts.failed += 1;
    else counts.cancelled += 1;
  }
  return counts;
}

/** "2 个运行中 · 11 个完成" — only non-zero buckets, active first. */
export function formatAgentCounts(counts: AgentStatusCounts): string {
  const parts: string[] = [];
  if (counts.running) parts.push(`${counts.running} 个运行中`);
  if (counts.pending) parts.push(`${counts.pending} 个等待中`);
  if (counts.completed) parts.push(`${counts.completed} 个完成`);
  if (counts.failed) parts.push(`${counts.failed} 个异常`);
  if (counts.cancelled) parts.push(`${counts.cancelled} 个已停止`);
  return parts.join(" · ");
}

/** Wall time for one agent: runtime-reported duration wins over timestamps. */
export function agentDurationMs(agent: ConversationAgentNodeView, nowMs = Date.now()): number | null {
  if (agent.durationMs != null && agent.durationMs > 0 && isAgentTerminal(String(agent.status))) {
    return agent.durationMs;
  }
  const start = agent.startedAt ? Date.parse(agent.startedAt) : NaN;
  if (!Number.isFinite(start)) return agent.durationMs ?? null;
  const end = agent.completedAt ? Date.parse(agent.completedAt) : NaN;
  if (Number.isFinite(end)) return Math.max(0, end - start);
  return isAgentTerminal(String(agent.status)) ? agent.durationMs ?? null : Math.max(0, nowMs - start);
}

const AVATAR_HUES = [262, 200, 160, 28, 340, 120, 45, 300];

/** Stable per-agent hue so the same subagent keeps its colour everywhere. */
export function agentAvatarHue(agent: ConversationAgentNodeView): number {
  const key = agent.nickname || agent.role || agent.agentId;
  let hash = 0;
  for (let i = 0; i < key.length; i += 1) hash = (hash * 31 + key.charCodeAt(i)) >>> 0;
  return AVATAR_HUES[hash % AVATAR_HUES.length]!;
}

export function agentInitial(agent: ConversationAgentNodeView): string {
  const name = agentDisplayName(agent).replace(/^[^\p{L}\p{N}]+/u, "");
  return (Array.from(name)[0] || "A").toUpperCase();
}
