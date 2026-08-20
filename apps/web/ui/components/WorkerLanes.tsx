"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { DeckState, SolverLane, isReviewWorkerLane, isVerifierWorkerLane, workerChat, workerIds, currentGenWorkerIds } from "@/lib/events";
import type { SwarmDigest } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { workerColor, workerEngine, toWorkerIdentity, workerDisplayName, workerGeneration } from "@/lib/workers";
import { compactLaneStatus, latestLaneActivity, laneStatusKind, rosterGroup } from "@/lib/workerLanePresentation";
import type { RosterGroup } from "@/lib/workerLanePresentation";
import { Icon } from "@/components/Icon";
import { PanelEmpty } from "@/components/PanelEmpty";

/**
 * Worker roster: a compact control list (who is working / stuck / done).
 * Event content lives in the activity stream — a row click jumps there.
 */

const SPAWN_ENGINES = [
  "claude", "codex", "cursor", "pi", "omp", "kimi", "grok", "opencode", "dsh",
];

export function WorkerSpawnControl({
  running,
  onSpawnWorker,
}: {
  running: boolean;
  onSpawnWorker: (engine?: string) => void;
}) {
  const t = useT();
  const [spawnEngine, setSpawnEngine] = useState("");
  if (!running) return null;
  return (
    <div className="wlane-spawn">
      <select value={spawnEngine} onChange={(e) => setSpawnEngine(e.target.value)} title={t("workerDock.engine")}>
        <option value="">{t("workerDock.auto")}</option>
        {SPAWN_ENGINES.map((engine) => <option key={engine} value={engine}>{engine}</option>)}
      </select>
      <button className="wlane-spawn-btn" onClick={() => onSpawnWorker(spawnEngine || undefined)}
        title={t("workerDock.addTitle")}>＋ {t("workerDock.add")}</button>
    </div>
  );
}

export function WorkerLanes({
  deck,
  running,
  focusWorker,
  onSpawnWorker,
  onKillWorker,
  onOpenSpeakerTimeline,
  phase,
  elapsed,
  calls,
}: {
  deck: DeckState;
  running: boolean;
  focusWorker?: { id: string; nonce: number } | null;
  onSpawnWorker: (engine?: string) => void;
  onKillWorker: (id: string) => void;
  onOpenSpeakerTimeline?: (id: string) => void;
  phase: SwarmDigest["phase"];
  elapsed?: string;
  calls: number;
}) {
  const t = useT();
  const [onlyAnomaly, setOnlyAnomaly] = useState(false);
  const [doneOpen, setDoneOpen] = useState(false);
  const allIds = useMemo(() => workerIds(deck), [deck]);
  // Headcount = current execution generation only: a continued run (resolve)
  // keeps the previous generation's finished workers listed, but they are not
  // roster members of the live generation.
  const curIds = useMemo(() => currentGenWorkerIds(deck), [deck]);
  const lastNonce = useRef<number | null>(null);
  const rowRefs = useRef(new Map<string, HTMLButtonElement>());

  useEffect(() => {
    if (!focusWorker || focusWorker.nonce === lastNonce.current) return;
    lastNonce.current = focusWorker.nonce;
    setOnlyAnomaly(false);
    setDoneOpen(true);
    requestAnimationFrame(() => {
      rowRefs.current.get(focusWorker.id)?.scrollIntoView({ block: "nearest" });
    });
  }, [focusWorker]);

  const toolsByWorker = useMemo(() => {
    const byWorker = new Map<string, string[]>();
    for (const msg of workerChat(deck)) {
      if (msg.kind !== "tool") continue;
      const id = msg.solverId!;
      const lines = byWorker.get(id) || [];
      lines.push(msg.content);
      byWorker.set(id, lines);
    }
    return byWorker;
  }, [deck]);

  const laneFor = (id: string): SolverLane => deck.lanes[id] || {
    solverId: id, reasoning: "", toolLines: [], status: running ? "waiting" : "done",
    solved: false, online: !deck.finished,
  };
  const siblings = useMemo(
    () => allIds.map((id) => toWorkerIdentity(id, deck.lanes[id])),
    [allIds, deck.lanes],
  );

  const grouped = useMemo(() => {
    const live: string[] = [];
    const issue: string[] = [];
    const done: string[] = [];
    for (const id of allIds) {
      const lane = laneFor(id);
      const bucket = rosterGroup(lane, lane.online !== false);
      if (bucket === "issue") issue.push(id);
      else if (bucket === "done") done.push(id);
      else live.push(id);
    }
    return { live, issue, done };
    // laneFor reads deck.lanes / running / finished; allIds already tracks deck.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [allIds, deck.lanes, deck.finished, running]);

  const renderRow = (id: string) => {
    const lane = laneFor(id);
    const online = lane.online !== false;
    const engine = workerEngine(id, lane.engine);
    const color = workerColor(id, lane.engine);
    const tools = (toolsByWorker.get(id) || []).slice(-6);
    const latestActivity = latestLaneActivity(lane.status, lane.statusReason, tools);
    const statusKind = laneStatusKind(lane, online);
    const statusLabel = compactLaneStatus(lane, online, t);
    const display = workerDisplayName(id, toWorkerIdentity(id, lane), siblings);
    const isReview = isReviewWorkerLane(lane);
    const isVerifier = isVerifierWorkerLane(lane);
    const focused = focusWorker?.id === id;
    const openTimeline = () => onOpenSpeakerTimeline?.(id);
    return (
      <div
        key={id}
        className={`wlane-row is-${statusKind} ${focused ? "focused" : ""}`}
        style={{ "--wc": color } as CSSProperties}
      >
        <button
          type="button"
          className="wlane-row-main"
          ref={(node) => {
            if (node) rowRefs.current.set(id, node);
            else rowRefs.current.delete(id);
          }}
          onClick={openTimeline}
          title={onOpenSpeakerTimeline ? t("wlane.viewInStream") : display.titleAttr}
        >
          <span className="wlane-avatar">{display.initial}</span>
          <span className="wlane-id">
            <span className="wlane-name" title={display.titleAttr}>{display.title}</span>
            {workerGeneration(id) > 1 && <span className="wlane-gen" title={id}>g{workerGeneration(id)}</span>}
          </span>
          <span className="wlane-eng">{engine}</span>
          <span className="wlane-latest" title={latestActivity || undefined}>{latestActivity || "—"}</span>
          <span className={`wlane-status is-${statusKind}`}>
            {statusLabel}
            {isReview && <span className="worker-role-chip review">{t("worker.role.review")}</span>}
            {isVerifier && <span className="worker-role-chip verifier">{t("worker.role.verifier")}</span>}
          </span>
        </button>
        {running && online ? (
          <button
            type="button"
            className="wlane-kill"
            title={t("worker.killTitle")}
            aria-label={t("worker.killTitle")}
            onClick={() => onKillWorker(id)}
          >
            <Icon name="x" size={13} />
          </button>
        ) : <span />}
      </div>
    );
  };

  const renderGroup = (key: RosterGroup, ids: string[], open: boolean, onToggle?: () => void) => {
    if (!ids.length) return null;
    const label = key === "live" ? t("wlane.groupLive") : key === "issue" ? t("wlane.groupIssues") : t("wlane.groupDone");
    return (
      <div className={`wlane-grp ${key}`}>
        {onToggle ? (
          <button type="button" className="wlane-grp-h" onClick={onToggle} aria-expanded={open}>
            <b>{label}</b>
            <Icon name={open ? "chevronDown" : "chevronRight"} size={13} />
            <span className="wlane-grp-n">{ids.length}</span>
          </button>
        ) : (
          <div className="wlane-grp-h">
            <b>{label}</b>
            <span className="wlane-grp-n">{ids.length}</span>
          </div>
        )}
        {open && ids.map(renderRow)}
      </div>
    );
  };

  return (
    <div className="runtime-worker-host panel-scroll-wrap">
      <div className="wlane-bar">
        <div className="wlane-bar-l">
          <i className={`wlane-dot ${running ? "live" : ""}`} aria-hidden="true" />
          <b>{t(`coord.phase.${phase}`)}</b>
          <span>{t("wlane.people", { n: curIds.length })}</span>
          <span>{t("wlane.calls", { n: calls })}</span>
          {elapsed ? <span>{elapsed}</span> : null}
        </div>
        <div className="wlane-bar-r">
          {allIds.length > 0 && (
            <button
              type="button"
              className={`wlane-anomaly ${onlyAnomaly ? "on" : ""}`}
              aria-pressed={onlyAnomaly}
              title={t("wlane.anomalyTitle")}
              onClick={() => setOnlyAnomaly((value) => !value)}
            >
              {t("wlane.anomaly")}
            </button>
          )}
          <WorkerSpawnControl running={running} onSpawnWorker={onSpawnWorker} />
        </div>
      </div>

      {allIds.length === 0 ? (
        <PanelEmpty icon="grid" title={t("wlane.empty")} hint={t("wlane.emptyHint")} />
      ) : onlyAnomaly ? (
        grouped.issue.length === 0 ? (
          <PanelEmpty icon="alert" title={t("wlane.anomalyEmpty")} hint={t("wlane.anomalyEmptyHint")} />
        ) : (
          <div className="wlane-roster">{grouped.issue.map(renderRow)}</div>
        )
      ) : (
        <div className="wlane-roster">
          {renderGroup("live", grouped.live, true)}
          {renderGroup("issue", grouped.issue, true)}
          {renderGroup("done", grouped.done, doneOpen, () => setDoneOpen((value) => !value))}
        </div>
      )}
    </div>
  );
}
