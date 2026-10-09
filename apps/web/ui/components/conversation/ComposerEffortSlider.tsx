"use client";

import { useEffect, useRef, useState, type CSSProperties } from "react";
import { AnimatePresence, animate, motion, useMotionValue, useMotionValueEvent, useTransform, type Transition } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { SegmentedControl, Tooltip } from "@/components/chat/ui";
import { useReducedMotion } from "@/components/chat/ui/motion";
import type { ConversationServiceTier } from "@/lib/useConversation";

const SNAP: Transition = { type: "spring", stiffness: 520, damping: 34, mass: 0.7 };
const FOLLOW: Transition = { type: "spring", stiffness: 1400, damping: 60, mass: 0.4 };
const INSTANT: Transition = { duration: 0 };
const SHOCK_EASE = [0.2, 0.6, 0.35, 1] as const;

/** Track height; the thumb centre travels between the two rounded ends. */
const TRACK_H = 26;
const THUMB = 30;

const TIER_LABELS: Record<string, string> = { fast: "快速", ultrafast: "极速" };

export function serviceTierLabel(tier: ConversationServiceTier | undefined): string {
  if (!tier) return "标准";
  return TIER_LABELS[tier.name.trim().toLowerCase()] || tier.name || tier.id;
}

export interface ComposerEffortSliderProps {
  /** Ordered provider levels, without the "follow model default" sentinel. */
  levels: string[];
  /** "" follows the model default. */
  value: string;
  modelDefault: string;
  modelLabel?: string;
  labelOf: (level: string) => string;
  onChange: (level: string) => void;
  serviceTiers: ConversationServiceTier[];
  serviceTier: string;
  onServiceTierChange: (tier: string) => void;
  /**
   * Rendered as a section of a larger popover: the panel drops its own
   * positioning so the arrival burst spreads across the nearest positioned
   * ancestor (the popover) instead of this section.
   */
  embedded?: boolean;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

// Seeded so server and client render identical particles.
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

const along = (fraction: number | string) => `calc(${TRACK_H / 2}px + (100% - ${TRACK_H}px) * ${fraction})`;

interface Burst { id: number; x: number; y: number; r: number }

/** Codex-style intelligence slider; the top stop charges the panel with light. */
export function ComposerEffortSlider({
  levels, value, modelDefault, modelLabel, labelOf, onChange, serviceTiers, serviceTier, onServiceTierChange, embedded = false,
}: ComposerEffortSliderProps) {
  const reduced = useReducedMotion();
  const fxRef = useRef<HTMLDivElement>(null);
  const trackRef = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<number | null>(null);
  const [bursts, setBursts] = useState<Burst[]>([]);
  const burstId = useRef(0);
  const directionRef = useRef(1);
  // Without an explicit choice the thumb rests on the level the model would use;
  // a model without a declared default gets a leading "默认" stop instead.
  const stops = !levels.length || value || modelDefault ? levels : [""].concat(levels);
  const committed = Math.max(0, stops.indexOf(value || modelDefault));
  const span = Math.max(1, stops.length - 1);
  const preview = drag === null ? committed : Math.round(drag * span);
  const previewLevel = stops[preview] ?? "";
  const followsDefault = !value && drag === null;
  const isMax = stops.length > 1 && preview === stops.length - 1;
  const dragging = drag !== null;
  const target = drag ?? committed / span;

  const progress = useMotionValue(target);
  const thumbLeft = useTransform(progress, (v) => along(v));
  const fillWidth = useTransform(progress, (v) => `calc(${TRACK_H}px + (100% - ${TRACK_H}px) * ${v})`);
  const fillClip = useTransform(progress, (v) => `inset(0 calc((100% - ${TRACK_H}px) * ${1 - v}) 0 0 round 999px)`);

  useEffect(() => {
    const controls = animate(progress, target, reduced ? INSTANT : dragging ? FOLLOW : SNAP);
    return () => controls.stop();
  }, [progress, target, dragging, reduced]);

  // The shockwave fires when the thumb physically hits the end, so it never
  // detonates ahead of a thumb that is still springing there. Opening the
  // panel already at max stays quiet.
  const isMaxRef = useRef(isMax);
  isMaxRef.current = isMax;
  const armed = useRef(target < 0.995);
  useMotionValueEvent(progress, "change", (v) => {
    if (v < 0.98) {
      armed.current = true;
      return;
    }
    if (v < 0.995 || !armed.current || !isMaxRef.current || reduced) return;
    armed.current = false;
    const root = fxRef.current?.getBoundingClientRect();
    const track = trackRef.current?.getBoundingClientRect();
    if (!root || !track) return;
    const x = track.right - TRACK_H / 2 - root.left;
    const y = track.top + track.height / 2 - root.top;
    const r = Math.hypot(Math.max(x, root.width - x), Math.max(y, root.height - y)) + 12;
    burstId.current += 1;
    const burst = { id: burstId.current, x, y, r };
    setBursts((list) => list.slice(-1).concat(burst));
  });

  const commit = (index: number) => {
    const next = stops[clamp(index, 0, stops.length - 1)] ?? "";
    directionRef.current = index >= committed ? 1 : -1;
    if (next !== value) onChange(next);
  };
  const fraction = (clientX: number) => {
    const rect = trackRef.current?.getBoundingClientRect();
    const inner = rect ? rect.width - TRACK_H : 0;
    if (!rect || inner <= 0) return 0;
    return clamp((clientX - rect.left - TRACK_H / 2) / inner, 0, 1);
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
  const activeTier = serviceTiers.find((tier) => tier.id === serviceTier);
  const tierCycle = [""].concat(serviceTiers.map((tier) => tier.id));
  const nextTierId = tierCycle[(tierCycle.indexOf(activeTier?.id || "") + 1) % tierCycle.length] ?? "";
  const nextTier = serviceTiers.find((tier) => tier.id === nextTierId);
  const speedHint = `速度：${serviceTierLabel(activeTier)}，点击切换为${serviceTierLabel(nextTier)}`;
  const subtitle = [modelLabel || "思考强度", followsDefault && modelDefault ? "默认" : ""].filter(Boolean).join(" · ");

  if (!stops.length) {
    return (
      <div className="flex flex-col gap-2 p-3" data-testid="composer-effort-panel">
        {serviceTiers.length ? (
          <section className="flex flex-col gap-2" data-testid="composer-speed-panel">
            <div className="flex items-center justify-between gap-2">
              <span className="flex items-center gap-1.5 text-[12px] font-medium text-cx-fg-3">
                <Icon name="zap" size={13} />
                速度
              </span>
              <SegmentedControl
                size="xs"
                ariaLabel="速度"
                value={activeTier ? activeTier.id : ""}
                onChange={onServiceTierChange}
                options={[{ value: "", label: "标准" }].concat(serviceTiers.map((tier) => ({ value: tier.id, label: serviceTierLabel(tier) })))}
              />
            </div>
            <p className={cn("text-[11.5px] leading-4", activeTier ? "text-cx-warning" : "text-cx-fg-4")} data-testid="composer-speed-detail">
              {activeTier ? activeTier.description || "更快响应，用量更高" : "标准速度与用量"}
            </p>
          </section>
        ) : null}
      </div>
    );
  }

  return (
    <div
      className={cn(
        "cx-effort-panel flex flex-col",
        embedded ? "gap-3 px-3.5 pb-3.5 pt-2.5" : "relative gap-3.5 rounded-xl px-3.5 pb-4 pt-3",
      )}
      data-max={isMax || undefined}
      data-testid="composer-effort-panel"
    >
      <div className="grid grid-cols-[28px_minmax(0,1fr)_28px] items-center gap-1">
        {serviceTiers.length ? (
          <Tooltip content={speedHint}>
            <button
              type="button"
              onClick={() => onServiceTierChange(nextTierId)}
              aria-label={speedHint}
              aria-pressed={Boolean(activeTier)}
              className={cn(
                "cx-press grid size-7 place-items-center rounded-full outline-none focus-visible:shadow-[0_0_0_2px_var(--cx-focus)]",
                activeTier ? "bg-cx-warning-soft text-cx-warning" : "text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg",
              )}
              data-testid="composer-speed-toggle"
              data-service-tier={activeTier?.id || undefined}
            >
              <Icon name="zap" size={15} />
            </button>
          </Tooltip>
        ) : (
          <span aria-hidden className="grid size-7 place-items-center text-cx-fg-4">
            <Icon name="brain" size={15} />
          </span>
        )}
        <div className="flex min-w-0 flex-col items-center">
          <span className="relative inline-flex h-[22px] w-full items-center justify-center overflow-hidden">
            <AnimatePresence mode="popLayout" initial={false} custom={directionRef.current}>
              <motion.span
                key={headline}
                initial={reduced ? { opacity: 0 } : { opacity: 0, y: directionRef.current * 12, filter: "blur(2px)" }}
                animate={{ opacity: 1, y: 0, filter: "blur(0px)" }}
                exit={reduced ? { opacity: 0 } : { opacity: 0, y: directionRef.current * -12, filter: "blur(2px)" }}
                transition={{ duration: 0.18 }}
                className={cn("cx-effort-title truncate text-[15px] font-semibold", isMax && "is-max")}
                data-testid="composer-effort-value"
              >
                {headline}
              </motion.span>
            </AnimatePresence>
          </span>
          <span className="max-w-full truncate text-[12px] leading-4 text-cx-fg-3" data-testid="composer-effort-model">
            {subtitle}
          </span>
        </div>
        <Tooltip content="恢复默认" disabled={!value}>
          <button
            type="button"
            onClick={() => onChange("")}
            disabled={!value}
            aria-label="恢复默认思考强度"
            className={cn(
              "cx-press grid size-7 place-items-center rounded-full text-cx-fg-3 outline-none",
              "hover:bg-cx-hover hover:text-cx-fg focus-visible:shadow-[0_0_0_2px_var(--cx-focus)]",
              "disabled:cursor-default disabled:opacity-35 disabled:hover:bg-transparent disabled:hover:text-cx-fg-3",
            )}
            data-testid="composer-effort-reset"
          >
            <Icon name="retry" size={15} />
          </button>
        </Tooltip>
      </div>

      <div
        ref={trackRef}
        className="cx-effort-track relative mx-0.5 cursor-pointer touch-none select-none rounded-full"
        style={{ height: TRACK_H, "--effort-level": preview / span } as CSSProperties}
        onPointerDown={(event) => {
          if (event.button !== 0) return;
          event.preventDefault();
          event.currentTarget.setPointerCapture(event.pointerId);
          event.currentTarget.querySelector<HTMLElement>("[role='slider']")?.focus({ preventScroll: true });
          setDrag(fraction(event.clientX));
        }}
        onPointerMove={(event) => {
          if (drag !== null) setDrag(fraction(event.clientX));
        }}
        onPointerUp={(event) => {
          if (drag === null) return;
          commit(Math.round(fraction(event.clientX) * span));
          setDrag(null);
        }}
        onPointerCancel={() => setDrag(null)}
        onLostPointerCapture={() => setDrag(null)}
        data-dragging={dragging || undefined}
        data-max={isMax || undefined}
        data-testid="composer-effort-track"
      >
        <motion.span aria-hidden className="cx-effort-glow" style={{ width: fillWidth }} />
        <motion.span aria-hidden className="cx-effort-fill" style={{ clipPath: fillClip }}>
          <span className="cx-effort-beam" />
          {SPARKS.map((style, index) => <span key={index} className="cx-effort-spark" style={style} />)}
          <span className="cx-effort-sheen" />
        </motion.span>
        {stops.map((stop, index) => (
          <span
            key={stop || "default"}
            aria-hidden
            className={cn(
              "pointer-events-none absolute top-1/2 size-1 -translate-x-1/2 -translate-y-1/2 rounded-full transition-[background-color,opacity] duration-200",
              index < preview ? "bg-white/45" : "bg-cx-fg-4/55",
              index === preview && "opacity-0",
            )}
            style={{ left: along(index / span) }}
            data-effort-stop={stop || "default"}
          />
        ))}
        <motion.div
          role="slider"
          tabIndex={0}
          data-autofocus=""
          aria-label="思考强度"
          aria-valuemin={0}
          aria-valuemax={stops.length - 1}
          aria-valuenow={preview}
          aria-valuetext={labelOf(previewLevel)}
          onKeyDown={onKeyDown}
          className="cx-effort-thumb absolute top-1/2 rounded-full bg-white outline-none"
          style={{ left: thumbLeft, width: THUMB, height: THUMB, x: "-50%", y: "-50%" }}
          initial={false}
          animate={{ scale: dragging ? 1.08 : 1 }}
          transition={reduced ? INSTANT : SNAP}
          whileHover={reduced ? undefined : { scale: dragging ? 1.08 : 1.04 }}
          data-testid="composer-effort-thumb"
        />
      </div>

      <AnimatePresence initial={false}>
        {activeTier ? (
          <motion.p
            key="tier"
            initial={reduced ? { opacity: 0 } : { opacity: 0, y: -3 }}
            animate={{ opacity: 1, y: 0 }}
            exit={reduced ? { opacity: 0 } : { opacity: 0, y: -3 }}
            transition={{ duration: 0.14 }}
            className="-mt-1 flex items-center justify-center gap-1 truncate text-[11.5px] leading-4 text-cx-warning"
            data-testid="composer-speed-detail"
          >
            <Icon name="zap" size={11} className="shrink-0" />
            <span className="truncate">{serviceTierLabel(activeTier)} · {activeTier.description || "更快响应，用量更高"}</span>
          </motion.p>
        ) : null}
      </AnimatePresence>

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
              transition={{ duration: 0.65, ease: "easeOut" }}
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
