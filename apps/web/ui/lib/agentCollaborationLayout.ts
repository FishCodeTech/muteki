/**
 * Layout constants and the column layout for the agent collaboration canvas.
 * Sizes here are the single source: the node card receives them as the node's
 * width / height, and solve-collaboration.css stretches to 100% inside that box.
 */

import {
  COORDINATOR_ID,
  isCollaborationWorker,
  type CollaborationAgent,
  type CollaborationAgentRole,
  type CollaborationRelation,
  type CollaborationRelationKind,
} from "./agentCollaboration";
import { toEpochMs } from "./format";

/**
 * Relation kinds that push the target one column right: an explore → review /
 * verify flow reads left to right. Dispatch, report, directive and lock carry
 * no column semantics and never move a card.
 */
const CAUSAL_KINDS = new Set<CollaborationRelationKind>(["handoff", "review", "verify"]);

export const LAYOUT = {
  nodeWidth: 360,
  nodeHeight: 164,
  columnGap: 190,
  rowGap: 80,
  origin: 72,
  maxDepth: 8,
  columnRows: 10,
  coordinatorHeight: 180,
  groupPadding: 26,
  groupLabelHeight: 30,
  groupGap: 96,
  autoWorkItemRows: 5,
  workItemRowHeight: 40,
  workListGap: 8,
  /** Horizontal span of the timeline axis (cards size as duration / run × this). */
  timelineUsableWidth: 1800,
  timelineRulerHeight: 32,
  timelineRoleGap: 36,
  /** Minimum gap before two overlapping timeline bars share a row. */
  timelineLaneGap: 16,
} as const;

export type CollabCanvasLayout = "hierarchy" | "timeline";

export function agentCardHeight(workItemRows = 0): number {
  return LAYOUT.nodeHeight + (workItemRows > 0 ? LAYOUT.workListGap + workItemRows * LAYOUT.workItemRowHeight : 0);
}

/**
 * Zoom bounds; `compact` is the scale below which a card drops its body rows and
 * CSS rebuilds the remainder as a filled identity tile (see AgentNodeCard /
 * solve-collaboration.css). Locating an agent keeps the current scale unless it
 * is below `centerMin`, then the viewport lifts to `center` so the card is
 * readable without jumping to `fitMax`.
 */
export const ZOOM = { min: 0.06, max: 1.8, fitMax: 1, compact: 0.55, detailEnter: 1.05, detailExit: 0.85, centerMin: 0.5, center: 0.75 } as const;

/**
 * Mini map: hidden by default until the graph holds at least `autoShowNodes`
 * cards (a smaller graph is fully framed by fitView and gains nothing from a
 * thumbnail); the size is passed as the map's style so its viewBox matches.
 */
export const MINIMAP = { autoShowNodes: 8, width: 150, height: 96 } as const;

/**
 * Floor for the fitView zoom. Above ZOOM.min so a 20+ worker layout stops
 * scaling down once cards reach a still-readable size, and the operator pans
 * instead of squinting at unreadable text.
 */
export const FIT_MIN_ZOOM = 0.25;

/**
 * `.artifact` / `.collab-shell` widths that switch docked / overlay / stacked
 * layout. CSS uses the same numbers as `@container runtimepanel`.
 */
export const COLLAB_OVERLAY_MAX = 1100;
export const COLLAB_MOBILE_MAX = 900;
export const COLLAB_WIDE_BREAKPOINT = COLLAB_OVERLAY_MAX;

/** Side-panel widths — keep in sync with the clamp() in solve-collaboration.css. */
export const COLLAB_INDEX_W = { min: 220, ratio: 0.22, max: 286 } as const;
export const COLLAB_DETAIL_W = { min: 280, ratio: 0.28, max: 372 } as const;

export function clampCollabPanelWidth(
  container: number,
  spec: { min: number; ratio: number; max: number },
): number {
  return Math.min(spec.max, Math.max(spec.min, container * spec.ratio));
}

/** Debounce for container resizes before the viewport is re-fitted. */
export const FIT_DELAY = { resizeSettle: 140 } as const;

/** Viewport animation durations (ms); zero when the user prefers reduced motion. */
export const MOTION = { fit: 260, center: 260 } as const;

/** Row caps for the long lists in the index and inspector. */
export const LIST_LIMITS = { activity: 100, knowledge: 120 } as const;

export type CollaborationPosition = { x: number; y: number };

/** Sub-column width step: a wrapped sub-column sits half a column gap to the right. */
const SUBCOLUMN_STEP = LAYOUT.nodeWidth + LAYOUT.columnGap / 2;

function roleRank(agent: CollaborationAgent): number {
  if (agent.role === "worker") return 0;
  if (agent.role === "review") return 1;
  if (agent.role === "verifier") return 2;
  if (agent.role === "coordinator") return -3;
  if (agent.role === "decision") return -2;
  return -1;
}

/** The base column an agent sits in from its role alone, before relations push it right. */
function baseColumn(agent: CollaborationAgent): number {
  if (!isCollaborationWorker(agent)) return 0;
  if (agent.role === "review" || agent.role === "verifier") return 2;
  return 1;
}

function nodeHeightOf(agent: CollaborationAgent): number {
  return agent.id === COORDINATOR_ID ? LAYOUT.coordinatorHeight : LAYOUT.nodeHeight;
}

/**
 * Column depth per agent: the role base column, or one more than the deepest
 * causal predecessor — the longest path over CAUSAL_KINDS relations, memoised
 * so a chain A → B → C → D lands in columns 1/2/3/4. A back edge met while a
 * node is still being resolved is skipped, so a cycle can never inflate depth
 * without bound; the coordinator neither contributes nor moves (column 0).
 */
function columnDepths(agents: CollaborationAgent[], relations: CollaborationRelation[]): Map<string, number> {
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  const incoming = new Map<string, string[]>();
  for (const relation of relations) {
    if (!CAUSAL_KINDS.has(relation.kind) || !isCollaborationWorker(byId.get(relation.source) ?? { role: "source" }) || !isCollaborationWorker(byId.get(relation.target) ?? { role: "source" })) continue;
    if (!byId.has(relation.source) || !byId.has(relation.target) || relation.source === relation.target) continue;
    const sources = incoming.get(relation.target) || [];
    sources.push(relation.source);
    incoming.set(relation.target, sources);
  }

  const depth = new Map<string, number>();
  const visiting = new Set<string>();
  const resolve = (id: string): number => {
    const known = depth.get(id);
    if (known !== undefined) return known;
    visiting.add(id);
    const agent = byId.get(id);
    let best = agent ? baseColumn(agent) : 1;
    for (const source of incoming.get(id) || []) {
      if (visiting.has(source)) continue; // back edge
      best = Math.max(best, Math.min(LAYOUT.maxDepth, resolve(source) + 1));
    }
    visiting.delete(id);
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
): void {
  const neighbors = new Map<string, string[]>();
  for (const relation of relations) {
    if (!CAUSAL_KINDS.has(relation.kind)) continue;
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

/**
 * Column layout: the coordinator in column 0, workers in their role base
 * column, and any agent pushed one column right per causal relation into it.
 * A column taller than LAYOUT.columnRows wraps its overflow into a sub-column
 * half a gap to the right. Columns are centred on the tallest one.
 */
function columnLayout(
  agents: CollaborationAgent[],
  relations: CollaborationRelation[],
): Record<string, CollaborationPosition> {
  const ordered = [...agents].sort((a, b) => (a.firstSeenAt ?? 0) - (b.firstSeenAt ?? 0) || a.id.localeCompare(b.id));
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
      || a.id.localeCompare(b.id));
  }
  minimizeColumnCrossings(columns, depth, relations);

  const rowPitch = LAYOUT.nodeHeight + LAYOUT.rowGap;
  // Tallest column decides the vertical span every column is centred within;
  // overflow sub-columns cap a column at LAYOUT.columnRows tall.
  const columnRowCount = (rows: CollaborationAgent[]) => Math.min(rows.length, LAYOUT.columnRows);
  const tallestRows = Math.max(1, ...Array.from(columns.entries())
    .filter(([column]) => column > 0)
    .map(([, rows]) => columnRowCount(rows)));
  const tallest = tallestRows * LAYOUT.nodeHeight + (tallestRows - 1) * LAYOUT.rowGap;

  const positions: Record<string, CollaborationPosition> = {};
  // Columns are placed left to right with a cursor, so a column that wrapped
  // into sub-columns reserves their full width before the next column starts.
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

/**
 * Positions for every agent. Without `previousPositions` this is the pure
 * column layout. With them (the slots from the last commit) a card that stays
 * in its column keeps its slot — including the same position object — so the
 * operator's viewport is not disturbed; a card that changed column and every
 * new card takes its target slot, sliding down one row pitch at a time past
 * any card already occupying that area (area overlap, not an exact coordinate
 * match, so a 95px offset still counts as taken).
 */
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

/** Column layout under the audit name `layoutHierarchy`. */
export const layoutHierarchy = layoutAgents;

export type TimelineLayout = {
  positions: Record<string, CollaborationPosition>;
  widths: Record<string, number>;
  startedAt: number;
  span: number;
  usableWidth: number;
};

const TIMELINE_ROLES: CollaborationAgentRole[] = ["coordinator", "decision", "source", "worker", "review", "verifier"];

function roleLane(role: CollaborationAgentRole): number {
  switch (role) {
    case "coordinator": return 0;
    case "decision": return 0.3;
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

/**
 * Gantt layout: x is start time, width is duration, y is role then a sub-row
 * when two bars would overlap. Span is (run end or now) − run start.
 */
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

export interface GenerationGroup {
  generation: number;
  /** Absolute-position bounding box of the group box (already padded). */
  x: number;
  y: number;
  width: number;
  height: number;
  agentIds: string[];
  /** Earliest member start; latest member end, unset while any member is still running. */
  startedAt?: number;
  endedAt?: number;
}

export interface GroupedLayout {
  /** Absolute positions for every agent (coordinator kept above the group bands). */
  positions: Record<string, CollaborationPosition>;
  groups: GenerationGroup[];
}

/**
 * Generation-banded layout for the "all executions" view: the coordinator sits
 * on top, then one vertical band per generation (oldest first), each laid out
 * with the same column rules and wrapped in a padded box. Bands are stacked
 * with LAYOUT.groupGap between them so cards from different runs never mix.
 */
export function layoutGroups(
  agents: CollaborationAgent[],
  relations: CollaborationRelation[],
): GroupedLayout {
  const coordinator = agents.find((agent) => agent.id === COORDINATOR_ID);
  const workers = agents.filter((agent) => agent.id !== COORDINATOR_ID);
  const generations = Array.from(new Set(workers.map((agent) => agent.generation)))
    .sort((a, b) => a - b);

  const positions: Record<string, CollaborationPosition> = {};
  const groups: GenerationGroup[] = [];
  if (coordinator) positions[COORDINATOR_ID] = { x: LAYOUT.origin, y: LAYOUT.origin };
  const coordinatorSpan = coordinator ? LAYOUT.coordinatorHeight + LAYOUT.groupGap : 0;
  let cursorY = LAYOUT.origin + coordinatorSpan;

  for (const generation of generations) {
    const members = workers.filter((agent) => agent.generation === generation);
    // Lay the band out in local coordinates, then shift it under the cursor.
    const local = columnLayout(members, relations);
    let minX = Infinity;
    let minY = Infinity;
    let maxX = -Infinity;
    let maxY = -Infinity;
    for (const agent of members) {
      const slot = local[agent.id];
      if (!slot) continue;
      minX = Math.min(minX, slot.x);
      minY = Math.min(minY, slot.y);
      maxX = Math.max(maxX, slot.x + LAYOUT.nodeWidth);
      maxY = Math.max(maxY, slot.y + nodeHeightOf(agent));
    }
    if (!Number.isFinite(minX)) continue;
    const offsetX = LAYOUT.origin + LAYOUT.groupPadding - minX;
    const offsetY = cursorY + LAYOUT.groupLabelHeight + LAYOUT.groupPadding - minY;
    const agentIds: string[] = [];
    let startedAt: number | undefined;
    let endedAt: number | undefined;
    let allEnded = true;
    for (const agent of members) {
      const slot = local[agent.id];
      if (!slot) continue;
      positions[agent.id] = { x: slot.x + offsetX, y: slot.y + offsetY };
      agentIds.push(agent.id);
      if (agent.startedAt && (!startedAt || agent.startedAt < startedAt)) startedAt = agent.startedAt;
      if (agent.endedAt) endedAt = Math.max(endedAt ?? 0, agent.endedAt);
      else allEnded = false;
    }
    const boxWidth = maxX - minX + LAYOUT.groupPadding * 2;
    const boxHeight = maxY - minY + LAYOUT.groupPadding * 2 + LAYOUT.groupLabelHeight;
    groups.push({
      generation,
      x: LAYOUT.origin,
      y: cursorY,
      width: boxWidth,
      height: boxHeight,
      agentIds,
      startedAt,
      endedAt: allEnded ? endedAt : undefined,
    });
    cursorY += boxHeight + LAYOUT.groupGap;
  }
  return { positions, groups };
}

/** True when any card sits somewhere other than where a fresh column layout would put it. */
export function layoutDiffers(
  positions: Record<string, CollaborationPosition>,
  target: Record<string, CollaborationPosition>,
): boolean {
  return Object.keys(target).some((id) => {
    const current = positions[id];
    return !current || current.x !== target[id].x || current.y !== target[id].y;
  });
}

/**
 * Extra height from expanded cards only pushes cards below them in the same
 * column (same x). Cached slots stay put so expand never clears the layout
 * cache or triggers fitView.
 */
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

/**
 * Timeline lanes share a y but not an x. Extra height from an expanded card
 * pushes later lanes down once (the max extra on that lane) and leaves
 * side-by-side cards on the same lane in place.
 */
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

/**
 * After a same-column expand shift, grow each generation box to its members
 * and push later bands down so frames do not overlap.
 */
export function restackGroups(
  groups: GenerationGroup[],
  positions: Record<string, CollaborationPosition>,
  extras: Record<string, number>,
): { positions: Record<string, CollaborationPosition>; groups: GenerationGroup[] } {
  if (!groups.length) return { positions, groups };
  let any = false;
  for (const id of Object.keys(extras)) {
    if (extras[id]) {
      any = true;
      break;
    }
  }
  if (!any) return { positions, groups };

  const nextPos = { ...positions };
  const nextGroups: GenerationGroup[] = [];
  let prevBottom = -Infinity;
  for (const group of groups) {
    let minY = Infinity;
    let maxY = -Infinity;
    let minX = Infinity;
    let maxX = -Infinity;
    for (const id of group.agentIds) {
      const slot = nextPos[id];
      if (!slot) continue;
      const height = LAYOUT.nodeHeight + (extras[id] ?? 0);
      minY = Math.min(minY, slot.y);
      maxY = Math.max(maxY, slot.y + height);
      minX = Math.min(minX, slot.x);
      maxX = Math.max(maxX, slot.x + LAYOUT.nodeWidth);
    }
    if (!Number.isFinite(minY)) {
      nextGroups.push(group);
      continue;
    }
    let y = minY - LAYOUT.groupLabelHeight - LAYOUT.groupPadding;
    const floor = Number.isFinite(prevBottom) ? prevBottom + LAYOUT.groupGap : y;
    const lift = floor - y;
    if (lift > 0) {
      y += lift;
      for (const id of group.agentIds) {
        const slot = nextPos[id];
        if (slot) nextPos[id] = { x: slot.x, y: slot.y + lift };
      }
      maxY += lift;
    }
    nextGroups.push({
      ...group,
      y,
      height: maxY + LAYOUT.groupPadding - y,
      width: Number.isFinite(minX) ? Math.max(group.width, maxX - minX + LAYOUT.groupPadding * 2) : group.width,
    });
    prevBottom = y + (maxY + LAYOUT.groupPadding - y);
  }
  return { positions: nextPos, groups: nextGroups };
}
