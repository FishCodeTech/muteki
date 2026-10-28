import type { ChatMessage, DeckState } from "@/lib/events";
import { projectRuntimeTrace, type TraceNode as RuntimeNode } from "@/lib/runtimeTrace";
import { abbreviateWorkspacePaths, splitToolCommand, toolCommandLabel } from "@/lib/activityLedger";
import { workerColorVar } from "@/lib/workers";
import { toEpochMs } from "@/lib/format";
import {
  compressIdle,
  finalizeScenario,
  sameGaps,
  type EventKind,
  type SpanKind,
  type TimeGap,
  type ToolKind,
  type TraceEventSpec,
  type TraceScenario,
  type TraceSpanSpec,
} from "./traceModel";
import type { TimelineLang } from "./traceCopy";

/** Keeps unchanged specs identical across rebuilds so memoized rows skip re-rendering. */
export interface DeckTraceCache {
  lang: string;
  spans: Map<string, TraceSpanSpec>;
  events: Map<string, TraceEventSpec>;
  gaps: TimeGap[];
}

export const createDeckTraceCache = (): DeckTraceCache => ({ lang: "", spans: new Map(), events: new Map(), gaps: [] });

type Translate = (key: string, vars?: Record<string, string | number>) => string;

const EN_LABELS: Record<string, string> = {
  协调器: "Coordinator",
  用户输入: "User input",
  运行事件: "Run events",
  任务规划: "Planning",
  复核记录: "Review records",
  系统事件: "System events",
  工具调用: "Tool call",
};

const KIND: Record<RuntimeNode["kind"], SpanKind> = {
  run: "agent",
  agent: "worker",
  group: "group",
  tool: "tool",
  llm: "llm",
  event: "event",
};

const TEXT_MAX = 4000;
/** A neighbouring record further away than this says too little about when a call ran. */
const ESTIMATE_MAX_MS = 5 * 60_000;

/** Outcomes where the run itself broke, as opposed to ending without a result. */
const RUN_FAILURES = new Set(["runtime_failure", "preflight_failed"]);

function firstLine(text: string | undefined, max = 240): string {
  if (!text) return "";
  const line = text.trim().split("\n", 1)[0] ?? "";
  return line.length > max ? `${line.slice(0, max)}…` : line;
}

/** A one-line summary for args/detail that may be pretty-printed JSON — the
 * first line alone renders as a bare `{`. Pull a meaningful value out instead. */
function summarizeDetail(text: string | undefined, max = 240): string {
  const line = firstLine(text, max);
  if (!line || !line.startsWith("{")) return line;
  try {
    const parsed = JSON.parse(text ?? "");
    if (!parsed || typeof parsed !== "object") return line;
    for (const key of ["title", "name", "command", "path", "file_path", "url"]) {
      const value = (parsed as Record<string, unknown>)[key];
      if (typeof value === "string" && value.trim()) return clip(value.trim(), 160) ?? "";
    }
    for (const value of Object.values(parsed as Record<string, unknown>)) {
      if (typeof value === "string" && value.trim()) return clip(value.trim(), 160) ?? "";
    }
  } catch {
    // not parseable JSON (truncated?) — collapse whitespace into one line;
    // a bare brace alone is meaningless, so drop it
    const collapsed = (text ?? "").replace(/\s+/g, " ").trim();
    if (collapsed === "{") return "";
    return clip(collapsed, max) ?? "";
  }
  return line;
}

function clip(text: string | undefined, max = TEXT_MAX): string | undefined {
  if (!text) return undefined;
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

export function toolKindOf(name: string, args: string): ToolKind {
  const tool = name.toLowerCase();
  const head = args.trimStart().slice(0, 60).toLowerCase();
  if (/todo|plan/.test(tool)) return "plan";
  if (/web|fetch|browse|http|url/.test(tool)) return "web";
  if (/grep|glob|search|find|^ls$|list/.test(tool)) return "search";
  if (/edit|write|patch|replace|create/.test(tool)) return "edit";
  if (/read|view|open/.test(tool)) return "read";
  // Shell-style tools carry the real intent in the command they run.
  if (/\b(pytest|jest|vitest|mocha|unittest|go test|cargo test|npm (run )?test|pnpm test|yarn test)\b/.test(head)) return "test";
  if (/^(\{"command":\s*")?git\b/.test(head) || /\bgit (status|diff|log|commit|add|checkout|switch|push|pull|show)\b/.test(head)) return "git";
  if (/\b(curl|wget)\b/.test(head)) return "web";
  if (/^(\{"command":\s*")?(rg|grep|find|fd|ls)\b/.test(head)) return "search";
  if (/^(\{"command":\s*")?(cat|head|tail|less|sed -n)\b/.test(head)) return "read";
  return "shell";
}

function eventKindOf(message: ChatMessage): EventKind {
  if (message.role === "human") return "input";
  if (message.role === "system") return "system";
  switch (message.kind) {
    case "reasoning": return "reasoning";
    case "insight": return "finding";
    case "guidance": return "guidance";
    case "flag": return "result";
    case "progress": return "progress";
    case "tool": return "call";
    case "text":
    case "status":
      return "message";
    default: {
      const exhaustive: never = message.kind;
      return exhaustive;
    }
  }
}

function shallowEqual<T extends object>(a: T, b: T): boolean {
  const keysA = Object.keys(a) as (keyof T)[];
  if (keysA.length !== Object.keys(b).length) return false;
  for (const key of keysA) if (a[key] !== b[key]) return false;
  return true;
}

function intern<T extends { id: string }>(previous: Map<string, T>, next: Map<string, T>, value: T): T {
  const old = previous.get(value.id);
  const kept = old && shallowEqual(old, value) ? old : value;
  next.set(value.id, kept);
  return kept;
}

/** Drops undefined fields so shallow comparison does not see phantom differences. */
function compact<T extends object>(value: T): T {
  for (const key of Object.keys(value) as (keyof T)[]) if (value[key] === undefined) delete value[key];
  return value;
}

/** The tab badge and the timeline both read the projection of the same deck. */
const projections = new WeakMap<DeckState, { running: boolean; model: ReturnType<typeof projectRuntimeTrace> }>();
function projection(deck: DeckState, running: boolean) {
  const cached = projections.get(deck);
  if (cached && cached.running === running) return cached.model;
  const model = projectRuntimeTrace(deck, running);
  projections.set(deck, { running, model });
  return model;
}

/** Number of rows the event ledger shows for this deck once fully played. */
export function countTimelineEvents(deck: DeckState, running: boolean): number {
  let extra = 0;
  for (const node of projection(deck, running).nodes) {
    if (node.kind === "tool" && !node.messageId) extra++;
    else if (node.kind === "llm") extra++;
    else if (node.kind === "event" && !node.id.startsWith("trace:chat:")) extra++;
  }
  return deck.chat.length + extra;
}

/** Index of the last value <= `value` in a sorted array, or -1. */
function floorIndex(list: number[], value: number): number {
  let lo = 0;
  let hi = list.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (list[mid] <= value) lo = mid + 1;
    else hi = mid;
  }
  return lo - 1;
}

/**
 * Projects the recorded run onto the timeline model: execution units and calls become spans,
 * every chat message becomes a ledger event attached to its call or its actor. Times are
 * relative to the earliest recorded instant, returned as `origin`, and long idle stretches are
 * compressed (see `compressIdle`).
 */
export function scenarioFromDeck(
  deck: DeckState,
  running: boolean,
  options: { lang: TimelineLang; t: Translate; cache: DeckTraceCache },
): { scenario: TraceScenario; origin: number | null } {
  const { lang, t, cache } = options;
  if (cache.lang !== lang) {
    cache.lang = lang;
    cache.spans = new Map();
    cache.events = new Map();
  }
  const en = lang === "en";
  const model = projection(deck, running);
  const messages = new Map(deck.chat.map((message) => [message.id, message]));

  let origin = Infinity;
  for (const node of model.nodes) if (node.start > 0) origin = Math.min(origin, node.start);
  for (const message of deck.chat) {
    const ts = toEpochMs(message.ts);
    if (ts > 0) origin = Math.min(origin, ts);
  }
  if (!Number.isFinite(origin)) {
    return { scenario: finalizeScenario({ id: deck.runId, title: deck.challengeName, task: "", spans: [], events: [] }), origin: null };
  }
  const rel = (ts: number) => Math.max(0, ts - origin);
  const label = (node: RuntimeNode) => {
    if (!en || node.kind === "agent") return node.label;
    if (node.kind === "llm") return node.label.replace(/^规划结果 · (\d+) 个意图$/, "Planning result · $1 intents");
    return EN_LABELS[node.label] ?? node.label;
  };
  const llmDetail = (node: RuntimeNode) =>
    en ? node.detail?.replace(/^(\d+) 次尝试；未记录单次模型调用起止时间$/, "$1 attempts; per-call timing was not recorded") : node.detail;

  const callMessage = new Map<string, string>();
  const actorTimes = new Map<string, number[]>();
  const noteTime = (spanId: string, ts: number) => {
    const list = actorTimes.get(spanId);
    if (list) list.push(ts);
    else actorTimes.set(spanId, [ts]);
  };
  const draftEvents: TraceEventSpec[] = [];
  for (const node of model.nodes) if (node.kind === "tool" && node.messageId) callMessage.set(node.messageId, node.id);
  for (const message of deck.chat) {
    if (message.kind === "tool" && callMessage.has(message.id)) continue;
    const actor = message.solverId || (message.role === "human" ? "operator" : "coordinator");
    const spanId = `trace:agent:${actor}`;
    const ts = rel(toEpochMs(message.ts));
    const text = message.i18nKey ? t(message.i18nKey, message.i18nVars) : message.content;
    const line = summarizeDetail(text, 300);
    draftEvents.push({
      id: message.id,
      spanId,
      ts,
      kind: eventKindOf(message),
      text: line,
      body: text.length > line.length || text.includes("\n") ? clip(text) : undefined,
    });
    noteTime(spanId, ts);
  }

  const spans: TraceSpanSpec[] = [];
  /** Leaf calls recorded at a single instant, with the container whose records bound them. */
  const singles: { spec: TraceSpanSpec; actorId: string; anchor: "start" | "end"; settled: boolean }[] = [];
  const leaves: { spec: TraceSpanSpec; node: RuntimeNode; recorded: number }[] = [];
  type Extent = { start: number; edge: number; open: boolean };
  const visit = (node: RuntimeNode, parentId: string | null, order: number, actorId: string): Extent | null => {
    if (node.kind === "event" && node.id.startsWith("trace:chat:")) return null;
    const kind = KIND[node.kind];
    const isContainer = kind === "agent" || kind === "worker" || kind === "group";
    const childActor = isContainer ? node.id : actorId;
    const children: Extent[] = [];
    node.children.forEach((child) => {
      const extent = visit(child, node.id, child.kind === "agent" ? 0 : 1, childActor);
      if (extent) children.push(extent);
    });
    const container = isContainer || children.length > 0;
    const failed = node.status === "failed";
    const cancelled = node.status === "cancelled";
    let spec: TraceSpanSpec;
    let extent: Extent;
    if (container) {
      const own = node.interval && node.start > 0;
      let start = own ? rel(node.start) : Infinity;
      let edge = own && node.end != null ? rel(node.end) : own ? start : -Infinity;
      let open = own && node.end == null && node.status === "running";
      for (const child of children) {
        start = Math.min(start, child.start);
        edge = Math.max(edge, child.edge);
        open ||= child.open;
      }
      for (const ts of actorTimes.get(node.id) ?? []) {
        start = Math.min(start, ts);
        edge = Math.max(edge, ts);
      }
      if (!Number.isFinite(start)) start = node.start > 0 ? rel(node.start) : 0;
      if (!Number.isFinite(edge) || edge < start) edge = start;
      open &&= running;
      const point = !open && edge <= start;
      extent = { start, edge, open };
      spec = {
        id: node.id,
        parentId,
        name: label(node),
        kind,
        start,
        end: open || point ? undefined : edge,
        point: point || undefined,
        failed: failed || undefined,
        cancelled: cancelled || undefined,
        order,
      };
      if (kind === "worker") {
        spec.engine = node.engine;
        spec.detail = node.detail;
        spec.color = workerColorVar(node.actor, node.engine);
      }
    } else {
      const start = node.start > 0 ? rel(node.start) : 0;
      const closed = node.interval && node.end != null && node.end >= node.start;
      const open = !closed && node.interval && node.status === "running" && running;
      const point = !closed && !open;
      const end = closed ? rel(node.end!) : undefined;
      extent = { start, edge: end ?? start, open };
      spec = {
        id: node.id,
        parentId,
        name: label(node),
        kind,
        start,
        end,
        point: point || undefined,
        failed: failed || undefined,
        unknown: !failed && point && node.status === "unknown" ? true : undefined,
        order,
      };
      noteTime(actorId, start);
      if (end != null) noteTime(actorId, end);
      if (point && kind === "tool" && node.anchor) {
        singles.push({ spec, actorId, anchor: node.anchor, settled: node.status === "complete" || failed });
      }
    }
    if (kind === "tool") {
      const message = node.messageId ? messages.get(node.messageId) : undefined;
      let name = label(node);
      let args = node.detail ?? "";
      if (message) {
        const command = splitToolCommand(toolCommandLabel(message));
        name = command.name || (en ? "Tool call" : "工具调用");
        args = command.args;
      }
      spec.name = name;
      spec.tool = toolKindOf(name, args);
      spec.detail = abbreviateWorkspacePaths(summarizeDetail(args)) || undefined;
      // Keep original strings for expanded details; summaries remain bounded below.
      spec.input = args || undefined;
      spec.output = message?.toolOutput || undefined;
      if (failed) spec.error = clip(message?.toolOutput || node.detail, 600);
    } else if (kind === "llm") {
      spec.detail = llmDetail(node);
    } else if (kind === "event") {
      spec.detail = summarizeDetail(node.detail) || undefined;
      if (failed) spec.error = clip(node.detail, 600);
    }
    spans.push(spec);
    if (kind === "tool" || kind === "llm" || kind === "event") leaves.push({ spec, node, recorded: spec.start });
    return extent;
  };
  visit(model.root, null, 0, model.root.id);

  // A call recorded only when it returned (or only when it began) borrows its other end from the
  // nearest record of the same actor; the bar and its duration are marked as estimates.
  for (const list of actorTimes.values()) list.sort((a, b) => a - b);
  for (const { spec, actorId, anchor, settled } of singles) {
    const times = actorTimes.get(actorId);
    if (!times) continue;
    if (anchor === "end") {
      let index = floorIndex(times, spec.start);
      while (index >= 0 && times[index] >= spec.start) index--;
      const before = index >= 0 ? times[index] : null;
      if (before == null || spec.start - before > ESTIMATE_MAX_MS) continue;
      spec.end = spec.start;
      spec.start = before;
    } else {
      // A call that never reported back has no bound at all.
      if (!settled) continue;
      const after = times[floorIndex(times, spec.start) + 1];
      if (after == null || after - spec.start > ESTIMATE_MAX_MS) continue;
      spec.end = after;
    }
    spec.point = undefined;
    spec.unknown = undefined;
    spec.estimated = true;
  }

  const callEvents: TraceEventSpec[] = [];
  for (const { spec, node, recorded } of leaves) {
    const body = spec.kind === "tool" ? [clip(spec.input), clip(spec.output)].filter(Boolean).join("\n\n") : node.detail;
    callEvents.push({
      id: node.messageId ?? `call:${node.id}`,
      spanId: node.id,
      ts: recorded,
      kind: "call",
      text: spec.name,
      body: clip(body) || undefined,
    });
  }

  const rootIndex = spans.findIndex((span) => span.id === model.root.id);
  if (rootIndex >= 0 && !running && RUN_FAILURES.has(deck.outcomeReason ?? "")) spans[rootIndex] = { ...spans[rootIndex], failed: true };

  const compressed = compressIdle(spans, [...draftEvents, ...callEvents]);
  const nextSpans = new Map<string, TraceSpanSpec>();
  const nextEvents = new Map<string, TraceEventSpec>();
  const finalSpans = compressed.spans.map((spec) => intern(cache.spans, nextSpans, compact({ ...spec })));
  const finalEvents = compressed.events.map((event) => intern(cache.events, nextEvents, compact({ ...event })));
  cache.spans = nextSpans;
  cache.events = nextEvents;
  if (!sameGaps(cache.gaps, compressed.gaps)) cache.gaps = compressed.gaps;
  return {
    scenario: finalizeScenario({ id: deck.runId, title: deck.challengeName, task: "", spans: finalSpans, events: finalEvents, gaps: cache.gaps }),
    origin,
  };
}
