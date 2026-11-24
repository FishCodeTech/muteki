
import type {
  ConversationAgentNode,
  ConversationEvent,
  ConversationView,
  ThreadAgentTreeSnapshot,
} from "@/lib/useConversation";
import {
  collectConversationTools,
  type ConversationToolRecord,
} from "./conversationEventViews";
import { unqualifiedToolName } from "../chat/timeline/toolPresentation";

export type ConversationAgentStatus =
  | "pending"
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "declined"
  | "interrupted"
  | "unknown"
  | "waiting";

export interface ConversationAgentNodeView {
  execution?: string;
  report?: string;
  delivery?: string;
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
  origin?: string;
  childThreadId?: string | null;
  lineageDepth?: number | null;
  adapterId?: string | null;
  accessMode?: string | null;
  effort?: string | null;
  acceptanceCriteria?: string | null;
  routingReason?: string | null;
  routing?: ConversationAgentNode["routing"] | null;
  isolation?: string | null;
  worktreePath?: string | null;
  worktreeState?: string | null;
  spawnState?: string | null;
  cancelRequested?: boolean;
  cancelSupported?: boolean;
  cancelReason?: string | null;
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

const LEGACY_DELEGATION_TOOL_NAMES = [
  "task",
  "tasktool",
  "agent",
  "subagent",
  "spawn",
  "spawn_agent",
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
  const toolName = unqualifiedToolName(record.name || "");
  if (toolName.startsWith("muteki_")) {
    return toolName === "muteki_spawn_run_worker";
  }
  if (record.isAgent != null) return record.isAgent;
  if (record.kind === "agent") return true;
  if (record.kind && record.kind !== "other") return false;
  if (isShellLikeTool(record)) return false;
  if (!/^[\w.-]+$/.test(record.name)) return false;
  return LEGACY_DELEGATION_TOOL_NAMES.includes(toolName);
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
      origin: unqualifiedToolName(tool.name || "") === "muteki_spawn_run_worker" ? "worker_run" : undefined,
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
    childThreadId: agent.thread_id,
    origin: agent.origin,
    adapterId: agent.adapter_id,
    effort: agent.effort,
    accessMode: agent.access_mode,
    lineageDepth: agent.depth,
    isolation: agent.isolation,
    worktreePath: agent.worktree_path,
    worktreeState: agent.worktree_state,
    acceptanceCriteria: agent.acceptance_criteria,
    routingReason: agent.routing_reason,
    routing: agent.routing ?? null,
    spawnState: agent.spawn_state,
    execution: agent.execution,
    report: agent.report,
    delivery: agent.delivery,
    cancelRequested: agent.cancel_requested,
    cancelSupported: agent.cancel_supported,
    cancelReason: agent.cancel_reason,
    errorCode: agent.error_code,
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
    messageId: null,
    callId: node.call_id,
    sessionRef: node.session_ref,
    status: node.status,
    request: node.request,
    result: node.result || node.error,
    error: node.error,
    activity: node.activity,
    toolUses: node.tool_uses,
    totalTokens: node.total_tokens,
    durationMs: node.duration_ms,
    startedAt: node.started_at,
    completedAt: node.completed_at,
    origin: node.origin || "app_owned",
    childThreadId: node.thread_id || node.agent_id,
    lineageDepth: node.depth ?? null,
    adapterId: node.adapter_id,
    accessMode: node.access_mode,
    effort: node.effort,
    acceptanceCriteria: node.acceptance_criteria,
    routingReason: node.routing_reason,
    routing: node.routing ?? null,
    isolation: node.isolation,
    worktreePath: node.worktree_path,
    worktreeState: node.worktree_state,
    spawnState: node.spawn_state,
    report: node.report,
    execution: node.execution,
    delivery: node.delivery,
    cancelRequested: Boolean(node.cancel_requested),
    cancelSupported: node.cancel_supported,
    cancelReason: node.cancel_reason,
    errorCode: node.error_code,
    depth: 0,
    children: [] as ConversationAgentNodeView[],
  }));
}

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
    case "waiting":
      return "等待消息";
    case "unknown":
      return "状态未知";
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
  if (agent.origin === "app_owned") return "Muteki 托管";
  if (agent.origin === "worker_run") return "后台 Worker";
  return "引擎原生";
}

const ROUTING_SOURCE_LABELS: Record<string, string> = {
  orchestration_role: "角色配置",
  workspace_policy: "工作区默认策略",
  explicit_request: "显式指定",
  inherited_parent: "继承主会话",
};

export function agentRoutingSourceLabel(agent: ConversationAgentNodeView): string {
  const source = agent.routing?.source || "";
  if (!source) return "";
  const label = ROUTING_SOURCE_LABELS[source] || source;
  const scopeTag =
    agent.routing?.workbench_mode === "src"
      ? agent.routing?.orchestration_scope === "mode_override"
        ? "SRC 独立"
        : "继承聊天"
      : "";
  if (source === "orchestration_role" || source === "workspace_policy") {
    const parts = [agent.routing?.role, scopeTag].filter(Boolean);
    return parts.length ? `${label} (${parts.join(" · ")})` : label;
  }
  return label;
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

function plainPreviewText(text: string): string {
  const code: string[] = [];
  return text
    .replace(/[\uE000\uE001]/g, "")
    .replace(/^\s*(?:```|~~~).*$/gm, " ")
    .replace(/`([^`\n]+)`/g, (_, span: string) => `\uE000${code.push(span) - 1}\uE001`)
    .replace(/^\s*([-*_])(?:\s*\1){2,}\s*$/gm, " ")
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/^\s{0,3}(?:#{1,6}\s+|>\s?|[-*+]\s+(?:\[[ xX]\]\s+)?)/gm, "")
    .replace(/(\*\*|~~)(?=\S)([\s\S]*?\S)\1/g, "$2")
    .replace(/(^|[^\w])__(?=\S)([\s\S]*?\S)__(?!\w)/g, "$1$2")
    .replace(/(^|[^\w*])\*(?=\S)([^*\n]*?\S)\*(?![\w*])/g, "$1$2")
    .replace(/\uE000(\d+)\uE001/g, (_, index: string) => code[Number(index)] ?? "")
    .replace(/\s+/g, " ")
    .trim();
}

const PREVIEW_SOURCE_CHARS = 600;

export function agentPreviewText(agent: ConversationAgentNodeView): string {
  const raw = isAgentTerminal(String(agent.status))
    ? (agent.error || agent.result || agent.activity || "")
    : (agent.activity || agent.request || "");
  return plainPreviewText(raw.slice(0, PREVIEW_SOURCE_CHARS));
}

export interface AgentStatusCounts {
  total: number;
  running: number;
  pending: number;
  completed: number;
  failed: number;
  cancelled: number;
  unknown: number;
  waiting: number;
}

export function countAgentStatuses(agents: ConversationAgentNodeView[]): AgentStatusCounts {
  const counts: AgentStatusCounts = { total: agents.length, running: 0, pending: 0, completed: 0, failed: 0, cancelled: 0, unknown: 0, waiting: 0 };
  for (const agent of agents) {
    const status = String(agent.status);
    if (status === "running") counts.running += 1;
    else if (status === "pending") counts.pending += 1;
    else if (status === "waiting") counts.waiting += 1;
    else if (status === "completed") counts.completed += 1;
    else if (status === "failed") counts.failed += 1;
    else if (isAgentTerminal(status)) counts.cancelled += 1;
    else counts.unknown += 1;
  }
  return counts;
}

export function formatAgentCounts(counts: AgentStatusCounts): string {
  const parts: string[] = [];
  if (counts.running) parts.push(`${counts.running} 个运行中`);
  if (counts.pending) parts.push(`${counts.pending} 个等待中`);
  if (counts.waiting) parts.push(`${counts.waiting} 个等待消息`);
  if (counts.completed) parts.push(`${counts.completed} 个完成`);
  if (counts.failed) parts.push(`${counts.failed} 个异常`);
  if (counts.cancelled) parts.push(`${counts.cancelled} 个已停止`);
  if (counts.unknown) parts.push(`${counts.unknown} 个状态未知`);
  return parts.join(" · ");
}

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
