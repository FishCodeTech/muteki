"use client";

import { MotionIcon } from "@/components/MotionIcon";

import { useMemo, useState } from "react";
import type { CSSProperties, MouseEvent as ReactMouseEvent, ReactElement } from "react";
import {
  DeckState, SolverLane, isReviewWorkerLane, isFactRetired,
  workerIds,
  currentGenWorkerIds,
  type BlackboardVulnReport,
  type PlatformConfirmationStatus,
} from "@/lib/events";
import { useLang, useT } from "@/lib/i18n";
import { workerColor, workerEngine, workerEngineKey, resumeCommand, toWorkerIdentity, workerDisplayName, formatWorkerSubtitle, workerGeneration } from "@/lib/workers";
import { Icon, type IconName } from "@/components/Icon";
import { EngineLogo } from "@/components/EngineLogo";
import { useCopied } from "@/lib/useCopied";
import { Button, ListBox, ListBoxItem, Select } from "@heroui/react";
import {
  estimateCvss,
  findingClassLabel,
  reportLocationLabel,
  reportsToCollectionMarkdown,
  type CvssRating,
} from "@/lib/reportMarkdown";
import type { ArtifactView } from "@/lib/events";
import { panelHotkey } from "@/lib/runtimeTabs";

/**
 * The collapsible right-column run inspector (the redesign's floating inspector):
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
  "claude", "codex", "cursor", "pi", "omp", "kimi", "grok", "opencode", "devin",
];

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

function platformStatusLabel(
  status: PlatformConfirmationStatus,
  t: (key: string) => string,
): string {
  switch (status) {
    case "accepted":
      return t("insp.run.platformAccepted");
    case "pending":
      return t("insp.run.platformPending");
    case "rejected":
      return t("insp.run.platformRejected");
    case "internal":
      return t("insp.run.platformInternal");
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
    <Button
      type="button"
      className={`insp-report-export ${copied ? "copied" : ""}`.trim()}
      data-tooltip={t("insp.run.exportCollection")}
      aria-label={t("runtime.reports.copyCollectionAria")}
      onClick={() => copy(text)}
    >
      <MotionIcon active={copied} from="copy" to="check" size={13} />
      <span>{copied ? t("common.copied") : t("insp.run.exportCollection")}</span>
    </Button>
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
          <Button
            type="button"
            className="insp-report-row"
            key={row.id}
            onClick={() => onOpenReport(row.id)}
            data-tooltip={t("insp.run.openReport", { title: row.title })}
            aria-label={t("insp.run.openReport", { title: `${typeLabel} ${location}` })}
          >
            <span className="insp-report-row-top">
              <span className="insp-report-type">{typeLabel}</span>
              <span
                className={`artifact-badge ${severityBadgeClass(cvss.rating)}`}
                data-tooltip={t("runtime.reports.cvssHint")}
              >
                {cvss.badge}
              </span>
              <span className={`artifact-badge ${reportStatusBadge(row.status)}`}>
                {reportStatusLabel(row.status, t)}
              </span>
            </span>
            <code className="insp-report-path">{location}</code>
          </Button>
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
  onOpenAgent,
}: {
  lane: SolverLane;
  running: boolean;
  isWinner: boolean;
  facts: number;
  siblings: ReturnType<typeof toWorkerIdentity>[];
  onKill: (id: string) => void;
  onOpen: (id: string) => void;
  onOpenAgent?: (id: string) => void;
}) {
  const t = useT();
  const online = lane.online !== false;
  const engine = workerEngine(lane.solverId, lane.engine);
  const engineKey = workerEngineKey(lane.solverId, lane.engine);
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
      data-tooltip={`${engine} · ${reason}${runtime ? ` · ${runtime}` : ""}`}
      role="button"
      tabIndex={0}
      aria-label={t("insp.run.openWorker", { id: display.title })}
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); }
      }}
    >
      <span className="iwk-avatar" aria-hidden="true">
        {engineKey
          ? <EngineLogo engine={engineKey} size={16} />
          : display.initial}
      </span>
      <span className="iwk-meta">
        <span className="iwk-name" data-tooltip={display.titleAttr}>{display.title}</span>
        {workerGeneration(lane.solverId) > 1 && (
          <span className="iwk-gen" data-tooltip={lane.solverId}>g{workerGeneration(lane.solverId)}</span>
        )}
        <span className="iwk-sub">
          <span className="iwk-dot" />
          <span className="iwk-eng">{engine}</span>
          <span className="iwk-conn">{subtitle}</span>
          {isReview && <span className="worker-role-chip review">{t("worker.role.review")}</span>}
          {runtime && <span className="iwk-runtime">{runtime}</span>}
          {session && (
            <span className={`iwk-sess ${copied ? "copied" : ""}`} data-tooltip={t("insp.run.copySession") + ": " + resumeCmd}
              role="button" tabIndex={0} aria-label={t("insp.run.copySession")}
              onClick={copySession}
              onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); e.stopPropagation(); copy(resumeCmd); } }}>
              {copied ? <><Icon name="check" size={11} /> {t("common.copied")}</> : session.slice(0, 12)}
            </span>
          )}
        </span>
        <span className={`iwk-stat ${idle ? "idle" : ""}`} data-tooltip={t("insp.run.statTitle")}>
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
        <span className="iwk-paused" data-tooltip={t("worker.paused")}><Icon name="pause" size={13} /></span>
      )}
      {online && !lane.paused && lane.status === "stalled" && (
        <span className="iwk-stalled" data-tooltip={t("worker.stalled")}><Icon name="clock" size={13} /></span>
      )}
      {isWinner && <span className="iwk-win" data-tooltip={t("insp.run.winner")}><Icon name="flag" size={14} /></span>}
      {onOpenAgent && (
        <Button className="iwk-collab" data-tooltip={t("collab.action.viewOnMap")} aria-label={t("collab.action.viewOnMap")}
          onClick={(e) => { e.stopPropagation(); onOpenAgent(lane.solverId); }}><Icon name="network" size={13} /></Button>
      )}
      {running && online && (
        <Button className="iwk-kill" data-tooltip={t("worker.killTitle")} aria-label={t("worker.killTitle")}
          onClick={(e) => { e.stopPropagation(); onKill(lane.solverId); }}><Icon name="x" size={13} /></Button>
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
  onOpenAgent,
  onWriteup,
  onMarkFalseFlag,
  onOpenReport,
  onClose,
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
  onOpenAgent?: (id: string) => void;
  onWriteup: () => void;
  onMarkFalseFlag: (flag: string) => void;
  onOpenReport: (reportId: string) => void;
  onClose?: () => void;
}) {
  const t = useT();
  const { lang } = useLang();
  const [spawnEngine, setSpawnEngine] = useState("");
  const [collapsedSections, setCollapsedSections] = useState<Set<string>>(new Set());

  const acceptedReports = (deck.blackboard.vulnReports ?? []).filter((row) => row.status === "accepted");
  const qualifiedReports = acceptedReports.filter((row) => row.goalQualified !== false);
  const submittedReports = (deck.blackboard.vulnReports ?? []).filter((row) => row.status === "submitted").length;
  const reproducingReports = Math.max(0, deck.verifying ?? 0);
  const directoryReports = (deck.blackboard.vulnReports ?? [])
    .filter((row) => row.status !== "rejected")
    .slice()
    .sort((a, b) => reportStatusRank(a.status) - reportStatusRank(b.status) || a.ts - b.ts);
  const pentest = deck.mode === "pentest";
  const taskContract = deck.taskContract;
  const completionContract = !pentest ? "" : deck.completionKind === "coverage"
    ? t("insp.run.contractCoverage")
    : deck.completionKind === "count"
      ? t("insp.run.contractCount").replace("{count}", String(deck.expectedFindings))
      : deck.outcomePredicate === "shell_access"
        ? t("insp.run.contractShell")
        : deck.outcomePredicate === "admin_access"
          ? t("insp.run.contractAdmin")
          : deck.outcomePredicate === "command_execution"
            ? t("insp.run.contractCommand")
            : t("insp.run.contractReport");
  const reportCollectionTitle = deck.challengeName ? `${deck.challengeName} 漏洞报告集` : "漏洞报告集";
  const workerSiblings = useMemo(
    () => workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id])),
    [deck],
  );
  // E: active resource locks held across the swarm (site/account/listener)
  const activeLocks = (deck.resourceLocks ?? []).filter((l) => l.status === "active");
  // H: how many times the graph was compacted this run
  const compactEpochs = deck.compactEpochs ?? 0;
  const degradedEvents = deck.blackboard.events.filter((e) =>
    e.kind === "runtime_degraded" || e.kind === "worker_backend_degraded"
    || e.kind === "engagement_planner_degraded");
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
  // lives in page.tsx); both read lib/runtimeTabs.ts.
  const panelBtn = (view: ArtifactView, key: string, ico: IconName, full = false) => {
    const label = t(`panelbtn.${key}`);
    const hk = panelHotkey(view);
    const title = hk ? `${label} (${hk})` : label;
    return (
      <Button
        className={`insp-panel-btn ${full ? "full" : ""} ${artifactOpen && artifactView === view ? "on" : ""}`}
        aria-pressed={artifactOpen && artifactView === view}
        data-tooltip={title}
        aria-label={title}
        onClick={() => onOpenArtifact(view)}
      >
        <span className="ico"><Icon name={ico} size={15} /></span>
        <span className="insp-panel-label">{label}</span>
        {hk && <kbd className="insp-panel-kbd" aria-hidden="true">{hk}</kbd>}
      </Button>
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
  const sectionHeader = (key: string, label: string, aside?: ReactElement) => {
    const open = sectionOpen(key);
    return (
      <div className="insp-sec-h">
        <span>{label}</span>
        <span className="insp-sec-actions">
          {aside}
          <Button
            className={`insp-sec-toggle ${open ? "open" : ""}`}
            onClick={() => toggleSection(key)}
            aria-expanded={open}
            data-tooltip={t(open ? "insp.run.collapseSection" : "insp.run.expandSection")}
            aria-label={t(open ? "insp.run.collapseSection" : "insp.run.expandSection")}
          >
            <Icon name="chevronDown" size={13} />
          </Button>
        </span>
      </div>
    );
  };

  return (
    <aside className={`run-inspector lang-${lang} t-page-slide`} aria-label={t("insp.run.title")}>
      {onClose && (
        <div className="insp-dock">
          <span className="insp-dock-title">{t("insp.run.title")}</span>
          <Button
            size="sm"
            variant="ghost"
            isIconOnly
            className="icon-btn insp-dock-close"
            onPress={onClose}
            aria-label={t("insp.run.hide")}
            data-tooltip={t("insp.run.hide")}
          >
            <Icon name="panel" size={14} />
          </Button>
        </div>
      )}
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
      {(deck.outcomeReason === "runtime_failure" || deck.outcomeReason === "no_progress") && deck.outcomeDetail && (
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
      {taskContract && (
        <section className={`insp-sec insp-task-contract ${sectionOpen("task") ? "" : "collapsed"}`}>
          {sectionHeader("task", t("insp.run.taskUnderstanding"), (
            <span className="insp-flag-count">{taskContract.mode.toUpperCase()}</span>
          ))}
          {sectionOpen("task") && (
            <div className="insp-task-grid">
              <div><span>{t("insp.run.taskGoal")}</span><b>{taskContract.completion.goal || taskContract.rawInstruction}</b></div>
              {taskContract.executionTarget ? (
                <div><span>{t("insp.run.taskTarget")}</span><b>{taskContract.executionTarget}</b></div>
              ) : null}
              {taskContract.authorizationScope ? (
                <div><span>{t("insp.run.taskScope")}</span><b>{taskContract.authorizationScope}</b></div>
              ) : null}
              {taskContract.mode === "pentest" ? (
                <>
                  <div><span>{t("insp.run.taskType")}</span><b>{taskContract.completion.taskType || taskContract.category}</b></div>
                  <div><span>{t("insp.run.taskQuantity")}</span><b>{taskContract.completion.kind === "coverage" ? t("insp.run.taskCoverage") : String(taskContract.completion.quantity ?? 1)}</b></div>
                </>
              ) : (
                <div><span>{t("insp.run.taskFlagFormat")}</span><b>{taskContract.completion.flagFormatHint || taskContract.completion.flagFormat || t("insp.run.taskDefaultFlag")}</b></div>
              )}
              {taskContract.attachments.length > 0 && (
                <div><span>{t("insp.run.taskAttachments")}</span><b>{taskContract.attachments.map((item) => item.summary || item.name).join("；")}</b></div>
              )}
            </div>
          )}
        </section>
      )}
      <section className={`insp-sec insp-sec-outcome ${sectionOpen("outcome") ? "" : "collapsed"}`}>
        {sectionHeader("outcome", t(pentest ? "insp.run.reports" : "insp.run.flag"), pentest ? (
          deck.expectedFindings > 1 ? (
            <span className="insp-flag-count">
              {qualifiedReports.length}/{deck.expectedFindings}
              {(submittedReports > 0 || reproducingReports > 0) && (
                <small>{submittedReports}/{reproducingReports}/{qualifiedReports.length}</small>
              )}
            </span>
          ) : qualifiedReports.length > 0 || submittedReports > 0 || reproducingReports > 0 ? (
            <span className="insp-flag-count">
              {qualifiedReports.length}
              {(submittedReports > 0 || reproducingReports > 0) && (
                <small>{submittedReports}/{reproducingReports}/{qualifiedReports.length}</small>
              )}
            </span>
          ) : undefined
        ) : deck.platformConfirmationRequired ? (
          <span className="insp-flag-count" title={t("insp.run.internalFindings")}>{deck.flags.length}</span>
        ) : deck.expectedFlags > 1 ? (
          <span className="insp-flag-count">{deck.flags.length}/{deck.expectedFlags}</span>
        ) : undefined)}
        {sectionOpen("outcome") && (
          <>
            {!pentest && deck.platformConfirmationRequired && (
              <div className="insp-flag-summary" aria-live="polite">
                <div><span>{t("insp.run.internalFindings")}</span><strong>{deck.flags.length}</strong></div>
                <div><span>{t("insp.run.platformAccepted")}</span><strong>{(deck.flagConfirmations || []).filter((row) => row.status === "accepted").length}<small>/{Math.max(1, deck.expectedFlags)}</small></strong></div>
              </div>
            )}
            {pentest && (
              <div className="insp-pending-hint">
                {t("insp.run.completionContract")}：{completionContract}
                {acceptedReports.length > 0 && (
                  <> · {t("insp.run.reportGateProgress")
                    .replace("{accepted}", String(acceptedReports.length))
                    .replace("{qualified}", String(qualifiedReports.length))}</>
                )}
              </div>
            )}
            {pentest && directoryReports.length > 0 ? (
              <PentestReportDirectory
                rows={directoryReports}
                accepted={acceptedReports}
                collectionTitle={reportCollectionTitle}
                onOpenReport={onOpenReport}
              />
            ) : pentest ? (
              <div className="insp-run-flag pending t-flag-target">
                <span className="insp-pending-row"><Icon name="list" size={13} /> {t("insp.run.pendingReports")}</span>
                <span className="insp-pending-hint">{t("insp.run.pendingReportsHint")}</span>
              </div>
            ) : deck.flags.length > 0 ? (
              <div className="insp-run-flags">
                {deck.platformConfirmationRequired && (
                  <div className="insp-pending-hint">{t("insp.run.platformHint")}</div>
                )}
                {deck.flags.map((f, index) => {
                  const confirmation = (deck.flagConfirmations || []).find((row) => row.flag === f);
                  const status = confirmation?.status || "internal";
                  const statusLabel = platformStatusLabel(status, t);
                  return (
                  <div className="insp-flag-row" key={f}>
                    <div className="insp-flag-row-head">
                      <span className="insp-flag-index">{String(index + 1).padStart(2, "0")}</span>
                      {deck.platformConfirmationRequired && (
                        <span className={`insp-flag-status ${status}`}>{statusLabel}</span>
                      )}
                      {!running && (
                        <Button
                          type="button"
                          isIconOnly
                          variant="ghost"
                          className="insp-flag-false"
                          data-tooltip={t("quick.markFalseTitle")}
                          aria-label={t("quick.markFalseTitle")}
                          onClick={() => onMarkFalseFlag(f)}
                        >
                          <Icon name="xCircle" size={14} />
                        </Button>
                      )}
                    </div>
                    <Button size="sm" variant="ghost" className="copytext insp-run-flag t-flag-target" aria-label={t("common.copyFlagAria", { flag: f })} onPress={() => void navigator.clipboard.writeText(f)}>
                      <code>{f}</code><Icon name="copy" size={13} className="insp-flag-copy-icon" />
                    </Button>
                  </div>
                  );
                })}
              </div>
            ) : deck.outcomeReason === "goal_met" ? (
              <Button size="sm" variant="outline" className="copytext insp-run-flag goal t-flag-target" aria-label={t("common.copyAnswer")} onPress={() => void navigator.clipboard.writeText(deck.goalWhy || t("insp.run.goalMet"))}>
                {deck.goalWhy || t("insp.run.goalMet")}
              </Button>
            ) : (
              <div className="insp-run-flag pending t-flag-target">
                <span className="insp-pending-row"><Icon name="flag" size={13} /> {t("insp.run.pending")}</span>
                <span className="insp-pending-hint">{t("insp.run.pendingHint")}</span>
              </div>
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
                    onKill={onKillWorker} onOpen={onOpenWorker} onOpenAgent={onOpenAgent} />
                ))}
                {ids.length > ROSTER_CAP && (
                  <Button className="iwk-showall" onClick={() => setShowAll((v) => !v)}
                    aria-expanded={!capped}>
                    {capped
                      ? t("insp.run.showAll", { n: ids.length })
                      : t("insp.run.showLess")}
                  </Button>
                )}
              </div>
            )}
            {running && (
              <div className="iwk-spawn">
                <Select aria-label={t("workerDock.engine")} selectedKey={spawnEngine} onSelectionChange={(key) => setSpawnEngine(String(key ?? ""))}>
                  <Select.Trigger><Select.Value /></Select.Trigger>
                  <Select.Popover><ListBox><ListBoxItem id="" textValue={t("workerDock.auto")}>{t("workerDock.auto")}</ListBoxItem>{SPAWN_ENGINES.map((engine) => <ListBoxItem key={engine} id={engine} textValue={engine}>{engine}</ListBoxItem>)}</ListBox></Select.Popover>
                </Select>
                <Button className="iwk-spawn-btn" onClick={() => onSpawnWorker(spawnEngine || undefined)}
                  data-tooltip={t("workerDock.addTitle")}>＋ {t("workerDock.add")}</Button>
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
                <span className="insp-compact-badge" data-tooltip={t("meta.compactEpochs")}>
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
                <div className="insp-lock-row" key={l.lockId} data-tooltip={l.resourceKey}>
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
          <div className="insp-panels">
            <div className="insp-panel-group" role="group" aria-label={t("insp.run.panelGroup.observe")}>
              <span className="insp-panel-group-label">{t("insp.run.panelGroup.observe")}</span>
              <div className="insp-panel-grid">
                {panelBtn("timeline", "timeline", "list", true)}
                {panelBtn("workers", "workers", "grid")}
                {panelBtn("collaboration", "collaboration", "network")}
              </div>
            </div>
            <div className="insp-panel-group" role="group" aria-label={t("insp.run.panelGroup.investigate")}>
              <span className="insp-panel-group-label">{t("insp.run.panelGroup.investigate")}</span>
              <div className="insp-panel-grid">
                {panelBtn("evidence", "evidence", "layers")}
                {panelBtn("findings", "findings", "alert")}
              </div>
            </div>
            <div className="insp-panel-group" role="group" aria-label={t("insp.run.panelGroup.assets")}>
              <span className="insp-panel-group-label">{t("insp.run.panelGroup.assets")}</span>
              <div className="insp-panel-grid">
                {panelBtn("credentials", "credentials", "lock")}
                {panelBtn("pocs", "pocs", "terminal")}
                {panelBtn("routes", "routes", "network")}
                {panelBtn("directives", "directives", "help")}
              </div>
            </div>
            <Button
              className="insp-panel-writeup"
              onClick={onWriteup}
              isDisabled={running}
              data-tooltip={running ? t("insp.run.writeupBusy") : t("insp.run.writeupHint")}
              aria-label={`${t("panelbtn.writeup")} — ${running ? t("insp.run.writeupBusy") : t("insp.run.writeupHint")}`}
            >
              <span className="insp-panel-writeup-icon"><Icon name="pencil" size={15} /></span>
              <span className="insp-panel-writeup-copy">
                <strong>{t("panelbtn.writeup")}</strong>
                <small>{running ? t("insp.run.writeupBusy") : t("insp.run.writeupHint")}</small>
              </span>
              <Icon name="chevronRight" size={14} />
            </Button>
          </div>
        )}
      </section>
    </aside>
  );
}
