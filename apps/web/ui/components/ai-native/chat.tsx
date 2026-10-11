"use client";

/* ─────────────────────────────────────────────────────────
 * CHAT — Scroll container + centered reading column + composer slot.
 *
 * Originally adapted from https://github.com/TurboKach/ai-native-react-components
 * (MIT, pinned 05dab2d2b5f1f3e40029776e339a486d70491079).
 * The reading width honors the C39 `--dsh-chat-content-width` reading pref.
 * ───────────────────────────────────────────────────────── */

import React, { useEffect, useRef } from "react";
import { cn } from "@/lib/cn";

/** Marks a side panel whose CSS width transition squeezes the chat stream. */
export const REFLOW_PANEL_ATTR = "data-cx-reflow-panel";

function hasRunningWidthTransition(element: HTMLElement) {
  return element.getAnimations().some((animation) =>
    animation.playState === "running"
    && (animation as CSSTransition).transitionProperty === "width");
}

/**
 * A side panel animating its width resizes the stream on every frame. Pinning
 * the reading column to the narrower of its start and end widths keeps the
 * transcript from re-wrapping (and every virtual row from re-measuring) each
 * frame; only the centering offset moves until the transition settles.
 */
function usePinColumnDuringPanelTransition(columnRef: React.RefObject<HTMLDivElement | null>) {
  useEffect(() => {
    const column = columnRef.current;
    const stream = column?.parentElement;
    if (!column || !stream) return;
    const panels = new Set<HTMLElement>();
    let fallback = 0;
    const release = () => {
      for (const panel of panels) {
        if (panel.isConnected && hasRunningWidthTransition(panel)) return;
      }
      panels.clear();
      window.clearTimeout(fallback);
      column.style.width = "";
    };
    const panelOf = (event: TransitionEvent) => {
      if (event.propertyName !== "width") return null;
      const target = event.target;
      return target instanceof HTMLElement && target.hasAttribute(REFLOW_PANEL_ATTR) ? target : null;
    };
    const onRun = (event: TransitionEvent) => {
      const panel = panelOf(event);
      if (!panel) return;
      const targetWidth = Number.parseFloat(panel.style.width);
      if (!Number.isFinite(targetWidth)) return;
      const growth = targetWidth - panel.getBoundingClientRect().width;
      const current = column.getBoundingClientRect().width;
      const pinned = Math.max(0, Math.min(current, stream.clientWidth - growth));
      panels.add(panel);
      column.style.width = `${Math.round(pinned)}px`;
      window.clearTimeout(fallback);
      fallback = window.setTimeout(() => {
        panels.clear();
        column.style.width = "";
      }, 1000);
    };
    const onSettle = (event: TransitionEvent) => {
      if (panelOf(event)) release();
    };
    document.addEventListener("transitionrun", onRun);
    document.addEventListener("transitionend", onSettle);
    document.addEventListener("transitioncancel", onSettle);
    return () => {
      document.removeEventListener("transitionrun", onRun);
      document.removeEventListener("transitionend", onSettle);
      document.removeEventListener("transitioncancel", onSettle);
      window.clearTimeout(fallback);
      column.style.width = "";
    };
  }, [columnRef]);
}

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
  const columnRef = useRef<HTMLDivElement>(null);
  usePinColumnDuringPanelTransition(columnRef);
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
          <div ref={columnRef} className="cx-timeline mx-auto flex w-full max-w-[var(--dsh-chat-content-width,var(--cx-content-w))] flex-col px-5 pb-8 pt-8 sm:px-6">
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
