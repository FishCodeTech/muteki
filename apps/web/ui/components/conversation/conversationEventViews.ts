import type { ConversationEvent } from "@/lib/useConversation";
import type { ToolItem } from "../ai-native/tool-chips";

export interface ConversationToolRecord extends ToolItem {
  turnId?: string;
  occurredAt?: string;
  parentId?: string | null;
  isAgent?: boolean;
  model?: string | null;
  messageId?: string | null;
}

export type ConversationMessagePhase = "commentary" | "final_answer" | "unknown";

function serializeEventValue(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

export function conversationErrorMessage(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value.trim();
  if (Array.isArray(value)) {
    return value.map(conversationErrorMessage).filter(Boolean).join("；");
  }
  if (typeof value === "object") {
    const row = value as Record<string, unknown>;
    for (const key of ["message", "detail", "errorMessage", "error", "reason"]) {
      const text = conversationErrorMessage(row[key]);
      if (text) return text;
    }
    const code = String(row.code || "").trim();
    const operation = String(row.operation || "").trim();
    const capability = String(row.capability || "").trim();
    if (code === "external_agent.capability.unsupported") {
      const label = capability || operation || "所需功能";
      return `当前 Agent 接入不支持 ${label}；Muteki 将使用已保存的对话历史重新建立会话`;
    }
    if (code) {
      const context = [operation && `操作：${operation}`, capability && `能力：${capability}`]
        .filter(Boolean)
        .join("，");
      return context ? `${code}（${context}）` : code;
    }
  }
  return "";
}

export type ConversationTurnSegment =
  | { kind: "thinking"; turnId: string; text: string; durationMs?: number }
  | {
      kind: "text";
      turnId: string;
      text: string;
      phase: ConversationMessagePhase;
    }
  | { kind: "tools"; turnId: string; tools: ConversationToolRecord[] };

export interface ConversationTurnPresentation {
  activity: ConversationTurnSegment[];
  answerText: string;
}

export interface ConversationLiveActivity {
  kind: "idle" | "tool" | "reasoning" | "response";
  label: string;
}

export interface ConversationTurnFoldState {
  segmentsByTurn: Record<string, ConversationTurnSegment[]>;
  textFromDeltas: Record<string, boolean>;
}

export interface ConversationTurnRuntime {
  adapterId: string;
  instanceId: string;
  credentialId: string;
  model: string;
  effort: string;
  accessMode: string;
}

function runtimeRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === "object"
    ? value as Record<string, unknown>
    : {};
}

function mergeTurnRuntime(
  current: ConversationTurnRuntime,
  value: unknown,
): ConversationTurnRuntime {
  const row = runtimeRecord(value);
  const pick = (snake: string, camel: string, fallback: string) => {
    const next = row[snake] ?? row[camel];
    return next == null || String(next).trim() === ""
      ? fallback
      : String(next).trim();
  };
  return {
    adapterId: pick("adapter_id", "adapterId", current.adapterId),
    instanceId: pick("instance_id", "instanceId", current.instanceId || "default"),
    credentialId: pick("credential_id", "credentialId", current.credentialId),
    model: pick("model", "model", current.model),
    effort: pick("effort", "effort", current.effort),
    accessMode: pick(
      "access_mode",
      "accessMode",
      current.accessMode,
    ),
  };
}

export const EMPTY_TURN_RUNTIME: ConversationTurnRuntime = {
  adapterId: "",
  instanceId: "default",
  credentialId: "",
  model: "",
  effort: "",
  accessMode: "",
};

/**
 * Reconstruct the Runtime selection that produced each turn.
 *
 * New streams carry a complete selection on ``core.turn.started``.  The
 * thread-created and session events keep older histories useful, including
 * the original ``default`` model alias paired with its credential endpoint.
 *
 * Do **not** seed from the Thread's *current* Runtime selection: that value
 * changes when the user switches Provider/model and would rewrite historical
 * attribution (#132).  Missing per-turn metadata stays empty so the UI can
 * label it ``unknown`` instead of falling back to the live selector.
 *
 * Superseded turns keep their recorded Runtime so retry / audit history can
 * still show who generated the answer.
 */
export function buildConversationTurnRuntimeMap(
  events: ConversationEvent[] = [],
  _fallbackRuntime: unknown = {},
): Record<string, ConversationTurnRuntime> {
  // Intentionally ignore _fallbackRuntime (kept for call-site compat).
  let current = { ...EMPTY_TURN_RUNTIME };
  const byTurn: Record<string, ConversationTurnRuntime> = {};
  const ordered = [...events].sort((left, right) => left.seq - right.seq);

  for (const event of ordered) {
    const payload = event.payload || {};
    const type = event.event_type || "";
    const nestedRuntime = runtimeRecord(payload.runtime);
    const hasNestedRuntime = Object.keys(nestedRuntime).length > 0;

    if (type === "core.thread.created") {
      current = mergeTurnRuntime(EMPTY_TURN_RUNTIME, nestedRuntime);
    } else if (type === "core.thread.runtime_switched") {
      current = mergeTurnRuntime(current, hasNestedRuntime ? nestedRuntime : payload);
    } else if (
      type === "core.session.started"
      || type === "core.session.resumed"
      || type === "core.turn.started"
    ) {
      current = mergeTurnRuntime(current, hasNestedRuntime ? nestedRuntime : payload);
    }

    const turnId = String(payload.turn_id || "");
    if (!turnId) continue;
    if (
      type === "core.turn.requested"
      || type === "core.session.started"
      || type === "core.session.resumed"
      || type === "core.turn.started"
      || hasNestedRuntime
    ) {
      // Prefer the nested snapshot on the event itself when present so a
      // late/out-of-band runtime field cannot be clobbered by `current`
      // after a subsequent Provider switch in the same fold pass.
      byTurn[turnId] = hasNestedRuntime
        ? mergeTurnRuntime(byTurn[turnId] || current, nestedRuntime)
        : { ...current };
    }
  }

  return byTurn;
}

const TURN_FINISH_EVENT_TYPES = new Set([
  "core.turn.completed",
  "core.turn.failed",
  "core.turn.interrupted",
]);

/** Matches backend EV_TURN_RETRIED / EDIT_RESENT / REWOUND. */
const TURN_SUPERSESSION_EVENT_TYPES = new Set([
  "core.turn.retried",
  "core.turn.edit_resent",
  "core.turn.rewound",
]);

export interface ConversationTurnTiming {
  startedAt?: string;
  finishedAt?: string;
  durationMs?: number;
}

function parseConversationTimeMs(value: unknown): number | undefined {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value < 1e12 ? value * 1000 : value;
  }
  if (typeof value === "string" && value.trim()) {
    const parsed = Date.parse(value);
    return Number.isFinite(parsed) ? parsed : undefined;
  }
  return undefined;
}

function reportedTurnDurationMs(
  payload: Record<string, unknown>,
): number | undefined {
  const raw = payload.duration_ms
    ?? payload.elapsed_ms
    ?? payload.durationMs
    ?? payload.elapsedMs;
  if (raw == null || raw === "") return undefined;
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || parsed < 0) return undefined;
  return parsed;
}

/**
 * Wall-clock timing for each turn. Prefers duration fields on the finish
 * event; otherwise uses started/finished timestamps. No value is invented
 * when those sources are missing.
 */
export function buildConversationTurnTimingMap(
  events: ConversationEvent[] = [],
): Record<string, ConversationTurnTiming> {
  const byTurn: Record<string, ConversationTurnTiming> = {};
  const ordered = [...events].sort((left, right) => left.seq - right.seq);

  for (const event of ordered) {
    const payload = event.payload || {};
    const type = event.event_type || "";
    if (TURN_SUPERSESSION_EVENT_TYPES.has(type)) {
      const superseded = Array.isArray(payload.superseded_turn_ids)
        ? payload.superseded_turn_ids.map(String)
        : [];
      for (const turnId of superseded) delete byTurn[turnId];
    }

    const turnId = String(payload.turn_id || "");
    if (!turnId) continue;
    const current = byTurn[turnId] || {};

    if (type === "core.turn.started") {
      byTurn[turnId] = {
        ...current,
        startedAt: event.occurred_at || current.startedAt,
      };
      continue;
    }

    if (!TURN_FINISH_EVENT_TYPES.has(type)) continue;

    const finishedAt = event.occurred_at || current.finishedAt;
    let durationMs = reportedTurnDurationMs(payload);
    if (durationMs == null) {
      const startedAt = current.startedAt;
      const start = parseConversationTimeMs(startedAt);
      const end = parseConversationTimeMs(finishedAt);
      if (start != null && end != null && end >= start) {
        durationMs = end - start;
      }
    }
    byTurn[turnId] = {
      ...current,
      finishedAt,
      ...(durationMs != null ? { durationMs } : {}),
    };
  }

  return byTurn;
}

export function resolveConversationTurnDurationMs(options: {
  timing?: ConversationTurnTiming;
  createdAt?: string;
  completedAt?: string;
  running?: boolean;
  nowMs?: number;
}): number | undefined {
  const { timing, createdAt, completedAt, running = false, nowMs } = options;
  if (timing?.durationMs != null) return timing.durationMs;
  const start = parseConversationTimeMs(timing?.startedAt || createdAt);
  const end = parseConversationTimeMs(timing?.finishedAt || completedAt);
  if (start != null && end != null && end >= start) return end - start;
  if (running && start != null && nowMs != null) {
    const elapsed = nowMs - start;
    if (elapsed >= 0) return elapsed;
  }
  return undefined;
}

function assistantRole(value: unknown): boolean {
  return ["assistant", "agent", ""].includes(String(value || "").toLowerCase());
}

function messagePhase(payload: Record<string, unknown>): ConversationMessagePhase {
  const value = String(
    payload.phase || payload.message_phase || payload.channel || "",
  ).toLowerCase();
  if (value === "commentary") return "commentary";
  if (value === "final_answer" || value === "final") return "final_answer";
  return "unknown";
}

function isConversationToolEvent(type: string): boolean {
  return (
    type.includes("tool") ||
    type === "core.tool.invoked" ||
    type === "core.tool.completed" ||
    type === "core.tool.failed"
  );
}

function conversationToolKey(
  payload: Record<string, unknown>,
  turnId: string,
): string {
  const provisionalName = String(
    payload.tool_name || payload.name || payload.tool || "",
  );
  return String(
    payload.tool_call_id ||
      payload.call_id ||
      payload.invocation_id ||
      payload.id ||
      `${turnId}:${provisionalName || "tool"}`,
  );
}

/** Map a finished turn onto open tool records (events may lag or omit tool end). */
export type ConversationTurnTerminalStatus = "completed" | "failed" | "interrupted";

function turnTerminalFromEventType(type: string): ConversationTurnTerminalStatus | null {
  if (type === "core.turn.completed") return "completed";
  if (type === "core.turn.failed") return "failed";
  if (type === "core.turn.interrupted") return "interrupted";
  return null;
}

function isOpenToolStatus(status: ConversationToolRecord["status"]): boolean {
  return status === "running" || status === "pending";
}

function normalizeToolPayloadStatus(raw: unknown): string {
  return String(raw ?? "").trim().toLowerCase();
}

/** Resolve real tool outcome; lifecycle "completed" ≠ success (#220). */
export function resolveToolOutcomeStatus(
  payload: Record<string, unknown>,
  eventType: string,
  previous?: ConversationToolRecord,
): ConversationToolRecord["status"] {
  const type = eventType || "";
  const payloadStatus = normalizeToolPayloadStatus(
    payload.status ?? payload.result_status ?? payload.outcome,
  );
  const exitRaw = payload.exit_code ?? payload.exitCode;
  const exitCode = exitRaw == null || exitRaw === "" ? null : Number(exitRaw);
  const hasError =
    Boolean(payload.error)
    || payload.is_error === true
    || payload.isError === true;
  const typeFailed = type.includes("failed");
  const typeCompleted = type.includes("completed");

  if (["declined", "denied", "rejected"].includes(payloadStatus)) {
    return "declined";
  }
  if (["cancelled", "canceled", "aborted", "interrupted"].includes(payloadStatus)) {
    return "cancelled";
  }
  if (["failed", "error"].includes(payloadStatus) || typeFailed || hasError) {
    return "failed";
  }
  if (exitCode != null && Number.isFinite(exitCode) && exitCode !== 0) {
    return "failed";
  }

  // Explicit denial is a durable outcome. A generic lifecycle completion
  // replay cannot turn a rejected operation into a successful execution.
  if (previous?.status === "declined") return "declined";
  if (["completed", "success", "ok", "succeeded"].includes(payloadStatus)) {
    return "completed";
  }

  if (typeCompleted) {
    // Lifecycle end without an explicit outcome is success. A prior interrupt
    // settle to cancelled must yield to a real completed (#133); declined /
    // failed / cancelled outcomes were already returned from payload above.
    return "completed";
  }

  // Non-terminal updates keep a known terminal outcome (e.g. late progress).
  if (previous?.status === "cancelled") return "cancelled";
  if (previous?.status === "completed" || previous?.status === "failed") {
    return previous.status;
  }
  return "running";
}

function toolOutcomeError(
  status: ConversationToolRecord["status"],
  payload: Record<string, unknown>,
  previous?: ConversationToolRecord,
  failedHint?: boolean,
): string | undefined {
  if (status === "declined") {
    return (
      previous?.error
      || (payload.error ? serializeEventValue(payload.error) : "")
      || "已拒绝 · 未执行"
    );
  }
  if (status === "cancelled") {
    if (previous?.status === "cancelled" && previous.error) return previous.error;
    return (
      (payload.error ? serializeEventValue(payload.error) : "")
      || previous?.error
      || "已取消"
    );
  }
  if (status === "failed") {
    if (previous?.status === "cancelled") return previous.error || "已中断";
    return (
      (payload.error ? serializeEventValue(payload.error) : "")
      || (failedHint
        ? serializeEventValue(payload.output || payload.result) || "工具执行失败"
        : "")
      || previous?.error
      || "工具执行失败"
    );
  }
  if (previous?.status === "cancelled") return previous.error || "已中断";
  return previous?.error;
}

/** Normalize durable turn.status / finish events for shell projection (#133). */
export function normalizeTurnTerminalStatus(
  turnStatus: string,
): ConversationTurnTerminalStatus | null {
  const status = String(turnStatus || "").trim().toLowerCase();
  if (status === "completed") return "completed";
  if (status === "failed") return "failed";
  // Pi/Codex stop may surface as aborted/cancelled; treat like interrupted.
  if (
    status === "interrupted"
    || status === "aborted"
    || status === "cancelled"
    || status === "canceled"
  ) {
    return "interrupted";
  }
  return null;
}

export function settleConversationToolForTurn(
  tool: ConversationToolRecord,
  turnStatus: string,
): ConversationToolRecord {
  const normalized = normalizeTurnTerminalStatus(turnStatus) || turnStatus;
  // Explicit denial/cancel must survive turn-level settle (#220).
  if (tool.status === "declined") {
    return tool.error ? tool : { ...tool, error: "已拒绝 · 未执行" };
  }
  // Interrupted/aborted: open tools and late tool.failed → 已取消/已中断, not 执行失败.
  if (normalized === "interrupted") {
    if (tool.status === "cancelled") {
      return tool.error ? tool : { ...tool, error: "已中断" };
    }
    if (isOpenToolStatus(tool.status) || tool.status === "failed") {
      return {
        ...tool,
        status: "cancelled",
        // Prefer interrupt copy over late kill/failed noise for chips (#133).
        error: tool.status === "failed" ? "已中断" : (tool.error || "已中断"),
      };
    }
    return tool;
  }
  if (!isOpenToolStatus(tool.status)) return tool;
  if (normalized === "failed") {
    return {
      ...tool,
      status: "failed",
      error: tool.error || "回合失败",
    };
  }
  if (normalized === "completed") {
    return {
      ...tool,
      status: "completed",
    };
  }
  return tool;
}

function settleToolsAgainstTurnStatus(
  tools: ConversationToolRecord[],
  turnStatus: string,
): ConversationToolRecord[] {
  let changed = false;
  const next = tools.map((tool) => {
    const settled = settleConversationToolForTurn(tool, turnStatus);
    if (settled !== tool) changed = true;
    return settled;
  });
  return changed ? next : tools;
}

export function settleTurnSegmentsAgainstStatus(
  segments: ConversationTurnSegment[],
  turnStatus: string,
): ConversationTurnSegment[] {
  const normalized = normalizeTurnTerminalStatus(turnStatus);
  if (!normalized) {
    return segments;
  }
  let changed = false;
  const next = segments.map((segment) => {
    if (segment.kind !== "tools") return segment;
    const tools = settleToolsAgainstTurnStatus(segment.tools, normalized);
    if (tools === segment.tools) return segment;
    changed = true;
    return { ...segment, tools };
  });
  return changed ? next : segments;
}

function mergeConversationTool(
  event: ConversationEvent,
  previous?: ConversationToolRecord,
): ConversationToolRecord {
  const payload = event.payload || {};
  const type = event.event_type || "";
  const turnId = String(payload.turn_id || previous?.turnId || "");
  const provisionalName = String(
    payload.tool_name || payload.name || payload.tool || "",
  );
  const failedHint =
    type.includes("failed") ||
    Boolean(payload.error) ||
    payload.is_error === true ||
    payload.isError === true;
  const parentRaw =
    payload.parent_tool_use_id
    ?? payload.parent_call_id
    ?? payload.parent_id
    ?? payload.parent_agent_id
    ?? previous?.parentId;
  const parentId = parentRaw == null || String(parentRaw).trim() === ""
    ? previous?.parentId ?? null
    : String(parentRaw);
  const explicitAgent =
    payload.is_agent === true
    || payload.isAgent === true
    || payload.agent === true
    || String(payload.kind || "").toLowerCase() === "agent";
  // Prefer payload outcome (declined/failed/…) over bare lifecycle "completed" (#220).
  // Late progress must not revive running; prior declined/cancelled survive.
  const status = resolveToolOutcomeStatus(payload, type, previous);
  return {
    id: conversationToolKey(payload, turnId),
    name: provisionalName || previous?.name || "未知工具",
    status,
    durationMs: Number(payload.duration_ms || payload.elapsed_ms || previous?.durationMs || 0),
    argsSummary:
      serializeEventValue(payload.arguments || payload.input || payload.args) ||
      previous?.argsSummary ||
      "",
    outputSummary:
      serializeEventValue(payload.output || payload.result || payload.summary) ||
      previous?.outputSummary ||
      "",
    error: toolOutcomeError(status, payload, previous, failedHint),
    turnId: turnId || previous?.turnId,
    // Keep the first observed start; late progress/replay must not move it (#218).
    occurredAt: previous?.occurredAt || event.occurred_at,
    parentId,
    isAgent: explicitAgent || previous?.isAgent || false,
    model: String(payload.model || previous?.model || "").trim() || previous?.model || null,
    messageId: String(payload.message_id || previous?.messageId || "").trim()
      || previous?.messageId
      || null,
  };
}

function findToolRecord(
  segments: ConversationTurnSegment[],
  toolId: string,
): ConversationToolRecord | undefined {
  for (const segment of segments) {
    if (segment.kind !== "tools") continue;
    const found = segment.tools.find((tool) => tool.id === toolId);
    if (found) return found;
  }
  return undefined;
}

function upsertToolSegment(
  segments: ConversationTurnSegment[],
  turnId: string,
  record: ConversationToolRecord,
): ConversationTurnSegment[] {
  let replaced = false;
  const next = segments.map((segment) => {
    if (segment.kind !== "tools") return segment;
    const index = segment.tools.findIndex((tool) => tool.id === record.id);
    if (index < 0) return segment;
    replaced = true;
    const tools = [...segment.tools];
    tools[index] = record;
    return { ...segment, tools };
  });
  if (replaced) return next;
  const last = next[next.length - 1];
  if (last?.kind === "tools") {
    next[next.length - 1] = { ...last, tools: [...last.tools, record] };
    return next;
  }
  next.push({ kind: "tools", turnId, tools: [record] });
  return next;
}

function appendTurnText(
  segments: ConversationTurnSegment[],
  turnId: string,
  kind: "text" | "thinking",
  text: string,
  durationMs?: number,
  phase: ConversationMessagePhase = "unknown",
): ConversationTurnSegment[] {
  const last = segments[segments.length - 1];
  const canMerge = last?.kind === kind
    && (last.kind !== "text" || last.phase === phase);
  if (canMerge) {
    const merged = kind === "thinking"
      ? {
          ...last,
          text: `${last.text}${text}`,
          ...(durationMs ? { durationMs } : {}),
        }
      : { ...last, text: `${last.text}${text}` };
    return [...segments.slice(0, -1), merged];
  }
  if (kind === "thinking") {
    return [...segments, { kind, turnId, text, ...(durationMs ? { durationMs } : {}) }];
  }
  return [...segments, { kind, turnId, text, phase }];
}

function compactActivityValue(value: unknown, maxLength = 32): string {
  return String(value ?? "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, maxLength);
}

function toolArgumentRecord(value: string): Record<string, unknown> {
  const text = String(value || "").trim();
  if (!text) return {};
  try {
    const parsed = JSON.parse(text) as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      return parsed as Record<string, unknown>;
    }
    return { command: parsed };
  } catch {
    return { command: text };
  }
}

function activityBasename(value: unknown): string {
  const text = compactActivityValue(value, 120).replace(/\\/g, "/");
  return compactActivityValue(text.split("/").filter(Boolean).at(-1), 28);
}

function runningToolActivity(record: ConversationToolRecord): string {
  const name = record.name.toLowerCase();
  const args = toolArgumentRecord(record.argsSummary || "");
  const command = compactActivityValue(
    args.command ?? args.cmd ?? args.shell_command,
    500,
  );
  const path = activityBasename(
    args.path ?? args.file_path ?? args.filePath ?? args.filename ?? args.cwd,
  );
  const query = compactActivityValue(
    args.query ?? args.pattern ?? args.search ?? args.q,
    28,
  );

  const mutekiActivity: Record<string, string> = {
    muteki_list_projects: "正在查询 Muteki 项目",
    muteki_list_threads: "正在查询 Muteki 对话",
    muteki_list_tasks: "正在查询单题任务",
    muteki_get_task: "正在读取单题任务",
    muteki_list_runs: "正在查询 Run 列表",
    muteki_get_command_receipt: "正在核对控制命令结果",
    muteki_create_task: "正在创建单题任务",
    muteki_dispatch_challenge: "正在下发 CTF 单题任务",
    muteki_create_run: "正在创建 Run",
    muteki_start_swarm: "正在启动 Coordinator",
    muteki_resolve_run: "正在汇总 Run 结果",
    muteki_get_run_snapshot: "正在读取 Run 快照",
    muteki_read_run_events: "正在读取 Run 事件",
    muteki_wait_run: "正在等待 Run 进展",
    muteki_read_shared_graph: "正在读取 Run 共享图",
    muteki_pause_run: "正在暂停 Run",
    muteki_resume_run: "正在恢复 Run",
    muteki_stop_run: "正在停止 Run",
    muteki_send_operator_directive: "正在发送 Operator 指令",
    muteki_add_run_context: "正在注入 Run 上下文",
    muteki_spawn_run_worker: "正在增加 Worker",
    muteki_cancel_run_worker: "正在取消 Worker",
  };
  const mutekiName = Object.keys(mutekiActivity).find((toolName) => (
    name === toolName || name.endsWith(`.${toolName}`) || name.endsWith(`/${toolName}`)
  ));
  if (mutekiName) return mutekiActivity[mutekiName];

  const terminalTool = [
    "terminal", "shell", "bash", "exec", "command", "run_terminal",
  ].some((marker) => name.includes(marker));
  if (terminalTool) {
    if (/(^|[;&|]\s*|\s)(ls|find|fd)(\s|$)|\brg\s+--files\b/.test(command)) {
      return path ? `正在列出 ${path} 中的文件` : "正在列出当前工作目录下的文件";
    }
    if (/(^|[;&|]\s*|\s)(rg|grep|ag)(\s|$)/.test(command)) {
      return "正在搜索工作区";
    }
    if (/(^|[;&|]\s*|\s)(cat|sed|head|tail|less)(\s|$)/.test(command)) {
      return "正在读取文件";
    }
    if (/\bgit\s+(status|diff|show)\b/.test(command)) {
      return "正在检查工作区变更";
    }
    if (/\b(pytest|vitest|jest|pnpm\s+(?:test|lint|build)|npm\s+(?:test|run)|tsc)\b/.test(command)) {
      return "正在运行项目检查";
    }
    return "正在运行命令";
  }
  if (name.includes("list") || name.includes("directory")) {
    return path ? `正在列出 ${path} 中的文件` : "正在列出文件";
  }
  if (name.includes("read") || name.includes("open_file") || name.includes("view_file")) {
    return path ? `正在读取 ${path}` : "正在读取文件";
  }
  if (name.includes("search") || name.includes("grep") || name.includes("find")) {
    return query ? `正在搜索“${query}”` : "正在搜索工作区";
  }
  if (name.includes("edit") || name.includes("write") || name.includes("patch")) {
    return path ? `正在更新 ${path}` : "正在更新文件";
  }
  if (isBrowserTool(record) || name.includes("web")) return "正在查看网页";
  if (name.includes("image") || name.includes("screenshot")) return "正在处理图片";
  return "正在调用工具";
}

function completedToolActivity(label: string): string {
  const exact: Record<string, string> = {
    "正在搜索工作区": "已完成工作区搜索",
    "正在检查工作区变更": "已完成工作区变更检查",
    "正在运行命令": "命令已完成",
    "正在运行项目检查": "项目检查已完成",
    "正在查看网页": "网页查看已完成",
    "正在处理图片": "图片处理已完成",
    "正在调用工具": "工具调用已完成",
    "正在查询 Muteki 项目": "已查询 Muteki 项目",
    "正在查询 Muteki 对话": "已查询 Muteki 对话",
    "正在查询单题任务": "已查询单题任务",
    "正在读取单题任务": "已读取单题任务",
    "正在查询 Run 列表": "已查询 Run 列表",
    "正在核对控制命令结果": "已核对控制命令结果",
    "正在创建单题任务": "已创建单题任务",
    "正在下发 CTF 单题任务": "已下发 CTF 单题任务",
    "正在创建 Run": "已创建 Run",
    "正在启动 Coordinator": "已启动 Coordinator",
    "正在汇总 Run 结果": "已汇总 Run 结果",
    "正在读取 Run 快照": "已读取 Run 快照",
    "正在读取 Run 事件": "已读取 Run 事件",
    "正在等待 Run 进展": "Run 等待已结束",
    "正在读取 Run 共享图": "已读取 Run 共享图",
    "正在暂停 Run": "Run 已暂停",
    "正在恢复 Run": "Run 已恢复",
    "正在停止 Run": "Run 已停止",
    "正在发送 Operator 指令": "已发送 Operator 指令",
    "正在注入 Run 上下文": "已注入 Run 上下文",
    "正在增加 Worker": "已增加 Worker",
    "正在取消 Worker": "已取消 Worker",
  };
  if (exact[label]) return `${exact[label]}，正在整理回复…`;
  const replacements: Array<[string, string]> = [
    ["正在查询", "已查询"],
    ["正在列出", "已列出"],
    ["正在读取", "已读取"],
    ["正在搜索", "已完成搜索"],
    ["正在检查", "已完成检查"],
    ["正在运行", "已完成运行"],
    ["正在更新", "已更新"],
    ["正在查看", "已查看"],
    ["正在处理", "已处理"],
    ["正在调用", "已完成"],
    ["正在创建", "已创建"],
    ["正在下发", "已下发"],
    ["正在启动", "已启动"],
    ["正在汇总", "已汇总"],
    ["正在等待", "等待已结束："],
    ["正在暂停", "已暂停"],
    ["正在恢复", "已恢复"],
    ["正在停止", "已停止"],
    ["正在发送", "已发送"],
    ["正在注入", "已注入"],
    ["正在增加", "已增加"],
    ["正在取消", "已取消"],
  ];
  const match = replacements.find(([prefix]) => label.startsWith(prefix));
  const completed = match
    ? `${match[1]}${label.slice(match[0].length)}`
    : "工具调用已完成";
  return `${completed}，正在整理回复…`;
}

/** Resolve a truthful live label from normalized Conversation events. */
export function resolveConversationLiveActivity(
  segments: ConversationTurnSegment[],
): ConversationLiveActivity {
  const last = segments.at(-1);
  if (!last) return { kind: "idle", label: "Agent 正在处理…" };
  if (last.kind === "thinking") {
    return { kind: "reasoning", label: "Agent 正在思考…" };
  }
  if (last.kind === "text") {
    return { kind: "response", label: "Agent 正在生成回复…" };
  }
  const tool = last.tools.at(-1);
  if (!tool) return { kind: "idle", label: "Agent 正在处理…" };
  const runningLabel = runningToolActivity(tool);
  if (tool.status === "running" || tool.status === "pending") {
    return { kind: "tool", label: runningLabel };
  }
  if (tool.status === "declined") {
    return { kind: "response", label: "工具调用已拒绝" };
  }
  if (tool.status === "cancelled") {
    return { kind: "response", label: "工具调用已取消" };
  }
  if (tool.status === "failed") {
    return { kind: "response", label: "工具调用失败" };
  }
  return { kind: "response", label: completedToolActivity(runningLabel) };
}

function emptyTurnFoldState(): ConversationTurnFoldState {
  return { segmentsByTurn: {}, textFromDeltas: {} };
}

function applyConversationEventToFold(
  state: ConversationTurnFoldState,
  event: ConversationEvent,
): ConversationTurnFoldState {
  const payload = event.payload || {};
  const type = event.event_type || "";
  if (TURN_SUPERSESSION_EVENT_TYPES.has(type)) {
    const superseded = new Set(
      (Array.isArray(payload.superseded_turn_ids)
        ? payload.superseded_turn_ids
        : [])
        .map(String)
        .filter(Boolean),
    );
    if (!superseded.size) return state;
    return {
      segmentsByTurn: Object.fromEntries(
        Object.entries(state.segmentsByTurn).filter(
          ([turnId]) => !superseded.has(turnId),
        ),
      ),
      textFromDeltas: Object.fromEntries(
        Object.entries(state.textFromDeltas).filter(
          ([turnId]) => !superseded.has(turnId),
        ),
      ),
    };
  }
  const turnId = String(payload.turn_id || "");
  if (!turnId) return state;

  const segments = state.segmentsByTurn[turnId] || [];
  let nextSegments = segments;
  let textFromDeltas = Boolean(state.textFromDeltas[turnId]);
  let changed = false;

  if (
    type === "core.message.delta"
    && payload.thinking !== true
    && assistantRole(payload.role)
  ) {
    const text = String(payload.text || "");
    if (!text) return state;
    nextSegments = appendTurnText(
      segments,
      turnId,
      "text",
      text,
      undefined,
      messagePhase(payload),
    );
    textFromDeltas = true;
    changed = true;
  } else if (type === "core.reasoning.summary") {
    const text = String(payload.reasoning_summary || payload.text || "");
    const durationMs = payload.duration_ms ? Number(payload.duration_ms) : undefined;
    if (!text && durationMs == null) return state;
    nextSegments = appendTurnText(segments, turnId, "thinking", text, durationMs);
    textFromDeltas = false;
    changed = true;
  } else if (isConversationToolEvent(type)) {
    const record = mergeConversationTool(event, findToolRecord(segments, conversationToolKey(payload, turnId)));
    nextSegments = upsertToolSegment(segments, turnId, record);
    textFromDeltas = false;
    changed = true;
  } else if (TURN_FINISH_EVENT_TYPES.has(type)) {
    const terminal = turnTerminalFromEventType(type);
    if (terminal) {
      nextSegments = settleTurnSegmentsAgainstStatus(segments, terminal);
      if (nextSegments !== segments) {
        textFromDeltas = false;
        changed = true;
      }
    }
  } else if (type === "core.message.completed" && assistantRole(payload.role)) {
    const text = String(payload.text || "");
    if (!text.trim()) return state;
    const phase = messagePhase(payload);
    const hasMatchingText = segments.some((segment) => (
      segment.kind === "text"
      && segment.phase === phase
      && segment.text.trim()
    ));
    if (hasMatchingText && textFromDeltas) {
      if (!state.textFromDeltas[turnId]) return state;
      return {
        ...state,
        textFromDeltas: { ...state.textFromDeltas, [turnId]: false },
      };
    }
    nextSegments = appendTurnText(
      segments,
      turnId,
      "text",
      text,
      undefined,
      phase,
    );
    textFromDeltas = false;
    changed = true;
  }

  if (!changed) return state;
  return {
    segmentsByTurn: { ...state.segmentsByTurn, [turnId]: nextSegments },
    textFromDeltas: { ...state.textFromDeltas, [turnId]: textFromDeltas },
  };
}

export function buildConversationTurnFold(
  events: ConversationEvent[] = [],
): ConversationTurnFoldState {
  return events.reduce(applyConversationEventToFold, emptyTurnFoldState());
}

/**
 * Separate the user-facing terminal answer from the auditable work process.
 * Explicit Runtime phases win. For older event streams without phases, only
 * the final contiguous text block of a completed turn is treated as the answer.
 * Interrupted and failed turns keep every emitted text block visible when no
 * explicit final answer exists: those blocks are the durable partial response
 * received before termination, including Runtime commentary emitted mid-turn.
 */
export function presentConversationTurn(
  segments: ConversationTurnSegment[],
  status: string,
): ConversationTurnPresentation {
  const explicitAnswerIndexes = new Set<number>();
  segments.forEach((segment, index) => {
    if (segment.kind === "text" && segment.phase === "final_answer") {
      explicitAnswerIndexes.add(index);
    }
  });

  if (!explicitAnswerIndexes.size && status === "completed") {
    for (let index = segments.length - 1; index >= 0; index -= 1) {
      const segment = segments[index];
      if (segment.kind !== "text" || segment.phase === "commentary") break;
      explicitAnswerIndexes.add(index);
    }
  } else if (
    !explicitAnswerIndexes.size
    && (status === "failed" || status === "interrupted")
  ) {
    segments.forEach((segment, index) => {
      if (segment.kind === "text") {
        explicitAnswerIndexes.add(index);
      }
    });
  }

  const answerText = segments
    .filter((_, index) => explicitAnswerIndexes.has(index))
    .map((segment) => (segment.kind === "text" ? segment.text : ""))
    .join("");
  const activity = segments.filter((_, index) => !explicitAnswerIndexes.has(index));
  return { activity, answerText };
}

export function collectConversationTools(
  events: ConversationEvent[] = [],
  turnStatusById?: Record<string, string> | Map<string, string> | null,
): ConversationToolRecord[] {
  const tools = new Map<string, ConversationToolRecord>();
  const turnTerminal = new Map<string, ConversationTurnTerminalStatus>();

  const statusLookup = (turnId: string): string | undefined => {
    if (!turnStatusById) return undefined;
    if (turnStatusById instanceof Map) return turnStatusById.get(turnId);
    return turnStatusById[turnId];
  };

  for (const event of events) {
    const type = event.event_type || "";
    const payload = event.payload || {};
    const turnId = String(payload.turn_id || "");

    if (TURN_SUPERSESSION_EVENT_TYPES.has(type)) {
      const superseded = new Set(
        (Array.isArray(payload.superseded_turn_ids)
          ? payload.superseded_turn_ids
          : [])
          .map(String)
          .filter(Boolean),
      );
      if (superseded.size) {
        for (const key of [...tools.keys()]) {
          const tool = tools.get(key);
          if (tool?.turnId && superseded.has(tool.turnId)) tools.delete(key);
        }
        for (const id of superseded) turnTerminal.delete(id);
      }
      continue;
    }

    const finish = turnTerminalFromEventType(type);
    if (finish && turnId) {
      turnTerminal.set(turnId, finish);
      for (const [key, tool] of tools) {
        if (tool.turnId !== turnId) continue;
        tools.set(key, settleConversationToolForTurn(tool, finish));
      }
      continue;
    }

    if (!isConversationToolEvent(type)) continue;
    const callKey = conversationToolKey(payload, turnId);
    let record = mergeConversationTool(event, tools.get(callKey));
    const terminal = turnTerminal.get(turnId)
      || normalizeTurnTerminalStatus(statusLookup(turnId) || "")
      || statusLookup(turnId);
    if (terminal) {
      record = settleConversationToolForTurn(record, terminal);
    }
    tools.set(callKey, record);
  }

  // Durable turn.status from the view covers truncated finish events.
  if (turnStatusById) {
    for (const [key, tool] of tools) {
      if (!tool.turnId || !isOpenToolStatus(tool.status)) continue;
      const status = normalizeTurnTerminalStatus(statusLookup(tool.turnId) || "")
        || statusLookup(tool.turnId);
      if (!status) continue;
      tools.set(key, settleConversationToolForTurn(tool, status));
    }
  }

  return Array.from(tools.values());
}

export function isBrowserTool(record: ConversationToolRecord): boolean {
  const name = record.name.toLowerCase();
  return [
    "browser",
    "playwright",
    "chrome",
    "webfetch",
    "web_fetch",
    "websearch",
    "web_search",
  ].some((marker) => name.includes(marker));
}
