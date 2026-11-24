"use client";

import { AnimatePresence, motion, type Variants } from "motion/react";
import {
  startTransition,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import { endsInLinkTarget, nextWordEnd, visibleLength } from "@/lib/rehypeStreamWords";
import { EASE_OUT, SPRING_SWAP, useReducedMotion } from "@muteki/ui";


const ROLL_VARIANTS: Variants = {
  initial: { opacity: 0, transform: "translateY(40%)" },
  animate: {
    opacity: 1,
    transform: "translateY(0%)",
    transition: { transform: SPRING_SWAP, opacity: { duration: 0.18, ease: EASE_OUT } },
  },
  exit: { opacity: 0, transform: "translateY(-40%)", transition: { duration: 0.16, ease: EASE_OUT } },
};

type SwapStage = "static" | "arming" | "live";

function useSwapStage(value: string, children: ReactNode, animated: boolean) {
  const shown = useRef({ value, children });
  const [stage, setStage] = useState<SwapStage>("static");
  if (stage === "static" && animated && value !== shown.current.value) setStage("arming");
  useLayoutEffect(() => {
    if (stage === "arming") setStage("live");
  }, [stage]);
  useLayoutEffect(() => {
    if (stage === "static") shown.current = { value, children };
  });
  return stage === "arming" ? { stage, ...shown.current } : { stage, value, children };
}

export function SwapText({ value, children, className }: { value: string; children: ReactNode; className?: string }) {
  const reduced = useReducedMotion();
  const swap = useSwapStage(value, children, !reduced);
  return (
    <span
      className={cn("relative inline-block min-w-0 max-w-full align-bottom", className)}
      style={{ clipPath: "inset(-1px -999px)", overflowY: "clip" }}
    >
      <span aria-hidden className="invisible block truncate">{children}</span>
      {swap.stage === "static" ? (
        <span className="absolute inset-x-0 top-0 block truncate">{children}</span>
      ) : (
        <AnimatePresence initial={false}>
          <motion.span
            key={swap.value}
            variants={ROLL_VARIANTS}
            initial={reduced ? false : "initial"}
            animate="animate"
            exit={reduced ? undefined : "exit"}
            className="absolute inset-x-0 top-0 block truncate"
          >
            {swap.children}
          </motion.span>
        </AnimatePresence>
      )}
    </span>
  );
}

const FRESH_WINDOW_MS = 1200;

export function useFreshKeys(keys: readonly string[], enabled: boolean): (key: string) => boolean {
  const seen = useRef<Map<string, number> | null>(null);
  const previous = seen.current;
  const current = new Map<string, number>();
  const now = Date.now();
  for (const key of keys) {
    current.set(key, previous?.get(key) ?? (previous && enabled ? now : 0));
  }
  seen.current = current;
  return useCallback((key: string) => {
    const since = seen.current?.get(key);
    return Boolean(since && Date.now() - since < FRESH_WINDOW_MS);
  }, []);
}

export function useEver(value: boolean): boolean {
  const ever = useRef(value);
  if (value) ever.current = true;
  return ever.current;
}

export function CountUp({ value, from, prefix = "", className }: { value: number; from?: number; prefix?: string; className?: string }) {
  const reduced = useReducedMotion();
  const ref = useRef<HTMLSpanElement>(null);
  const [initial] = useState(() => from ?? value);
  const shown = useRef(initial);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const start = shown.current;
    const end = value;
    if (start === end || reduced) {
      shown.current = end;
      el.textContent = `${prefix}${end}`;
      return;
    }
    const startedAt = performance.now();
    const duration = Math.min(900, 320 + Math.abs(end - start) * 18);
    let frame = 0;
    const tick = (now: number) => {
      const progress = Math.min(1, (now - startedAt) / duration);
      const eased = 1 - (1 - progress) ** 3;
      shown.current = Math.round(start + (end - start) * eased);
      el.textContent = `${prefix}${shown.current}`;
      if (progress < 1) frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => {
      cancelAnimationFrame(frame);
      shown.current = end;
    };
  }, [value, prefix, reduced]);
  return <span ref={ref} className={cn("cx-tabular", className)}>{`${prefix}${initial}`}</span>;
}

export function useStickToBottom<T extends HTMLElement = HTMLDivElement>(active: boolean) {
  const reduced = useReducedMotion();
  const [node, setNode] = useState<T | null>(null);
  useEffect(() => {
    if (!node || !active) return;
    let pinned = true;
    let frame = 0;
    const settle = () => {
      cancelAnimationFrame(frame);
      const step = () => {
        if (!pinned) return;
        const target = node.scrollHeight - node.clientHeight;
        const gap = target - node.scrollTop;
        if (gap <= 0.5 || reduced) {
          node.scrollTop = target;
          return;
        }
        node.scrollTop += Math.max(1, gap * 0.2);
        frame = requestAnimationFrame(step);
      };
      frame = requestAnimationFrame(step);
    };
    node.scrollTop = node.scrollHeight;
    const release = () => { pinned = false; cancelAnimationFrame(frame); };
    const onWheel = (event: WheelEvent) => { if (event.deltaY < 0) release(); };
    const onScroll = () => {
      if (node.scrollHeight - node.clientHeight - node.scrollTop <= 4) pinned = true;
    };
    node.addEventListener("wheel", onWheel, { passive: true });
    node.addEventListener("touchstart", release, { passive: true });
    node.addEventListener("scroll", onScroll, { passive: true });
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(settle);
    observer?.observe(node);
    for (const child of Array.from(node.children)) observer?.observe(child);
    const mutations = new MutationObserver(() => {
      for (const child of Array.from(node.children)) observer?.observe(child);
      settle();
    });
    mutations.observe(node, { childList: true });
    return () => {
      cancelAnimationFrame(frame);
      observer?.disconnect();
      mutations.disconnect();
      node.removeEventListener("wheel", onWheel);
      node.removeEventListener("touchstart", release);
      node.removeEventListener("scroll", onScroll);
    };
  }, [node, active, reduced]);
  return setNode;
}

const WORDS_PER_SECOND = 60;
const CATCH_UP_CHARS = 240;
const LAG_SECONDS = 0.35;
const MIN_CHARS_PER_SECOND = 24;

export function useSmoothText(text: string, streaming: boolean): string {
  const reduced = useReducedMotion();
  const [length, setLength] = useState(() => (streaming && !reduced ? 0 : text.length));
  const state = useRef({ length, slots: 0, budget: 0, text, streaming, frame: 0, last: 0 });
  state.current.text = text;
  state.current.streaming = streaming;
  const everStreamed = useEver(streaming);

  const shownText = text.slice(0, Math.min(length, text.length));
  const extends_ = text.startsWith(shownText);

  useEffect(() => {
    const current = state.current;
    if (reduced || !extends_ || (!everStreamed && !current.frame)) {
      cancelAnimationFrame(current.frame);
      current.frame = 0;
      current.length = text.length;
      setLength(text.length);
      return;
    }
    if (current.length >= text.length || current.frame) return;
    current.last = performance.now();
    current.slots = Math.max(current.slots, 1);
    const firstEnd = nextWordEnd(text, current.length);
    let word = { text, from: current.length, end: firstEnd, cost: visibleLength(text, current.length, firstEnd) };
    current.budget = Math.max(current.budget, word.cost);
    let remaining = { text: "", from: -1, visible: 0 };
    const step = (now: number) => {
      const s = state.current;
      const dt = Math.max(0, Math.min(0.1, (now - s.last) / 1000));
      s.last = now;
      const backlog = s.text.length - s.length;
      const perFrame = 1 + Math.floor(backlog / CATCH_UP_CHARS);
      s.slots = Math.min(perFrame, s.slots + dt * WORDS_PER_SECOND * perFrame);
      let visible = backlog;
      if (s.streaming) {
        if (remaining.text !== s.text || remaining.from !== s.length) {
          remaining = { text: s.text, from: s.length, visible: visibleLength(s.text, s.length, s.text.length) };
        }
        visible = remaining.visible;
      }
      s.budget = Math.min(visible, s.budget + dt * Math.max(MIN_CHARS_PER_SECOND, visible / LAG_SECONDS));
      let next = s.length;
      let waiting = false;
      while (s.slots >= 1 && next < s.text.length) {
        if (word.text !== s.text || word.from !== next) {
          const end = nextWordEnd(s.text, next);
          word = { text: s.text, from: next, end, cost: visibleLength(s.text, next, end) };
        }
        const { end, cost } = word;
        if (s.streaming && end >= s.text.length && !/\s$/.test(s.text) && !endsInLinkTarget(s.text)) {
          waiting = true;
          break;
        }
        if (s.streaming && s.budget < cost) break;
        s.budget = Math.max(0, s.budget - cost);
        next = end;
        s.slots -= 1;
      }
      if (next !== s.length) {
        s.length = next;
        startTransition(() => setLength(next));
      }
      if (next >= s.text.length || waiting) {
        s.slots = 0;
        s.budget = 0;
        s.frame = 0;
        return;
      }
      s.frame = requestAnimationFrame(step);
    };
    current.frame = requestAnimationFrame(step);
  }, [text, streaming, reduced, extends_, everStreamed]);

  useEffect(() => () => {
    cancelAnimationFrame(state.current.frame);
    state.current.frame = 0;
  }, []);

  return extends_ ? shownText : text;
}
export function WorkingGlyph({ paused = false, className }: { paused?: boolean; className?: string }) {
  return (
    <span aria-hidden className={cn("cx-working-glyph", className)} data-paused={paused || undefined}>
      {Array.from({ length: 9 }, (_, index) => <i key={index} />)}
    </span>
  );
}
