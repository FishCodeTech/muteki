"use client";

/**
 * C14 — Anchored comment card for a diff line or line range.
 * Submits a DiffLineAnnotation to the review basket.
 */

import React, { useMemo, useState, type RefObject } from "react";
import { Button, IconButton, Popover, Shortcut, TextArea } from "@/components/chat/ui";
import type { DiffLineAnnotation, DiffStaging } from "@/lib/conversationDiff";
import { annotationRangeLabel, sideLabel } from "@/components/chat/diff/DiffRows";

export interface DiffLineCommentPopoverProps {
  path: string;
  oldPath?: string;
  side: "old" | "new" | "unified";
  lineNumber: number;
  endLineNumber?: number;
  snapshot: string;
  staging?: DiffStaging;
  baselineId: string;
  /** Element the card is anchored to (legacy); prefer `anchorRef`. */
  anchorEl?: HTMLElement | null;
  anchorRef?: RefObject<HTMLElement | null>;
  onAdd: (annotation: Omit<DiffLineAnnotation, "id" | "createdAt" | "stale">) => void;
  onDismiss: () => void;
}

export function DiffLineCommentPopover({
  path,
  oldPath,
  side,
  lineNumber,
  endLineNumber,
  snapshot,
  staging,
  baselineId,
  anchorEl,
  anchorRef,
  onAdd,
  onDismiss,
}: DiffLineCommentPopoverProps) {
  const [comment, setComment] = useState("");
  const fallbackRef = useMemo<RefObject<HTMLElement | null>>(() => ({ current: anchorEl ?? null }), [anchorEl]);
  const ref = anchorRef ?? fallbackRef;
  const range = endLineNumber && endLineNumber > lineNumber ? endLineNumber : undefined;
  const snapshotLines = useMemo(() => snapshot.split("\n"), [snapshot]);

  const submit = () => {
    const trimmed = comment.trim();
    if (!trimmed) return;
    onAdd({ path, oldPath, side, lineNumber, endLineNumber: range, snapshot, staging, baselineId, comment: trimmed });
    setComment("");
    onDismiss();
  };

  return (
    <Popover
      open
      onOpenChange={(open) => { if (!open) onDismiss(); }}
      anchorRef={ref}
      placement="bottom-start"
      offset={4}
      ariaLabel="添加评审意见"
      className="w-[min(420px,calc(100vw-24px))] p-0"
    >
      <div data-testid="diff-comment-popover" className="flex flex-col">
        <div className="flex items-center gap-2 border-b border-cx-border-subtle py-1.5 pl-3 pr-1.5">
          <div className="flex min-w-0 flex-1 items-baseline gap-2">
            <code className="min-w-0 truncate font-cx-mono text-[12px] text-cx-fg">{path}</code>
            <span className="shrink-0 text-[12px] text-cx-fg-4">
              {sideLabel(side)} · {annotationRangeLabel({ lineNumber, endLineNumber: range })}
            </span>
          </div>
          <IconButton size="xs" icon="x" label="关闭" data-testid="diff-comment-popover-close" onClick={onDismiss} />
        </div>
        {snapshot ? (
          <pre
            className="cx-scroll m-0 max-h-[92px] overflow-auto border-b border-cx-border-subtle bg-cx-code px-3 py-2 font-cx-mono text-[12px] leading-[18px] text-cx-fg-2"
            data-testid="diff-comment-popover-snapshot"
          >
            {snapshotLines.slice(0, 12).join("\n")}
            {snapshotLines.length > 12 ? `\n… 另有 ${snapshotLines.length - 12} 行` : ""}
          </pre>
        ) : null}
        <div className="p-2.5">
          <TextArea
            data-autofocus
            autoResize
            maxRows={8}
            data-testid="diff-comment-popover-input"
            value={comment}
            onChange={(event) => setComment(event.target.value)}
            placeholder="描述你的问题或修改建议…"
            className="min-h-[64px] text-[13px]"
            onKeyDown={(event) => {
              if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
                event.preventDefault();
                submit();
              }
            }}
          />
          <div className="mt-2 flex items-center gap-2">
            <span className="flex items-center gap-1 text-[12px] text-cx-fg-4">
              <Shortcut keys="mod+enter" tone="subtle" />
              添加
            </span>
            <span className="flex-1" />
            <Button size="sm" variant="ghost" onClick={onDismiss}>取消</Button>
            <Button
              size="sm"
              variant="primary"
              onClick={submit}
              disabled={!comment.trim()}
              data-testid="diff-comment-popover-submit"
            >
              添加评审
            </Button>
          </div>
        </div>
      </div>
    </Popover>
  );
}
