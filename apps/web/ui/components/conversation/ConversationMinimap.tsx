"use client";

/**
 * Transcript minimap — a thin rail on the stream's right edge.
 *
 * One tick per user prompt, placed by the virtualizer's (measured or
 * estimated) row offset, plus a viewport window. Hover previews the prompt,
 * click jumps to the turn, press-and-drag on the rail scrubs the stream.
 * Keyboard turn navigation stays on Alt+↑/↓, so ticks are not tab stops.
 */

import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { streamCornerTop, useThreadDetailsCardHeight } from "@/lib/threadDetailsOverlayStore";

export type MinimapMarker = {
  key: string;
  messageId: string;
  /** Row offset inside the virtualized list, in px. */
  start: number;
  seq?: number;
  label: string;
  tone: "default" | "running" | "danger" | "muted";
};

type StreamMetrics = {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
  listTop: number;
  /** Free space between the reading column and the stream's right edge. */
  rightGap: number;
};

const MIN_MARKERS = 3;
const MIN_RIGHT_GAP = 44;
// Leaves room for the summary pill pinned to the stream's top-right corner.
const TRACK_INSET_TOP = 48;
const TRACK_INSET_BOTTOM = 64;

function readMetrics(stream: HTMLElement, list: HTMLElement | null): StreamMetrics {
  const streamRect = stream.getBoundingClientRect();
  const listRight = list ? list.getBoundingClientRect().right : streamRect.right;
  return {
    scrollTop: stream.scrollTop,
    scrollHeight: stream.scrollHeight,
    clientHeight: stream.clientHeight,
    listTop: list ? list.offsetTop : 0,
    rightGap: Math.round(streamRect.left + stream.clientWidth - listRight),
  };
}

function sameMetrics(a: StreamMetrics | null, b: StreamMetrics) {
  return Boolean(a)
    && a!.scrollTop === b.scrollTop
    && a!.scrollHeight === b.scrollHeight
    && a!.clientHeight === b.clientHeight
    && a!.listTop === b.listTop
    && a!.rightGap === b.rightGap;
}

export function ConversationMinimap({
  streamRef,
  listRef,
  markers,
  onJump,
}: {
  streamRef: React.RefObject<HTMLDivElement | null>;
  listRef: React.RefObject<HTMLDivElement | null>;
  markers: MinimapMarker[];
  onJump: (messageId: string) => void;
}) {
  const [metrics, setMetrics] = useState<StreamMetrics | null>(null);
  const [hovered, setHovered] = useState<string>("");
  const trackRef = useRef<HTMLDivElement>(null);
  const scrubRef = useRef<{ pointerId: number; moved: boolean } | null>(null);
  const frameRef = useRef(0);
  const detailsHeight = useThreadDetailsCardHeight();

  const measure = useCallback(() => {
    cancelAnimationFrame(frameRef.current);
    frameRef.current = requestAnimationFrame(() => {
      const stream = streamRef.current;
      if (!stream) return;
      const next = readMetrics(stream, listRef.current);
      setMetrics((prev) => (sameMetrics(prev, next) ? prev : next));
    });
  }, [listRef, streamRef]);

  useEffect(() => {
    const stream = streamRef.current;
    if (!stream) return;
    measure();
    stream.addEventListener("scroll", measure, { passive: true });
    const observer = new ResizeObserver(measure);
    observer.observe(stream);
    if (listRef.current) observer.observe(listRef.current);
    return () => {
      cancelAnimationFrame(frameRef.current);
      stream.removeEventListener("scroll", measure);
      observer.disconnect();
    };
  }, [listRef, measure, streamRef]);

  // Row measurements change without a scroll or resize on the stream itself.
  useLayoutEffect(() => {
    measure();
  }, [markers, measure]);

  if (!metrics || markers.length < MIN_MARKERS) return null;
  if (metrics.rightGap < MIN_RIGHT_GAP) return null;
  if (metrics.scrollHeight <= metrics.clientHeight * 1.5) return null;

  const trackTop = streamCornerTop(TRACK_INSET_TOP, detailsHeight);
  const trackHeight = Math.max(0, metrics.clientHeight - trackTop - TRACK_INSET_BOTTOM);
  const toTrack = (contentY: number) => (contentY / metrics.scrollHeight) * trackHeight;
  const viewportTop = toTrack(metrics.scrollTop);
  const viewportHeight = Math.max(12, toTrack(metrics.clientHeight));

  const center = metrics.scrollTop + metrics.clientHeight / 3;
  let activeKey = "";
  for (const marker of markers) {
    if (metrics.listTop + marker.start <= center) activeKey = marker.key;
    else break;
  }

  const scrubTo = (clientY: number) => {
    const stream = streamRef.current;
    const track = trackRef.current;
    if (!stream || !track || !trackHeight) return;
    const rect = track.getBoundingClientRect();
    const ratio = Math.min(1, Math.max(0, (clientY - rect.top) / trackHeight));
    stream.scrollTop = ratio * stream.scrollHeight - stream.clientHeight / 2;
  };

  const hoveredMarker = hovered ? markers.find((marker) => marker.key === hovered) : undefined;

  return (
    <div
      className="pointer-events-none absolute right-3 z-10 w-6"
      style={{ top: trackTop, height: trackHeight }}
      data-testid="conversation-minimap"
    >
      <div
        ref={trackRef}
        aria-hidden
        className="group/minimap pointer-events-auto relative h-full w-full cursor-pointer touch-none"
        onPointerDown={(event) => {
          if (event.button !== 0) return;
          if ((event.target as HTMLElement).closest("[data-minimap-marker]")) return;
          event.currentTarget.setPointerCapture(event.pointerId);
          scrubRef.current = { pointerId: event.pointerId, moved: false };
          scrubTo(event.clientY);
        }}
        onPointerMove={(event) => {
          const scrub = scrubRef.current;
          if (!scrub || scrub.pointerId !== event.pointerId) return;
          scrub.moved = true;
          scrubTo(event.clientY);
        }}
        onPointerUp={(event) => {
          if (scrubRef.current?.pointerId === event.pointerId) scrubRef.current = null;
        }}
        onPointerCancel={() => { scrubRef.current = null; }}
        onPointerLeave={() => setHovered("")}
      >
        <span className="absolute inset-y-0 right-[5px] w-px bg-cx-border-subtle opacity-0 transition-opacity duration-150 group-hover/minimap:opacity-100" />
        <span
          className="absolute right-0 w-[11px] rounded-[3px] bg-cx-fg-4/15 transition-colors group-hover/minimap:bg-cx-fg-4/25"
          style={{ top: viewportTop, height: Math.min(viewportHeight, trackHeight - viewportTop) }}
          data-testid="conversation-minimap-viewport"
        />
        {markers.map((marker) => {
          const top = Math.min(trackHeight - 2, toTrack(metrics.listTop + marker.start));
          const active = marker.key === activeKey;
          return (
            <button
              key={marker.key}
              type="button"
              tabIndex={-1}
              data-minimap-marker
              onPointerEnter={() => setHovered(marker.key)}
              onClick={() => onJump(marker.messageId)}
              className="absolute right-0 flex h-[7px] w-6 -translate-y-1/2 items-center justify-end"
              style={{ top }}
            >
              <span
                className={cn(
                  "block h-[2px] rounded-full transition-all duration-150",
                  marker.tone === "danger" && "bg-cx-danger",
                  marker.tone === "running" && "bg-cx-accent cx-pulse-dot",
                  marker.tone === "muted" && "bg-cx-fg-4/50",
                  marker.tone === "default" && (active ? "bg-cx-fg-2" : "bg-cx-fg-4"),
                  active || hovered === marker.key ? "w-[11px]" : "w-[7px]",
                )}
              />
            </button>
          );
        })}
      </div>
      {hoveredMarker ? (
        <div
          className="pointer-events-none absolute right-8 w-max max-w-[260px] -translate-y-1/2 rounded-lg bg-cx-overlay px-2.5 py-1.5 shadow-cx-pop"
          style={{ top: Math.min(trackHeight - 2, toTrack(metrics.listTop + hoveredMarker.start)) }}
          role="tooltip"
          data-testid="conversation-minimap-tooltip"
        >
          <p className="text-[11px] font-medium text-cx-fg-3">
            {hoveredMarker.seq ? `第 ${hoveredMarker.seq} 轮` : "用户消息"}
            {hoveredMarker.tone === "danger" ? " · 失败" : hoveredMarker.tone === "running" ? " · 执行中" : ""}
          </p>
          <p className="line-clamp-2 break-words text-[12px] leading-[17px] text-cx-fg">{hoveredMarker.label || "（空消息）"}</p>
        </div>
      ) : null}
    </div>
  );
}
