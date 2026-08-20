"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, ReactNode, KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from "react";
import { useRun, useRunList, useFolders, newRun, patchRun, deleteRun, uploadFiles, spawnWorker, killWorker, openWorkspace, createFolder, renameFolder, deleteFolder, SavedFile, apiFetch } from "@/lib/useRun";
import { useT, useLang } from "@/lib/i18n";
import { DeckState, GraphNode, ChatMessage, isRunActive, isWorkerLane, workerIds, swarmDigest } from "@/lib/events";
import { I18nProvider } from "@/lib/i18n";
import { ThreadRail } from "@/components/ThreadRail";
import { Conversation } from "@/components/Conversation";
import type { ControlCommandOpts, DispatchOpts } from "@/components/Conversation";
import { LoginGate } from "@/components/LoginGate";
import { CommandPalette } from "@/components/CommandPalette";
import { BtwPanel } from "@/components/BtwPanel";
import { ToastLane, useToasts } from "@/components/Toast";
import type { ArtifactView, SwarmDigest } from "@/lib/events";
import { clampRailWidth, RAIL_WIDTH_DEFAULT, RAIL_WIDTH_STORAGE_KEY } from "@/lib/railSizing";
import { useDeckMotion } from "@/lib/useDeckMotion";
import { Icon, type IconName } from "@/components/Icon";
import { SelectionGlider } from "@/components/SelectionGlider";
import { GraphView } from "@/components/GraphView";
import { NodeInspector } from "@/components/NodeInspector";
import { Blackboard } from "@/components/Blackboard";
import { WorkerLanes } from "@/components/WorkerLanes";
import { EvidenceChain } from "@/components/EvidenceChain";
import { PanelSkeleton } from "@/components/Skeleton";
import { PanelEmpty } from "@/components/PanelEmpty";
import { VulnReportsList } from "@/components/VulnReportDoc";
import { actorDisplayTitle, toWorkerIdentity, workerColor, workerDisplayName, workerEngine } from "@/lib/workers";
import { applySelection, readSavedSelection } from "@/lib/palette-engine";
import {
  ledgerItemContainsId,
  ledgerItemHeight,
  projectActivityLedger,
  toolCommandLabel,
  toolGroupFailedCommand,
  toolGroupLatestCommand,
} from "@/lib/activityLedger";

/**
 * Muteki Command Deck — conversation-first shell.
 *
 * The three pillars are NOT stacked. The spine is a ChatGPT/Claude conversation
 * (task dispatch is conversational); artifact panels are reserved for the two
 * spatial views: fact graph and blackboard. The run summary lives in the home
 * workspace instead of a separate statistics page.
 *
 *   ThreadRail (run list) │ Conversation (spine) │ RuntimeArtifactPanel (graph/blackboard)
 *
 * The deck stays a dumb subscriber (§3): dispatch POSTs /start with the prose
 * prompt (the swarm infers category/target/solvers), commands POST /hitl, and
 * everything else folds back from the run's SSE stream.
 */

// A draft is a local, not-yet-dispatched conversation. It never hits the backend
// until the operator sends a prompt — see dispatch() / onNewSolve().
const newDraftId = () => `draft-${Date.now().toString(36)}-${Math.floor(Math.random() * 1e6).toString(36)}`;
const isDraft = (id: string) => id.startsWith("draft-");
type ThemeMode = "light" | "dark";
const ARTIFACT_WIDTH_MIN = 360;
const ARTIFACT_WIDTH_MAX = 960;
const ARTIFACT_WIDTH_STORAGE_KEY = "muteki.artifact.width";

function artifactWidthMax(viewportWidth?: number): number {
  if (!viewportWidth || viewportWidth <= 0) return ARTIFACT_WIDTH_MAX;
  return Math.max(ARTIFACT_WIDTH_MIN, Math.min(ARTIFACT_WIDTH_MAX, Math.round(viewportWidth * 0.72)));
}

function artifactWidthDefault(viewportWidth?: number): number {
  return clampArtifactWidth(Math.round((viewportWidth || 1280) * 0.56), viewportWidth);
}

function clampArtifactWidth(width: number, viewportWidth?: number): number {
  const next = Number.isFinite(width) ? width : artifactWidthDefault(viewportWidth);
  return Math.round(Math.min(artifactWidthMax(viewportWidth), Math.max(ARTIFACT_WIDTH_MIN, next)));
}

// per-run routing (/run/<id>): the URL is derived from the active run id (drafts
// map to "/" since they have no backend row yet). Reading it on load restores a
// deep-linked conversation; the dynamic route app/run/[id]/page.tsx serves it.
const runIdFromPath = (): string => {
  if (typeof window === "undefined") return "";
  const m = window.location.pathname.match(/^\/run\/([^/]+)\/?$/);
  return m ? decodeURIComponent(m[1]) : "";
};
const urlForRun = (id: string): string =>
  id && !isDraft(id) ? `/run/${encodeURIComponent(id)}` : "/";

type RuntimeGroup = "observe" | "investigate" | "assets";
type RuntimeTab = { view: ArtifactView; key: string; group: RuntimeGroup; icon: IconName };

const RUNTIME_GROUPS: { id: RuntimeGroup; key: string; descKey: string; icon: IconName }[] = [
  { id: "observe", key: "runtime.group.observe", descKey: "runtime.group.observeDesc", icon: "radio" },
  { id: "investigate", key: "runtime.group.investigate", descKey: "runtime.group.investigateDesc", icon: "crosshair" },
  { id: "assets", key: "runtime.group.assets", descKey: "runtime.group.assetsDesc", icon: "layers" },
];

const RUNTIME_TABS: RuntimeTab[] = [
  { view: "timeline", key: "panelbtn.timeline", group: "observe", icon: "rows" },
  { view: "workers", key: "panelbtn.workers", group: "observe", icon: "cpu" },
  { view: "graph", key: "rc.factGraph", group: "observe", icon: "network" },
  { view: "evidence", key: "panelbtn.evidence", group: "investigate", icon: "layers" },
  { view: "blackboard", key: "rc.blackboard", group: "investigate", icon: "board" },
  { view: "findings", key: "panelbtn.findings", group: "investigate", icon: "alert" },
  { view: "reports", key: "panelbtn.reports", group: "assets", icon: "list" },
  { view: "credentials", key: "panelbtn.credentials", group: "assets", icon: "lock" },
  { view: "pocs", key: "panelbtn.pocs", group: "assets", icon: "terminal" },
  { view: "routes", key: "panelbtn.routes", group: "assets", icon: "network" },
  { view: "directives", key: "panelbtn.directives", group: "assets", icon: "send" },
];

const RUNTIME_COPY: Record<string, { zh: string; en: string }> = {
  "runtime.title": { zh: "运行时", en: "Runtime" },
  "runtime.untitled": { zh: "未命名任务", en: "Untitled run" },
  "runtime.group.observe": { zh: "轨迹", en: "Trace" },
  "runtime.group.observeDesc": { zh: "事件、Worker 与执行关系", en: "Events, workers, and execution relationships" },
  "runtime.group.investigate": { zh: "调查", en: "Investigation" },
  "runtime.group.investigateDesc": { zh: "证据、知识与审查结果", en: "Evidence, knowledge, and review results" },
  "runtime.group.assets": { zh: "资产", en: "Assets" },
  "runtime.group.assetsDesc": { zh: "漏洞报告、凭据、PoC、路线与指令", en: "Reports, credentials, PoCs, routes, and directives" },
  "runtime.status.live": { zh: "实时", en: "Live" },
  "runtime.status.complete": { zh: "已结束", en: "Complete" },
  "runtime.status.standby": { zh: "待命", en: "Standby" },
  "runtime.backToConversation": { zh: "返回对话", en: "Back to conversation" },
  "runtime.trace.title": { zh: "运行图谱", en: "Runtime graph" },
  "runtime.trace.coordinator": { zh: "调度", en: "Control" },
  "runtime.trace.workers": { zh: "Worker", en: "Worker" },
  "runtime.trace.tools": { zh: "工具", en: "Tools" },
  "runtime.trace.evidence": { zh: "证据", en: "Evidence" },
  "runtime.trace.relations": { zh: "关系线", en: "Relations" },
  "runtime.trace.fit": { zh: "适应窗口", en: "Fit view" },
  "runtime.trace.zoomLevel": { zh: "当前缩放", en: "Current zoom" },
  "runtime.trace.window": { zh: "当前窗口", en: "Visible window" },
  "runtime.trace.dragHint": { zh: "滚轮或触控板缩放 · 横向手势或拖动平移 · 双击复位", en: "Wheel or pinch to zoom · swipe or drag to pan · double-click to reset" },
  "runtime.trace.selectHint": { zh: "点击事件区块查看内容，并定位到下方事件记录。", en: "Select an event to inspect it and locate it in the ledger." },
  "runtime.trace.actor": { zh: "来源", en: "Actor" },
  "runtime.trace.type": { zh: "类型", en: "Type" },
  "runtime.trace.time": { zh: "时间", en: "Time" },
  "runtime.trace.closeDetail": { zh: "关闭事件详情", en: "Close event details" },
  "runtime.trace.events": { zh: "{n} 事件", en: "{n} events" },
  "runtime.trace.actors": { zh: "{n} 来源", en: "{n} actors" },
  "runtime.trace.calls": { zh: "{n} 调用", en: "{n} calls" },
  "runtime.trace.workerTotal": { zh: "{n} 个 Worker", en: "{n} workers" },
  "runtime.trace.noWorkers": { zh: "暂无 Worker", en: "No workers" },
  "runtime.ledger.title": { zh: "事件记录", en: "Event ledger" },
  "runtime.ledger.visible": { zh: "显示 {visible}/{total}", en: "Showing {visible}/{total}" },
  "runtime.ledger.search": { zh: "搜索事件、输出或 Worker", en: "Search events, output, or workers" },
  "runtime.ledger.event": { zh: "事件", en: "Event" },
  "runtime.ledger.content": { zh: "内容", en: "Content" },
  "runtime.ledger.time": { zh: "时间", en: "Time" },
  "runtime.ledger.jumpLatest": { zh: "回到最新", en: "Jump to latest" },
  "runtime.ledger.toolGroup": { zh: "调用了 {n} 次工具", en: "Called {n} tools" },
  "runtime.ledger.toolGroupLatest": { zh: "调用了 {n} 次工具 · 最近 {cmd}", en: "Called {n} tools · latest {cmd}" },
  "runtime.ledger.toolFailed": { zh: "失败 {cmd}", en: "Failed {cmd}" },
  "runtime.ledger.toolPending": { zh: "进行中", en: "Running" },
  "runtime.event.tools": { zh: "工具组", en: "Tools" },
  "runtime.trace.collapse": { zh: "收起运行图谱", en: "Collapse runtime graph" },
  "runtime.trace.expand": { zh: "展开运行图谱", en: "Expand runtime graph" },
  "runtime.event.input": { zh: "输入", en: "Input" },
  "runtime.event.system": { zh: "系统", en: "System" },
  "runtime.event.tool": { zh: "工具", en: "Tool" },
  "runtime.event.reasoning": { zh: "推理", en: "Reasoning" },
  "runtime.event.insight": { zh: "洞察", en: "Insight" },
  "runtime.event.guidance": { zh: "引导", en: "Guidance" },
  "runtime.event.result": { zh: "结果", en: "Result" },
  "runtime.event.worker": { zh: "Worker", en: "Worker" },
  "runtime.event.agent": { zh: "协调器", en: "Agent" },
  "runtime.findings.accepted": { zh: "证据门槛通过", en: "Evidence accepted" },
  "runtime.findings.review": { zh: "审查记录", en: "Review record" },
  "runtime.findings.resource": { zh: "资源", en: "Resource" },
  "runtime.findings.identities": { zh: "身份", en: "Identities" },
  "runtime.reports.accepted": { zh: "已入库", en: "Accepted" },
  "runtime.reports.submitted": { zh: "待复现", en: "Submitted" },
  "runtime.reports.reproduced": { zh: "已复现", en: "Reproduced" },
  "runtime.reports.rejected": { zh: "已拒绝", en: "Rejected" },
  "runtime.reports.reproFailed": { zh: "复现失败", en: "Reproduction failed" },
  "runtime.reports.severity.critical": { zh: "严重", en: "Critical" },
  "runtime.reports.severity.high": { zh: "高危", en: "High" },
  "runtime.reports.severity.medium": { zh: "中危", en: "Medium" },
  "runtime.reports.severity.low": { zh: "低危", en: "Low" },
  "runtime.reports.cvss": { zh: "参考向量", en: "Reference vector" },
  "runtime.reports.expand": { zh: "展开完整内容", en: "Expand report" },
  "runtime.reports.collapse": { zh: "收起", en: "Collapse report" },
  "runtime.reports.class": { zh: "类型", en: "Type" },
  "runtime.reports.resource": { zh: "位置", en: "Location" },
  "runtime.reports.impact": { zh: "影响", en: "Impact" },
  "runtime.reports.witness": { zh: "证明输出", en: "Proof of concept output" },
  "runtime.reports.copyMarkdown": { zh: "复制 Markdown", en: "Copy Markdown" },
  "runtime.reports.copyMarkdownAria": { zh: "复制报告 Markdown：{text}", en: "Copy report Markdown: {text}" },
  "runtime.reports.copyCollection": { zh: "复制合集", en: "Copy collection" },
  "runtime.reports.copyCollectionAria": { zh: "复制漏洞报告集 Markdown", en: "Copy vulnerability report collection Markdown" },
  "runtime.reports.summary": { zh: "漏洞概要", en: "Summary" },
  "runtime.reports.preconditions": { zh: "先决条件", en: "Prerequisites" },
  "runtime.reports.role": { zh: "影响对象", en: "Affected party" },
  "runtime.reports.steps": { zh: "复现步骤", en: "Steps to reproduce" },
  "runtime.reports.replay": { zh: "PoC", en: "PoC" },
  "runtime.reports.narrative": { zh: "漏洞概要", en: "Summary" },
  "runtime.reports.markdownSource": { zh: "Markdown 原文", en: "Markdown source" },
  "runtime.reports.missing": { zh: "（未填写）", en: "(not provided)" },
  "runtime.workers.overview": { zh: "执行阵列", en: "Execution roster" },
  "runtime.workers.online": { zh: "在线", en: "Online" },
  "runtime.workers.active": { zh: "活动", en: "Active" },
  "runtime.workers.solved": { zh: "完成", en: "Solved" },
  "runtime.workers.calls": { zh: "调用", en: "Calls" },
};

function useRuntimeT() {
  const base = useT();
  const { lang } = useLang();
  return useCallback((key: string, vars?: Record<string, string | number>) => {
    const copy = RUNTIME_COPY[key]?.[lang];
    if (!copy) return base(key, vars);
    return Object.entries(vars ?? {}).reduce((text, [name, value]) => text.replaceAll(`{${name}}`, String(value)), copy);
  }, [base, lang]);
}

const runtimeTsMs = (ts?: number) => !ts ? 0 : ts < 1e12 ? ts * 1000 : ts;
const runtimeClock = (ts?: number) => {
  const ms = runtimeTsMs(ts);
  if (!ms) return "--:--:--";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
};
const runtimeDuration = (ms: number) => {
  const total = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
};

// Live-ticking elapsed for the worker roster bar: ticks every 1s while the run is
// open, freezes at finishedAt once ended. Returns runtimeDuration's M:SS format.
function useRuntimeElapsed(startedAt?: number, finishedAt?: number): string {
  const live = startedAt != null && finishedAt == null;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!live) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [live]);
  if (startedAt == null) return "";
  const end = finishedAt != null ? runtimeTsMs(finishedAt) : now;
  return runtimeDuration(end - runtimeTsMs(startedAt));
}

function runtimeTabCount(view: ArtifactView, deck: DeckState): number | null {
  switch (view) {
    case "timeline": return deck.chat.length;
    case "workers": return Object.keys(deck.lanes).length;
    case "graph": return deck.model.nodes.length;
    case "evidence": return deck.blackboard.facts.length + deck.blackboard.deadEnds.length;
    case "blackboard": return deck.blackboard.facts.length + deck.blackboard.intents.length + deck.blackboard.pocs.length;
    case "findings": return deck.blackboard.reviewFindings?.length ?? 0;
    case "reports": return deck.blackboard.vulnReports?.length ?? 0;
    case "pocs": return deck.blackboard.pocs.length;
    case "routes": return (deck.blackboard.suppressedRoutes?.length ?? 0) + (deck.blackboard.branches?.length ?? 0);
    case "directives": return (deck.blackboard.directives?.length ?? 0) + deck.operatorDirectives.length;
    case "credentials": return null;
    default: {
      const _exhaustive: never = view;
      return _exhaustive;
    }
  }
}

type RuntimeTraceLane = "control" | "worker" | "tool" | "evidence";
type RuntimeTraceRecord = {
  message: ChatMessage;
  index: number;
  time: number;
  lane: RuntimeTraceLane;
  actor: string;
};

const RUNTIME_TRACE_LANE_Y: Record<RuntimeTraceLane, number> = { control: 18, worker: 50, tool: 82, evidence: 114 };

function runtimeTraceLane(message: ChatMessage): RuntimeTraceLane {
  if (message.kind === "flag" || message.kind === "insight") return "evidence";
  if (message.kind === "tool") return "tool";
  if (message.solverId && isWorkerLane(message.solverId)) return "worker";
  return "control";
}

function RuntimeTraceOverview({ deck, running, selectedId, onSelect }: {
  deck: DeckState;
  running: boolean;
  selectedId: string | null;
  onSelect: (id: string | null) => void;
}) {
  const t = useRuntimeT();
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState(0);
  const [showRelations, setShowRelations] = useState(true);
  // Collapse state persists per browser; the graph body folds away but the
  // head row stays as a compact summary strip.
  const [collapsed, setCollapsed] = useState(() => {
    if (typeof window === "undefined") return false;
    try { return window.localStorage.getItem("muteki.traceCollapsed") === "1"; } catch { return false; }
  });
  const toggleCollapsed = useCallback(() => {
    setCollapsed((value) => {
      const next = !value;
      try { window.localStorage.setItem("muteki.traceCollapsed", next ? "1" : "0"); } catch { /* storage unavailable */ }
      return next;
    });
  }, []);
  const [dragging, setDragging] = useState(false);
  const plotRef = useRef<HTMLDivElement>(null);
  const navigatorRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef<{ pointerId: number; startX: number; startPan: number } | null>(null);
  const navDragRef = useRef<number | null>(null);
  const zoomRef = useRef(zoom);
  const panRef = useRef(pan);
  const wheelFrameRef = useRef<number | null>(null);
  const wheelIntentRef = useRef<{ mode: "pan" | "zoom"; delta: number; focus: number } | null>(null);
  const dragFrameRef = useRef<number | null>(null);
  const pendingDragPanRef = useRef<number | null>(null);
  const records = useMemo<RuntimeTraceRecord[]>(() => deck.chat
    .map((message, index) => ({
      message,
      index,
      time: runtimeTsMs(message.ts),
      lane: runtimeTraceLane(message),
      actor: message.solverId || message.role,
    }))
    .filter((record) => record.time > 0), [deck.chat]);
  const workerSiblings = useMemo(
    () => workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id])),
    [deck],
  );
  const start = runtimeTsMs(deck.startedAt) || records[0]?.time || Date.now();
  const last = records[records.length - 1]?.time || start;
  const runtimeNow = useMemo(() => Date.now(), [records.length, running]);
  const end = running ? Math.max(runtimeNow, last, start + 1) : Math.max(runtimeTsMs(deck.finishedAt), last, start + 1);
  const domain = Math.max(1, end - start);
  const visibleDuration = domain / zoom;
  const maxOffset = Math.max(0, domain - visibleDuration);
  const viewStart = start + pan * maxOffset;
  const viewEnd = viewStart + visibleDuration;
  const selectedRecord = useMemo(() => records.find((record) => record.message.id === selectedId) ?? null, [records, selectedId]);
  const lanes = [
    { id: "control", label: t("runtime.trace.coordinator") },
    { id: "worker", label: t("runtime.trace.workers") },
    { id: "tool", label: t("runtime.trace.tools") },
    { id: "evidence", label: t("runtime.trace.evidence") },
  ] as { id: RuntimeTraceLane; label: string }[];
  const laneCounts = useMemo(() => records.reduce<Record<RuntimeTraceLane, number>>((counts, record) => {
    counts[record.lane] += 1;
    return counts;
  }, { control: 0, worker: 0, tool: 0, evidence: 0 }), [records]);
  const visibleRecords = useMemo(() => records.filter((record) => record.time >= viewStart && record.time <= viewEnd), [records, viewStart, viewEnd]);
  const renderedRecords = useMemo(() => {
    // Keep the graph responsive on long runs by grouping dense events into
    // lane-aware time buckets. Evidence and the selected event always remain
    // individually addressable.
    const bucketCount = 76;
    const buckets = new Map<string, RuntimeTraceRecord>();
    const priority = new Map<string, RuntimeTraceRecord>();
    visibleRecords.forEach((record) => {
      if (record.message.id === selectedId || record.lane === "evidence") {
        priority.set(record.message.id, record);
        return;
      }
      const ratio = Math.max(0, Math.min(.9999, (record.time - viewStart) / visibleDuration));
      const key = `${record.lane}-${Math.floor(ratio * bucketCount)}`;
      const current = buckets.get(key);
      if (!current || record.message.kind === "flag" || record.index > current.index) buckets.set(key, record);
    });
    return [...buckets.values(), ...priority.values()].sort((a, b) => a.time - b.time || a.index - b.index);
  }, [visibleRecords, selectedId, viewStart, visibleDuration]);
  const positionFor = useCallback((time: number) => Math.max(0, Math.min(99.35, ((time - viewStart) / visibleDuration) * 100)), [viewStart, visibleDuration]);
  const connections = useMemo(() => {
    const links: { id: string; from: RuntimeTraceRecord; to: RuntimeTraceRecord; kind: "actor" | "flow" }[] = [];
    const lastByActor = new Map<string, RuntimeTraceRecord>();
    let previous: RuntimeTraceRecord | null = null;
    renderedRecords.forEach((record) => {
      const actorPrevious = lastByActor.get(record.actor);
      if (actorPrevious && actorPrevious.message.id !== record.message.id) links.push({ id: `actor-${actorPrevious.message.id}-${record.message.id}`, from: actorPrevious, to: record, kind: "actor" });
      if (previous && previous.actor !== record.actor && previous.lane !== record.lane) links.push({ id: `flow-${previous.message.id}-${record.message.id}`, from: previous, to: record, kind: "flow" });
      lastByActor.set(record.actor, record);
      previous = record;
    });
    if (links.length <= 144) return links;
    const selectedLinks = links.filter((link) => link.from.message.id === selectedId || link.to.message.id === selectedId);
    const stride = Math.ceil(links.length / Math.max(1, 144 - selectedLinks.length));
    const sampled = links.filter((_, index) => index % stride === 0);
    return [...new Map([...sampled, ...selectedLinks].map((link) => [link.id, link])).values()].slice(-144);
  }, [renderedRecords, selectedId]);

  const navigatorRecords = useMemo(() => {
    const stride = Math.max(1, Math.ceil(records.length / 220));
    return records.filter((_, index) => index % stride === 0);
  }, [records]);

  useEffect(() => { zoomRef.current = zoom; }, [zoom]);
  useEffect(() => { panRef.current = pan; }, [pan]);

  // Recenter on the selected event exactly ONCE per selection change. This
  // must NOT depend on the viewport (viewStart/viewEnd/zoom/…): otherwise any
  // operator pan that moves the selection out of the window gets yanked back
  // by this effect, fighting the user's own scroll and producing visible
  // jitter. After the initial centering, the operator owns the viewport.
  const selectedMessageId = selectedRecord?.message.id ?? null;
  useEffect(() => {
    if (!selectedRecord || zoom <= 1 || (selectedRecord.time >= viewStart && selectedRecord.time <= viewEnd)) return;
    const nextStart = Math.max(start, Math.min(end - visibleDuration, selectedRecord.time - visibleDuration / 2));
    setPan(maxOffset ? (nextStart - start) / maxOffset : 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedMessageId]);

  const commitViewport = useCallback((nextZoom: number, nextPan: number) => {
    const safeZoom = Math.max(1, Math.min(12, nextZoom));
    const safePan = safeZoom <= 1.001 ? 0 : Math.max(0, Math.min(1, nextPan));
    zoomRef.current = safeZoom;
    panRef.current = safePan;
    setZoom(safeZoom);
    setPan(safePan);
  }, []);
  const fitView = useCallback(() => commitViewport(1, 0), [commitViewport]);

  useEffect(() => {
    const canvas = plotRef.current;
    if (!canvas) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      const rect = canvas.getBoundingClientRect();
      if (!rect.width) return;
      const unit = event.deltaMode === WheelEvent.DOM_DELTA_LINE ? 16 : event.deltaMode === WheelEvent.DOM_DELTA_PAGE ? rect.width : 1;
      const horizontal = event.shiftKey || Math.abs(event.deltaX) > Math.abs(event.deltaY) * .8;
      const pinchScale = event.ctrlKey ? 5 : 1;
      const delta = horizontal ? (Math.abs(event.deltaX) > 0 ? event.deltaX : event.deltaY) * unit : event.deltaY * unit * pinchScale;
      const focus = Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width));
      const previous = wheelIntentRef.current;
      wheelIntentRef.current = previous?.mode === (horizontal ? "pan" : "zoom")
        ? { mode: previous.mode, delta: previous.delta + delta, focus }
        : { mode: horizontal ? "pan" : "zoom", delta, focus };
      if (wheelFrameRef.current !== null) return;
      wheelFrameRef.current = window.requestAnimationFrame(() => {
        wheelFrameRef.current = null;
        const intent = wheelIntentRef.current;
        wheelIntentRef.current = null;
        if (!intent) return;
        const currentZoom = zoomRef.current;
        const currentPan = panRef.current;
        if (intent.mode === "pan") {
          if (currentZoom <= 1) return;
          commitViewport(currentZoom, currentPan + intent.delta / (rect.width * Math.max(1, currentZoom - 1)));
          return;
        }
        const nextZoom = Math.max(1, Math.min(12, currentZoom * Math.exp(-intent.delta * .0018)));
        const currentDuration = domain / currentZoom;
        const currentOffset = currentPan * Math.max(0, domain - currentDuration);
        const focusTime = start + currentOffset + intent.focus * currentDuration;
        const nextDuration = domain / nextZoom;
        const nextMaxOffset = Math.max(0, domain - nextDuration);
        const nextStart = Math.max(start, Math.min(end - nextDuration, focusTime - intent.focus * nextDuration));
        commitViewport(nextZoom, nextMaxOffset ? (nextStart - start) / nextMaxOffset : 0);
      });
    };
    canvas.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      canvas.removeEventListener("wheel", onWheel);
      if (wheelFrameRef.current !== null) window.cancelAnimationFrame(wheelFrameRef.current);
      if (dragFrameRef.current !== null) window.cancelAnimationFrame(dragFrameRef.current);
      wheelFrameRef.current = null;
      wheelIntentRef.current = null;
      dragFrameRef.current = null;
      pendingDragPanRef.current = null;
    };
  }, [commitViewport, domain, end, start]);
  const moveNavigator = (clientX: number) => {
    const rect = navigatorRef.current?.getBoundingClientRect();
    if (!rect || zoom <= 1) return;
    const ratio = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    const windowRatio = 1 / zoom;
    const startRatio = Math.max(0, Math.min(1 - windowRatio, ratio - windowRatio / 2));
    commitViewport(zoomRef.current, startRatio / (1 - windowRatio));
  };
  const startPlotDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (zoom <= 1 || (event.target as HTMLElement).closest("button")) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    dragRef.current = { pointerId: event.pointerId, startX: event.clientX, startPan: pan };
    setDragging(true);
  };
  const movePlotDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    const origin = dragRef.current;
    const width = plotRef.current?.clientWidth || 1;
    if (!origin || origin.pointerId !== event.pointerId || zoom <= 1) return;
    pendingDragPanRef.current = Math.max(0, Math.min(1, origin.startPan - (event.clientX - origin.startX) / (width * (zoomRef.current - 1))));
    if (dragFrameRef.current !== null) return;
    dragFrameRef.current = window.requestAnimationFrame(() => {
      dragFrameRef.current = null;
      if (pendingDragPanRef.current === null) return;
      commitViewport(zoomRef.current, pendingDragPanRef.current);
      pendingDragPanRef.current = null;
    });
  };
  const stopPlotDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (dragRef.current?.pointerId !== event.pointerId) return;
    dragRef.current = null;
    setDragging(false);
  };
  return (
    <section className={`runtime-trace ${collapsed ? "collapsed" : ""}`} aria-label={t("runtime.trace.title")}>
      <div className="runtime-trace-head">
        <button type="button" className="runtime-trace-collapse" onClick={toggleCollapsed} aria-expanded={!collapsed} title={t(collapsed ? "runtime.trace.expand" : "runtime.trace.collapse")}><Icon name={collapsed ? "chevronDown" : "chevronUp"} size={13} /></button>
        <div className="runtime-trace-title"><Icon name="radio" size={13} /><span>{t("runtime.trace.title")}</span>{running && <span className="runtime-trace-live">{t("runtime.status.live")}</span>}</div>
        {collapsed
          ? <span className="runtime-trace-hint">{t("runtime.trace.events", { n: records.length })} · {runtimeDuration(end - start)}</span>
          : <span className="runtime-trace-hint">{t("runtime.trace.dragHint")}</span>}
        {!collapsed && <div className="runtime-trace-controls" role="toolbar" aria-label={t("runtime.trace.title")}>
          <button type="button" className={showRelations ? "on" : ""} onClick={() => setShowRelations((value) => !value)} aria-pressed={showRelations} title={t("runtime.trace.relations")}><Icon name="network" size={13} /><span>{t("runtime.trace.relations")}</span></button>
          <button type="button" onClick={fitView} title={t("runtime.trace.fit")}><Icon name="crosshair" size={13} /><span>{t("runtime.trace.fit")}</span></button>
          <output className="runtime-trace-zoom" aria-label={t("runtime.trace.zoomLevel")} title={t("runtime.trace.dragHint")}>{zoom.toFixed(zoom % 1 ? 1 : 0)}×</output>
          <time>{runtimeDuration(end - start)}</time>
        </div>}
      </div>
      <div className="runtime-trace-fold">
      <div className="runtime-trace-stage">
        <div className="runtime-trace-labels" aria-hidden="true">
          {lanes.map((lane) => <span key={lane.id} className={`lane-${lane.id}`}><i /> <em>{lane.label}</em><b>{laneCounts[lane.id]}</b></span>)}
        </div>
        <div className={`runtime-trace-plot ${dragging ? "dragging" : ""}`}>
          <div ref={plotRef} className="runtime-trace-canvas" onDoubleClick={fitView} onPointerDown={startPlotDrag} onPointerMove={movePlotDrag} onPointerUp={stopPlotDrag} onPointerCancel={stopPlotDrag}>
            <svg className={`runtime-trace-relations ${showRelations ? "visible" : ""}`} viewBox="0 0 1000 132" preserveAspectRatio="none" aria-hidden="true">
              {connections.map((link) => {
                const x1 = positionFor(link.from.time) * 10;
                const x2 = positionFor(link.to.time) * 10;
                const y1 = RUNTIME_TRACE_LANE_Y[link.from.lane];
                const y2 = RUNTIME_TRACE_LANE_Y[link.to.lane];
                const cls = `${link.kind} ${link.from.message.id === selectedId || link.to.message.id === selectedId ? "selected" : ""}`;
                if (link.from.lane === link.to.lane) {
                  // Same-lane actor chains stay horizontal-tangent sweeps.
                  const curve = Math.max(8, Math.min(80, Math.abs(x2 - x1) * .35));
                  return <path key={link.id} className={cls} d={`M ${x1} ${y1} C ${x1 + curve} ${y1}, ${x2 - curve} ${y2}, ${x2} ${y2}`} />;
                }
                // Cross-lane links drop out of the upper block's bottom edge
                // and land on the lower block's top edge with vertical
                // tangents — no floating rounded elbow mid-air.
                const dir = y2 > y1 ? 1 : -1;
                const sy = y1 + 6 * dir;
                const ey = y2 - 6 * dir;
                const k = Math.max(10, Math.min(28, Math.abs(ey - sy) * .5));
                return <path key={link.id} className={cls} d={`M ${x1} ${sy} C ${x1} ${sy + k * dir}, ${x2} ${ey - k * dir}, ${x2} ${ey}`} />;
              })}
            </svg>
            {lanes.map((lane) => <div className={`runtime-trace-track lane-${lane.id}`} key={lane.id}>
              {renderedRecords.map((record, index) => {
                if (record.lane !== lane.id) return null;
                const nextTime = renderedRecords[index + 1]?.time ?? record.time + visibleDuration * .006;
                const width = Math.max(.42, Math.min(3.2, ((Math.max(record.time, nextTime) - record.time) / visibleDuration) * 100));
                const style = {
                  "--rt-left": `${positionFor(record.time)}%`,
                  "--rt-width": `${width}%`,
                  ...(lane.id === "worker" && record.message.solverId ? { "--rt-color": workerColor(record.message.solverId) } : {}),
                } as CSSProperties;
                return <button type="button" key={record.message.id || record.index} className={`runtime-trace-mark kind-${record.message.kind} ${record.message.id === selectedId ? "selected" : ""}`} style={style} title={`${runtimeClock(record.message.ts)} · ${actorDisplayTitle(record.actor, t, toWorkerIdentity(record.actor, deck.lanes[record.actor]), workerSiblings)} · ${record.message.content}`} aria-pressed={record.message.id === selectedId} onClick={() => onSelect(record.message.id === selectedId ? null : record.message.id)} />;
              })}
            </div>)}
            {running && viewEnd >= end - 1000 && <span className="runtime-trace-now" aria-hidden="true" />}
          </div>
          <div className="runtime-trace-scale" aria-hidden="true"><span>{runtimeClock(viewStart)}</span><span>{t("runtime.trace.window")}</span><span>{runtimeClock(viewEnd)}</span></div>
          <div ref={navigatorRef} className="runtime-trace-navigator" onPointerDown={(event) => { event.currentTarget.setPointerCapture(event.pointerId); navDragRef.current = event.pointerId; moveNavigator(event.clientX); }} onPointerMove={(event) => { if (navDragRef.current === event.pointerId) moveNavigator(event.clientX); }} onPointerUp={(event) => { if (navDragRef.current === event.pointerId) navDragRef.current = null; }} onPointerCancel={() => { navDragRef.current = null; }}>
            <div className="runtime-trace-density" aria-hidden="true">{navigatorRecords.map((record) => <i key={record.message.id} className={`lane-${record.lane}`} style={{ left: `${Math.max(0, Math.min(100, ((record.time - start) / domain) * 100))}%` }} />)}</div>
            <button type="button" role="slider" className="runtime-trace-window" style={{ left: `${pan * (100 - 100 / zoom)}%`, width: `${100 / zoom}%` }} aria-label={t("runtime.trace.window")} aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(pan * 100)} onKeyDown={(event) => { if (event.key === "ArrowLeft" || event.key === "ArrowRight") { event.preventDefault(); setPan((value) => Math.max(0, Math.min(1, value + (event.key === "ArrowLeft" ? -.04 : .04)))); } }} />
          </div>
        </div>
      </div>
      </div>
    </section>
  );
}

function RuntimeActivityStream({ deck, selectedEventId, onSelectEvent, focusSpeaker }: { deck: DeckState; selectedEventId: string | null; onSelectEvent: (id: string | null) => void; focusSpeaker?: { id: string; nonce: number } | null }) {
  const t = useRuntimeT();
  const { lang } = useLang();
  const [query, setQuery] = useState("");
  const [compact, setCompact] = useState(() => {
    if (typeof window === "undefined") return false;
    try { return window.localStorage.getItem("muteki.activity.compact") === "1"; } catch { return false; }
  });
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [expandedGroupId, setExpandedGroupId] = useState<string | null>(null);
  const [shown, setShown] = useState<Set<string>>(new Set());
  // Reverse focus-seed: when the operator clicks "在活动流中查看" in Worker 详情,
  // seed the speaker filter to that worker (nonce-gated so re-clicks re-seed).
  const lastSpeakerNonce = useRef<number | null>(null);
  useEffect(() => {
    if (!focusSpeaker || focusSpeaker.nonce === lastSpeakerNonce.current) return;
    lastSpeakerNonce.current = focusSpeaker.nonce;
    setShown(new Set([focusSpeaker.id]));
  }, [focusSpeaker]);
  const [viewport, setViewport] = useState({ top: 0, height: 360 });
  const scrollerRef = useRef<HTMLDivElement>(null);
  const scrollFrameRef = useRef<number | null>(null);
  const pendingViewportRef = useRef<{ top: number; height: number } | null>(null);
  // Follow-latest semantics (chat-app style): the ledger pins itself to the
  // newest event until the operator scrolls away; while unpinned, incoming
  // events only increment the "jump to latest" badge instead of yanking the
  // scroll position. `pinned` mirrors pinnedRef into state for the button.
  const [pinned, setPinned] = useState(true);
  const [unread, setUnread] = useState(0);
  const pinnedRef = useRef(true);
  const jumpingRef = useRef(false);
  const prevLenRef = useRef(0);
  const speakerKey = (message: ChatMessage) => message.solverId || message.role;
  const workerSiblings = useMemo(
    () => workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id])),
    [deck],
  );
  const speakers = useMemo(() => {
    const seen = new Map<string, string>();
    deck.chat.forEach((message) => seen.set(speakerKey(message), message.solverId || message.role));
    return [...seen.entries()];
  }, [deck.chat]);
  const visible = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    return deck.chat.filter((message) => {
      if (shown.size && !shown.has(speakerKey(message))) return false;
      if (!needle) return true;
      const text = message.i18nKey ? t(message.i18nKey, message.i18nVars) : message.content;
      const actorId = speakerKey(message);
      const actor = actorDisplayTitle(actorId, t, toWorkerIdentity(actorId, deck.lanes[actorId]), workerSiblings);
      return `${text} ${message.toolOutput || ""} ${message.solverId || ""} ${actor} ${message.role} ${message.kind}`.toLocaleLowerCase().includes(needle);
    });
  }, [deck.chat, deck.lanes, query, shown, t, workerSiblings]);
  const items = useMemo(() => projectActivityLedger(visible), [visible]);
  // Drill-down edge case: operator arrived from Worker 详情 for a worker that
  // hasn't emitted any chat event yet (its speaker chip won't even render). Show
  // a targeted hint instead of the generic "adjust your filter" empty state.
  const silentFocusedWorker = useMemo(() => {
    if (shown.size !== 1 || query.trim()) return null;
    const only = [...shown][0];
    const hasEvents = deck.chat.some((message) => speakerKey(message) === only);
    return hasEvents ? null : only;
  }, [shown, query, deck.chat]);
  const labelFor = (message: ChatMessage) => {
    if (message.role === "human") return t("runtime.event.input");
    if (message.role === "system") return t("runtime.event.system");
    switch (message.kind) {
      case "tool": return t("runtime.event.tool");
      case "reasoning": return t("runtime.event.reasoning");
      case "insight": return t("runtime.event.insight");
      case "guidance": return t("runtime.event.guidance");
      case "flag": return t("runtime.event.result");
      case "text":
      case "status":
        return message.solverId && isWorkerLane(message.solverId) ? t("runtime.event.worker") : t("runtime.event.agent");
      default: {
        const exhaustive: never = message.kind;
        return exhaustive;
      }
    }
  };
  const rowHeight = compact ? 32 : 46;
  const expandedHeight = compact ? 142 : 176;
  const headerHeight = 28;
  const activeExpandedId = selectedEventId || expandedId;
  const groupOpen = (itemId: string) => expandedGroupId === itemId || items.some((item) => item.id === itemId && ledgerItemContainsId(item, activeExpandedId));
  const heights = useMemo(() => items.map((item) => ledgerItemHeight(item, {
    row: rowHeight,
    expanded: expandedHeight,
    groupOpen: item.type === "tools" && groupOpen(item.id),
    expandedMessageId: activeExpandedId,
  })), [activeExpandedId, expandedGroupId, expandedHeight, items, rowHeight]);
  const offsets = useMemo(() => {
    const next = [0];
    for (const height of heights) next.push(next[next.length - 1] + height);
    return next;
  }, [heights]);
  const totalRowsHeight = offsets[offsets.length - 1] || 0;
  const indexAtOffset = useCallback((offset: number) => {
    let lo = 0;
    let hi = items.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (offsets[mid + 1] <= offset) lo = mid + 1;
      else hi = mid;
    }
    return Math.min(items.length, Math.max(0, lo));
  }, [items.length, offsets]);
  const viewportStart = Math.max(0, viewport.top - headerHeight);
  const startIndex = Math.max(0, indexAtOffset(viewportStart) - 4);
  const endIndex = Math.min(items.length, indexAtOffset(viewportStart + viewport.height) + 6);
  const virtualRows = items.slice(startIndex, endIndex);

  const updateViewport = useCallback((node: HTMLDivElement) => {
    pendingViewportRef.current = { top: node.scrollTop, height: node.clientHeight };
    if (scrollFrameRef.current !== null) return;
    scrollFrameRef.current = window.requestAnimationFrame(() => {
      scrollFrameRef.current = null;
      if (!pendingViewportRef.current) return;
      setViewport(pendingViewportRef.current);
      pendingViewportRef.current = null;
    });
  }, []);

  useEffect(() => {
    const node = scrollerRef.current;
    if (!node) return;
    updateViewport(node);
    const observer = new ResizeObserver(() => updateViewport(node));
    observer.observe(node);
    return () => {
      observer.disconnect();
      if (scrollFrameRef.current !== null) window.cancelAnimationFrame(scrollFrameRef.current);
      scrollFrameRef.current = null;
      pendingViewportRef.current = null;
    };
  }, [updateViewport]);

  // Filter/density changes re-pin to the newest event: this is a live ledger,
  // so the meaningful default end is always the bottom (not the first row).
  useEffect(() => {
    const node = scrollerRef.current;
    if (!node) return;
    pinnedRef.current = true;
    setPinned(true);
    setUnread(0);
    prevLenRef.current = items.length;
    node.scrollTop = node.scrollHeight;
    updateViewport(node);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [compact, query, shown, updateViewport]);

  // Live tail: while pinned, keep the latest event in view as rows stream in;
  // while unpinned, just accumulate the unread count for the jump button.
  useEffect(() => {
    const prev = prevLenRef.current;
    prevLenRef.current = items.length;
    const node = scrollerRef.current;
    if (!node) return;
    if (pinnedRef.current) {
      node.scrollTop = node.scrollHeight;
      updateViewport(node);
      setUnread(0);
      return;
    }
    if (items.length > prev) setUnread((count) => count + (items.length - prev));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [items.length]);

  // Scroll tracking: leaving the bottom unpins; returning to the bottom (by
  // any means) re-pins. During a programmatic smooth jump, intermediate
  // positions are ignored until the bottom is reached.
  const handleLedgerScroll = useCallback((node: HTMLDivElement) => {
    updateViewport(node);
    const atBottom = node.scrollTop + node.clientHeight >= node.scrollHeight - 48;
    if (jumpingRef.current) {
      if (atBottom) {
        jumpingRef.current = false;
        pinnedRef.current = true;
        setPinned(true);
        setUnread(0);
      }
      return;
    }
    if (atBottom !== pinnedRef.current) {
      pinnedRef.current = atBottom;
      setPinned(atBottom);
      if (atBottom) setUnread(0);
    }
  }, [updateViewport]);

  const jumpToLatest = useCallback(() => {
    const node = scrollerRef.current;
    if (!node) return;
    jumpingRef.current = true;
    pinnedRef.current = true;
    node.scrollTo({ top: node.scrollHeight, behavior: "smooth" });
    updateViewport(node);
  }, [updateViewport]);

  useEffect(() => {
    if (!selectedEventId) return;
    const index = items.findIndex((item) => ledgerItemContainsId(item, selectedEventId));
    const node = scrollerRef.current;
    if (index < 0 || !node) return;
    const target = headerHeight + offsets[index];
    const nextTop = Math.max(0, Math.min(target - node.clientHeight * .35, headerHeight + totalRowsHeight - node.clientHeight));
    node.scrollTo({ top: nextTop, behavior: "smooth" });
  }, [items, offsets, selectedEventId, totalRowsHeight]);
  return (
    <div className="panel-scroll-wrap activity-panel">
      <div className="activity-toolbar" role="toolbar" aria-label={t("activity.title")}>
        <div className="activity-toolbar-title"><strong>{t("runtime.ledger.title")}</strong><span>{t("runtime.ledger.visible", { visible: items.length, total: deck.chat.length })}</span></div>
        <label className="activity-search"><Icon name="search" size={13} /><input type="search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder={t("runtime.ledger.search")} aria-label={t("runtime.ledger.search")} /></label>
        <button type="button" className={`density-toggle ${compact ? "on" : ""}`} onClick={() => setCompact((value) => { const next = !value; try { window.localStorage.setItem("muteki.activity.compact", next ? "1" : "0"); } catch { /* storage unavailable */ } return next; })} aria-pressed={compact}><Icon name="rows" size={14} /><span>{t("activity.densityLabel")}</span></button>
      </div>
      {speakers.length > 1 && <div className="runtime-speaker-filter" aria-label={t("activity.filter")}>
        <button type="button" className={!shown.size ? "on" : ""} onClick={() => setShown(new Set())}>{t("activity.filterAll")}</button>
        {speakers.map(([key]) => {
          const identity = toWorkerIdentity(key, deck.lanes[key]);
          const label = actorDisplayTitle(key, t, identity, workerSiblings);
          const worker = isWorkerLane(key);
          const color = worker ? workerColor(key, deck.lanes[key]?.engine) : undefined;
          const title = worker ? workerDisplayName(key, identity, workerSiblings).titleAttr : label;
          return <button type="button" key={key} className={!shown.size || shown.has(key) ? "on" : ""} style={color ? ({ "--wc": color } as CSSProperties) : undefined} title={title} onClick={() => setShown((current) => { const next = new Set(current); next.has(key) ? next.delete(key) : next.add(key); return next; })}>{worker && <i className="spk-dot" />}<span>{label}</span></button>;
        })}
      </div>}
      <div ref={scrollerRef} className="panel-scroll runtime-ledger-scroll" onScroll={(event) => handleLedgerScroll(event.currentTarget)}>
        {!deck.chat.length ? <PanelEmpty icon="list" title={t("activity.empty")} hint={t("activity.emptyHint")} /> : !visible.length ? (silentFocusedWorker ? <PanelEmpty icon="clock" title={t("activity.workerSilent")} hint={t("activity.workerSilentHint")} /> : <PanelEmpty icon="search" title={t("activity.filterEmpty")} hint={t("activity.filterEmptyHint")} />) :
          <div className={`activity-feed trace-ledger virtualized ${compact ? "compact" : ""}`} style={{ height: headerHeight + totalRowsHeight }}>
            <div className="trace-ledger-head" aria-hidden="true"><span>#</span><span>{t("runtime.ledger.event")}</span><span>{t("runtime.ledger.content")}</span><span>{t("runtime.ledger.time")}</span></div>
            {virtualRows.map((item, virtualIndex) => {
              const index = startIndex + virtualIndex;
              const top = headerHeight + offsets[index];
              const height = heights[index];
              if (item.type === "tools") {
                const solverId = item.solverId;
                const isWorker = !!solverId && isWorkerLane(solverId);
                const color = isWorker ? workerColor(solverId, deck.lanes[solverId]?.engine) : undefined;
                const actorTitle = actorDisplayTitle(solverId, t, toWorkerIdentity(solverId, deck.lanes[solverId]), workerSiblings);
                const open = groupOpen(item.id);
                const failed = toolGroupFailedCommand(item.messages);
                const latest = toolGroupLatestCommand(item.messages);
                const pending = item.messages.some((message) => message.toolPending);
                const summary = failed
                  ? `${t("runtime.ledger.toolGroup", { n: item.messages.length })} · ${t("runtime.ledger.toolFailed", { cmd: failed })}`
                  : latest
                    ? t("runtime.ledger.toolGroupLatest", { n: item.messages.length, cmd: latest })
                    : t("runtime.ledger.toolGroup", { n: item.messages.length });
                const selected = item.messages.some((message) => message.id === selectedEventId);
                return (
                  <div
                    key={item.id}
                    className={`act-msg tool-group ${open ? "expanded" : ""} ${selected ? "selected" : ""} ${failed ? "tool-failed" : ""}`}
                    style={{ top, height, ...(color ? { "--wc": color } : {}) } as CSSProperties}
                  >
                    <button
                      type="button"
                      className="act-group-head"
                      aria-expanded={open}
                      aria-posinset={index + 1}
                      aria-setsize={items.length}
                      onClick={() => setExpandedGroupId((current) => current === item.id ? null : item.id)}
                      title={new Date(runtimeTsMs(item.ts)).toLocaleString(lang)}
                    >
                      <span className="act-index">{String(index + 1).padStart(2, "0")}</span>
                      <span className="act-kind kind-tool"><Icon name="terminal" size={12} />{t("runtime.event.tools")}</span>
                      <div className="act-main">
                        <div className="act-who">
                          <span className="act-actor">{actorTitle}</span>
                          <span className="k">{isWorker ? workerEngine(solverId, deck.lanes[solverId]?.engine) : "tool"}</span>
                          {pending && <span className="k">{t("runtime.ledger.toolPending")}</span>}
                        </div>
                        <div className="act-body">{summary}</div>
                      </div>
                      <time className="act-time">{runtimeClock(item.ts)}</time>
                      <Icon className="act-expand" name="chevronDown" size={12} />
                    </button>
                    {open && (
                      <div className="act-tool-list">
                        {item.messages.map((message) => {
                          const childExpanded = activeExpandedId === message.id;
                          const command = toolCommandLabel(message);
                          const body = childExpanded && message.toolOutput ? message.toolOutput : command;
                          return (
                            <button
                              type="button"
                              key={message.id}
                              className={`act-tool-child ${childExpanded ? "expanded" : ""} ${message.toolFailed ? "tool-failed" : ""} ${selectedEventId === message.id ? "selected" : ""}`}
                              onClick={() => {
                                const next = selectedEventId === message.id ? null : message.id;
                                setExpandedId(next);
                                onSelectEvent(next);
                              }}
                            >
                              <span className="act-tool-cmd">{message.toolFailed ? `! ${command}` : command}</span>
                              {childExpanded && message.toolOutput && <span className="act-tool-out">{body}</span>}
                            </button>
                          );
                        })}
                      </div>
                    )}
                  </div>
                );
              }
              if (item.type !== "single") {
                const exhaustive: never = item;
                void exhaustive;
                return null;
              }
              const message = item.message;
              const text = message.i18nKey ? t(message.i18nKey, message.i18nVars) : message.content;
              const isWorker = !!message.solverId && isWorkerLane(message.solverId);
              const selected = selectedEventId === message.id;
              const expanded = expandedId === message.id || selected;
              const color = isWorker ? workerColor(message.solverId!) : undefined;
              const actorId = message.solverId || message.role;
              const actorTitle = actorDisplayTitle(actorId, t, toWorkerIdentity(actorId, deck.lanes[actorId]), workerSiblings);
              return (
                <button
                  type="button"
                  key={item.id}
                  className={`act-msg ${message.role} ${message.kind} ${expanded ? "expanded" : ""} ${selected ? "selected" : ""}`}
                  aria-expanded={expanded}
                  aria-current={selected ? "true" : undefined}
                  aria-posinset={index + 1}
                  aria-setsize={items.length}
                  onClick={() => {
                    const next = selected ? null : message.id;
                    setExpandedId(next);
                    onSelectEvent(next);
                  }}
                  style={{ top, height, ...(color ? { "--wc": color } : {}) } as CSSProperties}
                  title={new Date(runtimeTsMs(message.ts)).toLocaleString(lang)}
                >
                  <span className="act-index">{String(index + 1).padStart(2, "0")}</span>
                  <span className={`act-kind kind-${message.kind}`}>{message.kind === "tool" ? <Icon name="terminal" size={12} /> : message.kind === "reasoning" ? <Icon name="radio" size={12} /> : message.kind === "flag" ? <Icon name="flag" size={12} /> : <Icon name="dot" size={9} />}{labelFor(message)}</span>
                  <div className="act-main"><div className="act-who"><span className="act-actor">{actorTitle}</span><span className="k">{isWorker ? workerEngine(message.solverId!, deck.lanes[message.solverId!]?.engine) : message.kind}</span></div><div className="act-body">{text}</div></div>
                  <time className="act-time">{runtimeClock(message.ts)}</time><Icon className="act-expand" name="chevronDown" size={12} />
                </button>
              );
            })}
          </div>}
      </div>
      {!pinned && items.length > 0 && <button type="button" className="activity-jump-latest" onClick={jumpToLatest} aria-label={t("runtime.ledger.jumpLatest")}>
        <Icon name="chevronDown" size={13} /><span>{t("runtime.ledger.jumpLatest")}</span>{unread > 0 && <b>{unread > 99 ? "99+" : unread}</b>}
      </button>}
    </div>
  );
}

function RuntimeWorkerView({ deck, running, focusWorker, onSpawnWorker, onKillWorker, onOpenSpeakerTimeline }: {
  deck: DeckState; running: boolean; focusWorker?: { id: string; nonce: number } | null;
  onSpawnWorker: (engine?: string) => void; onKillWorker: (id: string) => void;
  onOpenSpeakerTimeline?: (id: string) => void;
}) {
  const ids = workerIds(deck);
  const lanes = ids.map((id) => deck.lanes[id]).filter(Boolean);
  const calls = lanes.reduce((sum, lane) => sum + lane.toolLines.length, 0);
  const digest = swarmDigest(deck);
  const elapsed = useRuntimeElapsed(digest.startedAt, digest.finishedAt);
  return (
    <WorkerLanes
      deck={deck}
      running={running}
      focusWorker={focusWorker}
      onSpawnWorker={onSpawnWorker}
      onKillWorker={onKillWorker}
      onOpenSpeakerTimeline={onOpenSpeakerTimeline}
      phase={digest.phase}
      elapsed={elapsed}
      calls={calls}
    />
  );
}

function RuntimeList({ children }: { children: ReactNode }) { return <div className="artifact-list">{children}</div>; }
function RuntimeEmpty() { const t = useRuntimeT(); return <div className="artifact-list-empty">{t("panel.empty")}</div>; }
type CredRow = { entity: string; value: string; seq?: number };
type CredFetch = { status: "loading" | "ready" | "error"; rows: CredRow[]; message?: string; retry: () => void };

function useRunCredentials(runId: string, factGen: number): CredFetch {
  const t = useRuntimeT();
  const [state, setState] = useState<Omit<CredFetch, "retry">>({ status: "loading", rows: [] });
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let alive = true;
    setState((prev) => ({ ...prev, status: prev.rows.length ? "ready" : "loading", message: undefined }));
    apiFetch(`/api/runs/${encodeURIComponent(runId)}/credentials`)
      .then(async (response) => {
        if (!response.ok) {
          const message = response.status === 409
            ? t("runtime.credentials.unavailable")
            : response.status === 404
              ? t("runtime.credentials.missing")
              : `${t("runtime.credentials.error")} (${response.status})`;
          throw Object.assign(new Error(message), { shown: true });
        }
        return response.json();
      })
      .then((data) => {
        if (!alive) return;
        setState({ status: "ready", rows: Array.isArray(data.credentials) ? data.credentials : [] });
      })
      .catch((err) => {
        if (!alive) return;
        setState({
          status: "error",
          rows: [],
          message: err?.shown ? err.message : t("runtime.credentials.error"),
        });
      });
    return () => { alive = false; };
  }, [runId, factGen, tick, t]);
  return { ...state, retry: () => setTick((n) => n + 1) };
}

function RuntimeCredentials({ creds, onOpenFact }: { creds: CredFetch; onOpenFact?: (seq: number) => void }) {
  const t = useRuntimeT();
  const [revealed, setRevealed] = useState<Record<string, boolean>>({});
  const [copied, setCopied] = useState<string>("");
  if (creds.status === "loading" && !creds.rows.length) return <div className="panel-scroll"><PanelSkeleton rows={3} /></div>;
  if (creds.status === "error") {
    return <div className="artifact-list-empty">
      <div>{creds.message || t("runtime.credentials.error")}</div>
      <button type="button" className="evi-filter-btn" onClick={creds.retry}>{t("runtime.credentials.retry")}</button>
    </div>;
  }
  if (!creds.rows.length) return <div className="artifact-list-empty">{t("runtime.credentials.empty")}</div>;
  return <RuntimeList>{creds.rows.map((row) => {
    const key = `${row.entity}-${row.seq ?? row.value}`;
    const open = !!revealed[key];
    return <div className="artifact-row" key={key}>
      <div className="artifact-row-top">
        <span className="artifact-badge ok">cred</span>
        <span className="artifact-row-title">{row.entity}</span>
        {typeof row.seq === "number" && row.seq > 0 && (
          <button type="button" className="artifact-chip report-link-btn" onClick={() => onOpenFact?.(row.seq!)}>#{row.seq}</button>
        )}
      </div>
      <code className="artifact-code">{open ? row.value : "••••••••"}</code>
      <div className="report-links">
        <button type="button" className="report-link-btn" onClick={() => setRevealed((prev) => ({ ...prev, [key]: !open }))}>
          {open ? t("runtime.credentials.hide") : t("runtime.credentials.show")}
        </button>
        <button type="button" className="report-link-btn" onClick={() => { void navigator.clipboard.writeText(row.value); setCopied(key); }}>
          {copied === key ? t("common.copied") : t("runtime.credentials.copy")}
        </button>
      </div>
    </div>;
  })}</RuntimeList>;
}

function reviewSeverityRank(severity: string): number {
  if (severity === "blocker") return 0;
  if (severity === "warn" || severity === "high") return 1;
  if (severity === "info") return 3;
  return 2;
}

function reviewSeverityLabel(severity: string, t: (key: string) => string): string {
  if (severity === "blocker") return t("runtime.findings.sev.blocker");
  if (severity === "warn") return t("runtime.findings.sev.warn");
  if (severity === "info") return t("runtime.findings.sev.info");
  return severity;
}

function reviewKindLabel(kind: string, t: (key: string) => string): string {
  const key = `runtime.findings.kind.${kind}`;
  const label = t(key);
  return label === key ? kind : label;
}

function pocStatusLabel(status: string, t: (key: string) => string): string {
  const key = `runtime.pocs.status.${status}`;
  const label = t(key);
  return label === key ? status : label;
}

function directiveStatusLabel(status: string, t: (key: string) => string): string {
  const mapped = status === "applied" ? "acted" : status;
  const key = `directive.${mapped}`;
  const label = t(key);
  return label === key ? status : label;
}

function directiveRowKey(row: { actor: string; ts: number; action: string; directive: string }): string {
  const raw = `${row.actor}\0${row.ts}\0${row.action}\0${row.directive}`;
  let hash = 0;
  for (let i = 0; i < raw.length; i += 1) hash = (hash * 31 + raw.charCodeAt(i)) | 0;
  return `dir-${row.ts}-${(hash >>> 0).toString(36)}`;
}

function RuntimeDataView({ view, deck, focusReport, focusPoc, credentials, onOpenFact, onOpenPoc, onAdoptReview }: {
  view: ArtifactView;
  deck: DeckState;
  focusReport?: { id: string; nonce: number } | null;
  focusPoc?: { id: string; nonce: number } | null;
  credentials?: CredRow[];
  onOpenFact?: (factSeq: number) => void;
  onOpenPoc?: (pocId: string) => void;
  onAdoptReview?: (text: string) => void;
}) {
  const t = useRuntimeT();
  useEffect(() => {
    if (!focusPoc?.id) return;
    document.getElementById(`poc-${focusPoc.id}`)?.scrollIntoView({ block: "nearest" });
  }, [focusPoc]);
  if (view === "reports") {
    return (
      <VulnReportsList
        rows={deck.blackboard.vulnReports ?? []}
        empty={<RuntimeEmpty />}
        clock={runtimeClock}
        collectionTitle={deck.challengeName ? `${deck.challengeName} 漏洞报告集` : "漏洞报告集"}
        focusReport={focusReport}
        facts={deck.blackboard.facts}
        pocs={deck.blackboard.pocs}
        reviews={deck.blackboard.reviewFindings}
        credentials={credentials}
        truncated={!!deck.blackboard.truncated?.reports}
        onOpenFact={onOpenFact}
        onOpenPoc={onOpenPoc}
      />
    );
  }
  if (view === "findings") {
    const reviewed = [...(deck.blackboard.reviewFindings ?? [])]
      .sort((a, b) => reviewSeverityRank(a.severity) - reviewSeverityRank(b.severity) || b.ts - a.ts);
    if (!reviewed.length) return <div className="artifact-list-empty">{t("runtime.findings.empty")}</div>;
    const groups = new Map<string, typeof reviewed>();
    for (const row of reviewed) {
      const key = row.routeHash || "";
      const list = groups.get(key) ?? [];
      list.push(row);
      groups.set(key, list);
    }
    return <div className="panel-scroll-wrap">
      {deck.blackboard.truncated?.reviews && <div className="evi-density-note">{t("runtime.truncated", { n: 80 })}</div>}
      <RuntimeList>
        {[...groups.entries()].map(([route, items]) => (
          <div className="evi-group" key={route || "unrouted"}>
            <div className="evi-group-h">{route ? route : t("runtime.findings.unrouted")}</div>
            {items.map((row) => {
              const action = row.recommendedActions?.[0] || row.summary;
              return <div className="artifact-row" key={row.id}>
                <div className="artifact-row-top">
                  <span className={`artifact-badge sev-${row.severity === "blocker" ? "critical" : row.severity === "warn" ? "warn" : "low"}`}>
                    {t("runtime.findings.review")} · {reviewSeverityLabel(row.severity, t)}
                  </span>
                  <span className="artifact-row-title">{reviewKindLabel(row.kind, t)}</span>
                </div>
                <div className="artifact-row-body">{row.summary}</div>
                <div className="artifact-row-meta">{row.actor}</div>
                {action && <div className="report-links">
                  <button type="button" className="report-link-btn" onClick={() => onAdoptReview?.(action)}>{t("runtime.findings.adopt")}</button>
                </div>}
              </div>;
            })}
          </div>
        ))}
      </RuntimeList>
    </div>;
  }
  if (view === "pocs") {
    const rows = deck.blackboard.pocs ?? []; if (!rows.length) return <RuntimeEmpty />;
    return <RuntimeList>
      {deck.blackboard.truncated?.pocs && <div className="evi-density-note">{t("runtime.truncated", { n: 80 })}</div>}
      {[...rows].reverse().map((row) => <div className={`artifact-row ${focusPoc?.id === row.id ? "report-focused" : ""}`} key={row.id} id={`poc-${row.id}`}>
        <div className="artifact-row-top"><span className="artifact-badge">{pocStatusLabel(row.status, t)}</span><span className="artifact-row-title">{row.name || row.id}</span></div>
        {row.entryCommand && <code className="artifact-code">{row.entryCommand}</code>}
        {row.note && <div className="artifact-row-body">{row.note}</div>}
        <div className="artifact-row-meta">{[row.worker, row.intentId, row.path].filter(Boolean).join(" · ")}</div>
      </div>)}
    </RuntimeList>;
  }
  if (view === "routes") {
    const routes = deck.blackboard.suppressedRoutes ?? [];
    const branches = deck.blackboard.branches ?? [];
    if (!routes.length && !branches.length) return <RuntimeEmpty />;
    const suppressed = routes.filter((row) => !row.reopened);
    const reopened = routes.filter((row) => row.reopened);
    return <div className="panel-scroll-wrap">
      {deck.blackboard.truncated?.routes && <div className="evi-density-note">{t("runtime.truncated", { n: 60 })}</div>}
      <RuntimeList>
        {suppressed.length > 0 && <div className="evi-group"><div className="evi-group-h">{t("runtime.routes.group.suppressed")}</div>
          {[...suppressed].reverse().map((row) => <div className="artifact-row" key={row.routeHash}><div className="artifact-row-top"><span className="artifact-badge bad">{t("panel.suppressed")}</span><span className="artifact-row-title">{row.label || row.routeHash}</span></div><div className="artifact-row-body">{row.reason}</div><div className="artifact-row-meta">{row.routeHash}</div></div>)}
        </div>}
        {reopened.length > 0 && <div className="evi-group"><div className="evi-group-h">{t("runtime.routes.group.reopened")}</div>
          {[...reopened].reverse().map((row) => <div className="artifact-row" key={row.routeHash}><div className="artifact-row-top"><span className="artifact-badge ok">{t("panel.reopened")}</span><span className="artifact-row-title">{row.label || row.routeHash}</span></div><div className="artifact-row-body">{row.reason}</div><div className="artifact-row-meta">{row.routeHash}</div></div>)}
        </div>}
        {branches.length > 0 && <div className="evi-group"><div className="evi-group-h">{t("runtime.routes.group.branches")}</div>
          {[...branches].reverse().map((row) => <div className="artifact-row" key={row.branchId}><div className="artifact-row-top"><span className={`artifact-badge ${row.status === "resolved" ? "ok" : ""}`}>{row.status === "resolved" ? t("runtime.routes.status.resolved") : t("runtime.routes.status.open")}</span><span className="artifact-row-title">{row.title || row.branchId}</span></div><div className="artifact-row-meta">{row.branchId} · {row.actor}</div></div>)}
        </div>}
      </RuntimeList>
    </div>;
  }
  const rows = deck.blackboard.directives ?? [];
  const lifecycle = deck.operatorDirectives ?? [];
  if (!rows.length && !lifecycle.length) return <RuntimeEmpty />;
  return <div className="panel-scroll-wrap">
    {deck.blackboard.truncated?.directives && <div className="evi-density-note">{t("runtime.truncated", { n: 60 })}</div>}
    <RuntimeList>
      {lifecycle.length > 0 && <div className="evi-group"><div className="evi-group-h">{t("runtime.directives.operator")}</div>
        {[...lifecycle].reverse().map((row) => <div className="artifact-row" key={row.id}><div className="artifact-row-top"><span className="artifact-badge">{directiveStatusLabel(row.status, t)}</span><span className="artifact-row-title">{row.action}</span></div><div className="artifact-row-body">{row.text}</div>{row.boundWorker && <div className="artifact-row-meta">{row.boundWorker}</div>}</div>)}
      </div>}
      {rows.length > 0 && <div className="evi-group"><div className="evi-group-h">{t("runtime.directives.coordinator")}</div>
        {[...rows].reverse().map((row) => <div className="artifact-row" key={directiveRowKey(row)}><div className="artifact-row-top"><span className="artifact-badge">{row.action}</span><span className="artifact-row-title">{row.actor}</span></div><div className="artifact-row-body">{row.directive}</div></div>)}
      </div>}
    </RuntimeList>
  </div>;
}

function RuntimeArtifactPanel({ open, width, view, deck, running, loading, selected, onSelect, onView, onClose, onResize, minWidth, maxWidth, defaultWidth, onSpawnWorker, onKillWorker, focusWorker, focusReport, focusPoc, focusFact, focusSpeaker, onOpenSpeakerTimeline, onOpenFact, onOpenPoc, onAdoptReview, workspaceMode = false }: {
  open: boolean; width: number; view: ArtifactView; deck: DeckState; running: boolean; loading: boolean; selected: GraphNode | null;
  onSelect: (node: GraphNode | null) => void; onView: (view: ArtifactView) => void; onClose: () => void; onResize: (width: number) => void;
  minWidth: number; maxWidth: number; defaultWidth: number; onSpawnWorker: (engine?: string) => void; onKillWorker: (id: string) => void; focusWorker?: { id: string; nonce: number } | null; focusReport?: { id: string; nonce: number } | null; focusPoc?: { id: string; nonce: number } | null; focusFact?: { seq: number; nonce: number } | null; focusSpeaker?: { id: string; nonce: number } | null; onOpenSpeakerTimeline?: (id: string) => void; onOpenFact?: (seq: number) => void; onOpenPoc?: (pocId: string) => void; onAdoptReview?: (text: string) => void; workspaceMode?: boolean;
}) {
  const t = useRuntimeT();
  const factGen = Math.max(0, ...deck.blackboard.facts.filter((fact) => fact.verified && typeof fact.factSeq === "number").map((fact) => fact.factSeq as number));
  const creds = useRunCredentials(deck.runId, factGen);
  const workspaceActive = workspaceMode && open;
  const [resizing, setResizing] = useState(false);
  const [selectedRuntimeEventId, setSelectedRuntimeEventId] = useState<string | null>(null);
  const currentTab = RUNTIME_TABS.find((tab) => tab.view === view) ?? RUNTIME_TABS[0];
  const currentGroup = RUNTIME_GROUPS.find((group) => group.id === currentTab.group) ?? RUNTIME_GROUPS[0];
  const groupTabs = RUNTIME_TABS.filter((tab) => tab.group === currentGroup.id);
  const startResize = (event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault(); setResizing(true); document.body.classList.add("artifact-resizing");
    const move = (pointer: PointerEvent) => onResize(window.innerWidth - pointer.clientX);
    const stop = () => { setResizing(false); document.body.classList.remove("artifact-resizing"); window.removeEventListener("pointermove", move); window.removeEventListener("pointerup", stop); };
    window.addEventListener("pointermove", move); window.addEventListener("pointerup", stop); onResize(window.innerWidth - event.clientX);
  };
  const resizeKey = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.key === "ArrowLeft") { event.preventDefault(); onResize(width + (event.shiftKey ? 32 : 12)); }
    if (event.key === "ArrowRight") { event.preventDefault(); onResize(width - (event.shiftKey ? 32 : 12)); }
    if (event.key === "Home") { event.preventDefault(); onResize(minWidth); }
    if (event.key === "End") { event.preventDefault(); onResize(maxWidth); }
    if (event.key === "Enter") { event.preventDefault(); onResize(defaultWidth); }
  };
  return <div className={`artifact motion-artifact ${workspaceActive ? "workspace-mode" : ""} ${resizing ? "resizing" : ""}`} style={workspaceActive ? undefined : { width: open ? width : 0, flexBasis: open ? width : 0 }} role="region" aria-label={t("a11y.artifact")}>
    {open && <>
      {!workspaceActive && <div className="artifact-resizer" role="separator" tabIndex={0} aria-label={t("art.resizeCanvas")} aria-valuemin={minWidth} aria-valuemax={maxWidth} aria-valuenow={width} onPointerDown={startResize} onKeyDown={resizeKey} onDoubleClick={() => onResize(defaultWidth)} />}
      {!workspaceActive && <div className="artifact-head runtime-head"><div className="runtime-titlemark"><Icon name={currentTab.icon} size={16} /></div><div className="runtime-heading"><span className="runtime-eyebrow">{t("runtime.title")}</span><div className="runtime-titleline"><strong>{t(currentTab.key)}</strong><span className={`runtime-state ${running ? "live" : deck.finished ? "complete" : "standby"}`}><span className="runtime-state-dot" />{t(running ? "runtime.status.live" : deck.finished ? "runtime.status.complete" : "runtime.status.standby")}</span></div><span className="runtime-context">{deck.challengeName || t("runtime.untitled")} · {deck.runId}</span></div><span className="spacer" /><button className="x" onClick={onClose} aria-label={t("art.closeCanvas")}><Icon name="x" size={15} /></button></div>}
      <div className="runtime-navigation">
        <div className="runtime-primary-nav selection-glide-host" role="tablist">
          <SelectionGlider selectedKey={currentGroup.id} selector='button[aria-selected="true"]' className="compact" duration={260} />
          {RUNTIME_GROUPS.map((group) => <button key={group.id} type="button" role="tab" aria-selected={group.id === currentGroup.id} className={group.id === currentGroup.id ? "on" : ""} onClick={() => onView(RUNTIME_TABS.find((tab) => tab.group === group.id)?.view ?? "timeline")}><Icon name={group.icon} size={14} /><span>{t(group.key)}</span></button>)}
        </div>
        <span className="runtime-nav-divider" aria-hidden="true" />
        <div className="runtime-view-nav selection-glide-host" role="tablist">
          <SelectionGlider selectedKey={`${currentGroup.id}:${view}`} selector='button[aria-selected="true"]' className="compact" ensureVisible duration={260} />
          {groupTabs.map((tab) => { const count = runtimeTabCount(tab.view, deck); return <button key={tab.view} type="button" role="tab" aria-selected={tab.view === view} className={tab.view === view ? "on" : ""} onClick={() => onView(tab.view)}><Icon name={tab.icon} size={13} /><span>{t(tab.key)}</span>{count !== null && <b>{count}</b>}</button>; })}
        </div>
      </div>
      {(() => {
        const body = <div className="artifact-body"><div className="artifact-view motion-artifact-view" key={loading ? "loading" : view}>
          {loading ? <div className="panel-scroll"><PanelSkeleton rows={4} /></div>
            : view === "graph" ? <><GraphView model={deck.model} onSelect={onSelect} lanes={deck.lanes} />{selected && <div className="insp-float"><NodeInspector node={selected} onClose={() => onSelect(null)} /></div>}</>
            : view === "blackboard" ? <Blackboard bb={deck.blackboard} runId={deck.runId} lanes={deck.lanes} />
            : view === "workers" ? <RuntimeWorkerView deck={deck} running={running} focusWorker={focusWorker} onSpawnWorker={onSpawnWorker} onKillWorker={onKillWorker} onOpenSpeakerTimeline={onOpenSpeakerTimeline} />
            : view === "timeline" ? <RuntimeActivityStream deck={deck} selectedEventId={selectedRuntimeEventId} onSelectEvent={setSelectedRuntimeEventId} focusSpeaker={focusSpeaker} />
            : view === "evidence" ? <EvidenceChain deck={deck} focusFactSeq={focusFact?.seq} focusNonce={focusFact?.nonce} />
            : view === "credentials" ? <RuntimeCredentials creds={creds} onOpenFact={onOpenFact} />
            : <RuntimeDataView view={view} deck={deck} focusReport={focusReport} focusPoc={focusPoc} credentials={creds.rows} onOpenFact={onOpenFact} onOpenPoc={onOpenPoc} onAdoptReview={onAdoptReview} />}
        </div></div>;
        // The time-lane graph only belongs on the activity timeline. Worker
        // detail and the fact graph need the height for their own content.
        if (currentGroup.id !== "observe" || view !== "timeline") return body;
        return <div className="runtime-console">
          <RuntimeTraceOverview deck={deck} running={running} selectedId={selectedRuntimeEventId} onSelect={setSelectedRuntimeEventId} />
          {body}
        </div>;
      })()}
    </>}
  </div>;
}

export default function Page() {
  return (
    <I18nProvider>
      <LoginGate>
        <Deck />
      </LoginGate>
    </I18nProvider>
  );
}

function Deck() {
  // Start every page load on a FRESH draft conversation (ChatGPT-style: open →
  // empty new chat, history lives in the rail). We must NOT bind to a shared
  // fixed id like "deck-run" — that reopens (and replays) one ever-growing log,
  // which is exactly the "new solve still shows old chat" bug.
  //
  // The draft id is LOCAL until the operator actually dispatches: a draft never
  // touches the backend, so opening the deck (or hitting "+ New solve") never
  // mints empty run-NNNN stubs that clutter the rail. dispatch() promotes the
  // draft to a real backend run id at send time.
  const t = useT();
  // Init to "" (deterministic across the static-export prerender AND client
  // hydration), then mint the real random draft id in a mount-only effect.
  // newDraftId() uses Date.now()+Math.random(), so calling it during useState
  // initialization runs it twice — once at build prerender, once at hydration —
  // producing different ids and a React hydration "text content does not match"
  // error. Deferring to a post-mount effect keeps the first render identical.
  const [runId, setRunId] = useState("");
  useEffect(() => {
    // seed from the URL (/run/<id> deep-link / refresh); else a fresh draft.
    setRunId((cur) => cur || runIdFromPath() || newDraftId());
    // back/forward navigation between runs → re-read the path.
    const onPop = () => setRunId(runIdFromPath() || newDraftId());
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  // keep the URL in sync with the active run (replace, not push — internal run
  // switches shouldn't pile up history entries; deep-link/refresh still work).
  useEffect(() => {
    if (!runId) return;
    const url = urlForRun(runId);
    if (window.location.pathname !== url) window.history.replaceState({}, "", url);
  }, [runId]);
  const { deck, connected, start, sendHitl, resolve } = useRun(runId);

  const [railCollapsed, setRailCollapsed] = useState(false);
  const [railWidth, setRailWidth] = useState(RAIL_WIDTH_DEFAULT);
  const [railWidthReady, setRailWidthReady] = useState(false);
  const [theme, setTheme] = useState<ThemeMode>("dark");
  // Worker configuration now lives on a dedicated route. Keep this boolean as
  // the shared keyboard-layer guard; the old modal no longer mounts here.
  const showSettings = false;
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [btwOpen, setBtwOpen] = useState(false);
  const [artifactOpen, setArtifactOpen] = useState(false);
  // Default runtime view is the activity stream (live event ledger); the last
  // picked view is remembered per browser so a reopen restores the operator's
  // working context instead of forcing the fact graph.
  const [artifactView, setArtifactViewState] = useState<ArtifactView>(() => {
    if (typeof window === "undefined") return "timeline";
    try {
      const saved = window.localStorage.getItem("muteki.runtimeView");
      if (saved && RUNTIME_TABS.some((tab) => tab.view === saved)) return saved as ArtifactView;
    } catch { /* storage unavailable */ }
    return "timeline";
  });
  const setArtifactView = useCallback((view: ArtifactView) => {
    setArtifactViewState(view);
    try { window.localStorage.setItem("muteki.runtimeView", view); } catch { /* storage unavailable */ }
  }, []);
  const [artifactWidth, setArtifactWidth] = useState(() => artifactWidthDefault(typeof window !== "undefined" ? window.innerWidth : 1280));
  const [artifactWidthReady, setArtifactWidthReady] = useState(false);
  const [selected, setSelected] = useState<GraphNode | null>(null);
  // Roster-row → Worker 详情 focus seed. The nonce bumps on every click so
  // re-clicking the same worker re-focuses the lanes panel; WorkerLanes reacts
  // only to a new nonce, leaving the operator's manual chip filtering intact.
  const [focusedWorker, setFocusedWorker] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedSpeaker, setFocusedSpeaker] = useState<{ id: string; nonce: number } | null>(null);
  // Inspector report-row → 运行时漏洞报告集, same nonce pattern so a second
  // click on the same finding still expands and scrolls to it.
  const [focusedReport, setFocusedReport] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedPoc, setFocusedPoc] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedFact, setFocusedFact] = useState<{ seq: number; nonce: number } | null>(null);
  const [winW, setWinW] = useState(typeof window !== "undefined" ? window.innerWidth : 1280);
  const [listBump, setListBump] = useState(0);
  // Unified toast/action-feedback lane: every operator mutation confirms here
  // (or reports failure). pushToast(...) is threaded into the action handlers.
  const { toasts, push: pushToast, dismiss: dismissToast } = useToasts();
  const shellRef = useRef<HTMLDivElement | null>(null);
  const openWorkerSettings = useCallback(() => {
    const returnTo = `${window.location.pathname}${window.location.search}`;
    window.location.assign(`/settings/workers?return=${encodeURIComponent(returnTo)}`);
  }, []);
  // Files attached to the NEXT dispatch (file-based tracks). Saved server-side
  // the moment they're attached; we hold the returned absolute paths here so
  // dispatch() can put them on challenge.attachments. Lives at this level (not
  // in the Composer) so it survives into dispatch and resets on run switch.
  const [attachments, setAttachments] = useState<SavedFile[]>([]);

  const runs = useRunList(4000, listBump);
  const folders = useFolders(8000, listBump);
  const bump = () => setListBump((n) => n + 1);

  // shorthand for a failure toast — shown whenever a mutation helper returns
  // null/false (network / backend error), the biggest silent-failure gap today.
  const toastFail = () => pushToast({ msg: t("toast.actionFailed"), variant: "error" });

  useEffect(() => {
    try {
      const saved = window.localStorage.getItem("muteki.theme");
      if (saved === "dark" || saved === "light") {
        setTheme(saved);
        return;
      }
      if (window.matchMedia?.("(prefers-color-scheme: dark)").matches) setTheme("dark");
    } catch {
      // keep the default dark theme when storage/media is unavailable
    }
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    try {
      window.localStorage.setItem("muteki.theme", theme);
    } catch {
      // theming should still work for this session
    }
  }, [theme]);

  // Color scheme: the picker lives on the settings page; here we only
  // re-apply the saved selection whenever the light/dark mode flips, so the
  // palette is regenerated with the new mode's lightness/chroma curves.
  useEffect(() => {
    applySelection(readSavedSelection(), theme);
  }, [theme]);

  const toggleTheme = () => setTheme((cur) => (cur === "dark" ? "light" : "dark"));

  // Operator command → swarm. Wraps sendHitl so the otherwise-silent "生成复盘"
  // (writeup) command gives immediate feedback: the coordinator takes seconds to
  // produce the report and it lands as a normal chat bubble, so without this the
  // button looked dead. Every other command path is untouched (pass-through).
  const onCommand = useCallback(
    async (target: string, action: string, text: string,
           opts?: ControlCommandOpts): Promise<boolean> => {
      try {
        await sendHitl(target, action, text, opts);
        if (action === "writeup") {
          pushToast({ msg: t("toast.writeupRequested"), variant: "info", icon: "pencil" });
        }
        return true;
      } catch {
        pushToast({ msg: t("toast.actionFailed"), variant: "error" });
        return false;
      }
    },
    [sendHitl, pushToast, t]
  );

  useEffect(() => {
    const onR = () => setWinW(window.innerWidth);
    window.addEventListener("resize", onR);
    return () => window.removeEventListener("resize", onR);
  }, []);

  useEffect(() => {
    try {
      const raw = window.localStorage.getItem(ARTIFACT_WIDTH_STORAGE_KEY);
      const parsed = raw ? Number(raw) : NaN;
      setArtifactWidth(clampArtifactWidth(Number.isFinite(parsed) ? parsed : artifactWidthDefault(window.innerWidth), window.innerWidth));
    } catch {
      setArtifactWidth(artifactWidthDefault(typeof window !== "undefined" ? window.innerWidth : 1280));
    } finally {
      setArtifactWidthReady(true);
    }
  }, []);

  useEffect(() => {
    if (!artifactWidthReady) return;
    try {
      window.localStorage.setItem(ARTIFACT_WIDTH_STORAGE_KEY, String(artifactWidth));
    } catch {
      // localStorage may be blocked; resizing should still work for this session.
    }
  }, [artifactWidth, artifactWidthReady]);

  useEffect(() => {
    try {
      const raw = window.localStorage.getItem(RAIL_WIDTH_STORAGE_KEY);
      const parsed = raw ? Number(raw) : NaN;
      if (Number.isFinite(parsed)) setRailWidth(clampRailWidth(parsed, window.innerWidth));
    } catch {
      // localStorage may be blocked; keep the default width.
    } finally {
      setRailWidthReady(true);
    }
  }, []);

  useEffect(() => {
    if (!railWidthReady) return;
    try {
      window.localStorage.setItem(RAIL_WIDTH_STORAGE_KEY, String(railWidth));
    } catch {
      // localStorage may be blocked; resizing should still work for this session.
    }
  }, [railWidth, railWidthReady]);

  const onRailResize = useCallback((width: number) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    setRailWidth(clampRailWidth(width, viewport));
  }, []);
  const onArtifactResize = useCallback((width: number) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    setArtifactWidth(clampArtifactWidth(width, viewport));
  }, []);

  // Cmd/Ctrl+K opens (toggles) the command palette — the single, more-powerful
  // power-user entry point. This OWNS Cmd+K now: the old composer-focus path in
  // Conversation.tsx was moved off Cmd+K (it kept the bare "/" key, and the
  // palette also exposes a "focus composer" command), so the two never
  // double-fire. We preventDefault to swallow the browser default. The settings
  // modal sits above the palette; while it's up we don't toggle (Esc there owns
  // dismissal). The panel single-key shortcuts (e/w/g/t/b) already bail on any
  // modifier, so Cmd+K never collides with them.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && (e.key === "k" || e.key === "K")) {
        if (showSettings) return;
        e.preventDefault();
        setPaletteOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [showSettings]);

  // BTW observer: Ctrl/Cmd+Shift+/ opens the read-only side-query drawer.
  // Avoids Cmd+B (bookmark bar), `?` (help), and bare-key panel shortcuts
  // (those bail on any modifier). While settings/palette are up we defer.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && (e.key === "/" || e.key === "?")) {
        if (showSettings || paletteOpen) return;
        e.preventDefault();
        setBtwOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [showSettings, paletteOpen]);

  // Escape closes the open artifact panel (graph / blackboard / timeline / …).
  // Layering: the settings modal sits on top and owns Esc first (its handler is
  // capture-phase + stopPropagation, and we also guard on `showSettings` here so
  // a modal Esc never leaks through to close the panel). The rail's ⋯ menu Esc is
  // an independent transient and doesn't conflict. Only bind while the panel is
  // open and no modal is up. The palette is ALSO a modal layer: while it's open,
  // its own Esc closes it (a React handler with stopPropagation can't stop this
  // native window listener), so gate on !paletteOpen too — otherwise one Esc
  // would close both the palette and the panel beneath it.
  useEffect(() => {
    if (!artifactOpen || showSettings || paletteOpen || btwOpen) return;
    const onEsc = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); setArtifactOpen(false); }
    };
    window.addEventListener("keydown", onEsc);
    return () => window.removeEventListener("keydown", onEsc);
  }, [artifactOpen, showSettings, paletteOpen, btwOpen]);

  // Single-key shortcuts to jump the secondary panels from the keyboard, so a
  // power operator watching a run never has to reach for the mouse. Mnemonic map:
  //   e=evidence  w=workers  g=graph  t=timeline  b=blackboard
  // Esc (above) already closes, so a key only ever OPENS its panel.
  //
  // The make-or-break guard is "never fire while typing": we bail on any modifier
  // (so Cmd+K composer-focus, browser shortcuts, and Ctrl/Alt combos pass through),
  // on IME composition, and when focus is in a text field — checked on BOTH the
  // event.target AND document.activeElement so a steer like "go" typed into the
  // composer can never clobber a panel. Only armed once a run has started (panels
  // are meaningless on the welcome screen) and no modal is up.
  const PANEL_KEYS: Record<string, ArtifactView> = {
    e: "evidence", w: "workers", g: "graph", t: "timeline", b: "blackboard",
    f: "findings", o: "reports", c: "credentials", p: "pocs", r: "routes", d: "directives",
  };
  useEffect(() => {
    if (!deck.started || showSettings || paletteOpen || btwOpen) return;
    const isTyping = (el: EventTarget | Element | null): boolean => {
      const node = el as HTMLElement | null;
      if (!node || typeof node.tagName !== "string") return false;
      const tag = node.tagName.toLowerCase();
      return tag === "input" || tag === "textarea" || tag === "select" || node.isContentEditable;
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.metaKey || e.altKey || e.shiftKey || e.isComposing) return;
      if (isTyping(e.target) || isTyping(document.activeElement)) return;
      const view = PANEL_KEYS[e.key.toLowerCase()];
      if (!view) return;
      e.preventDefault();
      openArtifact(view);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // openArtifact is stable enough for this scope; re-bind only on the gates.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deck.started, showSettings, paletteOpen]);

  // When the auto-title lands live (RUN_TITLED → deck.challengeName), refetch the
  // rail immediately so the active row's "new conversation" placeholder is
  // replaced at once instead of waiting up to one poll interval.
  useEffect(() => {
    if (deck.challengeName) setListBump((n) => n + 1);
  }, [deck.challengeName]);

  const running = isRunActive(deck);
  // "still rehydrating" signal — purely client-derived, no backend field. A real
  // (non-draft) run is SELECTED and the rail's cheap summary says it started (so
  // there ARE events on disk to replay), but the deck hasn't folded its first SSE
  // event yet (deck.started still false). The SSE replay populates a beat after
  // selection, leaving a visible blank gap → show skeletons until RUN_STARTED
  // lands and flips deck.started true. Flips off the instant data arrives.
  const activeSummary = runs.find((r) => r.run_id === runId);
  const loading = !isDraft(runId)
    && !!activeSummary?.started
    && !activeSummary?.finished
    && !deck.started;
  useDeckMotion(shellRef, { flagCount: deck.flags.length });
  // Attach files to the next dispatch. They're uploaded to the run's folder
  // immediately and we keep the saved absolute paths. A draft conversation has
  // no backend run yet, so promote it to a real run-NNNN FIRST (same idiom as
  // dispatch) — then the upload lands under that id and dispatch reuses it.
  const addFiles = async (files: FileList | File[]) => {
    if (!files || files.length === 0) return;
    let id = runId;
    if (!id || isDraft(id)) {
      id = await newRun();
      setRunId(id);
    }
    const saved = await uploadFiles(id, files);
    if (saved.length) setAttachments((prev) => [...prev, ...saved]);
  };
  const removeFile = (path: string) =>
    setAttachments((prev) => prev.filter((f) => f.path !== path));

  // Dispatch the REAL swarm conversationally: one prose prompt → /start. The
  // backend infers category/target from the prompt and races the shelled CLI
  // workers (claude + codex). The flag still only counts if it traces to real
  // execution output (provenance gate).
  const [collectConfirm, setCollectConfirm] = useState<
    { prompt: string; opts?: DispatchOpts } | null
  >(null);
  // Open-ended collect guard (pentest only): dispatching with NO count (form
  // blank AND the planner LLM / regex finds no quota in the text) means the
  // run may never stop on its own. Confirm with the operator before burning
  // budget. CTF collect without a count is NOT gated here: it auto-pauses on
  // no progress, and the composer tooltip already documents that semantics.
  const needsCollectCountConfirm = async (
    prompt: string, opts?: DispatchOpts,
  ): Promise<boolean> => {
    if (!opts || opts.mode !== "pentest") return false;
    if ((opts.collectCount ?? 0) > 0) return false;
    {
      try {
        const res = await apiFetch("/api/dispatch/parse", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            prompt, goal: opts.goal ?? "", mode: "pentest",
          }),
        });
        const data = res.ok ? await res.json().catch(() => ({})) : {};
        const parsed = (data?.parsed ?? {}) as Record<string, unknown>;
        const expected = parsed.expected_findings;
        if (typeof expected === "number" && expected >= 1) return false;
        if (parsed.quantity === "first" || parsed.quantity === "recon") {
          return false;
        }
        if (parsed.quantity === "collect") {
          return parsed.collect_until_coverage !== false;
        }
      } catch { /* LLM unavailable — fall through to the regex mirror */ }
      // Same fallback the backend applies (parse_engagement_goal): a collect
      // keyword without any digit in the goal text is open-ended.
      const text = (opts.goal ?? "").trim() || prompt;
      return /收集|全部|所有|collect/i.test(text) && !/\d/.test(text);
    }
  };
  const dispatch = async (prompt: string, opts?: DispatchOpts) => {
    if (await needsCollectCountConfirm(prompt, opts)) {
      setCollectConfirm({ prompt, opts });
      return false; // intercepted — composer keeps the prompt text
    }
    await doDispatch(prompt, opts);
    return true;
  };
  const doDispatch = async (prompt: string, opts?: DispatchOpts) => {
    setSelected(null);
    // Promote a local draft to a real backend run id at send time, so the run
    // persists + orders as run-NNNN. Already-real ids (selected from the rail,
    // or promoted by addFiles when a file was attached) dispatch as-is. start()
    // reads the current `runId`; pass the freshly minted id explicitly to avoid
    // racing the setRunId state update.
    let id = runId;
    if (!id || isDraft(id)) {
      id = await newRun();
      setRunId(id);
    }
    // webSearch off → offline (backend denies the worker's WebSearch/WebFetch and,
    // by implication, the KB — a clean black-box run).
    const offline = opts ? !opts.webSearch : false;
    // pentest mode (goal-driven, no flag) adds goal/scope. CTF (default) sends
    // nothing extra so its dispatch body stays byte-identical.
    const challenge: Record<string, unknown> = {
      description: prompt,
      attachments: attachments.map((f) => f.path),
    };
    if (opts?.mode === "pentest") {
      challenge.mode = "pentest";
      if (opts.goal) challenge.goal = opts.goal;
      if (opts.scope) challenge.scope = opts.scope;
      if (opts.collectCount && opts.collectCount > 0) {
        challenge.expected_findings = opts.collectCount;
      }
    } else {
      if (opts?.flagFormat === "token") {
        challenge.flag_format = "token";
      } else if (opts?.flagFormat === "custom" && opts.flagWrapper?.trim()) {
        challenge.flag_format_wrapper = opts.flagWrapper.trim();
      }
      if (opts?.collect) {
        // collect mode is multi-flag only. Flag shape is independent: default keeps
        // the backend's brace regex; token/custom mode is opt-in from the advanced panel.
        challenge.multi_flag = true;
        if (opts.collectCount && opts.collectCount > 0) {
          challenge.expected_flags = opts.collectCount;
        }
      }
    }
    // worker isolation toggle → backend's worker_backend ("container" runs each
    // worker in a Docker container that can't read the host bench tree; default
    // "local" = host subprocess).
    const worker_backend = opts?.containerMode ? "container" : "local";
    const runOverrides: Record<string, unknown> = {};
    if (opts?.raceTimeout) runOverrides.race_timeout = opts.raceTimeout;
    if (opts?.wallClockBudget != null) runOverrides.wall_clock_budget = opts.wallClockBudget;
    if (opts?.maxTotalWorkers != null) runOverrides.max_total_workers = opts.maxTotalWorkers;
    if (opts?.costBudgetUsd != null) runOverrides.cost_budget_usd = opts.costBudgetUsd;
    if (opts?.raceEngines?.length) runOverrides.race_engines = opts.raceEngines;
    // attachments: absolute paths the backend saved; the worker stages them into
    // its cwd. Filtered server-side to existing paths, so a stale one is harmless.
    try {
      await start({ kind: "swarm", prompt, offline, challenge, worker_backend, ...runOverrides }, id);
      setAttachments([]); // chips consumed by this dispatch
      setListBump((n) => n + 1);
    } catch (err) {
      const detail = err instanceof Error ? err.message : "";
      pushToast({
        msg: detail ? `${t("toast.dispatchFailed")}：${detail}` : t("toast.dispatchFailed"),
        variant: "error",
      });
    }
  };

  const onNewSolve = () => {
    // Purely local: reset to a fresh empty draft. No backend run is created until
    // the operator dispatches, so "+ New solve" can't litter the rail with stubs.
    setArtifactOpen(false);
    setSelected(null);
    setAttachments([]);
    setRunId(newDraftId());
  };

  const onSelectRun = (id: string) => {
    if (id === runId) return;
    setArtifactOpen(false);
    setSelected(null);
    setAttachments([]);
    setRunId(id);
  };

  // Pin / archive / rename / delete from a row's ⋯ menu. Optimistic: mutate the
  // backend, then bump the rail poll. Archive shows an undo toast; delete asks
  // for confirmation first (irreversible).
  const onRailAction = async (a:
    | { kind: "pin"; runId: string; pinned: boolean }
    | { kind: "archive"; runId: string; archived: boolean }
    | { kind: "rename"; runId: string; name: string }
    | { kind: "delete"; runId: string }
    | { kind: "move"; runId: string; folderId: string | null }
    | { kind: "newFolder" }
    | { kind: "renameFolder"; folderId: string; name: string }
    | { kind: "deleteFolder"; folderId: string }
  ) => {
    if (a.kind === "move") {
      const ok = await patchRun(a.runId, { folder_id: a.folderId });
      bump();
      if (!ok) toastFail();
    } else if (a.kind === "newFolder") {
      // Create instantly with a default name and hand the folder back so the rail
      // drops it into inline-rename — no blocking prompt(). Blur keeps the default.
      const folder = await createFolder(t("rail.newFolderDefault"));
      bump();
      if (folder) pushToast({ msg: t("toast.folderCreated"), variant: "success" });
      else toastFail();
      return folder;
    } else if (a.kind === "renameFolder") {
      const ok = await renameFolder(a.folderId, a.name);
      bump();
      if (ok) pushToast({ msg: t("toast.folderRenamed"), variant: "success" });
      else toastFail();
    } else if (a.kind === "deleteFolder") {
      if (!window.confirm(t("rail.confirmDeleteFolder"))) return;
      const ok = await deleteFolder(a.folderId);
      bump();
      if (ok) pushToast({ msg: t("toast.folderDeleted"), variant: "success" });
      else toastFail();
    } else if (a.kind === "pin") {
      const ok = await patchRun(a.runId, { pinned: a.pinned });
      bump();
      if (!ok) toastFail();
    } else if (a.kind === "rename") {
      const ok = await patchRun(a.runId, { name: a.name });
      bump();
      if (ok) pushToast({ msg: t("toast.renamed"), variant: "success" });
      else toastFail();
    } else if (a.kind === "archive") {
      const ok = await patchRun(a.runId, { archived: a.archived });
      bump();
      if (!ok) { toastFail(); return; }
      if (a.archived) {
        // KEEP the archive-undo behavior — migrated into the unified lane.
        pushToast({
          msg: t("rail.toast.archived"),
          variant: "info",
          undo: async () => { await patchRun(a.runId, { archived: false }); bump(); },
        });
      }
    } else if (a.kind === "delete") {
      if (!window.confirm(t("rail.confirmDelete"))) return;
      const ok = await deleteRun(a.runId);
      // if we just deleted the open conversation, fall back to a fresh draft
      if (ok && a.runId === runId) { setArtifactOpen(false); setSelected(null); setAttachments([]); setRunId(newDraftId()); }
      bump();
      if (ok) pushToast({ msg: t("toast.deleted"), variant: "success" });
      else toastFail();
    }
  };

  const openArtifact = (view: ArtifactView) => {
    setArtifactView(view);
    setArtifactOpen(true);
  };

  // Roster mini-row click → open the Worker 详情 panel focused on that worker.
  // Bump the nonce so re-clicking the same row re-seeds the lanes filter.
  const onOpenWorker = (id: string) => {
    setFocusedWorker((prev) => ({ id, nonce: (prev?.nonce ?? 0) + 1 }));
    openArtifact("workers");
  };

  // Worker 详情 "在活动流中查看" → open the timeline tab with that worker's
  // speaker chip pre-selected (reverse of onOpenWorker; bump nonce to re-seed).
  const onOpenSpeakerTimeline = (id: string) => {
    setFocusedSpeaker((prev) => ({ id, nonce: (prev?.nonce ?? 0) + 1 }));
    openArtifact("timeline");
  };

  const onOpenReport = (id: string) => {
    setFocusedReport((prev) => ({ id, nonce: (prev?.nonce ?? 0) + 1 }));
    openArtifact("reports");
  };

  // operator runtime worker control (BE-worker-management): add/kill an engine on
  // the LIVE run. Best-effort; the coordinator drains the command next tick and the
  // worker lifecycle events (worker_spawned / worker_killed) fold back over SSE.
  const onSpawnWorker = async (engine?: string) => {
    if (!runId) return;
    const ok = await spawnWorker(runId, engine);
    if (ok) pushToast({ msg: t("toast.workerSpawned"), variant: "success", icon: "cpu" });
    else toastFail();
  };
  const onKillWorker = async (solverId: string) => {
    if (!runId) return;
    const ok = await killWorker(runId, solverId);
    if (ok) pushToast({ msg: t("toast.workerKilled"), variant: "info", icon: "cpu" });
    else toastFail();
  };
  // reveal the run's workspace dir in the host file manager (real backend run only).
  const onOpenWorkspace = () => { if (runId && !isDraft(runId)) openWorkspace(runId); };

  return (
    <div ref={shellRef} className="shell motion-root">
      <a href="#main-conversation" className="skip-link">{t("a11y.skipToMain")}</a>
      <ThreadRail
        collapsed={railCollapsed}
        width={railWidth}
        runs={runs}
        folders={folders}
        activeRunId={runId}
        draftActive={isDraft(runId)}
        connected={connected}
        onNew={onNewSolve}
        onSelect={onSelectRun}
        onAction={onRailAction}
        onResize={onRailResize}
        onOpenSettings={openWorkerSettings}
      />
      <main id="main-conversation" className="main motion-shell-piece" aria-label={t("a11y.main")}>
        <Conversation
          deck={deck}
          running={running}
          loading={loading}
          connected={connected}
          onCommand={onCommand}
          onResolve={resolve}
          onDispatch={dispatch}
          attachments={attachments}
          onAddFiles={addFiles}
          onRemoveFile={removeFile}
          artifactOpen={artifactOpen}
          artifactView={artifactView}
          onOpenArtifact={openArtifact}
          onShowConversation={() => setArtifactOpen(false)}
          runtimePanel={(
            <RuntimeArtifactPanel
              open={artifactOpen}
              width={artifactWidth}
              view={artifactView}
              deck={deck}
              running={running}
              loading={loading}
              selected={selected}
              onSelect={setSelected}
              onView={setArtifactView}
              onClose={() => setArtifactOpen(false)}
              onResize={onArtifactResize}
              minWidth={ARTIFACT_WIDTH_MIN}
              maxWidth={artifactWidthMax(winW)}
              defaultWidth={artifactWidthDefault(winW)}
              onSpawnWorker={onSpawnWorker}
              onKillWorker={onKillWorker}
              focusWorker={focusedWorker}
              focusReport={focusedReport}
              focusPoc={focusedPoc}
              focusFact={focusedFact}
              focusSpeaker={focusedSpeaker}
              onOpenSpeakerTimeline={onOpenSpeakerTimeline}
              onOpenFact={(seq) => {
                setArtifactOpen(true);
                setArtifactView("evidence");
                setFocusedFact({ seq, nonce: Date.now() });
              }}
              onOpenPoc={(id) => {
                setArtifactOpen(true);
                setArtifactView("pocs");
                setFocusedPoc({ id, nonce: Date.now() });
              }}
              onAdoptReview={(text) => { void onCommand("global", "directive", text); }}
              workspaceMode
            />
          )}
          onToggleRail={() => setRailCollapsed((v) => !v)}
          theme={theme}
          onToggleTheme={toggleTheme}
          onSpawnWorker={onSpawnWorker}
          onKillWorker={onKillWorker}
          onOpenWorker={onOpenWorker}
          onOpenReport={onOpenReport}
          onOpenWorkspace={onOpenWorkspace}
          onHitlAnswered={() => pushToast({ msg: t("hitl.answered"), variant: "success" })}
          onOpenBtw={() => setBtwOpen(true)}
        />
      </main>
      <ToastLane toasts={toasts} onDismiss={dismissToast} />
      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        started={deck.started}
        running={running}
        runs={runs}
        activeRunId={runId}
        onNewSolve={onNewSolve}
        onOpenArtifact={openArtifact}
        onSelectRun={onSelectRun}
        onSpawnWorker={onSpawnWorker}
        onOpenSettings={openWorkerSettings}
      />
      <BtwPanel
        open={btwOpen}
        onClose={() => setBtwOpen(false)}
        runId={runId}
      />
      {collectConfirm && (
        <div className="modal-backdrop" onClick={() => setCollectConfirm(null)}>
          <div className="modal collect-warn" onClick={(e) => e.stopPropagation()}>
            <div className="collect-warn-title">{t("collectWarn.title")}</div>
            <div className="collect-warn-body">{t("collectWarn.body")}</div>
            <div className="collect-warn-actions">
              <button type="button" onClick={() => setCollectConfirm(null)}>
                {t("collectWarn.cancel")}
              </button>
              <button
                type="button"
                className="danger"
                onClick={() => {
                  const pending = collectConfirm;
                  setCollectConfirm(null);
                  void doDispatch(pending.prompt, pending.opts);
                }}
              >
                {t("collectWarn.continue")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
