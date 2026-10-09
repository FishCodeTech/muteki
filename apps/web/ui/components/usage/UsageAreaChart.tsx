"use client";

import { useState, type PointerEvent } from "react";
import { cn } from "@/lib/cn";

const VIEW_WIDTH = 960;
const VIEW_HEIGHT = 260;
const PLOT_TOP = 8;

export interface UsageChartPoint {
  label: string;
  value: number;
}

type Point = { x: number; y: number };

/** Round the axis up to a 1/2/5 × 10ⁿ step so tick labels stay readable. */
export function niceScale(peak: number, count: number): { max: number; ticks: number[] } {
  if (peak <= 0) return { max: 0, ticks: [0] };
  const rawStep = peak / count;
  const magnitude = 10 ** Math.floor(Math.log10(rawStep));
  const normalized = rawStep / magnitude;
  const step = (normalized > 5 ? 10 : normalized > 2 ? 5 : normalized > 1 ? 2 : 1) * magnitude;
  const max = Math.ceil(peak / step) * step;
  const ticks: number[] = [];
  for (let tick = 0; tick <= max + step / 2; tick += step) ticks.push(tick);
  return { max, ticks };
}

/** Fritsch–Carlson tangents: the curve never overshoots a spike or dips below zero. */
function monotoneTangents(points: Point[]): number[] {
  const count = points.length;
  if (count < 2) return [0];
  const slopes: number[] = [];
  for (let index = 0; index < count - 1; index += 1) {
    const dx = points[index + 1].x - points[index].x;
    slopes.push(dx === 0 ? 0 : (points[index + 1].y - points[index].y) / dx);
  }
  const tangents = Array.from({ length: count }, () => 0);
  tangents[0] = slopes[0];
  tangents[count - 1] = slopes[count - 2];
  for (let index = 1; index < count - 1; index += 1) {
    const previous = slopes[index - 1];
    const next = slopes[index];
    tangents[index] = previous * next <= 0 ? 0 : (previous + next) / 2;
  }
  for (let index = 0; index < count - 1; index += 1) {
    const slope = slopes[index];
    if (slope === 0) {
      tangents[index] = 0;
      tangents[index + 1] = 0;
      continue;
    }
    const a = tangents[index] / slope;
    const b = tangents[index + 1] / slope;
    const magnitude = a * a + b * b;
    if (magnitude > 9) {
      const scale = 3 / Math.sqrt(magnitude);
      tangents[index] = scale * a * slope;
      tangents[index + 1] = scale * b * slope;
    }
  }
  return tangents;
}

function curvePath(points: Point[]): string {
  if (points.length === 0) return "";
  if (points.length === 1) return `M${points[0].x},${points[0].y}`;
  const tangents = monotoneTangents(points);
  let path = `M${points[0].x},${points[0].y}`;
  for (let index = 0; index < points.length - 1; index += 1) {
    const from = points[index];
    const to = points[index + 1];
    const dx = (to.x - from.x) / 3;
    path += ` C${from.x + dx},${from.y + dx * tangents[index]} ${to.x - dx},${to.y - dx * tangents[index + 1]} ${to.x},${to.y}`;
  }
  return path;
}

export function UsageAreaChart({ points, format, ariaLabel, className }: {
  points: UsageChartPoint[];
  format: (value: number) => string;
  ariaLabel: string;
  className?: string;
}) {
  const [hover, setHover] = useState<number | null>(null);
  const peak = Math.max(0, ...points.map(point => point.value));
  const { max, ticks } = niceScale(peak, 4);
  const toY = (value: number) => max === 0 ? VIEW_HEIGHT : VIEW_HEIGHT - (value / max) * (VIEW_HEIGHT - PLOT_TOP);
  const toX = (index: number) => points.length <= 1 ? VIEW_WIDTH / 2 : (index / (points.length - 1)) * VIEW_WIDTH;
  const coordinates = points.map((point, index) => ({ x: toX(index), y: toY(point.value) }));
  const line = curvePath(coordinates);
  const area = coordinates.length > 1 ? `${line} L${VIEW_WIDTH},${VIEW_HEIGHT} L0,${VIEW_HEIGHT} Z` : "";
  const axisLabels = points.length > 2
    ? [points[0], points[Math.floor((points.length - 1) / 2)], points[points.length - 1]]
    : points;

  const track = (event: PointerEvent<HTMLDivElement>) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    if (bounds.width === 0 || points.length === 0) return;
    const ratio = Math.min(1, Math.max(0, (event.clientX - bounds.left) / bounds.width));
    setHover(Math.round(ratio * (points.length - 1)));
  };
  const active = hover === null ? null : points[hover];

  return <div className={cn("flex flex-col gap-1.5", className)}>
    <div className="flex h-56">
      <div className="relative w-14 shrink-0 text-[11px] tabular-nums text-cx-fg-4" aria-hidden>
        {ticks.map(tick => <span key={tick} className="absolute right-2 -translate-y-1/2" style={{ top: `${(toY(tick) / VIEW_HEIGHT) * 100}%` }}>{format(tick)}</span>)}
      </div>
      <div className="relative min-w-0 flex-1" onPointerMove={track} onPointerLeave={() => setHover(null)} role="img" aria-label={ariaLabel}>
        <svg viewBox={`0 0 ${VIEW_WIDTH} ${VIEW_HEIGHT}`} preserveAspectRatio="none" className="absolute inset-0 size-full overflow-visible text-cx-fg">
          {ticks.map(tick => <line key={tick} x1={0} x2={VIEW_WIDTH} y1={toY(tick)} y2={toY(tick)} stroke="var(--cx-border-subtle)" vectorEffect="non-scaling-stroke" />)}
          {area ? <path d={area} fill="currentColor" fillOpacity={0.1} /> : null}
          {line ? <path d={line} fill="none" stroke="currentColor" strokeWidth={2} vectorEffect="non-scaling-stroke" strokeLinejoin="round" /> : null}
          {hover !== null ? <line x1={toX(hover)} x2={toX(hover)} y1={0} y2={VIEW_HEIGHT} stroke="var(--cx-fg-4)" vectorEffect="non-scaling-stroke" /> : null}
        </svg>
        {active ? <div className="pointer-events-none absolute top-2 z-10 rounded-lg border border-cx-border bg-cx-elevated px-2.5 py-1.5 text-[12px] shadow-cx-xs"
          style={{ left: `${(toX(hover!) / VIEW_WIDTH) * 100}%`, transform: `translateX(${hover! > (points.length - 1) / 2 ? "calc(-100% - 8px)" : "8px"})` }}>
          <div className="text-cx-fg-3">{active.label}</div>
          <div className="font-medium tabular-nums text-cx-fg">{format(active.value)}</div>
        </div> : null}
      </div>
    </div>
    <div className="flex justify-between pl-14 text-[11px] uppercase tracking-wide text-cx-fg-4" aria-hidden>
      {axisLabels.map((point, index) => <span key={`${point.label}-${index}`}>{point.label}</span>)}
    </div>
  </div>;
}
