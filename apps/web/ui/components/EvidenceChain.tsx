"use client";

import { MotionIcon } from "@/components/MotionIcon";

import { Fragment, useCallback, useEffect, useMemo, useState } from "react";
import { DeckState, BlackboardDeadEnd, BlackboardFact, isFactRetired } from "@/lib/events";
import { canvasModeOf } from "@/lib/swarmProjection";
import { formatRelativeTime } from "@/lib/format";
import { useT, useLang } from "@/lib/i18n";
import { useCopied } from "@/lib/useCopied";
import { Icon } from "@/components/Icon";
import { Button, Tabs } from "@heroui/react";

/**
 * The evidence chain — admitted Facts, retained Observations, and dead-ends,
 * each with its witness / artifact / verifier disclosure, plus the dead-ends the
 * swarm ruled out. This is the "show me the proof" panel: the provenance verdict
 * for each fact, not just its text.
 *
 * Two operator affordances on top of the raw list:
 *   1. a newest-first / oldest-first toggle (default newest — the live frontier
 *      is what an operator usually reaches for). Facts arrive in chronological
 *      order, so reversing the array gives newest-first.
 *   2. a faint per-item copy button (reuses `useCopied`) to lift a fact/witness
 *      straight into a writeup.
 */

const SORT_KEY = "muteki.evidence.newestFirst";
type PentestEvidenceFilter = "all" | "verified" | "observations" | "dead";
type CtfEvidenceFilter = "all" | "facts" | "observations" | "dead" | "steps" | "resources" | "goals" | "flags";
type EvidenceFilter = PentestEvidenceFilter | CtfEvidenceFilter;

// ts may be unix seconds or ms — normalise to ms (mirrors ActivityStream).
function tsMs(ts: number): number {
  if (!ts) return 0;
  return ts < 1e12 ? ts * 1000 : ts;
}

function exactTime(ts?: number): string {
  const ms = tsMs(Number(ts || 0));
  return ms ? new Date(ms).toLocaleString() : "";
}

function CopyFact({ text, t }: { text: string; t: (k: string, v?: Record<string, string | number>) => string }) {
  const [copied, copy] = useCopied();
  return (
    <Button
      type="button"
      className={`evi-copy ${copied ? "copied" : ""}`.trim()}
      data-tooltip={t("evidence.copyFact")}
      aria-label={t("evidence.copyFact")}
      onClick={() => copy(text)}
    >
      <MotionIcon active={copied} from="copy" to="check" size={13} />
    </Button>
  );
}

function factKey(f: BlackboardFact, prefix: string, index: number): string {
  return `${prefix}${f.factSeq ?? `${f.actor}-${f.ts}-${index}`}`;
}

function FactItem({ f, t, zh, expanded, onToggle, statusLabel, ctf }: {
  f: BlackboardFact;
  t: (k: string, v?: Record<string, string | number>) => string;
  zh: boolean;
  expanded: boolean;
  onToggle: () => void;
  statusLabel?: string;
  ctf?: boolean;
}) {
  const gist = (f.summary || "").trim();
  const text = gist || f.fact;
  // When a gist is shown as the label, the FULL raw fact must stay one click away —
  // a gist can truncate/omit an anchor (flag/cred/port), so the operator needs the
  // verbatim text. Mirror the Blackboard card's <details> disclosure. Only render it
  // when the gist actually differs from the raw (no point disclosing identical text).
  const hasRaw = !!gist && gist !== f.fact;
  const when = formatRelativeTime(f.ts, t);
  return (
    <div id={f.factSeq ? `fact-${f.factSeq}` : undefined} className={`evi-item ${f.verified ? "v" : "c"} ${expanded ? "expanded" : ""}`.trim()}>
      <div className="evi-head">
        <Button
          type="button"
          className="evi-row"
          aria-expanded={expanded}
          aria-label={t(expanded ? "evidence.collapseFact" : "evidence.expandFact")}
          onClick={onToggle}
        >
          <span className="evi-fact">{text}</span>
          <span className="evi-meta-inline">
            <span className={f.verified ? "ok" : "warn"}>{statusLabel ?? (ctf ? t("insp.run.factRecorded") : (f.verified ? t("insp.verified") : t("insp.unverified")))}</span>
            <span>{Number(f.confidence).toFixed(2)}</span>
            <span>{f.actor}</span>
            {when && <span>{when}</span>}
            {f.witness && <span>{t("evidence.hasWitness")}</span>}
            {f.artifactId && <span>{t("evidence.hasArtifact")}</span>}
          </span>
          <Icon name="chevronDown" size={13} />
        </Button>
        <CopyFact text={f.fact || text} t={t} />
      </div>
      {expanded && (
        <div className="evi-detail">
          {hasRaw && (
            <details className="evi-raw-d">
              <summary className="evi-raw-more">{t("insp.raw")}</summary>
              <div className="evi-raw-t">{f.fact}</div>
            </details>
          )}
          <dl className="evi-prov">
            <dt>{t("insp.provenance")}</dt>
            <dd className={f.verified ? "ok" : "warn"}>{statusLabel ?? (ctf ? t("insp.run.factRecorded") : (f.verified ? t("insp.verified") : t("insp.unverified")))}</dd>
            <dt>{t("insp.confidence")}</dt><dd>{Number(f.confidence).toFixed(2)}</dd>
            {!ctf && f.verifier && f.verifier !== "none" && <><dt>{t("insp.verifier")}</dt><dd>{f.verifier}</dd></>}
            {f.witness && <><dt>{t("evidence.witness")}</dt><dd className="witness">{f.witness}</dd></>}
            {f.artifactId && <><dt>{t("evidence.artifact")}</dt><dd>{f.artifactId}</dd></>}
            <dt>{t("insp.actor")}</dt><dd>{f.actor}</dd>
            {f.intentId && <><dt>{ctf ? t("collab.field.step") : (zh ? "当前 Intent" : "Current intent")}</dt><dd>{f.intentId}</dd></>}
            {f.targetEpoch && <><dt>{zh ? "目标 epoch" : "Target epoch"}</dt><dd>{f.targetEpoch}</dd></>}
            {f.provenance?.toolEventId && <><dt>{zh ? "工具事件" : "Tool event"}</dt><dd>{f.provenance.toolEventId}</dd></>}
            {f.provenance?.toolEventTs && <><dt>{zh ? "工具事件时间" : "Tool event time"}</dt><dd>{exactTime(f.provenance.toolEventTs)}</dd></>}
            {f.provenance?.observedAt && <><dt>{zh ? "观察时间" : "Observed at"}</dt><dd>{exactTime(f.provenance.observedAt)}</dd></>}
            {f.provenance?.artifactSha256 && <><dt>Artifact SHA-256</dt><dd className="witness">{f.provenance.artifactSha256}</dd></>}
            {f.sourceObservationSeq && <><dt>{t("evidence.sourceObservation")}</dt><dd>#{f.sourceObservationSeq}</dd></>}
            {f.provenance?.artifactRefs?.map((ref) => (
              <Fragment key={ref.artifactId}>
                <dt>{t("evidence.artifact")}</dt>
                <dd>{ref.artifactId}{ref.command ? ` · ${ref.command}` : ""}{ref.size != null ? ` · ${ref.size} bytes` : ""}</dd>
                {ref.sha256 && <><dt>SHA-256</dt><dd className="witness">{ref.sha256}</dd></>}
              </Fragment>
            ))}
            {!!f.observations?.length && <><dt>{zh ? "独立观察" : "Observations"}</dt><dd>{f.observations.length}</dd></>}
            {when && <><dt>{t("evidence.sortLabel")}</dt><dd className="evi-when">{when}</dd></>}
          </dl>
          {!!f.observations?.length && (
            <details className="evi-raw-d">
              <summary className="evi-raw-more">{zh ? "查看全部观察来源" : "View all observation sources"}</summary>
              <div className="evi-raw-t">
                {f.observations.map((observation, index) => (
                  <div key={observation.observationSeq ?? observation.provenance?.toolEventId ?? `${observation.actor}-${index}`}>
                    #{observation.observationSeq ?? "—"} · {observation.actor}
                    {observation.provenance?.intentId ? ` · ${observation.provenance.intentId}` : ""}
                    {observation.provenance?.targetEpoch ? ` · epoch ${observation.provenance.targetEpoch}` : ""}
                    {observation.provenance?.toolEventId ? ` · ${observation.provenance.toolEventId}` : ""}
                  </div>
                ))}
              </div>
            </details>
          )}
        </div>
      )}
    </div>
  );
}

function GenericEvidenceItem({
  text,
  status,
  actor,
  ts,
  t,
  expanded,
  onToggle,
  tone,
  detailRows,
}: {
  text: string;
  status: string;
  actor?: string;
  ts?: number;
  t: (k: string, v?: Record<string, string | number>) => string;
  expanded: boolean;
  onToggle: () => void;
  tone?: "v" | "c" | "d";
  detailRows?: { label: string; value: string }[];
}) {
  const when = formatRelativeTime(ts || 0, t);
  return (
    <div className={`evi-item ${tone || "c"} ${expanded ? "expanded" : ""}`.trim()}>
      <div className="evi-head">
        <Button
          type="button"
          className="evi-row"
          aria-expanded={expanded}
          aria-label={t(expanded ? "evidence.collapseFact" : "evidence.expandFact")}
          onClick={onToggle}
        >
          <span className="evi-fact">{text}</span>
          <span className="evi-meta-inline">
            <span>{status}</span>
            {actor && <span>{actor}</span>}
            {when && <span>{when}</span>}
          </span>
          <Icon name="chevronDown" size={13} />
        </Button>
        <CopyFact text={text} t={t} />
      </div>
      {expanded && (
        <div className="evi-detail">
          <dl className="evi-prov">
            <dt>{t("insp.provenance")}</dt><dd>{status}</dd>
            {actor && <><dt>{t("insp.actor")}</dt><dd>{actor}</dd></>}
            {detailRows?.map((row, index) => (
              <Fragment key={`${row.label}-${index}`}>
                <dt>{row.label}</dt><dd>{row.value}</dd>
              </Fragment>
            ))}
            {when && <><dt>{t("evidence.sortLabel")}</dt><dd className="evi-when">{when}</dd></>}
          </dl>
        </div>
      )}
    </div>
  );
}

function DeadEndItem({ d, t, expanded, onToggle }: {
  d: BlackboardDeadEnd;
  t: (k: string, v?: Record<string, string | number>) => string;
  expanded: boolean;
  onToggle: () => void;
}) {
  const when = formatRelativeTime(d.ts, t);
  return (
    <div className={`evi-item d ${expanded ? "expanded" : ""}`.trim()}>
      <div className="evi-head">
        <Button
          type="button"
          className="evi-row"
          aria-expanded={expanded}
          aria-label={t(expanded ? "evidence.collapseFact" : "evidence.expandFact")}
          onClick={onToggle}
        >
          <span className="evi-fact">{d.reason}</span>
          <span className="evi-meta-inline">
            <span>{t("evidence.deadShort")}</span>
            <span>{d.actor}</span>
            {when && <span>{when}</span>}
          </span>
          <Icon name="chevronDown" size={13} />
        </Button>
        <CopyFact text={d.reason} t={t} />
      </div>
      {expanded && (
        <div className="evi-detail">
          <dl className="evi-prov">
            <dt>{t("insp.actor")}</dt><dd>{d.actor}</dd>
            {d.testedScope && <><dt>{t("evidence.testedScope")}</dt><dd>{d.testedScope}</dd></>}
            {d.observedResult && <><dt>{t("evidence.observedResult")}</dt><dd>{d.observedResult}</dd></>}
            {d.intentId && <><dt>{t("collab.field.step")}</dt><dd>{d.intentId}</dd></>}
            {d.targetEpoch && <><dt>Epoch</dt><dd>{d.targetEpoch}</dd></>}
            {when && <><dt>{t("evidence.sortLabel")}</dt><dd className="evi-when">{when}</dd></>}
          </dl>
        </div>
      )}
    </div>
  );
}

function flagStatusLabel(
  status: "internal" | "pending" | "accepted" | "rejected",
  t: (k: string, v?: Record<string, string | number>) => string,
): string {
  switch (status) {
    case "accepted":
      return t("insp.run.platformAccepted");
    case "rejected":
      return t("insp.run.platformRejected");
    case "pending":
      return t("insp.run.platformPending");
    case "internal":
      return t("insp.run.platformInternal");
    default: {
      const _never: never = status;
      return _never;
    }
  }
}

export function EvidenceChain({ deck, focusFactSeq, focusNonce }: { deck: DeckState; focusFactSeq?: number; focusNonce?: number }) {
  const t = useT();
  const { lang } = useLang();
  const zh = lang === "zh";
  const isCtf = canvasModeOf(deck) === "ctf";

  const [newestFirst, setNewestFirst] = useState(true);
  const [filter, setFilter] = useState<EvidenceFilter>("all");
  const [expandedIds, setExpandedIds] = useState<Set<string>>(new Set());
  useEffect(() => {
    try {
      const v = localStorage.getItem(SORT_KEY);
      if (v != null) setNewestFirst(v === "1");
    } catch { /* private mode — keep default */ }
  }, []);
  useEffect(() => {
    if (!focusFactSeq) return;
    const id = `v${focusFactSeq}`;
    setExpandedIds((prev) => { const next = new Set(prev); next.add(id); return next; });
    const node = document.getElementById(`fact-${focusFactSeq}`);
    node?.scrollIntoView({ block: "nearest" });
  }, [focusFactSeq, focusNonce]);
  const toggleSort = (next: boolean) => {
    setNewestFirst(next);
    try { localStorage.setItem(SORT_KEY, next ? "1" : "0"); } catch { /* best effort */ }
  };

  // Facts arrive chronologically; newest-first = reverse. Keep source arrays
  // untouched (memo over a copy) so other panels reading deck stay stable.
  const order = useCallback(<T,>(arr: T[]): T[] => (newestFirst ? [...arr].reverse() : arr), [newestFirst]);

  // A: review-retired facts (rejected/merged/superseded) are NOT evidence — they
  // failed review and must not appear in the proof chain.
  const verified = useMemo(
    () => order(deck.blackboard.facts.filter((f) => f.verified && !isFactRetired(f))),
    [deck.blackboard.facts, order],
  );
  const observations = useMemo(
    () => order(deck.blackboard.observations
      .filter((observation) => !observation.admitted)
      .map((observation): BlackboardFact => ({
        fact: observation.text,
        verified: false,
        confidence: observation.confidence,
        actor: observation.actor,
        verifier: "",
        witness: observation.witness,
        artifactId: observation.artifactId,
        targetEpoch: observation.targetEpoch,
        provenance: observation.provenance,
        observations: [{
          observationSeq: observation.observationSeq,
          actor: observation.actor,
          verified: false,
          confidence: observation.confidence,
          artifactId: observation.artifactId,
          witness: observation.witness,
          provenance: observation.provenance,
          ts: observation.ts,
        }],
        intentId: observation.intentId,
        ts: observation.ts,
      }))),
    [deck.blackboard.observations, order],
  );
  const deadEnds = useMemo(
    () => order(deck.blackboard.deadEnds),
    [deck.blackboard.deadEnds, order],
  );
  const ctfFacts = useMemo(
    () => order(deck.blackboard.facts.filter((f) => !isFactRetired(f))),
    [deck.blackboard.facts, order],
  );
  const ctfSteps = useMemo(
    () => order(deck.blackboard.intents.map((intent) => ({
      intent,
      id: intent.id,
      text: intent.summary || intent.goal,
      actor: intent.worker || intent.proposedBy || "",
      ts: intent.proposedTs,
      status: intent.dispatchState || intent.status,
    }))),
    [deck.blackboard.intents, order],
  );
  const ctfResources = useMemo(
    () => order(deck.blackboard.pocs),
    [deck.blackboard.pocs, order],
  );
  const ctfGoals = useMemo(() => {
    const title = (deck.taskContract?.completion.goal || "").trim() || (zh ? "提交全部 FLAG" : "Submit every FLAG");
    const satisfied = !!deck.solved || !!deck.reason.goalMet;
    return [{
      id: "goal:final",
      text: title,
      status: satisfied ? t("collab.status.satisfied") : t("collab.status.open"),
      actor: "",
      ts: deck.finishedAt || deck.startedAt || 0,
    }];
  }, [deck.finishedAt, deck.reason.goalMet, deck.solved, deck.startedAt, deck.taskContract, t, zh]);
  const ctfFlags = useMemo(() => {
    if (deck.flagConfirmations.length) {
      return order(deck.flagConfirmations.map((row) => ({
        id: row.id,
        text: row.title || row.flag,
        status: flagStatusLabel(row.status, t),
        actor: row.actor || "",
        ts: row.ts,
        tone: row.status === "accepted" ? "v" as const : row.status === "rejected" ? "d" as const : "c" as const,
      })));
    }
    return order(deck.flags.map((flag, index) => ({
      id: `flag:${index}`,
      text: flag,
      status: t("insp.run.platformInternal"),
      actor: "",
      ts: 0,
      tone: "c" as const,
    })));
  }, [deck.flagConfirmations, deck.flags, order, t]);
  const latestReason = deck.blackboard.reasonRuns?.at(-1);
  const empty = isCtf
    ? ctfFacts.length === 0 && observations.length === 0 && deadEnds.length === 0 && ctfSteps.length === 0 && ctfResources.length === 0 && ctfGoals.length === 0 && ctfFlags.length === 0
    : verified.length === 0 && observations.length === 0 && deadEnds.length === 0;
  const total = isCtf
    ? ctfFacts.length + observations.length + deadEnds.length + ctfSteps.length + ctfResources.length + ctfGoals.length + ctfFlags.length
    : verified.length + observations.length + deadEnds.length;
  const actorCount = useMemo(() => new Set([
    ...deck.blackboard.facts.map((f) => f.actor),
    ...deck.blackboard.observations.map((observation) => observation.actor),
    ...deck.blackboard.deadEnds.map((d) => d.actor),
    ...deck.blackboard.intents.map((intent) => intent.worker || intent.proposedBy || ""),
  ].filter(Boolean)).size, [deck.blackboard.facts, deck.blackboard.observations, deck.blackboard.deadEnds, deck.blackboard.intents]);
  const filterButtons: { key: EvidenceFilter; label: string; n: number }[] = isCtf
    ? [
        { key: "all", label: t("evidence.all"), n: total },
        { key: "facts", label: t("evidence.factsShort"), n: ctfFacts.length },
        { key: "observations", label: t("evidence.observationsShort"), n: observations.length },
        { key: "dead", label: t("evidence.deadShort"), n: deadEnds.length },
        { key: "steps", label: t("evidence.stepsShort"), n: ctfSteps.length },
        { key: "resources", label: t("evidence.resourcesShort"), n: ctfResources.length },
        { key: "goals", label: t("evidence.goalsShort"), n: ctfGoals.length },
        { key: "flags", label: t("evidence.flagsShort"), n: ctfFlags.length },
      ]
    : [
        { key: "all", label: t("evidence.all"), n: total },
        { key: "verified", label: t("evidence.verifiedShort"), n: verified.length },
        { key: "observations", label: t("evidence.observationsShort"), n: observations.length },
        { key: "dead", label: t("evidence.deadShort"), n: deadEnds.length },
      ];
  const toggleExpanded = (id: string) =>
    setExpandedIds((prev) => { const n = new Set(prev); if (n.has(id)) n.delete(id); else n.add(id); return n; });

  return (
    <div className="panel-scroll-wrap evidence-panel">
      <div className="evi-toolbar">
        <div className="evi-toolbar-title">
          <div className="panel-title">{t(isCtf ? "evidence.titleCtf" : "evidence.title")}</div>
          <div className="evi-summary">
            {deck.blackboard.truncated?.facts && <span>{t("runtime.truncated", { n: 200 })}</span>}
            <span>{t("evidence.total", { n: total })}</span>
            <span>{t("evidence.actors", { n: actorCount })}</span>
            <span>{newestFirst ? t("evidence.sortNewest") : t("evidence.sortOldest")}</span>
          </div>
        </div>
        {!empty && (
          <div className="evi-controls">
            <Tabs selectedKey={filter} onSelectionChange={(key) => setFilter(key as EvidenceFilter)} aria-label={t("evidence.filterLabel")}>
              <Tabs.List className="evi-filter">
              {filterButtons.map((b) => (
                <Tabs.Tab
                  id={b.key}
                  key={b.key}
                  className={`evi-filter-btn ${filter === b.key ? "on" : ""}`.trim()}
                >
                  <span>{b.label}</span>
                  <b>{b.n}</b>
                </Tabs.Tab>
              ))}
              </Tabs.List>
            </Tabs>
            <div className="evi-sort" role="group" aria-label={t("evidence.sortLabel")}>
              <Button
                type="button"
                className={`evi-sort-btn ${newestFirst ? "on" : ""}`.trim()}
                aria-pressed={newestFirst}
                onClick={() => toggleSort(true)}
              >
                {t("evidence.sortNewest")}
              </Button>
              <Button
                type="button"
                className={`evi-sort-btn ${!newestFirst ? "on" : ""}`.trim()}
                aria-pressed={!newestFirst}
                onClick={() => toggleSort(false)}
              >
                {t("evidence.sortOldest")}
              </Button>
            </div>
          </div>
        )}
      </div>
      <div className="panel-scroll evi-scroll">
      {latestReason && (
        <div className="evi-group">
          <div className="evi-group-h">{isCtf ? (zh ? "最近一次 Decide 诊断" : "Latest Decide diagnostics") : (zh ? "最近一次 Reason 诊断" : "Latest Reason diagnostics")}</div>
          <details className="evi-item">
            <summary className="evi-row">
              <span className="evi-fact">
                {latestReason.attempts.at(-1)?.responseStatus ?? "not_recorded"}
                {" · "}{latestReason.attempts.at(-1)?.parseStatus ?? "not_recorded"}
              </span>
              <span className="evi-meta-inline">
                <span>{zh ? `${latestReason.proposed} 个已接收 Intent` : `${latestReason.proposed} intents accepted`}</span>
                <span>{zh ? `${latestReason.droppedTotal} 个已过滤` : `${latestReason.droppedTotal} filtered`}</span>
                {latestReason.attempts.some((attempt) => attempt.timedOut) && <span className="warn">timeout</span>}
              </span>
            </summary>
            <div className="evi-detail">
              {latestReason.attempts.map((attempt) => (
                <dl className="evi-prov" key={attempt.attemptIndex}>
                  <dt>{zh ? "调用" : "Attempt"}</dt><dd>#{attempt.attemptIndex}</dd>
                  <dt>{zh ? "原始响应状态" : "Raw response status"}</dt><dd>{attempt.responseStatus}</dd>
                  <dt>finish_reason</dt><dd>{attempt.finishReason || "—"}</dd>
                  <dt>{zh ? "超时" : "Timed out"}</dt><dd>{attempt.timedOut ? (zh ? "是" : "yes") : (zh ? "否" : "no")}</dd>
                  <dt>{zh ? "解析结果" : "Parse result"}</dt><dd>{attempt.parseStatus}</dd>
                  {attempt.parseDetail && <><dt>{zh ? "解析说明" : "Parse detail"}</dt><dd>{attempt.parseDetail}</dd></>}
                  <dt>{zh ? "响应字符数" : "Response characters"}</dt><dd>{attempt.rawResponseChars}</dd>
                  {attempt.rawResponseArtifactId && <><dt>{zh ? "原始响应 artifact" : "Raw response artifact"}</dt><dd>{attempt.rawResponseArtifactId}</dd></>}
                  {attempt.rawResponseSha256 && <><dt>Response SHA-256</dt><dd className="witness">{attempt.rawResponseSha256}</dd></>}
                </dl>
              ))}
              {!!latestReason.intentDecisions.length && (
                <details className="evi-raw-d">
                  <summary className="evi-raw-more">{zh ? "查看每条 Intent 的处理结果" : "View every intent decision"}</summary>
                  <div className="evi-raw-t">
                    {latestReason.intentDecisions.map((decision, index) => (
                      <div key={`${decision.rawIndex}-${decision.stage}-${index}`}>
                        #{decision.rawIndex} · {decision.modelIntentId || "—"} · {decision.outcome} · {decision.reasonCode}
                        {decision.goal ? ` · ${decision.goal}` : ""}
                      </div>
                    ))}
                  </div>
                </details>
              )}
            </div>
          </details>
        </div>
      )}
      {!empty && (
        <div className="evi-density-note">{t(isCtf ? "evidence.clickHintCtf" : "evidence.clickHint")}</div>
      )}
      {empty ? (
        <div className="panel-empty"><span className="panel-empty-ico" aria-hidden="true"><Icon name="layers" size={26} /></span><span className="panel-empty-title">{t("evidence.empty")}</span><span className="panel-empty-hint">{t(isCtf ? "evidence.emptyHintCtf" : "evidence.emptyHint")}</span></div>
      ) : (
        <>
          {isCtf && (filter === "all" || filter === "facts") && ctfFacts.length > 0 && (
            <div className="evi-group verified">
              <div className="evi-group-h">{t("evidence.facts", { n: ctfFacts.length })}</div>
              {ctfFacts.map((f, i) => {
                const id = factKey(f, "f", i);
                return (
                  <FactItem
                    key={id}
                    f={f}
                    t={t}
                    zh={zh}
                    ctf
                    statusLabel={t("insp.run.factRecorded")}
                    expanded={expandedIds.has(id)}
                    onToggle={() => toggleExpanded(id)}
                  />
                );
              })}
            </div>
          )}
          {isCtf && (filter === "all" || filter === "observations") && observations.length > 0 && (
            <div className="evi-group candidates">
              <div className="evi-group-h">{t("evidence.observations", { n: observations.length })}</div>
              {observations.map((f, i) => {
                const observationSeq = f.observations?.[0]?.observationSeq;
                const id = `o${observationSeq ?? `${f.actor}-${f.ts}-${i}`}`;
                return <FactItem key={id} f={f} t={t} zh={zh} ctf statusLabel={t("evidence.observationStatus")} expanded={expandedIds.has(id)} onToggle={() => toggleExpanded(id)} />;
              })}
            </div>
          )}
          {isCtf && (filter === "all" || filter === "dead") && deadEnds.length > 0 && (
            <div className="evi-group dead">
              <div className="evi-group-h">{t("evidence.dead", { n: deadEnds.length })}</div>
              {deadEnds.map((d, i) => {
                const id = `d${d.deadEndSeq ?? `${d.actor}-${d.ts}-${i}`}`;
                return <DeadEndItem key={id} d={d} t={t} expanded={expandedIds.has(id)} onToggle={() => toggleExpanded(id)} />;
              })}
            </div>
          )}
          {isCtf && (filter === "all" || filter === "steps") && ctfSteps.length > 0 && (
            <div className="evi-group">
              <div className="evi-group-h">{t("evidence.steps", { n: ctfSteps.length })}</div>
              {ctfSteps.map((step) => {
                const id = `s${step.id}`;
                return (
                  <GenericEvidenceItem
                    key={id}
                    text={step.text}
                    status={step.status}
                    actor={step.actor}
                    ts={step.ts}
                    detailRows={[
                      { label: t("collab.field.step"), value: step.id },
                      ...(step.intent.fromFacts.length ? [{ label: t("collab.field.inputs"), value: step.intent.fromFacts.map((seq) => `#${seq}`).join(", ") }] : []),
                      ...(step.intent.expectedObservable ? [{ label: t("collab.field.expected"), value: step.intent.expectedObservable }] : []),
                      ...(step.intent.stopCondition ? [{ label: t("collab.field.stop"), value: step.intent.stopCondition }] : []),
                      ...(step.intent.coverageKey ? [{ label: t("collab.field.coverage"), value: step.intent.coverageKey }] : []),
                      ...(step.intent.requiredPocs?.length ? [{ label: t("collab.knowledge.resource"), value: step.intent.requiredPocs.join(", ") }] : []),
                    ]}
                    t={t}
                    expanded={expandedIds.has(id)}
                    onToggle={() => toggleExpanded(id)}
                  />
                );
              })}
            </div>
          )}
          {isCtf && (filter === "all" || filter === "resources") && ctfResources.length > 0 && (
            <div className="evi-group">
              <div className="evi-group-h">{t("evidence.resources", { n: ctfResources.length })}</div>
              {ctfResources.map((resource) => {
                const id = `r${resource.id}`;
                return (
                  <GenericEvidenceItem
                    key={id}
                    text={resource.name || resource.id}
                    status={resource.status}
                    actor={resource.worker}
                    ts={resource.savedTs}
                    t={t}
                    detailRows={[
                      { label: "ID", value: resource.id },
                      ...(resource.path ? [{ label: t("evidence.path"), value: resource.path }] : []),
                      ...(resource.artifactId ? [{ label: t("evidence.artifact"), value: resource.artifactId }] : []),
                      ...(resource.entryCommand ? [{ label: t("evidence.entryCommand"), value: resource.entryCommand }] : []),
                      ...(resource.note ? [{ label: t("evidence.note"), value: resource.note }] : []),
                      ...(resource.intentId ? [{ label: t("collab.field.step"), value: resource.intentId }] : []),
                    ]}
                    expanded={expandedIds.has(id)}
                    onToggle={() => toggleExpanded(id)}
                  />
                );
              })}
            </div>
          )}
          {isCtf && (filter === "all" || filter === "goals") && ctfGoals.length > 0 && (
            <div className="evi-group">
              <div className="evi-group-h">{t("evidence.goals", { n: ctfGoals.length })}</div>
              {ctfGoals.map((goal) => {
                const id = goal.id;
                return (
                  <GenericEvidenceItem
                    key={id}
                    text={goal.text}
                    status={goal.status}
                    ts={goal.ts}
                    t={t}
                    tone={deck.solved || deck.reason.goalMet ? "v" : "c"}
                    expanded={expandedIds.has(id)}
                    onToggle={() => toggleExpanded(id)}
                  />
                );
              })}
            </div>
          )}
          {isCtf && (filter === "all" || filter === "flags") && ctfFlags.length > 0 && (
            <div className="evi-group">
              <div className="evi-group-h">{t("evidence.flags", { n: ctfFlags.length })}</div>
              {ctfFlags.map((row) => (
                <GenericEvidenceItem
                  key={row.id}
                  text={row.text}
                  status={row.status}
                  actor={row.actor}
                  ts={row.ts}
                  t={t}
                  tone={row.tone}
                  expanded={expandedIds.has(row.id)}
                  onToggle={() => toggleExpanded(row.id)}
                />
              ))}
            </div>
          )}
          {!isCtf && (filter === "all" || filter === "verified") && verified.length > 0 && (
            <div className="evi-group verified">
              <div className="evi-group-h">{t("evidence.verified", { n: verified.length })}</div>
              {verified.map((f, i) => {
                const id = factKey(f, "v", i);
                return <FactItem key={id} f={f} t={t} zh={zh} expanded={expandedIds.has(id)} onToggle={() => toggleExpanded(id)} />;
              })}
            </div>
          )}
          {!isCtf && (filter === "all" || filter === "observations") && observations.length > 0 && (
            <div className="evi-group candidates">
              <div className="evi-group-h">{t("evidence.observations", { n: observations.length })}</div>
              {observations.map((f, i) => {
                const observationSeq = f.observations?.[0]?.observationSeq;
                const id = `o${observationSeq ?? `${f.actor}-${f.ts}-${i}`}`;
                return <FactItem key={id} f={f} t={t} zh={zh} statusLabel={t("evidence.observationStatus")} expanded={expandedIds.has(id)} onToggle={() => toggleExpanded(id)} />;
              })}
            </div>
          )}
          {!isCtf && (filter === "all" || filter === "dead") && deadEnds.length > 0 && (
            <div className="evi-group dead">
              <div className="evi-group-h">{t("evidence.dead", { n: deadEnds.length })}</div>
              {deadEnds.map((d, i) => (
                <DeadEndItem
                  key={`d${d.actor}-${d.ts}-${i}`}
                  d={d}
                  t={t}
                  expanded={expandedIds.has(`d${d.actor}-${d.ts}-${i}`)}
                  onToggle={() => toggleExpanded(`d${d.actor}-${d.ts}-${i}`)}
                />
              ))}
            </div>
          )}
        </>
      )}
      </div>
    </div>
  );
}
