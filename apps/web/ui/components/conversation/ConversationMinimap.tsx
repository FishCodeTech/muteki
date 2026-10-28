"use client";

/**
 * Turn rail — a column of short ticks in the stream's left gutter, one per
 * user prompt, evenly spaced rather than proportional to content height. Past
 * MAX_TICKS the rail shows a window of turns around the reading position.
 * Turns inside the viewport read darker; hovering
 * swells the ticks around the pointer into a wave and previews the prompt and
 * the reply, click jumps to the turn. Keyboard turn navigation stays on
 * Alt+↑/↓, so the ticks are not tab stops.
 */

import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

export type MinimapMarker = {
  key: string;
  messageId: string;
  /** Virtualizer row index, used to read the live offset via `readRowStart`. */
  index: number;
  /** Row offset inside the virtualized list at the parent's last render, in px. */
  start: number;
  label: string;
  /** Plain-text excerpt of the turn's reply. */
  preview?: string;
  tone: "default" | "running" | "danger" | "muted";
};

type StreamMetrics = {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
  clientWidth: number;
  listTop: number;
  /** Free space between the stream's left edge and the reading column. */
  leftGap: number;
};

const MIN_MARKERS = 2;
const RAIL_LEFT = 16;
const MIN_LEFT_GAP = RAIL_LEFT + 14;
const INSET_Y = 24;
const TICK_PITCH = 10;
/** Like ChatGPT's rail, the column stays short enough to scan at a glance. */
const MAX_TICKS = 29;
const TICK_W = 6;
const PEAK_W = 26;
/** Ticks on each side of the pointer that the swell reaches. */
const WAVE_REACH = 4;
const HIT_W = 40;
const CARD_W = 320;
/** The card's left edge sits this far from the tick start, just past a fully swollen tick. */
const CARD_OFFSET = 36;

const TICK_COLOR = {
  idle: "color-mix(in srgb, var(--cx-fg) 20%, transparent)",
  inView: "color-mix(in srgb, var(--cx-fg) 60%, transparent)",
  hovered: "var(--cx-fg)",
  danger: "var(--cx-danger)",
  running: "var(--cx-accent)",
} as const;

/** Plain-text excerpt of a Markdown reply for the hover card. */
export function replyExcerpt(markdown: string, limit = 280): string {
  // Only the head is ever shown; bounding it keeps this cheap on every timeline render.
  return markdown
    .slice(0, limit * 8)
    .replace(/```[\s\S]*?(```|$)/g, " ")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/^\s{0,3}(#{1,6}|>|[-*+]|\d+[.)])\s+/gm, "")
    .replace(/^\s*\|?\s*:?-{3,}.*$/gm, " ")
    .replace(/[*`~|]/g, "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, limit);
}

function readMetrics(stream: HTMLElement, list: HTMLElement | null): StreamMetrics {
  const streamRect = stream.getBoundingClientRect();
  const listLeft = list ? list.getBoundingClientRect().left : streamRect.left;
  return {
    scrollTop: stream.scrollTop,
    scrollHeight: stream.scrollHeight,
    clientHeight: stream.clientHeight,
    clientWidth: stream.clientWidth,
    listTop: list ? list.offsetTop : 0,
    leftGap: Math.round(listLeft - streamRect.left),
  };
}

function sameMetrics(a: StreamMetrics | null, b: StreamMetrics) {
  return Boolean(a)
    && a!.scrollTop === b.scrollTop
    && a!.scrollHeight === b.scrollHeight
    && a!.clientHeight === b.clientHeight
    && a!.clientWidth === b.clientWidth
    && a!.listTop === b.listTop
    && a!.leftGap === b.leftGap;
}

function tickWidth(distance: number | null): number {
  if (distance === null) return TICK_W;
  const reach = Math.max(0, 1 - Math.abs(distance) / WAVE_REACH);
  return TICK_W + (PEAK_W - TICK_W) * reach ** 1.2;
}

export function ConversationMinimap({
  streamRef,
  listRef,
  markers,
  readRowStart,
  onJump,
}: {
  streamRef: React.RefObject<HTMLDivElement | null>;
  listRef: React.RefObject<HTMLDivElement | null>;
  markers: MinimapMarker[];
  /**
   * Row heights can change without the parent re-rendering (the virtualizer
   * repositions rows directly in the DOM), so offsets are re-read whenever the
   * list resize observer re-renders the minimap.
   */
  readRowStart?: (index: number) => number | undefined;
  onJump: (messageId: string) => void;
}) {
  const startOf = (marker: MinimapMarker) => readRowStart?.(marker.index) ?? marker.start;
  const [metrics, setMetrics] = useState<StreamMetrics | null>(null);
  /** Fractional tick position under the pointer, or null when not hovering. */
  const [pointer, setPointer] = useState<number | null>(null);
  const railRef = useRef<HTMLDivElement>(null);
  const frameRef = useRef(0);

  const measureNow = useCallback(() => {
    cancelAnimationFrame(frameRef.current);
    const stream = streamRef.current;
    if (!stream) return;
    const next = readMetrics(stream, listRef.current);
    setMetrics((prev) => (sameMetrics(prev, next) ? prev : next));
  }, [listRef, streamRef]);
  const measure = useCallback(() => {
    cancelAnimationFrame(frameRef.current);
    frameRef.current = requestAnimationFrame(measureNow);
  }, [measureNow]);

  useEffect(() => {
    const stream = streamRef.current;
    if (!stream) return;
    measure();
    stream.addEventListener("scroll", measure, { passive: true });
    // Resize callbacks run right after layout, so reading there is free; a
    // deferred read would land after animation frames have dirtied layout
    // again and force a second layout every frame of a panel transition.
    const observer = new ResizeObserver(measureNow);
    observer.observe(stream);
    if (listRef.current) observer.observe(listRef.current);
    return () => {
      cancelAnimationFrame(frameRef.current);
      stream.removeEventListener("scroll", measure);
      observer.disconnect();
    };
  }, [listRef, measure, measureNow, streamRef]);

  // Row measurements change without a scroll or resize on the stream itself.
  useLayoutEffect(() => {
    measure();
  }, [markers, measure]);

  if (!metrics || markers.length < MIN_MARKERS) return null;
  if (metrics.leftGap < MIN_LEFT_GAP) return null;
  if (metrics.scrollHeight <= metrics.clientHeight + 8) return null;

  const viewTop = metrics.scrollTop;
  const viewBottom = metrics.scrollTop + metrics.clientHeight;
  const starts = markers.map((marker) => metrics.listTop + startOf(marker));
  const allInView = starts.map((start, i) => {
    const end = i + 1 < markers.length ? starts[i + 1] : metrics.scrollHeight;
    return end > viewTop && start < viewBottom;
  });

  // One tick per turn up to a cap; longer threads show a window of turns centred on what is being read.
  const capacity = Math.max(MIN_MARKERS, Math.min(MAX_TICKS, Math.floor((metrics.clientHeight - INSET_Y * 2) / TICK_PITCH)));
  const firstInView = allInView.indexOf(true);
  const lastInView = allInView.lastIndexOf(true);
  const focus = firstInView < 0 ? markers.length - 1 : Math.round((firstInView + lastInView) / 2);
  const windowStart = Math.max(0, Math.min(markers.length - capacity, focus - Math.floor(capacity / 2)));
  const ticks = markers.slice(windowStart, windowStart + capacity);
  const inView = allInView.slice(windowStart, windowStart + capacity);

  const count = ticks.length;
  const pitch = TICK_PITCH;
  const railHeight = pitch * count;
  const railTop = Math.max(INSET_Y, (metrics.clientHeight - railHeight) / 2);

  const hoveredIndex = pointer === null ? -1 : Math.min(count - 1, Math.round(pointer));
  const hoveredMarker = hoveredIndex >= 0 ? ticks[hoveredIndex] : undefined;

  const pointerToTick = (clientY: number) => {
    const rail = railRef.current;
    if (!rail) return null;
    const y = clientY - rail.getBoundingClientRect().top;
    return Math.min(count - 1, Math.max(0, y / pitch - 0.5));
  };

  const cardWidth = Math.min(CARD_W, metrics.clientWidth - RAIL_LEFT - CARD_OFFSET - 16);
  const cardCenter = hoveredIndex >= 0
    ? Math.min(metrics.clientHeight - 64, Math.max(64, railTop + (hoveredIndex + 0.5) * pitch))
    : 0;

  return (
    <div className="pointer-events-none absolute inset-y-0 left-0 z-10" data-testid="conversation-minimap">
      <div
        ref={railRef}
        aria-hidden
        className="pointer-events-auto absolute cursor-pointer"
        style={{ left: RAIL_LEFT - 6, top: railTop, width: HIT_W, height: railHeight }}
        onPointerMove={(event) => setPointer(pointerToTick(event.clientY))}
        onPointerLeave={() => setPointer(null)}
        onClick={(event) => {
          const tick = pointerToTick(event.clientY);
          if (tick !== null) onJump(ticks[Math.round(tick)].messageId);
        }}
      >
        {ticks.map((marker, i) => {
          const color = marker.tone === "danger"
            ? TICK_COLOR.danger
            : marker.tone === "running"
              ? TICK_COLOR.running
              : i === hoveredIndex
                ? TICK_COLOR.hovered
                : inView[i]
                  ? TICK_COLOR.inView
                  : TICK_COLOR.idle;
          return (
            <span
              key={marker.key}
              data-minimap-marker
              className="absolute block h-[2px] -translate-y-1/2 rounded-full transition-[width,background-color] duration-150 ease-out motion-reduce:transition-none"
              style={{
                left: 6,
                top: (i + 0.5) * pitch,
                width: tickWidth(pointer === null ? null : i - pointer),
                backgroundColor: color,
                opacity: marker.tone === "muted" && i !== hoveredIndex ? 0.5 : undefined,
              }}
            />
          );
        })}
      </div>
      {hoveredMarker && cardWidth > 160 ? (
        <div
          className="pointer-events-none absolute -translate-y-1/2 rounded-xl border border-cx-border bg-cx-elevated px-2.5 py-3 shadow-[0_10px_28px_-10px_hsl(var(--cx-shadow-color)/0.22)]"
          style={{ top: cardCenter, left: RAIL_LEFT + CARD_OFFSET, width: cardWidth }}
          role="tooltip"
          data-testid="conversation-minimap-tooltip"
        >
          <p className="truncate text-[14px] font-semibold leading-[21px] text-cx-fg">
            {hoveredMarker.label || "（空消息）"}
          </p>
          {hoveredMarker.preview || hoveredMarker.tone !== "default" ? (
            <p
              className="mt-1 line-clamp-3 break-words text-[14px] leading-[21px]"
              style={{ color: "color-mix(in srgb, var(--cx-fg) 55%, transparent)" }}
            >
              {hoveredMarker.tone === "danger" ? "本轮失败。" : hoveredMarker.tone === "running" ? "正在回复… " : ""}
              {hoveredMarker.preview}
            </p>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
