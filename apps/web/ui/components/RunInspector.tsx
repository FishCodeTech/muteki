"use client";

import { useEffect, useMemo, useState } from "react";
import type { CSSProperties, MouseEvent as ReactMouseEvent } from "react";
import {
  DeckState, SolverLane, isReviewWorkerLane, isFactRetired,
  verifiedFactTexts, candidateFactTexts, openIntentTexts, deadEndTexts, workerIds,
  currentGenWorkerIds,
  type BlackboardVulnReport,
} from "@/lib/events";
import { useLang, useT } from "@/lib/i18n";
import { workerColor, workerEngine, resumeCommand, toWorkerIdentity, workerDisplayName, formatWorkerSubtitle, actorDisplayTitle, workerGeneration } from "@/lib/workers";
import { Icon, type IconName } from "@/components/Icon";
import { SelectionGlider } from "@/components/SelectionGlider";
import { useCopied } from "@/lib/useCopied";
import { CopyText } from "@/components/CopyText";
import {
  estimateCvss,
  findingClassLabel,
  reportLocationLabel,
  reportsToCollectionMarkdown,
  type CvssRating,
} from "@/lib/reportMarkdown";
import type { ArtifactView } from "@/lib/events";

/**
 * The persistent right-column run inspector (the redesign's floating inspector):
 *   ① flag / outcome + evidence chips
 *   ② child-worker mini rows (engine · status · session · winner · spawn/kill)
 *   ③ a button group that opens the secondary panels (evidence / workers / graph
 *      / activity / blackboard) + "generate writeup".
 *
 * The worker firehose itself lives in the secondary panels — here we only show
 * the compact roster, so the operator always sees WHO is racing without the
 * coordinator conversation being drowned out.
 */

const SPAWN_ENGINES = [
  "claude", "codex", "cursor", "pi", "omp", "kimi", "grok", "opencode", "dsh",
];

type InspectorSignal = "verified" | "candidates" | "intents" | "dead" | "cost";

type SignalCostRow = {
  id: string;
  label: string;
  engine: string;
  usd: number;
  tokens: number;
};

function compactNumber(value: number): string {
  if (value < 1000) return String(value);
  if (value < 1_000_000) return `${(value / 1000).toFixed(1)}k`;
  return `${(value / 1_000_000).toFixed(2)}m`;
}

function InspectorSignalDetail({
  signal,
  label,
  count,
  items,
  costRows,
  totalUsd,
  totalTokens,
  targetLabel,
  onClose,
  onOpenTarget,
}: {
  signal: InspectorSignal;
  label: string;
  count: number;
  items: string[];
  costRows: SignalCostRow[];
  totalUsd: number;
  totalTokens: number;
  targetLabel: string;
  onClose: () => void;
  onOpenTarget: () => void;
}) {
  const t = useT();
  const visibleItems = items.slice(0, 4);
  const visibleCosts = costRows.slice(0, 4);
  const remaining = signal === "cost"
    ? Math.max(0, costRows.length - visibleCosts.length)
    : Math.max(0, items.length - visibleItems.length);
  const empty = signal === "cost" ? visibleCosts.length === 0 : visibleItems.length === 0;

  return (
    <div id="inspector-signal-detail" className={`insp-signal-detail ${signal}`}>
      <div className="insp-signal-detail-head">
        <span><strong>{label}</strong><b>{count}</b></span>
        <button type="button" onClick={onClose} aria-label={t("settings.close")} title={t("settings.close")}>
          <Icon name="x" size={13} />
        </button>
      </div>
      {signal === "cost" && (
        <div className="insp-cost-total">
          <span>{t("insp.signal.totalCost")}</span>
          <b>${totalUsd.toFixed(4)}</b>
          <small>{compactNumber(totalTokens)} {t("meta.tokens")}</small>
        </div>
      )}
      {empty ? (
        <div className="insp-signal-empty">{t("insp.signal.empty")}</div>
      ) : signal === "cost" ? (
        <div className="insp-cost-list">
          {visibleCosts.map((row) => (
            <div className="insp-cost-row" key={row.id}>
              <span className="insp-cost-agent"><b>{row.label}</b><small>{row.engine}</small></span>
              <span className="insp-cost-value"><b>${row.usd.toFixed(4)}</b><small>{compactNumber(row.tokens)} {t("meta.tokens")}</small></span>
            </div>
          ))}
        </div>
      ) : (
        <ol className="insp-signal-list">
          {visibleItems.map((item, index) => <li key={`${index}-${item}`}><span>{item}</span></li>)}
        </ol>
      )}
      <div className="insp-signal-detail-foot">
        <span>{remaining > 0 ? t("insp.signal.more", { n: remaining }) : ""}</span>
        <button type="button" onClick={onOpenTarget}>
          {targetLabel}<Icon name="chevronRight" size={13} />
        </button>
      </div>
    </div>
  );
}

function reportStatusRank(status: BlackboardVulnReport["status"]): number {
  switch (status) {
    case "accepted":
      return 0;
    case "reproduced":
      return 1;
    case "submitted":
      return 2;
    case "repro_failed":
      return 3;
    case "rejected":
      return 4;
    default: {
      const _never: never = status;
      return _never;
    }
  }
}

function reportStatusLabel(
  status: BlackboardVulnReport["status"],
  t: (key: string) => string,
): string {
  switch (status) {
    case "accepted":
      return t("runtime.reports.accepted");
    case "reproduced":
      return t("runtime.reports.reproduced");
    case "submitted":
      return t("runtime.reports.submitted");
    case "repro_failed":
      return t("runtime.reports.reproFailed");
    case "rejected":
      return t("runtime.reports.rejected");
    default: {
      const _never: never = status;
      return _never;
    }
  }
}

function reportStatusBadge(status: BlackboardVulnReport["status"]): string {
  switch (status) {
    case "accepted":
      return "ok";
    case "rejected":
      return "bad";
    case "repro_failed":
      return "sev-warn";
    case "submitted":
    case "reproduced":
      return "";
    default: {
      const _never: never = status;
      return _never;
    }
  }
}

function severityBadgeClass(rating: CvssRating): string {
  switch (rating) {
    case "critical":
      return "sev-critical";
    case "high":
      return "sev-high";
    case "medium":
      return "sev-medium";
    case "low":
      return "sev-low";
    default: {
      const _never: never = rating;
      return _never;
    }
  }
}

function ExportCollectionButton({ text }: { text: string }) {
  const t = useT();
  const [copied, copy] = useCopied();
  return (
    <button
      type="button"
      className={`insp-report-export ${copied ? "copied" : ""}`.trim()}
      title={t("insp.run.exportCollection")}
      aria-label={t("runtime.reports.copyCollectionAria")}
      onClick={() => copy(text)}
    >
      <Icon name={copied ? "check" : "copy"} size={13} />
      <span>{copied ? t("common.copied") : t("insp.run.exportCollection")}</span>
    </button>
  );
}

function PentestReportDirectory({
  rows,
  accepted,
  collectionTitle,
  onOpenReport,
}: {
  rows: BlackboardVulnReport[];
  accepted: BlackboardVulnReport[];
  collectionTitle: string;
  onOpenReport: (id: string) => void;
}) {
  const t = useT();
  const collection = reportsToCollectionMarkdown(accepted, collectionTitle);
  return (
    <div className="insp-report-dir">
      {rows.map((row) => {
        const cvss = estimateCvss(row);
        const location = reportLocationLabel(row.resourceId) || row.title;
        const typeLabel = findingClassLabel(row.findingClass);
        return (
          <button
            type="button"
            className="insp-report-row"
            key={row.id}
            onClick={() => onOpenReport(row.id)}
            title={t("insp.run.openReport", { title: row.title })}
            aria-label={t("insp.run.openReport", { title: `${typeLabel} ${location}` })}
          >
            <span className="insp-report-row-top">
              <span className="insp-report-type">{typeLabel}</span>
              <span
                className={`artifact-badge ${severityBadgeClass(cvss.rating)}`}
                title={t("runtime.reports.cvssHint")}
              >
                {cvss.badge}
              </span>
              <span className={`artifact-badge ${reportStatusBadge(row.status)}`}>
                {reportStatusLabel(row.status, t)}
              </span>
            </span>
            <code className="insp-report-path">{location}</code>
          </button>
        );
      })}
      <div className="insp-report-foot">
        <span className="insp-report-hint">{t("insp.run.reportHint")}</span>
        {accepted.length > 0 && <ExportCollectionButton text={collection} />}
      </div>
    </div>
  );
}

function runtimeLabel(lane: SolverLane): string {
  const runtime = lane.runtime;
  if (!runtime?.backend) return "";
  const status = runtime.status ? `:${runtime.status}` : "";
  return `${runtime.backend}${status}`;
}

function WorkerMiniRow({
  lane,
  running,
  isWinner,
  facts,
  siblings,
  onKill,
  onOpen,
}: {
  lane: SolverLane;
  running: boolean;
  isWinner: boolean;
  facts: number;
  siblings: ReturnType<typeof toWorkerIdentity>[];
  onKill: (id: string) => void;
  onOpen: (id: string) => void;
}) {
  const t = useT();
  const online = lane.online !== false;
  const engine = workerEngine(lane.solverId, lane.engine);
  const color = workerColor(lane.solverId, lane.engine);
  const display = workerDisplayName(lane.solverId, toWorkerIdentity(lane.solverId, lane), siblings);
  const subtitle = formatWorkerSubtitle(display, t);
  const session = lane.session;
  const resumeCmd = session ? resumeCommand(engine, session) : "";
  const reason = lane.statusReason || lane.status;
  const runtime = runtimeLabel(lane);
  const [copied, copy] = useCopied();
  const copySession = (e: ReactMouseEvent) => { e.stopPropagation(); copy(resumeCmd); };
  // micro health-stat: verified facts + tool-call count. 0/0 = a spinning worker
  // (rendered "idle" + dimmed); >0 facts = productive (subtle tint).
  const tools = lane.toolLines.length;
  const productive = facts > 0;
  const idle = facts === 0 && tools === 0;
  const isReview = isReviewWorkerLane(lane);
  // The whole row is a click target → opens the "Worker 详情" panel focused on
  // this worker. The kill button + session-copy chip stopPropagation so they keep
  // their own behavior. role=button + Enter/Space keep it keyboard-accessible.
  const open = () => onOpen(lane.solverId);
  return (
    <div
      className={`iwk iwk-clickable ${online ? "online" : "offline"} ${isWinner ? "winner" : ""} ${productive ? "productive" : ""} ${isReview ? "review-worker" : ""}`}
      style={{ "--wc": color } as CSSProperties}
      title={`${engine} · ${reason}${runtime ? ` · ${runtime}` : ""}`}
      role="button"
      tabIndex={0}
      aria-label={t("insp.run.openWorker", { id: display.title })}
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); }
      }}
    >
      <span className="iwk-avatar">{display.initial}</span>
      <span className="iwk-meta">
        <span className="iwk-name" title={display.titleAttr}>{display.title}</span>
        {workerGeneration(lane.solverId) > 1 && (
          <span className="iwk-gen" title={lane.solverId}>g{workerGeneration(lane.solverId)}</span>
        )}
        <span className="iwk-sub">
          <span className="iwk-dot" />
          <span className="iwk-eng">{engine}</span>
          <span className="iwk-conn">{subtitle}</span>
          {isReview && <span className="worker-role-chip review">{t("worker.role.review")}</span>}
          {runtime && <span className="iwk-runtime">{runtime}</span>}
          {session && (
            <span className={`iwk-sess ${copied ? "copied" : ""}`} title={t("insp.run.copySession") + ": " + resumeCmd}
              role="button" tabIndex={0} aria-label={t("insp.run.copySession")}
              onClick={copySession}
              onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); e.stopPropagation(); copy(resumeCmd); } }}>
              {copied ? <><Icon name="check" size={11} /> {t("common.copied")}</> : session.slice(0, 12)}
            </span>
          )}
        </span>
        <span className={`iwk-stat ${idle ? "idle" : ""}`} title={t("insp.run.statTitle")}>
          {idle ? (
            t("insp.run.statIdle")
          ) : (
            <>
              <Icon name="check" size={10} />
              <b>{facts}</b> {t("insp.run.statFacts")}
              <span className="iwk-stat-sep">·</span>
              <Icon name="terminal" size={10} />
              <b>{tools}</b> {t("insp.run.statTools")}
            </>
          )}
        </span>
      </span>
      {/* I: paused / stalled markers so a held or stuck worker is visible at a glance */}
      {online && lane.paused && (
        <span className="iwk-paused" title={t("worker.paused")}><Icon name="pause" size={13} /></span>
      )}
      {online && !lane.paused && lane.status === "stalled" && (
        <span className="iwk-stalled" title={t("worker.stalled")}><Icon name="clock" size={13} /></span>
      )}
      {isWinner && <span className="iwk-win" title={t("insp.run.winner")}><Icon name="flag" size={14} /></span>}
      {running && online && (
        <button className="iwk-kill" title={t("worker.killTitle")} aria-label={t("worker.killTitle")}
          onClick={(e) => { e.stopPropagation(); onKill(lane.solverId); }}><Icon name="x" size={13} /></button>
      )}
    </div>
  );
}

export function RunInspector({
  deck,
  running,
  artifactOpen,
  artifactView,
  onOpenArtifact,
  onSpawnWorker,
  onKillWorker,
  onOpenWorker,
  onWriteup,
  onMarkFalseFlag,
  onOpenReport,
}: {
  deck: DeckState;
  running: boolean;
  artifactOpen: boolean;
  artifactView: ArtifactView;
  onOpenArtifact: (v: ArtifactView) => void;
  onSpawnWorker: (engine?: string) => void;
  onKillWorker: (id: string) => void;
  // open the "Worker 详情" panel focused on a single worker (roster row click).
  onOpenWorker: (id: string) => void;
  onWriteup: () => void;
  onMarkFalseFlag: (flag: string) => void;
  onOpenReport: (reportId: string) => void;
}) {
  const t = useT();
  const { lang } = useLang();
  const [spawnEngine, setSpawnEngine] = useState("");
  const [collapsedSections, setCollapsedSections] = useState<Set<string>>(new Set());
  const [activeSignal, setActiveSignal] = useState<InspectorSignal | null>(null);

  const verifiedItems = verifiedFactTexts(deck);
  const candidateItems = candidateFactTexts(deck);
  const intentItems = openIntentTexts(deck);
  const deadItems = deadEndTexts(deck);
  const verified = verifiedItems.length;
  const candidates = candidateItems.length;
  const intents = intentItems.length;
  const deads = deadItems.length;
  const acceptedReports = (deck.blackboard.vulnReports ?? []).filter((row) => row.status === "accepted");
  const submittedReports = (deck.blackboard.vulnReports ?? []).filter((row) => row.status === "submitted").length;
  const reproducingReports = Math.max(0, deck.verifying ?? 0);
  const directoryReports = (deck.blackboard.vulnReports ?? [])
    .filter((row) => row.status !== "rejected")
    .slice()
    .sort((a, b) => reportStatusRank(a.status) - reportStatusRank(b.status) || a.ts - b.ts);
  const pentest = deck.mode === "pentest";
  const reportCollectionTitle = deck.challengeName ? `${deck.challengeName} 漏洞报告集` : "漏洞报告集";
  const workerSiblings = useMemo(
    () => workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id])),
    [deck],
  );
  const costRows = useMemo<SignalCostRow[]>(() => Object.entries(deck.costBySolver)
    .map(([id, cost]) => ({
      id,
      label: actorDisplayTitle(id, t, toWorkerIdentity(id, deck.lanes[id]), workerSiblings),
      engine: cost.engine || workerEngine(id, deck.lanes[id]?.engine),
      usd: cost.usd,
      tokens: cost.tokensIn + cost.tokensOut,
    }))
    .filter((row) => row.usd > 0 || row.tokens > 0)
    .sort((a, b) => b.usd - a.usd || b.tokens - a.tokens), [deck.costBySolver, deck.lanes, t, workerSiblings]);
  // E: active resource locks held across the swarm (site/account/listener)
  const activeLocks = (deck.resourceLocks ?? []).filter((l) => l.status === "active");
  // H: how many times the graph was compacted this run
  const compactEpochs = deck.compactEpochs ?? 0;
  const degradedEvents = deck.blackboard.events.filter((e) =>
    e.kind === "runtime_degraded" || e.kind === "worker_backend_degraded");
  // engines dropped from this run's roster by a dispatch-time health check (e.g.
  // cursor headless auth lapsed). engine → reason; recover events clear it.
  const degradedEngines = Object.entries(deck.degradedEngines || {});

  const rawIds = useMemo(() => workerIds(deck), [deck]);
  const laneFor = (id: string): SolverLane => deck.lanes[id] || {
    solverId: id, reasoning: "", toolLines: [], status: running ? "waiting" : "done",
    solved: false, online: !deck.finished,
  };
  const winnerId = rawIds.find((id) => laneFor(id).solved);

  // verified-fact count per worker — mirrors WorkerLanes' `verifiedByActor`
  // derivation (blackboard provenance facts keyed by their `actor`). Lets the
  // roster read as a glanceable health board: a productive worker (facts + tools)
  // vs a spinning one (0/0) is distinguishable without opening the lanes panel.
  const verifiedByActor = useMemo(() => {
    const m = new Map<string, number>();
    for (const f of deck.blackboard.facts) {
      // A: a fact retired by review (rejected/merged/superseded) no longer counts
      if (f.verified && !isFactRetired(f)) m.set(f.actor, (m.get(f.actor) || 0) + 1);
    }
    return m;
  }, [deck.blackboard.facts]);

  // Health-ranked roster: winner → productive (facts, then tools) → online-idle
  // → offline. Stable: ties fall back to the original workerIds order so the
  // list doesn't jitter across polls. Only display order changes; the set is
  // identical to rawIds, so capping never drops the important workers.
  const ids = useMemo(() => {
    const lane = (id: string): SolverLane => deck.lanes[id] || {
      solverId: id, reasoning: "", toolLines: [], status: "waiting",
      solved: false, online: !deck.finished,
    };
    const rank = (id: string): number => {
      const l = lane(id);
      if (l.solved) return 4;                                   // winner first
      const facts = verifiedByActor.get(id) || 0;
      if (facts > 0 || l.toolLines.length > 0) return 3;        // productive
      if (l.online !== false) return 2;                         // online-idle
      return 1;                                                 // offline last
    };
    return rawIds
      .map((id, i) => ({ id, i }))
      .sort((a, b) => {
        const ra = rank(a.id), rb = rank(b.id);
        if (ra !== rb) return rb - ra;
        const fa = verifiedByActor.get(a.id) || 0, fb = verifiedByActor.get(b.id) || 0;
        if (fa !== fb) return fb - fa;                          // more facts first
        const ta = lane(a.id).toolLines.length, tb = lane(b.id).toolLines.length;
        if (ta !== tb) return tb - ta;                          // more tools first
        return a.i - b.i;                                       // stable tie-break
      })
      .map((e) => e.id);
  }, [rawIds, deck.lanes, deck.finished, verifiedByActor]);

  // roster summary + cap-with-expand. Headline counts follow the CURRENT
  // execution generation: a continued run (resolve) keeps previous generations'
  // finished workers listed below, but they are not live roster members.
  const ROSTER_CAP = 12;
  const curIds = currentGenWorkerIds(deck);
  const onlineCount = curIds.filter((id) => laneFor(id).online !== false).length;
  const solvedCount = ids.filter((id) => laneFor(id).solved).length;
  const [showAll, setShowAll] = useState(false);
  const capped = ids.length > ROSTER_CAP && !showAll;
  const visibleIds = capped ? ids.slice(0, ROSTER_CAP) : ids;

  // single-key shortcut advertised in each button's tooltip + aria-label (handler
  // lives in page.tsx). Mirrors PANEL_KEYS there — keep both maps in sync.
  const PANEL_HOTKEY: Partial<Record<ArtifactView, string>> = {
    evidence: "e", workers: "w", graph: "g", timeline: "t", blackboard: "b",
    findings: "f", credentials: "c", pocs: "p", routes: "r", directives: "d",
  };
  const panelBtn = (view: ArtifactView, key: string, ico: IconName, full = false) => {
    const label = t(`panelbtn.${key}`);
    const hk = PANEL_HOTKEY[view];
    const title = hk ? `${label} (${hk})` : label;
    return (
      <button
        className={`insp-panel-btn motion-panel-btn ${full ? "full" : ""} ${key === "writeup" ? "writeup" : ""} ${artifactOpen && artifactView === view ? "on" : ""}`}
        aria-pressed={artifactOpen && artifactView === view}
        title={title}
        aria-label={title}
        onClick={() => onOpenArtifact(view)}
      >
        <span className="ico"><Icon name={ico} size={15} /></span>
        <span className="insp-panel-label">{label}</span>
        {hk && <kbd className="insp-panel-kbd" aria-hidden="true">{hk}</kbd>}
      </button>
    );
  };
  const sectionOpen = (key: string) => !collapsedSections.has(key);
  const toggleSection = (key: string) => {
    setCollapsedSections((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };
  useEffect(() => {
    if (!activeSignal) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setActiveSignal(null);
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [activeSignal]);

  const signalItems: Record<InspectorSignal, string[]> = {
    verified: verifiedItems,
    candidates: candidateItems,
    intents: intentItems,
    dead: deadItems,
    cost: [],
  };
  const signalTargets: Record<InspectorSignal, { view: ArtifactView; label: string }> = {
    verified: { view: "evidence", label: t("insp.signal.openEvidence") },
    candidates: { view: "evidence", label: t("insp.signal.openEvidence") },
    intents: { view: "blackboard", label: t("insp.signal.openBlackboard") },
    dead: { view: "evidence", label: t("insp.signal.openEvidence") },
    cost: { view: "workers", label: t("insp.signal.openWorkers") },
  };
  const signalDefs: Array<{ key: InspectorSignal; icon: IconName; label: string; value: string; count: number }> = [
    { key: "verified", icon: "check", label: t("meta.verified"), value: String(verified), count: verified },
    { key: "candidates", icon: "help", label: t("meta.candidates"), value: String(candidates), count: candidates },
    { key: "intents", icon: "crosshair", label: t("meta.intents"), value: String(intents), count: intents },
    { key: "dead", icon: "xCircle", label: t("meta.dead"), value: String(deads), count: deads },
    { key: "cost", icon: "terminal", label: t("meta.cost"), value: `$${deck.usd.toFixed(3)}`, count: costRows.length },
  ];
  const activeSignalDef = activeSignal ? signalDefs.find((item) => item.key === activeSignal) : undefined;
  const activeSignalTarget = activeSignal ? signalTargets[activeSignal] : undefined;
  const sectionHeader = (key: string, label: string, aside?: JSX.Element) => {
    const open = sectionOpen(key);
    return (
      <div className="insp-sec-h">
        <span>{label}</span>
        <span className="insp-sec-actions">
          {aside}
          <button
            className={`insp-sec-toggle ${open ? "open" : ""}`}
            onClick={() => toggleSection(key)}
            aria-expanded={open}
            title={t(open ? "insp.run.collapseSection" : "insp.run.expandSection")}
            aria-label={t(open ? "insp.run.collapseSection" : "insp.run.expandSection")}
          >
            <Icon name="chevronDown" size={13} />
          </button>
        </span>
      </div>
    );
  };

  return (
    <aside className={`run-inspector lang-${lang} motion-inspector`} aria-label={t("insp.run.title")}>
      {deck.preparing && (
        <div className="insp-preflight preparing" role="status">
          <Icon name="terminal" size={14} />
          <span>{t("insp.run.preparing")}</span>
        </div>
      )}
      {deck.preflightFailures.length > 0 && (
        <div className="insp-preflight failed" role="alert">
          <div className="insp-preflight-title">
            <Icon name="xCircle" size={14} />
            <span>{t("insp.run.preflightFailed")}</span>
          </div>
          <div className="insp-preflight-list">
            {deck.preflightFailures.map((failure) => (
              <div className="insp-preflight-row" key={failure.errorId || failure.profileId}>
                <b>{failure.profileId || failure.engine}</b>
                <span>{[failure.errorId, failure.stage || failure.layer, failure.code]
                  .filter(Boolean).join(" · ")}</span>
                <span>{failure.detail}</span>
              </div>
            ))}
          </div>
        </div>
      )}
      {deck.outcomeReason === "runtime_failure" && deck.outcomeDetail && (
        <div className="insp-preflight failed" role="alert">
          <div className="insp-preflight-title">
            <Icon name="xCircle" size={14} />
            <span>{t("insp.run.runtimeDegraded")}</span>
          </div>
          <div className="insp-preflight-list">
            <div className="insp-preflight-row">
              <b>{[deck.outcomeErrorId, deck.outcomeFailurePhase,
                deck.outcomeFailureCode].filter(Boolean).join(" · ")}</b>
              <span>{deck.outcomeDetail}</span>
            </div>
          </div>
        </div>
      )}
      {degradedEvents.length > 0 && (
        <div className="insp-runtime-degraded" role="status">
          <Icon name="xCircle" size={14} />
          <span>{t("insp.run.runtimeDegraded")}</span>
          <b>{degradedEvents[degradedEvents.length - 1].label}</b>
        </div>
      )}
      {degradedEngines.map(([engine, reason]) => (
        <div className="insp-runtime-degraded insp-engine-degraded" role="status" key={engine}>
          <Icon name="xCircle" size={14} />
          <span>{t("insp.run.engineDegraded").replace("{engine}", engine)}</span>
          <b>{reason}</b>
        </div>
      ))}
      <section className={`insp-sec insp-sec-outcome ${sectionOpen("outcome") ? "" : "collapsed"}`}>
        {sectionHeader("outcome", t(pentest ? "insp.run.reports" : "insp.run.flag"), pentest ? (
          deck.expectedFindings > 1 ? (
            <span className="insp-flag-count">
              {acceptedReports.length}/{deck.expectedFindings}
              {(submittedReports > 0 || reproducingReports > 0) && (
                <small>{submittedReports}/{reproducingReports}/{acceptedReports.length}</small>
              )}
            </span>
          ) : acceptedReports.length > 0 || submittedReports > 0 || reproducingReports > 0 ? (
            <span className="insp-flag-count">
              {acceptedReports.length}
              {(submittedReports > 0 || reproducingReports > 0) && (
                <small>{submittedReports}/{reproducingReports}/{acceptedReports.length}</small>
              )}
            </span>
          ) : undefined
        ) : deck.expectedFlags > 1 ? (
          <span className="insp-flag-count">{deck.flags.length}/{deck.expectedFlags}</span>
        ) : undefined)}
        {sectionOpen("outcome") && (
          <>
            {pentest && directoryReports.length > 0 ? (
              <PentestReportDirectory
                rows={directoryReports}
                accepted={acceptedReports}
                collectionTitle={reportCollectionTitle}
                onOpenReport={onOpenReport}
              />
            ) : pentest ? (
              <div className="insp-run-flag pending motion-feedback">
                <span className="insp-pending-row"><Icon name="list" size={13} /> {t("insp.run.pendingReports")}</span>
                <span className="insp-pending-hint">{t("insp.run.pendingReportsHint")}</span>
              </div>
            ) : deck.flags.length > 0 ? (
              <div className="insp-run-flags">
                {deck.flags.map((f) => (
                  <div className="insp-flag-row" key={f}>
                    <CopyText value={f} className="insp-run-flag motion-feedback" />
                    {!running && (
                      <button
                        type="button"
                        className="insp-flag-false"
                        title={t("quick.markFalseTitle")}
                        aria-label={t("quick.markFalseTitle")}
                        onClick={() => onMarkFalseFlag(f)}
                      >
                        <Icon name="xCircle" size={15} />
                      </button>
                    )}
                  </div>
                ))}
              </div>
            ) : deck.outcomeReason === "goal_met" ? (
              <CopyText
                value={deck.goalWhy || t("insp.run.goalMet")}
                className="insp-run-flag goal motion-feedback"
                titleKey="common.copyAnswer"
                ariaLabelKey="common.copyAnswerAria"
              />
            ) : (
              <div className="insp-run-flag pending motion-feedback">
                <span className="insp-pending-row"><Icon name="flag" size={13} /> {t("insp.run.pending")}</span>
                <span className="insp-pending-hint">{t("insp.run.pendingHint")}</span>
              </div>
            )}
            <div className="insp-signals" role="group" aria-label={t("insp.signal.title")}>
              {signalDefs.map((signal) => {
                const selected = activeSignal === signal.key;
                return (
                  <button
                    type="button"
                    className={`insp-signal ${signal.key} ${selected ? "on" : ""}`}
                    aria-expanded={selected}
                    aria-controls="inspector-signal-detail"
                    title={t(selected ? "insp.signal.hide" : "insp.signal.show", { label: signal.label })}
                    onClick={() => setActiveSignal((current) => current === signal.key ? null : signal.key)}
                    key={signal.key}
                  >
                    <span><Icon name={signal.icon} size={12} />{signal.label}</span>
                    <b>{signal.value}</b>
                  </button>
                );
              })}
            </div>
            {activeSignal && activeSignalDef && activeSignalTarget && (
              <InspectorSignalDetail
                signal={activeSignal}
                label={activeSignalDef.label}
                count={activeSignalDef.count}
                items={signalItems[activeSignal]}
                costRows={costRows}
                totalUsd={deck.usd}
                totalTokens={deck.tokensIn + deck.tokensOut}
                targetLabel={activeSignalTarget.label}
                onClose={() => setActiveSignal(null)}
                onOpenTarget={() => {
                  setActiveSignal(null);
                  onOpenArtifact(activeSignalTarget.view);
                }}
              />
            )}
          </>
        )}
      </section>

      <section className={`insp-sec insp-sec-workers ${sectionOpen("workers") ? "" : "collapsed"}`}>
        {sectionHeader("workers", t("insp.run.workers"), ids.length > 0 ? (
            <span className="iwk-summary">
              {t("insp.run.rosterSummary", { online: onlineCount, total: curIds.length, solved: solvedCount })}
            </span>
          ) : undefined)}
        {sectionOpen("workers") && (
          <>
            {ids.length === 0 ? (
              <div className="iwk-empty">
                <span className="iwk-empty-ico" aria-hidden="true"><Icon name="grid" size={20} /></span>
                <span className="iwk-empty-title">{t("insp.run.noWorkers")}</span>
                <span className="iwk-empty-hint">{t("insp.run.noWorkersHint")}</span>
              </div>
            ) : (
              <div className="iwk-list">
                {visibleIds.map((id) => (
                  <WorkerMiniRow key={id} lane={laneFor(id)} running={running}
                    isWinner={id === winnerId} facts={verifiedByActor.get(id) || 0}
                    siblings={workerSiblings}
                    onKill={onKillWorker} onOpen={onOpenWorker} />
                ))}
                {ids.length > ROSTER_CAP && (
                  <button className="iwk-showall" onClick={() => setShowAll((v) => !v)}
                    aria-expanded={!capped}>
                    {capped
                      ? t("insp.run.showAll", { n: ids.length })
                      : t("insp.run.showLess")}
                  </button>
                )}
              </div>
            )}
            {running && (
              <div className="iwk-spawn">
                <select value={spawnEngine} onChange={(e) => setSpawnEngine(e.target.value)} title={t("workerDock.engine")}>
                  <option value="">{t("workerDock.auto")}</option>
                  {SPAWN_ENGINES.map((e) => <option key={e} value={e}>{e}</option>)}
                </select>
                <button className="iwk-spawn-btn" onClick={() => onSpawnWorker(spawnEngine || undefined)}
                  title={t("workerDock.addTitle")}>＋ {t("workerDock.add")}</button>
              </div>
            )}
          </>
        )}
      </section>

      {(activeLocks.length > 0 || compactEpochs > 0) && (
        <section className={`insp-sec insp-sec-locks ${sectionOpen("locks") ? "" : "collapsed"}`}>
          {sectionHeader("locks", t("resource.lockActive"), (
            <span className="iwk-summary">
              {activeLocks.length > 0 && <span>{activeLocks.length}</span>}
              {compactEpochs > 0 && (
                <span className="insp-compact-badge" title={t("meta.compactEpochs")}>
                  {t("insp.compactBadge")} ×{compactEpochs}
                </span>
              )}
            </span>
          ))}
          {sectionOpen("locks") && (
            <div className="insp-locks">
              {activeLocks.length === 0 ? (
                <div className="iwk-empty"><span className="iwk-empty-hint">{t("resource.lockActive")} —</span></div>
              ) : activeLocks.map((l) => (
                <div className="insp-lock-row" key={l.lockId} title={l.resourceKey}>
                  <span className="insp-lock-key">{l.resourceKey}</span>
                  <span className="insp-lock-owner">{t("resource.lockHolder")}: {l.ownerWorker || "?"}</span>
                  {l.riskClass && <span className="insp-lock-risk">{l.riskClass}</span>}
                </div>
              ))}
            </div>
          )}
        </section>
      )}

      <section className={`insp-sec insp-sec-panels ${sectionOpen("panels") ? "" : "collapsed"}`}>
        {sectionHeader("panels", t("insp.run.panels"))}
        {sectionOpen("panels") && (
          <div className="insp-panels selection-glide-host">
            <SelectionGlider selectedKey={artifactOpen ? artifactView : ""} selector=".insp-panel-btn.on" className="grid" duration={280} />
            {panelBtn("evidence", "evidence", "layers")}
            {panelBtn("workers", "workers", "grid")}
            {panelBtn("graph", "graph", "network")}
            {panelBtn("timeline", "timeline", "list")}
            {panelBtn("blackboard", "blackboard", "board")}
            {panelBtn("findings", "findings", "alert")}
            {panelBtn("credentials", "credentials", "lock")}
            {panelBtn("pocs", "pocs", "terminal")}
            {panelBtn("routes", "routes", "network")}
            {panelBtn("directives", "directives", "help")}
            <button className="insp-panel-btn motion-panel-btn writeup" onClick={onWriteup} disabled={running}
              title={running ? "" : t("panelbtn.writeup")}>
              <span className="ico"><Icon name="pencil" size={15} /></span>
              <span className="insp-panel-label">{t("panelbtn.writeup")}</span>
            </button>
          </div>
        )}
      </section>
    </aside>
  );
}
