"use client";

import {
  BaseEdge,
  Handle,
  Position,
  getBezierPath,
  getSmoothStepPath,
  getStraightPath,
  useInternalNode,
  useStore,
  type EdgeProps,
  type InternalNode,
  type Node,
  type NodeProps,
  type ReactFlowState,
} from "@xyflow/react";
import { Tooltip } from "@heroui/react";
import { useContext, useEffect, useRef, useState, type CSSProperties, type RefObject } from "react";

import { Icon } from "@/components/Icon";
import { EngineLogo } from "@/components/EngineLogo";
import { RECENT_EVENT_WINDOW_MS, recencyOf, isCollaborationWorker, recordedAmount, type CollaborationAgentRole, type CollaborationKnowledgeItem, type CollaborationKnowledgeKind, type CollaborationRelationKind } from "@/lib/agentCollaboration";
import { LAYOUT, ZOOM } from "@/lib/agentCollaborationLayout";
import { compactNumber, formatClock, formatElapsed, formatIsoDuration } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { COARSE_TICKER, SECOND_TICKER, useTicker } from "@/lib/ticker";
import { usePrefersReducedMotion } from "@/lib/usePrefersReducedMotion";
import type { LaneStatusKind } from "@/lib/workerLanePresentation";
import type { RunCanvasMode } from "@/lib/swarmProjection";

import { highlight, HighlightQueryContext } from "@/lib/textHighlight";

import { useCollaborationActions } from "./CollaborationActions";
import { AgentSparkline } from "./AgentSparkline";
import { knowledgeIcon, knowledgeLabel } from "./collaborationPresentation";
import { TimelineRuler } from "./TimelineRuler";

/**
 * Plain display values only — no agent object, no callbacks, no clock. The
 * canvas derives a signature from NODE_SIGNATURE_FIELDS and hands React Flow
 * the previous node object whenever nothing listed there changed; that is
 * what stops every card from being re-measured on every event.
 */
export type AgentNodeData = {
  id: string;
  title: string;
  initial: string;
  /** EngineLogo key; empty when the engine cannot be resolved. */
  engineKey: string;
  subtitle: string;
  engine: string;
  /** CSS colour expression (`var(--eng-…)`), consumed as `--agent-color`. */
  color: string;
  role: CollaborationAgentRole;
  roleLabel: string;
  statusKind: LaneStatusKind;
  statusLabel: string;
  intentText: string;
  intentTitle: string;
  latestActivity: string;
  facts: number;
  observations: number;
  candidates: number;
  deadEnds: number;
  locks: number;
  tokens: number;
  usd: number;
  unpricedCalls?: number;
  model: string;
  profileLabel: string;
  /** Lifecycle bounds in epoch ms (0 when unknown); `live` keeps the elapsed clock ticking. */
  startedAt: number;
  endedAt: number;
  live: boolean;
  /** The worker process is still up; with statusKind and lastEventAt this drives the idle judgement. */
  online: boolean;
  isCurrent: boolean;
  lastEventAt: number;
  /** Newest board-level step (ms, 0 = none); drives the one-shot "recent progress" glow. */
  lastProgressAt: number;
  lifecycleSource: string;
  selected: boolean;
  dimmed: boolean;
  /** Relation kind when this card is an endpoint of the selected edge; empty otherwise. */
  relationEndpoint: CollaborationRelationKind | "";
  /** Generation badge on the card head; shown only for generation > 1. */
  generation: number;
  expanded: boolean;
  manualExpanded: boolean;
  selectedKnowledgeId: string;
  workItems: readonly AgentWorkItemRow[];
  visibleWorkItemCount: number;
  sparklineEvents: readonly number[];
  sparklineTokens: readonly number[];
  sparklinePartial: boolean;
  /** Gantt stripe under the card (timeline layout). */
  durationBar: boolean;
  isAnomaly: boolean;
  /** Translated stop reason when the lane ended on oom / timeout / error / budget. */
  anomalyReason: string;
  pendingHitl: number;
  firstSeenAt: number;
  finishedAt: number;
  canvasMode?: RunCanvasMode;
};

export type AgentWorkItemRow = {
  id: string;
  kind: CollaborationKnowledgeKind;
  title: string;
  status: string;
  statusKey?: string;
  tone: CollaborationKnowledgeItem["tone"];
};

const EMPTY_WORK_ITEMS: readonly AgentWorkItemRow[] = [];

export function agentWorkItemsOf(items: readonly CollaborationKnowledgeItem[]): readonly AgentWorkItemRow[] {
  if (!items.length) return EMPTY_WORK_ITEMS;
  return [...items]
    .sort((a, b) => a.appearedTs - b.appearedTs || a.ts - b.ts || a.id.localeCompare(b.id))
    .map((row) => ({ id: row.id, kind: row.kind, title: row.title, status: row.status || "", statusKey: row.statusKey, tone: row.tone }));
}

/** Every field of AgentNodeData, in declaration order; the node signature is built from these. */
export const NODE_SIGNATURE_FIELDS = [
  "id", "title", "initial", "engineKey", "subtitle", "engine", "color",
  "role", "roleLabel", "statusKind", "statusLabel",
  "intentText", "intentTitle", "latestActivity",
  "facts", "candidates", "deadEnds", "locks", "tokens", "usd", "unpricedCalls", "model", "profileLabel",
  "startedAt", "endedAt", "live", "online", "isCurrent", "lastEventAt", "lastProgressAt", "lifecycleSource",
  "selected", "dimmed", "relationEndpoint", "generation",
  "expanded", "manualExpanded", "selectedKnowledgeId", "visibleWorkItemCount",
  "sparklinePartial", "durationBar",
  "isAnomaly", "anomalyReason", "pendingHitl", "firstSeenAt", "finishedAt", "canvasMode",
] as const satisfies readonly (keyof AgentNodeData)[];

export function nodeSignature(data: AgentNodeData): string {
  return NODE_SIGNATURE_FIELDS.map((field) => String(data[field])).join("")
    + data.workItems.map((row) => [row.id, row.kind, row.title, row.status, row.statusKey, row.tone].join("\x1e")).join("\x1f")
    + data.sparklineEvents.join(",")
    + data.sparklineTokens.join(",");
}

export type AgentFlowNode = Node<AgentNodeData>;

/** The coordinator carries dispatch metrics rather than a worker card body. */
export type CoordinatorNodeData = {
  id: string;
  title: string;
  initial: string;
  color: string;
  statusKind: LaneStatusKind;
  statusLabel: string;
  phaseLabel: string;
  generation: number;
  onlineWorkers: number;
  totalWorkers: number;
  openIntents: number;
  verifiedFacts: number;
  reasonRounds: number;
  latestActivity: string;
  startedAt: number;
  endedAt: number;
  live: boolean;
  selected: boolean;
  dimmed: boolean;
  relationEndpoint: CollaborationRelationKind | "";
  durationBar: boolean;
  degradedCount: number;
  preflightCount: number;
  firstSeenAt: number;
  finishedAt: number;
  canvasMode?: RunCanvasMode;
};

export const COORDINATOR_SIGNATURE_FIELDS = [
  "id", "title", "initial", "color", "statusKind", "statusLabel", "phaseLabel",
  "generation", "onlineWorkers", "totalWorkers", "openIntents", "verifiedFacts",
  "reasonRounds", "latestActivity", "startedAt", "endedAt", "live", "selected", "dimmed", "relationEndpoint", "durationBar",
  "degradedCount", "preflightCount", "firstSeenAt", "finishedAt", "canvasMode",
] as const satisfies readonly (keyof CoordinatorNodeData)[];

export function coordinatorSignature(data: CoordinatorNodeData): string {
  return COORDINATOR_SIGNATURE_FIELDS.map((field) => String(data[field])).join("");
}

export type CoordinatorFlowNode = Node<CoordinatorNodeData>;

/** A generation grouping box behind a band of worker cards ("all executions" view). */
export type GroupNodeData = { label: string };
export type GroupFlowNode = Node<GroupNodeData>;

function ElapsedTime({ start, end, live }: { start: number; end: number; live: boolean }) {
  const t = useT();
  // Cards with an end time never subscribe to the shared second ticker.
  const tick = useTicker(SECOND_TICKER, live && !end);
  if (!start) return null;
  const stop = end || (live ? tick : Date.now());
  const elapsedMs = Math.max(0, stop - start);
  const elapsedText = formatElapsed(elapsedMs);
  return (
    <time dateTime={formatIsoDuration(elapsedMs)} aria-label={`${t("collab.elapsed")} ${elapsedText}`}>
      {elapsedText}
    </time>
  );
}

/** Four floating handles per side; the floating edge picks the closest pair, so no handle id is needed. */
function FloatingHandles() {
  return (
    <>
      <Handle type="target" position={Position.Top} className="collab-handle" isConnectable={false} />
      <Handle type="source" position={Position.Top} id="t" className="collab-handle" isConnectable={false} />
      <Handle type="target" position={Position.Right} id="r" className="collab-handle" isConnectable={false} />
      <Handle type="source" position={Position.Right} className="collab-handle" isConnectable={false} />
      <Handle type="target" position={Position.Bottom} id="b" className="collab-handle" isConnectable={false} />
      <Handle type="source" position={Position.Bottom} id="sb" className="collab-handle" isConnectable={false} />
      <Handle type="target" position={Position.Left} className="collab-handle" isConnectable={false} />
      <Handle type="source" position={Position.Left} id="l" className="collab-handle" isConnectable={false} />
    </>
  );
}

/**
 * `.recent` for RECENT_EVENT_WINDOW_MS after lastProgressAt moves (never on
 * mount). Another step inside an open window only extends the window; the
 * glow keyframe restarts only once the previous window has closed.
 */
function useRecentFlag(element: RefObject<HTMLDivElement | null>, lastProgressAt: number): boolean {
  const [recent, setRecent] = useState(false);
  const seen = useRef(lastProgressAt);
  const timer = useRef(0);
  useEffect(() => {
    if (lastProgressAt === seen.current) return;
    seen.current = lastProgressAt;
    if (!lastProgressAt) return;
    const windowOpen = timer.current !== 0;
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      timer.current = 0;
      setRecent(false);
    }, RECENT_EVENT_WINDOW_MS);
    if (windowOpen) return;
    setRecent(true);
    const node = element.current;
    if (node) {
      node.style.animation = "none";
      void node.offsetWidth;
      node.style.animation = "";
    }
  }, [element, lastProgressAt]);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  return recent;
}

/** Mount-time enter and finishedAt-change exit; judged once, no second ticker. */
export const AGENT_ENTER_MS = 5000;

function useEnterExit(firstSeenAt: number, finishedAt: number): { entering: boolean; exiting: boolean } {
  const reduceMotion = usePrefersReducedMotion();
  const [entering] = useState(() => firstSeenAt > 0 && Date.now() - firstSeenAt < AGENT_ENTER_MS);
  const [exiting, setExiting] = useState(false);
  const seenFinished = useRef(finishedAt);
  useEffect(() => {
    if (finishedAt === seenFinished.current) return;
    seenFinished.current = finishedAt;
    if (finishedAt) setExiting(true);
  }, [finishedAt]);
  return { entering: entering && !reduceMotion, exiting: exiting && !reduceMotion };
}

function AgentBadges({
  anomaly,
  anomalyReason,
  pendingHitl,
  degradedCount,
  preflightCount,
}: {
  anomaly?: boolean;
  anomalyReason?: string;
  pendingHitl?: number;
  degradedCount?: number;
  preflightCount?: number;
}) {
  const t = useT();
  const hitl = pendingHitl ?? 0;
  const degraded = degradedCount ?? 0;
  const preflight = preflightCount ?? 0;
  if (!anomaly && !hitl && !degraded && !preflight) return null;
  return (
    <span className="collab-agent-badges">
      {anomaly && (
        <span className="collab-agent-badge danger" title={anomalyReason || undefined}>
          <Icon name="alert" size={10} />
          {anomalyReason ? <b>{anomalyReason}</b> : null}
        </span>
      )}
      {hitl > 0 && (
        <span className="collab-agent-badge" title={t("collab.pendingHitl", { n: hitl })}>
          <Icon name="help" size={10} />
          <b>{hitl}</b>
        </span>
      )}
      {degraded > 0 && (
        <span className="collab-agent-badge" title={t("collab.degradedEngines", { n: degraded })}>
          <Icon name="alert" size={10} />
          <b>{degraded}</b>
        </span>
      )}
      {preflight > 0 && (
        <span className="collab-agent-badge danger" title={t("collab.preflightFailures", { n: preflight })}>
          <Icon name="xCircle" size={10} />
          <b>{preflight}</b>
        </span>
      )}
    </span>
  );
}

/**
 * Below ZOOM.compact the card keeps its box but drops the intent / activity /
 * foot rows and CSS rebuilds the remainder as a filled identity tile (large
 * avatar + title + status). The selector yields a boolean, so cards re-render
 * only when the zoom crosses the threshold, never on every pan or zoom step.
 */
const selectCompact = (state: ReactFlowState) => state.transform[2] < ZOOM.compact;

/**
 * Focus, aria-label and click / Enter handling belong to React Flow's node
 * wrapper. The work-list controls stop pointer events so they do not select
 * the node.
 */
export function AgentNodeCard({ data }: NodeProps<AgentFlowNode>) {
  const t = useT();
  const q = useContext(HighlightQueryContext);
  const { toggleExpand, showAllWorkItems, selectPreviewKnowledge } = useCollaborationActions();
  const cardRef = useRef<HTMLDivElement | null>(null);
  const compact = useStore(selectCompact);
  const recent = useRecentFlag(cardRef, data.lastProgressAt);
  const quiet = data.online && (data.statusKind === "thinking" || data.statusKind === "waiting");
  const now = useTicker(COARSE_TICKER, quiet);
  const recency = quiet ? recencyOf(data, now) : undefined;
  const idle = !!recency?.idle;
  const stateKind = idle ? "idle" : data.statusKind;
  const statusText = idle ? t("collab.idleFor", { m: Math.floor((recency?.sinceMs ?? 0) / 60_000) }) : data.statusLabel;
  const { entering, exiting } = useEnterExit(data.firstSeenAt, data.finishedAt);
  const timeRange = t("collab.elapsedRange", {
    start: formatClock(data.startedAt, "—"),
    end: formatClock(data.endedAt, "—"),
  });
  const visibleItems = data.expanded ? data.workItems.slice(-data.visibleWorkItemCount) : [];
  const hiddenItems = Math.max(0, data.workItems.length - visibleItems.length);
  const nodeClass = `collab-agent-node status-${data.statusKind} role-${data.role} ${compact ? "compact" : ""} ${data.expanded ? "expanded" : ""} ${idle ? "status-idle" : ""} ${recent ? "recent" : ""} ${entering ? "entering" : ""} ${exiting ? "exiting" : ""} ${data.selected ? "selected" : ""} ${data.dimmed ? "dimmed" : ""} ${data.relationEndpoint ? `relation-endpoint relation-${data.relationEndpoint}` : ""}`;
  return (
    <Tooltip delay={300} closeDelay={60}>
      <Tooltip.Trigger className="collab-node-tip-trigger" role="presentation" tabIndex={-1}>
        <div className={nodeClass} style={{ "--agent-color": data.color } as CSSProperties}>
          <FloatingHandles />
          <div ref={cardRef} className="collab-agent-card">
            <div className="collab-agent-head">
              <span className="collab-agent-kicker">
                <span className="collab-agent-avatar" aria-hidden="true">
                  {data.engineKey
                    ? <EngineLogo engine={data.engineKey} size={12} />
                    : data.role === "decision" || data.role === "source"
                      ? <Icon name={data.role === "decision" ? "sparkles" : "file"} size={12} />
                      : data.initial}
                </span>
                <b>{data.roleLabel}</b>
                <span>· {data.engine}{data.generation > 1 ? ` · G${data.generation}` : ""}</span>
              </span>
              <span className={`collab-agent-state state-${stateKind}`}><i /><span>{statusText}</span></span>
              <AgentBadges anomaly={data.isAnomaly} anomalyReason={data.anomalyReason} pendingHitl={data.pendingHitl} />
            </div>
            <strong className="collab-agent-title">{highlight(data.title, q)}</strong>
            {!compact && (
              <>
                <div className="collab-agent-intent" title={data.intentTitle || data.intentText}><span>{highlight(data.intentText, q)}</span></div>
                <div className="collab-agent-activity">
                  <Icon name={data.latestActivity ? "terminal" : "clock"} size={11} />
                  <span>{data.latestActivity || "—"}</span>
                  {isCollaborationWorker(data) && (data.live || data.selected) && <AgentSparkline events={data.sparklineEvents} tokens={data.sparklineTokens} partial={data.sparklinePartial} />}
                </div>
                {isCollaborationWorker(data) && <div className="collab-agent-foot">
                  {data.facts > 0 && <span className="ok"><Icon name="check" size={10} />{data.facts}</span>}
                  {data.canvasMode === "ctf" && data.observations > 0 && <span className="warn"><Icon name="help" size={10} />{data.observations}</span>}
                  {data.canvasMode !== "ctf" && data.candidates > 0 && <span className="warn"><Icon name="help" size={10} />{data.candidates}</span>}
                  {data.deadEnds > 0 && <span className="bad"><Icon name="x" size={10} />{data.deadEnds}</span>}
                  {data.canvasMode !== "ctf" && data.locks > 0 && <span className="warn"><Icon name="lock" size={10} />{data.locks}</span>}
                  <ElapsedTime start={data.startedAt} end={data.endedAt} live={data.live} />
                </div>}
                {data.workItems.length > 0 && (
                  <button
                    type="button"
                    className="collab-agent-expand"
                    aria-label={t(data.expanded ? "collab.hideWorkItems" : "collab.showWorkItems")}
                    aria-expanded={data.expanded}
                    onPointerDown={(event) => event.stopPropagation()}
                    onClick={(event) => { event.stopPropagation(); toggleExpand(data.id); }}
                  >
                    <span>{t(data.expanded ? "collab.hideWorkItems" : "collab.showWorkItems")} <b>{data.workItems.length}</b></span>
                    <Icon name="chevronDown" size={12} />
                  </button>
                )}
              </>
            )}
            {data.durationBar && (
              <span className="collab-agent-duration" title={t("collab.durationBar", { start: formatClock(data.startedAt, "—"), end: formatClock(data.endedAt, "—") })}><i /></span>
            )}
          </div>
          {!compact && data.expanded && (
            <div className={`collab-agent-worklist ${data.manualExpanded ? "full" : "preview"}`} aria-label={t("collab.workItems")}>
              {visibleItems.map((row) => (
                <button
                  key={row.id}
                  type="button"
                  className={`collab-agent-workitem tone-${row.tone} ${data.selectedKnowledgeId === row.id ? "selected" : ""}`}
                  title={row.title}
                  onPointerDown={(event) => event.stopPropagation()}
                  onClick={(event) => { event.stopPropagation(); selectPreviewKnowledge(row.id); }}
                >
                  <Icon name={knowledgeIcon(row.kind)} size={13} />
                  <span>{row.title}</span>
                  <b>{row.statusKey ? t(row.statusKey) : row.status || knowledgeLabel(row.kind, t, data.canvasMode)}</b>
                  <Icon name="chevronRight" size={11} />
                </button>
              ))}
              {hiddenItems > 0 && (
                <button
                  type="button"
                  className="collab-agent-workitem collab-agent-show-all"
                  onPointerDown={(event) => event.stopPropagation()}
                  onClick={(event) => { event.stopPropagation(); showAllWorkItems(data.id); }}
                >
                  <Icon name="plus" size={12} /><span>{t("collab.moreWorkItems", { n: hiddenItems })}</span>
                </button>
              )}
            </div>
          )}
        </div>
      </Tooltip.Trigger>
      <Tooltip.Content className="collab-node-tip">
        <strong>{data.intentTitle || "—"}</strong>
        <span>{t("collab.field.model")} · {data.model || "—"}</span>
        <span>{t("collab.field.profile")} · {data.profileLabel || "—"}</span>
        <span>{t("meta.tokens")} · {compactNumber(data.tokens)}</span>
        <span>记账金额 · {recordedAmount(data.usd, data.tokens, data.unpricedCalls)}</span>
        <span>{timeRange}</span>
      </Tooltip.Content>
    </Tooltip>
  );
}

/** Coordinator metric cell: a zero reads as absence, so it is muted rather than highlighted. */
function CoordinatorMetric({ value, text, label }: { value: number; text?: string; label: string }) {
  return <span><b className={value ? "" : "zero"}>{text ?? value}</b><small>{label}</small></span>;
}

/**
 * The coordinator's own node type: a taller card whose body shows dispatch
 * state (phase, generation, online workers, unassigned intents, verified facts,
 * reason rounds) instead of the six evidence counters.
 */
export function CoordinatorNodeCard({ data }: NodeProps<CoordinatorFlowNode>) {
  const t = useT();
  const q = useContext(HighlightQueryContext);
  const compact = useStore(selectCompact);
  const { entering, exiting } = useEnterExit(data.firstSeenAt, data.finishedAt);
  const timeRange = t("collab.elapsedRange", {
    start: formatClock(data.startedAt, "—"),
    end: formatClock(data.endedAt, "—"),
  });
  return (
    <Tooltip delay={300} closeDelay={60}>
      <Tooltip.Trigger className="collab-node-tip-trigger" role="presentation" tabIndex={-1}>
    <div
      className={`collab-coordinator-card status-${data.statusKind} ${compact ? "compact" : ""} ${entering ? "entering" : ""} ${exiting ? "exiting" : ""} ${data.selected ? "selected" : ""} ${data.dimmed ? "dimmed" : ""} ${data.relationEndpoint ? `relation-endpoint relation-${data.relationEndpoint}` : ""}`}
      style={{ "--agent-color": data.color } as CSSProperties}
    >
      <FloatingHandles />
      <div className="collab-coordinator-head">
        <span className="collab-coordinator-avatar"><Icon name="network" size={16} /></span>
        <span className="collab-coordinator-identity">
          <strong>{highlight(data.title, q)}</strong>
          <small>{t("collab.coordinator.program")}</small>
        </span>
        <span className={`collab-agent-state state-${data.statusKind}`}><i /><span>{data.statusLabel}</span></span>
        <AgentBadges degradedCount={data.degradedCount} preflightCount={data.preflightCount} />
      </div>
      {!compact && (
        <>
          <div className="collab-coordinator-metrics">
            <CoordinatorMetric value={data.totalWorkers} text={`${data.onlineWorkers}/${data.totalWorkers}`} label={t("collab.coord.online")} />
            <CoordinatorMetric value={data.openIntents} label={t(data.canvasMode === "ctf" ? "collab.coord.pendingCtf" : "collab.coord.pending")} />
            <CoordinatorMetric value={data.verifiedFacts} label={t(data.canvasMode === "ctf" ? "collab.coord.facts" : "collab.coord.verified")} />
            <CoordinatorMetric value={data.reasonRounds} label={t("collab.coord.reason")} />
          </div>
          <div className="collab-coordinator-foot">
            <span className="collab-coordinator-activity">
              <Icon name="radio" size={11} />
              <span>{data.latestActivity || "—"}</span>
            </span>
            <ElapsedTime start={data.startedAt} end={data.endedAt} live={data.live} />
          </div>
        </>
      )}
      {data.durationBar && (
        <span
          className="collab-agent-duration"
          title={t("collab.durationBar", { start: formatClock(data.startedAt, "—"), end: formatClock(data.endedAt, "—") })}
        >
          <i />
        </span>
      )}
    </div>
      </Tooltip.Trigger>
      <Tooltip.Content className="collab-node-tip">
        <strong>{data.phaseLabel || "—"}</strong>
        <span>{data.latestActivity || "—"}</span>
        <span>{timeRange}</span>
      </Tooltip.Content>
    </Tooltip>
  );
}

export function GroupNode({ data }: NodeProps<GroupFlowNode>) {
  return (
    <div className="collab-group">
      <span className="collab-group-label">{data.label}</span>
    </div>
  );
}

export const NODE_TYPES = { agent: AgentNodeCard, coordinator: CoordinatorNodeCard, group: GroupNode, ruler: TimelineRuler };

export type CollabEdgeGeometry = "bezier" | "straight" | "smoothstep";
export type CollabEdgeData = { geometry: CollabEdgeGeometry; sourcePort?: number; targetPort?: number };

function collabEdgePath(
  geometry: CollabEdgeGeometry,
  args: {
    sourceX: number;
    sourceY: number;
    targetX: number;
    targetY: number;
    sourcePosition: Position;
    targetPosition: Position;
  },
): [string, number, number] {
  switch (geometry) {
    case "straight": {
      const [path, x, y] = getStraightPath({ sourceX: args.sourceX, sourceY: args.sourceY, targetX: args.targetX, targetY: args.targetY });
      return [path, x, y];
    }
    case "smoothstep": {
      const [path, x, y] = getSmoothStepPath(args);
      return [path, x, y];
    }
    case "bezier": {
      const [path, x, y] = getBezierPath(args);
      return [path, x, y];
    }
    default: {
      const _never: never = geometry;
      return _never;
    }
  }
}

/**
 * Floating edge: expanded work items reserve layout height, but relations stay
 * attached to the fixed summary card. Port offsets fan same-source / same-target
 * curves across that card instead of stacking them on one centre point.
 */
function nodeBox(node: InternalNode): { left: number; top: number; x: number; y: number; w: number; h: number } {
  const w = node.measured.width ?? LAYOUT.nodeWidth;
  const measuredHeight = node.measured.height ?? LAYOUT.nodeHeight;
  const h = node.type === "coordinator" ? Math.min(measuredHeight, LAYOUT.coordinatorHeight) : Math.min(measuredHeight, LAYOUT.nodeHeight);
  const left = node.internals.positionAbsolute.x;
  const top = node.internals.positionAbsolute.y;
  return { left, top, x: left + w / 2, y: top + h / 2, w, h };
}

function edgePoints(source: InternalNode, target: InternalNode, sourcePort: number, targetPort: number) {
  const a = nodeBox(source);
  const b = nodeBox(target);
  const horizontal = Math.abs(b.x - a.x) >= Math.max(a.w, b.w) * 0.3;
  if (horizontal) {
    const rightward = b.x >= a.x;
    return {
      source: { x: rightward ? a.left + a.w : a.left, y: a.y + sourcePort * Math.max(0, a.h - 36), position: rightward ? Position.Right : Position.Left },
      target: { x: rightward ? b.left : b.left + b.w, y: b.y + targetPort * Math.max(0, b.h - 36), position: rightward ? Position.Left : Position.Right },
    };
  }
  const downward = b.y >= a.y;
  return {
    source: { x: a.x + sourcePort * Math.max(0, a.w - 60), y: downward ? a.top + a.h : a.top, position: downward ? Position.Bottom : Position.Top },
    target: { x: b.x + targetPort * Math.max(0, b.w - 60), y: downward ? b.top : b.top + b.h, position: downward ? Position.Top : Position.Bottom },
  };
}

export function FloatingEdge({ id, source, target, markerEnd, style, data, ...rest }: EdgeProps) {
  const sourceNode = useInternalNode(source);
  const targetNode = useInternalNode(target);
  if (!sourceNode || !targetNode) return null;
  const edgeData = data as CollabEdgeData | undefined;
  const points = edgePoints(sourceNode, targetNode, edgeData?.sourcePort ?? 0, edgeData?.targetPort ?? 0);
  const sourcePoint = points.source;
  const targetPoint = points.target;
  const geometry: CollabEdgeGeometry = edgeData?.geometry ?? "bezier";
  const [path, labelX, labelY] = collabEdgePath(geometry, {
    sourceX: sourcePoint.x,
    sourceY: sourcePoint.y,
    sourcePosition: sourcePoint.position,
    targetX: targetPoint.x,
    targetY: targetPoint.y,
    targetPosition: targetPoint.position,
  });
  return (
    <BaseEdge
      id={id}
      path={path}
      labelX={labelX}
      labelY={labelY}
      markerEnd={markerEnd}
      style={style}
      label={rest.label}
      labelStyle={rest.labelStyle}
      labelShowBg
      labelBgPadding={rest.labelBgPadding}
      labelBgBorderRadius={rest.labelBgBorderRadius}
      labelBgStyle={rest.labelBgStyle}
    />
  );
}

export const EDGE_TYPES = { floating: FloatingEdge };
