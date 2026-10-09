"use client";

/* ─────────────────────────────────────────────────────────
 * CONTEXT CARDS — Artifact / attachment / memory cards.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * ───────────────────────────────────────────────────────── */

import React from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";

export interface ContextChunk {
  id: string;
  title: string;
  body: string;
  chars?: string | number;
  source?: string;
  badge?: string;
  tone?: string;
  onClick?: () => void;
}

export interface ContextCardsProps {
  title?: string;
  chunks: ContextChunk[];
  totalCount?: number;
  className?: string;
}

export function ContextCards({ title = "上下文条目", chunks = [], totalCount, className = "" }: ContextCardsProps) {
  if (!chunks.length) return null;

  return (
    <section className={cn("flex w-full flex-col gap-2", className)} aria-label={title}>
      <div className="flex items-center gap-2 px-0.5">
        <span className="text-[13px] font-medium text-cx-fg-2">{title}</span>
        <span className="cx-tabular text-[12px] text-cx-fg-4">{totalCount ?? chunks.length}</span>
      </div>
      <div className="grid gap-2 sm:grid-cols-2">
        {chunks.map((chunk) => {
          const body = (
            <>
              <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-3">
                <Icon name="file" size={15} />
              </span>
              <span className="flex min-w-0 flex-1 flex-col">
                <span className="flex min-w-0 items-center gap-1.5">
                  <span className="min-w-0 truncate text-[13px] font-medium text-cx-fg">{chunk.title}</span>
                  {chunk.badge ? (
                    <span className="shrink-0 rounded-[5px] bg-cx-hover px-1 font-cx-mono text-[12px] font-medium uppercase text-cx-fg-3">
                      {chunk.badge}
                    </span>
                  ) : null}
                </span>
                <span className="min-w-0 truncate text-[12px] text-cx-fg-3">{chunk.body}</span>
              </span>
              <span className="flex shrink-0 flex-col items-end gap-0.5 text-[12px] text-cx-fg-4">
                {chunk.chars ? <span className="cx-tabular">{chunk.chars}</span> : null}
                {chunk.source ? <span className="font-cx-mono">{chunk.source}</span> : null}
              </span>
            </>
          );
          const cardClass = "group flex min-w-0 items-center gap-3 rounded-xl border border-cx-border bg-cx-elevated px-3 py-2.5 text-left";
          return chunk.onClick ? (
            <button
              key={chunk.id}
              type="button"
              onClick={chunk.onClick}
              data-testid="context-chunk-open"
              className={cn(cardClass, "cx-press hover:border-cx-border-strong hover:bg-cx-bg-subtle")}
            >
              {body}
            </button>
          ) : (
            <div key={chunk.id} className={cardClass}>{body}</div>
          );
        })}
      </div>
    </section>
  );
}
