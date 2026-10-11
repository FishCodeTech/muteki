"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.


import { ChevronDown } from "lucide-react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import {
  memo,
  type ReactNode,
  type Ref,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { ThinkingShimmer } from "@/components/agentui/agents/loading-states/thinking-shimmer";
import { AgentDisclosure } from "@/components/agentui/agents/agent-disclosure";
import {
  EASE_OUT,
  SPRING_LAYOUT,
  SPRING_SWAP,
} from "@/components/agentui/lib/ease";
import { cn } from "@/lib/cn";
import { ActivityRow } from "./activity-row";
import type {
  AgentActivityContentType,
  AgentActivityItem,
  AgentActivityProps,
} from "./types";

export type {
  AgentActivityContentType,
  AgentActivityItem,
  AgentActivityProps,
  AgentActivitySearch,
  AgentActivityStatus,
  AgentActivityStep,
  AgentActivityText,
  AgentActivityTool,
  AgentActivityTrace,
  AgentSearchResult,
  AgentStepStatus,
  AgentTraceKind,
} from "./types";

// Rows kept mounted while a live list follows its newest entry. The live
// viewport is capped at a few hundred pixels (about 15 rows); the rest of a
// long run stays out of the DOM. Once the reader pauses, the window is frozen
// and slides a batch at a time as they scroll toward either edge, keeping at
// most PAUSED_WINDOW_ROWS mounted.
const LIVE_TAIL_ROWS = 24;
const REVEAL_STEP_ROWS = 24;
const REVEAL_MARGIN_PX = 320;
const PAUSED_WINDOW_ROWS = 96;

type ScrollAnchor = { id: string; offset: number };

function findRow(viewport: HTMLElement, id: string) {
  return viewport.querySelector<HTMLElement>(`[data-activity-id="${CSS.escape(id)}"]`);
}

function firstVisibleRow(viewport: HTMLElement): ScrollAnchor | null {
  const top = viewport.getBoundingClientRect().top;
  for (const row of viewport.querySelectorAll<HTMLElement>("[data-activity-id]")) {
    const rect = row.getBoundingClientRect();
    if (rect.bottom > top) return { id: row.dataset.activityId ?? "", offset: rect.top - top };
  }
  return null;
}

const ROW_ENTER = { opacity: 0, y: 6, filter: "blur(3px)" };
const ROW_ENTER_REDUCED = { opacity: 1 };
const ROW_SHOWN = { opacity: 1, y: 0, filter: "blur(0px)", transitionEnd: { filter: "none" } };
const ROW_EXIT = { opacity: 0, y: -3 };
const ROW_EXIT_REDUCED = { opacity: 0 };
const ROW_TRANSITION = {
  opacity: { duration: 0.18, ease: EASE_OUT },
  filter: { duration: 0.32, ease: EASE_OUT },
  y: SPRING_LAYOUT,
};
const ROW_TRANSITION_REDUCED = { duration: 0 };

// Memoized so a streaming list re-renders only the rows whose item changed;
// callers keep unchanged item objects stable across updates.
const ActivityListRow = memo(function ActivityListRow({
  item,
  reduce,
  exitable,
  enter,
  ref,
}: {
  item: AgentActivityItem;
  reduce: boolean;
  exitable: boolean;
  /** Rows revealed around a paused reader mount in place. */
  enter: boolean;
  ref?: Ref<HTMLDivElement>;
}) {
  // No `layout` projection here: a long tool list makes every re-render
  // snapshot and re-measure each row, which forces style recalculation per
  // row while a run is streaming.
  return (
    <motion.div
      ref={ref}
      role="listitem"
      data-activity-id={item.id}
      initial={!enter ? false : reduce ? ROW_ENTER_REDUCED : ROW_ENTER}
      animate={ROW_SHOWN}
      exit={exitable ? (reduce ? ROW_EXIT_REDUCED : ROW_EXIT) : undefined}
      transition={reduce ? ROW_TRANSITION_REDUCED : ROW_TRANSITION}
    >
      <ActivityRow item={item} />
    </motion.div>
  );
});

function formatDuration(duration: number) {
  const seconds = Math.max(0, Math.round(duration));
  if (seconds < 60) return `${seconds}秒`;

  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return remainder === 0 ? `${minutes}分` : `${minutes}分${remainder}秒`;
}

function useControllableOpen({
  open,
  defaultOpen,
  onOpenChange,
}: {
  open?: boolean;
  defaultOpen: boolean;
  onOpenChange?: (open: boolean) => void;
}) {
  const [internalOpen, setInternalOpen] = useState(defaultOpen);
  const controlled = open !== undefined;
  const currentOpen = open ?? internalOpen;

  const setOpen = useCallback(
    (next: boolean) => {
      if (!controlled) setInternalOpen(next);
      onOpenChange?.(next);
    },
    [controlled, onOpenChange],
  );

  return [currentOpen, setOpen] as const;
}

function getContentType(items: AgentActivityItem[]): AgentActivityContentType {
  const first = items[0]?.type;
  return first && items.every((item) => item.type === first) ? first : "mixed";
}

function getActiveLabel(type: AgentActivityContentType) {
  if (type === "search") return "正在搜索…";
  if (type === "tool") return "正在调用工具…";
  if (type === "trace" || type === "mixed" || type === "custom") return "正在处理…";
  return "思考中…";
}

function getSummary(
  type: AgentActivityContentType,
  items: AgentActivityItem[],
  duration: number,
): ReactNode {
  if (type === "step" || type === "text") {
    return (
      <>
        思考了 <span className="tabular-nums">{formatDuration(duration)}</span>
      </>
    );
  }
  if (type === "search") return "已搜索";
  if (type === "tool") return `调用了 ${items.length} 个工具`;
  if (type === "trace") {
    const messages = items.filter(
      (item) =>
        item.type === "trace" &&
        (item.kind === "thinking" || item.kind === "message"),
    ).length;
    return `${items.length - messages} 次工具调用 · ${messages} 条消息`;
  }
  return `完成 ${items.length} 个步骤`;
}

export function AgentActivity({
  items,
  contentType: initialContentType,
  status = "working",
  duration = 0,
  open,
  defaultOpen = false,
  onOpenChange,
  collapseOnComplete = true,
  activeLabel,
  summary,
  renderWorkingStatus,
  renderCompletedStatus,
  maxHeight = 208,
  completedMaxHeight,
  timeline = false,
  className,
  contentClassName,
}: AgentActivityProps) {
  const reduce = useReducedMotion() ?? false;
  const baseId = useId();
  const triggerId = `${baseId}-trigger`;
  const contentId = `${baseId}-content`;
  const viewportRef = useRef<HTMLDivElement>(null);
  const previousStatus = useRef(status);
  const [contentHeight, setContentHeight] = useState(0);
  const [liveCollapsed, setLiveCollapsed] = useState(false);
  const followLiveRef = useRef(true);
  const scrollGestureRef = useRef(false);
  const [followingLive, setFollowingLive] = useState(true);
  const [currentOpen, setOpen] = useControllableOpen({
    open,
    defaultOpen,
    onOpenChange,
  });
  const working = status === "working";
  const expanded = working ? !liveCollapsed : currentOpen;
  const contentType = items.length
    ? getContentType(items)
    : (initialContentType ?? "mixed");
  const limit = Math.max(0, working ? maxHeight : completedMaxHeight ?? maxHeight);
  const capped = contentHeight > limit;
  // While working, only a window ending at the newest row is mounted. A paused
  // window is frozen and anchored by item ids because the live event window
  // can drop rows from the head; rows appended meanwhile stay unmounted until
  // the reader scrolls toward them.
  const [pausedWindow, setPausedWindow] = useState<{ startId: string; endId: string } | null>(null);
  // Rows up to this id were already on screen (list mount, reopen, resume) and
  // mount without the entry animation.
  const [quietThroughId, setQuietThroughId] = useState<string | null>(() => items.at(-1)?.id ?? null);
  const indexOfId = (id: string | null) => (id === null ? -1 : items.findIndex((item) => item.id === id));
  let windowStart = 0;
  let windowEnd = items.length;
  if (working) {
    if (!followingLive && pausedWindow) {
      windowStart = Math.max(0, indexOfId(pausedWindow.startId));
      const end = indexOfId(pausedWindow.endId);
      windowEnd = end >= windowStart ? end + 1 : Math.min(items.length, windowStart + PAUSED_WINDOW_ROWS);
    } else {
      windowStart = Math.max(0, items.length - LIVE_TAIL_ROWS);
    }
  }
  const quietThrough = indexOfId(quietThroughId);
  const renderedItems = windowStart || windowEnd < items.length ? items.slice(windowStart, windowEnd) : items;
  const itemsRef = useRef(items);
  itemsRef.current = items;
  const windowRef = useRef({ start: windowStart, end: windowEnd });
  windowRef.current = { start: windowStart, end: windowEnd };
  const scrollAnchorRef = useRef<ScrollAnchor | null>(null);

  const followLive = useCallback((next: boolean) => {
    const current = itemsRef.current;
    if (!next && followLiveRef.current) {
      const start = current[Math.max(0, current.length - LIVE_TAIL_ROWS)];
      const end = current[current.length - 1];
      setPausedWindow(start && end ? { startId: start.id, endId: end.id } : null);
    } else if (next && !followLiveRef.current) {
      setPausedWindow(null);
      setQuietThroughId(current.at(-1)?.id ?? null);
    }
    followLiveRef.current = next;
    setFollowingLive(next);
  }, []);

  // Slides the paused window when the reader nears either edge. Rows dropped
  // from the opposite edge keep the window bounded; the first visible row is
  // pinned so the reader does not move.
  const slidePausedWindow = (viewport: HTMLDivElement) => {
    if (followLiveRef.current || scrollAnchorRef.current) return;
    const current = itemsRef.current;
    const { start, end } = windowRef.current;
    const fromBottom = viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop;
    let nextStart = start;
    let nextEnd = end;
    if (viewport.scrollTop <= REVEAL_MARGIN_PX && start > 0) {
      nextStart = Math.max(0, start - REVEAL_STEP_ROWS);
      nextEnd = Math.min(end, nextStart + PAUSED_WINDOW_ROWS);
    } else if (fromBottom <= REVEAL_MARGIN_PX && end < current.length) {
      nextEnd = Math.min(current.length, end + REVEAL_STEP_ROWS);
      nextStart = Math.max(start, nextEnd - PAUSED_WINDOW_ROWS);
    } else {
      return;
    }
    const startItem = current[nextStart];
    const endItem = current[nextEnd - 1];
    if (!startItem || !endItem) return;
    scrollAnchorRef.current = firstVisibleRow(viewport);
    setPausedWindow({ startId: startItem.id, endId: endItem.id });
  };

  const pauseLive = () => {
    if (working) followLive(false);
  };

  // A paused window that still fits has no scroll range to slide and no
  // follow control, so freezing its end would hide every later row until the
  // list remounts. Keep it ending at the newest row until the list overflows;
  // from then on the reader's position is preserved as usual.
  const newestId = items.at(-1)?.id ?? null;
  useLayoutEffect(() => {
    if (!working || followingLive || capped || !pausedWindow || !newestId) return;
    if (pausedWindow.endId === newestId) return;
    setPausedWindow({ startId: pausedWindow.startId, endId: newestId });
  }, [capped, followingLive, newestId, pausedWindow, working]);

  // The viewport is sized by CSS max-height; the observed height only decides
  // whether the list is capped. Reading it from the observer entry avoids a
  // forced synchronous layout per instance when a long thread mounts. The list
  // unmounts while collapsed, so the observer follows the node via a ref callback.
  const observeContent = useCallback((node: HTMLDivElement | null) => {
    if (!node || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      const entry = entries[entries.length - 1];
      const size = entry?.borderBoxSize?.[0]?.blockSize;
      setContentHeight(size ?? node.offsetHeight);
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (previousStatus.current === "working" && status === "complete") {
      setOpen(!collapseOnComplete);
    }
    if (previousStatus.current !== status) {
      setLiveCollapsed(false);
      followLive(true);
      scrollGestureRef.current = false;
    }
    previousStatus.current = status;
  }, [collapseOnComplete, followLive, setOpen, status]);

  // Use the native scroll range, not a translated/clipped list. Pausing before
  // a disclosure opens keeps its added height from dragging the reader away.
  useLayoutEffect(() => {
    const viewport = viewportRef.current;
    const anchor = scrollAnchorRef.current;
    scrollAnchorRef.current = null;
    if (!viewport) return;
    if (anchor && working && !followLiveRef.current) {
      const row = findRow(viewport, anchor.id);
      if (row) {
        const offset = row.getBoundingClientRect().top - viewport.getBoundingClientRect().top;
        viewport.scrollTop += offset - anchor.offset;
      }
      return;
    }
    if (working && expanded && followLiveRef.current) {
      viewport.scrollTop = viewport.scrollHeight;
    }
  }, [contentHeight, expanded, working, windowStart, windowEnd]);

  const toggle = () => {
    if (working) {
      // Reopening remounts the rows already seen; they should not animate in.
      if (liveCollapsed) setQuietThroughId(items.at(-1)?.id ?? null);
      setLiveCollapsed((collapsed) => !collapsed);
      return;
    }
    const next = !currentOpen;
    setOpen(next);
    if (next) requestAnimationFrame(() => viewportRef.current?.scrollTo({ top: 0 }));
  };

  const liveLabel = activeLabel ?? getActiveLabel(contentType);
  const completedSummary = summary ?? getSummary(contentType, items, duration);
  const maskImage = capped && (!working || followingLive)
    ? working
      ? "linear-gradient(to bottom, transparent, black 12px)"
      : "linear-gradient(to bottom, transparent, black 12px, black calc(100% - 12px), transparent)"
    : undefined;

  return (
    <div
      data-state={working ? "working" : expanded ? "open" : "closed"}
      data-content={contentType}
      data-following={working ? followingLive : undefined}
      aria-busy={working}
      className={cn("w-full text-sm", className)}
    >
      <button
        id={triggerId}
        type="button"
        aria-expanded={expanded}
        aria-controls={contentId}
        onClick={toggle}
        className={cn(
          "group flex h-7 min-w-0 max-w-full items-center gap-1 rounded-lg pl-1.5 pr-2 text-left text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg-2 focus-visible:ring-2 focus-visible:ring-cx-focus focus-visible:ring-offset-2 focus-visible:ring-offset-cx-bg",
          !working && "font-medium",
        )}
      >
        <motion.span
          aria-hidden="true"
          initial={false}
          animate={{ rotate: expanded ? 0 : -90 }}
          transition={reduce ? { duration: 0 } : SPRING_SWAP}
          className="inline-flex size-4 shrink-0 items-center justify-center text-cx-fg-3/70 group-hover:text-cx-fg-2"
        >
          <ChevronDown className="size-3.5" />
        </motion.span>
        <span className="min-w-0 truncate">
          {working
            ? renderWorkingStatus
              ? renderWorkingStatus({ label: liveLabel, duration })
              : <ThinkingShimmer>{liveLabel}</ThinkingShimmer>
            : renderCompletedStatus
              ? renderCompletedStatus({ summary: completedSummary, duration })
              : completedSummary}
        </span>
      </button>
      {working ? (
        <span role="status" className="sr-only">{liveLabel}</span>
      ) : null}

      <AgentDisclosure
        id={contentId}
        role="region"
        aria-labelledby={triggerId}
        open={expanded}
        className="relative"
      >
        <div
          ref={viewportRef}
          data-testid="activity-scroll-viewport"
          tabIndex={capped && expanded ? 0 : undefined}
          className={cn(
            "cx-scroll overflow-x-hidden pr-1 outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-cx-focus",
            capped && expanded ? "overflow-y-auto" : "overflow-y-hidden",
          )}
          style={{ maxHeight: limit, maskImage, WebkitMaskImage: maskImage, overflowAnchor: "none" }}
          onPointerDownCapture={(event) => {
            pauseLive();
            scrollGestureRef.current = event.target === event.currentTarget;
          }}
          onClickCapture={() => {
            pauseLive();
            scrollGestureRef.current = false;
          }}
          onWheel={(event) => {
            scrollGestureRef.current = true;
            if (event.deltaY < 0) pauseLive();
          }}
          onTouchStart={() => {
            pauseLive();
            scrollGestureRef.current = true;
          }}
          onKeyDownCapture={(event) => {
            if (["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " "].includes(event.key)) {
              pauseLive();
              scrollGestureRef.current = true;
            }
          }}
          onScroll={(event) => {
            if (!working) return;
            const viewport = event.currentTarget;
            slidePausedWindow(viewport);
            if (!scrollGestureRef.current) return;
            const atBottom = viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop <= 8
              && windowRef.current.end >= itemsRef.current.length;
            // Layout changes may also dispatch scroll. Only a reader's scroll
            // gesture may resume following; a disclosure click must not.
            if (atBottom) scrollGestureRef.current = false;
            followLive(atBottom);
          }}
        >
          <div
            ref={observeContent}
            role="list"
            className={cn("space-y-0.5 py-2", timeline && "cx-activity-timeline", contentClassName)}
          >
            {/* While live, rows leave the window without an exit animation, so
                the list skips AnimatePresence: it would re-render every row on
                each append, and popLayout would force a layout per append.
                Only rows appended while following animate in. Settling
                remounts the list with exit animations. */}
            {working ? (
              renderedItems.map((item, index) => (
                <ActivityListRow
                  key={item.id}
                  item={item}
                  reduce={reduce}
                  exitable={false}
                  enter={followingLive && windowStart + index > quietThrough}
                />
              ))
            ) : (
              <AnimatePresence key="all" initial={false} mode="popLayout" presenceAffectsLayout={false}>
                {renderedItems.map((item) => (
                  <ActivityListRow key={item.id} item={item} reduce={reduce} exitable enter />
                ))}
              </AnimatePresence>
            )}
          </div>
        </div>
        {working && capped && !followingLive ? (
          <button
            type="button"
            onClick={() => {
              followLive(true);
              scrollGestureRef.current = false;
              const viewport = viewportRef.current;
              if (viewport) viewport.scrollTop = viewport.scrollHeight;
            }}
            className="absolute bottom-2 right-3 inline-flex h-7 items-center gap-1 rounded-full border border-cx-border bg-cx-elevated px-2.5 text-[12px] text-cx-fg-2 shadow-cx-sm hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-cx-focus"
          >
            <ChevronDown className="size-3.5" aria-hidden />
            跟随最新进展
          </button>
        ) : null}
      </AgentDisclosure>
    </div>
  );
}
