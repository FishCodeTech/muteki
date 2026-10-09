"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.


import { ChevronDown } from "lucide-react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import {
  type ReactNode,
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
  const contentRef = useRef<HTMLDivElement>(null);
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
  const cappedHeight = Math.min(contentHeight, limit);
  const viewportHeight = working ? Math.min(contentHeight, limit) : cappedHeight;
  const capped = contentHeight > limit;

  const followLive = useCallback((next: boolean) => {
    followLiveRef.current = next;
    setFollowingLive(next);
  }, []);

  const pauseLive = () => {
    if (working) followLive(false);
  };

  useLayoutEffect(() => {
    const node = contentRef.current;
    if (!node) return;

    const measure = () => setContentHeight(node.offsetHeight);
    measure();

    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
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
    if (viewport && working && expanded && followLiveRef.current) {
      viewport.scrollTop = viewport.scrollHeight;
    }
  }, [contentHeight, expanded, viewportHeight, working]);

  const toggle = () => {
    if (working) {
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
        openHeight={viewportHeight}
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
          style={{ height: viewportHeight, maskImage, WebkitMaskImage: maskImage, overflowAnchor: "none" }}
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
            if (!working || !scrollGestureRef.current) return;
            const viewport = event.currentTarget;
            const atBottom = viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop <= 8;
            // Layout changes may also dispatch scroll. Only a reader's scroll
            // gesture may resume following; a disclosure click must not.
            if (atBottom) scrollGestureRef.current = false;
            followLive(atBottom);
          }}
        >
          <motion.div
            ref={contentRef}
            role="list"
            className={cn("space-y-0.5 py-2", timeline && "cx-activity-timeline", contentClassName)}
          >
            <AnimatePresence mode="popLayout">
              {items.map((item) => (
                <motion.div
                  layout="position"
                  key={item.id}
                  role="listitem"
                  initial={reduce ? { opacity: 1 } : { opacity: 0, y: 6, filter: "blur(3px)" }}
                  animate={{ opacity: 1, y: 0, filter: "blur(0px)", transitionEnd: { filter: "none" } }}
                  exit={reduce ? { opacity: 0 } : { opacity: 0, y: -3 }}
                  transition={
                    reduce
                      ? { duration: 0 }
                      : {
                          opacity: { duration: 0.18, ease: EASE_OUT },
                          filter: { duration: 0.32, ease: EASE_OUT },
                          y: SPRING_LAYOUT,
                          layout: SPRING_LAYOUT,
                        }
                  }
                >
                  <ActivityRow item={item} />
                </motion.div>
              ))}
            </AnimatePresence>
          </motion.div>
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
