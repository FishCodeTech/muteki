"use client";

import { MotionIcon } from "@/components/MotionIcon";

import { Button } from "@heroui/react";
import { memo, useContext, useId, useMemo } from "react";

import { Icon } from "@/components/Icon";
import type { CollaborationKnowledgeItem } from "@/lib/agentCollaboration";
import type { BlackboardIntent } from "@/lib/events";
import type { RunCanvasMode } from "@/lib/swarmProjection";
import { formatClock } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { knowledgeStatusLabel } from "@/lib/statusLabels";
import { highlight, HighlightQueryContext } from "@/lib/textHighlight";
import { useCopied } from "@/lib/useCopied";

import { EventStamp } from "./EventStamp";
import { knowledgeIcon, knowledgeLabel, type Translate } from "./collaborationPresentation";

/** Verifier line for a fact row: the resolved worker, or the raw verifier marked as unresolved. */
export function knowledgeVerifierLabel(
  item: CollaborationKnowledgeItem,
  actorName: (id?: string) => string,
  t: Translate,
): string {
  if (item.verifierAgentId) return actorName(item.verifierAgentId);
  if (item.verifier) return t("collab.verifierUnresolved", { verifier: item.verifier });
  return "";
}

/** Proposer line for an intent row when it differs from the owning agent. */
export function knowledgeProposerLabel(
  item: CollaborationKnowledgeItem,
  actorName: (id?: string) => string,
): string {
  if (!item.proposerId || item.proposerId === item.agentId) return "";
  return actorName(item.proposerId);
}

/** Catalog lookup used by intent / fact jump links (`intent:${id}`, `fact:${seq}`). */
export function findKnowledge(
  knowledge: CollaborationKnowledgeItem[],
  id: string,
): CollaborationKnowledgeItem | undefined {
  return knowledge.find((row) => row.id === id);
}

/** Rows that share this item's intentId or factSeq, plus intents that list the fact in fromFacts. */
export function citingKnowledge(
  item: CollaborationKnowledgeItem,
  knowledge: CollaborationKnowledgeItem[],
  intents: BlackboardIntent[],
): CollaborationKnowledgeItem[] {
  const cited = new Map<string, CollaborationKnowledgeItem>();
  for (const row of knowledge) {
    if (row.id === item.id) continue;
    if (item.intentId && row.intentId === item.intentId) cited.set(row.id, row);
    if (typeof item.factSeq === "number" && row.factSeq === item.factSeq) cited.set(row.id, row);
  }
  if (typeof item.factSeq === "number") {
    for (const intent of intents) {
      if (!intent.fromFacts.includes(item.factSeq)) continue;
      const row = findKnowledge(knowledge, `intent:${intent.id}`);
      if (row && row.id !== item.id) cited.set(row.id, row);
    }
  }
  return [...cited.values()].sort((a, b) => b.ts - a.ts || a.id.localeCompare(b.id));
}

/** Text when the catalog has no matching row; a button when it does. */
export function KnowledgeLink({
  target,
  label,
  onSelect,
}: {
  target?: CollaborationKnowledgeItem;
  label: string;
  onSelect?: (item: CollaborationKnowledgeItem) => void;
}) {
  if (!target || !onSelect) return label;
  return (
    <button type="button" className="collab-knowledge-link" onClick={() => onSelect(target)}>
      {label}
    </button>
  );
}

/** `#seq` chips for an intent's fromFacts; unresolved seqs stay plain text. */
export function FactSeqLinks({
  seqs,
  knowledge,
  onSelect,
}: {
  seqs: number[];
  knowledge: CollaborationKnowledgeItem[];
  onSelect: (item: CollaborationKnowledgeItem) => void;
}) {
  return (
    <>
      {seqs.map((seq, index) => (
        <span key={seq}>
          {index > 0 && ", "}
          <KnowledgeLink target={findKnowledge(knowledge, `fact:${seq}`)} label={`#${seq}`} onSelect={onSelect} />
        </span>
      ))}
    </>
  );
}

type KnowledgeRowProps = {
  item: CollaborationKnowledgeItem;
  actor: string;
  selected: boolean;
  fresh?: boolean;
  onSelect: (itemId: string) => void;
  kindLabel?: string;
  mode?: RunCanvasMode;
};

function knowledgeRowEqual(prev: KnowledgeRowProps, next: KnowledgeRowProps): boolean {
  return prev.item.id === next.item.id
    && prev.item.title === next.item.title
    && prev.item.kind === next.item.kind
    && prev.item.status === next.item.status
    && prev.item.statusKey === next.item.statusKey
    && prev.item.tone === next.item.tone
    && prev.item.ts === next.item.ts
    && prev.actor === next.actor
    && prev.selected === next.selected
    && prev.fresh === next.fresh
    && prev.onSelect === next.onSelect
    && prev.kindLabel === next.kindLabel
    && prev.mode === next.mode;
}

export const KnowledgeRow = memo(function KnowledgeRow({ item, actor, selected, fresh, onSelect, kindLabel, mode }: KnowledgeRowProps) {
  const t = useT();
  const q = useContext(HighlightQueryContext);
  const [copied, copy] = useCopied();
  const kind = kindLabel ?? knowledgeLabel(item.kind, t, mode);
  const statusText = item.status ? knowledgeStatusLabel(item, t) : "";
  const clock = formatClock(item.ts, "—");
  const describedBy = useId();
  const statusId = `${describedBy}-status`;
  const timeId = `${describedBy}-time`;
  const row = (
    <button
      type="button"
      data-knowledge-id={item.id}
      className={`collab-knowledge-row tone-${item.tone} ${selected ? "selected" : ""} ${fresh ? "fresh" : ""}`}
      aria-current={selected ? "true" : undefined}
      aria-label={t("collab.a11y.knowledgeRow", { kind, title: item.title })}
      aria-describedby={statusText ? `${statusId} ${timeId}` : timeId}
      onClick={() => onSelect(item.id)}
    >
      <span className="collab-knowledge-icon"><Icon name={knowledgeIcon(item.kind)} size={13} /></span>
      <span className="collab-knowledge-copy">
        <span>
          <b>{kind}</b>
          {statusText && <small id={statusId} aria-label={t("collab.a11y.knowledgeStatus", { status: statusText })}>{statusText}</small>}
        </span>
        <strong>{highlight(item.title, q)}</strong>
        <small id={timeId} aria-label={t("collab.a11y.knowledgeTime", { time: clock })}>
          {actor || t("collab.unassigned")} · <EventStamp ts={item.ts} />
        </small>
      </span>
    </button>
  );
  if (item.kind !== "flag") return row;
  return (
    <div className="collab-knowledge-item">
      {row}
      <button
        type="button"
        className={`collab-knowledge-flag-copy${copied ? " copied" : ""}`}
        aria-label={t("common.copyFlagAria", { flag: item.title })}
        title={t("common.copyFlag")}
        onClick={() => copy(item.title)}
      >
        <MotionIcon active={copied} from="copy" to="check" size={12} />
      </button>
    </div>
  );
}, knowledgeRowEqual);

export function KnowledgeDetail({
  item, actor, proposer, verifier, knowledge = [], intents = [],
  onSelectKnowledge, onOpenFact, onOpenPoc, onOpenAgentKnowledge, onClose,
  mode,
}: {
  item: CollaborationKnowledgeItem;
  actor: string;
  proposer?: string;
  verifier?: string;
  knowledge?: CollaborationKnowledgeItem[];
  intents?: BlackboardIntent[];
  onSelectKnowledge?: (item: CollaborationKnowledgeItem) => void;
  onOpenFact?: (seq: number) => void;
  onOpenPoc?: (id: string) => void;
  onOpenAgentKnowledge?: () => void;
  onClose?: () => void;
  mode?: RunCanvasMode;
}) {
  const t = useT();
  const [copied, copy] = useCopied();
  const pocId = item.kind === "poc" ? item.id.slice(4) : "";
  const factSeq = item.factSeq;
  const copyText = item.kind === "flag" ? item.title : (item.detail || item.title);
  const copyLabel = item.kind === "flag" ? t("common.copyFlag") : t("common.copyShort");
  const intentTarget = item.intentId ? findKnowledge(knowledge, `intent:${item.intentId}`) : undefined;
  const factTarget = typeof factSeq === "number" ? findKnowledge(knowledge, `fact:${factSeq}`) : undefined;
  const cited = useMemo(() => citingKnowledge(item, knowledge, intents), [item, knowledge, intents]);
  return (
    <div className={`collab-selected-knowledge tone-${item.tone}`}>
      <div className="collab-selected-knowledge-head">
        <span><Icon name={knowledgeIcon(item.kind)} size={14} />{knowledgeLabel(item.kind, t, mode)}</span>
        <div className="collab-selected-knowledge-tools">
          {item.status && <b>{knowledgeStatusLabel(item, t)}</b>}
          {onClose && (
            <Button
              size="sm"
              variant="ghost"
              isIconOnly
              className="collab-selected-knowledge-close"
              aria-label={t("collab.action.closeKnowledge")}
              onClick={onClose}
            >
              <Icon name="x" size={12} />
            </Button>
          )}
        </div>
      </div>
      <div className="collab-selected-knowledge-content">
        <strong>{item.title}</strong>
        {item.detail && item.detail !== item.title && <p>{item.detail}</p>}
      </div>
      <dl>
        <dt>{t("collab.field.agent")}</dt><dd>{actor || t("collab.unassigned")}</dd>
        {proposer && <><dt>{t("collab.field.proposer")}</dt><dd>{proposer}</dd></>}
        {item.intentId && (
          <>
            <dt>{t("collab.field.intent")}</dt>
            <dd>
              <KnowledgeLink
                target={intentTarget && intentTarget.id !== item.id ? intentTarget : undefined}
                label={item.intentId}
                onSelect={onSelectKnowledge}
              />
            </dd>
          </>
        )}
        {typeof factSeq === "number" && (
          <>
            <dt>{t("collab.field.fact")}</dt>
            <dd>
              <KnowledgeLink
                target={factTarget && factTarget.id !== item.id ? factTarget : undefined}
                label={`#${factSeq}`}
                onSelect={onSelectKnowledge}
              />
            </dd>
          </>
        )}
        {verifier && <><dt>{t("collab.field.verifier")}</dt><dd>{verifier}</dd></>}
        {typeof item.confidence === "number" && <><dt>{t("collab.field.confidence")}</dt><dd>{item.confidence.toFixed(2)}</dd></>}
        {item.witness && <><dt>{t("collab.field.witness")}</dt><dd>{item.witness}</dd></>}
      </dl>
      <div className="collab-detail-actions">
        <Button size="sm" variant="ghost" className={copied ? "copied" : ""} onClick={() => copy(copyText)}>
          <MotionIcon active={copied} from="copy" to="check" size={12} />{copyLabel}
        </Button>
        {typeof factSeq === "number" && onOpenFact && (
          <Button size="sm" variant="ghost" onClick={() => onOpenFact(factSeq)}>
            <Icon name="layers" size={12} />{t("collab.action.openEvidence")}
          </Button>
        )}
        {pocId && onOpenPoc && (
          <Button size="sm" variant="ghost" onClick={() => onOpenPoc(pocId)}>
            <Icon name="terminal" size={12} />{t("collab.action.openPoc")}
          </Button>
        )}
        {onOpenAgentKnowledge && (
          <Button size="sm" variant="ghost" onClick={onOpenAgentKnowledge}>
            <Icon name="arrowRight" size={12} />{t("collab.action.openAgentKnowledge")}
          </Button>
        )}
      </div>
      {cited.length > 0 && onSelectKnowledge && (
        <div className="collab-cited">
          <h4>{t("collab.citedBy")}</h4>
          <ul className="collab-cited-list">
            {cited.map((row) => (
              <li key={row.id}>
                <KnowledgeLink
                  target={row}
                  label={`${knowledgeLabel(row.kind, t, mode)} · ${row.title}`}
                  onSelect={onSelectKnowledge}
                />
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
