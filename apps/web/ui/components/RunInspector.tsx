"use client";

import { useMemo, useState } from "react";
import type { CSSProperties, MouseEvent as ReactMouseEvent } from "react";
import {
  DeckState, SolverLane, isReviewWorkerLane, isFactRetired,
  verifiedFactTexts, candidateFactTexts, openIntentTexts, deadEndTexts, workerIds,
} from "@/lib/events";
import { useLang, useT } from "@/lib/i18n";
import { workerColor, workerEngine, workerInitial, resumeCommand } from "@/lib/workers";
import { Icon, type IconName } from "@/components/Icon";
import { useCopied } from "@/lib/useCopied";
import { CopyText } from "@/components/CopyText";
import type { ArtifactView } from "@/components/ArtifactPanel";

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

const SPAWN_ENGINES = ["cursor", "claude", "codex"];

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
  onKill,
  onOpen,
}: {
  lane: SolverLane;
  running: boolean;
  isWinner: boolean;
  facts: number;
  onKill: (id: string) => void;
  onOpen: (id: string) => void;
}) {
  const t = useT();
  const online = lane.online !== false;
  const engine = workerEngine(lane.solverId, lane.engine);
  const color = workerColor(lane.solverId, lane.engine);
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
      aria-label={t("insp.run.openWorker", { id: lane.solverId })}
      onClick={open}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); }
      }}
    >
      <span className="iwk-avatar">{workerInitial(lane.solverId)}</span>
      <span className="iwk-meta">
        <span className="iwk-name">{lane.solverId}</span>
        <span className="iwk-sub">
          <span className="iwk-dot" />
          <span className="iwk-eng">{engine}</span>
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
}) {
  const t = useT();
  const { lang } = useLang();
  const [spawnEngine, setSpawnEngine] = useState("");
  const [collapsedSections, setCollapsedSections] = useState<Set<string>>(new Set());
  // two-tab split keeps the column short: "outcome" = flag + panel launcher
  // (the things an operator captures / navigates with), "runtime" = the live
  // worker roster + resource locks. Only one tab's regions render at a time, so
  // the FLAG is always first-paint visible and nothing can push it out of view.
  const [tab, setTab] = useState<"outcome" | "runtime">("outcome");

  const verified = verifiedFactTexts(deck).length;
  const candidates = candidateFactTexts(deck).length;
  const intents = openIntentTexts(deck).length;
  const deads = deadEndTexts(deck).length;
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

  // roster summary + cap-with-expand
  const ROSTER_CAP = 12;
  const onlineCount = ids.filter((id) => laneFor(id).online !== false).length;
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
      <div className="insp-tabs" role="tablist" aria-label={t("insp.run.title")}>
        <button
          className={`insp-tab ${tab === "outcome" ? "on" : ""}`}
          role="tab"
          aria-selected={tab === "outcome"}
          onClick={() => setTab("outcome")}
        >
          <Icon name="flag" size={13} />
          {t("insp.tab.outcome")}
          {deck.flags.length > 0 && <span className="insp-tab-badge">{deck.flags.length}</span>}
        </button>
        <button
          className={`insp-tab ${tab === "runtime" ? "on" : ""}`}
          role="tab"
          aria-selected={tab === "runtime"}
          onClick={() => setTab("runtime")}
        >
          <Icon name="cpu" size={13} />
          {t("insp.tab.runtime")}
          {onlineCount > 0 && <span className="insp-tab-badge live">{onlineCount}</span>}
        </button>
      </div>
      {tab === "outcome" && (<>
      <section className={`insp-sec insp-sec-outcome ${sectionOpen("outcome") ? "" : "collapsed"}`}>
        {sectionHeader("outcome", t("insp.run.flag"), deck.expectedFlags > 1 ? (
          <span className="insp-flag-count">{deck.flags.length}/{deck.expectedFlags}</span>
        ) : undefined)}
        {sectionOpen("outcome") && (
          <>
            {deck.flags.length > 0 ? (
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
            <div className="insp-chips">
              <span className="insp-chip verified motion-feedback"><Icon name="check" size={13} /> {t("meta.verified")} <b>{verified}</b></span>
              <span className="insp-chip candidates motion-feedback"><Icon name="help" size={13} /> {t("meta.candidates")} <b>{candidates}</b></span>
              <span className="insp-chip intents motion-feedback"><Icon name="crosshair" size={13} /> {t("meta.intents")} <b>{intents}</b></span>
              <span className="insp-chip dead motion-feedback"><Icon name="xCircle" size={13} /> {t("meta.dead")} <b>{deads}</b></span>
              <span className="insp-chip cost motion-feedback">$ <b>{deck.usd.toFixed(4)}</b></span>
            </div>
          </>
        )}
      </section>
      </>)}

      {tab === "runtime" && (<>
      <section className={`insp-sec insp-sec-workers ${sectionOpen("workers") ? "" : "collapsed"}`}>
        {sectionHeader("workers", t("insp.run.workers"), ids.length > 0 ? (
            <span className="iwk-summary">
              {t("insp.run.rosterSummary", { online: onlineCount, total: ids.length, solved: solvedCount })}
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
      </>)}

      {tab === "outcome" && (
      <section className={`insp-sec insp-sec-panels ${sectionOpen("panels") ? "" : "collapsed"}`}>
        {sectionHeader("panels", t("insp.run.panels"))}
        {sectionOpen("panels") && (
          <div className="insp-panels">
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
      )}
    </aside>
  );
}
