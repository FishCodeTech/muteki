"use client";

import { setThemePreference, subscribeThemePreference } from "@/lib/themePreference";
import { UsageDashboard } from "@/components/UsageDashboard";
import { ActivityTimeline } from "@/components/activity-timeline/ActivityTimeline";
import { countTimelineEvents } from "@/components/activity-timeline/fromDeck";
import { Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import type { ReactNode, KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from "react";
import { useRun, useRunList, useFolders, newRun, patchRun, deleteRun, uploadFiles, spawnWorker, killWorker, openWorkspace, createFolder, renameFolder, deleteFolder, SavedFile, apiFetch } from "@/lib/useRun";
import { useT, useLang } from "@/lib/i18n";
import { DeckState, isFactRetired, isRunActive, workerIds, swarmDigest } from "@/lib/events";
import { ThreadRail } from "@/components/ThreadRail";
import { Conversation } from "@/components/Conversation";
import type { ControlCommandOpts, DispatchOpts } from "@/components/Conversation";
import { CommandPalette } from "@/components/CommandPalette";
import { BtwPanel } from "@/components/BtwPanel";
import type { ArtifactView } from "@/lib/events";
import { clampRailWidth, RAIL_WIDTH_DEFAULT, RAIL_WIDTH_STORAGE_KEY } from "@/lib/railSizing";
import { useFlagPulse } from "@/lib/useFlagPulse";
import { Icon, type IconName } from "@/components/Icon";
import { WorkerLanes } from "@/components/WorkerLanes";
import { EvidenceChain } from "@/components/EvidenceChain";
import { PentestEvidence } from "@/components/PentestEvidence";
import { PentestReport } from "@/components/PentestReport";
import { Button, Modal, Skeleton, Tabs, toast } from "@heroui/react";
import { toWorkerIdentity, workerDisplayName, workerEngine } from "@/lib/workers";
import { applySelection, readSavedTheme, readSavedSelection } from "@/lib/palette-engine";
import { formatElapsed, toEpochMs } from "@/lib/format";
import { PANEL_HOTKEYS, RUNTIME_TABS, type RuntimeGroup } from "@/lib/runtimeTabs";
import { shouldResetRuntimePanelForRun, shouldRestoreRuntimePanelOpen } from "@/lib/draftRuntimePanel";
import { collabPagePath, readCollabUrlState } from "@/lib/collabUrlState";
import { directiveStatusLabel, pocStatusLabel, reviewKindLabel, reviewSeverityLabel } from "@/lib/statusLabels";

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

/**
 * Muteki Command Deck — conversation-first shell.
 *
 * The three pillars are NOT stacked. The spine is a ChatGPT/Claude conversation
 * (task dispatch is conversational); the spatial collaboration view maps every
 * active runtime agent and keeps knowledge attached to its producer. The run
 * summary lives in the home workspace instead of a separate statistics page.
 *
 *   ThreadRail (run list) │ Conversation (spine) │ RuntimeArtifactPanel (agent map)
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

// per-run routing (/run/<id>): the URL is derived from the active run id (drafts
// map to "/" since they have no backend row yet). Reading it on load restores a
// deep-linked conversation; the dynamic route app/run/[id]/page.tsx serves it.
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

// Live-ticking elapsed for the worker roster bar: ticks every 1s while the run is
// open, freezes at finishedAt once ended. Returns formatElapsed's M:SS format.
function useRuntimeElapsed(startedAt?: number, finishedAt?: number): string {
  const live = startedAt != null && finishedAt == null;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!live) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [live]);
  if (startedAt == null) return "";
  const end = finishedAt != null ? toEpochMs(finishedAt) : now;
  return formatElapsed(end - toEpochMs(startedAt));
}

function runtimeTabCount(view: ArtifactView, deck: DeckState, running: boolean, runMode: "ctf" | "pentest"): number | null {
  switch (view) {
    case "usage": return null;
    case "timeline": return countTimelineEvents(deck, running);
    case "workers": return Object.keys(deck.lanes).length;
    // The agent map is its own page; never a runtime tab here.
    case "collaboration": return null;
    case "evidence": return deck.mode === "pentest"
      ? deck.blackboard.facts.filter((fact) => fact.verified && !isFactRetired(fact) && fact.actor !== "origin").length
        + deck.blackboard.observations.filter((observation) => !observation.admitted).length
        + deck.blackboard.deadEnds.length
      : deck.blackboard.facts.length + deck.blackboard.deadEnds.length;
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
      onOpenAgent={onOpenAgent}
      phase={digest.phase}
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
    if (!runId || runId.startsWith("draft-")) {
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
      <Button type="button" className="evi-filter-btn" onClick={creds.retry}>{t("runtime.credentials.retry")}</Button>
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
          <Button type="button" className="artifact-chip report-link-btn" onClick={() => onOpenFact?.(row.seq!)}>#{row.seq}</Button>
        )}
      </div>
      <code className="artifact-code">{open ? row.value : "••••••••"}</code>
      <div className="report-links">
        <Button type="button" className="report-link-btn" onClick={() => setRevealed((prev) => ({ ...prev, [key]: !open }))}>
          {open ? t("runtime.credentials.hide") : t("runtime.credentials.show")}
        </Button>
        <Button type="button" className="report-link-btn" onClick={() => { void navigator.clipboard.writeText(row.value); setCopied(key); }}>
          {copied === key ? t("common.copied") : t("runtime.credentials.copy")}
        </Button>
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
                  <Button type="button" className="report-link-btn" onClick={() => onAdoptReview?.(action)}>{t("runtime.findings.adopt")}</Button>
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

function RuntimeArtifactPanel({ runId, open, width, view, deck, running, loading, onView, onClose, onResize, minWidth, maxWidth, defaultWidth, onSpawnWorker, onKillWorker, focusWorker, focusPoc, focusFact, focusSpeaker, onOpenSpeakerTimeline, onOpenAgent, onOpenFact, onOpenReport, onAdoptReview, workspaceMode = false, runMode = "ctf" }: {
  runId: string; open: boolean; width: number; view: ArtifactView; deck: DeckState; running: boolean; loading: boolean;
  onView: (view: ArtifactView) => void; onClose: () => void; onResize: (width: number) => void;
  minWidth: number; maxWidth: number; defaultWidth: number; onSpawnWorker: (engine?: string) => void; onKillWorker: (id: string) => void; focusWorker?: { id: string; nonce: number } | null; focusPoc?: { id: string; nonce: number } | null; focusFact?: { seq: number; nonce: number } | null; focusSpeaker?: { id: string; nonce: number } | null; onOpenSpeakerTimeline?: (id: string) => void; onOpenAgent?: (id: string) => void; onOpenFact?: (seq: number) => void; onOpenReport?: () => void; onAdoptReview?: (text: string) => void; workspaceMode?: boolean; runMode?: "ctf" | "pentest";
}) {
  const t = useRuntimeT();
  const credentialsNeeded = open && view === "credentials";
  const factGen = credentialsNeeded ? Math.max(0, ...deck.blackboard.facts.filter((fact) => fact.verified && typeof fact.factSeq === "number").map((fact) => fact.factSeq as number)) : 0;
  const creds = useRunCredentials(credentialsNeeded ? deck.runId : "", factGen);
  const workspaceActive = workspaceMode && open;
  const [resizing, setResizing] = useState(false);
  const [pentestEvidenceView, setPentestEvidenceView] = useState<"detail" | "context">("detail");
  const focusedFact = focusFact?.seq ? deck.blackboard.facts.find((item) => item.factSeq === focusFact.seq) : undefined;
  const focusedFactHasEvidence = !!focusedFact?.verified && !isFactRetired(focusedFact);
  useEffect(() => {
    if (runMode !== "pentest" || !focusFact?.seq) return;
    setPentestEvidenceView(focusedFactHasEvidence ? "detail" : "context");
  }, [runMode, focusFact?.seq, focusFact?.nonce, focusedFactHasEvidence]);
  const currentTab = RUNTIME_TABS.find((tab) => tab.view === view) ?? RUNTIME_TABS[0];
  const currentGroup = RUNTIME_GROUPS.find((group) => group.id === currentTab.group) ?? RUNTIME_GROUPS[0];
  const visibleTabs = RUNTIME_TABS.filter((tab) => runMode !== "pentest" || tab.view !== "findings");
  const groupTabs = visibleTabs.filter((tab) => tab.group === currentGroup.id);
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
  const runtimeNavigation = <div className="runtime-navigation">
    <Tabs selectedKey={currentGroup.id} onSelectionChange={(key) => onView(visibleTabs.find((tab) => tab.group === key)?.view ?? "timeline")}>
      <Tabs.List className="runtime-primary-nav" aria-label={t("runtime.nav.groups")}>{RUNTIME_GROUPS.map((group) => <Tabs.Tab id={group.id} key={group.id}><Icon name={group.icon} size={14} /><span>{t(group.key)}</span><Tabs.Indicator /></Tabs.Tab>)}</Tabs.List>
    </Tabs>
    <span className="runtime-nav-divider" aria-hidden="true" />
    <Tabs selectedKey={view} onSelectionChange={(key) => onView(key as ArtifactView)}>
      <Tabs.List className="runtime-view-nav" aria-label={t("runtime.nav.views")}>{groupTabs.map((tab) => { const count = runtimeTabCount(tab.view, deck, running, runMode); return <Tabs.Tab id={tab.view} key={tab.view}><Icon name={tab.icon} size={13} /><span>{t(runMode === "pentest" && tab.view === "routes" ? "runtime.pentest.process" : tab.key)}</span>{count !== null && <b>{count}</b>}<Tabs.Indicator /></Tabs.Tab>; })}</Tabs.List>
    </Tabs>
  </div>;
  if (runMode === "pentest" && workspaceActive) {
    const body = <div className="pentest-runtime-content">
        {loading ? <div className="panel-scroll"><PanelLoading rows={4} /></div>
          : view === "evidence" ? <Tabs className="pentest-evidence-views" selectedKey={pentestEvidenceView} onSelectionChange={(key) => setPentestEvidenceView(key as "detail" | "context")}>
              <Tabs.List className="runtime-view-nav" aria-label={t("runtime.evidence.views")}>
                <Tabs.Tab id="detail">{t("runtime.evidence.detail")}<Tabs.Indicator /></Tabs.Tab>
                <Tabs.Tab id="context">{t("runtime.evidence.context")}<Tabs.Indicator /></Tabs.Tab>
              </Tabs.List>
              <Tabs.Panel id="detail"><PentestEvidence deck={deck} runId={runId} focusFactSeq={focusFact?.seq} focusNonce={focusFact?.nonce} onOpenReport={onOpenReport} /></Tabs.Panel>
              <Tabs.Panel id="context"><EvidenceChain deck={deck} focusFactSeq={focusFact?.seq} focusNonce={focusFact?.nonce} /></Tabs.Panel>
            </Tabs>
          : view === "timeline" ? <ActivityTimeline deck={deck} running={running} focusSpeaker={focusSpeaker} />
          : view === "workers" ? <RuntimeWorkerView deck={deck} running={running} focusWorker={focusWorker} onSpawnWorker={onSpawnWorker} onKillWorker={onKillWorker} onOpenSpeakerTimeline={onOpenSpeakerTimeline} onOpenAgent={onOpenAgent} />
          : view === "routes" ? <div className="pentest-process"><h2>实际执行过程</h2><p>按共享图中真实记录的 Step 展示，不预设攻击路径。</p>{deck.blackboard.intents.length ? deck.blackboard.intents.map((intent, index) => <article key={intent.id}><span>{String(index + 1).padStart(2, "0")}</span><div><h3>{intent.summary || intent.goal}</h3><small>{intent.worker || intent.proposedBy || "Coordinator"} · {intent.status} · {intent.proposedTs ? new Date(toEpochMs(intent.proposedTs)).toLocaleString() : "—"}</small></div></article>) : <div className="panel-empty">尚无 Step 记录</div>}</div>
          : view === "credentials" ? <RuntimeCredentials creds={creds} onOpenFact={onOpenFact} />
          : view === "usage" ? <UsageDashboard runId={runId} />
          : <RuntimeDataView view={view} deck={deck} focusPoc={focusPoc} onAdoptReview={onAdoptReview} />}
      </div>;
    return <div className="artifact workspace-mode pentest-runtime" role="region" aria-label="渗透测试运行时">
      {runtimeNavigation}
      {body}
    </div>;
  }
  return <div className={`artifact t-page-slide ${workspaceActive ? "workspace-mode" : ""} ${resizing ? "resizing" : ""}`} style={workspaceActive ? undefined : { width: open ? width : 0, flexBasis: open ? width : 0 }} role="region" aria-label={t("a11y.artifact")}>
    {open && <>
      {!workspaceActive && <div className="artifact-resizer" role="separator" tabIndex={0} aria-label={t("art.resizeCanvas")} aria-valuemin={minWidth} aria-valuemax={maxWidth} aria-valuenow={width} onPointerDown={startResize} onKeyDown={resizeKey} onDoubleClick={() => onResize(defaultWidth)} />}
      {!workspaceActive && <div className="artifact-head runtime-head"><div className="runtime-titlemark"><Icon name={currentTab.icon} size={16} /></div><div className="runtime-heading"><span className="runtime-eyebrow">{t("runtime.title")}</span><div className="runtime-titleline"><strong>{t(currentTab.key)}</strong><span className={`runtime-state ${running ? "live" : deck.finished ? "complete" : "standby"}`}><span className="runtime-state-dot" />{t(running ? "runtime.status.live" : deck.finished ? "runtime.status.complete" : "runtime.status.standby")}</span></div><span className="runtime-context">{deck.challengeName || t("runtime.untitled")} · {deck.runId}</span></div><span className="spacer" /><Button className="x" onClick={onClose} aria-label={t("art.closeCanvas")}><Icon name="x" size={15} /></Button></div>}
      {runtimeNavigation}
      <div className="artifact-body"><div className="artifact-view t-skeleton-reveal" key={loading ? "loading" : view}>
          {loading ? <div className="panel-scroll"><PanelLoading rows={4} /></div>
            : view === "usage" ? <UsageDashboard runId={runId} />
            : view === "workers" ? <RuntimeWorkerView deck={deck} running={running} focusWorker={focusWorker} onSpawnWorker={onSpawnWorker} onKillWorker={onKillWorker} onOpenSpeakerTimeline={onOpenSpeakerTimeline} onOpenAgent={onOpenAgent} />
            : view === "timeline" ? <ActivityTimeline deck={deck} running={running} focusSpeaker={focusSpeaker} />
            : view === "evidence" ? <EvidenceChain deck={deck} focusFactSeq={focusFact?.seq} focusNonce={focusFact?.nonce} />
            : view === "credentials" ? <RuntimeCredentials creds={creds} onOpenFact={onOpenFact} />
            : <RuntimeDataView view={view} deck={deck} focusPoc={focusPoc} onAdoptReview={onAdoptReview} />}
        </div></div>
    </>}
  </div>;
}

export default function Page() {
  return <Suspense fallback={<PanelLoading />}><Deck workspaceMode="ctf" /></Suspense>;
}

export function PentestWorkbench() {
  return <Suspense fallback={<PanelLoading />}><Deck workspaceMode="pentest" /></Suspense>;
}

function Deck({ workspaceMode }: { workspaceMode: "ctf" | "pentest" }) {
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
  const router = useRouter();
  const searchParams = useSearchParams();
  const requestedRunId = workspaceMode === "pentest" ? searchParams.get("run") || "" : "";
  const requestedSearch = searchParams.toString();
  useEffect(() => {
    // Legacy deep link (/run/<id>?view=collaboration&agent=…) → the standalone
    // page, keeping the agent / knowledge / relation / scope selection.
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
    // seed from the URL (/run/<id> deep-link / refresh); else a fresh draft.
    setRunId(current => legacyRun || (isDraft(current) ? current : newDraftId()));
    // back/forward navigation between runs → re-read the path.
    const onPop = () => setRunId((workspaceMode === "pentest"
      ? new URLSearchParams(window.location.search).get("run") || ""
      : runIdFromPath()) || newDraftId());
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, [requestedRunId, router, workspaceMode]);
  // keep the URL in sync with the active run (replace, not push — internal run
  // switches shouldn't pile up history entries; deep-link/refresh still work).
  // Keep search so a draft→run promotion does not drop collaboration params.
  useEffect(() => {
    if (!runId) return;
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
  const { deck, connected, start, sendHitl, requestProgress, resolve } = useRun(runId);
  useEffect(() => {
    // Cross-mode redirects must keep view/focus handoff params so a remounted
    // deck can still open the panel the operator asked for (e.g. usage deep
    // links from the global UsageDashboard). Dropping them lands pentest on
    // the default evidence tab instead of the requested view.
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

  const [railCollapsed, setRailCollapsed] = useState(false);
  useEffect(() => {
    const narrow = window.matchMedia("(max-width: 1180px)");
    const syncRail = () => setRailCollapsed(narrow.matches);
    syncRail();
    narrow.addEventListener("change", syncRail);
    return () => narrow.removeEventListener("change", syncRail);
  }, []);
  const [railWidth, setRailWidth] = useState(RAIL_WIDTH_DEFAULT);
  const [railWidthReady, setRailWidthReady] = useState(false);
  const [theme, setTheme] = useState<ThemeMode>("dark");
  const [themeReady, setThemeReady] = useState(false);
  // Worker configuration now lives on a dedicated route. Keep this boolean as
  // the shared keyboard-layer guard; the old modal no longer mounts here.
  const showSettings = false;
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [btwOpen, setBtwOpen] = useState(false);
  // `?view=<runtime tab>` opens that panel on load — this is how the standalone
  // collaboration page hands off to worker lanes / timeline / evidence / PoC /
  // reports (with a matching `worker= / speaker= / fact= / poc= / report=`).
  const urlRuntimeView = (): ArtifactView | null => {
    if (typeof window === "undefined") return null;
    const view = new URLSearchParams(window.location.search).get("view");
    return view && RUNTIME_TABS.some((tab) => tab.view === view) ? (view as ArtifactView) : null;
  };
  const [artifactOpen, setArtifactOpen] = useState(() => {
    if (typeof window === "undefined") return false;
    // Draft landings (/ctf, /pentest without ?run=) must not inherit a prior
    // workspace's muteki.artifactOpen — that hides the challenge input with no
    // conversation/runtime toggle on drafts (FishCodeTech/muteki#18).
    const concreteRunId = workspaceMode === "pentest"
      ? (new URLSearchParams(window.location.search).get("run") || "")
      : runIdFromPath();
    let sessionOpen = false;
    try { sessionOpen = window.sessionStorage.getItem(ARTIFACT_OPEN_STORAGE_KEY) === "1"; }
    catch { /* storage unavailable */ }
    return shouldRestoreRuntimePanelOpen({
      concreteRunId,
      hasUrlRuntimeView: Boolean(urlRuntimeView()),
      sessionOpen,
    });
  });
  const [reportOpen, setReportOpen] = useState(false);
  // Route-direct draft entry (and draft id swaps) close the shared panel the same
  // way onNewSolve() does, so stale open state cannot survive into the input UI.
  useEffect(() => {
    if (!shouldResetRuntimePanelForRun(runId)) return;
    setArtifactOpen(false);
    setReportOpen(false);
  }, [runId]);
  const previousReportOpen = useRef(reportOpen);
  useEffect(() => {
    // Route changes must not let the previous panel erase a pending report URL.
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
  // Default runtime view is the activity stream (live event ledger); the last
  // picked view is remembered per browser so a reopen restores the operator's
  // working context. A `view=` in the search string wins.
  const [artifactView, setArtifactViewState] = useState<ArtifactView>(() => {
    if (typeof window === "undefined") return "timeline";
    const fromUrl = urlRuntimeView();
    if (fromUrl) return fromUrl;
    try {
      const session = window.sessionStorage.getItem(ARTIFACT_VIEW_STORAGE_KEY);
      if (session && RUNTIME_TABS.some((tab) => tab.view === session)) return session as ArtifactView;
    } catch { /* storage unavailable */ }
    try {
      const saved = window.localStorage.getItem(ARTIFACT_VIEW_STORAGE_KEY);
      if (saved && RUNTIME_TABS.some((tab) => tab.view === saved)) return saved as ArtifactView;
    } catch { /* storage unavailable */ }
    return "timeline";
  });
  const setArtifactView = useCallback((view: ArtifactView) => {
    setArtifactViewState(view);
    try { window.localStorage.setItem(ARTIFACT_VIEW_STORAGE_KEY, view); } catch { /* storage unavailable */ }
    try { window.sessionStorage.setItem(ARTIFACT_VIEW_STORAGE_KEY, view); } catch { /* storage unavailable */ }
  }, []);
  useEffect(() => {
    if (workspaceMode === "pentest" && artifactView === "findings") setArtifactView("evidence");
  }, [artifactView, setArtifactView, workspaceMode]);
  useEffect(() => {
    try { window.sessionStorage.setItem(ARTIFACT_OPEN_STORAGE_KEY, artifactOpen ? "1" : "0"); }
    catch { /* storage unavailable */ }
  }, [artifactOpen]);
  const [artifactWidth, setArtifactWidth] = useState(() => artifactWidthDefault(typeof window !== "undefined" ? window.innerWidth : 1280));
  const [artifactWidthReady, setArtifactWidthReady] = useState(false);
  // Roster-row → Worker 详情 focus seed. The nonce bumps on every click so
  // re-clicking the same worker re-focuses the lanes panel; WorkerLanes reacts
  // only to a new nonce, leaving the operator's manual chip filtering intact.
  const [focusedWorker, setFocusedWorker] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedSpeaker, setFocusedSpeaker] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedPoc, setFocusedPoc] = useState<{ id: string; nonce: number } | null>(null);
  const [focusedFact, setFocusedFact] = useState<{ seq: number; nonce: number } | null>(null);
  // Focus seeds handed over by the collaboration page (see urlRuntimeView); the
  // handoff keys are consumed once and dropped from the URL. Wait until the
  // run mode is known and matches this shell — otherwise a cross-mode replace
  // remounts without view/worker/speaker/poc/fact and the requested panel is lost.
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
    // App-owned history updates must let Next refresh its search-param snapshot.
    window.history.replaceState({}, "", `${window.location.pathname}${query ? `?${query}` : ""}${window.location.hash}`);
  }, [deck.started, deck.runId, deck.mode, requestedRunId, requestedSearch, runId, setArtifactView, workspaceMode]);
  const [winW, setWinW] = useState(typeof window !== "undefined" ? window.innerWidth : 1280);
  const [listBump, setListBump] = useState(0);
  const pushToast = useCallback((input: { msg: string; variant?: "success" | "error" | "info"; undo?: () => void; icon?: IconName }) => {
    toast(input.msg, {
      variant: input.variant === "error" ? "danger" : input.variant === "success" ? "success" : "default",
      indicator: input.icon ? <Icon name={input.icon} size={15} /> : undefined,
      timeout: input.undo ? 6500 : 3500,
      actionProps: input.undo ? { children: t("rail.toast.undo"), onPress: input.undo } : undefined,
    });
  }, [t]);
  const shellRef = useRef<HTMLDivElement | null>(null);
  const openWorkerSettings = useCallback(() => {
    const returnTo = `${window.location.pathname}${window.location.search}`;
    router.push(`${workspaceMode === "pentest" ? "/pentest/workers" : "/ctf/workers"}?return=${encodeURIComponent(returnTo)}`);
  }, [router, workspaceMode]);
  // Files attached to the NEXT dispatch (file-based tracks). Saved server-side
  // the moment they're attached; we hold the returned absolute paths here so
  // dispatch() can put them on challenge.attachments. Lives at this level (not
  // in the Composer) so it survives into dispatch and resets on run switch.
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
  const runs = allRuns.filter((item) => (item.mode || "ctf") === workspaceMode);
  const allFolders = useFolders(8000, listBump);
  const folders = workspaceMode === "pentest" ? [] : allFolders;
  const bump = () => setListBump((n) => n + 1);

  // shorthand for a failure toast — shown whenever a mutation helper returns
  // null/false (network / backend error), the biggest silent-failure gap today.
  const toastFail = () => pushToast({ msg: t("toast.actionFailed"), variant: "error" });

  useEffect(() => {
    const update = () => setTheme(readSavedTheme());
    update(); setThemeReady(true);
    return subscribeThemePreference(update);
  }, []);

  // Color scheme: the picker lives on the settings page; here we only
  // re-apply the saved selection whenever the light/dark mode flips, so the
  // palette is regenerated with the new mode's lightness/chroma curves.
  useEffect(() => {
    if (!themeReady) return;
    applySelection(readSavedSelection(), theme);
  }, [theme, themeReady]);

  const toggleTheme = () => setThemePreference(theme === "dark" ? "light" : "dark");

  // The run list becomes an overlay drawer below 640px. Start that drawer
  // closed on a narrow viewport so a direct visit exposes the task composer
  // instead of covering it. The menu button still opens and closes it normally.
  useEffect(() => {
    if (window.matchMedia("(max-width: 640px)").matches) setRailCollapsed(true);
  }, []);

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

  const onRequestProgress = useCallback(async (): Promise<boolean> => {
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
  }, [pushToast, requestProgress, t]);

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

  // Escape closes the open runtime panel (collaboration / workers / timeline / …).
  // Layering: the settings modal sits on top and owns Esc first (its handler is
  // capture-phase + stopPropagation, and we also guard on `showSettings` here so
  // a modal Esc never leaks through to close the panel). The rail's ⋯ menu Esc is
  // an independent transient and doesn't conflict. Only bind while the panel is
  // open and no modal is up. The palette is ALSO a modal layer: while it's open,
  // its own Esc closes it (a React handler with stopPropagation can't stop this
  // native window listener), so gate on !paletteOpen too — otherwise one Esc
  // would close both the palette and the panel beneath it. The collab canvas
  // claims Esc while focus is inside .collab-shell / .react-flow (it
  // preventDefaults after peeling a selection layer). Honour defaultPrevented,
  // and also bail when the target is inside the canvas, so a focused node or
  // search does not close the runtime panel.
  useEffect(() => {
    if (!artifactOpen || showSettings || paletteOpen || btwOpen) return;
    const onEsc = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      if ((e.target as Element | null)?.closest(".react-flow, .collab-shell")) return;
      e.preventDefault();
      setArtifactOpen(false);
    };
    window.addEventListener("keydown", onEsc);
    return () => window.removeEventListener("keydown", onEsc);
  }, [artifactOpen, showSettings, paletteOpen, btwOpen]);

  // Single-key shortcuts to jump the secondary panels from the keyboard, so a
  // power operator watching a run never has to reach for the mouse. The key map
  // is PANEL_HOTKEYS from lib/runtimeTabs.ts (one hotkey per RUNTIME_TABS entry,
  // plus the retired `b` alias). Esc (above) already closes, so a key only ever
  // OPENS its panel.
  //
  // The make-or-break guard is "never fire while typing": we bail on any modifier
  // (so Cmd+K composer-focus, browser shortcuts, and Ctrl/Alt combos pass through),
  // on IME composition, and when focus is in a text field — checked on BOTH the
  // event.target AND document.activeElement so a steer like "go" typed into the
  // composer can never clobber a panel. Only armed once a run has started (panels
  // are meaningless on the welcome screen) and no modal is up.
  useEffect(() => {
    if (!deck.started || showSettings || paletteOpen || btwOpen) return;
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
  // A promoted draft becomes a real run before the rail's polling summary has
  // observed it. Treat every real run with no folded event yet as loading;
  // otherwise the URL changes to /run/<id> while the old new-task welcome
  // remains visible until a refresh.
  const loading = !isDraft(runId) && !deck.started;
  useFlagPulse(shellRef, deck.flags.length);
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

  // Dispatch the REAL swarm conversationally: one prose prompt → /start.
  // The original prompt is the Swarm task body. Planner only labels the left
  // rail with a title and category; it does not rewrite Decide or Worker text.
  const dispatch = async (prompt: string, opts?: DispatchOpts) => doDispatch(prompt, opts);
  const doDispatch = async (prompt: string, opts?: DispatchOpts) => {
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
    const offline = opts?.mode === "pentest" ? false : opts ? !opts.webSearch : false;
    // pentest mode (goal-driven, no flag) adds goal/scope. CTF (default) sends
    // nothing extra so its dispatch body stays byte-identical.
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
        // Send the human sample explicitly.  The shared backend contract turns
        // it into the bounded brace regex before planner/worker admission.
        challenge.flag_format = "flag{...}";
      } else if (opts?.flagFormat === "token") {
        challenge.flag_format = "token";
      } else if (opts?.flagFormat === "custom" && opts.flagWrapper?.trim()) {
        challenge.flag_format_wrapper = opts.flagWrapper.trim();
      }
      if (opts?.collect) {
        // Collect mode is multi-flag only. Flag shape is independent: brace sends
        // its explicit sample contract; token/custom mode stays opt-in.
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
    const startBody = {
      kind: "swarm",
      prompt,
      offline,
      allow_operator_input: opts?.allowOperatorInput ?? false,
      challenge,
      worker_backend,
      ...runOverrides,
    };
    try {
      await start(startBody, id);
      setAttachments([]); // chips consumed by this dispatch
      setListBump((n) => n + 1);
      return true;
    } catch (err) {
      const detail = err instanceof Error ? err.message : "";
      pushToast({
        msg: detail ? `${t("toast.dispatchFailed")}：${detail}` : t("toast.dispatchFailed"),
        variant: "error",
      });
      return false;
    }
  };

  const onNewSolve = () => {
    // Purely local: reset to a fresh empty draft. No backend run is created until
    // the operator dispatches, so "+ New solve" can't litter the rail with stubs.
    setArtifactOpen(false);
    setReportOpen(false);
    if (workspaceMode === "pentest") {
      const url = new URL(window.location.href);
      url.searchParams.delete("report_seq");
      url.searchParams.delete("finding");
      window.history.replaceState(window.history.state ?? {}, "", `${url.pathname}${url.search}${url.hash}`);
    }
    setAttachments([]);
    setRunId(newDraftId());
    if (window.innerWidth <= 900) setRailCollapsed(true);
  };

  const onSelectRun = (id: string) => {
    if (window.innerWidth <= 900) setRailCollapsed(true);
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
    setRunId(id);
  };

  // Pin / archive / rename / delete from a row action or context menu. Optimistic: mutate the
  // backend, then bump the rail poll. Archive shows an undo toast; delete asks
  // for confirmation first (irreversible).
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
  ) => {
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
      // Create instantly with a default name and hand the folder back so the rail
      // drops it into inline-rename without a blocking browser dialog. Blur keeps the default.
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
        // KEEP the archive-undo behavior — migrated into the unified lane.
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
      // if we just deleted the open conversation, fall back to a fresh draft
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

  // The agent map lives on its own page; every entry point (top switcher,
  // inspector button, palette, `g` hotkey, signal strip) routes there. A draft
  // has no run to map yet, so the jump is a no-op until dispatch.
  const openCollaboration = (query?: Record<string, string>) => {
    if (!runId || isDraft(runId)) return;
    const params = new URLSearchParams(query);
    const search = params.toString();
    router.push(`${collabPagePath(runId)}${search ? `?${search}` : ""}`);
  };
  const openArtifact = (view: ArtifactView) => {
    if (view === "collaboration") { openCollaboration(); return; }
    setReportOpen(false);
    setArtifactView(view);
    setArtifactOpen(true);
  };
  const openReport = () => {
    setArtifactOpen(false);
    setReportOpen(true);
  };
  const openFact = (seq: number) => {
    setReportOpen(false);
    setArtifactOpen(true);
    setArtifactView("evidence");
    setFocusedFact({ seq, nonce: Date.now() });
  };

  // Roster mini-row click → open the Worker 详情 panel focused on that worker.
  // Bump the nonce so re-clicking the same row re-seeds the lanes filter.
  const onOpenWorker = (id: string) => {
    setFocusedWorker((prev) => ({ id, nonce: (prev?.nonce ?? 0) + 1 }));
    openArtifact("workers");
  };

  const onOpenAgent = (id: string) => openCollaboration({ agent: id });

  const onOpenKnowledge = (id: string) => openCollaboration({ k: id });

  const paletteAgents = useMemo(() => {
    const ids = workerIds(deck);
    const siblings = ids.map((id) => toWorkerIdentity(id, deck.lanes[id]));
    return ids.map((id) => {
      const lane = deck.lanes[id];
      return {
        id,
        title: workerDisplayName(id, toWorkerIdentity(id, lane), siblings).title,
        engine: workerEngine(id, lane?.engine),
        status: lane?.status || "",
      };
    });
  }, [deck]);

  // Worker 详情 "在活动流中查看" → open the timeline tab with that worker's
  // speaker chip pre-selected (reverse of onOpenWorker; bump nonce to re-seed).
  const onOpenSpeakerTimeline = (id: string) => {
    setFocusedSpeaker((prev) => ({ id, nonce: (prev?.nonce ?? 0) + 1 }));
    openArtifact("timeline");
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
  const onKillWorker = (solverId: string) => {
    if (!runId) return;
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
  // reveal the run's workspace dir in the host file manager (real backend run only).
  const onOpenWorkspace = () => { if (runId && !isDraft(runId)) openWorkspace(runId); };

  return (
    <div ref={shellRef} className={`shell motion-root ${workspaceMode === "pentest" ? "pentest-workbench" : ""}`}>
      <a href="#main-conversation" className="skip-link">{t("a11y.skipToMain")}</a>
      <ThreadRail
        workspaceMode={workspaceMode}
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
        onClose={() => setRailCollapsed(true)}
      />
      {!railCollapsed && <button type="button" className="rail-scrim" onClick={() => setRailCollapsed(true)} aria-label={t("rail.close")} />}
      <main id="main-conversation" className="main t-texts-reveal" aria-label={t("a11y.main")}>
        <Conversation
          workspaceMode={workspaceMode}
          deck={deck}
          running={running}
          loading={loading}
          connected={connected}
          onCommand={onCommand}
          onRequestProgress={onRequestProgress}
          onResolve={resolve}
          onDispatch={dispatch}
          attachments={attachments}
          onAddFiles={addFiles}
          onRemoveFile={removeFile}
          artifactOpen={artifactOpen}
          artifactView={artifactView}
          onOpenArtifact={openArtifact}
          onShowConversation={() => { setArtifactOpen(false); setReportOpen(false); }}
          reportOpen={reportOpen}
          reportPanel={workspaceMode === "pentest" && !isDraft(runId)
            ? <PentestReport key={runId} runId={runId} runTitle={deck.challengeName} startedAt={deck.startedAt} finishedAt={deck.finishedAt} onOpenFact={openFact} />
            : undefined}
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
              onView={setArtifactView}
              onClose={() => setArtifactOpen(false)}
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
          onToggleRail={() => setRailCollapsed((v) => !v)}
          theme={theme}
          onToggleTheme={toggleTheme}
          onSpawnWorker={onSpawnWorker}
          onKillWorker={onKillWorker}
          onOpenWorker={onOpenWorker}
          onOpenAgent={onOpenAgent}
          onOpenKnowledge={onOpenKnowledge}
          onOpenWorkspace={onOpenWorkspace}
          onHitlAnswered={() => pushToast({ msg: t("hitl.answered"), variant: "success" })}
          onOpenBtw={() => setBtwOpen(true)}
        />
      </main>
      <CommandPalette
        workspaceMode={workspaceMode}
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
        agents={paletteAgents}
        onOpenAgent={onOpenAgent}
        onOpenReport={workspaceMode === "pentest" ? openReport : undefined}
      />
      <BtwPanel
        open={btwOpen}
        onClose={() => setBtwOpen(false)}
        runId={runId}
      />
      <Modal.Backdrop isOpen={Boolean(railDeleteConfirm)} onOpenChange={(open) => !open && setRailDeleteConfirm(null)} isDismissable={!railDeleteBusy}>
        <Modal.Container><Modal.Dialog>
          <Modal.Header className="flex flex-col gap-1"><Modal.Heading>{railDeleteConfirm?.kind === "folder" ? t("rail.folderDelete") : t("rail.menu.delete")}</Modal.Heading><small className="font-normal text-muted">{railDeleteConfirm?.kind === "folder" ? t("rail.confirmDeleteFolder") : t("rail.confirmDelete")}</small></Modal.Header>
          <Modal.Body>{railDeleteConfirm ? <div className="action-dialog-summary"><span><strong>{railDeleteConfirm.label}</strong></span><code>{railDeleteConfirm.id}</code></div> : null}</Modal.Body>
          <Modal.Footer><Button variant="ghost" onPress={() => setRailDeleteConfirm(null)} isDisabled={railDeleteBusy}>{t("dialog.cancel")}</Button><Button variant="danger" isPending={railDeleteBusy} onPress={() => void confirmRailDelete()}>{t("rail.menu.delete")}</Button></Modal.Footer>
        </Modal.Dialog></Modal.Container>
      </Modal.Backdrop>
      <Modal.Backdrop isOpen={Boolean(killConfirm)} onOpenChange={(open) => !open && setKillConfirm(null)} isDismissable={!killConfirmBusy}>
        <Modal.Container><Modal.Dialog>
          <Modal.Header className="flex flex-col gap-1"><Modal.Heading>{t("worker.confirmKillTitle")}</Modal.Heading><small className="font-normal text-muted">{t("worker.confirmKill")}</small></Modal.Header>
          <Modal.Body>{killConfirm ? <div className="action-dialog-summary"><span><strong>{killConfirm.label}</strong></span><code>{killConfirm.id}</code></div> : null}</Modal.Body>
          <Modal.Footer><Button variant="ghost" onPress={() => setKillConfirm(null)} isDisabled={killConfirmBusy}>{t("dialog.cancel")}</Button><Button variant="danger" isPending={killConfirmBusy} onPress={() => void confirmKillWorker()}>{t("worker.killTitle")}</Button></Modal.Footer>
        </Modal.Dialog></Modal.Container>
      </Modal.Backdrop>
    </div>
  );
}
