"use client";

import { Button, Chip, ListBox, ListBoxItem, Select, Tooltip } from "@heroui/react";

import { useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, KeyboardEvent } from "react";
import { DeckState, SolverLane, isReviewWorkerLane, isVerifierWorkerLane, workerChat, workerIds, currentGenWorkerIds } from "@/lib/events";
import type { SwarmDigest } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { SPAWN_ENGINES, workerColor, workerEngine, workerEngineKey, toWorkerIdentity, workerDisplayName, workerGeneration } from "@/lib/workers";
import { compactLaneStatus, laneActivityDetail, laneStatusKind, laneStatusTone, rosterGroup } from "@/lib/workerLanePresentation";
import type { RosterGroup } from "@/lib/workerLanePresentation";
import { Icon } from "@/components/Icon";
import { EngineLogo } from "@/components/EngineLogo";
import { WorkerPromptButton } from "@/components/WorkerPromptButton";

/**
 * Worker roster: a compact control list (who is working / stuck / done).
 * Event content lives in the activity stream — a row click jumps there.
 */

function WorkerSpawnControl({
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
      <Select aria-label={t("workerDock.engine")} selectedKey={spawnEngine} onSelectionChange={(key) => setSpawnEngine(String(key ?? ""))}>
        <Select.Trigger><Select.Value /></Select.Trigger>
        <Select.Popover><ListBox><ListBoxItem id="" textValue={t("workerDock.auto")}>{t("workerDock.auto")}</ListBoxItem>{SPAWN_ENGINES.map((engine) => <ListBoxItem key={engine} id={engine} textValue={engine}>{engine}</ListBoxItem>)}</ListBox></Select.Popover>
      </Select>
      <Button className="wlane-spawn-btn" onClick={() => onSpawnWorker(spawnEngine || undefined)}
        data-tooltip={t("workerDock.addTitle")}>＋ {t("workerDock.add")}</Button>
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
  onOpenAgent,
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
  onOpenAgent?: (id: string) => void;
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
  const rowRefs = useRef(new Map<string, HTMLDivElement>());

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
    const activity = laneActivityDetail(lane, online, tools);
    const statusKind = laneStatusKind(lane, online);
    const statusLabel = compactLaneStatus(lane, online, t);
    const statusTone = laneStatusTone(statusKind);
    const rawStatus = (lane.statusReason || lane.status || "").trim();
    const rawStatusHint = rawStatus && !/^tool:/i.test(rawStatus) && rawStatus !== statusLabel
      ? rawStatus
      : "";
    const display = workerDisplayName(id, toWorkerIdentity(id, lane), siblings);
    const generation = workerGeneration(id);
    const isReview = isReviewWorkerLane(lane);
    const engineKey = workerEngineKey(id, lane.engine);
    const isVerifier = isVerifierWorkerLane(lane);
    const focused = focusWorker?.id === id;
    const openTimeline = () => onOpenSpeakerTimeline?.(id);
    const activateOnKey = (event: KeyboardEvent<HTMLDivElement>) => {
      if (event.key !== "Enter" && event.key !== " ") return;
      event.preventDefault();
      openTimeline();
    };
    return (
      <div
        key={id}
        ref={(node) => {
          if (node) rowRefs.current.set(id, node);
          else rowRefs.current.delete(id);
        }}
        className={`wlane-row is-${statusKind} ${focused ? "focused" : ""}`}
        style={{ "--wc": color } as CSSProperties}
      >
        <Tooltip delay={320} closeDelay={60}>
          <Tooltip.Trigger
            className="wlane-row-main"
            aria-label={`${display.title} · ${engine} · ${statusLabel}`}
            onClick={openTimeline}
            onKeyDown={activateOnKey}
          >
            <span className="wlane-avatar" aria-hidden="true">
              {engineKey
                ? <EngineLogo engine={engineKey} size={14} />
                : display.initial}
            </span>
            <span className="wlane-id">
              <span className="wlane-name">{display.title}</span>
              {generation > 1 && (
                <Chip className="wlane-gen" size="sm" variant="soft" color="accent">
                  <Chip.Label>g{generation}</Chip.Label>
                </Chip>
              )}
              {isReview && (
                <Chip className="worker-role-chip review" size="sm" variant="soft">
                  <Chip.Label>{t("worker.role.review")}</Chip.Label>
                </Chip>
              )}
              {isVerifier && (
                <Chip className="worker-role-chip verifier" size="sm" variant="soft">
                  <Chip.Label>{t("worker.role.verifier")}</Chip.Label>
                </Chip>
              )}
            </span>
            <Chip className="wlane-eng" size="sm" variant="soft">
              <Chip.Label>{engine}</Chip.Label>
            </Chip>
            <Chip className={`wlane-status is-${statusKind}`} size="sm" variant="soft" color={statusTone}>
              <Chip.Label>{statusLabel}</Chip.Label>
            </Chip>
          </Tooltip.Trigger>
          <Tooltip.Content className="wlane-tip">
            <b className="wlane-tip-name">{display.title}</b>
            <span className="wlane-tip-meta">{engine} · {statusLabel}{rawStatusHint ? ` · ${rawStatusHint}` : ""}</span>
            {display.titleAttr ? <span className="wlane-tip-meta">{display.titleAttr}</span> : null}
            {activity ? <span className="wlane-tip-activity">{activity}</span> : null}
            {onOpenSpeakerTimeline ? <span className="wlane-tip-hint">{t("wlane.viewInStream")}</span> : null}
            {onOpenAgent ? (
              <button type="button" className="wlane-tip-action" onClick={() => onOpenAgent(id)}>
                {t("collab.action.viewOnMap")}
              </button>
            ) : null}
          </Tooltip.Content>
        </Tooltip>
        <div className="wlane-row-actions">
          <WorkerPromptButton deck={deck} workerId={id} name={display.title} compact className="wlane-collab" />
          {onOpenAgent ? (
            <Button
              type="button"
              className="wlane-collab"
              aria-label={t("collab.action.viewOnMap")}
              onClick={() => onOpenAgent(id)}
            >
              <Icon name="network" size={13} />
            </Button>
          ) : null}
          {running && online ? (
            <Button
              type="button"
              className="wlane-kill"
              aria-label={t("worker.killTitle")}
              onClick={() => onKillWorker(id)}
            >
              <Icon name="x" size={13} />
            </Button>
          ) : onOpenAgent ? null : <span />}
        </div>
      </div>
    );
  };

  const renderGroup = (key: RosterGroup, ids: string[], open: boolean, onToggle?: () => void) => {
    if (!ids.length) return null;
    const label = key === "live" ? t("wlane.groupLive") : key === "issue" ? t("wlane.groupIssues") : t("wlane.groupDone");
    return (
      <div className={`wlane-grp ${key}`}>
        {onToggle ? (
          <Button type="button" className="wlane-grp-h" onClick={onToggle} aria-expanded={open}>
            <b>{label}</b>
            <Icon name={open ? "chevronDown" : "chevronRight"} size={13} />
            <span className="wlane-grp-n">{ids.length}</span>
          </Button>
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
            <Button
              type="button"
              className={`wlane-anomaly ${onlyAnomaly ? "on" : ""}`}
              aria-pressed={onlyAnomaly}
              aria-label={t("wlane.anomalyTitle")}
              onClick={() => setOnlyAnomaly((value) => !value)}
            >
              {t("wlane.anomaly")}
            </Button>
          )}
          <WorkerSpawnControl running={running} onSpawnWorker={onSpawnWorker} />
        </div>
      </div>

      {allIds.length === 0 ? (
        <div className="panel-empty"><span className="panel-empty-ico" aria-hidden="true"><Icon name="grid" size={26} /></span><span className="panel-empty-title">{t("wlane.empty")}</span><span className="panel-empty-hint">{t("wlane.emptyHint")}</span></div>
      ) : onlyAnomaly ? (
        grouped.issue.length === 0 ? (
          <div className="panel-empty"><span className="panel-empty-ico" aria-hidden="true"><Icon name="alert" size={26} /></span><span className="panel-empty-title">{t("wlane.anomalyEmpty")}</span><span className="panel-empty-hint">{t("wlane.anomalyEmptyHint")}</span></div>
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
