"use client";

import type { ReactNode } from "react";
import { Badge, Button, IconButton, type Tone } from "@/components/chat/ui";
import { Icon, type IconName } from "@/components/Icon";
import {
  FINDING_SEVERITY_LABELS, FINDING_STATE_LABELS, REF_KIND_LABELS,
  type FindingSeverity, type FindingState, type ResearchChangeOrigin, type ResearchOrigin, type ResearchRef,
} from "@/lib/srcResearch";
import styles from "./srcResearch.module.css";

export const REF_ICONS: Record<ResearchRef["kind"], IconName> = {
  task: "target", asset: "database", finding: "listChecks", evidence: "fileSearch", run: "activity", import: "upload", monitor: "clock",
};

const STATE_TONES: Record<FindingState, Tone> = { reviewed: "success", reviewing: "warning", needs_evidence: "danger", unreviewed: "neutral" };
const SEVERITY_TONES: Record<FindingSeverity, Tone> = { critical: "danger", high: "warning", medium: "accent", low: "neutral", unrated: "neutral" };

export function FindingStateBadge({ state }: { state: FindingState }) {
  return <Badge tone={STATE_TONES[state]} dot>{FINDING_STATE_LABELS[state]}</Badge>;
}

export function SeverityBadge({ severity }: { severity: FindingSeverity }) {
  return <Badge tone={SEVERITY_TONES[severity]}>{FINDING_SEVERITY_LABELS[severity]}</Badge>;
}

export function jobTone(state: string): Tone {
  if (["completed", "ready", "succeeded"].includes(state)) return "success";
  if (["running", "dispatched", "processing"].includes(state)) return "running";
  if (["failed", "cancelled", "outcome_unknown"].includes(state)) return "danger";
  if (["paused", "pending", "waiting_approval", "partial"].includes(state)) return "warning";
  return "neutral";
}

const JOB_STATE_LABELS: Record<string, string> = {
  completed: "已完成", running: "进行中", paused: "已暂停", pending: "排队中", failed: "失败", cancelled: "已取消", waiting_approval: "待审批", partial: "部分完成",
};
export const jobStateLabel = (state: string) => JOB_STATE_LABELS[state] || state;

const ORIGIN_LABELS: Record<ResearchChangeOrigin | ResearchOrigin, string> = { record: "记录", simulated: "本地模拟", chat: "对话", local: "本地操作" };
export function OriginTag({ origin }: { origin: ResearchChangeOrigin | ResearchOrigin }) {
  return <span className={styles.origin} data-origin={origin}>{ORIGIN_LABELS[origin]}</span>;
}

/** A pinned or linked object. The main button opens it; the optional trailing button removes it. */
export function RefChip({ refValue, onOpen, onRemove, dashed, compact }: {
  refValue: ResearchRef;
  onOpen?: (ref: ResearchRef) => void;
  onRemove?: (ref: ResearchRef) => void;
  dashed?: boolean;
  compact?: boolean;
}) {
  const label = compact ? refValue.id : refValue.label;
  return <span className={styles.chip} data-dashed={dashed ? "true" : undefined} title={`${REF_KIND_LABELS[refValue.kind]} · ${refValue.label}`}>
    {onOpen ? <button type="button" className={styles.chipMain} onClick={() => onOpen(refValue)} aria-label={`打开${REF_KIND_LABELS[refValue.kind]} ${refValue.label}`}>
      <Icon name={REF_ICONS[refValue.kind]} size={12} /><span>{label}</span>
    </button> : <span className={styles.chipMain}><Icon name={REF_ICONS[refValue.kind]} size={12} /><span>{label}</span></span>}
    {onRemove ? <button type="button" className={styles.chipRemove} onClick={() => onRemove(refValue)} aria-label={`从对话上下文移除 ${refValue.id}`}><Icon name="x" size={11} /></button> : null}
  </span>;
}

export function Pager({ page, pageSize, total, onChange, label }: { page: number; pageSize: number; total: number; onChange: (page: number) => void; label: string }) {
  const pages = Math.max(1, Math.ceil(total / pageSize));
  const from = total ? page * pageSize + 1 : 0;
  const to = Math.min(total, (page + 1) * pageSize);
  return <nav className={styles.pager} aria-label={label}>
    <span className="cx-tabular">{from}–{to} / {total.toLocaleString()}</span>
    <IconButton size="sm" icon="chevronLeft" label="上一页" disabled={page <= 0} onClick={() => onChange(page - 1)} />
    <span className="cx-tabular">{page + 1} / {pages}</span>
    <IconButton size="sm" icon="chevronRight" label="下一页" disabled={page + 1 >= pages} onClick={() => onChange(page + 1)} />
  </nav>;
}

export function PaneHeader({ eyebrow, title, actions, onBack, backLabel }: { eyebrow?: ReactNode; title: ReactNode; actions?: ReactNode; onBack?: () => void; backLabel?: string }) {
  return <header className={styles.paneHeader}>
    {onBack ? <Button size="sm" icon="arrowLeft" onClick={onBack}>{backLabel || "返回"}</Button> : null}
    <div className={styles.paneTitle}>{eyebrow ? <span>{eyebrow}</span> : null}<h2>{title}</h2></div>
    {actions ? <div className={styles.paneActions}>{actions}</div> : null}
  </header>;
}

export function Definitions({ rows }: { rows: Array<[string, ReactNode]> }) {
  return <dl className={styles.definitions}>{rows.map(([term, value]) => <div key={term}><dt>{term}</dt><dd>{value ?? "—"}</dd></div>)}</dl>;
}

export function formatTime(value?: string): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" });
}
