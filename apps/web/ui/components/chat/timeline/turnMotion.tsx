"use client";

import { AnimatePresence, motion, type Variants } from "motion/react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";
import { endsInLinkTarget, nextWordEnd, visibleLength } from "@/lib/rehypeStreamWords";
import { EASE_OUT, SPRING_SWAP, useReducedMotion } from "@/components/chat/ui/motion";

/* Motion primitives for a turn's live work log: label swaps, entry tracking,
   counters, a gliding stick-to-bottom viewport and paced streaming text. */

const ROLL_BLUR = "blur(2px)";

const ROLL_VARIANTS: Variants = {
  initial: { opacity: 0, y: "85%", filter: ROLL_BLUR },
  animate: {
    opacity: 1,
    y: "0%",
    filter: "blur(0px)",
    transitionEnd: { filter: "none" },
    transition: { y: SPRING_SWAP, opacity: { duration: 0.18, ease: EASE_OUT }, filter: { duration: 0.22, ease: EASE_OUT } },
  },
  exit: { opacity: 0, y: "-85%", filter: ROLL_BLUR, transition: { duration: 0.16, ease: EASE_OUT } },
};

type SwapStage = "static" | "arming" | "live";

/**
 * Lists hold many labels that never change, so the presence tree mounts only
 * after the first change. One layout pass renders the previous value inside
 * the tree, then the new value replaces it before paint, so even the first
 * swap rolls.
 */
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

/**
 * A one-line label that rolls (old up and out, new up and in) whenever
 * `value` changes. The slot takes the new label's width at once and clips
 * vertically, so neighbours never jump mid-roll.
 */
export function SwapText({ value, children, className }: { value: string; children: ReactNode; className?: string }) {
  const reduced = useReducedMotion();
  const swap = useSwapStage(value, children, !reduced);
  return (
    <span
      className={cn("relative inline-block min-w-0 max-w-full align-bottom", className)}
      // overflow-y: clip keeps the rolling label out of the scroll height too; clip-path alone only hides it.
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

/** Long enough for a landing row's entrance to finish. */
const FRESH_WINDOW_MS = 1200;

/**
 * True for keys that appear after the first render, so only work that
 * arrives while the reader watches animates in; history mounts quietly.
 * A key stays fresh only while its entrance plays: a row that remounts later
 * (its group folded and reopened) must not replay it.
 */
export function useFreshKeys(keys: readonly string[], enabled: boolean): (key: string) => boolean {
  const known = useRef<Set<string> | null>(null);
  if (known.current === null) known.current = new Set(keys);
  const fresh = useRef(new Map<string, number>());
  const now = Date.now();
  for (const key of keys) {
    if (known.current.has(key) || fresh.current.has(key)) continue;
    // A class added to an already mounted node would still start its animation.
    if (enabled) fresh.current.set(key, now);
    else known.current.add(key);
  }
  return useCallback((key: string) => {
    const since = fresh.current.get(key);
    return since !== undefined && Date.now() - since < FRESH_WINDOW_MS;
  }, []);
}

/** Remembers whether a value was ever true during this component's life. */
export function useEver(value: boolean): boolean {
  const ever = useRef(value);
  if (value) ever.current = true;
  return ever.current;
}

/**
 * A number that counts to each new value. It writes its text node directly,
 * so the count never re-renders the row it sits in.
 */
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

/**
 * Keeps a scroll container glued to its newest content while `active`.
 * Growth glides the view down instead of snapping; scrolling up releases it
 * and returning to the bottom re-attaches.
 */
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

/** At most one word lands per 60 Hz frame, whatever the display's refresh rate. */
const WORDS_PER_SECOND = 60;
/** A backlog this many characters deep adds one more word per frame, so a large burst still drains. */
const CATCH_UP_CHARS = 240;
/** While streaming, the reveal trails the received text by about this long, so gaps between deltas are absorbed. */
const LAG_SECONDS = 0.35;
/** Keeps the last few characters of a backlog from crawling. */
const MIN_CHARS_PER_SECOND = 24;

/**
 * Paces streamed text into a steady flow of whole words, each fading in as a
 * unit so the newest dozen overlap into one soft leading edge. While the
 * stream is live, the reveal speed tracks the backlog (backlog / lag), so
 * words keep landing one at a time at the rate text arrives instead of
 * draining each delta in a burst and then stalling. Once the stream ends the
 * rest lands one word per frame. A word still arriving at the end of the
 * stream waits for its last character. Text that is not streaming passes
 * straight through, and a rewrite that is not an extension jumps to the new
 * text.
 */
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
    // The first word of a fresh run lands on the next frame instead of waiting out a full interval.
    current.slots = Math.max(current.slots, 1);
    current.budget = Math.max(current.budget, visibleLength(text, current.length, nextWordEnd(text, current.length)));
    const step = (now: number) => {
      const s = state.current;
      const dt = Math.max(0, Math.min(0.1, (now - s.last) / 1000));
      s.last = now;
      const backlog = s.text.length - s.length;
      const perFrame = 1 + Math.floor(backlog / CATCH_UP_CHARS);
      s.slots = Math.min(perFrame, s.slots + dt * WORDS_PER_SECOND * perFrame);
      // Pace by what will show: link addresses and table rules would otherwise rush the words before them.
      const visible = s.streaming ? visibleLength(s.text, s.length, s.text.length) : backlog;
      s.budget = Math.min(visible, s.budget + dt * Math.max(MIN_CHARS_PER_SECOND, visible / LAG_SECONDS));
      let next = s.length;
      let waiting = false;
      while (s.slots >= 1 && next < s.text.length) {
        const end = nextWordEnd(s.text, next);
        if (s.streaming && end >= s.text.length && !/\s$/.test(s.text) && !endsInLinkTarget(s.text)) {
          waiting = true;
          break;
        }
        const cost = visibleLength(s.text, next, end);
        if (s.streaming && s.budget < cost) break;
        s.budget = Math.max(0, s.budget - cost);
        next = end;
        s.slots -= 1;
      }
      if (next !== s.length) {
        s.length = next;
        setLength(next);
      }
      if (next >= s.text.length || waiting) {
        // Idle until the next delta; a stall must not bank words for a burst afterwards.
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
    // Zero the handle too: a remounted effect must be free to start a new loop.
    cancelAnimationFrame(state.current.frame);
    state.current.frame = 0;
  }, []);

  return extends_ ? shownText : text;
}
/** 3×3 dot matrix that ripples while the agent works. */
export function WorkingGlyph({ paused = false, className }: { paused?: boolean; className?: string }) {
  return (
    <span aria-hidden className={cn("cx-working-glyph", className)} data-paused={paused || undefined}>
      {Array.from({ length: 9 }, (_, index) => <i key={index} />)}
    </span>
  );
}
