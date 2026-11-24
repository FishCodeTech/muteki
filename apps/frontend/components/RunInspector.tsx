"use client";

import { useMemo, useState } from "react";
import type { CSSProperties, ReactElement, ReactNode } from "react";
import {
  DeckState, SolverLane, formatFollowupProgressDetail, isReviewWorkerLane, isFactRetired,
  workerIds,
  currentGenWorkerIds,
  type PlatformConfirmationStatus,
} from "@/lib/events";
import { useLang, useT } from "@/lib/i18n";
import { workerColor, workerEngine, workerEngineKey, resumeCommand, toWorkerIdentity, workerDisplayName, workerGeneration } from "@/lib/workers";
import { Icon, type IconName } from "@muteki/ui";
import { EngineLogo } from "@/components/EngineLogo";
import { useCopied } from "@/lib/useCopied";
import { useCopyText } from "@/lib/useCopyText";
import { Button, Menu, MenuItem, MenuSeparator } from "@muteki/ui";
import type { ArtifactView } from "@/lib/events";
import { useWorkerSpawnPending } from "@/lib/useRun";
import { useRunUsageLedger } from "@/lib/useRunUsageLedger";
import { summarizeRunCost } from "@/lib/runInspectorMetrics";
import { providerWorkerEngines, readyCatalog, useProviderDescriptors } from "@/lib/providerDescriptors";
import {
  LocalPiMaterialsDetails,
  LocalPiOverviewDetails,
  localPiMaterialsMeta,
  localPiSessionMeta,
  useLocalPiInspector,
} from "@/components/run-view/LocalPiInspector";
import { RunUsageSummaryCard } from "@/components/run-view/RunUsageSummaryCard";


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

function runtimeLabel(lane: SolverLane): string {
  const runtime = lane.runtime;
  if (!runtime?.backend) return "";
  const status = runtime.status ? `:${runtime.status}` : "";
  return `${runtime.backend}${status}`;
}

type Activity = { kind: "tool" | "think" | "review" | "wait" | "warn" | "ended" | "solved"; icon: IconName; text: string };

function laneActivity(lane: SolverLane, t: (key: string) => string): Activity {
  const status = (lane.status || "").trim();
  if (lane.solved) return { kind: "solved", icon: "flag", text: t("rv.act.solved") };
  if (lane.online === false) {
    const reason = (lane.statusReason || "").toLowerCase();
    if (reason === "killed" || status === "killed") return { kind: "ended", icon: "x", text: t("rv.act.killed") };
    if (reason === "error") return { kind: "warn", icon: "alert", text: t("rv.act.error") };
    return { kind: "ended", icon: "check", text: t("rv.act.ended") };
  }
  if (lane.paused) return { kind: "warn", icon: "pause", text: t("rv.act.paused") };
  if (status === "stalled") return { kind: "warn", icon: "clock", text: t("rv.act.stalled") };
  if (status.startsWith("tool:")) return { kind: "tool", icon: "terminal", text: status.slice(5).trim() };
  if (status === "reviewing" || isReviewWorkerLane(lane)) return { kind: "review", icon: "eye", text: t("rv.act.reviewing") };
  if (status === "thinking") return { kind: "think", icon: "sparkles", text: t("rv.act.thinking") };
  return { kind: "wait", icon: "dot", text: t("rv.act.idle") };
}

function WorkerRow({
  lane,
  running,
  isWinner,
  facts,
  siblings,
  onToggleFreezeWorker,
  onKill,
  onOpen,
  onOpenAgent,
}: {
  lane: SolverLane;
  running: boolean;
  isWinner: boolean;
  facts: number;
  siblings: ReturnType<typeof toWorkerIdentity>[];
  onToggleFreezeWorker?: (id: string, paused: boolean) => void;
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
  const session = lane.session;
  const resumeCmd = session ? resumeCommand(engine, session) : "";
  const runtime = runtimeLabel(lane);
  const [copied, copy] = useCopied();
  const isReview = isReviewWorkerLane(lane);
  const activity = laneActivity(lane, t);
  const generation = workerGeneration(lane.solverId);
  const open = () => onOpen(lane.solverId);
  const tooltip = [display.titleAttr || lane.solverId, engine, runtime].filter(Boolean).join(" · ");
  return (
    <div
      className={`rv-wk ${online ? "online" : "offline"} ${isWinner ? "winner" : ""} act-${activity.kind}`}
      style={{ "--wc": color } as CSSProperties}
      data-tooltip={tooltip}
      role="button"
      tabIndex={0}
      aria-label={t("insp.run.openWorker", { id: display.title })}
      onClick={open}
      onKeyDown={(e) => {
        if (e.target !== e.currentTarget) return;
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); }
      }}
    >
      <span className="rv-wk-avatar" aria-hidden="true">
        {engineKey ? <EngineLogo engine={engineKey} size={15} /> : display.initial}
        {online && <span className="rv-wk-pulse" />}
      </span>
      <span className="rv-wk-main">
        <span className="rv-wk-line">
          <span className="rv-wk-name">{display.title}</span>
          {generation > 1 && <span className="rv-wk-tag">g{generation}</span>}
          {isReview && <span className="rv-wk-tag review">{t("worker.role.review")}</span>}
          {display.model && <span className="rv-wk-model">{display.model}</span>}
        </span>
        <span className={`rv-wk-act kind-${activity.kind}`}>
          <Icon name={activity.icon} size={11} />
          <span className={activity.kind === "tool" ? "rv-wk-cmd" : undefined}>{activity.text}</span>
        </span>
      </span>
      <span className="rv-wk-side">
        {facts > 0 && <span className="rv-wk-facts" data-tooltip={t("rv.insp.facts", { n: facts })}><Icon name="check" size={10} />{facts}</span>}
        {isWinner && <span className="rv-wk-win" data-tooltip={t("insp.run.winner")}><Icon name="flag" size={13} /></span>}
        <span className="rv-wk-actions">
          {running && online && onToggleFreezeWorker && <Button iconOnly size="sm" variant="ghost" className="rv-wk-btn"
            aria-label={lane.paused ? "解冻" : "冻结"} data-tooltip={lane.paused ? "解冻" : "冻结"}
            onClick={event => { event.stopPropagation(); onToggleFreezeWorker(lane.solverId, Boolean(lane.paused)); }}>
            <Icon name={lane.paused ? "play" : "lock"} size={12} />
          </Button>}
          {resumeCmd && (
            <Button
              iconOnly size="sm" variant="ghost" className="rv-wk-btn"
              data-tooltip={copied ? t("common.copied") : `${t("rv.insp.resume")}: ${resumeCmd}`}
              aria-label={t("rv.insp.resume")}
              onClick={(e) => { e.stopPropagation(); copy(resumeCmd); }}
            >
              <Icon name={copied ? "check" : "terminal"} size={12} />
            </Button>
          )}
          {onOpenAgent && (
            <Button
              iconOnly size="sm" variant="ghost" className="rv-wk-btn"
              data-tooltip={t("collab.action.viewOnMap")} aria-label={t("collab.action.viewOnMap")}
              onClick={(e) => { e.stopPropagation(); onOpenAgent(lane.solverId); }}
            >
              <Icon name="network" size={12} />
            </Button>
          )}
          {running && online && (
            <Button
              iconOnly size="sm" variant="ghost" agentConfirm className="rv-wk-btn danger"
              data-tooltip={t("worker.killTitle")} aria-label={t("worker.killTitle")}
              onClick={(e) => { e.stopPropagation(); onKill(lane.solverId); }}
            >
              <Icon name="x" size={12} />
            </Button>
          )}
        </span>
      </span>
    </div>
  );
}

export function RunInspector({
  deck,
  running,
  onOpenArtifact,
  onSpawnWorker,
  onKillWorker,
  onOpenWorker,
  onOpenAgent,
  onOpenWriteup, onToggleFreezeWorker,
  onOpenReport,
  onMarkFalseFlag,
  onClose,
}: {
  deck: DeckState;
  running: boolean;
  onOpenArtifact: (v: ArtifactView) => void;
  onSpawnWorker: (engine?: string) => void;
  onKillWorker: (id: string) => void;
  onOpenWorker: (id: string) => void;
  onOpenAgent?: (id: string) => void;
  onOpenWriteup?: () => void;
  onToggleFreezeWorker?: (id: string, paused: boolean) => void;
  onOpenReport?: () => void;
  onMarkFalseFlag: (flag: string) => void;
  onClose?: () => void;
}) {
  const t = useT();
  const isPentest = deck.mode === "pentest";
  const isLocalPi = deck.driverKind === "local_pi";
  const copyText = useCopyText();
  const { lang } = useLang();
  const spawnPending = useWorkerSpawnPending(deck.runId);
  const spawnEngines = providerWorkerEngines(readyCatalog(useProviderDescriptors()));
  const [collapsedSections, setCollapsedSections] = useState<Set<string>>(() => new Set(["task"]));
  const [showEnded, setShowEnded] = useState(false);
  const localPi = useLocalPiInspector(
    deck.runId, isLocalPi, running,
    `${deck.localPiSession?.turnCount ?? deck.localPiSession?.turn_count ?? 0}:${deck.finished}:${deck.flags.length}`,
  );
  const { data: usage, error: usageError } = useRunUsageLedger(
    deck.runId,
    undefined,
    running,
    `${deck.finished}:${deck.tokensIn + deck.tokensOut}:${Object.keys(deck.costBySolver).length}:${deck.localPiSession?.turnCount ?? deck.localPiSession?.turn_count ?? 0}`,
  );
  const swarmFallbackUsage =
    deck.tokensIn > 0 || deck.tokensOut > 0
      ? {
          input_tokens: deck.tokensIn,
          output_tokens: deck.tokensOut,
          total_tokens: deck.tokensIn + deck.tokensOut,
        }
      : null;
  const isEn = lang === "en";
  const swarmCostSummary = useMemo(
    () => summarizeRunCost(usage?.totals, usage?.buckets?.solve, usage?.buckets?.auxiliary, isEn),
    [usage, isEn],
  );

  const taskContract = deck.taskContract;
  const pentestFactCount = deck.blackboard.facts.filter((fact) => fact.verified && fact.actor !== "origin").length;
  const workerSiblings = useMemo(
    () => workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id])),
    [deck],
  );
  const activeLocks = (deck.resourceLocks ?? []).filter((l) => l.status === "active");
  const compactEpochs = deck.compactEpochs ?? 0;
  const degradedEvents = deck.blackboard.events.filter((e) =>
    e.kind === "runtime_degraded" || e.kind === "worker_backend_degraded"
    || e.kind === "engagement_planner_degraded");
  const degradedEngines = Object.entries(deck.degradedEngines || {});

  const rawIds = useMemo(() => workerIds(deck), [deck]);
  const laneFor = (id: string): SolverLane => deck.lanes[id] || {
    solverId: id, reasoning: "", toolLines: [], status: running ? "waiting" : "done",
    solved: false, online: !deck.finished,
  };
  const winnerId = rawIds.find((id) => laneFor(id).solved);

  const verifiedByActor = useMemo(() => {
    const m = new Map<string, number>();
    for (const f of deck.blackboard.facts) {
      if (f.verified && !isFactRetired(f)) m.set(f.actor, (m.get(f.actor) || 0) + 1);
    }
    return m;
  }, [deck.blackboard.facts]);

  const ids = useMemo(() => {
    const lane = (id: string): SolverLane => deck.lanes[id] || {
      solverId: id, reasoning: "", toolLines: [], status: "waiting",
      solved: false, online: !deck.finished,
    };
    const rank = (id: string): number => {
      const l = lane(id);
      if (l.solved) return 3;
      const facts = verifiedByActor.get(id) || 0;
      if (facts > 0 || l.toolLines.length > 0) return 2;
      return 1;
    };
    return rawIds
      .map((id, i) => ({ id, i }))
      .sort((a, b) => {
        const ra = rank(a.id), rb = rank(b.id);
        if (ra !== rb) return rb - ra;
        const fa = verifiedByActor.get(a.id) || 0, fb = verifiedByActor.get(b.id) || 0;
        if (fa !== fb) return fb - fa;
        const ta = lane(a.id).toolLines.length, tb = lane(b.id).toolLines.length;
        if (ta !== tb) return tb - ta;
        return a.i - b.i;
      })
      .map((e) => e.id);
  }, [rawIds, deck.lanes, deck.finished, verifiedByActor]);

  const curIds = currentGenWorkerIds(deck);
  const onlineCount = curIds.filter((id) => laneFor(id).online !== false).length;
  const activeIds = ids.filter((id) => laneFor(id).online !== false || laneFor(id).solved);
  const endedIds = ids.filter((id) => !activeIds.includes(id));
  const foldEnded = running && activeIds.length > 0;
  const shownIds = foldEnded ? activeIds : ids;

  const sectionOpen = (key: string) => !collapsedSections.has(key);
  const toggleSection = (key: string) => {
    setCollapsedSections((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };
  const section = (key: string, label: string, body: ReactNode, aside?: ReactElement | null, className = "") => {
    const open = sectionOpen(key);
    return (
      <section className={`rv-sec rv-sec-${key} ${open ? "" : "collapsed"} ${className}`}>
        <div className="rv-sec-h">
          <button type="button" className="rv-sec-toggle" onClick={() => toggleSection(key)} aria-expanded={open}>
            <Icon name="chevronDown" size={12} className="rv-sec-chev" />
            <span>{label}</span>
          </button>
          {aside}
        </div>
        {open && <div className="rv-sec-body">{body}</div>}
      </section>
    );
  };

  const flagFormat = taskContract?.completion.flagFormatHint || taskContract?.completion.flagFormat || "";
  const workerRow = (id: string) => (
    <WorkerRow key={id} lane={laneFor(id)} running={running}
      isWinner={id === winnerId} facts={verifiedByActor.get(id) || 0}
      siblings={workerSiblings}
      onToggleFreezeWorker={onToggleFreezeWorker} onKill={onKillWorker} onOpen={onOpenWorker} onOpenAgent={onOpenAgent} />
  );

  const writeupGenerating = deck.followupPending && (
    deck.activeFollowupKind === "writeup"
    || deck.chat.some((m) => m.followupId === deck.activeFollowupJobId && m.followupKind === "writeup")
  );
  const reportGenerating = Boolean(deck.reportGenerating);
  const writeupDetail = formatFollowupProgressDetail(deck.followupProgress) || "正在整理证据与解题过程";

  const outcomeBody = isPentest ? (
    <div className="insp-pentest-outcome">
      <strong className={deck.solved ? "confirmed" : ""}>{deck.solved ? "目标已证实" : deck.finished ? "目标尚未证实" : "正在核对目标"}</strong>
      <span>授权范围：{taskContract?.authorizationScope || deck.target || "待读取"}</span>
      <span>真实证据：{pentestFactCount} 条 Fact</span>
      {reportGenerating ? (
        <div className="insp-panel-writeup is-generating" role="status" data-report-generating="true">
          <span className="insp-panel-writeup-icon"><Icon name="loader" size={15} className="cx-spin" /></span>
          <span className="insp-panel-writeup-copy">
            <strong>正在生成测试报告…</strong>
            <small>正在汇总已验证证据与报告内容</small>
          </span>
        </div>
      ) : onOpenReport ? <Button className="insp-panel-writeup" onClick={onOpenReport}>
        <span className="insp-panel-writeup-icon"><Icon name="rows" size={15} /></span>
        <span className="insp-panel-writeup-copy"><strong>查看测试报告</strong><small>目标判断、证据与定制内容</small></span>
        <Icon name="chevronRight" size={14} />
      </Button> : null}
    </div>
  ) : (
    <>
      {!isPentest && writeupGenerating ? (
        <div className="insp-panel-writeup is-generating" role="status" data-writeup-generating="true">
          <span className="insp-panel-writeup-icon"><Icon name="loader" size={15} className="cx-spin" /></span>
          <span className="insp-panel-writeup-copy">
            <strong>正在生成 Writeup…</strong>
            <small>{writeupDetail}</small>
          </span>
        </div>
      ) : !isPentest && deck.hasWriteup && onOpenWriteup ? (
        <Button className="insp-panel-writeup" onClick={() => onOpenWriteup()}>
          <span className="insp-panel-writeup-icon"><Icon name="file" size={15} /></span>
          <span className="insp-panel-writeup-copy">
            <strong>查看 Writeup</strong>
            <small>前往 Writeup 工作区查看与编辑</small>
          </span>
          <Icon name="arrowUpRight" size={14} />
        </Button>
      ) : null}
      {deck.platformConfirmationRequired && (
        <div className="insp-flag-summary" aria-live="polite">
          <div><span>{t("insp.run.internalFindings")}</span><strong>{deck.flags.length}</strong></div>
          <div><span>{t("insp.run.platformAccepted")}</span><strong>{(deck.flagConfirmations || []).filter((row) => row.status === "accepted").length}<small>/{Math.max(1, deck.expectedFlags)}</small></strong></div>
        </div>
      )}
      {deck.flags.length > 0 ? (
        <div className="rv-flags">
      {deck.platformConfirmationRequired && (
            <div className="rv-hint">{t("insp.run.platformHint")}</div>
          )}
          {deck.flags.map((f) => {
            const confirmation = (deck.flagConfirmations || []).find((row) => row.flag === f);
            const status = confirmation?.status || "internal";
            return (
              <div className="rv-flag" key={f}>
                <Button size="sm" variant="ghost" className="copytext rv-flag-value t-flag-target" aria-label={t("common.copyFlagAria", { flag: f })} onClick={() => copyText(f)}>
                  <code>{f}</code><Icon name="copy" size={12} />
                </Button>
      {deck.platformConfirmationRequired && (
                  <span className={`insp-flag-status ${status}`}>{platformStatusLabel(status, t)}</span>
                )}
                {!running && (
                  <Button
                    type="button" iconOnly size="sm" variant="ghost" className="rv-flag-false"
                    data-tooltip={t("quick.markFalseTitle")} aria-label={t("quick.markFalseTitle")}
                    onClick={() => onMarkFalseFlag(f)}
                  >
                    <Icon name="xCircle" size={13} />
                  </Button>
                )}
              </div>
            );
          })}
        </div>
      ) : deck.outcomeReason === "goal_met" ? (
        <Button size="sm" variant="outline" className="copytext insp-run-flag goal t-flag-target" aria-label={t("common.copyAnswer")} onClick={() => copyText(deck.goalWhy || t("insp.run.goalMet"))}>
          {deck.goalWhy || t("insp.run.goalMet")}
        </Button>
      ) : (
        <div className="rv-pending t-flag-target" data-tooltip={t("insp.run.pendingHint")}>
          <Icon name="flag" size={13} />
          <span>{deck.finished ? t("rv.insp.noFlagFinished") : t("rv.insp.noFlag")}</span>
          {flagFormat && <code className="rv-pending-format">{t("rv.insp.format", { format: flagFormat })}</code>}
        </div>
      )}
    </>
  );

  return (
    <aside className={`run-inspector rv-insp lang-${lang} t-page-slide`} aria-label={t("insp.run.title")}>
      {onClose && (
        <div className="insp-dock">
          <span className="insp-dock-title">{t("rv.insp.title")}</span>
          <Button
            size="sm"
            variant="ghost"
            iconOnly
            className="icon-btn insp-dock-close"
            onClick={onClose}
            aria-label={t("insp.run.hide")}
            data-tooltip={t("insp.run.hide")}
          >
            <Icon name="panelRightClose" size={14} />
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

      {section(
        "outcome",
        isPentest ? "目标与证据" : t("insp.run.flag"),
        outcomeBody,
        isPentest ? <span className="rv-sec-meta">{pentestFactCount} Fact</span>
          : deck.expectedFlags > 1 ? <span className="rv-sec-meta">{deck.flags.length}/{deck.expectedFlags}</span>
            : null,
      )}

      {isLocalPi && section(
        "piSession",
        t("rv.insp.contextAndCost"),
        localPi.snapshot ? (
          <LocalPiOverviewDetails
            snapshot={localPi.snapshot}
            usage={usage}
            usageError={usageError}
            en={isEn}
          />
        ) : (
          <div className="rv-empty">{localPi.error ? `读取失败：${localPi.error}` : "正在读取会话信息…"}</div>
        ),
        localPi.snapshot ? (
          <span className="rv-sec-meta">{localPiSessionMeta(localPi.snapshot, deck.solved, isEn)}</span>
        ) : null,
      )}
      {isLocalPi && localPi.error && localPi.snapshot && (
        <div className="rv-hint text-cx-danger" role="status">会话信息刷新失败：{localPi.error}</div>
      )}
      {isLocalPi && localPi.snapshot && section(
        "piMaterials",
        t("rv.insp.filesAndArtifacts"),
        <LocalPiMaterialsDetails snapshot={localPi.snapshot} en={isEn} />,
        <span className="rv-sec-meta">{localPiMaterialsMeta(localPi.snapshot, isEn)}</span>,
      )}

      {!isLocalPi && section(
        "workers",
        t("rv.insp.workers"),
        <>
          {ids.length === 0 ? (
            <div className="rv-empty">{t("insp.run.noWorkersHint")}</div>
          ) : (
            <div className="rv-wk-list">
              {shownIds.map(workerRow)}
              {foldEnded && endedIds.length > 0 && (
                <>
                  <button type="button" className="rv-wk-fold" aria-expanded={showEnded} onClick={() => setShowEnded((value) => !value)}>
                    <Icon name="chevronDown" size={12} className="rv-sec-chev" />
                    <span>{t("rv.insp.ended", { n: endedIds.length })}</span>
                  </button>
                  {showEnded && endedIds.map(workerRow)}
                </>
              )}
            </div>
          )}
          {running && (
            <Menu
              placement="bottom-start"
              ariaLabel={t("workerDock.addTitle")}
              trigger={(
                <button type="button" className="rv-spawn-btn" disabled={spawnPending} aria-busy={spawnPending || undefined}>
                  <Icon name="plus" size={13} />
                  <span>{spawnPending ? t("workerDock.adding") : t("workerDock.add")}</span>
                </button>
              )}
            >
              <MenuItem icon="sparkles" onSelect={() => onSpawnWorker(undefined)}>{t("workerDock.auto")}</MenuItem>
              {spawnEngines.length > 0 && <MenuSeparator />}
              {spawnEngines.map((engine) => (
                <MenuItem key={engine} description={engine} onSelect={() => onSpawnWorker(engine)}>
                  {workerEngine(engine, engine)}
                </MenuItem>
              ))}
            </Menu>
          )}
        </>,
        ids.length > 0 ? (
          <button type="button" className="rv-sec-link" onClick={() => onOpenArtifact("workers")} data-tooltip={t("rv.insp.openWorkers")} aria-label={t("rv.insp.openWorkers")}>
            {t("rv.insp.online", { online: onlineCount, total: curIds.length })}
            <Icon name="chevronRight" size={12} />
          </button>
        ) : null,
      )}

      {!isLocalPi && section(
        "usage",
        t("rv.insp.costAndUsage"),
        <RunUsageSummaryCard
          ledger={usage?.totals}
          solve={usage?.buckets?.solve}
          auxiliary={usage?.buckets?.auxiliary}
          workerGroups={usage?.groups?.worker_id}
          error={usageError}
          fallbackUsage={swarmFallbackUsage}
          en={isEn}
          showWorkerBreakdown
        />,
        <span className="rv-sec-meta">{swarmCostSummary.primary}</span>,
      )}

      {isPentest && taskContract && section(
        "task",
        t("rv.insp.task"),
        <dl className="rv-task">
          {taskContract.completion.goal && (
            <div><dt>{t("insp.run.taskGoal")}</dt><dd>{taskContract.completion.goal}</dd></div>
          )}
          {taskContract.executionTarget && (
            <div><dt>{t("insp.run.taskTarget")}</dt><dd><code>{taskContract.executionTarget}</code></dd></div>
          )}
          {taskContract.authorizationScope && (
            <div><dt>{t("insp.run.taskScope")}</dt><dd>{taskContract.authorizationScope}</dd></div>
          )}
          {taskContract.attachments.length > 0 && (
            <div><dt>{t("insp.run.taskAttachments")}</dt><dd>{taskContract.attachments.map((item) => item.summary || item.name).join("、")}</dd></div>
          )}
        </dl>,
      )}

      {(activeLocks.length > 0 || compactEpochs > 0) && section(
        "locks",
        t("resource.lockActive"),
        <div className="insp-locks">
          {activeLocks.map((l) => (
            <div className="insp-lock-row" key={l.lockId} data-tooltip={l.resourceKey}>
              <span className="insp-lock-key">{l.resourceKey}</span>
              <span className="insp-lock-owner">{t("resource.lockHolder")}: {l.ownerWorker || "?"}</span>
              {l.riskClass && <span className="insp-lock-risk">{l.riskClass}</span>}
            </div>
          ))}
        </div>,
        <span className="rv-sec-meta">
          {activeLocks.length > 0 && <span>{activeLocks.length}</span>}
          {compactEpochs > 0 && (
            <span className="insp-compact-badge" data-tooltip={t("meta.compactEpochs")}>
              {t("insp.compactBadge")} ×{compactEpochs}
            </span>
          )}
        </span>,
      )}
    </aside>
  );
}
