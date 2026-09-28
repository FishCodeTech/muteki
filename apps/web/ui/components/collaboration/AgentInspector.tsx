"use client";

import { Button } from "@heroui/react";
import { useEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent } from "react";

import { Icon } from "@/components/Icon";
import { EngineLogo } from "@/components/EngineLogo";
import { WorkerPromptButton } from "@/components/WorkerPromptButton";
import {
  activityCountForAgent,
  activityForAgent,
  collaborationAgentId,
  COORDINATOR_ID,
  isCollaborationWorker,
  OPERATOR_ID,
  unattributedUsd,
  type CollaborationAgent,
  type CollaborationKnowledgeItem,
  type CollaborationKnowledgeKind,
  type CollaborationRelation,
} from "@/lib/agentCollaboration";
import { LIST_LIMITS } from "@/lib/agentCollaborationLayout";
import type { BlackboardEvent, BlackboardIntent, DeckState } from "@/lib/events";
import type { RunCanvasMode } from "@/lib/swarmProjection";
import { compactNumber, formatClock, toEpochMs } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { activityTitle, activityTitleKey, intentStatusLabel } from "@/lib/statusLabels";

import { relationLabel, roleLabel, statusLabel, type AgentDisplay, type Translate } from "./collaborationPresentation";
import { EventStamp } from "./EventStamp";
import { KnowledgeKindChips, knowledgeKindCounts } from "./KnowledgeKindFilter";
import { FactSeqLinks, KnowledgeDetail, KnowledgeLink, KnowledgeRow, findKnowledge, knowledgeProposerLabel, knowledgeVerifierLabel } from "./KnowledgeRow";
import { RelationSwatch } from "./RelationSwatch";
import { handleTabListKeyDown } from "./tablist";
import { useFeedUnread } from "./useFeedUnread";
import { COORDINATOR_INSPECTOR_TABS, INSPECTOR_TABS, type InspectorTab } from "./useCollaborationSelection";

function IntentDetail({
  intent, knowledge, onSelectKnowledge, t, mode,
}: {
  intent: BlackboardIntent;
  knowledge: CollaborationKnowledgeItem[];
  onSelectKnowledge: (item: CollaborationKnowledgeItem) => void;
  t: Translate;
  mode?: RunCanvasMode;
}) {
  return (
    <div className="collab-intent-detail">
      <strong>{intent.summary || intent.goal}</strong>
      {intent.summary && intent.summary !== intent.goal && <p>{intent.goal}</p>}
      <dl>
        <dt>{t(mode === "ctf" ? "collab.field.step" : "collab.field.intent")}</dt>
        <dd>
          <KnowledgeLink
            target={findKnowledge(knowledge, `intent:${intent.id}`)}
            label={intent.id}
            onSelect={onSelectKnowledge}
          />
        </dd>
        <dt>{t("collab.field.status")}</dt><dd>{intentStatusLabel(intent.dispatchState || intent.status, t)}</dd>
        {intent.requestedPriority && <><dt>{t("collab.field.requestedPriority")}</dt><dd>{intent.requestedPriority}</dd></>}
        {intent.priority !== undefined && <><dt>{t("collab.field.effectivePriority")}</dt><dd>{intent.priority}</dd></>}
        {intent.priorityReason && <><dt>{t("collab.field.priorityReason")}</dt><dd>{intent.priorityReason}</dd></>}
        {!!intent.requiresCapabilities?.length && <><dt>{t("collab.field.requiresCapabilities")}</dt><dd>{intent.requiresCapabilities.join(", ")}</dd></>}
        {!!intent.requiredPocs?.length && <><dt>{t("collab.knowledge.resource")}</dt><dd>{intent.requiredPocs.join(", ")}</dd></>}
        {intent.closeReason && <><dt>{t("collab.field.closeReason")}</dt><dd>{intent.closeReason}</dd></>}
        {intent.expectedObservable && <><dt>{t("collab.field.expected")}</dt><dd>{intent.expectedObservable}</dd></>}
        {intent.stopCondition && <><dt>{t("collab.field.stop")}</dt><dd>{intent.stopCondition}</dd></>}
        {intent.coverageKey && <><dt>{t("collab.field.coverage")}</dt><dd>{intent.coverageKey}</dd></>}
        {intent.fromFacts.length > 0 && (
          <>
            <dt>{t("collab.field.inputs")}</dt>
            <dd>
              <FactSeqLinks seqs={intent.fromFacts} knowledge={knowledge} onSelect={onSelectKnowledge} />
            </dd>
          </>
        )}
      </dl>
    </div>
  );
}

/** Output-summary cell: zero is muted, a non-zero value carries its category colour. */
function Metric({ value, label, tone }: { value: number; label: string; tone?: "ok" | "warn" | "bad" }) {
  return <span><b className={value ? tone ?? "" : "zero"}>{value}</b><small>{label}</small></span>;
}

/** Full local date for a clock cell's tooltip; the cell itself shows HH:MM:SS. */
function fullDate(ts?: number): string | undefined {
  const ms = toEpochMs(ts);
  return ms ? new Date(ms).toLocaleString() : undefined;
}

/** Start / end / time-source rows shared by the worker and coordinator runtime lists. */
function LifecycleRows({ agent, t }: { agent: CollaborationAgent; t: Translate }) {
  return (
    <>
      <dt>{t("collab.field.started")}</dt><dd title={fullDate(agent.startedAt)}>{formatClock(agent.startedAt, "—")}</dd>
      <dt>{t("collab.field.finished")}</dt><dd title={fullDate(agent.endedAt)}>{formatClock(agent.endedAt, "—")}</dd>
      <dt>{t("collab.field.lifecycleSource")}</dt><dd>{t(`collab.lifecycle.${agent.lifecycleSource}`)}</dd>
    </>
  );
}

export type CollaborationRunStats = {
  generation: number;
  openIntents: number;
  facts: number;
  observations: number;
  candidates: number;
  deadEnds: number;
  pocs: number;
};

function CoordinatorOverview({
  deck,
  agent,
  runStats,
  onViewActivity,
  t,
  mode,
}: {
  deck: DeckState;
  agent: CollaborationAgent;
  runStats: CollaborationRunStats;
  onViewActivity: () => void;
  t: Translate;
  mode?: RunCanvasMode;
}) {
  const metrics = agent.coordination;
  const phaseText = t(`coord.phase.${agent.phase}`);
  const events = deck.blackboard.events;
  const lastEvent = events[events.length - 1];
  const recentText = lastEvent?.label || agent.latestActivity;
  const recentTs = lastEvent?.ts ?? agent.latestActivityTs;
  return (
    <>
      <section className="collab-detail-section">
        <h3>{t("collab.runOverview")}</h3>
        <dl className="collab-runtime-detail">
          <dt>{t("collab.field.phase")}</dt><dd>{phaseText}</dd>
          <dt>{t("collab.field.generation")}</dt><dd>{runStats.generation}</dd>
          <dt>{t("collab.field.workers")}</dt><dd>{metrics?.onlineWorkers ?? 0}/{metrics?.totalWorkers ?? 0}</dd>
          <dt>{t(mode === "ctf" ? "collab.coord.pendingCtf" : "collab.coord.pending")}</dt><dd>{runStats.openIntents}</dd>
          {mode !== "ctf" && <><dt>{t("collab.activeCapabilities")}</dt><dd>{deck.blackboard.capabilities.filter((item) => item.state === "active").length}</dd>
          <dt>{t("collab.readyAccessPaths")}</dt><dd>{deck.blackboard.accessPaths.filter((item) => item.state === "ready").length}</dd></>}
        </dl>
      </section>
      <section className="collab-detail-section">
        <h3>{t("collab.recentEvent")}</h3>
        {recentText ? (
          <div className="collab-intent-detail">
            <strong>{recentText}</strong>
            <p>{formatClock(recentTs, "—")}</p>
            <div className="collab-detail-actions">
              <Button size="sm" variant="ghost" onClick={onViewActivity}>
                <Icon name="rows" size={12} />{t("collab.action.viewActivity")}
              </Button>
            </div>
          </div>
        ) : <div className="collab-section-empty">{t("collab.noActivity")}</div>}
      </section>
      <section className="collab-detail-section">
        <h3>{t("collab.outputSummary")}</h3>
        <div className="collab-metric-grid">
          {mode === "ctf" ? (
            <>
              <Metric value={runStats.facts} label={t("meta.facts")} tone="ok" />
              <Metric value={runStats.observations} label={t("meta.observations")} tone="warn" />
              <Metric value={runStats.deadEnds} label={t("collab.exclusions")} tone="bad" />
              <Metric value={runStats.pocs} label={t("collab.knowledge.resource")} />
              <Metric value={deck.blackboard.intents.length} label={t("collab.workspace.tasks")} />
              <Metric value={deck.flags.length} label={t("meta.flags")} />
            </>
          ) : (
            <>
              <Metric value={runStats.facts} label={t("meta.verified")} tone="ok" />
              <Metric value={runStats.candidates} label={t("meta.observations")} tone="warn" />
              <Metric value={runStats.deadEnds} label={t("collab.exclusions")} tone="bad" />
              <Metric value={runStats.pocs} label={t("collab.knowledge.poc")} />
            </>
          )}
        </div>
      </section>
      <section className="collab-detail-section">
        <h3>{t("collab.runtime")}</h3>
        <dl className="collab-runtime-detail">
          <dt>{t("collab.field.phase")}</dt><dd>{phaseText}</dd>
          <LifecycleRows agent={agent} t={t} />
          <dt>{t("collab.workspace.totalTokens")}</dt><dd>{compactNumber(deck.tokensIn + deck.tokensOut)}</dd>
          <dt>{t("collab.workspace.totalCost")}</dt><dd>${deck.usd.toFixed(4)}</dd>
          <dt>{t("collab.unattributedCost")}</dt><dd>${unattributedUsd(deck).toFixed(4)}</dd>
        </dl>
      </section>
    </>
  );
}

function WorkerOverview({
  agent, display, actorName, knowledge, onSelectKnowledge, t, mode,
}: {
  agent: CollaborationAgent;
  display: AgentDisplay;
  actorName: (id?: string) => string;
  knowledge: CollaborationKnowledgeItem[];
  onSelectKnowledge: (item: CollaborationKnowledgeItem) => void;
  t: Translate;
  mode?: RunCanvasMode;
}) {
  const intent = agent.currentIntent;
  const last = intent ? undefined : agent.lastIntent;
  const spawnPhase = [agent.spawnPhase, agent.raceScout ? t("collab.raceScout") : ""].filter(Boolean).join(" · ");
  const spawner = collaborationAgentId(agent.spawnedBy);
  const spawnedBy = spawner === OPERATOR_ID ? t("collab.byOperator") : spawner ? actorName(spawner) : "";
  return (
    <>
      <section className="collab-detail-section">
        <h3>{last ? t("collab.lastTask") : t("collab.currentTask")}</h3>
        {intent ? <IntentDetail intent={intent} knowledge={knowledge} onSelectKnowledge={onSelectKnowledge} t={t} mode={mode} />
          : last ? <IntentDetail intent={last} knowledge={knowledge} onSelectKnowledge={onSelectKnowledge} t={t} mode={mode} />
            : <div className="collab-section-empty">{agent.latestActivity || t("collab.noCurrentTask")}</div>}
      </section>
      <section className="collab-detail-section">
        <h3>{t("collab.outputSummary")}</h3>
        <div className="collab-metric-grid">
          {mode === "ctf" ? (
            <>
              <Metric value={agent.facts} label={t("meta.facts")} tone="ok" />
              <Metric value={agent.observations} label={t("meta.observations")} tone="warn" />
              <Metric value={agent.deadEnds} label={t("collab.exclusions")} tone="bad" />
              <Metric value={agent.pocs} label={t("collab.knowledge.resource")} />
              <Metric value={agent.intents.length} label={t("meta.steps")} />
            </>
          ) : (
            <>
              <Metric value={agent.facts} label={t("meta.verified")} tone="ok" />
              <Metric value={agent.candidates} label={t("meta.candidates")} tone="warn" />
              <Metric value={agent.deadEnds} label={t("collab.exclusions")} tone="bad" />
              <Metric value={agent.pocs} label={t("collab.knowledge.poc")} />
              <Metric value={agent.reviews} label={t("collab.reviews")} />
            </>
          )}
        </div>
      </section>
      <section className="collab-detail-section">
        <h3>{t("collab.runtime")}</h3>
        <dl className="collab-runtime-detail">
          <dt>{t("collab.field.agent")}</dt><dd>{agent.id}</dd>
          <dt>{t("collab.field.phase")}</dt><dd>{agent.phase || "—"}</dd>
          <dt>{t("collab.field.engine")}</dt><dd>{display.engine || "—"}</dd>
          <dt>{t("collab.field.model")}</dt><dd>{agent.identity.model || "—"}</dd>
          <dt>{t("collab.field.profile")}</dt><dd>{agent.identity.profileLabel || agent.identity.profileId || "—"}</dd>
          <dt>{t("collab.field.session")}</dt><dd>{agent.session || "—"}</dd>
          <LifecycleRows agent={agent} t={t} />
          <dt>{t("collab.field.spawnPhase")}</dt><dd>{spawnPhase || "—"}</dd>
          <dt>{t("collab.field.spawnedBy")}</dt><dd>{spawnedBy || "—"}</dd>
          <dt>{t("collab.field.lastEvent")}</dt><dd title={fullDate(agent.lastEventAt)}>{formatClock(agent.lastEventAt, "—")}</dd>
          <dt>{t("meta.tokens")}</dt><dd>{compactNumber(agent.tokens)}</dd>
          <dt>{t("meta.cost")}</dt><dd>${agent.usd.toFixed(4)}</dd>
        </dl>
      </section>
    </>
  );
}

function RelationRow({
  relation,
  agentId,
  actorName,
  onSelect,
}: {
  relation: CollaborationRelation;
  agentId: string;
  actorName: (id?: string) => string;
  onSelect: () => void;
}) {
  const t = useT();
  const outbound = relation.source === agentId;
  const peerId = outbound ? relation.target : relation.source;
  return (
    <button type="button" className="collab-relation-row" onClick={onSelect}>
      <span className="collab-relation-row-swatch"><RelationSwatch kind={relation.kind} /></span>
      <span className="collab-relation-row-copy">
        <span>
          <b>{relationLabel(relation.kind, t)}</b>
          <small>{t(outbound ? "collab.relation.outbound" : "collab.relation.inbound")}</small>
        </span>
        <strong>{actorName(peerId)}</strong>
        <small>{relation.count} · {formatClock(relation.lastTs, "—")}</small>
      </span>
    </button>
  );
}

function AgentActivityList({ deck, agent, query, asOf }: { deck: DeckState; agent: CollaborationAgent; query: string; asOf?: number }) {
  const t = useT();
  const [activityLimit, setActivityLimit] = useState<number>(LIST_LIMITS.activity);
  const [expandedActivityId, setExpandedActivityId] = useState<string | null>(null);
  const coordinator = agent.id === COORDINATOR_ID;
  const feed = coordinator ? deck.blackboard.events : deck.runtimeEvents;
  const extra = coordinator ? deck.controlCommands : deck.chat;
  const agentId = agent.id;
  const total = useMemo(
    () => activityCountForAgent(deck, agent, asOf),
    // The scanners only read agent.id and the feed/extra slices already listed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [agentId, asOf, extra, feed],
  );
  const fetchLimit = query ? Math.max(activityLimit, total) : activityLimit;
  const activities = useMemo(
    () => activityForAgent(deck, agent, fetchLimit, asOf),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [asOf, fetchLimit, agentId, extra, feed],
  );
  const visibleActivities = useMemo(() => {
    if (!query) return activities;
    return activities.filter((item) => {
      const title = activityTitle(item, t);
      return title.toLowerCase().includes(query) || item.detail.toLowerCase().includes(query);
    });
  }, [activities, query, t]);
  const { listRef, lastSeenTs, freshCount, showJump, onScroll, jumpToLatest, markSeen } = useFeedUnread(
    visibleActivities,
    `${deck.runId}:${agent.id}`,
  );
  const headingCount = query
    ? t("collab.tabMatchCount", { matched: visibleActivities.length, total })
    : total;
  return (
    <section className="collab-detail-section no-border collab-activity-feed">
      <h3>{t("collab.agentActivity")}<span>{headingCount}</span></h3>
      {showJump && (
        <button type="button" className="collab-feed-new" onClick={jumpToLatest}>
          {t("collab.feedNew", { n: freshCount })}
        </button>
      )}
      <div className="collab-activity-list" ref={listRef} onScroll={onScroll}>
        {total ? (
          visibleActivities.length ? (
            <>
              {visibleActivities.map((item) => {
                const open = expandedActivityId === item.id;
                return (
                  <button
                    type="button"
                    className={`collab-activity-row tone-${item.tone}${open ? " is-open" : ""}${item.ts > lastSeenTs ? " fresh" : ""}`}
                    key={item.id}
                    aria-expanded={open}
                    onClick={() => {
                      markSeen();
                      setExpandedActivityId(open ? null : item.id);
                    }}
                  >
                    <i />
                    <span>
                      <strong>{activityTitle(item, t)}</strong>
                      <p title={open ? undefined : item.detail}>{item.detail || "—"}</p>
                    </span>
                    <EventStamp ts={item.ts} />
                  </button>
                );
              })}
              {!query && total > activityLimit && (
                <button
                  type="button"
                  className="collab-index-more"
                  onClick={() => setActivityLimit((n) => n + LIST_LIMITS.activity)}
                >
                  {t("collab.activityMore", { shown: activities.length, total })}
                </button>
              )}
            </>
          ) : <div className="collab-section-empty">{t("collab.noMatches")}</div>
        ) : <div className="collab-section-empty">{t("collab.noActivity")}</div>}
      </div>
    </section>
  );
}

const DECISION_FOLLOW_KINDS = new Set(["worker_spawned", "worker_spawn_rejected", "awaiting_operator", "budget_exhausted"]);

function intentOutcomeDanger(outcome: string): boolean {
  return outcome.includes("drop") || outcome.includes("reject");
}

function CoordinatorDecisions({
  deck,
  knowledge,
  onSelectKnowledge,
  t,
}: {
  deck: DeckState;
  knowledge: CollaborationKnowledgeItem[];
  onSelectKnowledge: (item: CollaborationKnowledgeItem) => void;
  t: Translate;
}) {
  const rounds = useMemo(() => {
    const runs = deck.blackboard.reasonRuns;
    const follow = deck.blackboard.events.filter((event) => DECISION_FOLLOW_KINDS.has(event.kind));
    return runs.map((run, index) => {
      const nextTs = runs[index + 1]?.ts ?? Number.POSITIVE_INFINITY;
      const followEvents = follow.filter((event) => event.ts >= run.ts && event.ts < nextTs);
      return { run, followEvents };
    }).reverse();
  }, [deck.blackboard.events, deck.blackboard.reasonRuns]);

  const openIntent = (id: string) => {
    const item = knowledge.find((row) => row.id === `intent:${id}`);
    if (item) onSelectKnowledge(item);
  };

  if (!rounds.length) {
    return (
      <section className="collab-detail-section no-border">
        <h3>{t("collab.tab.decisions")}</h3>
        <div className="collab-section-empty">
          {t("collab.noDecisions")}
          <small>{t("collab.noDecisionsHint")}</small>
        </div>
      </section>
    );
  }

  return (
    <section className="collab-detail-section no-border">
      <h3>{t("collab.tab.decisions")}<span>{rounds.length}</span></h3>
      <div className="collab-decision-list">
        {rounds.map(({ run, followEvents }) => {
          const emptyDetail = run.attempts.length === 0 && run.intentDecisions.length === 0;
          return (
            <article className="collab-decision-card" key={run.ts}>
              <header>
                <EventStamp ts={run.ts} />
                <span>{t("collab.decision.proposed", { n: run.proposed })}</span>
                <span>{t("collab.decision.dropped", { n: run.droppedTotal })}</span>
                {run.plannerFailure && <span className="tone-danger">{t("collab.decision.plannerFailure")} · {run.plannerFailure}</span>}
              </header>
              {emptyDetail && <p className="collab-decision-empty">{t("collab.decision.noDetail")}</p>}
              {run.attempts.length > 0 && (
                <table className="collab-decision-attempts">
                  <thead>
                    <tr>
                      <th>{t("collab.decision.attempt")}</th>
                      <th>{t("collab.decision.response")}</th>
                      <th>{t("collab.decision.parse")}</th>
                      <th>{t("collab.decision.timeout")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {run.attempts.map((attempt) => (
                      <tr key={attempt.attemptIndex}>
                        <td>{attempt.attemptIndex}</td>
                        <td>{attempt.responseStatus}</td>
                        <td>{attempt.parseStatus}</td>
                        <td>{attempt.timedOut ? t("collab.decision.yes") : t("collab.decision.no")}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
              {run.intentDecisions.length > 0 && (
                <ul className="collab-decision-intents">
                  {run.intentDecisions.map((decision, index) => {
                    const danger = intentOutcomeDanger(decision.outcome);
                    const resolvedId = decision.resolvedIntentId;
                    return (
                      <li className={danger ? "tone-danger" : ""} key={`${decision.modelIntentId}:${index}`}>
                        <strong>{decision.goal || decision.modelIntentId || decision.reasonCode}</strong>
                        <small>{[decision.outcome, decision.stage, decision.reasonCode].filter(Boolean).join(" · ")}</small>
                        {resolvedId && (
                          <button type="button" onClick={() => openIntent(resolvedId)}>
                            {t("collab.decision.openIntent", { id: resolvedId })}
                          </button>
                        )}
                      </li>
                    );
                  })}
                </ul>
              )}
              {followEvents.length > 0 && (
                <ul className="collab-decision-follow">
                  <li className="collab-decision-follow-label">{t("collab.decision.follow")}</li>
                  {followEvents.map((event: BlackboardEvent) => (
                    <li key={event.id}>
                      <strong>{activityTitle({ title: event.kind, titleKey: activityTitleKey("event", event.kind) }, t)}</strong>
                      <small>{event.label}</small>
                      <EventStamp ts={event.ts} />
                    </li>
                  ))}
                </ul>
              )}
            </article>
          );
        })}
      </div>
    </section>
  );
}

export function AgentInspector({
  deck,
  agent,
  display,
  tab,
  selectedKnowledge,
  outOfScope,
  unselected,
  runStats,
  knowledge,
  relations,
  knowledgeKinds,
  canvasMode,
  matchesKnowledge,
  query,
  actorName,
  onTab,
  onToggleKnowledgeKind,
  onCloseKnowledge,
  onSelectKnowledge,
  onSelectRow,
  onSelectRelation,
  onReveal,
  onOpenTimeline,
  onKillWorker,
  onOpenFact,
  onOpenPoc,
  onClosePanel,
  asOf,
}: {
  deck: DeckState;
  agent: CollaborationAgent;
  display: AgentDisplay;
  tab: InspectorTab;
  selectedKnowledge?: CollaborationKnowledgeItem;
  outOfScope?: boolean;
  unselected?: boolean;
  runStats: CollaborationRunStats;
  knowledge: CollaborationKnowledgeItem[];
  relations: CollaborationRelation[];
  knowledgeKinds: Set<CollaborationKnowledgeKind>;
  canvasMode?: RunCanvasMode;
  matchesKnowledge: (item: CollaborationKnowledgeItem) => boolean;
  query: string;
  actorName: (id?: string) => string;
  onTab: (tab: InspectorTab) => void;
  onToggleKnowledgeKind: (kind: CollaborationKnowledgeKind) => void;
  onCloseKnowledge: () => void;
  onSelectKnowledge: (item: CollaborationKnowledgeItem) => void;
  onSelectRow: (itemId: string) => void;
  onSelectRelation: (relation: CollaborationRelation) => void;
  onReveal?: () => void;
  onOpenTimeline?: (id: string) => void;
  onKillWorker?: (id: string) => void;
  onOpenFact?: (seq: number) => void;
  onOpenPoc?: (id: string) => void;
  onClosePanel?: () => void;
  asOf?: number;
}) {
  const t = useT();
  const bodyRef = useRef<HTMLDivElement>(null);
  const agentRelations = useMemo(
    () => relations
      .filter((relation) => relation.source === agent.id || relation.target === agent.id)
      .sort((a, b) => b.lastTs - a.lastTs || a.id.localeCompare(b.id)),
    [agent.id, relations],
  );
  const isWorker = isCollaborationWorker(agent);
  const inspectorTabs: InspectorTab[] = agent.role === "decision" ? ["overview", "decisions", "relations"]
    : agent.role === "source" ? ["overview", "knowledge", "relations"]
    : isWorker ? INSPECTOR_TABS : COORDINATOR_INSPECTOR_TABS;
  const activeTab = inspectorTabs.includes(tab) ? tab : "overview";
  const searchedKnowledge = useMemo(
    () => agent.knowledge.filter(matchesKnowledge),
    [agent.knowledge, matchesKnowledge],
  );
  const visibleKnowledge = knowledgeKinds.size === 0
    ? searchedKnowledge
    : searchedKnowledge.filter((item) => knowledgeKinds.has(item.kind));
  const kindCounts = useMemo(() => knowledgeKindCounts(searchedKnowledge), [searchedKnowledge]);
  const knowledgeFiltered = knowledgeKinds.size > 0 || searchedKnowledge.length !== agent.knowledge.length;
  const [knowledgeLimit, setKnowledgeLimit] = useState<number>(LIST_LIMITS.knowledge);
  useEffect(() => {
    setKnowledgeLimit(LIST_LIMITS.knowledge);
  }, [agent.id]);
  const shownKnowledge = visibleKnowledge.slice(0, knowledgeLimit);
  const knowledgeHeading = query
    ? t("collab.tabMatchCount", { matched: visibleKnowledge.length, total: agent.knowledge.length })
    : visibleKnowledge.length;
  const selectedKnowledgeId = selectedKnowledge?.id;
  useEffect(() => {
    if (tab !== "knowledge" || !selectedKnowledgeId) return;
    bodyRef.current?.scrollTo({ top: 0 });
  }, [selectedKnowledgeId, tab]);
  return (
    <>
      <div className="collab-inspector-head" tabIndex={-1} style={{ "--agent-color": display.color } as CSSProperties}>
        <span className="collab-inspector-avatar" aria-hidden="true">
          {display.engineKey
            ? <EngineLogo engine={display.engineKey} size={20} />
            : <Icon name={agent.role === "decision" ? "sparkles" : agent.role === "source" ? "file" : "network"} size={18} />}
        </span>
        <span>
          <strong>{unselected ? t("collab.runOverview") : display.title}</strong>
          <small>{unselected ? t("collab.coordinatorSubtitle") : roleLabel(agent, t)}</small>
        </span>
        <span className={`collab-inspector-status state-${agent.statusKind}`}>
          <i /><span>{statusLabel(agent, t)}</span>
        </span>
        <Button variant="ghost" isIconOnly className="collab-panel-close" aria-label={t("collab.hideDetails")} onClick={onClosePanel}>
          <Icon name="x" size={14} />
        </Button>
      </div>
      {outOfScope && (
        <div className="collab-inspector-scope" role="status">
          <Icon name="eyeOff" size={12} />
          <span>{t("collab.outOfScope")}</span>
          <button type="button" onClick={onReveal}>{t("collab.showHiddenAgent")}</button>
        </div>
      )}
      <div
        className="collab-inspector-tabs"
        role="tablist"
        aria-label={t("collab.details")}
        onKeyDown={(event: KeyboardEvent<HTMLDivElement>) => handleTabListKeyDown(event, inspectorTabs, activeTab, onTab)}
      >
        {inspectorTabs.map((value) => (
          <button
            type="button"
            key={value}
            id={`collab-tab-${value}`}
            role="tab"
            aria-selected={activeTab === value}
            aria-controls={`collab-panel-${value}`}
            tabIndex={activeTab === value ? 0 : -1}
            className={activeTab === value ? "on" : ""}
            onClick={() => onTab(value)}
          >
            {t(`collab.tab.${value}`)}
          </button>
        ))}
      </div>
      <div
        className="collab-inspector-body"
        ref={bodyRef}
        id={`collab-panel-${activeTab}`}
        role="tabpanel"
        aria-labelledby={`collab-tab-${activeTab}`}
      >
        {activeTab === "overview" && agent.role === "decision" && (
          <section className="collab-detail-section">
            <h3>{t("collab.role.decision")}</h3>
            <div className="collab-intent-detail">
              <strong>{agent.identity.model || t("collab.decision.unknown")}</strong>
              <p>{t("collab.decision.description")}</p>
            </div>
            <dl className="collab-runtime-detail">
              {agent.identity.engine && <><dt>{t("collab.field.engine")}</dt><dd>{agent.identity.engine}</dd></>}
              <dt>{t("collab.workspace.rounds")}</dt><dd>{deck.blackboard.reasonRuns.length}</dd>
              <dt>{t("collab.workspace.tasks")}</dt><dd>{deck.blackboard.reasonRuns.reduce((sum, run) => sum + run.proposed, 0)}</dd>
              <dt>{t("meta.tokens")}</dt><dd>{deck.costBySolver.reason ? compactNumber(agent.tokens) : "—"}</dd>
              <dt>{t("meta.cost")}</dt><dd>{deck.costBySolver.reason ? `$${agent.usd.toFixed(4)}` : "—"}</dd>
            </dl>
            <div className="collab-detail-actions">
              <Button size="sm" variant="ghost" onClick={() => onTab("decisions")}><Icon name="rows" size={14} />{t("collab.tab.decisions")} · {deck.blackboard.reasonRuns.length}</Button>
            </div>
          </section>
        )}
        {activeTab === "overview" && agent.role === "source" && (
          <section className="collab-detail-section">
            <h3>{t("collab.source.title")}</h3>
            <p className="collab-section-empty">{t("collab.workspace.inputHint")}</p>
            {agent.knowledge.map((item) => <KnowledgeRow key={item.id} item={item} actor={display.title} selected={false} onSelect={onSelectRow} mode={canvasMode} />)}
          </section>
        )}
        {activeTab === "overview" && agent.role !== "decision" && agent.role !== "source" && (isWorker
          ? <WorkerOverview agent={agent} display={display} actorName={actorName} knowledge={knowledge} onSelectKnowledge={onSelectKnowledge} t={t} mode={canvasMode} />
          : <CoordinatorOverview deck={deck} agent={agent} runStats={runStats} onViewActivity={() => onTab("activity")} t={t} mode={canvasMode} />)}
        {activeTab === "knowledge" && (
          <section className="collab-detail-section no-border">
            {selectedKnowledge && (
              <KnowledgeDetail
                key={selectedKnowledge.id}
                item={selectedKnowledge}
                actor={actorName(selectedKnowledge.agentId)}
                proposer={knowledgeProposerLabel(selectedKnowledge, actorName)}
                verifier={knowledgeVerifierLabel(selectedKnowledge, actorName, t)}
                knowledge={knowledge}
                intents={deck.blackboard.intents}
                onSelectKnowledge={onSelectKnowledge}
                onOpenFact={onOpenFact}
                onOpenPoc={onOpenPoc}
                onClose={onCloseKnowledge}
                mode={canvasMode}
              />
            )}
            <h3>{t("collab.agentKnowledge")}<span>{knowledgeHeading}</span></h3>
            <KnowledgeKindChips counts={kindCounts} selected={knowledgeKinds} onToggle={onToggleKnowledgeKind} mode={canvasMode} />
            <div className="collab-inspector-list">
              {shownKnowledge.length ? (
                <>
                  {shownKnowledge.map((item) => (
                    <KnowledgeRow
                      key={item.id}
                      item={item}
                      actor={actorName(item.agentId)}
                      selected={selectedKnowledge?.id === item.id}
                      onSelect={onSelectRow}
                      mode={canvasMode}
                    />
                  ))}
                  {visibleKnowledge.length > knowledgeLimit && (
                    <button
                      type="button"
                      className="collab-index-more"
                      onClick={() => setKnowledgeLimit((n) => n + LIST_LIMITS.knowledge)}
                    >
                      {t("collab.knowledgeMore", { shown: shownKnowledge.length, total: visibleKnowledge.length })}
                    </button>
                  )}
                </>
              ) : <div className="collab-section-empty">{t(knowledgeFiltered ? "collab.noMatches" : "collab.noKnowledge")}</div>}
            </div>
          </section>
        )}
        {activeTab === "activity" && <AgentActivityList key={agent.id} deck={deck} agent={agent} query={query} asOf={asOf} />}
        {activeTab === "decisions" && (
          <CoordinatorDecisions deck={deck} knowledge={knowledge} onSelectKnowledge={onSelectKnowledge} t={t} />
        )}
        {activeTab === "relations" && (
          <section className="collab-detail-section no-border">
            <h3>{t("collab.agentRelations")}<span>{agentRelations.length}</span></h3>
            <div className="collab-inspector-list">
              {agentRelations.length ? agentRelations.map((relation) => (
                <RelationRow
                  key={relation.id}
                  relation={relation}
                  agentId={agent.id}
                  actorName={actorName}
                  onSelect={() => onSelectRelation(relation)}
                />
              )) : <div className="collab-section-empty">{t("collab.noRelations")}</div>}
            </div>
          </section>
        )}
      </div>
      <div className="collab-inspector-actions">
        {isWorker && <WorkerPromptButton key={agent.id} deck={deck} workerId={agent.id} name={display.title} asOf={asOf} />}
        {isWorker && onOpenTimeline && (
          <Button size="sm" variant="ghost" onClick={() => onOpenTimeline(agent.id)}>
            <Icon name="rows" size={13} />{t("collab.action.timeline")}
          </Button>
        )}
        {isWorker && agent.online && onKillWorker && (
          <Button size="sm" variant="ghost" className="danger" onClick={() => onKillWorker(agent.id)}>
            <Icon name="stop" size={13} />{t("collab.action.stop")}
          </Button>
        )}
      </div>
    </>
  );
}
