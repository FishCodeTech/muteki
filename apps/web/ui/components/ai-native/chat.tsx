"use client";

/* ─────────────────────────────────────────────────────────
 * CHAT — Scroll container + centered reading column + composer slot.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * The reading width honors the C39 `--dsh-chat-content-width` reading pref.
 * ───────────────────────────────────────────────────────── */

import React from "react";
import { cn } from "@/lib/cn";

export interface ChatMessageItem {
  id: string;
  role: "user" | "assistant" | "system";
  text: string;
  createdAt?: string;
  attachments?: Array<{ name: string; size?: number }>;
}

export interface ChatProps {
  children: React.ReactNode;
  header?: React.ReactNode;
  footer?: React.ReactNode;
  streamOverlay?: React.ReactNode;
  /** Absolutely positioned against the stream viewport (e.g. a minimap rail). */
  streamAside?: React.ReactNode;
  streamState?: React.ReactNode;
  streamBusy?: boolean;
  streamRef?: React.Ref<HTMLDivElement>;
  onStreamScroll?: React.UIEventHandler<HTMLDivElement>;
  emptyState?: React.ReactNode;
  className?: string;
}

export function Chat({
  children,
  header,
  footer,
  streamOverlay,
  streamAside,
  streamState,
  streamBusy = false,
  streamRef,
  onStreamScroll,
  emptyState,
  className = "",
}: ChatProps) {
  return (
    <div className={cn("cx-chat relative flex min-h-0 w-full flex-1 flex-col overflow-hidden bg-cx-bg", className)}>
      {header ? <div className="shrink-0">{header}</div> : null}

      <div className="relative min-h-0 flex-1" aria-busy={streamBusy}>
        <div
          ref={streamRef}
          onScroll={onStreamScroll}
          className="ai-chat-stream cx-chat-stream cx-scroll absolute inset-0 overflow-y-auto overflow-x-hidden"
          data-testid="cx-chat-stream"
        >
          <div className="cx-timeline mx-auto flex w-full max-w-[var(--dsh-chat-content-width,var(--cx-content-w))] flex-col px-5 pb-8 pt-8 sm:px-6">
            {emptyState ? emptyState : children}
          </div>
        </div>
        {streamAside}
        {streamOverlay ? (
          <div className="pointer-events-none absolute inset-x-0 bottom-4 z-10 flex justify-center px-5 sm:px-6">
            {streamOverlay}
          </div>
        ) : null}
        {streamState ? (
          <div className="cx-animate-in absolute inset-0 z-20 grid place-items-center bg-cx-bg">
            {streamState}
          </div>
        ) : null}
      </div>

      {footer ? (
        <div className="cx-chat-footer cx-composer-dock relative shrink-0 px-4 pb-3 sm:px-6">
          <div className="mx-auto w-full max-w-[var(--dsh-composer-card-max-width,calc(var(--cx-content-w)+32px))]">{footer}</div>
        </div>
      ) : null}
    </div>
  );
}
