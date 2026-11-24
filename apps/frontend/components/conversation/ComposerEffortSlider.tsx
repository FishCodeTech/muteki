"use client";

import { memo, useEffect, useRef, useState, type CSSProperties } from "react";
import { animate, motion, useMotionValue, useMotionValueEvent, useTransform, type Transition } from "motion/react";
import { cn } from "@/lib/cn";
import { EASE_OUT, Icon } from "@muteki/ui";
import { Menu, MenuItem, Tooltip, handleMenuKeyDown } from "@muteki/ui";
import { useReducedMotion } from "@muteki/ui";
import type { ConversationServiceTier } from "@/lib/useConversation";
import { ComposerSpeedIcon, serviceTierSpeedLevel, type ComposerSpeedLevel } from "./ComposerSpeedIcon";

const SNAP: Transition = { type: "spring", stiffness: 520, damping: 34, mass: 0.7 };
const SHOCK_EASE = [0.2, 0.6, 0.35, 1] as const;

const TRACK_H = 24;
const THUMB = 28;

const TIER_LABELS: Record<string, string> = { standard: "标准", default: "标准", fast: "快速", ultrafast: "超快", "ultra-fast": "超快" };
const SPEED_DESCRIPTIONS: Record<ComposerSpeedLevel, string> = {
  standard: "默认速度",
  fast: "更快响应，用量更高",
  ultrafast: "最快响应，用量最高",
};

export function serviceTierLabel(tier: ConversationServiceTier | undefined): string {
  if (!tier) return "标准";
  return TIER_LABELS[tier.name.trim().toLowerCase()] || tier.name || tier.id;
}

export interface ComposerSpeedOption {
  value: string;
  label: string;
  description?: string;
  level: ComposerSpeedLevel;
  disabled?: boolean;
}

export function serviceTierSpeedOptions(tiers: ConversationServiceTier[]): ComposerSpeedOption[] {
  if (!tiers.length) return [];
  const standard: ComposerSpeedOption = { value: "", label: "标准", description: SPEED_DESCRIPTIONS.standard, level: "standard" };
  return [standard, ...tiers.map((tier): ComposerSpeedOption => {
    const level = serviceTierSpeedLevel(tier);
    return { value: tier.id, label: serviceTierLabel(tier), description: tier.description || SPEED_DESCRIPTIONS[level], level };
  })];
}

export function variantSpeedOption(value: string, disabled = false): ComposerSpeedOption {
  const key = value.trim().toLowerCase();
  const level: ComposerSpeedLevel = key.includes("ultra") ? "ultrafast"
    : ["", "standard", "default", "normal", "off"].includes(key) ? "standard" : "fast";
  return { value, label: TIER_LABELS[key] || value, description: SPEED_DESCRIPTIONS[level], level, disabled };
}

export interface ComposerEffortSliderProps {
  levels: string[];
  value: string;
  modelDefault: string;
  modelLabel?: string;
  labelOf: (level: string) => string;
  onChange: (level: string) => void;
  canReset?: boolean;
  speedOptions: ComposerSpeedOption[];
  speed: string;
  onSpeedChange: (value: string) => void;
  embedded?: boolean;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

const along = (fraction: number) => `calc(${TRACK_H / 2}px + (100% - ${TRACK_H}px) * ${fraction})`;

function seeded(n: number): number {
  const x = Math.sin(n * 12.9898 + 78.233) * 43758.5453;
  return x - Math.floor(x);
}

const SPARKS = Array.from({ length: 26 }, (_, i) => {
  const size = 1.4 + seeded(i + 100) * 1.8;
  return {
    left: `${3 + seeded(i) * 94}%`,
    top: `${16 + seeded(i + 50) * 68}%`,
    width: size,
    height: size,
    "--d": `${1.6 + seeded(i + 150) * 2.4}s`,
    "--delay": `-${(seeded(i + 200) * 4).toFixed(2)}s`,
    "--drift": `${0.9 + seeded(i + 250) * 1.1}s`,
  } as CSSProperties;
});

const EffortTrackEffects = memo(function EffortTrackEffects() {
  return (
    <>
      <span className="cx-effort-beam" />
      {SPARKS.map((style, index) => <span key={index} className="cx-effort-spark" style={style} />)}
      <span className="cx-effort-sheen" />
    </>
  );
});

interface Burst { id: number; x: number; y: number; r: number }

interface EffortDrag {
  pointerId: number;
  start: number;
  distance: number;
  index: number;
}

function dragProgress(drag: EffortDrag, clientX: number): number {
  return clamp((clientX - drag.start) / drag.distance, 0, 1);
}

export function ComposerEffortSlider({
  levels, value, modelDefault, modelLabel, labelOf, onChange, canReset = true, speedOptions, speed: speedValue, onSpeedChange, embedded = false,
}: ComposerEffortSliderProps) {
  const reduced = useReducedMotion();
  const [speedOpen, setSpeedOpen] = useState(false);
  const fxRef = useRef<HTMLDivElement>(null);
  const trackRef = useRef<HTMLDivElement>(null);
  const sliderRef = useRef<HTMLDivElement>(null);
  const dragRef = useRef<EffortDrag | null>(null);
  const [dragIndex, setDragIndex] = useState<number | null>(null);
  const [bursts, setBursts] = useState<Burst[]>([]);
  const burstId = useRef(0);
  const stops = !levels.length || value || modelDefault ? levels : [""].concat(levels);
  const committed = Math.max(0, stops.indexOf(value || modelDefault));
  const lastIndex = stops.length - 1;
  const span = Math.max(1, lastIndex);
  const preview = dragIndex ?? committed;
  const previewLevel = stops[preview] ?? "";
  const followsDefault = !value && dragIndex === null;
  const isMax = stops.length > 1 && preview === stops.length - 1;
  const dragging = dragIndex !== null;
  const target = committed / span;

  const progress = useMotionValue(target);
  const thumbTransform = useTransform(progress, (v) => `translateX(${v * 100}%)`);
  const fillTransform = useTransform(progress, (v) => `translateX(calc(${(1 - v) * TRACK_H}px - ${(1 - v) * 100}%))`);
  const effectsTransform = useTransform(progress, (v) => `translateX(calc(${(1 - v) * 100}% - ${(1 - v) * TRACK_H}px))`);
  const glowTransform = useTransform(progress, (v) => `scaleX(${clamp(v, 0, 1)})`);

  useEffect(() => {
    if (dragging || progress.get() === target) return;
    if (reduced) {
      progress.jump(target);
      return;
    }
    const controls = animate(progress, target, SNAP);
    return () => controls.stop();
  }, [progress, target, dragging, reduced]);

  const armed = useRef(target < 0.995);
  useMotionValueEvent(progress, "change", (v) => {
    if (v < 0.98) {
      armed.current = true;
      return;
    }
    if (v < 0.995 || !armed.current || !lastIndex || reduced) return;
    armed.current = false;
    const root = fxRef.current?.getBoundingClientRect();
    const track = trackRef.current?.getBoundingClientRect();
    if (!root || !track) return;
    const x = track.right - TRACK_H / 2 - root.left;
    const y = track.top + track.height / 2 - root.top;
    const r = Math.hypot(Math.max(x, root.width - x), Math.max(y, root.height - y)) + 12;
    const burst = { id: ++burstId.current, x, y, r };
    setBursts((list) => list.slice(-1).concat(burst));
  });

  const commit = (index: number) => {
    const next = stops[clamp(index, 0, stops.length - 1)] ?? "";
    if (next !== value) onChange(next);
  };
  const endDrag = (pointerId: number) => {
    if (dragRef.current?.pointerId !== pointerId) return;
    dragRef.current = null;
    setDragIndex(null);
  };
  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    let next = -1;
    if (event.key === "ArrowRight" || event.key === "ArrowUp") next = Math.min(stops.length - 1, committed + 1);
    else if (event.key === "ArrowLeft" || event.key === "ArrowDown") next = Math.max(0, committed - 1);
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = stops.length - 1;
    if (next < 0) return;
    event.preventDefault();
    event.stopPropagation();
    commit(next);
  };

  const headline = labelOf(previewLevel);
  const activeSpeed = speedOptions.find((option) => option.value === speedValue) ?? speedOptions[0];
  const speedLevel = activeSpeed?.level ?? "standard";
  const speed = speedLevel === "standard" ? undefined : speedLevel;
  const speedSummary = activeSpeed ? [`速度：${activeSpeed.label}`, activeSpeed.description].filter(Boolean).join(" · ") : "";
  const chooseSpeed = (option: ComposerSpeedOption) => {
    if (!option.disabled && option.value !== activeSpeed?.value) onSpeedChange(option.value);
  };
  const speedItems = speedOptions.map((option) => (
    <MenuItem
      key={option.value || "standard"}
      checked={option.value === activeSpeed?.value}
      disabled={option.disabled}
      description={option.description}
      wrapDescription
      onSelect={() => chooseSpeed(option)}
    >
      {option.label}
    </MenuItem>
  ));
  const subtitle = [modelLabel || "思考强度", followsDefault && modelDefault ? "默认" : ""].filter(Boolean).join(" · ");

  if (!stops.length) {
    return (
      <div className="flex flex-col p-1" data-testid="composer-effort-panel">
        {speedOptions.length ? (
          <div role="menu" aria-label="速度" onKeyDown={handleMenuKeyDown} className="flex flex-col" data-testid="composer-speed-panel">
            {speedItems}
          </div>
        ) : null}
      </div>
    );
  }

  return (
    <div
      className={cn(
        "cx-effort-panel flex flex-col",
        embedded ? "gap-2.5 px-3 pb-3 pt-2" : "relative gap-3 rounded-xl px-3 pb-3 pt-2.5",
      )}
      data-testid="composer-effort-panel"
    >
      <div className="grid grid-cols-[28px_minmax(0,1fr)_28px] items-center gap-1">
        {speedOptions.length ? (
          <Tooltip content={speedSummary} disabled={speedOpen}>
            <span className="inline-flex">
              <Menu
                open={speedOpen}
                onOpenChange={setSpeedOpen}
                placement="right-start"
                ariaLabel="速度"
                className="w-[232px]"
                trigger={(
                  <button
                    type="button"
                    aria-label={speedSummary}
                    className={cn(
                      "cx-press grid size-7 place-items-center rounded-lg outline-none focus-visible:shadow-[0_0_0_2px_var(--cx-focus)]",
                      speed ? "text-cx-accent" : "text-cx-fg-3",
                      "hover:bg-cx-hover data-[state=open]:bg-cx-active",
                    )}
                    data-testid="composer-speed-toggle"
                    data-speed={activeSpeed?.value || undefined}
                  >
                    <ComposerSpeedIcon level={speedLevel} size={14} />
                  </button>
                )}
              >
                {speedItems}
              </Menu>
            </span>
          </Tooltip>
        ) : (
          <span aria-hidden className="grid size-7 place-items-center text-cx-fg-4">
            <Icon name="brain" size={14} />
          </span>
        )}
        <div className="flex min-w-0 flex-col items-center">
          <span
            className={cn("cx-effort-title h-5 max-w-full truncate text-cx-base font-semibold", isMax && "is-max")}
            data-testid="composer-effort-value"
          >
            {headline}
          </span>
          <span className="max-w-full truncate text-cx-xs leading-4 text-cx-fg-3" data-testid="composer-effort-model">
            {subtitle}
          </span>
        </div>
        <Tooltip content="恢复默认" disabled={!value || !canReset}>
          <button
            type="button"
            onClick={() => onChange("")}
            disabled={!value || !canReset}
            aria-label="恢复默认思考强度"
            className={cn(
              "cx-press grid size-7 place-items-center rounded-lg text-cx-fg-3 outline-none",
              "hover:bg-cx-hover hover:text-cx-fg focus-visible:shadow-[0_0_0_2px_var(--cx-focus)]",
              "disabled:cursor-default disabled:opacity-35 disabled:hover:bg-transparent disabled:hover:text-cx-fg-3",
            )}
            data-testid="composer-effort-reset"
          >
            <Icon name="retry" size={14} />
          </button>
        </Tooltip>
      </div>

      <div
        ref={trackRef}
        className="cx-effort-track relative mx-0.5 cursor-pointer touch-none select-none rounded-full"
        style={{ height: TRACK_H }}
        onPointerDown={(event) => {
          if (event.button !== 0 || dragRef.current) return;
          event.preventDefault();
          const rect = event.currentTarget.getBoundingClientRect();
          const drag = {
            pointerId: event.pointerId,
            start: rect.left + TRACK_H / 2,
            distance: Math.max(1, rect.width - TRACK_H),
            index: committed,
          };
          const position = dragProgress(drag, event.clientX);
          drag.index = Math.round(position * lastIndex);
          dragRef.current = drag;
          event.currentTarget.setPointerCapture(event.pointerId);
          sliderRef.current?.focus({ preventScroll: true });
          progress.jump(position);
          setDragIndex(drag.index);
        }}
        onPointerMove={(event) => {
          const drag = dragRef.current;
          if (drag?.pointerId !== event.pointerId) return;
          const position = dragProgress(drag, event.clientX);
          progress.set(position);
          const index = Math.round(position * lastIndex);
          if (drag.index !== index) {
            drag.index = index;
            setDragIndex(index);
          }
        }}
        onPointerUp={(event) => {
          const drag = dragRef.current;
          if (drag?.pointerId !== event.pointerId) return;
          const position = dragProgress(drag, event.clientX);
          progress.set(position);
          endDrag(event.pointerId);
          commit(Math.round(position * lastIndex));
        }}
        onPointerCancel={(event) => endDrag(event.pointerId)}
        onLostPointerCapture={(event) => endDrag(event.pointerId)}
        data-dragging={dragging || undefined}
        data-max={isMax || undefined}
        data-speed={speed}
        data-testid="composer-effort-track"
      >
        <motion.span aria-hidden className="cx-effort-glow" style={{ transform: glowTransform, opacity: isMax ? undefined : preview / span * 0.32 }} />
        <span aria-hidden className="pointer-events-none absolute inset-0 overflow-hidden rounded-full">
          <motion.span className="cx-effort-fill" style={{ transform: fillTransform }}>
            <motion.span className="absolute inset-0" style={{ transform: effectsTransform }}>
              <EffortTrackEffects />
            </motion.span>
          </motion.span>
        </span>
        {stops.map((stop, index) => (
          <span
            key={stop || "default"}
            aria-hidden
            className={cn(
              "pointer-events-none absolute top-1/2 size-1 -translate-x-1/2 -translate-y-1/2 rounded-full",
              index < preview ? "bg-white/45" : "bg-cx-fg-4/55",
              index === preview && "opacity-0",
            )}
            style={{ left: along(index / span) }}
            data-effort-stop={stop || "default"}
          />
        ))}
        <motion.div
          className="pointer-events-none absolute top-1/2"
          style={{ insetInline: TRACK_H / 2, transform: thumbTransform }}
        >
          <div
            ref={sliderRef}
            role="slider"
            tabIndex={0}
            data-autofocus=""
            aria-label="思考强度"
            aria-valuemin={0}
            aria-valuemax={stops.length - 1}
            aria-valuenow={preview}
            aria-valuetext={headline}
            onKeyDown={onKeyDown}
            className="cx-effort-thumb absolute left-0 top-0 -translate-x-1/2 -translate-y-1/2 rounded-full bg-white outline-none"
            style={{ width: THUMB, height: THUMB }}
            data-testid="composer-effort-thumb"
          />
        </motion.div>
      </div>
      <div
        ref={fxRef}
        aria-hidden
        className={cn("pointer-events-none absolute inset-0 overflow-hidden", embedded ? "rounded-xl" : "rounded-[inherit]")}
      >
        {bursts.map((burst) => (
          <span key={burst.id}>
            <motion.span
              className="cx-effort-bloom"
              style={{ left: burst.x - 70, top: burst.y - 70 }}
              initial={{ scale: 0.2, opacity: 0.95 }}
              animate={{ scale: 1.7, opacity: 0 }}
              transition={{ duration: 0.65, ease: EASE_OUT }}
            />
            <motion.span
              className="cx-effort-shock"
              style={{ left: burst.x - burst.r, top: burst.y - burst.r, width: burst.r * 2, height: burst.r * 2 }}
              initial={{ scale: 0.04, opacity: 1 }}
              animate={{ scale: 1, opacity: [1, 1, 0] }}
              transition={{ duration: 1.05, ease: SHOCK_EASE, opacity: { duration: 1.05, times: [0, 0.6, 1] } }}
              onAnimationComplete={() => setBursts((list) => list.filter((row) => row.id !== burst.id))}
            />
            <motion.span
              className="cx-effort-edge"
              initial={{ opacity: 0 }}
              animate={{ opacity: [0, 1, 0] }}
              transition={{ duration: 1.1, delay: 0.22, times: [0, 0.25, 1] }}
            />
          </span>
        ))}
      </div>
    </div>
  );
}
