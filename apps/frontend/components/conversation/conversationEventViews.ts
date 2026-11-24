import type { ConversationEvent } from "@/lib/useConversation";
import type { ToolItem } from "@muteki/ui/agent";
import type { ToolFile } from "@/lib/resourcePreview";
import type { ConversationLiveTextRun } from "@/lib/conversationStreamReducer";
import { toolActivityRows, toolActivityText, type ToolCommandAction } from "../chat/timeline/toolPresentation";

export interface ConversationToolRecord extends ToolItem {
  turnId?: string;
  occurredAt?: string;
  parentId?: string | null;
  isAgent?: boolean;
  agentId?: string | null;
  model?: string | null;
  messageId?: string | null;
  commandActions?: ToolCommandAction[];
  mcpServer?: string;
  resources?: ToolFile[];
}

export type ConversationMessagePhase = "commentary" | "final_answer" | "unknown";

const serializedEventValues = new WeakMap<object, string>();

function serializeEventValue(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  if (typeof value === "object") {
    const cached = serializedEventValues.get(value);
    if (cached !== undefined) return cached;
  }
  let text: string;
  try {
    text = JSON.stringify(value);
  } catch {
    text = String(value);
  }
  if (typeof value === "object") serializedEventValues.set(value, text);
  return text;
}

export function conversationErrorMessage(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value.trim();
  if (Array.isArray(value)) {
    return value.map(conversationErrorMessage).filter(Boolean).join("；");
  }
  if (typeof value === "object") {
    const row = value as Record<string, unknown>;
    for (const key of ["message", "detail", "errorMessage", "error"]) {
      const text = conversationErrorMessage(row[key]);
      if (text) return text;
    }
    const code = String(row.code || "").trim();
    const operation = String(row.operation || "").trim();
    const capability = String(row.capability || "").trim();
    if (code === "external_agent.capability.unsupported" || row.reason === "capability.unsupported") {
      const label = capability || operation || "所需功能";
      return `当前 Agent 接入不支持 ${label}；Muteki 将使用已保存的对话历史重新建立会话`;
    }
    const reason = conversationErrorMessage(row.reason);
    if (reason) return reason;
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
  | { kind: "thinking"; turnId: string; text: string; durationMs?: number; itemId?: string; partial?: boolean }
  | {
      kind: "text";
      turnId: string;
      text: string;
      phase: ConversationMessagePhase;
      spans?: ConversationTextSpan[];
    }
  | { kind: "tools"; turnId: string; tools: ConversationToolRecord[] }
  | {
      kind: "compaction";
      turnId: string;
      status: "started" | "completed" | "failed" | "interrupted";
      trigger: "manual" | "auto";
      confirmed: boolean;
      summary?: string;
      message?: string;
      tokensBefore?: number;
      tokensAfter?: number;
    };

export interface ConversationTextSpan {
  messageId: string;
  start: number;
  end: number;
  sourceStart?: number;
}

function compactionSegment(turnId: string, payload: Record<string, unknown>): ConversationTurnSegment | null {
  const status = String(payload.status || "");
  if (status !== "started" && status !== "completed" && status !== "failed" && status !== "interrupted") return null;
  const count = (value: unknown) => (typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : undefined);
  return {
    kind: "compaction",
    turnId,
    status,
    trigger: payload.trigger === "auto" ? "auto" : "manual",
    confirmed: payload.confirmed !== false,
    summary: typeof payload.summary === "string" && payload.summary.trim() ? payload.summary : undefined,
    message: typeof payload.message === "string" && payload.message.trim() ? payload.message : undefined,
    tokensBefore: count(payload.tokens_before),
    tokensAfter: count(payload.tokens_after),
  };
}

export function restoreConversationStreamPrefix(
  segments: ConversationTurnSegment[], fullText: string,
  runs: ConversationLiveTextRun[], turnId: string,
): ConversationTurnSegment[] {
  if (fullText.length === 0) return segments;
  let eventLength = 0;
  for (const segment of segments) {
    if (segment.kind === "text") eventLength += segment.text.length;
  }
  const prefixEnd = fullText.length - eventLength;
  if (prefixEnd <= 0) return segments;
  let eventOffset = prefixEnd;
  for (const segment of segments) {
    if (segment.kind !== "text") continue;
    if (!fullText.startsWith(segment.text, eventOffset)) return segments;
    eventOffset += segment.text.length;
  }
  const prefix: ConversationTurnSegment[] = [];
  let covered = 0;
  for (const run of runs) {
    if (run.start >= prefixEnd) break;
    if (run.start !== covered || run.end <= run.start) break;
    const end = Math.min(run.end, prefixEnd);
    prefix.push({ kind: "text", turnId, text: fullText.slice(run.start, end), phase: run.phase });
    covered = end;
    if (covered === prefixEnd) break;
  }
  if (covered < prefixEnd) prefix.push({ kind: "text", turnId, text: fullText.slice(covered, prefixEnd), phase: "unknown" });
  return [...prefix, ...segments];
}

export interface ConversationTurnPresentation {
  activity: ConversationTurnSegment[];
  answerText: string;
  answerSpans: ConversationTextSpan[];
}

export interface ConversationLiveActivity {
  kind: "idle" | "tool" | "reasoning" | "response";
  label: string;
}

export interface ConversationTurnFoldState {
  segmentsByTurn: Record<string, ConversationTurnSegment[]>;
  textFromDeltas: Record<string, boolean>;
  agentToolsByTurn: Record<string, Record<string, ConversationToolRecord[]>>;
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

export function buildConversationTurnRuntimeMap(
  events: ConversationEvent[] = [],
): Record<string, ConversationTurnRuntime> {
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

const TURN_SUPERSESSION_EVENT_TYPES = new Set([
  "core.turn.retried",
  "core.turn.edit_resent",
  "core.turn.rewound",
]);

export interface ConversationTurnTiming {
  startedAt?: string;
  finishedAt?: string;
  durationMs?: number;
  toolMs?: number;
  wallMs?: number;
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

    if (type === "core.turn.timing") {
      const toolMs = Number(payload.tool_ms);
      const wallMs = Number(payload.wall_ms);
      byTurn[turnId] = {
        ...current,
        ...(Number.isFinite(toolMs) ? { toolMs } : {}),
        ...(Number.isFinite(wallMs) ? { wallMs } : {}),
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

export function buildConversationTurnRetryMap(
  events: ConversationEvent[] = [],
): Record<string, string> {
  const byTurn: Record<string, string> = {};
  for (const event of [...events].sort((left, right) => left.seq - right.seq)) {
    if (event.event_type !== "core.turn.retry_scheduled") continue;
    const payload = event.payload || {};
    const turnId = String(payload.turn_id || "");
    if (!turnId) continue;
    const retryAt = typeof payload.retry_at === "string" ? payload.retry_at : "";
    if (retryAt) byTurn[turnId] = retryAt;
    else delete byTurn[turnId];
  }
  return byTurn;
}

export function buildConversationTurnChangesMap(
  events: ConversationEvent[] = [],
): Record<string, { completeness?: string }> {
  const byTurn: Record<string, { completeness?: string }> = {};
  for (const event of [...events].sort((left, right) => left.seq - right.seq)) {
    if (event.event_type !== "core.turn.changes_updated") continue;
    const payload = event.payload || {};
    const turnId = String(payload.turn_id || "");
    if (!turnId) continue;
    const completeness = typeof payload.completeness === "string" ? payload.completeness : undefined;
    byTurn[turnId] = { completeness };
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

export function messagePhase(payload: Record<string, unknown>): ConversationMessagePhase {
  const value = String(
    payload.phase || payload.message_phase || payload.channel || "",
  ).toLowerCase();
  if (value === "commentary" || value === "partial_answer") return "commentary";
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

export type ConversationTurnTerminalStatus = "completed" | "failed" | "interrupted" | "cancelled";

function turnTerminalFromEventType(type: string, status?: unknown): ConversationTurnTerminalStatus | null {
  if (type === "core.turn.completed") return "completed";
  if (type === "core.turn.failed") return "failed";
  if (type === "core.turn.interrupted") return status === "cancelled" ? "cancelled" : "interrupted";
  return null;
}

function isOpenToolStatus(status: ConversationToolRecord["status"]): boolean {
  return status === "running" || status === "pending";
}

function normalizeToolPayloadStatus(raw: unknown): string {
  return String(raw ?? "").trim().toLowerCase();
}

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

  if (previous?.status === "declined") return "declined";
  if (["completed", "success", "ok", "succeeded"].includes(payloadStatus)) {
    return "completed";
  }

  if (typeCompleted) {
    return "completed";
  }

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
    const error = conversationErrorMessage(payload.error);
    if (error) return error;
    if (failedHint) {
      const output = serializeEventValue(payload.output || payload.result);
      if (output.trimStart().startsWith("{")) {
        try {
          const result = JSON.parse(output);
          if (result?.ok === false) {
            const message = conversationErrorMessage(result.error);
            if (message) return message;
          }
        } catch {
          return output;
        }
      }
      return output || "工具执行失败";
    }
    return previous?.error || "工具执行失败";
  }
  if (previous?.status === "cancelled") return previous.error || "已中断";
  return previous?.error;
}

export function normalizeTurnTerminalStatus(
  turnStatus: string,
): ConversationTurnTerminalStatus | null {
  const status = String(turnStatus || "").trim().toLowerCase();
  if (status === "completed") return "completed";
  if (status === "failed") return "failed";
  if (status === "cancelled" || status === "canceled") return "cancelled";
  if (
    status === "interrupted"
    || status === "aborted"
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
  if (tool.status === "declined") {
    return tool.error ? tool : { ...tool, error: "已拒绝 · 未执行" };
  }
  if (normalized === "interrupted" || normalized === "cancelled") {
    const label = normalized === "cancelled" ? "已取消" : "已中断";
    if (tool.status === "cancelled") {
      return tool.error ? tool : { ...tool, error: label };
    }
    if (isOpenToolStatus(tool.status) || tool.status === "failed") {
      return {
        ...tool,
        status: "cancelled",
        error: tool.status === "failed" ? label : (tool.error || label),
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
  const status = resolveToolOutcomeStatus(payload, type, previous);
  const suppliedOutput = payload.output ?? payload.result ?? payload.summary;
  const native = payload.native && typeof payload.native === "object"
    ? payload.native as Record<string, unknown>
    : {};
  const commandActions = Array.isArray(native.command_actions)
    ? (native.command_actions as unknown[]).filter((action): action is ToolCommandAction => (
      Boolean(action) && typeof action === "object" && typeof (action as ToolCommandAction).type === "string"
    ))
    : previous?.commandActions;
  const mcpServer = String(native.mcp_server || "").trim() || previous?.mcpServer;
  const resources = Array.isArray(payload.resources) ? payload.resources as ToolFile[] : undefined;
  const previousMedia = previous?.resources?.filter((file) => file.kind === "recorded");
  const retainedMedia = resources?.some((file) => file.kind === "recorded") ? [] : previousMedia;
  const files = resources ? [...(retainedMedia ?? []), ...resources] : previous?.resources;
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
      suppliedOutput != null ? serializeEventValue(suppliedOutput)
        : typeof payload.chunk === "string" ? `${previous?.outputSummary || ""}${payload.chunk}`
        : previous?.outputSummary || "",
    error: toolOutcomeError(status, payload, previous, failedHint),
    turnId: turnId || previous?.turnId,
    occurredAt: previous?.occurredAt || event.occurred_at,
    parentId,
    isAgent: explicitAgent ? true : typeof payload.is_agent === "boolean" ? payload.is_agent
      : typeof payload.isAgent === "boolean" ? payload.isAgent : previous?.isAgent,
    title: String(payload.title || previous?.title || "").trim() || undefined,
    kind: String(payload.kind || previous?.kind || "").trim() || undefined,
    outputUnavailable: typeof native.output_unavailable === "boolean"
      ? native.output_unavailable : previous?.outputUnavailable,
    agentId: String(payload.agent_id || "").trim() || previous?.agentId || null,
    model: String(payload.model || previous?.model || "").trim() || previous?.model || null,
    messageId: String(payload.message_id || previous?.messageId || "").trim()
      || previous?.messageId
      || null,
    ...(commandActions?.length ? { commandActions } : {}),
    ...(mcpServer ? { mcpServer } : {}),
    ...(files?.length ? { resources: files } : {}),
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
  messageId?: string,
): ConversationTurnSegment[] {
  const last = segments[segments.length - 1];
  let sourceStart = 0;
  if (messageId && !(last?.kind === "text" && last.phase === phase && last.spans?.at(-1)?.messageId === messageId)) {
    for (const segment of segments) {
      if (segment.kind !== "text") continue;
      for (const span of segment.spans || []) {
        if (span.messageId === messageId) sourceStart += Array.from(segment.text.slice(span.start, span.end)).length;
      }
    }
  }
  if (last?.kind === "text" && kind === "text" && last.phase === phase) {
    return [...segments.slice(0, -1), {
      ...last,
      text: `${last.text}${text}`,
      ...(messageId || last.spans
        ? { spans: extendSpans(last.spans, messageId, last.text.length, text.length, sourceStart) }
        : {}),
    }];
  }
  if (last?.kind === "thinking" && kind === "thinking") {
    return [...segments.slice(0, -1), {
      ...last,
      text: `${last.text}${text}`,
      ...(durationMs ? { durationMs } : {}),
    }];
  }
  if (kind === "thinking") {
    return [...segments, { kind, turnId, text, ...(durationMs ? { durationMs } : {}) }];
  }
  return [...segments, {
    kind, turnId, text, phase,
    ...(messageId ? { spans: [{ messageId, start: 0, end: text.length, sourceStart }] } : {}),
  }];
}

function extendSpans(
  spans: ConversationTextSpan[] | undefined,
  messageId: string | undefined,
  start: number,
  length: number,
  sourceStart: number,
): ConversationTextSpan[] {
  const list = spans ? [...spans] : [];
  if (!messageId) return list;
  const last = list.at(-1);
  if (last?.messageId === messageId && last.end === start) {
    list[list.length - 1] = { ...last, end: start + length };
  } else {
    list.push({ messageId, start, end: start + length, sourceStart });
  }
  return list;
}

function replaceTurnMessage(
  segments: ConversationTurnSegment[],
  messageId: string,
  text: string,
): ConversationTurnSegment[] | null {
  let placed = false;
  const next: ConversationTurnSegment[] = [];
  for (const segment of segments) {
    if (segment.kind !== "text" || !segment.spans?.some((span) => span.messageId === messageId)) {
      next.push(segment);
      continue;
    }
    let rebuilt = "";
    let covered = 0;
    const spans: ConversationTextSpan[] = [];
    for (const span of segment.spans) {
      rebuilt += segment.text.slice(covered, span.start);
      covered = span.end;
      const piece = span.messageId !== messageId ? segment.text.slice(span.start, span.end)
        : placed ? null : text;
      if (piece === null) continue;
      if (span.messageId === messageId) placed = true;
      spans.push({ ...span, start: rebuilt.length, end: rebuilt.length + piece.length,
        ...(span.messageId === messageId ? { sourceStart: 0 } : {}) });
      rebuilt += piece;
    }
    rebuilt += segment.text.slice(covered);
    if (rebuilt) next.push({ ...segment, text: rebuilt, spans });
  }
  return placed ? next : null;
}

export function resolveConversationLiveActivity(
  segments: ConversationTurnSegment[],
): ConversationLiveActivity {
  const last = segments.at(-1);
  if (!last) return { kind: "idle", label: "正在思考" };
  if (last.kind === "thinking") return { kind: "reasoning", label: "正在思考" };
  if (last.kind === "text") return { kind: "response", label: "正在生成回复" };
  if (last.kind === "compaction") {
    return last.status === "started"
      ? { kind: "tool", label: "正在压缩上下文" }
      : { kind: "reasoning", label: "正在思考" };
  }
  const tool = last.tools.at(-1);
  if (tool && (tool.status === "running" || tool.status === "pending")) {
    const row = toolActivityRows(tool).at(-1);
    if (row) return { kind: "tool", label: toolActivityText(row) };
  }
  return { kind: "reasoning", label: "正在思考" };
}

function emptyTurnFoldState(): ConversationTurnFoldState {
  return { segmentsByTurn: {}, textFromDeltas: {}, agentToolsByTurn: {} };
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
      agentToolsByTurn: Object.fromEntries(
        Object.entries(state.agentToolsByTurn).filter(
          ([turnId]) => !superseded.has(turnId),
        ),
      ),
    };
  }
  const turnId = String(payload.turn_id || "");
  if (!turnId) return state;

  const ownerAgentId = String(payload.agent_id || "").trim();
  if (ownerAgentId && isConversationToolEvent(type)) {
    const byAgent = state.agentToolsByTurn[turnId] || {};
    const list = byAgent[ownerAgentId] || [];
    const key = conversationToolKey(payload, turnId);
    const index = list.findIndex((tool) => tool.id === key);
    const record = mergeConversationTool(event, index >= 0 ? list[index] : undefined);
    const nextList = index >= 0
      ? list.map((tool, i) => (i === index ? record : tool))
      : [...list, record];
    return {
      ...state,
      agentToolsByTurn: {
        ...state.agentToolsByTurn,
        [turnId]: { ...byAgent, [ownerAgentId]: nextList },
      },
    };
  }

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
    const messageId = String(payload.message_id || "").trim() || undefined;
    nextSegments = (messageId && payload.replace === true
      ? replaceTurnMessage(segments, messageId, text)
      : null)
      ?? appendTurnText(
        segments,
        turnId,
        "text",
        text,
        undefined,
        messagePhase(payload),
        messageId,
      );
    textFromDeltas = true;
    changed = true;
  } else if (type === "core.reasoning.summary") {
    const text = String(payload.reasoning_summary || payload.text || "");
    const durationMs = payload.duration_ms != null ? Number(payload.duration_ms) : undefined;
    if (!text && durationMs == null) return state;
    const itemId = String(payload.item_id || "");
    if (itemId) {
      const index = segments.findIndex(segment => segment.kind === "thinking" && segment.turnId === turnId && segment.itemId === itemId);
      const existing = index >= 0 ? segments[index] : undefined;
      const segment: ConversationTurnSegment = {
        kind: "thinking", turnId, itemId, partial: payload.partial === true,
        text: existing?.kind === "thinking" && payload.partial === true ? existing.text + text : text,
        ...(existing?.kind === "thinking" && existing.durationMs != null ? { durationMs: existing.durationMs } : {}),
        ...(durationMs != null ? { durationMs } : {}),
      };
      nextSegments = index < 0 ? [...segments, segment] : segments.map((value, at) => at === index ? segment : value);
    } else {
      nextSegments = appendTurnText(segments, turnId, "thinking", text, durationMs);
    }
    textFromDeltas = false;
    changed = true;
  } else if (isConversationToolEvent(type)) {
    const record = mergeConversationTool(event, findToolRecord(segments, conversationToolKey(payload, turnId)));
    nextSegments = upsertToolSegment(segments, turnId, record);
    textFromDeltas = false;
    changed = true;
  } else if (TURN_FINISH_EVENT_TYPES.has(type)) {
    const terminal = turnTerminalFromEventType(type, payload.status);
    if (terminal) {
      nextSegments = settleTurnSegmentsAgainstStatus(segments, terminal);
      if (nextSegments !== segments) {
        textFromDeltas = false;
        changed = true;
      }
    }
  } else if (type === "core.context.compaction") {
    const segment = compactionSegment(turnId, payload);
    if (!segment || segment.kind !== "compaction") return state;
    const index = segments.findIndex((row) => (
      row.kind === "compaction" && row.status === "started" && row.trigger === segment.trigger
    ));
    nextSegments = index >= 0 && segment.status !== "started"
      ? segments.map((row, at) => (at === index ? segment : row))
      : [...segments, segment];
    textFromDeltas = false;
    changed = true;
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
      String(payload.message_id || "").trim() || undefined,
    );
    textFromDeltas = false;
    changed = true;
  }

  if (!changed) return state;
  return {
    ...state,
    segmentsByTurn: { ...state.segmentsByTurn, [turnId]: nextSegments },
    textFromDeltas: { ...state.textFromDeltas, [turnId]: textFromDeltas },
  };
}

export function buildConversationTurnFold(
  events: ConversationEvent[] = [],
): ConversationTurnFoldState {
  return events.reduce(applyConversationEventToFold, emptyTurnFoldState());
}

export function extendConversationTurnFold(
  state: ConversationTurnFoldState,
  events: ConversationEvent[],
): ConversationTurnFoldState {
  return events.reduce(applyConversationEventToFold, state);
}

export function isAppendedEventList(
  previous: readonly ConversationEvent[],
  next: readonly ConversationEvent[],
): boolean {
  if (next.length < previous.length) return false;
  if (!previous.length) return true;
  return next[0] === previous[0] && next[previous.length - 1] === previous[previous.length - 1];
}

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
    && (status === "failed" || status === "interrupted" || status === "cancelled")
  ) {
    segments.forEach((segment, index) => {
      if (segment.kind === "text") {
        explicitAnswerIndexes.add(index);
      }
    });
  }

  let answerText = "";
  const answerSpans: ConversationTextSpan[] = [];
  segments.forEach((segment, index) => {
    if (!explicitAnswerIndexes.has(index) || segment.kind !== "text") return;
    for (const span of segment.spans || []) answerSpans.push({ ...span, start: answerText.length + span.start, end: answerText.length + span.end });
    answerText += segment.text;
  });
  const activity = segments.filter((_, index) => !explicitAnswerIndexes.has(index));
  return { activity, answerText, answerSpans };
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

    const finish = turnTerminalFromEventType(type, payload.status);
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
