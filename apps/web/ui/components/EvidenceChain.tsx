"use client";

import { useEffect, useMemo, useState } from "react";
import { DeckState, BlackboardFact, isFactRetired } from "@/lib/events";
import { useT, useLang } from "@/lib/i18n";
import { useCopied } from "@/lib/useCopied";
import { PanelEmpty } from "@/components/PanelEmpty";
import { Icon } from "@/components/Icon";

/**
 * The evidence chain — every provenance-gated fact (verified ✓ / candidate ?),
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
type EvidenceFilter = "all" | "verified" | "candidates" | "dead";

// ts may be unix seconds or ms — normalise to ms (mirrors ActivityStream).
function tsMs(ts: number): number {
  if (!ts) return 0;
  return ts < 1e12 ? ts * 1000 : ts;
}

// Coarse "x ago" stamp; facts carry a real `ts`, so this is genuine, not faked.
function relTime(ts: number, zh: boolean): string {
  const ms = tsMs(ts);
  if (!ms) return "";
  const sec = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (sec < 5) return zh ? "刚刚" : "just now";
  if (sec < 60) return zh ? `${sec} 秒前` : `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return zh ? `${min} 分钟前` : `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return zh ? `${hr} 小时前` : `${hr}h ago`;
  const day = Math.floor(hr / 24);
  return zh ? `${day} 天前` : `${day}d ago`;
}

function CopyFact({ text, t }: { text: string; t: (k: string, v?: Record<string, string | number>) => string }) {
  const [copied, copy] = useCopied();
  return (
    <button
      type="button"
      className={`evi-copy ${copied ? "copied" : ""}`.trim()}
      title={t("evidence.copyFact")}
      aria-label={t("evidence.copyFact")}
      onClick={() => copy(text)}
    >
      <Icon name={copied ? "check" : "copy"} size={13} />
    </button>
  );
}

function factKey(f: BlackboardFact, prefix: string, index: number): string {
  return `${prefix}${f.factSeq ?? `${f.actor}-${f.ts}-${index}`}`;
}

function FactItem({ f, t, zh, expanded, onToggle }: {
  f: BlackboardFact;
  t: (k: string, v?: Record<string, string | number>) => string;
  zh: boolean;
  expanded: boolean;
  onToggle: () => void;
}) {
  const gist = (f.summary || "").trim();
  const text = gist || f.fact;
  // When a gist is shown as the label, the FULL raw fact must stay one click away —
  // a gist can truncate/omit an anchor (flag/cred/port), so the operator needs the
  // verbatim text. Mirror the Blackboard card's <details> disclosure. Only render it
  // when the gist actually differs from the raw (no point disclosing identical text).
  const hasRaw = !!gist && gist !== f.fact;
  const when = relTime(f.ts, zh);
  return (
    <div className={`evi-item ${f.verified ? "v" : "c"} ${expanded ? "expanded" : ""}`.trim()}>
      <div className="evi-head">
        <button
          type="button"
          className="evi-row"
          aria-expanded={expanded}
          title={t(expanded ? "evidence.collapseFact" : "evidence.expandFact")}
          onClick={onToggle}
        >
          <span className="evi-fact">{text}</span>
          <span className="evi-meta-inline">
            <span className={f.verified ? "ok" : "warn"}>{f.verified ? t("insp.verified") : t("insp.unverified")}</span>
            <span>{Number(f.confidence).toFixed(2)}</span>
            <span>{f.actor}</span>
            {when && <span>{when}</span>}
            {f.witness && <span>{t("evidence.hasWitness")}</span>}
            {f.artifactId && <span>{t("evidence.hasArtifact")}</span>}
          </span>
          <Icon name="chevronDown" size={13} />
        </button>
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
            <dd className={f.verified ? "ok" : "warn"}>{f.verified ? t("insp.verified") : t("insp.unverified")}</dd>
            <dt>{t("insp.confidence")}</dt><dd>{Number(f.confidence).toFixed(2)}</dd>
            {f.verifier && f.verifier !== "none" && <><dt>{t("insp.verifier")}</dt><dd>{f.verifier}</dd></>}
            {f.witness && <><dt>{t("bb.witness")}</dt><dd className="witness">{f.witness}</dd></>}
            {f.artifactId && <><dt>{t("bb.artifact")}</dt><dd>{f.artifactId}</dd></>}
            <dt>{t("insp.actor")}</dt><dd>{f.actor}</dd>
            {when && <><dt>{t("evidence.sortLabel")}</dt><dd className="evi-when">{when}</dd></>}
          </dl>
        </div>
      )}
    </div>
  );
}

function DeadEndItem({ d, t, zh, expanded, onToggle }: {
  d: { reason: string; actor: string; ts: number };
  t: (k: string, v?: Record<string, string | number>) => string;
  zh: boolean;
  expanded: boolean;
  onToggle: () => void;
}) {
  const when = relTime(d.ts, zh);
  return (
    <div className={`evi-item d ${expanded ? "expanded" : ""}`.trim()}>
      <div className="evi-head">
        <button
          type="button"
          className="evi-row"
          aria-expanded={expanded}
          title={t(expanded ? "evidence.collapseFact" : "evidence.expandFact")}
          onClick={onToggle}
        >
          <span className="evi-fact">{d.reason}</span>
          <span className="evi-meta-inline">
            <span>{t("evidence.deadShort")}</span>
            <span>{d.actor}</span>
            {when && <span>{when}</span>}
          </span>
          <Icon name="chevronDown" size={13} />
        </button>
        <CopyFact text={d.reason} t={t} />
      </div>
      {expanded && (
        <div className="evi-detail">
          <dl className="evi-prov">
            <dt>{t("insp.actor")}</dt><dd>{d.actor}</dd>
            {when && <><dt>{t("evidence.sortLabel")}</dt><dd className="evi-when">{when}</dd></>}
          </dl>
        </div>
      )}
    </div>
  );
}

export function EvidenceChain({ deck }: { deck: DeckState }) {
  const t = useT();
  const { lang } = useLang();
  const zh = lang === "zh";

  const [newestFirst, setNewestFirst] = useState(true);
  const [filter, setFilter] = useState<EvidenceFilter>("all");
  const [expandedIds, setExpandedIds] = useState<Set<string>>(new Set());
  useEffect(() => {
    try {
      const v = localStorage.getItem(SORT_KEY);
      if (v != null) setNewestFirst(v === "1");
    } catch { /* private mode — keep default */ }
  }, []);
  const toggleSort = (next: boolean) => {
    setNewestFirst(next);
    try { localStorage.setItem(SORT_KEY, next ? "1" : "0"); } catch { /* best effort */ }
  };

  // Facts arrive chronologically; newest-first = reverse. Keep source arrays
  // untouched (memo over a copy) so other panels reading deck stay stable.
  const order = <T,>(arr: T[]): T[] => (newestFirst ? [...arr].reverse() : arr);

  // A: review-retired facts (rejected/merged/superseded) are NOT evidence — they
  // failed review and must not appear in the proof chain.
  const verified = useMemo(
    () => order(deck.blackboard.facts.filter((f) => f.verified && !isFactRetired(f))),
    [deck.blackboard.facts, newestFirst],
  );
  const candidates = useMemo(
    () => order(deck.blackboard.facts.filter((f) => !f.verified && !isFactRetired(f))),
    [deck.blackboard.facts, newestFirst],
  );
  const deadEnds = useMemo(
    () => order(deck.blackboard.deadEnds),
    [deck.blackboard.deadEnds, newestFirst],
  );
  const empty = verified.length === 0 && candidates.length === 0 && deadEnds.length === 0;
  const total = verified.length + candidates.length + deadEnds.length;
  const actorCount = useMemo(() => new Set([
    ...deck.blackboard.facts.map((f) => f.actor),
    ...deck.blackboard.deadEnds.map((d) => d.actor),
  ].filter(Boolean)).size, [deck.blackboard.facts, deck.blackboard.deadEnds]);
  const filterButtons: { key: EvidenceFilter; label: string; n: number }[] = [
    { key: "all", label: t("evidence.all"), n: total },
    { key: "verified", label: t("evidence.verifiedShort"), n: verified.length },
    { key: "candidates", label: t("evidence.candidatesShort"), n: candidates.length },
    { key: "dead", label: t("evidence.deadShort"), n: deadEnds.length },
  ];
  const toggleExpanded = (id: string) =>
    setExpandedIds((prev) => { const n = new Set(prev); n.has(id) ? n.delete(id) : n.add(id); return n; });

  return (
    <div className="panel-scroll-wrap evidence-panel">
      <div className="evi-toolbar">
        <div className="evi-toolbar-title">
          <div className="panel-title">{t("evidence.title")}</div>
          <div className="evi-summary">
            <span>{t("evidence.total", { n: total })}</span>
            <span>{t("evidence.actors", { n: actorCount })}</span>
            <span>{newestFirst ? t("evidence.sortNewest") : t("evidence.sortOldest")}</span>
          </div>
        </div>
        {!empty && (
          <div className="evi-controls">
            <div className="evi-filter" role="tablist" aria-label={t("evidence.filterLabel")}>
              {filterButtons.map((b) => (
                <button
                  key={b.key}
                  type="button"
                  role="tab"
                  aria-selected={filter === b.key}
                  className={`evi-filter-btn ${filter === b.key ? "on" : ""}`.trim()}
                  onClick={() => setFilter(b.key)}
                >
                  <span>{b.label}</span>
                  <b>{b.n}</b>
                </button>
              ))}
            </div>
            <div className="evi-sort" role="group" aria-label={t("evidence.sortLabel")}>
              <button
                type="button"
                className={`evi-sort-btn ${newestFirst ? "on" : ""}`.trim()}
                aria-pressed={newestFirst}
                onClick={() => toggleSort(true)}
              >
                {t("evidence.sortNewest")}
              </button>
              <button
                type="button"
                className={`evi-sort-btn ${!newestFirst ? "on" : ""}`.trim()}
                aria-pressed={!newestFirst}
                onClick={() => toggleSort(false)}
              >
                {t("evidence.sortOldest")}
              </button>
            </div>
          </div>
        )}
      </div>
      <div className="panel-scroll evi-scroll">
      {!empty && (
        <div className="evi-density-note">{t("evidence.clickHint")}</div>
      )}
      {empty ? (
        <PanelEmpty icon="layers" title={t("evidence.empty")} hint={t("evidence.emptyHint")} />
      ) : (
        <>
          {(filter === "all" || filter === "verified") && verified.length > 0 && (
            <div className="evi-group verified">
              <div className="evi-group-h">{t("evidence.verified", { n: verified.length })}</div>
              {verified.map((f, i) => {
                const id = factKey(f, "v", i);
                return <FactItem key={id} f={f} t={t} zh={zh} expanded={expandedIds.has(id)} onToggle={() => toggleExpanded(id)} />;
              })}
            </div>
          )}
          {(filter === "all" || filter === "candidates") && candidates.length > 0 && (
            <div className="evi-group candidates">
              <div className="evi-group-h">{t("evidence.candidates", { n: candidates.length })}</div>
              {candidates.map((f, i) => {
                const id = factKey(f, "c", i);
                return <FactItem key={id} f={f} t={t} zh={zh} expanded={expandedIds.has(id)} onToggle={() => toggleExpanded(id)} />;
              })}
            </div>
          )}
          {(filter === "all" || filter === "dead") && deadEnds.length > 0 && (
            <div className="evi-group dead">
              <div className="evi-group-h">{t("evidence.dead", { n: deadEnds.length })}</div>
              {deadEnds.map((d, i) => (
                <DeadEndItem
                  key={`d${d.actor}-${d.ts}-${i}`}
                  d={d}
                  t={t}
                  zh={zh}
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
