"use client";

import { useEffect, useState, useRef, type ReactNode } from "react";
import type {
  BlackboardFact,
  BlackboardPoc,
  BlackboardReviewFinding,
  BlackboardVulnReport,
  VulnReportStatus,
} from "@/lib/events";
import { isFactRetired } from "@/lib/events";
import {
  estimateCvss,
  findingClassLabel,
  reportSummary,
  reportToMarkdown,
  reportsToCollectionMarkdown,
  reproIntentId,
  type CvssRating,
} from "@/lib/reportMarkdown";
import { useCopied } from "@/lib/useCopied";
import { useT } from "@/lib/i18n";
import { Icon } from "@/components/Icon";

export type ReportCredential = { entity: string; value: string; seq?: number };
export type ReportStatusFilter = "all" | VulnReportStatus;

function statusLabel(
  status: VulnReportStatus,
  t: (key: string) => string,
): string {
  switch (status) {
    case "accepted":
      return t("runtime.reports.accepted");
    case "submitted":
      return t("runtime.reports.submitted");
    case "reproduced":
      return t("runtime.reports.reproduced");
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

function badgeClass(status: VulnReportStatus): string {
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

function reportIntentIds(row: BlackboardVulnReport): string[] {
  const ids: string[] = [];
  if (row.intentId) ids.push(row.intentId);
  if (row.id) ids.push(reproIntentId(row.id));
  return ids;
}

function severityLabel(rating: CvssRating, t: (key: string) => string): string {
  switch (rating) {
    case "critical":
      return t("runtime.reports.severity.critical");
    case "high":
      return t("runtime.reports.severity.high");
    case "medium":
      return t("runtime.reports.severity.medium");
    case "low":
      return t("runtime.reports.severity.low");
    default: {
      const _never: never = rating;
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

function CopyAction({
  text,
  label,
  titleKey,
  ariaKey,
  className,
}: {
  text: string;
  label?: string;
  titleKey: string;
  ariaKey: string;
  className: string;
}) {
  const t = useT();
  const [copied, copy] = useCopied();
  return (
    <button
      type="button"
      className={`${className} ${copied ? "copied" : ""}`.trim()}
      title={t(titleKey)}
      aria-label={t(ariaKey, { text })}
      onClick={() => copy(text)}
    >
      <Icon name={copied ? "check" : "copy"} size={13} />
      {label !== undefined && <span>{copied ? t("common.copied") : label}</span>}
    </button>
  );
}

function Field({ label, value }: { label: string; value?: string }) {
  if (!value) return null;
  return (
    <div className="artifact-row-body">
      <b>{label}</b>
      {" · "}
      {value}
    </div>
  );
}

export function VulnReportDoc({
  row,
  clock,
  focused,
  focusNonce,
  relatedFacts = [],
  relatedPocs = [],
  relatedCreds = [],
  relatedReviews = [],
  onOpenFact,
  onOpenPoc,
}: {
  row: BlackboardVulnReport;
  clock?: (ts: number) => string;
  focused?: boolean;
  focusNonce?: number;
  relatedFacts?: BlackboardFact[];
  relatedPocs?: BlackboardPoc[];
  relatedCreds?: ReportCredential[];
  relatedReviews?: BlackboardReviewFinding[];
  onOpenFact?: (factSeq: number) => void;
  onOpenPoc?: (pocId: string) => void;
}) {
  const t = useT();
  const [open, setOpen] = useState(!!focused);
  const rootRef = useRef<HTMLElement>(null);
  useEffect(() => {
    if (!focused) return;
    setOpen(true);
    rootRef.current?.scrollIntoView({ block: "nearest" });
  }, [focused, focusNonce]);
  const markdown = reportToMarkdown(row);
  const cvss = estimateCvss(row);
  const summary = reportSummary(row);
  const impact = [row.impactWho, row.impactWhat].filter(Boolean).join(" — ");
  const witness = (row.witness ?? "").trim();
  const witnessFacts = witness.length >= 8
    ? relatedFacts.filter((fact) => !isFactRetired(fact) && (`${fact.fact}\n${fact.witness ?? ""}`).includes(witness))
    : [];
  const history = row.history?.length
    ? row.history
    : [{ status: row.status, ts: row.ts, actor: row.actor, eventSeq: row.eventSeq ?? 0, reason: row.reason }];
  return (
    <article
      ref={rootRef}
      className={`artifact-row report-row ${open ? "expanded" : "collapsed"} ${row.status === "accepted" ? "finding-accepted" : ""} ${focused ? "report-focused" : ""}`}
    >
      <div className="artifact-row-top">
        <span
          className={`artifact-badge ${severityBadgeClass(cvss.rating)}`}
          title={t("runtime.reports.cvssHint")}
        >
          {cvss.badge}
        </span>
        <span className={`artifact-badge ${badgeClass(row.status)}`}>{statusLabel(row.status, t)}</span>
        <button
          type="button"
          className="report-toggle"
          aria-expanded={open}
          title={t(open ? "runtime.reports.collapse" : "runtime.reports.expand")}
          onClick={() => setOpen((value) => !value)}
        >
          <span className="artifact-row-title">{row.title || row.id}</span>
          <Icon name="chevronDown" size={13} />
        </button>
        {row.findingClass && (
          <span className="artifact-chip">{findingClassLabel(row.findingClass)}</span>
        )}
        <CopyAction
          text={markdown}
          titleKey="runtime.reports.copyMarkdown"
          ariaKey="runtime.reports.copyMarkdownAria"
          className="evi-copy"
        />
      </div>
      {!open && (
        <div className="report-summary">
          <Field label={t("runtime.reports.resource")} value={row.resourceId} />
          {summary && <div className="artifact-row-body">{summary}</div>}
        </div>
      )}
      {open && (
        <>
          <Field label={t("runtime.reports.class")} value={row.findingClass ? `${findingClassLabel(row.findingClass)} (${row.findingClass})` : undefined} />
          <Field label={t("runtime.reports.resource")} value={row.resourceId} />
          <Field
            label={t("runtime.reports.cvss")}
            value={`${severityLabel(cvss.rating, t)} ${cvss.score.toFixed(1)}（${cvss.vector}）`}
          />
          <Field label={t("runtime.reports.preconditions")} value={row.preconditions} />
          <Field label={t("runtime.reports.role")} value={row.affectedRole} />
          {row.narrative && <div className="artifact-row-body">{row.narrative}</div>}
          {row.reason && <div className="artifact-row-body">{row.reason}</div>}
          {row.steps && row.steps.length > 0 && (
            <>
              <div className="artifact-row-body"><b>{t("runtime.reports.steps")}</b></div>
              <ol className="report-steps">
                {row.steps.map((step, index) => (
                  <li key={`${row.id}-step-${index}`}>{step}</li>
                ))}
              </ol>
            </>
          )}
          {row.replayCommand && (
            <>
              <div className="artifact-row-body"><b>{t("runtime.reports.replay")}</b></div>
              <code className="artifact-code">{row.replayCommand}</code>
            </>
          )}
          {row.witness && (
            <>
              <div className="artifact-row-body"><b>{t("runtime.reports.witness")}</b></div>
              <code className="artifact-code">{row.witness}</code>
              {witnessFacts.length > 0 && (
                <div className="report-links">
                  {witnessFacts.map((fact) => fact.factSeq ? (
                    <button
                      type="button"
                      key={`wit-${fact.factSeq}`}
                      className="report-link-btn"
                      onClick={() => onOpenFact?.(fact.factSeq!)}
                    >
                      {t("runtime.reports.openEvidence")} #{fact.factSeq}
                    </button>
                  ) : null)}
                </div>
              )}
            </>
          )}
          <Field label={t("runtime.reports.impact")} value={impact} />
          {history.length > 0 && (
            <>
              <div className="artifact-row-body"><b>{t("runtime.reports.lifecycle")}</b></div>
              <ol className="report-history">
                {history.map((entry, index) => (
                  <li key={`${row.id}-hist-${entry.eventSeq}-${index}`}>
                    <span className={`artifact-badge ${badgeClass(entry.status)}`}>{statusLabel(entry.status, t)}</span>
                    <span>{[entry.actor, clock ? clock(entry.ts) : ""].filter(Boolean).join(" · ")}</span>
                    {entry.reason && <span className="report-history-reason">{entry.reason}</span>}
                  </li>
                ))}
              </ol>
            </>
          )}
          {relatedPocs.length > 0 && (
            <>
              <div className="artifact-row-body"><b>{t("runtime.reports.relatedPoc")}</b></div>
              <div className="report-links">
                {relatedPocs.map((poc) => (
                  <button type="button" key={poc.id} className="report-link-btn" onClick={() => onOpenPoc?.(poc.id)}>
                    {poc.name || poc.id}
                  </button>
                ))}
              </div>
            </>
          )}
          {relatedCreds.length > 0 && (
            <>
              <div className="artifact-row-body"><b>{t("runtime.reports.relatedCred")}</b></div>
              <div className="report-links">
                {relatedCreds.map((cred) => (
                  <span key={`${cred.entity}-${cred.seq ?? cred.value}`} className="artifact-chip">
                    {cred.entity}{cred.seq ? ` #${cred.seq}` : ""}
                  </span>
                ))}
              </div>
            </>
          )}
          {relatedReviews.length > 0 && (
            <>
              <div className="artifact-row-body"><b>{t("runtime.reports.relatedReview")}</b></div>
              {relatedReviews.map((review) => (
                <div className="artifact-row-body" key={review.id}>{review.summary}</div>
              ))}
            </>
          )}
          <details className="evi-raw-d">
            <summary className="evi-raw-more">{t("runtime.reports.markdownSource")}</summary>
            <pre className="artifact-code">{markdown}</pre>
          </details>
          <div className="artifact-row-meta">
            {[row.submitter || row.actor, clock ? clock(row.ts) : ""].filter(Boolean).join(" · ")}
          </div>
        </>
      )}
    </article>
  );
}

export function VulnReportsList({
  rows,
  empty,
  clock,
  collectionTitle,
  focusReport,
  facts = [],
  pocs = [],
  reviews = [],
  credentials = [],
  truncated = false,
  onOpenFact,
  onOpenPoc,
}: {
  rows: BlackboardVulnReport[];
  empty: ReactNode;
  clock?: (ts: number) => string;
  collectionTitle?: string;
  focusReport?: { id: string; nonce: number } | null;
  facts?: BlackboardFact[];
  pocs?: BlackboardPoc[];
  reviews?: BlackboardReviewFinding[];
  credentials?: ReportCredential[];
  truncated?: boolean;
  onOpenFact?: (factSeq: number) => void;
  onOpenPoc?: (pocId: string) => void;
}) {
  const t = useT();
  const [filter, setFilter] = useState<ReportStatusFilter>("all");
  if (!rows.length) return empty;
  const accepted = rows.filter((row) => row.status === "accepted");
  const pending = rows.filter((row) => row.status === "submitted" || row.status === "reproduced");
  const reproFailed = rows.filter((row) => row.status === "repro_failed");
  const rejected = rows.filter((row) => row.status === "rejected");
  const collection = reportsToCollectionMarkdown(accepted, collectionTitle);
  const visible = filter === "all"
    ? rows
    : filter === "submitted"
      ? pending
      : rows.filter((row) => row.status === filter);
  const chips: { key: ReportStatusFilter; label: string; n: number }[] = [
    { key: "all", label: t("runtime.reports.filterAll"), n: rows.length },
    { key: "accepted", label: t("runtime.reports.accepted"), n: accepted.length },
    { key: "submitted", label: t("runtime.reports.submitted"), n: pending.length },
    { key: "repro_failed", label: t("runtime.reports.reproFailed"), n: reproFailed.length },
    { key: "rejected", label: t("runtime.reports.rejected"), n: rejected.length },
  ];
  return (
    <div className="panel-scroll-wrap report-panel">
      <div className="evi-toolbar">
        <div className="evi-toolbar-title">
          {truncated && <div className="evi-density-note">{t("runtime.truncated", { n: 80 })}</div>}
          <div className="evi-filter" role="tablist" aria-label={t("runtime.reports.filterAll")}>
            {chips.filter((chip) => chip.key === "all" || chip.n > 0).map((chip) => (
              <button
                key={chip.key}
                type="button"
                role="tab"
                aria-selected={filter === chip.key}
                className={`evi-filter-btn ${filter === chip.key ? "on" : ""}`.trim()}
                onClick={() => setFilter(chip.key)}
              >
                <span>{chip.label}</span>
                <b>{chip.n}</b>
              </button>
            ))}
          </div>
          {accepted.length > 0 && (
            <CopyAction
              text={collection}
              label={t("runtime.reports.copyCollection")}
              titleKey="runtime.reports.copyCollection"
              ariaKey="runtime.reports.copyCollectionAria"
              className="evi-filter-btn"
            />
          )}
        </div>
      </div>
      <div className="artifact-list">
        {[...visible].reverse().map((row) => {
          const intents = new Set(reportIntentIds(row));
          const relatedPocs = pocs.filter((poc) => poc.intentId && intents.has(poc.intentId));
          const relatedFacts = facts.filter((fact) => fact.intentId && intents.has(fact.intentId));
          const factSeqs = new Set(relatedFacts.map((fact) => fact.factSeq).filter((seq): seq is number => typeof seq === "number"));
          const relatedCreds = credentials.filter((cred) => typeof cred.seq === "number" && factSeqs.has(cred.seq));
          const relatedReviews = reviews.filter((review) => (review.intentIds ?? []).some((id) => intents.has(id)));
          return (
            <VulnReportDoc
              key={row.id}
              row={row}
              clock={clock}
              focused={focusReport?.id === row.id}
              focusNonce={focusReport?.nonce}
              relatedFacts={relatedFacts}
              relatedPocs={relatedPocs}
              relatedCreds={relatedCreds}
              relatedReviews={relatedReviews}
              onOpenFact={onOpenFact}
              onOpenPoc={onOpenPoc}
            />
          );
        })}
      </div>
    </div>
  );
}
