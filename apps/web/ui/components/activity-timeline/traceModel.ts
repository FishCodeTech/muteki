export type SpanKind = "agent" | "worker" | "group" | "llm" | "tool" | "event";
export type ToolKind = "read" | "edit" | "search" | "web" | "shell" | "test" | "git" | "plan";
export type SpanStatus = "running" | "done" | "failed" | "cancelled" | "unknown";
export type EventKind =
  | "call"
  | "message"
  | "reasoning"
  | "finding"
  | "approval"
  | "result"
  | "input"
  | "system"
  | "progress"
  | "guidance";

export interface TraceSpanSpec {
  id: string;
  parentId: string | null;
  name: string;
  kind: SpanKind;
  tool?: ToolKind;
  engine?: string;
  /** CSS color for the actor accent; falls back to the engine token. */
  color?: string;
  detail?: string;
  start: number;
  /** Missing while the span is still open. */
  end?: number;
  /** Recorded as an instant: no measurable duration. */
  point?: boolean;
  failed?: boolean;
  /** Finished without a known result. */
  unknown?: boolean;
  /** Stopped on purpose before it finished. */
  cancelled?: boolean;
  /**
   * Only one end of the call was recorded; the other is the neighbouring record of the same
   * actor, so the duration is an upper bound.
   */
  estimated?: boolean;
  error?: string;
  retryOf?: string;
  tokensIn?: number;
  tokensOut?: number;
  cost?: number;
  input?: string;
  output?: string;
  /** Sibling order before start time; lower comes first. */
  order?: number;
}

export interface TraceEventSpec {
  id: string;
  spanId: string;
  ts: number;
  kind: EventKind;
  text: string;
  body?: string;
}

export interface TraceScenario {
  id: string;
  title: string;
  task: string;
  spans: TraceSpanSpec[];
  events: TraceEventSpec[];
  duration: number;
  /** Sorted, de-duplicated instants where the visible data changes. */
  boundaries: number[];
  /** Idle stretches drawn narrower than they lasted; times in the scenario are on the compressed axis. */
  gaps: TimeGap[];
}

/** An idle stretch from real `start` to `end`, drawn as `width` axis units beginning at `at`. */
export interface TimeGap {
  at: number;
  width: number;
  start: number;
  end: number;
}

export interface TraceNode {
  spec: TraceSpanSpec;
  status: SpanStatus;
  end: number | null;
  depth: number;
  parent: TraceNode | null;
  children: TraceNode[];
  /** Own plus finished descendants. */
  cost: number;
  tokens: number;
  /** Nearest agent/worker/group at or above this span, used as the ledger actor. */
  actor: TraceNode;
}

export interface TraceStats {
  duration: number;
  cost: number;
  tokens: number;
  llmCalls: number;
  toolCalls: number;
  workers: number;
  errors: number;
}

export interface TraceSnapshot {
  root: TraceNode | null;
  byId: Map<string, TraceNode>;
  order: TraceNode[];
  events: TraceEventSpec[];
  stats: TraceStats;
  slowestId: string | null;
  running: boolean;
}

export const isLeafKind = (kind: SpanKind) => kind === "llm" || kind === "tool" || kind === "event";
export const isContainerKind = (kind: SpanKind) => kind === "agent" || kind === "worker" || kind === "group";

/** Where a span's recorded data ends: its end, or its start while open or instantaneous. */
export const specEdge = (spec: TraceSpanSpec) => spec.end ?? spec.start;

export function finalizeScenario(input: Omit<TraceScenario, "boundaries" | "duration" | "gaps"> & { gaps?: TimeGap[] }): TraceScenario {
  const set = new Set<number>();
  for (const span of input.spans) {
    set.add(span.start);
    if (span.end != null) set.add(span.end);
  }
  for (const event of input.events) set.add(event.ts);
  const boundaries = [...set].sort((a, b) => a - b);
  const spans = [...input.spans].sort((a, b) => a.start - b.start || (b.end ?? Infinity) - (a.end ?? Infinity));
  const events = [...input.events].sort((a, b) => a.ts - b.ts);
  return { ...input, spans, events, boundaries, duration: boundaries[boundaries.length - 1] ?? 0, gaps: input.gaps ?? [] };
}

/** Axis time to real elapsed time since trace start. */
export function toRealTime(gaps: readonly TimeGap[], t: number): number {
  let offset = 0;
  for (const gap of gaps) {
    if (t <= gap.at) break;
    if (t < gap.at + gap.width) return gap.start + ((t - gap.at) * (gap.end - gap.start)) / gap.width;
    offset = gap.end - gap.at - gap.width;
  }
  return t + offset;
}

/** Real elapsed time to axis time. */
export function toAxisTime(gaps: readonly TimeGap[], real: number): number {
  let offset = 0;
  for (const gap of gaps) {
    if (real <= gap.start) break;
    if (real < gap.end) return gap.at + ((real - gap.start) * gap.width) / (gap.end - gap.start);
    offset = gap.end - gap.at - gap.width;
  }
  return real - offset;
}

export const sameGaps = (a: readonly TimeGap[], b: readonly TimeGap[]) =>
  a.length === b.length && a.every((gap, index) => gap.at === b[index].at && gap.width === b[index].width && gap.start === b[index].start && gap.end === b[index].end);

/**
 * Finds idle stretches (no call running, nothing recorded) long enough to swamp the axis and
 * draws each as a narrow break. Estimated call bounds do not count as activity.
 */
export function compressIdle(
  spans: TraceSpanSpec[],
  events: TraceEventSpec[],
  options: { minGap?: number; minShare?: number } = {},
): { spans: TraceSpanSpec[]; events: TraceEventSpec[]; gaps: TimeGap[] } {
  const minGap = options.minGap ?? 45_000;
  const minShare = options.minShare ?? 0.08;
  const intervals: [number, number][] = [];
  for (const span of spans) {
    if (isLeafKind(span.kind)) {
      if (span.estimated) continue;
      intervals.push([span.start, span.end ?? (span.point ? span.start : Infinity)]);
    } else {
      // A container's own boundaries are recorded moments; its extent says nothing about activity.
      intervals.push([span.start, span.start]);
      if (span.end != null) intervals.push([span.end, span.end]);
    }
  }
  for (const event of events) intervals.push([event.ts, event.ts]);
  if (intervals.length < 2) return { spans, events, gaps: [] };
  intervals.sort((a, b) => a[0] - b[0]);
  const idle: [number, number][] = [];
  let reach = intervals[0][1];
  let first = intervals[0][0];
  let last = reach;
  for (const [start, end] of intervals) {
    if (start > reach) idle.push([reach, start]);
    reach = Math.max(reach, end);
    if (Number.isFinite(end)) last = Math.max(last, end);
    first = Math.min(first, start);
  }
  const total = Math.max(last, Number.isFinite(reach) ? reach : last) - first;
  const threshold = Math.max(minGap, total * minShare);
  const long = idle.filter(([start, end]) => end - start >= threshold);
  if (!long.length) return { spans, events, gaps: [] };
  const active = total - long.reduce((sum, [start, end]) => sum + end - start, 0);
  const gaps: TimeGap[] = [];
  let removed = 0;
  for (const [start, end] of long) {
    const width = Math.round(Math.min((end - start) / 2, Math.max(1000, Math.min(15_000, active * 0.04))));
    gaps.push({ at: start - removed, width, start, end });
    removed += end - start - width;
  }
  const axis = (t: number) => toAxisTime(gaps, t);
  return {
    gaps,
    spans: spans.map((span) => {
      const start = axis(span.start);
      const end = span.end == null ? undefined : axis(span.end);
      return start === span.start && end === span.end ? span : { ...span, start, ...(end != null && { end }) };
    }),
    events: events.map((event) => {
      const ts = axis(event.ts);
      return ts === event.ts ? event : { ...event, ts };
    }),
  };
}

/** Number of data boundaries already reached; snapshots only change when it does. */
export function epochAt(scenario: TraceScenario, now: number): number {
  const list = scenario.boundaries;
  let lo = 0;
  let hi = list.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (list[mid] <= now) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

function settledStatus(spec: TraceSpanSpec): SpanStatus {
  return spec.failed ? "failed" : spec.cancelled ? "cancelled" : spec.unknown ? "unknown" : "done";
}

export function snapshotAt(scenario: TraceScenario, now: number): TraceSnapshot {
  const byId = new Map<string, TraceNode>();
  const roots: TraceNode[] = [];
  for (const spec of scenario.spans) {
    if (spec.start > now) continue;
    const finished = !spec.point && spec.end != null && spec.end <= now;
    byId.set(spec.id, {
      spec,
      status: spec.point || finished ? settledStatus(spec) : "running",
      end: finished ? spec.end! : null,
      depth: 0,
      parent: null,
      children: [],
      cost: finished || spec.point ? spec.cost ?? 0 : 0,
      tokens: finished || spec.point ? (spec.tokensIn ?? 0) + (spec.tokensOut ?? 0) : 0,
      actor: null as unknown as TraceNode,
    });
  }
  // Linked in a second pass: recorded data does not guarantee parents sort before children.
  for (const node of byId.values()) {
    const parent = node.spec.parentId ? byId.get(node.spec.parentId) : undefined;
    if (parent && parent !== node) {
      node.parent = parent;
      parent.children.push(node);
    } else roots.push(node);
  }
  const bySibling = (a: TraceNode, b: TraceNode) => (a.spec.order ?? 0) - (b.spec.order ?? 0) || a.spec.start - b.spec.start;
  let root: TraceNode | null = roots[0] ?? null;
  // Orphans whose parent has not started yet hang off the root so they stay visible.
  if (root && roots.length > 1) {
    for (const orphan of roots.slice(1)) {
      orphan.parent = root;
      root.children.push(orphan);
    }
  }
  if (!root) root = null;
  const order: TraceNode[] = [];
  const stats: TraceStats = { duration: 0, cost: 0, tokens: 0, llmCalls: 0, toolCalls: 0, workers: 0, errors: 0 };
  let slowest: { id: string; d: number } | null = null;
  let running = false;
  const walk = (node: TraceNode, depth: number) => {
    node.depth = depth;
    node.actor = node.parent && !isContainerKind(node.spec.kind) ? node.parent.actor : node;
    order.push(node);
    node.children.sort(bySibling);
    for (const child of node.children) walk(child, depth + 1);
    for (const child of node.children) {
      node.cost += child.cost;
      node.tokens += child.tokens;
    }
    const { spec } = node;
    if (spec.kind === "llm") stats.llmCalls++;
    if (spec.kind === "tool") stats.toolCalls++;
    if (spec.kind === "worker") stats.workers++;
    if (node.status === "failed") stats.errors++;
    if (node.status === "running") running = true;
    if (node.end != null && !spec.estimated && (spec.kind === "llm" || spec.kind === "tool")) {
      const d = node.end - spec.start;
      if (!slowest || d > slowest.d) slowest = { id: spec.id, d };
    }
  };
  if (root) walk(root, 0);
  if (root) {
    stats.cost = root.cost;
    stats.tokens = root.tokens;
    stats.duration = (root.end ?? now) - root.spec.start;
  }
  const events = scenario.events.filter((event) => event.ts <= now && byId.has(event.spanId));
  return { root, byId, order, events, stats, slowestId: (slowest as { id: string } | null)?.id ?? null, running };
}

/** Snapshots rebuild every node; this compares what a row actually renders. */
export function sameNodeState(a: TraceNode, b: TraceNode): boolean {
  return (
    a === b ||
    (a.spec === b.spec &&
      a.status === b.status &&
      a.end === b.end &&
      a.cost === b.cost &&
      a.tokens === b.tokens &&
      a.depth === b.depth &&
      a.children.length === b.children.length &&
      a.actor.spec === b.actor.spec)
  );
}

export function isAncestor(ancestor: TraceNode, node: TraceNode | null | undefined): boolean {
  for (let current = node; current; current = current.parent) if (current === ancestor) return true;
  return false;
}

const RANGE_STEPS_S = [10, 15, 20, 30, 45, 60, 90, 120, 180, 240, 300, 450, 600, 900, 1200, 1800, 2700, 3600, 5400, 7200];

/** The axis grows in round steps so ticks stay readable while a run gets longer. */
export function niceRange(ms: number, min = 10_000): number {
  const need = Math.max(min, ms);
  for (const step of RANGE_STEPS_S) if (step * 1000 >= need) return step * 1000;
  return Math.ceil(need / 3_600_000) * 3_600_000;
}

const TICK_STEPS_S = [0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 20, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 14400];

export interface AxisTick {
  /** Axis position. */
  at: number;
  /** Real elapsed time shown as the label. */
  value: number;
}

/** Round real-time ticks; across an idle break the labels jump with the time they stand for. */
export function ticksFor(start: number, range: number, gaps: readonly TimeGap[] = [], maxTicks = 6): AxisTick[] {
  const step = (TICK_STEPS_S.find((s) => range / (s * 1000) <= maxTicks) ?? 14400) * 1000;
  const end = start + range;
  const segments: [number, number][] = [];
  let cursor = start;
  for (const gap of gaps) {
    if (gap.at + gap.width <= cursor) continue;
    if (gap.at >= end) break;
    if (gap.at > cursor) segments.push([cursor, gap.at]);
    cursor = gap.at + gap.width;
  }
  if (cursor < end) segments.push([cursor, end]);
  const out: AxisTick[] = [];
  const spacing = range / (maxTicks * 1.6);
  for (const [from, to] of segments) {
    const realFrom = toRealTime(gaps, from);
    const realTo = toRealTime(gaps, to);
    for (let value = Math.ceil(realFrom / step) * step; value <= realTo + 1; value += step) {
      const at = toAxisTime(gaps, value);
      if (out.length && at - out[out.length - 1].at < spacing) continue;
      out.push({ at: Math.round(at), value: Math.round(value) });
    }
  }
  return out;
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))}ms`;
  if (ms < 10_000) return `${(ms / 1000).toFixed(1)}s`;
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  const total = Math.round(ms / 1000);
  if (total >= 3600) {
    const hours = Math.floor(total / 3600);
    const minutes = Math.floor((total % 3600) / 60);
    return minutes ? `${hours}h ${minutes}m` : `${hours}h`;
  }
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return seconds ? `${minutes}m ${seconds}s` : `${minutes}m`;
}

export function formatTick(ms: number): string {
  if (ms === 0) return "0s";
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${+(ms / 1000).toFixed(1)}s`;
  return formatDuration(ms);
}

export function formatClock(ms: number): string {
  const tenths = Math.floor(Math.max(0, ms) / 100);
  const totalSeconds = Math.floor(tenths / 10);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours) return `${hours}:${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
  return `${minutes}:${String(seconds).padStart(2, "0")}.${tenths % 10}`;
}

export function formatCost(amount: number): string {
  if (amount <= 0) return "$0";
  return amount < 0.1 ? `$${amount.toFixed(3)}` : `$${amount.toFixed(2)}`;
}

export function formatTokens(count: number): string {
  if (count < 1000) return String(count);
  if (count >= 1_000_000) return `${(count / 1_000_000).toFixed(count < 10_000_000 ? 1 : 0)}M`;
  return `${(count / 1000).toFixed(count < 10_000 ? 1 : 0)}k`;
}
