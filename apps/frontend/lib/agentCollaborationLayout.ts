
import {
  COORDINATOR_ID,
  isCollaborationWorker,
  type CollaborationAgent,
  type CollaborationAgentRole,
  type CollaborationRelation,
} from "./agentCollaboration";
import { toEpochMs } from "./format";

export const LAYOUT = {
  nodeWidth: 360,
  nodeHeight: 164,
  columnGap: 190,
  rowGap: 80,
  origin: 72,
  maxDepth: 24,
  columnRows: 10,
  coordinatorHeight: 180,
  autoWorkItemRows: 5,
  workItemRowHeight: 40,
  workListGap: 8,
  timelineUsableWidth: 1800,
  timelineRulerHeight: 32,
  timelineRoleGap: 36,
  timelineLaneGap: 16,
} as const;

export type CollabCanvasLayout = "hierarchy" | "timeline";

export function agentCardHeight(workItemRows = 0): number {
  return LAYOUT.nodeHeight + (workItemRows > 0 ? LAYOUT.workListGap + workItemRows * LAYOUT.workItemRowHeight : 0);
}

export const ZOOM = { min: 0.06, max: 1.8, fitMax: 1, compact: 0.55, detailEnter: 1.05, detailExit: 0.85, centerMin: 0.5, center: 0.75 } as const;

export const MINIMAP = { autoShowNodes: 8, width: 150, height: 96 } as const;

export const FIT_MIN_ZOOM = 0.25;

export const COLLAB_OVERLAY_MAX = 1100;
export const COLLAB_MOBILE_MAX = 900;

export const COLLAB_INDEX_W = { min: 220, ratio: 0.22, max: 286 } as const;
export const COLLAB_DETAIL_W = { min: 280, ratio: 0.28, max: 372 } as const;

export function clampCollabPanelWidth(
  container: number,
  spec: { min: number; ratio: number; max: number },
): number {
  return Math.min(spec.max, Math.max(spec.min, container * spec.ratio));
}

export const FIT_DELAY = { resizeSettle: 140 } as const;

export const MOTION = { fit: 260, center: 260 } as const;

export const LIST_LIMITS = { activity: 100, knowledge: 120 } as const;

export type CollaborationPosition = { x: number; y: number };

const SUBCOLUMN_STEP = LAYOUT.nodeWidth + LAYOUT.columnGap / 2;

function roleRank(agent: CollaborationAgent): number {
  if (agent.role === "worker") return 0;
  if (agent.role === "review") return 1;
  if (agent.role === "verifier") return 2;
  if (agent.role === "coordinator") return -3;
  if (agent.role === "decision" || agent.role === "challenger") return -2;
  return -1;
}

function baseColumn(agent: CollaborationAgent): number {
  if (!isCollaborationWorker(agent)) return 0;
  if (agent.role === "review" || agent.role === "verifier") return 2;
  return 1;
}

function nodeHeightOf(agent: CollaborationAgent): number {
  return agent.id === COORDINATOR_ID ? LAYOUT.coordinatorHeight : LAYOUT.nodeHeight;
}

function compareAgentId(a: string, b: string): number {
  return a.localeCompare(b, undefined, { numeric: true });
}

function causalWorkerEdge(
  byId: Map<string, CollaborationAgent>,
  relation: CollaborationRelation,
): { upstream: string; downstream: string } | undefined {
  if (relation.source === relation.target) return undefined;
  const source = byId.get(relation.source);
  const target = byId.get(relation.target);
  if (!source || !target || !isCollaborationWorker(source) || !isCollaborationWorker(target)) return undefined;
  if (relation.kind === "handoff") return { upstream: relation.source, downstream: relation.target };
  if (relation.kind === "review" && source.role === "review") return { upstream: relation.target, downstream: relation.source };
  if (relation.kind === "verify" && source.role === "verifier") return { upstream: relation.target, downstream: relation.source };
  return undefined;
}

function isCausalCrossingRelation(
  byId: Map<string, CollaborationAgent>,
  relation: CollaborationRelation,
): boolean {
  if (relation.kind === "handoff") return true;
  const source = byId.get(relation.source);
  if (!source) return false;
  if (relation.kind === "review") return source.role === "review" || source.role === "challenger";
  if (relation.kind === "verify") return source.role === "verifier";
  return false;
}

function columnDepths(agents: CollaborationAgent[], relations: CollaborationRelation[]): Map<string, number> {
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  const rawEdges: Array<{ upstream: string; downstream: string }> = [];
  const edgeKeys = new Set<string>();
  for (const relation of relations) {
    const edge = causalWorkerEdge(byId, relation);
    if (!edge) continue;
    const key = `${edge.upstream}\0${edge.downstream}`;
    if (edgeKeys.has(key)) continue;
    edgeKeys.add(key);
    rawEdges.push(edge);
  }
  rawEdges.sort((a, b) =>
    (byId.get(a.upstream)?.firstSeenAt ?? 0) - (byId.get(b.upstream)?.firstSeenAt ?? 0)
    || (byId.get(a.downstream)?.firstSeenAt ?? 0) - (byId.get(b.downstream)?.firstSeenAt ?? 0)
    || compareAgentId(a.upstream, b.upstream)
    || compareAgentId(a.downstream, b.downstream));

  const outgoing = new Map<string, string[]>();
  const incoming = new Map<string, string[]>();
  const canReach = (from: string, target: string): boolean => {
    if (from === target) return true;
    const visited = new Set<string>([from]);
    const queue = [from];
    for (let i = 0; i < queue.length; i += 1) {
      for (const next of outgoing.get(queue[i]) || []) {
        if (next === target) return true;
        if (!visited.has(next)) {
          visited.add(next);
          queue.push(next);
        }
      }
    }
    return false;
  };
  for (const { upstream, downstream } of rawEdges) {
    if (canReach(downstream, upstream)) continue;
    const outs = outgoing.get(upstream) || [];
    outs.push(downstream);
    outgoing.set(upstream, outs);
    const ins = incoming.get(downstream) || [];
    ins.push(upstream);
    incoming.set(downstream, ins);
  }

  const depth = new Map<string, number>();
  const resolve = (id: string): number => {
    const known = depth.get(id);
    if (known !== undefined) return known;
    const agent = byId.get(id);
    let best = agent ? baseColumn(agent) : 1;
    for (const source of incoming.get(id) || []) {
      best = Math.max(best, Math.min(LAYOUT.maxDepth, resolve(source) + 1));
    }
    depth.set(id, best);
    return best;
  };
  for (const agent of agents) resolve(agent.id);
  return depth;
}

function minimizeColumnCrossings(
  columns: Map<number, CollaborationAgent[]>,
  depth: Map<string, number>,
  relations: CollaborationRelation[],
  byId: Map<string, CollaborationAgent>,
): void {
  const neighbors = new Map<string, string[]>();
  for (const relation of relations) {
    if (!isCausalCrossingRelation(byId, relation)) continue;
    const sourceDepth = depth.get(relation.source);
    const targetDepth = depth.get(relation.target);
    if (sourceDepth === undefined || targetDepth === undefined || sourceDepth === targetDepth) continue;
    neighbors.set(relation.source, [...(neighbors.get(relation.source) || []), relation.target]);
    neighbors.set(relation.target, [...(neighbors.get(relation.target) || []), relation.source]);
  }
  const depths = [...columns.keys()].sort((a, b) => a - b);
  const rank = new Map<string, { depth: number; index: number; count: number }>();
  const refresh = (column: number) => {
    const rows = columns.get(column) || [];
    rows.forEach((agent, index) => rank.set(agent.id, { depth: column, index, count: rows.length }));
  };
  depths.forEach(refresh);
  const sweep = (forward: boolean) => {
    const order = forward ? depths : [...depths].reverse();
    for (const column of order) {
      if (column === 0) continue;
      const rows = columns.get(column);
      if (!rows || rows.length < 2) continue;
      const oldIndex = new Map(rows.map((agent, index) => [agent.id, index]));
      const score = (agent: CollaborationAgent): number => {
        const adjacent = (neighbors.get(agent.id) || [])
          .map((id) => rank.get(id))
          .filter((item): item is { depth: number; index: number; count: number } => !!item && (forward ? item.depth < column : item.depth > column));
        if (!adjacent.length) return (oldIndex.get(agent.id) ?? 0) / Math.max(1, rows.length - 1);
        return adjacent.reduce((sum, item) => sum + item.index / Math.max(1, item.count - 1), 0) / adjacent.length;
      };
      const scores = new Map(rows.map((agent) => [agent.id, score(agent)]));
      rows.sort((a, b) => (scores.get(a.id) ?? 0) - (scores.get(b.id) ?? 0) || (oldIndex.get(a.id) ?? 0) - (oldIndex.get(b.id) ?? 0));
      refresh(column);
    }
  };
  sweep(true);
  sweep(false);
  sweep(true);
}

function columnLayout(
  agents: CollaborationAgent[],
  relations: CollaborationRelation[],
): Record<string, CollaborationPosition> {
  const ordered = [...agents].sort((a, b) => (a.firstSeenAt ?? 0) - (b.firstSeenAt ?? 0) || compareAgentId(a.id, b.id));
  const byId = new Map(ordered.map((agent) => [agent.id, agent]));
  const depth = columnDepths(ordered, relations);

  const columns = new Map<number, CollaborationAgent[]>();
  for (const agent of ordered) {
    const d = depth.get(agent.id) ?? 1;
    const rows = columns.get(d) || [];
    rows.push(agent);
    columns.set(d, rows);
  }
  for (const rows of columns.values()) {
    rows.sort((a, b) =>
      roleRank(a) - roleRank(b)
      || (a.generation ?? 0) - (b.generation ?? 0)
      || (a.firstSeenAt ?? 0) - (b.firstSeenAt ?? 0)
      || compareAgentId(a.id, b.id));
  }
  minimizeColumnCrossings(columns, depth, relations, byId);

  const rowPitch = LAYOUT.nodeHeight + LAYOUT.rowGap;
  const columnRowCount = (rows: CollaborationAgent[]) => Math.min(rows.length, LAYOUT.columnRows);
  const tallestRows = Math.max(1, ...Array.from(columns.entries())
    .filter(([column]) => column > 0)
    .map(([, rows]) => columnRowCount(rows)));
  const tallest = tallestRows * LAYOUT.nodeHeight + (tallestRows - 1) * LAYOUT.rowGap;

  const positions: Record<string, CollaborationPosition> = {};
  const sortedColumns = Array.from(columns.entries()).sort((a, b) => a[0] - b[0]);
  let cursor = LAYOUT.origin;
  for (const [column, rows] of sortedColumns) {
    const subCount = Math.ceil(rows.length / LAYOUT.columnRows);
    const columnX = cursor;
    cursor += subCount * LAYOUT.nodeWidth + (subCount - 1) * (LAYOUT.columnGap / 2) + LAYOUT.columnGap;
    const visibleRows = columnRowCount(rows);
    const height = visibleRows * LAYOUT.nodeHeight + Math.max(0, visibleRows - 1) * LAYOUT.rowGap;
    const stagger = column > 0 && column % 2 === 0 ? rowPitch / 2 : 0;
    const top = LAYOUT.origin + Math.max(0, (tallest - height) / 2) + stagger;
    rows.forEach((agent, index) => {
      const sub = Math.floor(index / LAYOUT.columnRows);
      const row = index % LAYOUT.columnRows;
      const height2 = nodeHeightOf(agent) - LAYOUT.nodeHeight;
      positions[agent.id] = {
        x: columnX + sub * SUBCOLUMN_STEP,
        y: top + row * rowPitch - Math.max(0, height2) / 2,
      };
    });
  }
  return positions;
}

export function layoutAgents(
  agents: CollaborationAgent[],
  relations: CollaborationRelation[],
  previousPositions: Record<string, CollaborationPosition> = {},
): Record<string, CollaborationPosition> {
  const target = columnLayout(agents, relations);
  const ids = Object.keys(target);
  if (!ids.some((id) => previousPositions[id])) return target;
  const rowPitch = LAYOUT.nodeHeight + LAYOUT.rowGap;
  const positions: Record<string, CollaborationPosition> = {};
  const occupied: CollaborationPosition[] = [];
  const pending: string[] = [];
  for (const id of ids) {
    const previous = previousPositions[id];
    if (previous && previous.x === target[id].x) {
      positions[id] = previous;
      occupied.push(previous);
    } else {
      pending.push(id);
    }
  }
  pending.sort((a, b) => target[a].x - target[b].x || target[a].y - target[b].y);
  for (const id of pending) {
    const slot = { ...target[id] };
    while (occupied.some((taken) => taken.x === slot.x && Math.abs(taken.y - slot.y) < rowPitch)) slot.y += rowPitch;
    positions[id] = slot;
    occupied.push(slot);
  }
  return positions;
}

export const layoutHierarchy = layoutAgents;

export type TimelineLayout = {
  positions: Record<string, CollaborationPosition>;
  widths: Record<string, number>;
  startedAt: number;
  span: number;
  usableWidth: number;
};

const TIMELINE_ROLES: CollaborationAgentRole[] = ["coordinator", "decision", "challenger", "source", "worker", "review", "verifier"];

function roleLane(role: CollaborationAgentRole): number {
  switch (role) {
    case "coordinator": return 0;
    case "decision": return 0.3;
    case "challenger": return 0.45;
    case "source": return 0.6;
    case "worker": return 1;
    case "review": return 2;
    case "verifier": return 3;
    default: {
      const _never: never = role;
      return _never;
    }
  }
}

export function layoutTimeline(
  agents: CollaborationAgent[],
  runStartedAt: number | undefined,
  runFinishedAt: number | undefined,
  now: number,
): TimelineLayout {
  const firstStart = agents.reduce((min, agent) => {
    const start = toEpochMs(agent.firstSeenAt) || toEpochMs(agent.startedAt);
    return start && start < min ? start : min;
  }, now);
  const startedAt = toEpochMs(runStartedAt) || firstStart;
  const end = toEpochMs(runFinishedAt) || now;
  const span = Math.max(1, end - startedAt);
  const usableWidth = LAYOUT.timelineUsableWidth;
  const positions: Record<string, CollaborationPosition> = {};
  const widths: Record<string, number> = {};
  let y = LAYOUT.origin + LAYOUT.timelineRulerHeight + LAYOUT.rowGap;

  const byRole = TIMELINE_ROLES.map((role) => ({
    role,
    members: agents.filter((agent) => agent.role === role).sort((a, b) =>
      ((toEpochMs(a.firstSeenAt) || toEpochMs(a.startedAt)) - (toEpochMs(b.firstSeenAt) || toEpochMs(b.startedAt))) || a.id.localeCompare(b.id)),
  })).sort((a, b) => roleLane(a.role) - roleLane(b.role));

  for (const { role, members } of byRole) {
    if (!members.length) continue;
    type Occupied = { x: number; w: number };
    const lanes: Occupied[][] = [];
    const placed: Array<{ id: string; x: number; w: number; lane: number }> = [];
    for (const agent of members) {
      const start = toEpochMs(agent.firstSeenAt) || toEpochMs(agent.startedAt) || startedAt;
      const stop = toEpochMs(agent.finishedAt) || toEpochMs(agent.endedAt) || end;
      const duration = Math.max(1, stop - start);
      const x = LAYOUT.origin + ((start - startedAt) / span) * usableWidth;
      const w = agent.role === "source" ? LAYOUT.nodeWidth : Math.max(LAYOUT.nodeWidth, (duration / span) * usableWidth);
      let lane = lanes.findIndex((row) =>
        row.every((slot) => x >= slot.x + slot.w + LAYOUT.timelineLaneGap || x + w + LAYOUT.timelineLaneGap <= slot.x));
      if (lane < 0) {
        lane = lanes.length;
        lanes.push([]);
      }
      lanes[lane].push({ x, w });
      placed.push({ id: agent.id, x, w, lane });
    }
    const rowPitch = (role === "coordinator" ? LAYOUT.coordinatorHeight : LAYOUT.nodeHeight) + LAYOUT.rowGap;
    for (const item of placed) {
      widths[item.id] = item.w;
      positions[item.id] = { x: item.x, y: y + item.lane * rowPitch };
    }
    y += lanes.length * rowPitch + LAYOUT.timelineRoleGap;
  }
  return { positions, widths, startedAt, span, usableWidth };
}

export function layoutDiffers(
  positions: Record<string, CollaborationPosition>,
  target: Record<string, CollaborationPosition>,
): boolean {
  return Object.keys(target).some((id) => {
    const current = positions[id];
    return !current || current.x !== target[id].x || current.y !== target[id].y;
  });
}

export function applyExpandedShift(
  positions: Record<string, CollaborationPosition>,
  extras: Record<string, number>,
): Record<string, CollaborationPosition> {
  let any = false;
  for (const id of Object.keys(extras)) {
    if (extras[id] && positions[id]) {
      any = true;
      break;
    }
  }
  if (!any) return positions;

  const columns = new Map<number, string[]>();
  for (const id of Object.keys(positions)) {
    const x = positions[id].x;
    const col = columns.get(x);
    if (col) col.push(id);
    else columns.set(x, [id]);
  }
  const next: Record<string, CollaborationPosition> = { ...positions };
  for (const ids of columns.values()) {
    ids.sort((a, b) => positions[a].y - positions[b].y || a.localeCompare(b));
    let extra = 0;
    for (const id of ids) {
      if (extra) next[id] = { x: positions[id].x, y: positions[id].y + extra };
      extra += extras[id] ?? 0;
    }
  }
  return next;
}

export function applyExpandedLaneShift(
  positions: Record<string, CollaborationPosition>,
  extras: Record<string, number>,
): Record<string, CollaborationPosition> {
  let any = false;
  for (const id of Object.keys(extras)) {
    if (extras[id] && positions[id]) {
      any = true;
      break;
    }
  }
  if (!any) return positions;

  const lanes = new Map<number, string[]>();
  for (const id of Object.keys(positions)) {
    const y = positions[id].y;
    const row = lanes.get(y);
    if (row) row.push(id);
    else lanes.set(y, [id]);
  }
  const next: Record<string, CollaborationPosition> = { ...positions };
  let extra = 0;
  for (const [y, ids] of Array.from(lanes.entries()).sort((a, b) => a[0] - b[0])) {
    if (extra) {
      for (const id of ids) next[id] = { x: positions[id].x, y: y + extra };
    }
    let laneExtra = 0;
    for (const id of ids) laneExtra = Math.max(laneExtra, extras[id] ?? 0);
    extra += laneExtra;
  }
  return next;
}
