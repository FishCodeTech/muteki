"use client";

import {
  Background,
  BackgroundVariant,
  MarkerType,
  MiniMap,
  ReactFlow,
  ReactFlowProvider,
  useNodesInitialized,
  useReactFlow,
  useStore,
  useStoreApi,
  type AriaLabelConfig,
  type Edge,
  type EdgeChange,
  type Node,
  type NodeChange,
} from "@xyflow/react";
import { useCallback, useDeferredValue, useEffect, useMemo, useRef, useState, type CSSProperties, type MouseEvent } from "react";

import { Icon } from "@/components/Icon";
import {
  ACTIVITY_FILTER_WINDOW_MS,
  buildAgentCollaborationModel,
  COORDINATOR_ID,
  DECISION_ID,
  INPUT_SOURCE_ID,
  isCollaborationWorker,
  OPERATOR_ID,
  RELATION_RECENT_WINDOW_MS,
  recencyOf,
  type CollaborationAgent,
  type CollaborationKnowledgeItem,
  type CollaborationKnowledgeKind,
  type CollaborationRelationKind,
  type CollaborationScope,
} from "@/lib/agentCollaboration";
import {
  agentPresentAt,
} from "@/lib/agentCollaborationReplay";
import {
  LAYOUT,
  MINIMAP,
  MOTION,
  ZOOM,
  FIT_MIN_ZOOM,
  agentCardHeight,
  applyExpandedLaneShift,
  applyExpandedShift,
  layoutAgents,
  layoutDiffers,
  layoutTimeline,
  type CollabCanvasLayout,
  type CollaborationPosition,
} from "@/lib/agentCollaborationLayout";
import { sparklineSeriesByAgent } from "@/lib/agentSparkline";
import { downloadCollaborationJson, downloadCollaborationPng } from "@/lib/collabExport";
import type { BlackboardIntent, DeckState } from "@/lib/events";
import {
  cancelCollabUrlWrite,
  readCollabUrlState,
  scheduleCollabUrlWrite,
} from "@/lib/collabUrlState";
import {
  collabSelectionFromSession,
  loadCollabView,
  patchCollabMemory,
  readCollabLayout,
  writeCollabLayout,
  writeCollabPrefs,
  writeCollabSession,
  type CollabViewLoad,
} from "@/lib/collabViewState";
import { formatClock, formatElapsed, toEpochMs } from "@/lib/format";
import type { IconName } from "@/lib/iconNames";
import { useT } from "@/lib/i18n";
import { canvasModeOf } from "@/lib/swarmProjection";
import { knowledgeStatusLabel } from "@/lib/statusLabels";
import { HighlightQueryContext, matchesSearch } from "@/lib/textHighlight";
import { COARSE_TICKER, useTicker } from "@/lib/ticker";
import { usePrefersReducedMotion } from "@/lib/usePrefersReducedMotion";
import {
  actorDisplayTitle,
  formatWorkerSubtitle,
  toWorkerIdentity,
  workerColorVar,
  workerDisplayName,
  workerEngine,
  workerEngineKey,
  type WorkerIdentity,
} from "@/lib/workers";

import { AgentInspector } from "./collaboration/AgentInspector";
import {
  EDGE_TYPES,
  NODE_TYPES,
  AGENT_ENTER_MS,
  agentWorkItemsOf,
  coordinatorSignature,
  nodeSignature,
  type AgentFlowNode,
  type AgentNodeData,
  type CollabEdgeData,
  type CollabEdgeGeometry,
  type CoordinatorFlowNode,
  type CoordinatorNodeData,
  type GroupFlowNode,
} from "./collaboration/AgentNodeCard";
import { CanvasBar, MiniMapToggle } from "./collaboration/CanvasChrome";
import { CollaborationScrubber } from "./collaboration/CollaborationScrubber";
import { CollaborationActionsProvider } from "./collaboration/CollaborationActions";
import { CollaborationContextMenu, type CollaborationContextMenuState } from "./collaboration/CollaborationContextMenu";
import { CollaborationIndex } from "./collaboration/CollaborationIndex";
import { EdgeHoverTip } from "./collaboration/EdgeHoverTip";
import { CollaborationToolbar } from "./collaboration/CollaborationToolbar";
import { EventStampProvider } from "./collaboration/EventStamp";
import { FlowMatrix } from "./collaboration/FlowMatrix";
import { TIMELINE_RULER_ID, type RulerFlowNode } from "./collaboration/TimelineRuler";
import { coordinatorStatus, knowledgeLabel, RELATION_KINDS, relationLabel, roleLabel, statusLabel, type AgentDisplay } from "./collaboration/collaborationPresentation";
import { knowledgeKindCounts } from "./collaboration/KnowledgeKindFilter";
import { RelationInspector } from "./collaboration/RelationInspector";
import { SidebarResizer } from "./collaboration/SidebarResizer";
import { FIT_VIEW_OPTIONS, useCollaborationFit } from "./collaboration/useCollaborationFit";
import {
  COLLAB_DETAIL_WIDTH_DEFAULT,
  COLLAB_INDEX_WIDTH_DEFAULT,
  useCollaborationSidebarResize,
} from "./collaboration/useCollaborationSidebarResize";
import { useCollaborationShortcuts } from "./collaboration/useCollaborationShortcuts";
import { handleTabListKeyDown } from "./collaboration/tablist";
import { MOBILE_VIEWS, useCollaborationLayout } from "./collaboration/useCollaborationLayout";
import { useCollaborationReplay } from "./collaboration/useCollaborationReplay";
import { COORDINATOR_INSPECTOR_TABS, INITIAL_SELECTION, INSPECTOR_TABS, useCollaborationSelection } from "./collaboration/useCollaborationSelection";

type FocusTarget = { id: string; nonce: number };

type CanvasProps = {
  deck: DeckState;
  running: boolean;
  onKillWorker?: (id: string) => void;
  onOpenTimeline?: (id: string) => void;
  onOpenWorker?: (id: string) => void;
  onSpawnWorker?: (engine?: string) => void;
  onOpenFact?: (seq: number) => void;
  onOpenPoc?: (id: string) => void;
  focusAgent?: FocusTarget | null;
  focusKnowledge?: FocusTarget | null;
};

const FIELD_SEPARATOR = "";
const PRO_OPTIONS = { hideAttribution: true };
const DEFAULT_EDGE_OPTIONS = { type: "floating" };
const EDGE_MARKER = { type: MarkerType.ArrowClosed, width: 15, height: 15 };
const EDGE_LABEL_STYLE = { fontSize: 10.5, fontWeight: 650 };
const EDGE_LABEL_PADDING: [number, number] = [5, 3];
const RELATED_STYLE = { opacity: 1 };
const UNRELATED_STYLE = { opacity: 0.12 };
const COORDINATOR_UNRELATED_STYLE = { opacity: 0.35 };
const EMPTY_SPARK: readonly number[] = [];
const PRESENCE_TOAST_MS = 3000;
/* Passed as the map's style so its viewBox is computed for this size. */
const MINIMAP_STYLE = { width: MINIMAP.width, height: MINIMAP.height };

function coordinatorEmptyCopy(
  t: (key: string, vars?: Record<string, string | number>) => string,
  deck: DeckState,
  hasWorkers: boolean,
): { title: string; hint: string; icon: IconName; failures: DeckState["preflightFailures"] } {
  if (hasWorkers) {
    return { title: t("collab.noWorkers"), hint: t("collab.noWorkersHint"), icon: "network", failures: [] };
  }
  if (deck.preflightFailures.length) {
    return { title: t("collab.empty.preflight"), hint: t("collab.empty.preflightHint"), icon: "xCircle", failures: deck.preflightFailures };
  }
  if (deck.preparing) {
    return { title: t("collab.empty.preparing"), hint: t("collab.empty.preparingHint"), icon: "terminal", failures: [] };
  }
  if (deck.finished) {
    return { title: t("collab.empty.noSpawned"), hint: t("collab.empty.noSpawnedHint"), icon: "network", failures: [] };
  }
  return { title: t("collab.empty.waitingWorkers"), hint: t("collab.empty.waitingWorkersHint"), icon: "clock", failures: [] };
}

function selectionFromSearch() {
  if (typeof window === "undefined") return INITIAL_SELECTION;
  const url = readCollabUrlState();
  return {
    agentId: url.agentId,
    knowledgeId: url.knowledgeId,
    relationId: url.relationId,
    tab: url.knowledgeId ? "knowledge" as const : "overview" as const,
  };
}

function initialRelationKinds(restored: CollabViewLoad): Set<CollaborationRelationKind> {
  const saved = restored.prefs.relationKinds;
  if (!saved || (saved.length === 7 && !saved.includes("model"))) return new Set(RELATION_KINDS);
  return new Set(RELATION_KINDS.filter((kind) => saved.includes(kind)));
}

function initialSelection(restored: CollabViewLoad, known: (id: string) => boolean) {
  if (restored.fromMemory) return collabSelectionFromSession(restored.session, known);
  const url = typeof window === "undefined" ? undefined : readCollabUrlState();
  if (url && (url.agentId || url.knowledgeId || url.relationId)) return selectionFromSearch();
  return collabSelectionFromSession(restored.session, known);
}

/** Keep the previous value while its signature is unchanged (read in render, recorded after commit). */
function useStableBySignature<T>(value: T, signature: string): T {
  const cache = useRef<{ signature: string; value: T } | null>(null);
  const stable = cache.current && cache.current.signature === signature ? cache.current.value : value;
  useEffect(() => {
    cache.current = { signature, value: stable };
  }, [signature, stable]);
  return stable;
}

type Signed<T> = { id: string; signature: string; item: T };

/**
 * Object identity by signature: an item whose signature matches the previous
 * commit keeps its object, and when every item is reused the array is reused
 * too — React Flow then sees the same `nodes` / `edges` prop and skips
 * setNodes / setEdges entirely, so nothing is re-measured.
 */
function useSignedList<T>(candidates: Signed<T>[]): T[] {
  const cache = useRef<{ entries: Map<string, { signature: string; item: T }>; list: T[] }>({ entries: new Map(), list: [] });
  const resolved = useMemo(() => {
    const { entries, list } = cache.current;
    let reused = 0;
    const items = candidates.map(({ id, signature, item }) => {
      const hit = entries.get(id);
      if (hit && hit.signature === signature) {
        reused += 1;
        return hit.item;
      }
      return item;
    });
    const sameList = reused === items.length && list.length === items.length && list.every((item, index) => item === items[index]);
    if (sameList) return cache.current;
    return {
      list: items,
      entries: new Map(candidates.map(({ id, signature }, index) => [id, { signature, item: items[index] }])),
    };
  }, [candidates]);
  useEffect(() => {
    cache.current = resolved;
  }, [resolved]);
  return resolved.list;
}

function AgentCollaborationInner({ deck, running, onKillWorker, onOpenTimeline, onOpenWorker, onSpawnWorker, onOpenFact, onOpenPoc, focusAgent, focusKnowledge }: CanvasProps) {
  const t = useT();
  const restored = useRef(loadCollabView(deck.runId)).current;
  const model = useMemo(() => buildAgentCollaborationModel(deck, "all"), [deck]);
  const canvasMode = canvasModeOf(deck);
  const { asOf, canReplay, marks, onScrub, onTogglePlay, playing, replaying, truncated, view } = useCollaborationReplay(deck, model);
  const [scope, setScope] = useState<CollaborationScope>(() => readCollabUrlState().scope);
  const [query, setQuery] = useState(restored.session.query);
  const deferredQuery = useDeferredValue(query);
  const [onlyAnomalies, setOnlyAnomalies] = useState(restored.prefs.onlyAnomalies);
  const [recentOnly, setRecentOnly] = useState(restored.prefs.recentOnly);
  const [relationKinds, setRelationKinds] = useState<Set<CollaborationRelationKind>>(() => initialRelationKinds(restored));
  const [knowledgeKinds, setKnowledgeKinds] = useState<Set<CollaborationKnowledgeKind>>(() => new Set());
  const [expandedIds, setExpandedIds] = useState<Set<string>>(() => new Set());
  const [autoCollapsedIds, setAutoCollapsedIds] = useState<Set<string>>(() => new Set());
  const [zoomExpanded, setZoomExpanded] = useState(false);
  const compactView = useStore((state) => state.transform[2] < ZOOM.compact);
  const zoomWantsExpansion = useStore((state) => state.transform[2] >= (zoomExpanded ? ZOOM.detailExit : ZOOM.detailEnter));
  useEffect(() => {
    if (zoomWantsExpansion === zoomExpanded) return;
    setZoomExpanded(zoomWantsExpansion);
    if (!zoomWantsExpansion) setAutoCollapsedIds(new Set());
  }, [zoomExpanded, zoomWantsExpansion]);
  const [flowView, setFlowView] = useState(false);
  const [followNew, setFollowNew] = useState(false);
  const [presenceToasts, setPresenceToasts] = useState<Array<{ id: string; agentId: string; text: string }>>([]);
  const [outsideNewIds, setOutsideNewIds] = useState<string[]>([]);
  const [layoutMode, setLayoutMode] = useState<CollabCanvasLayout>(() => readCollabLayout());
  const [contextMenu, setContextMenu] = useState<CollaborationContextMenuState | null>(null);
  const [edgeHover, setEdgeHover] = useState<{ x: number; y: number; relationId: string } | null>(null);
  const [selection, dispatch] = useCollaborationSelection(() => initialSelection(restored, (id) => model.agentById.has(id)));
  useEffect(() => {
    scheduleCollabUrlWrite({
      view: "collaboration",
      agentId: selection.agentId,
      knowledgeId: selection.knowledgeId,
      relationId: selection.relationId,
      scope,
    });
    return () => cancelCollabUrlWrite();
  }, [scope, selection.agentId, selection.knowledgeId, selection.relationId]);
  useEffect(() => {
    const onPop = () => {
      const url = readCollabUrlState();
      setScope(url.scope);
      dispatch({
        type: "hydrate",
        state: {
          agentId: url.agentId,
          knowledgeId: url.knowledgeId,
          relationId: url.relationId,
          tab: url.knowledgeId ? "knowledge" : "overview",
        },
      });
    };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, [dispatch]);
  useEffect(() => {
    writeCollabSession(deck.runId, {
      selectedAgentId: selection.agentId ?? "",
      selectedKnowledgeId: selection.knowledgeId,
      selectedRelationId: selection.relationId,
      inspectorTab: selection.tab,
      query,
    });
  }, [deck.runId, query, selection]);
  const [flowReady, setFlowReady] = useState(false);
  const [layoutVersion, setLayoutVersion] = useState(restored.memory.layoutVersion);
  const canvasRef = useRef<HTMLElement>(null);
  const shellRef = useRef<HTMLDivElement>(null);
  const inspectorRef = useRef<HTMLElement>(null);
  const lastFocusAgentNonce = useRef<number | null>(null);
  const lastFocusKnowledgeNonce = useRef<number | null>(null);
  const pendingAgentFocus = useRef<string | null>(null);
  const pendingKnowledgeFocus = useRef<string | null>(null);
  const indexWasOpen = useRef<boolean | null>(null);
  const detailWasOpen = useRef<boolean | null>(null);
  const reduceMotion = usePrefersReducedMotion();
  const { setCenter, getViewport, fitView } = useReactFlow();
  const storeApi = useStoreApi();
  const followNewRef = useRef(followNew);
  followNewRef.current = followNew;
  // 30-second clock for edge recency and the time-based filters; the model and
  // the nodes never depend on it. Unsubscribed once the run is over unless the
  // "recently active" filter still needs a live reading.
  const now = useTicker(COARSE_TICKER, running || recentOnly);

  // `now` enters the visible set only while a time-based filter is on, so the
  // tick leaves the nodes and every downstream memo alone otherwise.
  const filterNow = onlyAnomalies || recentOnly ? now : 0;
  const visibleAgents = useMemo(() => model.allAgents.filter((agent) => {
    if (agent.id === COORDINATOR_ID) return true;
    if (scope === "current" && !agent.isCurrent) return false;
    if (onlyAnomalies || recentOnly) {
      const recency = recencyOf(agent, filterNow);
      if (onlyAnomalies && !agent.isAnomaly && !recency.idle) return false;
      if (recentOnly && !(recency.sinceMs !== undefined && recency.sinceMs < ACTIVITY_FILTER_WINDOW_MS)) return false;
    }
    return true;
  }), [filterNow, model.allAgents, onlyAnomalies, recentOnly, scope]);
  const visibleIds = useMemo(() => new Set(visibleAgents.map((agent) => agent.id)), [visibleAgents]);
  const presentIds = useMemo(() => {
    if (!replaying) return visibleIds;
    const ids = new Set<string>();
    for (const agent of visibleAgents) {
      if (agentPresentAt(agent, asOf)) ids.add(agent.id);
    }
    return ids;
  }, [asOf, replaying, visibleAgents, visibleIds]);
  const anomalyCount = useMemo(
    () => model.allAgents.filter((agent) => agent.isAnomaly).length,
    [model.allAgents],
  );
  const {
    overlay, mobile, shellWidth,
    indexOpen, setIndexOpen, detailOpen, setDetailOpen, mobileView, setMobileView, openDetail,
    toggleIndex, toggleDetail, closeOverlays,
    miniMapOpen, toggleMiniMap, legendOpen, toggleLegend,
  } = useCollaborationLayout(visibleAgents.length, shellRef, {
    indexOpen: restored.prefs.indexOpen,
    detailOpen: restored.prefs.detailOpen,
  });
  useEffect(() => {
    writeCollabPrefs({
      relationKinds: [...relationKinds],
      onlyAnomalies,
      recentOnly,
      indexOpen,
      detailOpen,
    });
  }, [detailOpen, indexOpen, onlyAnomalies, recentOnly, relationKinds]);
  useEffect(() => {
    writeCollabLayout(layoutMode);
  }, [layoutMode]);

  // All executions share the same column layout; generation is metadata,
  // not a separate vertically stacked canvas band.
  // Cards keep their slot across updates; bumping layoutVersion (toolbar "fit",
  // run end) lays out from scratch. The cache is written after commit only.
  const layoutCache = useRef<{ version: number; positions: Record<string, CollaborationPosition> }>({
    version: restored.memory.layoutVersion,
    positions: restored.memory.positions,
  });
  // The column layout reads roster-level fields only (id / role / generation /
  // start time). model.workers keeps its identity while the roster and lane
  // lifecycle hold, so the list handed to the layout is rebuilt from it and
  // pinned by signature. Its memo depends on that pinned list, the layout
  // version and the relations only, so a streaming event (new model, same
  // roster) never re-lays the column view out.
  const layoutRoster = model.allAgents;
  const layoutInput = useStableBySignature(
    layoutRoster,
    useMemo(() => layoutRoster.map((agent) => [agent.id, agent.role, agent.generation, agent.startedAt ?? 0].join(FIELD_SEPARATOR)).join("\n"), [layoutRoster]),
  );
  const columnPositions = useMemo(
    () => layoutAgents(layoutInput, model.relations, layoutCache.current.version === layoutVersion ? layoutCache.current.positions : {}),
    [layoutInput, layoutVersion, model.relations],
  );
  const timelineClock = toEpochMs(deck.finishedAt) || now;
  const timelineLayout = useMemo(
    () => (layoutMode === "timeline"
      ? layoutTimeline(visibleAgents, deck.startedAt, deck.finishedAt, timelineClock)
      : undefined),
    [deck.finishedAt, deck.startedAt, layoutMode, timelineClock, visibleAgents],
  );
  const basePositions = timelineLayout
    ? timelineLayout.positions
    : columnPositions;
  useEffect(() => {
    if (layoutMode === "hierarchy") {
      layoutCache.current = { version: layoutVersion, positions: basePositions };
      patchCollabMemory(deck.runId, { layoutVersion, positions: basePositions });
    }
  }, [basePositions, deck.runId, layoutMode, layoutVersion]);
  const targetPositions = useMemo(
    () => layoutAgents(layoutInput, model.relations),
    [layoutInput, model.relations],
  );
  const layoutStale = useMemo(
    () => layoutMode === "hierarchy" && layoutDiffers(basePositions, targetPositions),
    [basePositions, layoutMode, targetPositions],
  );
  // Expand only slides cards below in the same column. The slot cache and
  // fitView stay put so opening work items does not reframe the viewport.
  const workItemsByAgent = useMemo(() => {
    const items = new Map<string, ReturnType<typeof agentWorkItemsOf>>();
    for (const base of visibleAgents) {
      const agent = view.agentById.get(base.id) ?? base;
      if (agent.id !== COORDINATOR_ID) items.set(agent.id, agentWorkItemsOf(agent.knowledge));
    }
    return items;
  }, [view.agentById, visibleAgents]);
  const expansionMode = useCallback((id: string): "none" | "preview" | "full" => {
    if (compactView) return "none";
    if (expandedIds.has(id)) return "full";
    if (zoomExpanded && !autoCollapsedIds.has(id)) return "preview";
    return "none";
  }, [autoCollapsedIds, compactView, expandedIds, zoomExpanded]);
  const expandExtras = useMemo(() => {
    const extras: Record<string, number> = {};
    for (const [id, items] of workItemsByAgent) {
      const mode = expansionMode(id);
      const visible = mode === "full"
        ? items.length
        : mode === "preview"
          ? Math.min(items.length, LAYOUT.autoWorkItemRows) + (items.length > LAYOUT.autoWorkItemRows ? 1 : 0)
          : 0;
      if (visible) extras[id] = agentCardHeight(visible) - LAYOUT.nodeHeight;
    }
    return extras;
  }, [expansionMode, workItemsByAgent]);
  const positions = useMemo(
    () => layoutMode === "timeline"
      ? applyExpandedLaneShift(basePositions, expandExtras)
      : applyExpandedShift(basePositions, expandExtras),
    [basePositions, expandExtras, layoutMode],
  );
  const visibleRelations = useMemo(
    () => view.relations.filter((relation) => relationKinds.has(relation.kind) && presentIds.has(relation.source) && presentIds.has(relation.target)),
    [presentIds, relationKinds, view.relations],
  );
  // Chain highlight: every agent reachable from the selected one over the
  // visible relations, in either direction. The coordinator is reached but
  // never expanded — its dispatch edges would otherwise join every card into
  // one chain. Null while nothing, the coordinator, or a filtered-off agent is
  // selected: no dimming.
  const chain = useMemo(() => {
    const start = selection.agentId;
    if (!start || start === COORDINATOR_ID || !visibleIds.has(start)) return null;
    const reach = new Set<string>([start]);
    const pending = [start];
    for (let i = 0; i < pending.length; i += 1) {
      const id = pending[i];
      if (id !== start && !isCollaborationWorker(model.agentById.get(id) ?? { role: "source" })) continue;
      for (const relation of visibleRelations) {
        const next = relation.source === id ? relation.target : relation.target === id ? relation.source : undefined;
        if (next && !reach.has(next)) {
          reach.add(next);
          pending.push(next);
        }
      }
    }
    return reach;
  }, [model.agentById, selection.agentId, visibleIds, visibleRelations]);
  const selectedRelation = selection.relationId
    ? view.relations.find((relation) => relation.id === selection.relationId)
    : undefined;

  // One identity index for every worker; display names and actor labels both
  // read from it. The Map only changes when an identity field changes, so the
  // display / actor-name callbacks — and with them the edges — stay stable
  // across events that leave the roster alone.
  const identitySignature = useMemo(() => model.allAgents.map((agent) => [
    agent.id,
    agent.identity.engine,
    agent.identity.profileId,
    agent.identity.profileLabel,
    agent.identity.model,
    agent.identity.accountId,
    agent.identity.endpointHost,
    agent.identity.connection,
    agent.identity.provider,
  ].join(FIELD_SEPARATOR)).join("\n"), [model.allAgents]);
  const identities = useStableBySignature(useMemo(() => {
    const result = new Map<string, WorkerIdentity>();
    for (const agent of model.allAgents) {
      if (isCollaborationWorker(agent)) result.set(agent.id, toWorkerIdentity(agent.id, agent.identity));
    }
    return result;
  }, [model.allAgents]), identitySignature);
  const identityList = useMemo(() => Array.from(identities.values()), [identities]);
  const displays = useMemo(() => {
    const result = new Map<string, AgentDisplay>();
    result.set(COORDINATOR_ID, {
      title: t("coord.title"),
      initial: "CO",
      engineKey: "",
      subtitle: t("collab.coordinator.description"),
      engine: t("collab.coordinator.program"),
      color: "var(--blue)",
    });
    result.set(DECISION_ID, { title: canvasMode === "ctf" ? "Decide" : "Reason", initial: "AI", engineKey: "", subtitle: t("collab.role.decision"), engine: t("collab.role.decision"), color: "var(--violet)" });
    result.set(INPUT_SOURCE_ID, { title: t("collab.source.title"), initial: "IN", engineKey: "", subtitle: t("collab.role.source"), engine: t("collab.role.source"), color: "var(--muted)" });
    for (const [id, identity] of identities) {
      const display = workerDisplayName(id, identity, identityList);
      result.set(id, {
        title: display.title,
        initial: display.initial,
        engineKey: workerEngineKey(id, identity.engine),
        subtitle: formatWorkerSubtitle(display, t),
        engine: workerEngine(id, identity.engine),
        color: workerColorVar(id, identity.engine),
      });
    }
    return result;
  }, [canvasMode, identities, identityList, t]);
  const displayFor = useCallback((agent: CollaborationAgent): AgentDisplay => {
    const known = displays.get(agent.id);
    if (known) return known;
    const display = workerDisplayName(agent.id, null);
    return {
      title: display.title,
      initial: display.initial,
      engineKey: workerEngineKey(agent.id, agent.identity.engine),
      subtitle: "",
      engine: workerEngine(agent.id, agent.identity.engine),
      color: workerColorVar(agent.id, agent.identity.engine),
    };
  }, [displays]);
  const actorName = useCallback((id?: string) => {
    if (!id) return "";
    // Ids outside the model (human / system / report-value …) use the shared actor labels.
    return displays.get(id)?.title || actorDisplayTitle(id, t, identities.get(id) ?? null, identityList);
  }, [displays, identities, identityList, t]);
  const miniMapColor = useCallback((node: Node) => {
    if (node.type === "group" || node.type === "ruler") return "transparent";
    return displays.get(node.id)?.color || "var(--eng-default)";
  }, [displays]);

  // Read the store's absolute position: a grouped worker's `position` is
  // relative to its generation band, and the coordinator card is taller.
  // Keep the current scale; only lift when it is below ZOOM.centerMin so a
  // 15-card overview at 0.4 is not blown up to fitMax.
  const centerAgent = useCallback((id: string) => {
    requestAnimationFrame(() => {
      const { nodeLookup, transform } = storeApi.getState();
      const internal = nodeLookup.get(id);
      if (!internal) return;
      const { x, y } = internal.internals.positionAbsolute;
      const w = internal.measured.width ?? LAYOUT.nodeWidth;
      const h = internal.measured.height ?? (id === COORDINATOR_ID ? LAYOUT.coordinatorHeight : LAYOUT.nodeHeight);
      const currentZoom = transform[2];
      void setCenter(x + w / 2, y + h / 2, {
        zoom: currentZoom < ZOOM.centerMin ? ZOOM.center : currentZoom,
        duration: reduceMotion ? 0 : MOTION.center,
      });
    });
  }, [reduceMotion, setCenter, storeApi]);

  const centerKeepZoom = useCallback((id: string) => {
    requestAnimationFrame(() => {
      const { nodeLookup } = storeApi.getState();
      const internal = nodeLookup.get(id);
      if (!internal) return;
      const { x, y } = internal.internals.positionAbsolute;
      const w = internal.measured.width ?? LAYOUT.nodeWidth;
      const h = internal.measured.height ?? (id === COORDINATOR_ID ? LAYOUT.coordinatorHeight : LAYOUT.nodeHeight);
      void setCenter(x + w / 2, y + h / 2, {
        zoom: getViewport().zoom,
        duration: reduceMotion ? 0 : MOTION.center,
      });
    });
  }, [getViewport, reduceMotion, setCenter, storeApi]);

  const nodeOutsideView = useCallback((id: string) => {
    const { nodeLookup, transform } = storeApi.getState();
    const internal = nodeLookup.get(id);
    const canvas = canvasRef.current;
    if (!internal || !canvas) return false;
    const { x, y } = internal.internals.positionAbsolute;
    const w = internal.measured.width ?? LAYOUT.nodeWidth;
    const h = internal.measured.height ?? LAYOUT.nodeHeight;
    const [tx, ty, zoom] = transform;
    const left = x * zoom + tx;
    const top = y * zoom + ty;
    return left + w * zoom < 0 || top + h * zoom < 0 || left > canvas.clientWidth || top > canvas.clientHeight;
  }, [storeApi]);

  const prevPresence = useRef<Map<string, { firstSeenAt?: number; finishedAt?: number; name: string }> | null>(null);
  useEffect(() => {
    const next = new Map<string, { firstSeenAt?: number; finishedAt?: number; name: string }>();
    for (const agent of visibleAgents) {
      if (!isCollaborationWorker(agent)) continue;
      next.set(agent.id, {
        firstSeenAt: agent.firstSeenAt,
        finishedAt: agent.finishedAt,
        name: displayFor(agent).title,
      });
    }
    const prev = prevPresence.current;
    prevPresence.current = next;
    if (!prev) return;
    const joined: string[] = [];
    const toasts: Array<{ id: string; agentId: string; text: string }> = [];
    const nowMs = Date.now();
    for (const [id, cur] of next) {
      const was = prev.get(id);
      if (!was) {
        const seen = toEpochMs(cur.firstSeenAt);
        if (seen && nowMs - seen < AGENT_ENTER_MS) {
          joined.push(id);
          toasts.push({ id: `join:${id}:${seen}`, agentId: id, text: t("collab.agentJoined", { name: cur.name }) });
        }
      } else if (!was.finishedAt && cur.finishedAt) {
        toasts.push({ id: `exit:${id}:${cur.finishedAt}`, agentId: id, text: t("collab.agentExited", { name: cur.name }) });
      }
    }
    if (toasts.length) {
      setPresenceToasts((current) => [...current, ...toasts]);
      for (const toast of toasts) {
        window.setTimeout(() => {
          setPresenceToasts((current) => current.filter((item) => item.id !== toast.id));
        }, PRESENCE_TOAST_MS);
      }
    }
    if (!joined.length) return;
    const newest = joined.reduce((best, id) => (
      toEpochMs(next.get(id)?.firstSeenAt) >= toEpochMs(next.get(best)?.firstSeenAt) ? id : best
    ));
    if (followNewRef.current) {
      centerKeepZoom(newest);
      return;
    }
    requestAnimationFrame(() => {
      const outside = joined.filter((id) => nodeOutsideView(id));
      if (outside.length) setOutsideNewIds((current) => [...new Set([...current, ...outside])]);
    });
  }, [centerKeepZoom, displayFor, nodeOutsideView, t, visibleAgents]);

  const revealOutsideAgents = useCallback(() => {
    const ids = outsideNewIds;
    setOutsideNewIds([]);
    if (ids.length === 1) {
      centerKeepZoom(ids[0]);
      return;
    }
    if (!ids.length) return;
    void fitView({
      nodes: ids.map((id) => ({ id })),
      padding: 0.2,
      minZoom: FIT_MIN_ZOOM,
      maxZoom: ZOOM.fitMax,
      duration: reduceMotion ? 0 : MOTION.fit,
    });
  }, [centerKeepZoom, fitView, outsideNewIds, reduceMotion]);

  const focusInspectorHead = useCallback(() => {
    requestAnimationFrame(() => {
      inspectorRef.current
        ?.querySelector<HTMLElement>(".collab-inspector-head, .collab-relation-head")
        ?.focus({ preventScroll: true });
    });
  }, []);

  const selectAgent = useCallback((id: string, center = false) => {
    dispatch({ type: "selectAgent", agentId: id });
    openDetail();
    if (center) centerAgent(id);
  }, [centerAgent, dispatch, openDetail]);
  // Arrow / j/k move selection on the canvas without opening the overlay inspector.
  const moveAgent = useCallback((id: string, center = false) => {
    dispatch({ type: "selectAgent", agentId: id });
    if (center) centerAgent(id);
  }, [centerAgent, dispatch]);

  const selectKnowledge = useCallback((item: CollaborationKnowledgeItem, center = false) => {
    const agentId = item.agentId && model.agentById.has(item.agentId) ? item.agentId : COORDINATOR_ID;
    dispatch({ type: "selectKnowledge", agentId, knowledgeId: item.id });
    openDetail("detail");
    if (center) centerAgent(agentId);
    focusInspectorHead();
  }, [centerAgent, dispatch, focusInspectorHead, model.agentById, openDetail]);

  const selectKnowledgeById = useCallback((itemId: string, center = false) => {
    const item = view.knowledgeById.get(itemId) ?? model.knowledgeById.get(itemId);
    if (item) selectKnowledge(item, center);
  }, [model.knowledgeById, selectKnowledge, view.knowledgeById]);

  const selectKnowledgeCenteredById = useCallback((itemId: string) => {
    selectKnowledgeById(itemId, true);
  }, [selectKnowledgeById]);

  const selectRelationById = useCallback((relationId: string) => {
    dispatch({ type: "selectRelation", relationId });
    openDetail("detail");
    focusInspectorHead();
  }, [dispatch, focusInspectorHead, openDetail]);

  const toggleExpand = useCallback((agentId: string) => {
    const mode = expansionMode(agentId);
    if (mode === "full") {
      setExpandedIds((current) => { const next = new Set(current); next.delete(agentId); return next; });
      if (zoomExpanded) setAutoCollapsedIds((current) => new Set(current).add(agentId));
      return;
    }
    if (mode === "preview") {
      setAutoCollapsedIds((current) => new Set(current).add(agentId));
      return;
    }
    setExpandedIds((current) => new Set(current).add(agentId));
    setAutoCollapsedIds((current) => { const next = new Set(current); next.delete(agentId); return next; });
  }, [expansionMode, zoomExpanded]);

  const showAllWorkItems = useCallback((agentId: string) => {
    setExpandedIds((current) => new Set(current).add(agentId));
    setAutoCollapsedIds((current) => { const next = new Set(current); next.delete(agentId); return next; });
  }, []);

  const selectPreviewKnowledge = useCallback((knowledgeId: string) => {
    selectKnowledgeById(knowledgeId);
  }, [selectKnowledgeById]);

  const collabActions = useMemo(
    () => ({ toggleExpand, showAllWorkItems, selectPreviewKnowledge }),
    [selectPreviewKnowledge, showAllWorkItems, toggleExpand],
  );

  const selectIntent = useCallback((intent: BlackboardIntent) => {
    dispatch({ type: "selectKnowledge", agentId: COORDINATOR_ID, knowledgeId: `intent:${intent.id}` });
    openDetail("detail");
    focusInspectorHead();
  }, [dispatch, focusInspectorHead, openDetail]);

  const revealSelected = useCallback(() => {
    const id = selection.agentId;
    setScope("all");
    setOnlyAnomalies(false);
    setRecentOnly(false);
    if (id) requestAnimationFrame(() => centerAgent(id));
  }, [centerAgent, selection.agentId]);

  useEffect(() => {
    if (!focusAgent || focusAgent.nonce === lastFocusAgentNonce.current) return;
    lastFocusAgentNonce.current = focusAgent.nonce;
    pendingAgentFocus.current = focusAgent.id;
    if (!visibleIds.has(focusAgent.id)) {
      setScope("all");
      setOnlyAnomalies(false);
      setRecentOnly(false);
    }
  }, [focusAgent, visibleIds]);

  useEffect(() => {
    if (!focusKnowledge || focusKnowledge.nonce === lastFocusKnowledgeNonce.current) return;
    lastFocusKnowledgeNonce.current = focusKnowledge.nonce;
    pendingKnowledgeFocus.current = focusKnowledge.id;
    const item = model.knowledgeById.get(focusKnowledge.id);
    const owner = item?.agentId && model.agentById.has(item.agentId) ? item.agentId : COORDINATOR_ID;
    if (!visibleIds.has(owner)) {
      setScope("all");
      setOnlyAnomalies(false);
      setRecentOnly(false);
    }
  }, [focusKnowledge, model.agentById, model.knowledgeById, visibleIds]);

  useEffect(() => {
    const agentId = pendingAgentFocus.current;
    if (agentId && visibleIds.has(agentId)) {
      pendingAgentFocus.current = null;
      selectAgent(agentId, true);
      setDetailOpen(true);
      focusInspectorHead();
    }
    const knowledgeId = pendingKnowledgeFocus.current;
    if (!knowledgeId) return;
    const item = model.knowledgeById.get(knowledgeId);
    if (!item) return;
    const owner = item.agentId && visibleIds.has(item.agentId) ? item.agentId : COORDINATOR_ID;
    if (!visibleIds.has(owner)) return;
    pendingKnowledgeFocus.current = null;
    selectKnowledge(item, true);
    setDetailOpen(true);
    focusInspectorHead();
  }, [focusInspectorHead, model.knowledgeById, selectAgent, selectKnowledge, setDetailOpen, visibleIds]);

  useEffect(() => {
    if (mobileView !== "detail") return;
    focusInspectorHead();
  }, [focusInspectorHead, mobileView]);

  useEffect(() => {
    const closed = indexWasOpen.current === true && !indexOpen;
    indexWasOpen.current = indexOpen;
    if (!closed) return;
    const active = document.activeElement;
    const index = document.querySelector(".collab-shell .collab-index");
    if (active === document.body || (index && index.contains(active))) {
      canvasRef.current?.focus({ preventScroll: true });
    }
  }, [indexOpen]);

  useEffect(() => {
    const closed = detailWasOpen.current === true && !detailOpen;
    detailWasOpen.current = detailOpen;
    if (!closed) return;
    const active = document.activeElement;
    const inspector = inspectorRef.current;
    if (active === document.body || (inspector && inspector.contains(active))) {
      canvasRef.current?.focus({ preventScroll: true });
    }
  }, [detailOpen]);

  // Mouse clicks and keyboard Enter both arrive as a React Flow "select"
  // change. Selection is owned by the reducer, so the store-side flag is
  // cleared right away — otherwise the next Enter on the same card yields
  // no change at all.
  const onNodesChange = useCallback((changes: NodeChange<AgentFlowNode | CoordinatorFlowNode | GroupFlowNode | RulerFlowNode>[]) => {
    for (const change of changes) {
      if (change.type !== "select" || !change.selected) continue;
      if (selection.agentId === change.id && !selection.relationId) dispatch({ type: "clear" });
      else selectAgent(change.id);
      const { nodeLookup, unselectNodesAndEdges } = storeApi.getState();
      const internal = nodeLookup.get(change.id);
      if (internal) unselectNodesAndEdges({ nodes: [internal], edges: [] });
    }
  }, [dispatch, selectAgent, selection.agentId, selection.relationId, storeApi]);
  const onEdgesChange = useCallback((changes: EdgeChange[]) => {
    for (const change of changes) {
      if (change.type !== "select" || !change.selected) continue;
      const relation = model.relations.find((item) => item.id === change.id);
      if (!relation) continue;
      // The "selected" outline is driven by selection.relationId (edge className),
      // so clear the store flag right away — otherwise pressing Enter on the same
      // edge again yields no select change and the inspector never re-opens.
      selectRelationById(relation.id);
      const { edgeLookup, unselectNodesAndEdges } = storeApi.getState();
      const internal = edgeLookup.get(change.id);
      if (internal) unselectNodesAndEdges({ nodes: [], edges: [internal] });
    }
  }, [model.relations, selectRelationById, storeApi]);
  const onPaneClick = useCallback(() => {
    setContextMenu(null);
    setEdgeHover(null);
    dispatch({ type: "clear" });
    if (overlay) closeOverlays();
  }, [closeOverlays, dispatch, overlay]);
  const onEdgeMouseEnter = useCallback((event: MouseEvent, edge: Edge) => {
    const rect = canvasRef.current?.getBoundingClientRect();
    if (!rect) return;
    setEdgeHover({ x: event.clientX - rect.left, y: event.clientY - rect.top, relationId: edge.id });
  }, []);
  const onEdgeMouseLeave = useCallback(() => setEdgeHover(null), []);
  const onNodeDoubleClick = useCallback((_: MouseEvent, node: Node) => {
    if (!isCollaborationWorker(model.agentById.get(node.id) ?? { role: "source" })) return;
    onOpenWorker?.(node.id);
  }, [model.agentById, onOpenWorker]);
  const onNodeContextMenu = useCallback((event: MouseEvent, node: Node) => {
    event.preventDefault();
    if (node.type === "group" || node.type === "ruler") {
      setContextMenu({ x: event.clientX, y: event.clientY });
      return;
    }
    setContextMenu({ x: event.clientX, y: event.clientY, nodeId: node.id });
  }, []);
  const onPaneContextMenu = useCallback((event: MouseEvent | globalThis.MouseEvent) => {
    event.preventDefault();
    setContextMenu({ x: event.clientX, y: event.clientY });
  }, []);
  const onInit = useCallback(() => setFlowReady(true), []);

  const q = deferredQuery.trim().toLowerCase();
  const matchedAgentIds = useMemo(() => {
    if (!q) return null;
    const ids = new Set<string>();
    for (const base of visibleAgents) {
      const agent = view.agentById.get(base.id) ?? base;
      const display = displayFor(agent);
      if (
        agent.searchText.includes(q)
        || display.title.toLowerCase().includes(q)
        || display.subtitle.toLowerCase().includes(q)
      ) ids.add(agent.id);
    }
    return ids;
  }, [displayFor, q, view.agentById, visibleAgents]);
  const matchedAgents = useMemo(
    () => (matchedAgentIds ? visibleAgents.filter((agent) => matchedAgentIds.has(agent.id)) : visibleAgents),
    [matchedAgentIds, visibleAgents],
  );
  const cycleMatch = useCallback((delta: number) => {
    if (!q || matchedAgents.length === 0) return;
    const index = matchedAgents.findIndex((agent) => agent.id === selection.agentId);
    const pick = index < 0
      ? (delta > 0 ? matchedAgents[0] : matchedAgents[matchedAgents.length - 1])
      : matchedAgents[(index + delta + matchedAgents.length) % matchedAgents.length];
    selectAgent(pick.id, true);
  }, [matchedAgents, q, selectAgent, selection.agentId]);

  const openIntentCount = view.openIntents.length;
  const sparklines = useMemo(
    () => sparklineSeriesByAgent(
      visibleAgents,
      deck.runtimeEvents,
      deck.chat,
      deck.costHistory,
      deck.finishedAt,
      running ? now : toEpochMs(deck.finishedAt) || now,
    ),
    [deck.chat, deck.costHistory, deck.finishedAt, deck.runtimeEvents, now, running, visibleAgents],
  );
  const nodeCandidates = useMemo<Signed<AgentFlowNode | CoordinatorFlowNode>[]>(() => visibleAgents.map((base) => {
    const agent = view.agentById.get(base.id) ?? base;
    const present = !replaying || agentPresentAt(base, asOf);
    const display = displayFor(agent);
    const startedAt = toEpochMs(agent.startedAt);
    const endedAt = toEpochMs(agent.endedAt);
    const relationEndpoint = selectedRelation && (agent.id === selectedRelation.source || agent.id === selectedRelation.target)
      ? selectedRelation.kind
      : "";
    const coordinatorFocus = selection.agentId === COORDINATOR_ID && !selectedRelation;
    const related = selectedRelation
      ? relationEndpoint !== ""
      : coordinatorFocus
        ? agent.id === COORDINATOR_ID || agent.isCurrent
        : !chain || chain.has(agent.id);
    const role = roleLabel(agent, t);
    const status = replaying && isCollaborationWorker(agent)
      ? (agent.online ? t("collab.online") : t("collab.offline"))
      : statusLabel(agent, t, canvasMode);
    const absolute = positions[agent.id] || { x: LAYOUT.origin, y: LAYOUT.origin };
    // A card the search filtered out stays on the canvas dimmed but leaves the Tab order.
    const matched = (!matchedAgentIds || matchedAgentIds.has(agent.id)) && present;

    if (agent.id === COORDINATOR_ID) {
      const coordStatus = replaying
        ? (agent.online ? t("collab.status.coordinating") : t("collab.complete"))
        : coordinatorStatus(agent, t, canvasMode);
      // Dispatch metrics come straight from the model's coordinator record.
      const coordination = agent.coordination;
      const data: CoordinatorNodeData = {
        id: agent.id,
        title: display.title,
        initial: display.initial,
        color: display.color,
        statusKind: agent.statusKind,
        statusLabel: coordStatus,
        phaseLabel: agent.phase,
        generation: agent.generation,
        onlineWorkers: coordination?.onlineWorkers ?? 0,
        totalWorkers: coordination?.totalWorkers ?? 0,
        openIntents: openIntentCount,
        verifiedFacts: coordination?.verifiedFacts ?? 0,
        reasonRounds: coordination?.reasonRounds ?? 0,
        latestActivity: agent.latestActivity,
        startedAt,
        endedAt,
        live: running && !endedAt,
        selected: selection.agentId === agent.id && !selection.relationId,
        dimmed: !matched || (!!selectedRelation && !related),
        relationEndpoint,
        durationBar: layoutMode === "timeline",
        degradedCount: Object.keys(deck.degradedEngines).length,
        preflightCount: deck.preflightFailures.length,
        firstSeenAt: toEpochMs(agent.firstSeenAt),
        finishedAt: toEpochMs(agent.finishedAt),
        canvasMode,
      };
      const node: CoordinatorFlowNode = {
        id: agent.id,
        type: "coordinator",
        position: absolute,
        width: timelineLayout?.widths[agent.id] ?? LAYOUT.nodeWidth,
        height: LAYOUT.coordinatorHeight,
        data,
        draggable: false,
        selectable: true,
        focusable: matched,
        deletable: false,
        ariaRole: "button",
        ariaLabel: `${display.title} · ${coordStatus} · ${data.onlineWorkers}/${data.totalWorkers}`,
        hidden: !present,
      };
      return { id: agent.id, signature: [coordinatorSignature(data), absolute.x, absolute.y, node.width, matched, present].join(FIELD_SEPARATOR), item: node };
    }

    const workItems = isCollaborationWorker(agent) ? workItemsByAgent.get(agent.id) ?? [] : [];
    const mode = expansionMode(agent.id);
    const expanded = mode !== "none" && workItems.length > 0;
    const manualExpanded = mode === "full";
    const visibleWorkItemCount = manualExpanded ? workItems.length : Math.min(workItems.length, LAYOUT.autoWorkItemRows);
    const renderedWorkItemRows = expanded ? visibleWorkItemCount + (workItems.length > visibleWorkItemCount ? 1 : 0) : 0;
    const spark = sparklines.get(agent.id);
    const data: AgentNodeData = {
      id: agent.id,
      title: display.title,
      initial: display.initial,
      engineKey: display.engineKey,
      subtitle: display.subtitle,
      engine: display.engine,
      color: display.color,
      role: agent.role,
      roleLabel: role,
      statusKind: agent.statusKind,
      statusLabel: status,
      intentText: agent.role === "decision"
        ? (agent.identity.model || t("collab.decision.unknown"))
        : agent.role === "source" ? t("collab.source.description")
        : (agent.currentIntent?.summary || agent.currentIntent?.goal || agent.lastIntent?.summary || agent.lastIntent?.goal || t("collab.workspace.noTask")),
      intentTitle: (agent.currentIntent ?? agent.lastIntent)?.goal || agent.latestActivity,
      latestActivity: agent.role === "decision" ? `${deck.blackboard.reasonRuns.length} ${t("collab.workspace.rounds")}` : agent.role === "source" ? t("collab.workspace.inputHint") : agent.latestActivity,
      facts: agent.facts,
      observations: agent.observations,
      candidates: agent.candidates,
      deadEnds: agent.deadEnds,
      locks: agent.locks,
      tokens: agent.tokens,
      usd: agent.usd,
      unpricedCalls: agent.unpricedCalls,
      model: agent.identity.model || "",
      profileLabel: agent.identity.profileLabel || "",
      startedAt,
      endedAt,
      live: running && !endedAt,
      online: agent.online,
      isCurrent: agent.isCurrent,
      lastEventAt: toEpochMs(agent.lastEventAt),
      lastProgressAt: toEpochMs(agent.lastProgressAt),
      lifecycleSource: agent.lifecycleSource,
      selected: selection.agentId === agent.id && !selection.relationId,
      dimmed: !matched || !related,
      relationEndpoint,
      generation: agent.generation,
      expanded,
      manualExpanded,
      selectedKnowledgeId: selection.knowledgeId ?? "",
      workItems,
      visibleWorkItemCount,
      sparklineEvents: spark?.events ?? EMPTY_SPARK,
      sparklineTokens: spark?.tokens ?? EMPTY_SPARK,
      sparklinePartial: spark?.partial ?? false,
      durationBar: layoutMode === "timeline",
      isAnomaly: agent.isAnomaly,
      anomalyReason: agent.anomalyReasonKey ? t(agent.anomalyReasonKey) : "",
      pendingHitl: agent.pendingHitl,
      firstSeenAt: toEpochMs(agent.firstSeenAt),
      finishedAt: toEpochMs(agent.finishedAt),
      canvasMode,
    };
    // Grouped worker: position is relative to its generation band's group node.
    const position = absolute;
    const node: AgentFlowNode = {
      id: agent.id,
      type: "agent",
      position,
      width: timelineLayout?.widths[agent.id] ?? LAYOUT.nodeWidth,
      height: agentCardHeight(renderedWorkItemRows),
      data,
      draggable: false,
      selectable: present,
      focusable: matched,
      deletable: false,
      ariaRole: "button",
      ariaLabel: !isCollaborationWorker(agent) ? [display.title, role, status, data.intentText].join(" · ") : [
        display.title,
        role,
        status,
        agent.currentIntent ? data.intentText : "",
        `${knowledgeLabel("fact", t, canvasMode)} ${agent.facts}`,
        canvasMode === "ctf"
          ? `${knowledgeLabel("step", t, canvasMode)} ${agent.intents.length}`
          : `${knowledgeLabel("candidate", t, canvasMode)} ${agent.candidates}`,
        canvasMode === "ctf" ? "" : `${t("collab.exclusions")} ${agent.deadEnds}`,
      ].filter(Boolean).join(" · "),
      hidden: !present,
    };
    return { id: agent.id, signature: [nodeSignature(data), position.x, position.y, node.width, matched, present].join(FIELD_SEPARATOR), item: node };
  }), [asOf, canvasMode, chain, displayFor, deck.blackboard.reasonRuns.length, deck.degradedEngines, deck.preflightFailures, expansionMode, layoutMode, matchedAgentIds, openIntentCount, positions, replaying, running, selectedRelation, selection.agentId, selection.knowledgeId, selection.relationId, sparklines, t, timelineLayout, view.agentById, visibleAgents, workItemsByAgent]);

  const rulerCandidates = useMemo<Signed<RulerFlowNode>[]>(() => {
    if (!timelineLayout) return [];
    const node: RulerFlowNode = {
      id: TIMELINE_RULER_ID,
      type: "ruler",
      position: { x: LAYOUT.origin, y: LAYOUT.origin },
      width: timelineLayout.usableWidth,
      height: LAYOUT.timelineRulerHeight,
      data: { startedAt: timelineLayout.startedAt, span: timelineLayout.span },
      draggable: false,
      selectable: false,
      focusable: false,
      deletable: false,
      zIndex: 2,
    };
    return [{ id: node.id, signature: [timelineLayout.startedAt, timelineLayout.span, timelineLayout.usableWidth].join(FIELD_SEPARATOR), item: node }];
  }, [timelineLayout]);

  const nodes = useSignedList<AgentFlowNode | CoordinatorFlowNode | GroupFlowNode | RulerFlowNode>(
    useMemo(() => [...rulerCandidates, ...nodeCandidates], [nodeCandidates, rulerCandidates]),
  );

  const edgePorts = useMemo(() => {
    const ports = new Map<string, { sourcePort: number; targetPort: number }>();
    for (const relation of visibleRelations) ports.set(relation.id, { sourcePort: 0, targetPort: 0 });
    const assign = (endpoint: "source" | "target") => {
      const groups = new Map<string, typeof visibleRelations>();
      for (const relation of visibleRelations) {
        const id = relation[endpoint];
        groups.set(id, [...(groups.get(id) || []), relation]);
      }
      for (const relations of groups.values()) {
        const other = endpoint === "source" ? "target" : "source";
        relations.sort((a, b) => (positions[a[other]]?.y ?? 0) - (positions[b[other]]?.y ?? 0) || (positions[a[other]]?.x ?? 0) - (positions[b[other]]?.x ?? 0) || a.id.localeCompare(b.id));
        relations.forEach((relation, index) => {
          const port = relations.length < 2 ? 0 : (index / (relations.length - 1) - 0.5) * 0.72;
          const current = ports.get(relation.id)!;
          if (endpoint === "source") current.sourcePort = port;
          else current.targetPort = port;
        });
      }
    };
    assign("source");
    assign("target");
    return ports;
  }, [positions, visibleRelations]);

  const edgeCandidates = useMemo<Signed<Edge>[]>(() => visibleRelations.map((relation) => {
    const coordinatorFocus = selection.agentId === COORDINATOR_ID && !selectedRelation;
    const endpointsMatch = !matchedAgentIds || (matchedAgentIds.has(relation.source) && matchedAgentIds.has(relation.target));
    const related = (selectedRelation
      ? relation.id === selectedRelation.id
      : coordinatorFocus
        ? relation.kind === "dispatch" || relation.kind === "model"
        : !chain || (chain.has(relation.source) && chain.has(relation.target)))
      && endpointsMatch;
    const selected = relation.id === selection.relationId;
    const label = relationLabel(relation.kind, t);
    const text = relation.count > 1 ? `${label} ×${relation.count}` : label;
    // Only a relation that fired inside the window flows; an older relation whose
    // target is still online keeps a static coloured line.
    const recent = running && now - toEpochMs(relation.lastTs) < RELATION_RECENT_WINDOW_MS;
    const animated = relation.active && recent && !reduceMotion;
    const sourceName = relation.initiator === OPERATOR_ID ? actorName(OPERATOR_ID) : actorName(relation.source);
    const ariaLabel = t(`collab.direction.${relation.kind}`, { source: sourceName, target: actorName(relation.target) });
    // Every worker has one dispatch edge with the same label; the CSS keeps that
    // label for hover / focus / selection unless the relation fired more than once.
    const multi = relation.count > 1;
    const geometry: CollabEdgeGeometry = layoutMode === "timeline"
      ? (relation.kind === "dispatch" ? "straight" : "smoothstep")
      : "bezier";
    const ports = edgePorts.get(relation.id) ?? { sourcePort: 0, targetPort: 0 };
    const edge: Edge<CollabEdgeData> = {
      id: relation.id,
      source: relation.source,
      target: relation.target,
      type: "floating",
      animated,
      label: text,
      className: `collab-edge relation-${relation.kind} ${multi ? "multi" : ""} ${recent ? "recent" : ""} ${selected ? "selected" : ""} ${related ? "" : "dimmed"}`,
      markerEnd: EDGE_MARKER,
      style: related ? RELATED_STYLE : coordinatorFocus ? COORDINATOR_UNRELATED_STYLE : UNRELATED_STYLE,
      labelStyle: EDGE_LABEL_STYLE,
      labelBgPadding: EDGE_LABEL_PADDING,
      labelBgBorderRadius: 5,
      selectable: true,
      focusable: true,
      deletable: false,
      ariaLabel,
      data: { geometry, ...ports },
    };
    return {
      id: relation.id,
      signature: [relation.kind, relation.source, relation.target, relation.count, relation.lastTs, recent, animated, text, related, selected, coordinatorFocus, ariaLabel, geometry, ports.sourcePort, ports.targetPort].join(FIELD_SEPARATOR),
      item: edge,
    };
  }), [actorName, chain, edgePorts, layoutMode, matchedAgentIds, now, reduceMotion, running, selectedRelation, selection.agentId, selection.relationId, t, visibleRelations]);
  const edges = useSignedList(edgeCandidates);

  // Measurement completes via store.updateNodeInternals without a nodes prop
  // change, so read the lookup directly instead of the setNodes-time flag.
  const nodesInitialized = useNodesInitialized({ includeHiddenNodes: true });
  const skipInitialFit = !!restored.memory.viewport;
  const skipFilterFit = useRef(skipInitialFit);
  const { requestFit } = useCollaborationFit({
    flowReady,
    nodesInitialized,
    nodeCount: nodes.length,
    reduceMotion,
    containerRef: canvasRef,
    skipInitialFit,
    overlay,
    indexOpen,
    detailOpen,
    containerWidth: shellWidth,
  });
  // Filters change which cards are on the canvas; re-frame even when the count stays the same.
  useEffect(() => {
    if (skipFilterFit.current) {
      skipFilterFit.current = false;
      return;
    }
    requestFit();
  }, [layoutMode, onlyAnomalies, recentOnly, scope, requestFit]);
  useEffect(() => {
    if (skipInitialFit) return;
    requestFit();
  }, [deck.runId, requestFit, skipInitialFit]);
  useEffect(() => {
    const known = !selection.agentId || model.agentById.has(selection.agentId);
    dispatch({ type: "dropUnknown", known });
    if (known) return;
    const canvas = canvasRef.current;
    const active = document.activeElement;
    if (active && active !== document.body && !canvas?.contains(active)) return;
    const coord = canvas?.querySelector<HTMLElement>('[data-id="coordinator"]');
    (coord ?? canvas)?.focus({ preventScroll: true });
  }, [dispatch, model.agentById, selection.agentId]);

  useEffect(() => {
    const canvas = canvasRef.current;
    const active = document.activeElement;
    const focusedId = active instanceof HTMLElement ? active.getAttribute("data-id") : null;
    if (focusedId && canvas?.contains(active) && !visibleIds.has(focusedId)) {
      const coord = canvas.querySelector<HTMLElement>('[data-id="coordinator"]');
      (coord ?? canvas).focus({ preventScroll: true });
      return;
    }
    if ((!active || active === document.body) && selection.agentId && selection.agentId !== COORDINATOR_ID && !visibleIds.has(selection.agentId)) {
      const coord = canvas?.querySelector<HTMLElement>('[data-id="coordinator"]');
      (coord ?? canvas)?.focus({ preventScroll: true });
    }
  }, [selection.agentId, visibleIds]);

  const resetLayout = useCallback(() => {
    setLayoutVersion((version) => version + 1);
    requestFit();
  }, [requestFit]);
  // Once the run has ended nothing else moves, so settle every card into its column slot.
  const wasRunning = useRef(running);
  useEffect(() => {
    if (wasRunning.current && !running) resetLayout();
    wasRunning.current = running;
  }, [resetLayout, running]);

  const ariaLabels = useMemo<Partial<AriaLabelConfig>>(() => ({
    "node.a11yDescription.default": t("collab.a11y.node"),
    "node.a11yDescription.keyboardDisabled": t("collab.a11y.node"),
    "edge.a11yDescription.default": t("collab.a11y.edge"),
    "controls.zoomIn.ariaLabel": t("collab.a11y.zoomIn"),
    "controls.zoomOut.ariaLabel": t("collab.a11y.zoomOut"),
    "controls.fitView.ariaLabel": t("collab.a11y.fitView"),
    "minimap.ariaLabel": t("collab.a11y.minimap"),
  }), [t]);

  const selectedAgent = selection.agentId ? view.agentById.get(selection.agentId) : undefined;
  const inspectorAgent = selectedAgent ?? view.coordinator;
  const inspectorTabs = inspectorAgent.role === "coordinator" || inspectorAgent.role === "decision" ? COORDINATOR_INSPECTOR_TABS : INSPECTOR_TABS;
  const selectedKnowledge = selection.knowledgeId ? view.knowledgeById.get(selection.knowledgeId) : undefined;
  const relationSource = selectedRelation ? view.agentById.get(selectedRelation.source) : undefined;
  const relationTarget = selectedRelation ? view.agentById.get(selectedRelation.target) : undefined;
  const relationReferences = useMemo(() => (selectedRelation
    ? selectedRelation.refIds
      .map((id) => view.knowledgeById.get(id))
      .filter((item): item is CollaborationKnowledgeItem => !!item)
    : []), [selectedRelation, view.knowledgeById]);
  const hoverRelation = edgeHover
    ? view.relations.find((relation) => relation.id === edgeHover.relationId)
    : undefined;
  const hoverTitles = hoverRelation
    ? hoverRelation.refIds
      .map((id) => view.knowledgeById.get(id)?.title)
      .filter((title): title is string => !!title)
      .slice(0, 3)
    : [];
  const matchesKnowledge = useCallback((item: CollaborationKnowledgeItem) => {
    if (!q) return true;
    if (item.searchText.includes(q)) return true;
    const actor = actorName(item.agentId);
    if (actor.toLowerCase().includes(q)) return true;
    const status = knowledgeStatusLabel(item, t);
    if (status.toLowerCase().includes(q)) return true;
    return knowledgeLabel(item.kind, t, canvasMode).toLowerCase().includes(q);
  }, [actorName, canvasMode, q, t]);
  const searchedKnowledge = useMemo(() => view.knowledge.filter(matchesKnowledge), [matchesKnowledge, view.knowledge]);
  const searchHits = q
    ? t("collab.searchHits", {
      agents: matchedAgents.length,
      knowledge: searchedKnowledge.length,
    })
    : undefined;
  const filteredKnowledge = useMemo(() => (
    knowledgeKinds.size === 0
      ? searchedKnowledge
      : searchedKnowledge.filter((item) => knowledgeKinds.has(item.kind))
  ), [knowledgeKinds, searchedKnowledge]);
  const kindCounts = useMemo(() => knowledgeKindCounts(searchedKnowledge), [searchedKnowledge]);
  const runStats = useMemo(() => {
    let facts = 0;
    let observations = 0;
    let candidates = 0;
    let deadEnds = 0;
    let pocs = 0;
    for (const item of view.knowledge) {
      if (item.kind === "fact") facts += 1;
      else if (item.kind === "observation") observations += 1;
      else if (item.kind === "candidate") candidates += 1;
      else if (item.kind === "dead_end") deadEnds += 1;
      else if (item.kind === "poc") pocs += 1;
    }
    return { generation: view.currentGeneration, openIntents: view.openIntents.length, facts, observations, candidates, deadEnds, pocs };
  }, [view.currentGeneration, view.knowledge, view.openIntents.length]);
  const toggleKnowledgeKind = useCallback((kind: CollaborationKnowledgeKind) => {
    setKnowledgeKinds((current) => {
      const next = new Set(current);
      if (next.has(kind)) next.delete(kind); else next.add(kind);
      return next;
    });
  }, []);
  const filteredOpenIntents = useMemo(
    () => view.openIntents.filter((intent) => matchesSearch([intent.id, intent.goal, intent.summary, intent.workerClass], q)),
    [q, view.openIntents],
  );
  const liveSelection = selectedRelation
    ? t("collab.a11y.selectedRelation", {
      kind: relationLabel(selectedRelation.kind, t),
      source: actorName(selectedRelation.source),
      target: actorName(selectedRelation.target),
    })
    : selectedAgent
      ? t("collab.a11y.selectedAgent", {
        name: displayFor(selectedAgent).title,
        role: roleLabel(selectedAgent, t),
        status: statusLabel(selectedAgent, t, canvasMode),
      })
      : t("collab.runOverview");
  const liveStatus = t("collab.a11y.live", {
    selection: liveSelection,
    visible: t("collab.a11y.visible", { n: visibleAgents.length, m: visibleRelations.length }),
  });
  const showCoordinatorEmpty = visibleAgents.length === 1 && !((onlyAnomalies || recentOnly) && model.workers.length > 0);
  const emptyWorkers = showCoordinatorEmpty ? coordinatorEmptyCopy(t, deck, model.workers.length > 0) : null;

  const toggleRelation = useCallback((kind: CollaborationRelationKind) => {
    setRelationKinds((current) => {
      const next = new Set(current);
      if (next.has(kind)) next.delete(kind); else next.add(kind);
      return next;
    });
  }, []);
  const resetRelations = useCallback(() => setRelationKinds(new Set(RELATION_KINDS)), []);
  const {
    indexWidth,
    detailWidth,
    resizing,
    startIndexResize,
    startDetailResize,
    onIndexResizeKey,
    onDetailResizeKey,
    resetIndex,
    resetDetail,
  } = useCollaborationSidebarResize();
  const showResizers = !overlay && !mobile;

  // Esc peels one layer; / focuses search; arrows and j/k move between cards;
  // Enter opens the inspector head; [ ] cycle inspector tabs.
  const onShellKeyDown = useCollaborationShortcuts({
    contextMenu,
    onCloseContextMenu: () => setContextMenu(null),
    selection,
    dispatch,
    query,
    setQuery,
    visibleAgents,
    positions,
    selectAgent: moveAgent,
    openDetail,
    focusInspectorHead,
    canvasRef,
    shellRef,
    inspectorTabs,
  });

  useEffect(() => {
    if (!overlay || (!indexOpen && !detailOpen)) return;
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if ((event.target as Element | null)?.closest?.("[role='dialog'], [role='menu'], [role='listbox']")) return;
      if (contextMenu) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      closeOverlays();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [closeOverlays, contextMenu, detailOpen, indexOpen, overlay]);

  const shellClass = `collab-shell layout-${layoutMode} mobile-${mobileView} ${indexOpen ? "index-open" : ""} ${detailOpen ? "detail-open" : ""} ${resizing ? "sidebar-resizing" : ""}`;
  const shellStyle = {
    // Open-target widths only. The animated --collab-*-w vars are toggled by
    // .index-open / .detail-open so @property can interpolate 0px ↔ Npx.
    ...(showResizers ? {
      "--collab-index-open-w": `${indexWidth ?? COLLAB_INDEX_WIDTH_DEFAULT}px`,
      "--collab-detail-open-w": `${detailWidth ?? COLLAB_DETAIL_WIDTH_DEFAULT}px`,
    } : {}),
  } as CSSProperties;

  return (
    <EventStampProvider live={running} finished={deck.finished} origin={deck.startedAt}>
    <CollaborationActionsProvider value={collabActions}>
    <HighlightQueryContext.Provider value={q}>
    <div ref={shellRef} className={shellClass} style={shellStyle} onKeyDownCapture={onShellKeyDown}>
      <div className="sr-only" role="status" aria-live="polite" aria-atomic="true">{liveStatus}</div>
      <CollaborationToolbar
        query={query}
        onQueryChange={setQuery}
        searchHits={searchHits}
        onCycleMatch={cycleMatch}
        scope={scope}
        onScopeChange={setScope}
        onlyAnomalies={onlyAnomalies}
        onToggleAnomalies={() => setOnlyAnomalies((value) => !value)}
        anomalyCount={anomalyCount}
        recentOnly={recentOnly}
        onToggleRecent={() => setRecentOnly((value) => !value)}
        relationKinds={relationKinds}
        onToggleRelation={toggleRelation}
        onResetRelations={resetRelations}
        flowView={flowView}
        onToggleFlow={() => setFlowView((value) => !value)}
        followNew={followNew}
        onToggleFollow={() => setFollowNew((value) => {
          const next = !value;
          if (next) setOutsideNewIds([]);
          return next;
        })}
        layoutMode={layoutMode}
        onLayoutChange={setLayoutMode}
        onExportJson={() => downloadCollaborationJson(deck, model, scope, layoutMode)}
        onExportPng={() => downloadCollaborationPng(nodes, edges, `${deck.runId || "run"}-collaboration.png`)}
        indexOpen={indexOpen}
        onToggleIndex={toggleIndex}
        detailOpen={detailOpen}
        onToggleDetail={toggleDetail}
        onFit={requestFit}
      />
      <div className="collab-mobile-tabs">
        <div
          className="collab-mobile-tablist"
          role="tablist"
          aria-label={t("collab.mobileViews")}
          onKeyDown={(event) => handleTabListKeyDown(event, MOBILE_VIEWS, mobileView, setMobileView)}
        >
          {MOBILE_VIEWS.map((view) => (
            <button
              type="button"
              id={`collab-tab-${view}`}
              role="tab"
              key={view}
              aria-selected={mobileView === view}
              aria-controls={`collab-panel-${view}`}
              tabIndex={mobileView === view ? 0 : -1}
              className={mobileView === view ? "on" : ""}
              onClick={() => setMobileView(view)}
            >
              {t(`collab.mobile.${view}`)}
            </button>
          ))}
        </div>
        <div
          className="collab-scope"
          role="radiogroup"
          aria-label={t("collab.scope")}
          onKeyDown={(event) => {
            if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
            event.preventDefault();
            event.stopPropagation();
            setScope(scope === "current" ? "all" : "current");
            requestAnimationFrame(() => {
              event.currentTarget.querySelector<HTMLElement>("[role='radio'][aria-checked='true']")?.focus();
            });
          }}
        >
          <button
            type="button"
            className={scope === "current" ? "on" : ""}
            role="radio"
            aria-checked={scope === "current"}
            tabIndex={scope === "current" ? 0 : -1}
            onClick={() => setScope("current")}
          >
            {t("collab.scope.current")}
          </button>
          <button
            type="button"
            className={scope === "all" ? "on" : ""}
            role="radio"
            aria-checked={scope === "all"}
            tabIndex={scope === "all" ? 0 : -1}
            onClick={() => setScope("all")}
          >
            {t("collab.scope.all")}
          </button>
        </div>
      </div>
      <CollaborationIndex
        totalIntents={view.openIntents.length}
        totalKnowledge={view.knowledge.length}
        intents={filteredOpenIntents}
        knowledge={filteredKnowledge}
        hasQuery={!!query || knowledgeKinds.size > 0}
        selectedKnowledgeId={selection.knowledgeId}
        actorName={actorName}
        running={running}
        kindCounts={kindCounts}
        knowledgeKinds={knowledgeKinds}
        onToggleKnowledgeKind={toggleKnowledgeKind}
        canvasMode={canvasMode}
        onSpawnWorker={onSpawnWorker}
        onSelectIntent={selectIntent}
        onSelectKnowledge={selectKnowledgeCenteredById}
        onClosePanel={() => setIndexOpen(false)}
        resetKey={deck.runId}
        id="collab-panel-index"
        resizer={showResizers && indexOpen ? (
          <SidebarResizer
            className="collab-index-resizer"
            label={t("collab.resizeIndex")}
            value={indexWidth ?? COLLAB_INDEX_WIDTH_DEFAULT}
            onPointerDown={(event) => startIndexResize(event, shellRef.current)}
            onKeyDown={(event) => onIndexResizeKey(event, shellRef.current)}
            onReset={resetIndex}
          />
        ) : null}
      />
      <section className={`collab-canvas ${flowView ? "flow-view" : ""}`} id="collab-panel-canvas" aria-label={flowView ? t("collab.flow") : t("collab.canvas")} ref={canvasRef} tabIndex={-1}>
        <ReactFlow
          aria-label={t("collab.canvas")}
          nodes={nodes}
          edges={edges}
          nodeTypes={NODE_TYPES}
          edgeTypes={EDGE_TYPES}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          onEdgeMouseEnter={onEdgeMouseEnter}
          onEdgeMouseMove={onEdgeMouseEnter}
          onEdgeMouseLeave={onEdgeMouseLeave}
          onPaneClick={onPaneClick}
          onNodeDoubleClick={onNodeDoubleClick}
          onNodeContextMenu={onNodeContextMenu}
          onPaneContextMenu={onPaneContextMenu}
          onInit={onInit}
          onMoveStart={(event) => {
            setEdgeHover(null);
            if (event) setFollowNew(false);
          }}
          onMoveEnd={(_, viewport) => patchCollabMemory(deck.runId, { viewport, fitted: true })}
          fitView={!skipInitialFit}
          defaultViewport={restored.memory.viewport}
          fitViewOptions={mobile ? { ...FIT_VIEW_OPTIONS, minZoom: ZOOM.min } : FIT_VIEW_OPTIONS}
          nodesDraggable={false}
          nodesConnectable={false}
          elementsSelectable
          deleteKeyCode={null}
          selectionKeyCode={null}
          multiSelectionKeyCode={null}
          onlyRenderVisibleElements
          minZoom={ZOOM.min}
          maxZoom={ZOOM.max}
          proOptions={PRO_OPTIONS}
          defaultEdgeOptions={DEFAULT_EDGE_OPTIONS}
          ariaLabelConfig={ariaLabels}
        >
          <Background variant={BackgroundVariant.Dots} gap={22} size={1} color="var(--line)" />
          <CanvasBar
            mode={canvasMode}
            running={running}
            finished={deck.finished}
            finishedAt={deck.finishedAt}
            agentCount={presentIds.size}
            relationCount={visibleRelations.length}
            layoutStale={layoutStale}
            reduceMotion={reduceMotion}
            legendOpen={legendOpen}
            relationKinds={relationKinds}
            onFit={requestFit}
            onRelayout={resetLayout}
            onToggleLegend={toggleLegend}
            onToggleRelation={toggleRelation}
            replayNote={canReplay ? t("collab.replay.appearance") : undefined}
            presenceToasts={presenceToasts}
            onPresenceClick={centerKeepZoom}
          />
          <MiniMapToggle open={miniMapOpen} onToggle={toggleMiniMap} />
          {miniMapOpen && (
            <MiniMap
              pannable
              zoomable
              position="bottom-right"
              style={MINIMAP_STYLE}
              nodeColor={miniMapColor}
              maskColor="color-mix(in srgb, var(--bg) 72%, transparent)"
              ariaLabel={t("collab.a11y.minimap")}
            />
          )}
        </ReactFlow>
        {outsideNewIds.length > 0 && !followNew && (
          <button type="button" className="collab-outside-agents" onClick={revealOutsideAgents}>
            {t("collab.newAgentsOutside", { n: outsideNewIds.length })}
          </button>
        )}
        {flowView && (
          <FlowMatrix
            agentIds={visibleAgents.filter((agent) => presentIds.has(agent.id)).map((agent) => agent.id)}
            displays={displays}
            relations={visibleRelations}
            selectedRelationId={selection.relationId}
            onSelectRelation={selectRelationById}
          />
        )}
        {!flowView && edgeHover && hoverRelation && (
          <EdgeHoverTip
            x={edgeHover.x}
            y={edgeHover.y}
            source={hoverRelation.initiator === OPERATOR_ID ? actorName(OPERATOR_ID) : actorName(hoverRelation.source)}
            target={actorName(hoverRelation.target)}
            count={hoverRelation.count}
            titles={hoverTitles}
            reduceMotion={reduceMotion}
          />
        )}
        {canReplay && (
          <CollaborationScrubber
            start={toEpochMs(deck.startedAt)}
            end={toEpochMs(deck.finishedAt) || toEpochMs(deck.startedAt)}
            value={asOf}
            playing={playing}
            truncated={truncated}
            marks={marks}
            onChange={onScrub}
            onTogglePlay={onTogglePlay}
          />
        )}
        {deck.finished && (
          <div className="collab-run-banner" role="status">
            {t("collab.runBanner", {
              outcome: t(
                deck.mode === "pentest"
                  ? ((deck.solved || deck.reason.goalMet) ? "collab.legend.goalProven" : "collab.goalUnproven")
                  : (deck.solved ? "collab.legend.solved" : "collab.unsolved"),
              ),
              elapsed: formatElapsed(Math.max(0, toEpochMs(deck.finishedAt) - toEpochMs(deck.startedAt))),
              ended: formatClock(deck.finishedAt, "—"),
              n: model.allAgents.length,
            })}
          </div>
        )}
        {relationKinds.size === 0 && (
          <div className="collab-canvas-notice" role="status">
            <Icon name="eyeOff" size={12} />
            <span>{t("collab.relationsHidden")}</span>
            <button type="button" onClick={resetRelations}>{t("collab.showAllRelations")}</button>
          </div>
        )}
        {q && matchedAgentIds && matchedAgentIds.size === 0 && (
          <div className="collab-canvas-empty">
            <Icon name="search" size={24} />
            <strong>{t("collab.noMatchingAgents")}</strong>
            <span>{t("collab.noMatchingAgentsHint")}</span>
            <button type="button" onClick={() => setQuery("")}>{t("collab.clearSearch")}</button>
          </div>
        )}
        {(onlyAnomalies || recentOnly) && visibleAgents.length === 1 && model.workers.length > 0 && (
          <div className="collab-canvas-empty">
            <Icon name={recentOnly ? "clock" : "checkCircle"} size={24} />
            <strong>{t(recentOnly ? "collab.noRecent" : "collab.noAnomalies")}</strong>
            <span>{recentOnly ? t("collab.noRecentHint", { s: ACTIVITY_FILTER_WINDOW_MS / 1000 }) : t("collab.noAnomaliesHint")}</span>
          </div>
        )}
        {/* Coordinator-only canvas: phase copy when no worker was spawned; scope copy when workers exist but are filtered out. Sits under the card. */}
        {emptyWorkers && (
          <div className="collab-canvas-empty neutral">
            <Icon name={emptyWorkers.icon} size={24} />
            <strong>{emptyWorkers.title}</strong>
            <span>{emptyWorkers.hint}</span>
            {emptyWorkers.failures.length > 0 && (
              <ul>
                {emptyWorkers.failures.map((failure) => (
                  <li key={failure.errorId || failure.profileId}>
                    {[failure.profileId || failure.engine, failure.detail].filter(Boolean).join(" · ")}
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </section>
      <aside className="collab-inspector" id="collab-panel-detail" aria-label={t("collab.details")} ref={inspectorRef}>
        {showResizers && detailOpen ? (
          <SidebarResizer
            className="collab-detail-resizer"
            label={t("collab.resizeDetails")}
            value={detailWidth ?? COLLAB_DETAIL_WIDTH_DEFAULT}
            onPointerDown={(event) => startDetailResize(event, shellRef.current)}
            onKeyDown={(event) => onDetailResizeKey(event, shellRef.current)}
            onReset={resetDetail}
          />
        ) : null}
        {selectedRelation && relationSource && relationTarget ? (
          <RelationInspector
            relation={selectedRelation}
            source={relationSource}
            target={relationTarget}
            references={relationReferences}
            knowledge={view.knowledge}
            intents={deck.blackboard.intents}
            canvasMode={canvasMode}
            actorName={actorName}
            onBack={() => dispatch({ type: "clearRelation" })}
            onSelectAgent={(id) => selectAgent(id, true)}
            onSelectKnowledge={selectKnowledge}
            onOpenFact={onOpenFact}
            onOpenPoc={onOpenPoc}
            onClosePanel={() => setDetailOpen(false)}
          />
        ) : (
          <AgentInspector
            deck={deck}
            agent={inspectorAgent}
            display={displayFor(inspectorAgent)}
            tab={selection.tab}
            selectedKnowledge={selectedKnowledge}
            outOfScope={!!selection.agentId && !visibleIds.has(selection.agentId)}
            unselected={!selection.agentId}
            runStats={runStats}
            knowledge={view.knowledge}
            relations={view.relations}
            knowledgeKinds={knowledgeKinds}
            canvasMode={canvasMode}
            matchesKnowledge={matchesKnowledge}
            query={q}
            actorName={actorName}
            onTab={(tab) => dispatch({ type: "setTab", tab })}
            onToggleKnowledgeKind={toggleKnowledgeKind}
            onCloseKnowledge={() => dispatch({ type: "clearKnowledge" })}
            onSelectKnowledge={selectKnowledge}
            onSelectRow={selectKnowledgeById}
            onSelectRelation={(relation) => selectRelationById(relation.id)}
            onReveal={revealSelected}
            onOpenTimeline={onOpenTimeline}
            onKillWorker={onKillWorker}
            onOpenFact={onOpenFact}
            onOpenPoc={onOpenPoc}
            onClosePanel={() => setDetailOpen(false)}
            asOf={replaying ? asOf : undefined}
          />
        )}
      </aside>
      {contextMenu && (
        <CollaborationContextMenu
          menu={contextMenu}
          agentOnline={contextMenu.nodeId ? model.agentById.get(contextMenu.nodeId)?.online : undefined}
          onClose={() => setContextMenu(null)}
          onOpenWorker={onOpenWorker}
          onOpenTimeline={onOpenTimeline}
          onKillWorker={onKillWorker}
          onFocusRelated={(id) => selectAgent(id, true)}
          onFit={requestFit}
        />
      )}
    </div>
    </HighlightQueryContext.Provider>
    </CollaborationActionsProvider>
    </EventStampProvider>
  );
}

export function AgentCollaborationCanvas(props: CanvasProps) {
  return <ReactFlowProvider key={props.deck.runId}><AgentCollaborationInner {...props} /></ReactFlowProvider>;
}
