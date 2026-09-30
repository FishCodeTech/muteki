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
  | "declined";

export interface ConversationAgentNodeView {
  agentId: string;
  parentId?: string | null;
  title: string;
  model?: string | null;
  turnId?: string | null;
  messageId?: string | null;
  callId?: string | null;
  status: ConversationAgentStatus | string;
  request?: string | null;
  result?: string | null;
  error?: string | null;
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
  if (isShellLikeTool(record)) return false;
  const name = normalizeToolName(record.name || "");
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
  if (record.parentId && record.isAgent) return true;
  return Boolean(record.isAgent);
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
    model: agent.model,
    turnId: agent.turn_id,
    messageId: agent.message_id,
    callId: agent.call_id,
    status: agent.status,
    request: agent.request,
    result: agent.result || agent.error,
    error: agent.error,
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

/**
 * Prefer durable ThreadState.agents when present; otherwise derive from
 * delegation tool events. Never invent agent counts from ordinary tools.
 */
export function buildConversationAgentTree(
  events: ConversationEvent[],
  view?: ConversationView | null,
): ConversationAgentTreeView {
  const snapshot = view?.state?.agents;
  const turnStatusById = Object.fromEntries(
    (view?.turns || []).map((turn) => [turn.turn_id, turn.status]),
  );

  if (snapshot && (snapshot.agents?.length || snapshot.unsupported)) {
    const tree = snapshotToTree(snapshot);
    if (!tree.toolActivitySummary) {
      const tools = collectConversationTools(events, turnStatusById);
      tree.toolActivitySummary = summarizeToolActivity(tools);
    }
    return tree;
  }

  const tools = collectConversationTools(events, turnStatusById);
  const roots = agentsFromTools(tools);
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
    default:
      return status || "未知";
  }
}
