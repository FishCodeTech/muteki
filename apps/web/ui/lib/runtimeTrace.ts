import type { AgentRuntimeFeedEvent, ChatMessage, DeckState } from "./events";
import { isWorkerLane } from "./events";
import { toWorkerIdentity, workerDisplayName } from "./workers";
import { toEpochMs } from "./format";

export type TraceNode = {
  id: string;
  parentId?: string;
  actor: string;
  label: string;
  detail?: string;
  kind: "run" | "agent" | "group" | "tool" | "llm" | "event";
  status: "complete" | "running" | "failed" | "cancelled" | "unknown";
  start: number;
  end?: number;
  interval: boolean;
  /** For a call recorded with a single instant: whether `start` is when it began or when it returned. */
  anchor?: "start" | "end";
  messageId?: string;
  engine?: string;
  source: string;
  sourceIds: string[];
  children: TraceNode[];
};
const string = (value: unknown) => (typeof value === "string" ? value : "");
const time = (value: unknown) =>
  typeof value === "number" ? toEpochMs(value) : 0;
const failed = (p: Record<string, unknown>) =>
  p.is_error === true ||
  p.error != null ||
  p.success === false ||
  (typeof p.exit_code === "number" && p.exit_code !== 0) ||
  ["failed", "error"].includes(string(p.status));
const callId = (p: Record<string, unknown>) =>
  string(p.call_id) || string(p.tool_call_id) || string(p.tool_use_id);
/** Terminal lane reasons, matching the Worker roster's reading of them. */
const LANE_FAILED = new Set(["timeout", "oom", "error", "budget"]);
const LANE_CANCELLED = new Set(["cancelled", "killed"]);
const context = (event: AgentRuntimeFeedEvent) =>
  [
    event.solverId,
    event.payload.execution_generation ?? "",
    event.payload.exec_id ?? "",
    event.payload.agent_session_id ?? "",
    event.payload.turn_id ?? "",
  ].join(":");

/** Preserve recorded boundaries. Chat tool timestamps represent results, not starts. */
export function projectRuntimeTrace(deck: DeckState, running: boolean) {
  const allTimes = [
    ...deck.chat.map((m) => toEpochMs(m.ts)),
    ...deck.runtimeEvents.map((e) => toEpochMs(e.ts)),
  ].filter(Boolean);
  const start = toEpochMs(deck.startedAt) || Math.min(...allTimes, Infinity);
  const safeStart = Number.isFinite(start) ? start : 0;
  const root: TraceNode = {
    id: "trace:run",
    actor: "coordinator",
    label: "协调器",
    kind: "run",
    start: safeStart,
    end: running ? undefined : toEpochMs(deck.finishedAt) || undefined,
    interval: !!safeStart,
    status: running ? "running" : deck.finished ? "complete" : "unknown",
    source: "run lifecycle",
    sourceIds: [deck.runId],
    children: [],
  };
  const agents = new Map<string, TraceNode>();
  const workerIdentities = Object.keys(deck.lanes)
    .filter(isWorkerLane)
    .map((id) => toWorkerIdentity(id, deck.lanes[id]));
  const group = (actor: string) => {
    let node = agents.get(actor);
    if (node) return node;
    const lane = deck.lanes[actor];
    const worker = isWorkerLane(actor) && !!lane;
    const display = worker
      ? workerDisplayName(
          actor,
          toWorkerIdentity(actor, lane),
          workerIdentities,
        )
      : null;
    const begin = time(lane?.runtime?.started_at) || time(lane?.firstSeenAt);
    const end = time(lane?.runtime?.finished_at) || time(lane?.finishedAt);
    const active = running && !!lane?.online && !lane.paused && !end;
    const reason = (lane?.statusReason || lane?.status || "").trim();
    const stopped = !!lane && !active && !lane.online && !lane.solved;
    node = {
      id: `trace:agent:${actor}`,
      parentId: root.id,
      actor,
      label:
        display?.title ||
        (actor === "operator"
          ? "用户输入"
          : actor === "coordinator"
            ? "运行事件"
            : actor === "reason"
              ? "任务规划"
              : actor === "challenger"
                ? "复核记录"
                : "系统事件"),
      engine: worker ? lane?.engine : undefined,
      detail: lane?.model,
      kind: worker ? "agent" : "group",
      start: begin,
      end: end || undefined,
      interval: !!begin && (!!end || active),
      status:
        lane?.runtime?.error ||
        (lane?.runtime?.rc != null && lane.runtime.rc !== 0) ||
        (stopped && LANE_FAILED.has(reason))
          ? "failed"
          : active
            ? "running"
            : stopped && LANE_CANCELLED.has(reason)
              ? "cancelled"
              : end
                ? "complete"
                : "unknown",
      source: "worker lifecycle / semantic group",
      sourceIds: lane?.runtime?.exec_id ? [lane.runtime.exec_id] : [actor],
      children: [],
    };
    agents.set(actor, node);
    root.children.push(node);
    return node;
  };
  Object.keys(deck.lanes).forEach(group);
  // Runtime events are matched to their chat rows through indexes; scanning the chat per event
  // made long runs quadratic.
  const chatById = new Map<string, ChatMessage>();
  const toolByCall = new Map<string, ChatMessage[]>();
  const toolByTs = new Map<string, ChatMessage[]>();
  const push = (map: Map<string, ChatMessage[]>, key: string, message: ChatMessage) => {
    const list = map.get(key);
    if (list) list.push(message);
    else map.set(key, [message]);
  };
  for (const message of deck.chat) {
    chatById.set(message.id, message);
    if (message.kind !== "tool") continue;
    if (message.toolCallId) push(toolByCall, `${message.solverId}:${message.toolCallId}`, message);
    push(toolByTs, `${message.solverId}:${toEpochMs(message.ts)}`, message);
  }
  const linkedChat = new Set<string>();
  const open = new Map<string, TraceNode>();
  const calls = new Map<string, TraceNode>();
  const parentReferences: { node: TraceNode; key: string }[] = [];
  for (const event of deck.runtimeEvents) {
    const p = event.payload;
    const id = callId(p);
    const isTool =
      event.eventType === "tool.started" ||
      event.eventType === "tool.completed";
    if (!isTool) continue;
    const key = `${context(event)}:${id}`;
    const endEvent = event.eventType === "tool.completed";
    const prior = id && endEvent ? open.get(key) : undefined;
    const ts = toEpochMs(event.ts);
    const name = string(p.tool) || string(p.name) || "工具调用";
    const byTime = (m: ChatMessage) => !linkedChat.has(m.id) && (endEvent || !!m.toolPending);
    const sameTs = toolByTs.get(`${event.solverId}:${ts}`) ?? [];
    const matches = id
      ? [
          ...(toolByCall.get(`${event.solverId}:${id}`) ?? []).filter(
            (m) =>
              !linkedChat.has(m.id) &&
              (toEpochMs(m.toolStartedAt ?? 0) === ts || toEpochMs(m.toolEndedAt ?? 0) === ts),
          ),
          ...sameTs.filter((m) => !m.toolCallId && byTime(m)),
        ]
      : sameTs.filter(byTime);
    const message = matches.length === 1 ? matches[0] : undefined;
    if (message) linkedChat.add(message.id);
    if (prior) {
      if (ts >= prior.start) {
        prior.end = ts;
        prior.interval = true;
      }
      prior.status = failed(p) || message?.toolFailed ? "failed" : "complete";
      prior.sourceIds.push(event.id);
      prior.messageId = message?.id || prior.messageId;
      prior.detail = string(p.output) || string(p.error) || prior.detail;
      open.delete(key);
      continue;
    }
    const lane = deck.lanes[event.solverId];
    const active =
      !endEvent &&
      !!id &&
      running &&
      (p.execution_generation == null ||
        p.execution_generation === deck.executionGeneration) &&
      !!message?.toolPending &&
      !!lane?.online &&
      !lane.paused &&
      !time(lane.finishedAt);
    const node: TraceNode = {
      id: `trace:runtime:${event.id}`,
      parentId: group(event.solverId).id,
      actor: event.solverId,
      label: name,
      kind: "tool",
      start: ts,
      interval: active,
      status: endEvent
        ? failed(p)
          ? "failed"
          : "complete"
        : active
          ? "running"
          : "unknown",
      anchor: endEvent ? "end" : "start",
      messageId: message?.id,
      detail: string(p.input) || string(p.output) || string(p.error),
      source: "agent.runtime",
      sourceIds: [event.id],
      children: [],
    };
    group(event.solverId).children.push(node);
    if (id && !endEvent) {
      open.set(key, node);
      calls.set(key, node);
    }
    const parent =
      string(p.parent_tool_use_id) || string(p.parent_tool_call_id);
    if (parent)
      parentReferences.push({ node, key: `${context(event)}:${parent}` });
  }
  for (const node of groupNodes(agents)) {
    const message = node.messageId ? chatById.get(node.messageId) : undefined;
    if (message?.toolStartedAt && message.toolEndedAt) {
      node.start = toEpochMs(message.toolStartedAt);
      node.end = toEpochMs(message.toolEndedAt);
      node.interval = node.end >= node.start;
    }
  }
  // Only explicit parent call IDs establish nesting; solver groups are semantic.
  for (const { node, key } of parentReferences) {
    const parent = calls.get(key);
    if (!parent || parent === node || parent.start > node.start) continue;
    // Guard malformed cycles from external adapters.
    let ancestor: TraceNode | undefined = parent;
    const visited = new Set<string>([node.id]);
    while (ancestor && !visited.has(ancestor.id)) {
      visited.add(ancestor.id);
      ancestor = calls.get(
        parentReferences.find((r) => r.node.id === ancestor?.id)?.key || "",
      );
    }
    if (ancestor) continue;
    const siblings = group(node.actor).children;
    const index = siblings.indexOf(node);
    if (index >= 0) siblings.splice(index, 1);
    node.parentId = parent.id;
    parent.children.push(node);
  }
  for (const event of deck.runtimeEvents) {
    if (
      !["runtime.error", "runtime.warning", "turn.failed"].includes(
        event.eventType,
      )
    )
      continue;
    group(event.solverId).children.push({
      id: `trace:runtime:${event.id}`,
      parentId: group(event.solverId).id,
      actor: event.solverId,
      label: event.eventType,
      detail: string(event.payload.message) || string(event.payload.error),
      kind: "event",
      status: event.eventType === "runtime.warning" ? "unknown" : "failed",
      start: toEpochMs(event.ts),
      interval: false,
      source: "agent.runtime",
      sourceIds: [event.id],
      children: [],
    });
  }
  for (const message of deck.chat) {
    if (linkedChat.has(message.id)) continue;
    const actor =
      message.solverId ||
      (message.role === "human" ? "operator" : "coordinator");
    const begin = toEpochMs(message.toolStartedAt ?? 0);
    const end = toEpochMs(message.toolEndedAt ?? 0);
    const lane = deck.lanes[actor];
    const active =
      message.kind === "tool" &&
      !!begin &&
      !!message.toolPending &&
      running &&
      (message.executionGeneration == null ||
        message.executionGeneration === deck.executionGeneration) &&
      !!lane?.online &&
      !lane.paused;
    const measured =
      message.kind === "tool" && !!begin && ((end >= begin && !!end) || active);
    const node: TraceNode = {
      id: `trace:chat:${message.id}`,
      parentId: group(actor).id,
      actor,
      label: chatLabel(message),
      kind: message.kind === "tool" ? "tool" : "event",
      status: message.toolFailed
        ? "failed"
        : message.toolPending
          ? active
            ? "running"
            : "unknown"
          : "complete",
      start: begin || toEpochMs(message.ts),
      end: end || undefined,
      interval: measured,
      anchor: message.kind === "tool" && !measured ? (begin ? "start" : "end") : undefined,
      messageId: message.id,
      detail: message.content,
      source: message.toolTimingSource
        ? `chat · ${message.toolTimingSource}`
        : "chat",
      sourceIds: [
        message.id,
        ...(message.toolCallId ? [message.toolCallId] : []),
      ],
      children: [],
    };
    group(actor).children.push(node);
  }
  deck.blackboard.reasonRuns.forEach((round, index) => {
    group("reason").children.push({
      id: `trace:reason:${index}:${round.ts}`,
      parentId: group("reason").id,
      actor: "reason",
      label: `规划结果 · ${round.proposed} 个意图`,
      kind: "llm",
      status: round.plannerFailure ? "failed" : "complete",
      start: toEpochMs(round.ts),
      interval: false,
      detail:
        round.plannerFailure ||
        `${round.attempts.length} 次尝试；未记录单次模型调用起止时间`,
      source: "blackboard.reasonRuns",
      sourceIds: [`reason:${index}:${round.ts}`],
      children: [],
    });
  });
  const nodes: TraceNode[] = [];
  const visit = (node: TraceNode) => {
    node.children.sort(
      (a, b) =>
        (node.kind === "run"
          ? Number(b.kind === "agent") - Number(a.kind === "agent")
          : 0) || a.start - b.start,
    );
    if (!node.start)
      node.start = node.children.find((c) => c.start)?.start || safeStart;
    nodes.push(node);
    node.children.forEach(visit);
  };
  visit(root);
  let longest: TraceNode | undefined;
  for (const node of nodes)
    if (
      (node.kind === "tool" || node.kind === "llm") &&
      node.interval &&
      node.end != null &&
      node.status !== "running" &&
      (!longest || node.end - node.start > longest.end! - longest.start)
    )
      longest = node;
  const unpriced = Object.values(deck.costBySolver).some(
    (c) => c.unpricedCalls != null,
  )
    ? Object.values(deck.costBySolver).reduce(
        (n, c) => n + (c.unpricedCalls ?? 0),
        0,
      )
    : undefined;
  return {
    root,
    nodes,
    start: safeStart,
    last: Math.max(
      safeStart,
      ...allTimes,
      ...nodes.map((n) => n.end || n.start),
    ),
    longestId: longest?.id,
    agents: [...agents.values()].filter((n) => n.kind === "agent").length,
    tools: nodes.filter((n) => n.kind === "tool").length,
    unpriced,
    gaps: nodes.filter(
      (n) => (n.kind === "tool" || n.kind === "llm") && !n.interval,
    ).length,
  };
}
function chatLabel(message: ChatMessage) {
  if (message.kind === "tool")
    return message.content.split("\n")[0].slice(0, 90) || "工具调用";
  return {
    reasoning: "推理记录",
    text: "输出记录",
    insight: "发现记录",
    guidance: "指导",
    flag: "证据已记录",
    status: "状态更新",
    progress: "进展摘要",
  }[message.kind];
}
export function traceDuration(node: TraceNode, now: number) {
  return node.interval && (node.end != null || node.status === "running")
    ? Math.max(0, (node.end ?? now) - node.start)
    : null;
}
export function roundedTraceRange(ms: number) {
  const rough = Math.max(1000, ms) / 6;
  const power = 10 ** Math.floor(Math.log10(rough));
  const step = [1, 2, 5, 10].find((n) => n * power >= rough)! * power;
  return Math.ceil(Math.max(1000, ms) / step) * step;
}

function groupNodes(agents: Map<string, TraceNode>) {
  return [...agents.values()].flatMap((n) => n.children);
}
