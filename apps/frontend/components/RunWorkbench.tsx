"use client";
import { setThemePreference, subscribeThemePreference } from "@/lib/themePreference";
import { countTimelineEvents } from "@/components/activity-timeline/fromDeck";
import { lazy, Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import type { ActivityProps, ExoticComponent, ReactNode, KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from "react";
import { useRun, useRunList, useFolders, newRun, patchRun, deleteRun, uploadFiles, spawnWorker, killWorker, openWorkspace, createFolder, renameFolder, deleteFolder, decideRunGuardApproval, RunStartError, SavedFile, apiFetch, type OperatorTriageLabel } from "@/lib/useRun";
import { useT, useLang } from "@/lib/i18n";
import { DeckState, type GuardApprovalDecisionAction, isFactRetired, isRunActive, workerIds, swarmPhase } from "@/lib/events";
import { CTF_DEMO_DEFAULT_RUN, CTF_DEMO_NOTICE, ctfDemoDeck, ctfDemoRunSummaries, isCtfDemoRunId } from "@/lib/ctfDemoRun";
import { ThreadRail } from "@/components/ThreadRail";
import { conversationHref } from "@/lib/runWorkspaceRoutes";
import type { ControlCommandOpts, DispatchOpts } from "@/components/Conversation";
import { CommandPalette } from "@/components/CommandPalette";
import { RunSideChatColumn, runSideChatStatus, useRunSideChatController } from "@/components/run-side/RunSideChat";
import type { ArtifactView } from "@/lib/events";
import { useConversationChrome } from "@/components/conversationChrome";
import { setPaneResizing } from "@/lib/paneResize";
import { useFlagPulse } from "@/lib/useFlagPulse";
import { Icon, type IconName } from "@muteki/ui";
import { Button, Dialog, Skeleton, Tabs, TabsContent, TabsList, TabsTrigger, toast } from "@muteki/ui";
import { toWorkerIdentity, workerDisplayName, workerEngine } from "@/lib/workers";
import { readSavedTheme } from "@/lib/palette-engine";
import { formatElapsed, toEpochMs } from "@/lib/format";
import { PANEL_HOTKEYS, RUNTIME_TABS, type RuntimeGroup } from "@/lib/runtimeTabs";
import { shouldResetRuntimePanelForRun, shouldRestoreRuntimePanelOpen } from "@/lib/draftRuntimePanel";
import { collabPagePath, readCollabUrlState } from "@/lib/collabUrlState";
import { directiveStatusLabel, pocStatusLabel, reviewKindLabel, reviewSeverityLabel } from "@/lib/statusLabels";

const Activity = Symbol.for("react.activity") as unknown as ExoticComponent<ActivityProps>;
const Conversation = lazy(() => import("@/components/Conversation").then((module) => ({ default: module.Conversation })));
const ActivityTimeline = lazy(() => import("@/components/activity-timeline/ActivityTimeline").then((module) => ({ default: module.ActivityTimeline })));
const WorkerLanes = lazy(() => import("@/components/WorkerLanes").then((module) => ({ default: module.WorkerLanes })));
const EvidenceChain = lazy(() => import("@/components/EvidenceChain").then((module) => ({ default: module.EvidenceChain })));
const PentestEvidence = lazy(() => import("@/components/PentestEvidence").then((module) => ({ default: module.PentestEvidence })));
const UsageDashboard = lazy(() => import("@/components/UsageDashboard").then((module) => ({ default: module.UsageDashboard })));
const ReportsPage = lazy(() => import("@/components/reports/ReportsPage").then((module) => ({ default: module.ReportsPage })));
const PentestReportsPage = lazy(() => import("@/components/reports/PentestReportsPage").then((module) => ({ default: module.PentestReportsPage })));
const PentestReport = lazy(() => import("@/components/PentestReport").then((module) => ({ default: module.PentestReport })));

function PanelLoading({ rows = 4 }: { rows?: number }) {
  return (
    <div className="panel-skel" aria-hidden="true">
      {Array.from({ length: rows }).map((_, index) => (
        <div className="panel-skel-row" key={index}>
          <div className="panel-skel-head">
            <Skeleton className="skel-box t-skeleton" style={{ width: 22, height: 22, borderRadius: 6 }} />
            <Skeleton className="skel-line t-skeleton" style={{ width: `${52 - index * 4}%`, height: 11 }} />
            <Skeleton className="skel-line t-skeleton" style={{ width: 40, height: 9, marginLeft: "auto" }} />
          </div>
          <Skeleton className="skel-line t-skeleton" style={{ width: "100%", height: 10 }} />
          <Skeleton className="skel-line t-skeleton" style={{ width: `${72 - index * 6}%`, height: 10 }} />
        </div>
      ))}
    </div>
  );
}


const newDraftId = () => `draft-${Date.now().toString(36)}-${Math.floor(Math.random() * 1e6).toString(36)}`;
const isDraft = (id: string) => id.startsWith("draft-");
type ThemeMode = "light" | "dark";
const ARTIFACT_WIDTH_MIN = 360;
const ARTIFACT_WIDTH_MAX = 960;
const ARTIFACT_WIDTH_STORAGE_KEY = "muteki.artifact.width";
const ARTIFACT_OPEN_STORAGE_KEY = "muteki.artifactOpen";
const ARTIFACT_VIEW_STORAGE_KEY = "muteki.runtimeView";

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

const runIdFromPath = (): string => {
  if (typeof window === "undefined") return "";
  const m = window.location.pathname.match(/^\/run\/([^/]+)\/?$/);
  return m ? decodeURIComponent(m[1]) : "";
};
const urlForRun = (id: string): string =>
  id && !isDraft(id) ? `/run/${encodeURIComponent(id)}` : "/ctf";

const RUNTIME_GROUPS: { id: RuntimeGroup; key: string; descKey: string; icon: IconName }[] = [
  { id: "observe", key: "runtime.group.observe", descKey: "runtime.group.observeDesc", icon: "radio" },
  { id: "investigate", key: "runtime.group.investigate", descKey: "runtime.group.investigateDesc", icon: "crosshair" },
  { id: "assets", key: "runtime.group.assets", descKey: "runtime.group.assetsDesc", icon: "layers" },
];

const RUNTIME_COPY: Record<string, { zh: string; en: string }> = {
  "runtime.title": { zh: "运行时", en: "Runtime" },
  "runtime.untitled": { zh: "未命名任务", en: "Untitled run" },
  "runtime.nav.groups": { zh: "运行时分类", en: "Runtime categories" },
  "runtime.nav.views": { zh: "运行时视图", en: "Runtime views" },
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
  "runtime.ledger.toolGroup": { zh: "{n} 次调用", en: "{n} calls" },
  "runtime.ledger.toolGroupAria": { zh: "调用了 {n} 次工具，最近 {cmd}", en: "Called {n} tools, latest {cmd}" },
  "runtime.ledger.toolFailed": { zh: "失败", en: "Failed" },
  "runtime.ledger.toolPending": { zh: "进行中", en: "Running" },
  "runtime.ledger.toolCallFailed": { zh: "调用失败", en: "Call failed" },
  "runtime.ledger.toolCallOk": { zh: "调用成功", en: "Call succeeded" },
  "runtime.pentest.process": { zh: "执行过程", en: "Execution steps" },
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

function useRuntimeElapsed(startedAt?: number, finishedAt?: number, activeDurationMs = 0, activeSince?: number): string {
  const live = activeSince != null && finishedAt == null;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!live) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [live]);
  if (startedAt == null) return "";
  const end = finishedAt != null ? toEpochMs(finishedAt) : now;
  return formatElapsed(activeDurationMs + (activeSince == null ? 0 : Math.max(0, end - toEpochMs(activeSince))));
}

function runtimeTabCount(view: ArtifactView, deck: DeckState, running: boolean, runMode: "ctf" | "pentest"): number | null {
  switch (view) {
    case "usage": return null;
    case "timeline": return countTimelineEvents(deck, running);
    case "workers": return Object.keys(deck.lanes).length;
    case "collaboration": return null;
    case "evidence": return deck.mode === "pentest"
      ? deck.blackboard.facts.filter((fact) => fact.verified && !isFactRetired(fact) && fact.actor !== "origin").length
        + deck.blackboard.observations.filter((observation) => !observation.admitted).length
        + deck.blackboard.deadEnds.length
      : deck.blackboard.facts.filter((fact) => fact.actor !== "origin").length + deck.blackboard.deadEnds.length;
    case "findings": return deck.blackboard.reviewFindings?.length ?? 0;
    case "pocs": return deck.blackboard.pocs.length;
    case "routes": return runMode === "pentest"
      ? deck.blackboard.intents.length
      : (deck.blackboard.suppressedRoutes?.length ?? 0) + (deck.blackboard.branches?.length ?? 0);
    case "directives": return (deck.blackboard.directives?.length ?? 0) + deck.operatorDirectives.length;
    case "credentials": return null;
    default: {
      const _exhaustive: never = view;
      return _exhaustive;
    }
  }
}

function RuntimeWorkerView({ deck, running, focusWorker, onSpawnWorker, onKillWorker, onOpenSpeakerTimeline, onOpenAgent }: {
  deck: DeckState; running: boolean; focusWorker?: { id: string; nonce: number } | null;
  onSpawnWorker: (engine?: string) => void; onKillWorker: (id: string) => void;
  onOpenSpeakerTimeline?: (id: string) => void;
  onOpenAgent?: (id: string) => void;
}) {
  const ids = workerIds(deck);
  const lanes = ids.map((id) => deck.lanes[id]).filter(Boolean);
  const calls = lanes.reduce((sum, lane) => sum + lane.toolLines.length, 0);
  const phase = swarmPhase(deck);
  const elapsed = useRuntimeElapsed(deck.startedAt, deck.finishedAt, deck.activeDurationMs, deck.activeSince);
  return (
    <WorkerLanes
      deck={deck}
      running={running}
      focusWorker={focusWorker}
      onSpawnWorker={onSpawnWorker}
      onKillWorker={onKillWorker}
      onOpenSpeakerTimeline={onOpenSpeakerTimeline}
      onOpenAgent={onOpenAgent}
      phase={phase}
      elapsed={elapsed}
      calls={calls}
    />
  );
}

function RuntimeList({ children }: { children: ReactNode }) { return <div className="artifact-list">{children}</div>; }
function RuntimeEmpty({ kind = "generic" }: { kind?: "generic" | "credentials" | "pocs" | "routes" | "directives" | "findings" }) {
  const t = useRuntimeT();
  return <div className="artifact-list-empty"><div className="panel-empty">
    <span className="panel-empty-ico" aria-hidden="true"><Icon name="info" size={22} /></span>
    <strong className="panel-empty-title">{t(`runtime.empty.${kind}.title`)}</strong>
    <span className="panel-empty-hint">{t(`runtime.empty.${kind}.hint`)}</span>
  </div></div>;
}
type CredRow = { entity: string; value: string; seq?: number };
type CredFetch = { status: "loading" | "ready" | "error"; rows: CredRow[]; message?: string; retry: () => void };

function useRunCredentials(runId: string, factGen: number): CredFetch {
  const t = useRuntimeT();
  const [state, setState] = useState<Omit<CredFetch, "retry">>({ status: "loading", rows: [] });
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let alive = true;
    if (!runId || runId.startsWith("draft-") || isCtfDemoRunId(runId)) {
      setState({ status: "ready", rows: [] });
      return () => { alive = false; };
    }
    setState((prev) => ({ ...prev, status: prev.rows.length ? "ready" : "loading", message: undefined }));
    const timer = window.setTimeout(() => {
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
    }, 250);
    return () => { alive = false; window.clearTimeout(timer); };
  }, [runId, factGen, tick, t]);
  return { ...state, retry: () => setTick((n) => n + 1) };
}

function RuntimeCredentials({ creds, onOpenFact }: { creds: CredFetch; onOpenFact?: (seq: number) => void }) {
  const t = useRuntimeT();
  const [revealed, setRevealed] = useState<Record<string, boolean>>({});
  const [copied, setCopied] = useState<string>("");
  if (creds.status === "loading" && !creds.rows.length) return <div className="panel-scroll"><PanelLoading rows={3} /></div>;
  if (creds.status === "error") {
    return <div className="artifact-list-empty">
      <div>{creds.message || t("runtime.credentials.error")}</div>
      <button type="button" className="evi-filter-btn" onClick={creds.retry}>{t("runtime.credentials.retry")}</button>
    </div>;
  }
  if (!creds.rows.length) return <RuntimeEmpty kind="credentials" />;
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

function directiveRowKey(row: { actor: string; ts: number; action: string; directive: string }): string {
  const raw = `${row.actor}\0${row.ts}\0${row.action}\0${row.directive}`;
  let hash = 0;
  for (let i = 0; i < raw.length; i += 1) hash = (hash * 31 + raw.charCodeAt(i)) | 0;
  return `dir-${row.ts}-${(hash >>> 0).toString(36)}`;
}

function RuntimeDataView({ view, deck, focusPoc, onAdoptReview }: {
  view: ArtifactView;
  deck: DeckState;
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
  if (view === "findings") {
    const reviewed = [...(deck.blackboard.reviewFindings ?? [])]
      .sort((a, b) => reviewSeverityRank(a.severity) - reviewSeverityRank(b.severity) || b.ts - a.ts);
    if (!reviewed.length) return <RuntimeEmpty kind="findings" />;
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
    const rows = deck.blackboard.pocs ?? []; if (!rows.length) return <RuntimeEmpty kind="pocs" />;
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
    if (!routes.length && !branches.length) return <RuntimeEmpty kind="routes" />;
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
  if (!rows.length && !lifecycle.length) return <RuntimeEmpty kind="directives" />;
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

function RuntimeArtifactPanel({ runId, open, width, view, deck, running, loading, hotkeySwitch = false, onView, onClose, onResize, minWidth, maxWidth, defaultWidth, onSpawnWorker, onKillWorker, focusWorker, focusPoc, focusFact, focusSpeaker, onOpenSpeakerTimeline, onOpenAgent, onOpenFact, onOpenReport, onAdoptReview, workspaceMode = false, runMode = "ctf" }: {
  runId: string; open: boolean; width: number; view: ArtifactView; deck: DeckState; running: boolean; loading: boolean; hotkeySwitch?: boolean;
  onView: (view: ArtifactView) => void; onClose: () => void; onResize: (width: number) => void;
  minWidth: number; maxWidth: number; defaultWidth: number; onSpawnWorker: (engine?: string) => void; onKillWorker: (id: string) => void; focusWorker?: { id: string; nonce: number } | null; focusPoc?: { id: string; nonce: number } | null; focusFact?: { seq: number; nonce: number } | null; focusSpeaker?: { id: string; nonce: number } | null; onOpenSpeakerTimeline?: (id: string) => void; onOpenAgent?: (id: string) => void; onOpenFact?: (seq: number) => void; onOpenReport?: () => void; onAdoptReview?: (text: string) => void; workspaceMode?: boolean; runMode?: "ctf" | "pentest";
}) {
  const t = useRuntimeT();
  const credentialsNeeded = open && view === "credentials";
  const factGen = credentialsNeeded ? Math.max(0, ...deck.blackboard.facts.filter((fact) => fact.verified && typeof fact.factSeq === "number").map((fact) => fact.factSeq as number)) : 0;
  const creds = useRunCredentials(credentialsNeeded ? deck.runId : "", factGen);
  const workspaceActive = workspaceMode && open;
  const [resizing, setResizing] = useState(false);
  const artifactRef = useRef<HTMLDivElement | null>(null);
  const resizeCleanupRef = useRef<(() => void) | null>(null);
  useEffect(() => () => resizeCleanupRef.current?.(), []);
  useLayoutEffect(() => {
    if (resizing) return;
    const node = artifactRef.current;
    if (!node || workspaceActive) return;
    node.style.width = `${open ? width : 0}px`;
    node.style.flexBasis = `${open ? width : 0}px`;
  }, [resizing, open, width, workspaceActive]);
  const [pentestEvidenceView, setPentestEvidenceView] = useState<"detail" | "context">("detail");
  const focusedFact = focusFact?.seq ? deck.blackboard.facts.find((item) => item.factSeq === focusFact.seq) : undefined;
  const focusedFactHasEvidence = !!focusedFact?.verified && !isFactRetired(focusedFact);
  useEffect(() => {
    if (runMode !== "pentest" || !focusFact?.seq) return;
    setPentestEvidenceView(focusedFactHasEvidence ? "detail" : "context");
  }, [runMode, focusFact?.seq, focusFact?.nonce, focusedFactHasEvidence]);
  const isLocalPi = deck.driverKind === "local_pi";
  const isViewDisabled = (v: ArtifactView) => isLocalPi && v !== "timeline" && v !== "evidence" && v !== "usage";
  const currentTab = RUNTIME_TABS.find((tab) => tab.view === view) ?? RUNTIME_TABS[0];
  const currentGroup = RUNTIME_GROUPS.find((group) => group.id === currentTab.group) ?? RUNTIME_GROUPS[0];
  const visibleTabs = RUNTIME_TABS.filter((tab) => runMode !== "pentest" || tab.view !== "findings");
  const groupTabs = visibleTabs.filter((tab) => tab.group === currentGroup.id);
  useEffect(() => {
    if (isLocalPi && isViewDisabled(view)) {
      onView("timeline");
    }
  }, [isLocalPi, view, onView]);
  const startResize = (event: ReactPointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.currentTarget.setPointerCapture?.(event.pointerId);
    resizeCleanupRef.current?.();
    setResizing(true);
    setPaneResizing(true);
    const startX = event.clientX;
    const startWidth = width;
    let pendingRaw: number | null = null;
    let lastRaw = startWidth;
    let frame = 0;
    const flush = () => {
      frame = 0;
      if (pendingRaw == null) return;
      const raw = pendingRaw;
      pendingRaw = null;
      lastRaw = raw;
      const node = artifactRef.current;
      const shown = Math.max(0, Math.min(maxWidth, raw));
      if (node) {
        node.style.width = `${shown}px`;
        node.style.flexBasis = `${shown}px`;
      }
    };
    const move = (pointer: PointerEvent) => {
      pendingRaw = Math.round(startWidth + (startX - pointer.clientX));
      if (!frame) frame = requestAnimationFrame(flush);
    };
    const stop = () => {
      resizeCleanupRef.current = null;
      cancelAnimationFrame(frame);
      flush();
      setResizing(false);
      setPaneResizing(false);
      if (lastRaw < Math.min(200, minWidth - 80)) onClose();
      else onResize(Math.max(minWidth, Math.min(maxWidth, lastRaw)));
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    resizeCleanupRef.current = stop;
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  };
  const resizeKey = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.key === "ArrowLeft") { event.preventDefault(); onResize(width + (event.shiftKey ? 32 : 12)); }
    if (event.key === "ArrowRight") { event.preventDefault(); onResize(width - (event.shiftKey ? 32 : 12)); }
    if (event.key === "Home") { event.preventDefault(); onResize(minWidth); }
    if (event.key === "End") { event.preventDefault(); onResize(maxWidth); }
    if (event.key === "Enter") { event.preventDefault(); onResize(defaultWidth); }
  };
  const runtimeNavigation = open ? <div className="runtime-navigation">
    <Tabs value={currentGroup.id} onChange={(key: string) => onView(visibleTabs.find((tab) => tab.group === key && !isViewDisabled(tab.view))?.view ?? "timeline")} variant="segmented" size="sm">
      <TabsList className="runtime-primary-nav" ariaLabel={t("runtime.nav.groups")}>{RUNTIME_GROUPS.map((group) => <TabsTrigger value={group.id} key={group.id} disabled={isLocalPi && group.id === "assets"} icon={group.icon}><span>{t(group.key)}</span></TabsTrigger>)}</TabsList>
    </Tabs>
    <span className="runtime-nav-divider" aria-hidden="true" />
    <Tabs value={view} onChange={(key: string) => onView(key as ArtifactView)} variant="underline" size="sm">
      <TabsList className="runtime-view-nav" ariaLabel={t("runtime.nav.views")}>{groupTabs.map((tab) => { const count = runtimeTabCount(tab.view, deck, running, runMode); return <TabsTrigger value={tab.view} key={tab.view} disabled={isViewDisabled(tab.view)} icon={tab.icon}><span>{t(runMode === "pentest" && tab.view === "routes" ? "runtime.pentest.process" : tab.key)}</span>{count !== null && <b>{count}</b>}</TabsTrigger>; })}</TabsList>
    </Tabs>
  </div> : null;
  if (runMode === "pentest" && workspaceActive) {
    const body = <div className="pentest-runtime-content">
      <Suspense fallback={<div className="panel-scroll"><PanelLoading /></div>}>
        {loading ? <div className="panel-scroll"><PanelLoading rows={4} /></div>
          : view === "evidence" ? <Tabs className="pentest-evidence-views" value={pentestEvidenceView} onChange={(key: string) => setPentestEvidenceView(key as "detail" | "context")} variant="underline" size="sm">
              <TabsList className="runtime-view-nav" ariaLabel={t("runtime.evidence.views")}>
                <TabsTrigger value="detail">{t("runtime.evidence.detail")}</TabsTrigger>
                <TabsTrigger value="context">{t("runtime.evidence.context")}</TabsTrigger>
              </TabsList>
              <TabsContent value="detail"><PentestEvidence deck={deck} runId={runId} focusFactSeq={focusFact?.seq} focusNonce={focusFact?.nonce} onOpenReport={onOpenReport} /></TabsContent>
              <TabsContent value="context"><EvidenceChain deck={deck} focusFactSeq={focusFact?.seq} focusNonce={focusFact?.nonce} /></TabsContent>
            </Tabs>
          : view === "timeline" ? <ActivityTimeline deck={deck} running={running} focusSpeaker={focusSpeaker} />
          : view === "workers" ? <RuntimeWorkerView deck={deck} running={running} focusWorker={focusWorker} onSpawnWorker={onSpawnWorker} onKillWorker={onKillWorker} onOpenSpeakerTimeline={onOpenSpeakerTimeline} onOpenAgent={onOpenAgent} />
          : view === "routes" ? <div className="pentest-process"><h2>实际执行过程</h2><p>按共享图中真实记录的 Step 展示，不预设攻击路径。</p>{deck.blackboard.intents.length ? deck.blackboard.intents.map((intent, index) => <article key={intent.id}><span>{String(index + 1).padStart(2, "0")}</span><div><h3>{intent.summary || intent.goal}</h3><small>{intent.worker || intent.proposedBy || "Coordinator"} · {intent.status} · {intent.proposedTs ? new Date(toEpochMs(intent.proposedTs)).toLocaleString() : "—"}</small></div></article>) : <div className="panel-empty">尚无 Step 记录</div>}</div>
          : view === "credentials" ? <RuntimeCredentials creds={creds} onOpenFact={onOpenFact} />
          : view === "usage" ? <UsageDashboard runId={runId} />
          : <RuntimeDataView view={view} deck={deck} focusPoc={focusPoc} onAdoptReview={onAdoptReview} />}
      </Suspense>
      </div>;
    return <div className="artifact workspace-mode pentest-runtime" role="region" aria-label="渗透测试运行时">
      {runtimeNavigation}
      {body}
    </div>;
  }
  return <div ref={artifactRef} className={`artifact ${workspaceActive ? "workspace-mode" : ""} ${resizing ? "resizing" : ""}`} style={workspaceActive ? undefined : { width: open ? width : 0, flexBasis: open ? width : 0 }} role="region" aria-label={t("a11y.artifact")}>
    {open && <>
      {!workspaceActive && <div className="artifact-resizer" role="separator" tabIndex={0} aria-label={t("art.resizeCanvas")} aria-valuemin={minWidth} aria-valuemax={maxWidth} aria-valuenow={width} onPointerDown={startResize} onKeyDown={resizeKey} onDoubleClick={() => onResize(defaultWidth)} />}
      {!workspaceActive && <div className="artifact-head runtime-head"><div className="runtime-titlemark"><Icon name={currentTab.icon} size={16} /></div><div className="runtime-heading"><span className="runtime-eyebrow">{t("runtime.title")}</span><div className="runtime-titleline"><strong>{t(currentTab.key)}</strong><span className={`runtime-state ${running ? "live" : deck.finished ? "complete" : "standby"}`}><span className="runtime-state-dot" />{t(running ? "runtime.status.live" : deck.finished ? "runtime.status.complete" : "runtime.status.standby")}</span></div><span className="runtime-context">{deck.challengeName || t("runtime.untitled")} · {deck.runId}</span></div><span className="spacer" /><Button variant="unstyled" className="x" onClick={onClose} aria-label={t("art.closeCanvas")}><Icon name="x" size={15} /></Button></div>}
      {runtimeNavigation}
      <div className="artifact-body"><div className="artifact-view" data-instant={hotkeySwitch ? "true" : undefined} key={loading ? "loading" : view}>
        <Suspense fallback={<div className="panel-scroll"><PanelLoading /></div>}>
          {loading ? <div className="panel-scroll"><PanelLoading rows={4} /></div>
            : view === "usage" ? <UsageDashboard runId={runId} />
            : view === "workers" ? <RuntimeWorkerView deck={deck} running={running} focusWorker={focusWorker} onSpawnWorker={onSpawnWorker} onKillWorker={onKillWorker} onOpenSpeakerTimeline={onOpenSpeakerTimeline} onOpenAgent={onOpenAgent} />
            : view === "timeline" ? <ActivityTimeline deck={deck} running={running} focusSpeaker={focusSpeaker} />
            : view === "evidence" ? <EvidenceChain deck={deck} focusFactSeq={focusFact?.seq} focusNonce={focusFact?.nonce} />
            : view === "credentials" ? <RuntimeCredentials creds={creds} onOpenFact={onOpenFact} />
            : <RuntimeDataView view={view} deck={deck} focusPoc={focusPoc} onAdoptReview={onAdoptReview} />}
        </Suspense>
        </div></div>
    </>}
  </div>;
}

export function CtfWorkspace() {
  const pathname = usePathname();
  return <Deck workspaceMode="ctf" surface={pathname === "/ctf/reports" ? "reports" : "conversation"} />;
}

export default function Page() {
  return <Suspense fallback={<PanelLoading />}><Deck workspaceMode="ctf" /></Suspense>;
}

export function PentestWorkbench() {
  return <Suspense fallback={<PanelLoading />}><Deck workspaceMode="pentest" /></Suspense>;
}

export function CtfReportsWorkbench() {
  return <Suspense fallback={<PanelLoading />}><Deck workspaceMode="ctf" surface="reports" /></Suspense>;
}

export function PentestReportsWorkbench() {
  return <Suspense fallback={<PanelLoading />}><Deck workspaceMode="pentest" surface="reports" /></Suspense>;
}

function Deck({ workspaceMode, surface = "conversation" }: { workspaceMode: "ctf" | "pentest"; surface?: "conversation" | "reports" }) {
  const t = useT();
  const [runId, setRunId] = useState("");
  const runIdRef = useRef(runId);
  runIdRef.current = runId;
  const router = useRouter();
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const reportsSurface = surface === "reports";
  const [reportsOpened, setReportsOpened] = useState(reportsSurface);
  const [conversationOpened, setConversationOpened] = useState(!reportsSurface);
  if (reportsSurface && !reportsOpened) setReportsOpened(true);
  if (!reportsSurface && !conversationOpened) setConversationOpened(true);
  const requestedRunId = !reportsSurface && workspaceMode === "pentest" ? searchParams.get("run") || "" : "";
  const requestedSearch = searchParams.toString();
  useLayoutEffect(() => {
    if (reportsSurface) {
      setRunId(current => current || newDraftId());
      return;
    }
    const legacyRun = workspaceMode === "pentest"
      ? requestedRunId
      : runIdFromPath();
    if (workspaceMode === "ctf" && legacyRun && readCollabUrlState().view === "collaboration") {
      const params = new URLSearchParams(window.location.search);
      params.delete("view");
      const query = params.toString();
      router.replace(`${collabPagePath(legacyRun)}${query ? `?${query}` : ""}`);
      return;
    }
    const demoLanding = workspaceMode === "ctf" && new URLSearchParams(window.location.search).get("demo") === "1";
    setRunId(current => legacyRun || (demoLanding && !current ? CTF_DEMO_DEFAULT_RUN : isDraft(current) ? current : newDraftId()));
    const onPop = () => {
      if (reportsSurface) return;
      setRunId((workspaceMode === "pentest"
        ? new URLSearchParams(window.location.search).get("run") || ""
        : runIdFromPath()) || newDraftId());
    };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, [pathname, requestedRunId, router, workspaceMode, reportsSurface]);
  useEffect(() => {
    if (!runId || reportsSurface) return;
    if (workspaceMode === "pentest") {
      const params = new URLSearchParams(window.location.search);
      if (isDraft(runId)) params.delete("run");
      else params.set("run", runId);
      const query = params.toString();
      const url = `/pentest${query ? `?${query}` : ""}`;
      if (`${window.location.pathname}${window.location.search}` !== url) {
        window.history.replaceState({}, "", url);
      }
    } else {
      const url = urlForRun(runId);
      if (window.location.pathname !== url) window.history.replaceState({}, "", `${url}${window.location.search}`);
    }
  }, [runId, workspaceMode]);
  const demoDeck = workspaceMode === "ctf" ? ctfDemoDeck(runId) : null;
  const liveRun = useRun(demoDeck ? "" : runId);
  const { replayEmpty, start, sendHitl, requestProgress, resolve, startFollowup, cancelFollowup, refreshGuardState } = liveRun;
  const deck = demoDeck ?? liveRun.deck;
  const connected = demoDeck ? true : liveRun.connected;
  useEffect(() => {
    if (workspaceMode === "ctf" && deck.started && deck.mode === "pentest" && runId) {
      const params = new URLSearchParams(window.location.search);
      params.set("run", runId);
      const query = params.toString();
      router.replace(`/pentest?${query}`);
    } else if (workspaceMode === "pentest" && deck.started && deck.mode === "ctf" && runId) {
      const params = new URLSearchParams(window.location.search);
      params.delete("run");
      const query = params.toString();
      router.replace(`/run/${encodeURIComponent(runId)}${query ? `?${query}` : ""}`);
    }
  }, [deck.started, deck.mode, router, runId, workspaceMode]);

  const {
    sidebarCollapsed, setSidebarCollapsed, toggleSidebarCollapsed, sidebarWidth: railWidth, setSidebarWidth: onRailResize,
    compactScreen, mobileSidebarOpen, setMobileSidebarOpen,
  } = useConversationChrome();
  const railCollapsed = compactScreen ? !mobileSidebarOpen : sidebarCollapsed;
  const closeRail = () => {
    if (compactScreen) setMobileSidebarOpen(false);
    else setSidebarCollapsed(true);
  };
  const toggleRail = () => {
    if (compactScreen) setMobileSidebarOpen((open) => !open);
    else toggleSidebarCollapsed();
  };
  const [theme, setTheme] = useState<ThemeMode>("dark");
  const showSettings = false;
  const [paletteOpen, setPaletteOpen] = useState(false);
  const sideChat = useRunSideChatController(runId && !isDraft(runId) && !isCtfDemoRunId(runId) ? runId : "");
  const urlRuntimeView = (): ArtifactView | null => {
    if (typeof window === "undefined") return null;
    const view = new URLSearchParams(window.location.search).get("view");
    return view && RUNTIME_TABS.some((tab) => tab.view === view) ? (view as ArtifactView) : null;
  };
  const [artifactOpen, setArtifactOpen] = useState(() => {
    if (typeof window === "undefined") return false;
    const concreteRunId = workspaceMode === "pentest"
      ? (new URLSearchParams(window.location.search).get("run") || "")
      : runIdFromPath();
    let sessionOpen = false;
    try { sessionOpen = window.sessionStorage.getItem(ARTIFACT_OPEN_STORAGE_KEY) === "1"; }
    catch {  }
    return shouldRestoreRuntimePanelOpen({
      concreteRunId,
      hasUrlRuntimeView: Boolean(urlRuntimeView()),
      sessionOpen,
    });
  });
  const [reportOpen, setReportOpen] = useState(false);
  useEffect(() => {
    if (!shouldResetRuntimePanelForRun(runId)) return;
    setArtifactOpen(false);
    setReportOpen(false);
  }, [runId]);
  const previousReportOpen = useRef(reportOpen);
  useEffect(() => {
    if (previousReportOpen.current === reportOpen) return;
    previousReportOpen.current = reportOpen;
    if (workspaceMode !== "pentest" || !runId) return;
    const params = new URLSearchParams(window.location.search);
    if (reportOpen && !isDraft(runId)) params.set("view", "report");
    else if (params.get("view") === "report") params.delete("view");
    const query = params.toString();
    const url = `/pentest${query ? `?${query}` : ""}`;
    if (`${window.location.pathname}${window.location.search}` !== url) {
      window.history.replaceState({}, "", url);
    }
  }, [reportOpen, runId, workspaceMode]);
  const [artifactView, setArtifactViewState] = useState<ArtifactView>(() => {
    if (typeof window === "undefined") return "timeline";
    const fromUrl = urlRuntimeView();
    if (fromUrl) return fromUrl;
    try {
      const session = window.sessionStorage.getItem(ARTIFACT_VIEW_STORAGE_KEY);
      if (session && RUNTIME_TABS.some((tab) => tab.view === session)) return session as ArtifactView;
    } catch {  }
    try {
      const saved = window.localStorage.getItem(ARTIFACT_VIEW_STORAGE_KEY);
      if (saved && RUNTIME_TABS.some((tab) => tab.view === saved)) return saved as ArtifactView;
    } catch {  }
    return "timeline";
  });
  const [hotkeySwitch, setHotkeySwitch] = useState(false);
  const setArtifactView = useCallback((view: ArtifactView) => {
    setArtifactViewState(view);
    try { window.localStorage.setItem(ARTIFACT_VIEW_STORAGE_KEY, view); } catch {  }
    try { window.sessionStorage.setItem(ARTIFACT_VIEW_STORAGE_KEY, view); } catch {  }
  }, []);
  useEffect(() => {
    if (workspaceMode === "pentest" && artifactView === "findings") setArtifactView("evidence");
  }, [artifactView, setArtifactView, workspaceMode]);
  useEffect(() => {
    try { window.sessionStorage.setItem(ARTIFACT_OPEN_STORAGE_KEY, artifactOpen ? "1" : "0"); }
    catch {  }
  }, [artifactOpen]);
  const [artifactWidth, setArtifactWidth] = useState(() => artifactWidthDefault(typeof window !== "undefined" ? window.innerWidth : 1280));
  const [artifactWidthReady, setArtifactWidthReady] = useState(false);
  const [focusedWorker, setFocusedWorker] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedSpeaker, setFocusedSpeaker] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedPoc, setFocusedPoc] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedFact, setFocusedFact] = useState<{ seq: number; nonce: number } | null>(null);
  useEffect(() => {
    if (!deck.started || deck.runId !== runId || (deck.mode !== "ctf" && deck.mode !== "pentest") || deck.mode !== workspaceMode) return;
    if (workspaceMode === "pentest" && requestedRunId !== runId) return;
    const params = new URLSearchParams(window.location.search);
    const view = params.get("view");
    if (!view || view === "collaboration") return;
    if (view === "report") {
      if (workspaceMode === "pentest") { setArtifactOpen(false); setReportOpen(true); }
      return;
    }
    if (RUNTIME_TABS.some((tab) => tab.view === view)) {
      setReportOpen(false);
      setArtifactView(view as ArtifactView);
      setArtifactOpen(true);
    }
    const nonce = Date.now();
    const worker = params.get("worker"); if (worker) setFocusedWorker({ id: worker, nonce });
    const speaker = params.get("speaker"); if (speaker) setFocusedSpeaker({ id: speaker, nonce });
    const poc = params.get("poc"); if (poc) setFocusedPoc({ id: poc, nonce });
    const fact = Number(params.get("fact")); if (params.has("fact") && Number.isFinite(fact)) setFocusedFact({ seq: fact, nonce });
    for (const key of ["view", "worker", "speaker", "poc", "fact"]) params.delete(key);
    const query = params.toString();
    window.history.replaceState({}, "", `${window.location.pathname}${query ? `?${query}` : ""}${window.location.hash}`);
  }, [deck.started, deck.runId, deck.mode, requestedRunId, requestedSearch, runId, setArtifactView, workspaceMode]);
  const [winW, setWinW] = useState(typeof window !== "undefined" ? window.innerWidth : 1280);
  const [listBump, setListBump] = useState(0);
  const pushToast = useCallback((input: {
    msg: string; variant?: "success" | "error" | "info"; undo?: () => void; icon?: IconName;
    action?: { label: string; onClick: () => void };
  }) => {
    const action = input.action ?? (input.undo ? { label: t("rail.toast.undo"), onClick: input.undo } : undefined);
    toast(input.msg, {
      tone: input.variant === "error" ? "danger" : input.variant === "success" ? "success" : "neutral",
      icon: input.icon,
      duration: action ? 6500 : 3500,
      action,
    });
  }, [t]);
  const shellRef = useRef<HTMLDivElement | null>(null);
  const openWorkerSettings = useCallback(() => {
    const returnTo = `${window.location.pathname}${window.location.search}`;
    let preferLocalPi = deck.driverKind === "local_pi";
    if (!preferLocalPi && !deck.started && workspaceMode === "ctf") {
      try {
        preferLocalPi = window.localStorage.getItem("muteki.executionKind") === "local_pi";
      } catch {
        preferLocalPi = false;
      }
    }
    const sectionQuery = preferLocalPi && workspaceMode === "ctf" ? "section=local_pi&" : "";
    router.push(`${workspaceMode === "pentest" ? "/pentest/workers" : "/ctf/workers"}?${sectionQuery}return=${encodeURIComponent(returnTo)}`);
  }, [deck.driverKind, deck.started, router, workspaceMode]);
  const openWorkerSettingsSection = useCallback((section: string) => {
    const returnTo = `${window.location.pathname}${window.location.search}`;
    const workers = workspaceMode === "pentest" ? "/pentest/workers" : "/ctf/workers";
    router.push(`${workers}?section=${section}&return=${encodeURIComponent(returnTo)}`);
  }, [router, workspaceMode]);
  const [attachments, setAttachments] = useState<SavedFile[]>([]);
  const [railDeleteConfirm, setRailDeleteConfirm] = useState<
    | { kind: "folder"; id: string; label: string }
    | { kind: "run"; id: string; label: string }
    | null
  >(null);
  const [railDeleteBusy, setRailDeleteBusy] = useState(false);
  const [killConfirm, setKillConfirm] = useState<{ id: string; label: string } | null>(null);
  const [killConfirmBusy, setKillConfirmBusy] = useState(false);

  const allRuns = useRunList(4000, listBump);
  const [demoRuns] = useState(ctfDemoRunSummaries);
  const demoListed = workspaceMode === "ctf" && (searchParams.get("demo") === "1" || isCtfDemoRunId(runId));
  const runs = useMemo(() => [
    ...(demoListed ? demoRuns : []),
    ...allRuns.filter((item) => (item.mode || "ctf") === workspaceMode),
  ], [allRuns, demoListed, demoRuns, workspaceMode]);
  const allFolders = useFolders(8000, listBump);
  const folders = useMemo(() => workspaceMode === "pentest" ? [] : allFolders, [workspaceMode, allFolders]);
  const bump = () => setListBump((n) => n + 1);

  const toastFail = () => pushToast({ msg: t("toast.actionFailed"), variant: "error" });

  useEffect(() => {
    const update = () => setTheme(readSavedTheme());
    update();
    return subscribeThemePreference(update);
  }, []);

  const toggleTheme = () => setThemePreference(theme === "dark" ? "light" : "dark");

  const demoNotice = useCallback(() => pushToast({ msg: CTF_DEMO_NOTICE, variant: "info", icon: "info" }), [pushToast]);
  const onCommand = useCallback(
    async (target: string, action: string, text: string,
           opts?: ControlCommandOpts): Promise<boolean> => {
      if (isCtfDemoRunId(runIdRef.current)) { demoNotice(); return false; }
      try {
        const result = await sendHitl(target, action, text, opts);
        if (action === "writeup") {
          pushToast({ msg: t("toast.writeupRequested"), variant: "info", icon: "pencil" });
        } else if (["hint", "focus", "redirect", "directive", "correction"].includes(action)) {
          const effect = result?.effect && typeof result.effect === "object" ? result.effect : {};
          const delivered = Number(effect.delivered ?? 0);
          const workers = Number(effect.workers ?? 0);
          const msg = effect.kind === "live_guidance_delivered" ? t("toast.guidanceDelivered", { target: target.replace(/^(solver|worker):/, "") })
            : effect.kind === "live_guidance_broadcast"
              ? (delivered > 0 ? t("toast.guidanceBroadcast", { delivered, workers })
                : workers === 0 ? t("toast.guidanceNoWorker") : t("toast.guidanceUnacked"))
              : t("toast.guidanceQueued");
          pushToast({ msg, variant: "success", icon: "check" });
        }
        return true;
      } catch (error) {
        const detail = error instanceof Error ? error.message : "";
        pushToast({ msg: detail || t("toast.actionFailed"), variant: "error" });
        return false;
      }
    },
    [sendHitl, pushToast, t, demoNotice]
  );

  const onRequestProgress = useCallback(async (): Promise<boolean> => {
    if (isCtfDemoRunId(runIdRef.current)) { demoNotice(); return false; }
    try {
      const result = await requestProgress();
      pushToast({
        msg: t(result.status === "unchanged"
          ? "toast.progressUnchanged"
          : "toast.progressRequested"),
        variant: "info",
        icon: "radio",
      });
      return true;
    } catch {
      pushToast({ msg: t("toast.actionFailed"), variant: "error" });
      return false;
    }
  }, [pushToast, requestProgress, t, demoNotice]);

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
    }
  }, [artifactWidth, artifactWidthReady]);

  const onArtifactResize = useCallback((width: number) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    setArtifactWidth(clampArtifactWidth(width, viewport));
  }, []);

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

  const toggleSideChat = sideChat.toggle;
  const sideChatAvailable = Boolean(sideChat.runId) && deck.started;
  useEffect(() => {
    if (!sideChatAvailable) return;
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && (e.key === "/" || e.key === "?")) {
        if (showSettings || paletteOpen) return;
        e.preventDefault();
        toggleSideChat();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [showSettings, paletteOpen, sideChatAvailable, toggleSideChat]);

  useEffect(() => {
    if (!artifactOpen || showSettings || paletteOpen || reportsSurface) return;
    const onEsc = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      if ((e.target as Element | null)?.closest(".react-flow, .collab-shell, .run-side-slot")) return;
      e.preventDefault();
      setHotkeySwitch(false);
      setArtifactOpen(false);
    };
    window.addEventListener("keydown", onEsc);
    return () => window.removeEventListener("keydown", onEsc);
  }, [artifactOpen, showSettings, paletteOpen, reportsSurface]);

  useEffect(() => {
    if (!deck.started || showSettings || paletteOpen || reportsSurface) return;
    const isTyping = (el: EventTarget | Element | null): boolean => {
      const node = el as HTMLElement | null;
      if (!node || typeof node.tagName !== "string") return false;
      if (node.closest?.(".react-flow, .collab-shell")) return true;
      const tag = node.tagName.toLowerCase();
      return tag === "input" || tag === "textarea" || tag === "select" || node.isContentEditable;
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.metaKey || e.altKey || e.shiftKey || e.isComposing) return;
      if (isTyping(e.target) || isTyping(document.activeElement)) return;
      const view = PANEL_HOTKEYS[e.key.toLowerCase()];
      if (!view) return;
      e.preventDefault();
      openArtifact(view, true);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // openArtifact is stable enough for this scope; re-bind only on the gates.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deck.started, showSettings, paletteOpen, reportsSurface]);

  useEffect(() => {
    if (deck.challengeName) setListBump((n) => n + 1);
  }, [deck.challengeName]);

  const running = isRunActive(deck);
  const [stagedRunId, setStagedRunId] = useState("");
  const [promotedDraft, setPromotedDraft] = useState<{ sourceId: string; runId: string }>();
  const composerDraftId = promotedDraft?.runId === runId ? promotedDraft.sourceId : runId;
  const [pendingDispatches, setPendingDispatches] = useState<ReadonlyMap<string, string>>(() => new Map());
  const [acceptedDispatch, setAcceptedDispatch] = useState<{ runId: string; prompt: string }>();
  const awaitingRunEvents = acceptedDispatch?.runId === runId && !deck.started ? acceptedDispatch : undefined;
  const pendingDispatchesRef = useRef(pendingDispatches);
  const updatePendingDispatches = (update: (pending: Map<string, string>) => void) => {
    const next = new Map(pendingDispatchesRef.current);
    update(next);
    pendingDispatchesRef.current = next;
    setPendingDispatches(next);
  };
  const backendDraft = allRuns.some((item) => item.run_id === runId && item.status === "draft");
  const loading = !isDraft(runId) && !deck.started && stagedRunId !== runId && !backendDraft;
  useFlagPulse(shellRef, deck.flags.length);
  const addFiles = async (files: FileList | File[]): Promise<number | void> => {
    if (!files || files.length === 0) return;
    let id = runId;
    if (!id || isDraft(id)) {
      try {
        id = await newRun(workspaceMode);
      } catch (error) {
        pushToast({ msg: error instanceof Error ? error.message : "创建任务失败", variant: "error" });
        return 0;
      }
      if (runIdRef.current !== runId) return;
      setPromotedDraft({ sourceId: runId, runId: id });
      setStagedRunId(id);
      setRunId(id);
      runIdRef.current = id;
    }
    const saved = await uploadFiles(id, files);
    if (runIdRef.current !== id) return;
    if (saved.length) setAttachments((prev) => [...prev, ...saved]);
    return saved.length;
  };
  const removeFile = (path: string) =>
    setAttachments((prev) => prev.filter((f) => f.path !== path));

  const dispatch = async (prompt: string, opts?: DispatchOpts) => {
    const offline = opts?.mode === "pentest" ? false : opts ? !opts.webSearch : false;
    const challenge: Record<string, unknown> = {
      description: prompt,
      attachments: attachments.map((f) => f.path),
    };
    if (opts?.mode === "pentest") {
      challenge.mode = "pentest";
      challenge.report_goal_mode = opts.reportGoalMode || "automatic";
      if (opts.reportGoalMode === "count") {
        challenge.expected_findings = opts.expectedFindings;
      }
      if (opts.goal) challenge.goal = opts.goal;
      if (opts.scope) challenge.scope = opts.scope;
    } else {
      if (opts?.flagFormat === "brace") {
        challenge.flag_format = "flag{...}";
      } else if (opts?.flagFormat === "token") {
        challenge.flag_format = "token";
      } else if (opts?.flagFormat === "custom" && opts.flagWrapper?.trim()) {
        challenge.flag_format_wrapper = opts.flagWrapper.trim();
      }
      if (opts?.collect) {
        challenge.multi_flag = true;
        if (opts.collectCount && opts.collectCount > 0) {
          challenge.expected_flags = opts.collectCount;
        }
      }
    }
    const worker_backend = opts?.containerMode ? "container" : "local";
    const runOverrides: Record<string, unknown> = {};
    if (opts?.wallClockBudget != null) runOverrides.wall_clock_budget = opts.wallClockBudget;
    if (opts?.maxTotalWorkers != null) runOverrides.max_total_workers = opts.maxTotalWorkers;
    if (opts?.costBudgetUsd != null) runOverrides.cost_budget_usd = opts.costBudgetUsd;
    const isLocalPi = opts?.mode !== "pentest" && opts?.executionKind === "local_pi";
    const startBody = {
      kind: isLocalPi ? "local_pi" : "swarm",
      prompt,
      offline,
      allow_operator_input: opts?.allowOperatorInput ?? false,
      challenge,
      worker_backend,
      ...(isLocalPi ? {
        local_pi_config: {
          ...(opts?.localPiModel ? { model: opts.localPiModel } : {}),
          ...(opts?.localPiAccountId ? { account_id: opts.localPiAccountId } : {}),
          ...(opts?.localPiEffort ? { effort: opts.localPiEffort } : {}),
          ...(opts?.localPiSkills ? { skills: opts.localPiSkills } : {}),
        },
      } : {}),
      ...(!isLocalPi && opts?.dispatchTemplateId ? { dispatch_template_id: opts.dispatchTemplateId } : {}),
      ...(opts?.mode === "pentest" && opts.productionGuard ? { production_guard: opts.productionGuard } : {}),
      ...runOverrides,
    };
    const sourceId = runId;
    if (awaitingRunEvents || pendingDispatchesRef.current.has(sourceId)) return false;
    let id = sourceId;
    updatePendingDispatches((pending) => pending.set(sourceId, prompt));
    try {
      if (!id || isDraft(id)) {
        id = await newRun(workspaceMode);
        updatePendingDispatches((pending) => {
          pending.delete(sourceId);
          pending.set(id, prompt);
        });
        if (runIdRef.current === sourceId) {
          setPromotedDraft({ sourceId, runId: id });
          runIdRef.current = id;
          setRunId(id);
        }
      }
      await start(startBody, id);
      const stillSelected = runIdRef.current === id;
      if (stillSelected) {
        setAcceptedDispatch({ runId: id, prompt });
        setAttachments([]);
        setStagedRunId((staged) => staged === id ? "" : staged);
      }
      setListBump((n) => n + 1);
      return stillSelected;
    } catch (err) {
      if (id && !isDraft(id) && runIdRef.current === id) setStagedRunId(id);
      const detail = err instanceof Error ? err.message : "";
      const plannerRequired = err instanceof RunStartError
        && err.payload.code === "run.dispatch.planner_required";
      pushToast({
        msg: detail ? `${t("toast.dispatchFailed")}：${detail}` : t("toast.dispatchFailed"),
        variant: "error",
        action: plannerRequired
          ? { label: t("composer.configurePlanner"), onClick: () => openWorkerSettingsSection("models") }
          : undefined,
      });
      return false;
    } finally {
      updatePendingDispatches((pending) => pending.delete(id));
    }
  };

  const onNewSolve = () => {
    setArtifactOpen(false);
    setReportOpen(false);
    if (workspaceMode === "pentest") {
      const url = new URL(window.location.href);
      url.searchParams.delete("report_seq");
      url.searchParams.delete("finding");
      window.history.replaceState(window.history.state ?? {}, "", `${url.pathname}${url.search}${url.hash}`);
    }
    if (reportsSurface && workspaceMode === "ctf") router.push("/ctf");
    setAttachments([]);
    const id = newDraftId();
    runIdRef.current = id;
    setRunId(id);
    if (compactScreen) closeRail();
  };

  const onSelectRun = (id: string) => {
    if (compactScreen) closeRail();
    if (reportsSurface && workspaceMode === "ctf") router.push(conversationHref(id, workspaceMode));
    if (id === runId) return;
    setArtifactOpen(false);
    setReportOpen(false);
    if (workspaceMode === "pentest") {
      const url = new URL(window.location.href);
      url.searchParams.delete("report_seq");
      url.searchParams.delete("finding");
      window.history.replaceState(window.history.state ?? {}, "", `${url.pathname}${url.search}${url.hash}`);
    }
    setAttachments([]);
    runIdRef.current = id;
    setRunId(id);
  };

  const onRailAction = async (a:
    | { kind: "pin"; runId: string; pinned: boolean }
    | { kind: "archive"; runId: string; archived: boolean }
    | { kind: "rename"; runId: string; name: string }
    | { kind: "delete"; runId: string }
    | { kind: "move"; runId: string; folderId: string | null }
    | { kind: "reorder"; runIds: string[]; pinned: boolean }
    | { kind: "newFolder" }
    | { kind: "renameFolder"; folderId: string; name: string }
    | { kind: "deleteFolder"; folderId: string }
    | { kind: "operatorLabel"; runId: string; label: OperatorTriageLabel | null }
  ) => {
    if (("runId" in a && isCtfDemoRunId(a.runId)) || (a.kind === "reorder" && a.runIds.some(isCtfDemoRunId))) {
      demoNotice();
      return;
    }
    if (a.kind === "operatorLabel") {
      const ok = await patchRun(a.runId, { operator_label: a.label });
      bump();
      if (!ok) toastFail();
      return;
    }
    if (a.kind === "reorder") {
      const maxOrder = Math.max(a.runIds.length, ...runs.map((r) => r.order || 0));
      const pinnedAt = Date.now() / 1000;
      for (const [index, runId] of a.runIds.entries()) {
        const run = runs.find((item) => item.run_id === runId);
        if (!run) { toastFail(); break; }
        const patch = a.pinned
          ? { pinned: true, now: pinnedAt - index * 0.001 }
          : { order: maxOrder - index };
        if (!a.pinned && run.order === patch.order) continue;
        if (!await patchRun(runId, patch)) { toastFail(); break; }
      }
      bump();
    } else if (a.kind === "move") {
      const ok = await patchRun(a.runId, { folder_id: a.folderId });
      bump();
      if (!ok) toastFail();
    } else if (a.kind === "newFolder") {
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
      const folder = folders.find((item) => item.id === a.folderId);
      setRailDeleteConfirm({ kind: "folder", id: a.folderId, label: folder?.name || a.folderId });
      return;
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
        pushToast({
          msg: t("rail.toast.archived"),
          variant: "info",
          undo: async () => { await patchRun(a.runId, { archived: false }); bump(); },
        });
      }
    } else if (a.kind === "delete") {
      const run = runs.find((item) => item.run_id === a.runId);
      setRailDeleteConfirm({ kind: "run", id: a.runId, label: run?.name || a.runId });
      return;
    }
  };

  const confirmRailDelete = async () => {
    const pending = railDeleteConfirm;
    if (!pending || railDeleteBusy) return;
    setRailDeleteBusy(true);
    try {
      if (pending.kind === "folder") {
        const ok = await deleteFolder(pending.id);
        bump();
        if (ok) pushToast({ msg: t("toast.folderDeleted"), variant: "success" });
        else toastFail();
        return;
      }
      const ok = await deleteRun(pending.id);
      if (ok && pending.id === runId) {
        setArtifactOpen(false);
            setAttachments([]);
        setRunId(newDraftId());
      }
      bump();
      if (ok) pushToast({ msg: t("toast.deleted"), variant: "success" });
      else toastFail();
    } finally {
      setRailDeleteBusy(false);
      setRailDeleteConfirm(null);
    }
  };

  const openCollaboration = (query?: Record<string, string>) => {
    if (!runId || isDraft(runId) || deck.driverKind === "local_pi") return;
    const params = new URLSearchParams(query);
    const search = params.toString();
    router.push(`${collabPagePath(runId)}${search ? `?${search}` : ""}`);
  };
  const openArtifact = (view: ArtifactView, instant = false) => {
    if (view === "collaboration") { openCollaboration(); return; }
    if (deck.driverKind === "local_pi" && view !== "timeline" && view !== "evidence" && view !== "usage") {
      return;
    }
    setHotkeySwitch(instant);
    setReportOpen(false);
    setArtifactView(view);
    setArtifactOpen(true);
  };
  const openReport = () => {
    setHotkeySwitch(false);
    setArtifactOpen(false);
    setReportOpen(true);
  };
  const openWriteup = useCallback((reportId?: string) => {
    setHotkeySwitch(false);
    setArtifactOpen(false);
    setReportOpen(false);
    if (reportId) {
      router.push(`/ctf/reports?report=${encodeURIComponent(reportId)}`);
      return;
    }
    if (runId && !isDraft(runId)) {
      router.push(`/ctf/reports?focusRun=${encodeURIComponent(runId)}`);
      return;
    }
    router.push("/ctf/reports");
  }, [router, runId]);
  const openFact = (seq: number) => {
    setHotkeySwitch(false);
    setReportOpen(false);
    setArtifactOpen(true);
    setArtifactView("evidence");
    setFocusedFact({ seq, nonce: Date.now() });
  };

  const onOpenWorker = (id: string) => {
    setFocusedWorker((prev) => ({ id, nonce: (prev?.nonce ?? 0) + 1 }));
    openArtifact("workers");
  };

  const onOpenAgent = (id: string) => openCollaboration({ agent: id });

  const onOpenKnowledge = (id: string) => openCollaboration({ k: id });

  const readPaletteAgents = useCallback(() => {
    const ids = workerIds(deck);
    const siblings = ids.map((id) => toWorkerIdentity(id, deck.lanes[id]));
    return ids.map((id, index) => {
      const lane = deck.lanes[id];
      return {
        id,
        title: workerDisplayName(id, siblings[index], siblings).title,
        engine: workerEngine(id, lane?.engine),
        status: lane?.status || "",
      };
    });
  }, [deck]);

  const onOpenSpeakerTimeline = (id: string) => {
    setFocusedSpeaker((prev) => ({ id, nonce: (prev?.nonce ?? 0) + 1 }));
    openArtifact("timeline");
  };

  const onSpawnWorker = async (engine?: string) => {
    if (!runId) return;
    if (isCtfDemoRunId(runId)) { demoNotice(); return; }
    const ok = await spawnWorker(runId, engine);
    if (ok === null) return;
    if (ok) pushToast({ msg: t("toast.workerSpawned"), variant: "success", icon: "cpu" });
    else toastFail();
  };
  const onKillWorker = (solverId: string) => {
    if (!runId) return;
    if (isCtfDemoRunId(runId)) { demoNotice(); return; }
    const siblings = workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id]));
    const label = workerDisplayName(solverId, toWorkerIdentity(solverId, deck.lanes[solverId]), siblings).title;
    setKillConfirm({ id: solverId, label });
  };
  const confirmKillWorker = async () => {
    const pending = killConfirm;
    if (!pending || killConfirmBusy || !runId) return;
    setKillConfirmBusy(true);
    try {
      const ok = await killWorker(runId, pending.id);
      if (ok) pushToast({ msg: t("toast.workerKilled"), variant: "info", icon: "cpu" });
      else toastFail();
    } finally {
      setKillConfirmBusy(false);
      setKillConfirm(null);
    }
  };
  const onOpenWorkspace = () => {
    if (isCtfDemoRunId(runId)) { demoNotice(); return; }
    if (runId && !isDraft(runId)) openWorkspace(runId);
  };
  const onResolve = async (text?: string): Promise<boolean> => {
    if (isCtfDemoRunId(runId)) { demoNotice(); return false; }
    try { await resolve(text); return true; }
    catch (error) { pushToast({ msg: error instanceof Error ? error.message : String(error), variant: "error" }); return false; }
  };
  const onFollowup = async (
    kind: "ask" | "writeup",
    question: string,
    opts?: { templateId?: string; customPrompt?: string },
  ) => {
    if (isCtfDemoRunId(runId)) { demoNotice(); return { ok: false }; }
    try { return await startFollowup(kind, question, opts); }
    catch (error) { pushToast({ msg: error instanceof Error ? error.message : String(error), variant: "error" }); return { ok: false }; }
  };
  const onCancelFollowup = async (id: string) => {
    if (isCtfDemoRunId(runId)) { demoNotice(); return; }
    try { await cancelFollowup(id); }
    catch (error) { pushToast({ msg: error instanceof Error ? error.message : String(error), variant: "error" }); }
  };
  const onExpireDirective = async (id: string) => {
    await onCommand("global", "expire_context", "", { contextId: id, commandId: crypto.randomUUID(), expectedGeneration: deck.controlGeneration });
  };
  const onToggleFreezeWorker = (id: string, paused: boolean) => { void onCommand(`solver:${id}`, paused ? "thaw" : "freeze", ""); };
  const onDecideGuardApproval = useCallback(async (
    approvalId: string,
    action: GuardApprovalDecisionAction,
    note?: string,
  ): Promise<boolean> => {
    if (!runId || isDraft(runId) || isCtfDemoRunId(runId)) return false;
    try {
      await decideRunGuardApproval(runId, approvalId, action, note);
      pushToast({
        msg: action === "allow_once"
          ? "已允许本次动作执行"
          : action === "deny_once"
            ? "已拒绝本次动作"
            : "已触发紧急停止 Run",
        variant: action === "emergency_stop" ? "error" : "success",
        icon: action === "emergency_stop" ? "shieldAlert" : "check",
      });
      void refreshGuardState();
      return true;
    } catch (error) {
      pushToast({
        msg: error instanceof Error ? error.message : t("toast.actionFailed"),
        variant: "error",
      });
      return false;
    }
  }, [runId, pushToast, refreshGuardState, t]);
  const onEmergencyStop = useCallback(async (): Promise<boolean> => {
    if (!runId || isDraft(runId) || isCtfDemoRunId(runId)) return false;
    const ok = await onCommand("global", "emergency_stop", "", { expectedGeneration: deck.controlGeneration });
    if (ok) {
      pushToast({
        msg: "已下发紧急停止指令：撤销授权并强杀 Worker",
        variant: "error",
        icon: "shieldAlert",
      });
      void refreshGuardState();
    }
    return ok;
  }, [runId, onCommand, deck.controlGeneration, pushToast, refreshGuardState]);

  return (
    <div ref={shellRef} className={`shell motion-root ${workspaceMode === "pentest" ? "pentest-workbench" : ""}`}>
      <a href="#main-conversation" className="skip-link">{t("a11y.skipToMain")}</a>
      <ThreadRail
        workspaceMode={workspaceMode}
        collapsed={railCollapsed}
        width={railWidth}
        runs={runs}
        folders={folders}
        activeRunId={reportsSurface ? "" : runId}
        draftActive={!reportsSurface && isDraft(runId)}
        onNew={reportsSurface && workspaceMode !== "ctf"
          ? () => router.push(workspaceMode === "pentest" ? "/pentest" : "/ctf")
          : onNewSolve}
        onSelect={reportsSurface && workspaceMode !== "ctf"
          ? (id: string) => router.push(conversationHref(id, workspaceMode))
          : onSelectRun}
        onOpenReports={reportsSurface
          ? () => {}
          : () => router.push(workspaceMode === "pentest" ? "/pentest/reports" : "/ctf/reports")}
        reportsActive={reportsSurface}
        onAction={onRailAction}
        onResize={onRailResize}
        onOpenSettings={openWorkerSettings}
        onClose={closeRail}
      />
      {!railCollapsed && <button type="button" className="rail-scrim" onClick={closeRail} aria-label={t("rail.close")} />}
      <main id="main-conversation" className="main" aria-label={t("a11y.main")}>
        {reportsOpened && (
          <Activity mode={reportsSurface ? "visible" : "hidden"}>
            <Suspense fallback={<PanelLoading />}>
              {workspaceMode === "pentest" ? (
                <PentestReportsPage
                  runs={runs}
                  active={reportsSurface}
                  onOpenRun={(id) => router.push(conversationHref(id, "pentest"))}
                />
              ) : (
                <ReportsPage
                  workspaceMode="ctf"
                  runs={runs}
                  active={reportsSurface}
                  onOpenRun={(id) => onSelectRun(id)}
                />
              )}
            </Suspense>
          </Activity>
        )}
        {conversationOpened && (
          <Activity mode={reportsSurface ? "hidden" : "visible"}>
            <Suspense fallback={<PanelLoading />}>
              <Conversation
                workspaceMode={workspaceMode}
                deck={deck}
                operatorLabel={runs.find((r) => r.run_id === runId)?.operator_label ?? deck.operatorLabel}
                onUpdateOperatorLabel={(label) => {
                  if (runId && !isDraft(runId)) void onRailAction({ kind: "operatorLabel", runId, label });
                }}
                chatThreadId={runs.find((r) => r.run_id === runId)?.chat_thread_id ?? undefined}
                runTitle={runs.find((r) => r.run_id === runId)?.name || undefined}
                running={running}
                loading={loading}
                pendingDispatchPrompt={pendingDispatches.get(runId) ?? awaitingRunEvents?.prompt}
                dispatchAccepted={Boolean(awaitingRunEvents)}
                composerDraftId={composerDraftId}
                replayEmpty={replayEmpty}
                connected={connected}
                onCommand={onCommand}
                onDecideGuardApproval={onDecideGuardApproval}
                onEmergencyStop={onEmergencyStop}
                onRequestProgress={onRequestProgress}
                onResolve={onResolve}
                onFollowup={onFollowup} onCancelFollowup={onCancelFollowup}
                onExpireDirective={onExpireDirective} onToggleFreezeWorker={onToggleFreezeWorker}
                onOpenWriteup={workspaceMode === "ctf" ? openWriteup : undefined}
                onDispatch={dispatch}
                attachments={attachments}
                onAddFiles={addFiles}
                onRemoveFile={removeFile}
                artifactOpen={artifactOpen}
                artifactView={artifactView}
                onOpenArtifact={(view) => openArtifact(view)}
                onShowConversation={() => { setHotkeySwitch(false); setArtifactOpen(false); setReportOpen(false); }}
                reportOpen={reportOpen}
                reportPanel={workspaceMode === "pentest" && !isDraft(runId) ? (
                  <Suspense fallback={<PanelLoading />}>
                    <PentestReport key={runId} runId={runId} runTitle={deck.challengeName} startedAt={deck.startedAt} finishedAt={deck.finishedAt} onOpenFact={openFact} />
                  </Suspense>
                ) : undefined}
                onOpenReport={workspaceMode === "pentest" ? openReport : undefined}
                runtimePanel={(
                  <RuntimeArtifactPanel
                    runMode={workspaceMode}
                    runId={runId}
                    open={artifactOpen}
                    width={artifactWidth}
                    view={artifactView}
                    deck={deck}
                    running={running}
                    loading={loading}
                    hotkeySwitch={hotkeySwitch}
                    onView={(next) => { setHotkeySwitch(false); setArtifactView(next); }}
                    onClose={() => { setHotkeySwitch(false); setArtifactOpen(false); }}
                    onResize={onArtifactResize}
                    minWidth={ARTIFACT_WIDTH_MIN}
                    maxWidth={artifactWidthMax(winW)}
                    defaultWidth={artifactWidthDefault(winW)}
                    onSpawnWorker={onSpawnWorker}
                    onKillWorker={onKillWorker}
                    focusWorker={focusedWorker}
                    focusPoc={focusedPoc}
                    focusFact={focusedFact}
                    focusSpeaker={focusedSpeaker}
                    onOpenSpeakerTimeline={onOpenSpeakerTimeline}
                    onOpenAgent={onOpenAgent}
                    onOpenFact={openFact}
                    onOpenReport={workspaceMode === "pentest" ? openReport : undefined}
                    onAdoptReview={(text) => { void onCommand("global", "directive", text); }}
                    workspaceMode
                  />
                )}
                onToggleRail={toggleRail}
                theme={theme}
                onToggleTheme={toggleTheme}
                onSpawnWorker={onSpawnWorker}
                onKillWorker={onKillWorker}
                onOpenWorker={onOpenWorker}
                onOpenAgent={onOpenAgent}
                onOpenKnowledge={onOpenKnowledge}
                onOpenWorkspace={onOpenWorkspace}
                onHitlAnswered={() => pushToast({ msg: t("hitl.answered"), variant: "success" })}
                onOpenSideChat={sideChatAvailable ? (question) => void sideChat.open({ question }) : undefined}
                sideChat={sideChatAvailable ? {
                  open: sideChat.layout.open,
                  side: sideChat.layout.side,
                  maximized: sideChat.layout.maximized,
                  width: sideChat.layout.width,
                  onAskSelection: (quote) => void sideChat.open({ quote }),
                  node: <RunSideChatColumn controller={sideChat} run={{
                    runId,
                    mode: workspaceMode,
                    title: deck.challengeName || runs.find((r) => r.run_id === runId)?.name || "Run",
                    status: runSideChatStatus(deck, running),
                  }} />,
                } : undefined}
              />
            </Suspense>
          </Activity>
        )}
      </main>
      <CommandPalette onOpenWriteup={deck.hasWriteup && workspaceMode === "ctf" ? () => openWriteup() : undefined}
        workspaceMode={workspaceMode}
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        started={deck.started}
        running={running}
        runs={runs}
        activeRunId={runId}
        onNewSolve={reportsSurface && workspaceMode !== "ctf"
          ? () => router.push(workspaceMode === "pentest" ? "/pentest" : "/ctf")
          : onNewSolve}
        onOpenArtifact={(view) => openArtifact(view)}
        onSelectRun={reportsSurface && workspaceMode !== "ctf"
          ? (id: string) => router.push(conversationHref(id, workspaceMode))
          : onSelectRun}
        onSpawnWorker={onSpawnWorker}
        onOpenSettings={openWorkerSettings}
        readAgents={readPaletteAgents}
        onOpenAgent={onOpenAgent}
        onOpenReport={workspaceMode === "pentest" ? openReport : undefined}
        onOpenSideChat={sideChatAvailable ? () => void sideChat.open() : undefined}
      />
      <Dialog
        open={Boolean(railDeleteConfirm)}
        onOpenChange={(open) => !open && setRailDeleteConfirm(null)}
        dismissable={!railDeleteBusy}
        className="cx-settings-dialog"
        size="sm"
        tone="danger"
        title={railDeleteConfirm?.kind === "folder" ? t("rail.folderDelete") : t("rail.menu.delete")}
        description={railDeleteConfirm?.kind === "folder" ? t("rail.confirmDeleteFolder") : t("rail.confirmDelete")}
        footer={<>
          <Button variant="ghost" onClick={() => setRailDeleteConfirm(null)} disabled={railDeleteBusy}>{t("dialog.cancel")}</Button>
          <Button variant="danger" agentConfirm="destructive" loading={railDeleteBusy} onClick={() => void confirmRailDelete()}>{t("rail.menu.delete")}</Button>
        </>}
      >
        {railDeleteConfirm ? <div className="action-dialog-summary"><span><strong>{railDeleteConfirm.label}</strong></span><code>{railDeleteConfirm.id}</code></div> : null}
      </Dialog>
      <Dialog
        open={Boolean(killConfirm)}
        onOpenChange={(open) => !open && setKillConfirm(null)}
        dismissable={!killConfirmBusy}
        className="cx-settings-dialog"
        size="sm"
        tone="danger"
        title={t("worker.confirmKillTitle")}
        description={t("worker.confirmKill")}
        footer={<>
          <Button variant="ghost" onClick={() => setKillConfirm(null)} disabled={killConfirmBusy}>{t("dialog.cancel")}</Button>
          <Button variant="danger" agentConfirm="destructive" loading={killConfirmBusy} onClick={() => void confirmKillWorker()}>{t("worker.killTitle")}</Button>
        </>}
      >
        {killConfirm ? <div className="action-dialog-summary"><span><strong>{killConfirm.label}</strong></span><code>{killConfirm.id}</code></div> : null}
      </Dialog>
    </div>
  );
}
