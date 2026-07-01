"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { DeckState, SolverLane, isReviewWorkerLane, isFactRetired, workerChat, workerIds } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { copyToClipboard } from "@/lib/clipboard";
import { workerColor, workerEngine, workerInitial, workerShortLabel, resumeCommand } from "@/lib/workers";
import { compactLaneStatus, latestLaneActivity } from "@/lib/workerLanePresentation";
import { Icon } from "@/components/Icon";
import { PanelEmpty } from "@/components/PanelEmpty";
import { ChipFilterBar } from "@/components/ChipFilterBar";

/**
 * Rich per-worker lanes (the worker firehose, moved out of the main thread).
 * Each lane shows the engine-coloured header, live stat tiles (verified facts /
 * tool calls / status), the live reasoning stream, and the recent tool/output
 * lines — plus spawn/kill controls so the operator drives the swarm from here.
 */

const SPAWN_ENGINES = ["cursor", "claude", "codex"];

function runtimeLabel(lane: SolverLane): string {
  const runtime = lane.runtime;
  if (!runtime?.backend) return "";
  const status = runtime.status ? `:${runtime.status}` : "";
  const rc = typeof runtime.rc === "number" ? ` rc=${runtime.rc}` : "";
  return `${runtime.backend}${status}${rc}`;
}

export function WorkerLanes({
  deck,
  running,
  focusWorker,
  onSpawnWorker,
  onKillWorker,
}: {
  deck: DeckState;
  running: boolean;
  // when this seed changes (operator clicked a roster mini-row), seed `shown` to
  // just that worker so the panel opens focused on it. The operator can then add
  // chips or hit "全部" to clear back to all lanes — manual filtering is untouched.
  focusWorker?: { id: string; nonce: number } | null;
  onSpawnWorker: (engine?: string) => void;
  onKillWorker: (id: string) => void;
}) {
  const t = useT();
  const [spawnEngine, setSpawnEngine] = useState("");
  const allIds = useMemo(() => workerIds(deck), [deck]);
  // CLICK-TO-SHOW worker focus — `shown` holds the workers the operator SELECTED
  // to display. Empty = show every lane (default); click chips to light up and show
  // only those workers (multi-select). Mirrors the activity-stream filter.
  const [shown, setShown] = useState<Set<string>>(new Set());
  const [expandedLaneIds, setExpandedLaneIds] = useState<Set<string>>(new Set());
  const toggleShown = (id: string) =>
    setShown((prev) => { const n = new Set(prev); n.has(id) ? n.delete(id) : n.add(id); return n; });
  const toggleExpanded = (id: string) =>
    setExpandedLaneIds((prev) => { const n = new Set(prev); n.has(id) ? n.delete(id) : n.add(id); return n; });
  // External focus seed (roster row click): each click bumps `nonce`, so re-clicking
  // the SAME worker still re-focuses. We only react to a new nonce (not to `shown`
  // edits), so the operator's manual chip toggles afterward aren't clobbered.
  const lastNonce = useRef<number | null>(null);
  useEffect(() => {
    if (!focusWorker || focusWorker.nonce === lastNonce.current) return;
    lastNonce.current = focusWorker.nonce;
    setShown(new Set([focusWorker.id]));
  }, [focusWorker]);
  useEffect(() => {
    setExpandedLaneIds((prev) => new Set([...prev].filter((id) => allIds.includes(id))));
  }, [allIds]);
  const ids = shown.size === 0 ? allIds : allIds.filter((id) => shown.has(id));
  const filterSummary = shown.size === 0
    ? t("wlane.focusAll")
    : t("wlane.focusSelected", { n: ids.length, total: allIds.length });

  // click-to-copy resume command with transient feedback (one chip at a time).
  // Lanes render in a .map(), so the per-component useCopied hook can't be used
  // here — track the copied worker id at the parent instead.
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const copyTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const copyResume = (id: string, cmd: string) => {
    if (!cmd) return;
    void copyToClipboard(cmd).then((ok) => {
      if (!ok) return;
      setCopiedId(id);
      if (copyTimer.current) clearTimeout(copyTimer.current);
      copyTimer.current = setTimeout(() => setCopiedId(null), 1200);
    });
  };

  // group worker chat by solver: reasoning text (live) + recent tool lines
  const byWorker = useMemo(() => {
    const m = new Map<string, { reasoning: string[]; tools: string[] }>();
    for (const msg of workerChat(deck)) {
      const id = msg.solverId!;
      const e = m.get(id) || { reasoning: [], tools: [] };
      if (msg.kind === "reasoning" || msg.kind === "text") e.reasoning.push(msg.content);
      else if (msg.kind === "tool") e.tools.push(msg.content);
      m.set(id, e);
    }
    return m;
  }, [deck]);

  // verified-fact count per actor (from the blackboard provenance facts)
  const verifiedByActor = useMemo(() => {
    const m = new Map<string, number>();
    for (const f of deck.blackboard.facts) {
      // A: skip review-retired facts (rejected/merged/superseded)
      if (f.verified && !isFactRetired(f)) m.set(f.actor, (m.get(f.actor) || 0) + 1);
    }
    return m;
  }, [deck.blackboard.facts]);

  // E: resource locks held per worker — surfaced as a lane tile
  const locksByOwner = useMemo(() => {
    const m = new Map<string, number>();
    for (const l of deck.resourceLocks ?? []) {
      if (l.status === "active" && l.ownerWorker) {
        m.set(l.ownerWorker, (m.get(l.ownerWorker) || 0) + 1);
      }
    }
    return m;
  }, [deck.resourceLocks]);

  const laneFor = (id: string): SolverLane => deck.lanes[id] || {
    solverId: id, reasoning: "", toolLines: [], status: running ? "waiting" : "done",
    solved: false, online: !deck.finished,
  };

  return (
    <div className="panel-scroll">
      <div className="panel-title">{t("wlane.title")}</div>
      <div className="panel-sub">{t("workerDock.subtitle", { n: allIds.length, online: allIds.filter((id) => laneFor(id).online !== false).length })}</div>

      {allIds.length > 1 && (
        <ChipFilterBar
          id="wlane-filter-chips"
          className="wlane-filterbar"
          label={t("wlane.focus")}
          summary={filterSummary}
          title={t("wlane.focusTitle")}
          expandTitle={t("wlane.focusExpand")}
          collapseTitle={t("wlane.focusCollapse")}
          clearLabel={t("wlane.focusAll")}
          showClear={shown.size > 0}
          onClear={() => setShown(new Set())}
        >
          {allIds.map((id) => (
            <button
              key={id}
              type="button"
              className={`filter-chip ${shown.size === 0 || shown.has(id) ? "on" : "off"}`}
              style={{ "--fc": workerColor(id) } as CSSProperties}
              onClick={() => toggleShown(id)}
              aria-pressed={shown.has(id)}
              title={`${id} · ${t("wlane.focusTitle")}`}
            >
              <span className="filter-chip-ico">{workerInitial(id)}</span>
              <span className="filter-chip-text">{workerShortLabel(id)}</span>
            </button>
          ))}
        </ChipFilterBar>
      )}

      {running && (
        <div className="wlane-spawn">
          <select value={spawnEngine} onChange={(e) => setSpawnEngine(e.target.value)} title={t("workerDock.engine")}>
            <option value="">{t("workerDock.auto")}</option>
            {SPAWN_ENGINES.map((e) => <option key={e} value={e}>{e}</option>)}
          </select>
          <button className="wlane-spawn-btn" onClick={() => onSpawnWorker(spawnEngine || undefined)}
            title={t("workerDock.addTitle")}>＋ {t("workerDock.add")}</button>
        </div>
      )}

      {ids.length === 0 ? (
        <PanelEmpty icon="grid" title={t("wlane.empty")} hint={t("wlane.emptyHint")} />
      ) : ids.map((id) => {
        const lane = laneFor(id);
        const online = lane.online !== false;
        const engine = workerEngine(id, lane.engine);
        const color = workerColor(id, lane.engine);
        const chat = byWorker.get(id) || { reasoning: [], tools: [] };
        const reasoning = (lane.reasoning || chat.reasoning.slice(-1)[0] || "").trim();
        const tools = chat.tools.slice(-6);
        const session = lane.session;
        const resumeCmd = session ? resumeCommand(engine, session) : "";
        const copied = copiedId === id;
        const latestActivity = latestLaneActivity(lane.status, lane.statusReason, tools);
        const statusLabel = compactLaneStatus(lane, online, t);
        const runtime = runtimeLabel(lane);
        const isExpanded = expandedLaneIds.has(id);
        const isReview = isReviewWorkerLane(lane);
        // I: highlight a stalled / paused lane so the operator spots a stuck worker
        const stalledCls = lane.status === "stalled" ? "stalled" : (lane.paused ? "paused" : "");
        return (
          <div key={id} className={`wlane ${isExpanded ? "expanded" : "collapsed"} ${lane.solved ? "solved" : ""} ${online ? "" : "offline"} ${isReview ? "review-worker" : ""} ${stalledCls}`}
            style={{ "--wc": color } as CSSProperties}>
            <div
              className="wlane-head"
              role="button"
              tabIndex={0}
              aria-expanded={isExpanded}
              aria-label={t(isExpanded ? "wlane.collapse" : "wlane.expand")}
              onClick={() => toggleExpanded(id)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  toggleExpanded(id);
                }
              }}
            >
              <span className="wlane-avatar">{workerInitial(id)}</span>
              <span className="wlane-meta">
                <span className="wlane-idrow">
                  <span className="wlane-name" title={id}>{id}</span>
                  <span className="wlane-eng">{engine}</span>
                  {isReview && <span className="worker-role-chip review">{t("worker.role.review")}</span>}
                  {session && (
                    <span className={`wlane-sess ${copied ? "copied" : ""}`}
                      role="button" tabIndex={0}
                      aria-label={t("insp.run.copySession")}
                      onClick={(e) => { e.stopPropagation(); copyResume(id, resumeCmd); }}
                      onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); e.stopPropagation(); copyResume(id, resumeCmd); } }}
                      title={t("insp.run.copySession") + ": " + resumeCmd}>
                      {copied ? <><Icon name="check" size={10} /> {t("common.copied")}</> : <>{t("worker.session")} {session.slice(0, 12)}</>}
                    </span>
                  )}
                </span>
                {latestActivity && (
                  <span className="wlane-latest" title={latestActivity}>
                    <span>{t("wlane.latest")}</span>{latestActivity}
                  </span>
                )}
              </span>
              <span className={`wlane-status ${lane.solved ? "solved" : ""}`}>
                {statusLabel}
              </span>
              <span className={`wlane-toggle ${isExpanded ? "expanded" : ""}`} aria-hidden="true">
                <Icon name="chevronDown" size={13} />
              </span>
              {running && online && (
                <button className="wlane-kill" title={t("worker.killTitle")} aria-label={t("worker.killTitle")} onClick={(e) => { e.stopPropagation(); onKillWorker(id); }}><Icon name="x" size={13} /></button>
              )}
            </div>
            {isExpanded && (
              <div className="wlane-detail">
                <div className="wlane-tiles">
                  <div className="wlane-tile"><div className="tk">{t("wlane.facts")}</div><div className="tv">{verifiedByActor.get(id) || 0}</div></div>
                  <div className="wlane-tile"><div className="tk">{t("wlane.toolcount")}</div><div className="tv">{lane.toolLines.length}</div></div>
                  {typeof lane.tokensSpent === "number" && lane.tokensSpent > 0 && (
                    <div className="wlane-tile"><div className="tk">{t("wlane.tokensSpent")}</div><div className="tv">{lane.tokensSpent.toLocaleString()}</div></div>
                  )}
                  {(locksByOwner.get(id) || 0) > 0 && (
                    <div className="wlane-tile"><div className="tk">{t("resource.lockActive")}</div><div className="tv">{locksByOwner.get(id)}</div></div>
                  )}
                  <div className="wlane-tile">
                    <div className="tk">{t("wlane.statusLabel")}</div>
                    <div className={`tv wlane-online ${online ? "on" : "off"}`}>
                      <span className="wlane-online-dot" aria-hidden="true" />
                      {online ? t("workerDock.online") : t("workerDock.offline")}
                    </div>
                  </div>
                  <div className="wlane-tile">
                    <div className="tk">{t("wlane.runtime")}</div>
                    <div className="tv wlane-runtime" title={runtime || t("wlane.runtimeUnknown")}>
                      {runtime || t("wlane.runtimeUnknown")}
                    </div>
                  </div>
                </div>
                <div className="wlane-body">
                  <div>
                    <div className="wlane-block-h">{t("wlane.reasoning")}</div>
                    <div className={`wlane-reason ${reasoning ? "" : "idle"}`}>{reasoning || t("wlane.idle")}</div>
                  </div>
                  <div>
                    <div className="wlane-block-h">{t("wlane.tools")}</div>
                    {tools.length === 0 ? (
                      <div className="wlane-reason idle">{t("wlane.noTools")}</div>
                    ) : (
                      <div className="wlane-tools">
                        {tools.map((tl, i) => <div className="wlane-toolline" key={i}>{tl}</div>)}
                      </div>
                    )}
                  </div>
                </div>
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
