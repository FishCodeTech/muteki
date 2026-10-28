"use client";

import { Icon } from "@/components/Icon";
import { REF_KIND_LABELS, sameRef, type ResearchRef } from "@/lib/srcResearch";
import { RefChip } from "./SrcResearchParts";
import styles from "./srcResearch.module.css";

/** Composer accessory: what the next message is about, plus the object currently open in the work area. */
export function SrcResearchChatContext({ refs, viewing, note, onOpen, onAdd, onRemove }: {
  refs: ResearchRef[];
  viewing?: ResearchRef;
  note: string;
  onOpen: (ref: ResearchRef) => void;
  onAdd: (ref: ResearchRef) => void;
  onRemove: (ref: ResearchRef) => void;
}) {
  const suggest = viewing && !refs.some(ref => sameRef(ref, viewing)) ? viewing : undefined;
  return <div className={styles.chatContext} role="group" aria-label="对话上下文">
    <div className={styles.chatContextRow}>
      <span className={styles.chatContextLabel}><Icon name="at" size={12} />上下文</span>
      {refs.map(ref => <RefChip key={`${ref.kind}:${ref.id}`} refValue={ref.kind === "task" ? { ...ref, label: `研究 · ${ref.label}` } : ref} compact={ref.kind !== "task"} onOpen={onOpen} onRemove={ref.kind === "task" ? undefined : onRemove} />)}
      {suggest ? <button type="button" className={styles.chipSuggest} onClick={() => onAdd(suggest)} title={suggest.label}>
        <Icon name="plus" size={11} />正在查看的{REF_KIND_LABELS[suggest.kind]} {suggest.id}
      </button> : null}
    </div>
    <p className={styles.chatContextNote}>{note}</p>
  </div>;
}
