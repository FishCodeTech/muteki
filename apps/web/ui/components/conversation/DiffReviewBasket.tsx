"use client";

/**
 * C14 — Review basket: pending diff annotations with edit / delete / send-all.
 * No external PR comment is created; send pushes structured refs into the composer.
 */

import React, { useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Button, Collapse, IconButton } from "@/components/chat/ui";
import type { DiffLineAnnotation } from "@/lib/conversationDiff";
import { annotationRangeLabel } from "@/components/chat/diff/DiffRows";

export interface DiffReviewBasketProps {
  annotations: DiffLineAnnotation[];
  onUpdate: (id: string, comment: string) => void;
  onDelete: (id: string) => void;
  onSendAll: () => void;
}

function AnnotationRow({
  annotation,
  onUpdate,
  onDelete,
}: {
  annotation: DiffLineAnnotation;
  onUpdate: (id: string, comment: string) => void;
  onDelete: (id: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(annotation.comment);
  const sideLabel = annotation.side === "old" ? "旧" : annotation.side === "new" ? "新" : "统";

  const commitEdit = () => {
    const trimmed = draft.trim();
    if (trimmed) onUpdate(annotation.id, trimmed);
    else setDraft(annotation.comment);
    setEditing(false);
  };

  return (
    <div
      className={cn("group flex items-start gap-2.5 rounded-lg px-2 py-1.5 hover:bg-cx-hover", annotation.stale && "opacity-70")}
      data-testid="diff-basket-row"
      data-stale={annotation.stale ? "true" : "false"}
    >
      <Icon name="messageCircle" size={13} className="mt-[3px] shrink-0 text-cx-accent" />
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 items-center gap-1.5 text-[11.5px] text-cx-fg-4">
          <code className="min-w-0 truncate font-cx-mono text-cx-fg-3">{annotation.path}</code>
          <span className="shrink-0">[{sideLabel}] {annotationRangeLabel(annotation)}</span>
          {annotation.stale ? (
            <span className="shrink-0 rounded bg-cx-warning-soft px-1 text-cx-warning" title="基线已变更，请重新标注后再发送">已过期</span>
          ) : null}
        </div>
        {editing ? (
          <div className="mt-1 flex flex-col gap-1.5">
            <textarea
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              rows={2}
              autoFocus
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) commitEdit();
                if (e.key === "Escape") { e.stopPropagation(); setDraft(annotation.comment); setEditing(false); }
              }}
              className="w-full resize-none rounded-lg border border-cx-border-strong bg-cx-elevated px-2 py-1.5 text-[12.5px] leading-5 text-cx-fg outline-none"
            />
            <div className="flex justify-end gap-1.5">
              <Button size="xs" variant="ghost" onClick={() => { setDraft(annotation.comment); setEditing(false); }}>取消</Button>
              <Button size="xs" variant="primary" onClick={commitEdit} disabled={!draft.trim()}>保存</Button>
            </div>
          </div>
        ) : (
          <p className="mt-0.5 cursor-text whitespace-pre-wrap break-words text-[12.5px] leading-5 text-cx-fg" onClick={() => setEditing(true)}>
            {annotation.comment}
          </p>
        )}
      </div>
      {!editing ? (
        <div className="flex shrink-0 items-center opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100">
          <IconButton size="xs" icon="pencil" label="编辑评论" onClick={() => setEditing(true)} />
          <IconButton size="xs" icon="trash" label="删除评论" className="hover:text-cx-danger" onClick={() => onDelete(annotation.id)} />
        </div>
      ) : null}
    </div>
  );
}

export function DiffReviewBasket({ annotations, onUpdate, onDelete, onSendAll }: DiffReviewBasketProps) {
  const [open, setOpen] = useState(true);
  const hasStale = annotations.some((a) => a.stale);
  const canSend = annotations.length > 0 && !hasStale;

  return (
    <div className="shrink-0 border-t border-cx-border bg-cx-elevated shadow-[0_-8px_20px_-16px_hsl(var(--cx-shadow-color)/0.4)]" data-testid="diff-review-basket">
      <div className="flex h-11 items-center gap-2 px-3">
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen((value) => !value)}
          className="cx-press -ml-1.5 flex min-w-0 items-center gap-1.5 rounded-lg px-1.5 py-1 text-[13px] font-medium text-cx-fg hover:bg-cx-hover"
        >
          <Icon name="chevronRight" size={13} className={cn("text-cx-fg-4 transition-transform duration-150", open && "rotate-90")} />
          评审意见
          <span className="cx-tabular rounded-full bg-cx-accent-soft px-1.5 text-[11.5px] text-cx-accent">{annotations.length}</span>
        </button>
        {hasStale ? <span className="min-w-0 truncate text-[12px] text-cx-warning">含已过期评论，请删除后再发送</span> : null}
        <Button
          size="sm"
          variant="primary"
          icon="cornerDownLeft"
          className="ml-auto"
          onClick={onSendAll}
          disabled={!canSend}
          data-testid="diff-basket-send"
        >
          发送到输入框
        </Button>
      </div>
      <Collapse open={open}>
        <div className="cx-scroll flex max-h-56 flex-col overflow-y-auto px-2 pb-2">
          {annotations.map((annotation) => (
            <AnnotationRow key={annotation.id} annotation={annotation} onUpdate={onUpdate} onDelete={onDelete} />
          ))}
        </div>
      </Collapse>
    </div>
  );
}
