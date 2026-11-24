"use client";

import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";
import { Icon } from "../icons/Icon";
import { ActionSwapRollText } from "../primitives/ActionSwap";
import { AgentDisclosure } from "./AgentDisclosure";
import { EASE_OUT, SPRING_LAYOUT, SPRING_SWAP } from "../primitives/motion";

export type TodoItemStatus = "pending" | "in-progress" | "completed" | "cancelled";

export interface TodoItem {
  id: string;
  title: ReactNode;
  status?: TodoItemStatus;
  progress?: number;
  detail?: ReactNode;
}

export interface TodoListProps {
  items: TodoItem[];
  title?: ReactNode;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  collapseOnComplete?: boolean;
  maxHeight?: number;
  className?: string;
}

function statusLabel(status: TodoItemStatus) {
  if (status === "in-progress") return "进行中";
  if (status === "completed") return "已完成";
  if (status === "cancelled") return "已取消";
  return "待处理";
}

function TodoHeaderIcon({ complete }: { complete: boolean }) {
  const reduced = useReducedMotion();

  return (
    <span aria-hidden="true" className="relative grid size-6 shrink-0 place-items-center">
      <AnimatePresence initial={false} mode="popLayout">
        {complete ? (
          <motion.svg
            key="complete"
            viewBox="0 0 24 24"
            initial={reduced ? { opacity: 1 } : { opacity: 0, transform: "scale(0.8)" }}
            animate={{ opacity: 1, transform: "scale(1)" }}
            exit={{ opacity: 0 }}
            transition={reduced ? { duration: 0 } : SPRING_SWAP}
            className="absolute size-5.5 overflow-visible text-cx-success"
          >
            <circle cx="12" cy="12" r="9" fill="currentColor" />
            <motion.path
              d="M7.5 12.25 10.5 15.25 16.75 8.75"
              fill="none"
              stroke="white"
              strokeWidth="2.25"
              strokeLinecap="round"
              strokeLinejoin="round"
              initial={reduced ? { pathLength: 1 } : { pathLength: 0 }}
              animate={{ pathLength: 1 }}
              transition={reduced ? { duration: 0 } : { duration: 0.24, ease: EASE_OUT }}
            />
          </motion.svg>
        ) : (
          <motion.span
            key="todo"
            initial={reduced ? { opacity: 1 } : { opacity: 0, transform: "scale(0.8)" }}
            animate={{ opacity: 1, transform: "scale(1)" }}
            exit={reduced ? { opacity: 0 } : { opacity: 0, transform: "scale(0.8)" }}
            transition={reduced ? { duration: 0 } : SPRING_SWAP}
            className="absolute grid place-items-center text-cx-fg-3"
          >
            <Icon name="listTodo" size={16} />
          </motion.span>
        )}
      </AnimatePresence>
    </span>
  );
}

export function TodoStatusIcon({
  status,
  progress,
}: {
  status: TodoItemStatus;
  progress?: number;
}) {
  const reduced = useReducedMotion();
  const normalizedProgress =
    progress === undefined ? 0.68 : Math.min(100, Math.max(0, progress)) / 100;
  const spinning = status === "in-progress" && progress === undefined && !reduced;

  return (
    <motion.svg
      aria-hidden="true"
      viewBox="0 0 24 24"
      initial={false}
      className={cn(
        "mx-0.5 size-5 shrink-0 overflow-visible text-cx-fg-3",
        status === "in-progress" && "text-cx-fg",
        status === "cancelled" && "text-cx-danger",
      )}
    >
      <motion.circle
        cx="12"
        cy="12"
        r="9"
        fill="currentColor"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeDasharray={status === "pending" ? "2 3" : undefined}
        strokeLinecap="round"
        initial={false}
        animate={{ fillOpacity: status === "completed" ? 0.06 : 0 }}
        transition={reduced ? { duration: 0 } : { duration: 0.18, ease: EASE_OUT }}
        className={cn(status === "in-progress" && "opacity-20")}
      />
      <g className={cn(spinning && "cx-spin")} style={{ transformOrigin: "12px 12px" }}>
        <motion.circle
          cx="12"
          cy="12"
          r="9"
          pathLength="1"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          transform="rotate(-90 12 12)"
          initial={false}
          animate={{
            pathLength: status === "in-progress" ? normalizedProgress : 0,
            opacity: status === "in-progress" ? 1 : 0,
          }}
          transition={reduced ? { duration: 0 } : SPRING_LAYOUT}
        />
      </g>
      <motion.path
        d="M7.5 12.25 10.5 15.25 16.75 8.75"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
        initial={false}
        animate={{
          pathLength: status === "completed" ? 1 : 0,
          opacity: status === "completed" ? 1 : 0,
        }}
        transition={reduced ? { duration: 0 } : { duration: 0.24, ease: EASE_OUT }}
      />
      <motion.path
        d="M8.5 8.5 15.5 15.5M15.5 8.5 8.5 15.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        initial={false}
        animate={{
          pathLength: status === "cancelled" ? 1 : 0,
          opacity: status === "cancelled" ? 1 : 0,
        }}
        transition={reduced ? { duration: 0 } : { duration: 0.2, ease: EASE_OUT }}
      />
    </motion.svg>
  );
}

export function TodoList({
  items,
  title = "待办",
  open,
  defaultOpen = true,
  onOpenChange,
  collapseOnComplete = true,
  maxHeight = 248,
  className,
}: TodoListProps) {
  const reduced = useReducedMotion();
  const baseId = useId();
  const triggerId = `${baseId}-trigger`;
  const contentId = `${baseId}-content`;
  const viewportRef = useRef<HTMLDivElement>(null);
  const previousComplete = useRef(false);
  const hasAutoScrolledRef = useRef(false);
  const [internalOpen, setInternalOpen] = useState(defaultOpen);
  const currentOpen = open ?? internalOpen;
  const completed = items.filter((item) => item.status === "completed").length;
  const allComplete = items.length > 0 && completed === items.length;
  const itemCount = items.length;

  const setOpen = useCallback(
    (next: boolean) => {
      if (open === undefined) setInternalOpen(next);
      onOpenChange?.(next);
    },
    [onOpenChange, open],
  );

  useEffect(() => {
    if (previousComplete.current && !allComplete) setOpen(true);
    if (!previousComplete.current && allComplete && collapseOnComplete) setOpen(false);
    previousComplete.current = allComplete;
  }, [allComplete, collapseOnComplete, setOpen]);

  useLayoutEffect(() => {
    const viewport = viewportRef.current;
    if (!viewport || !currentOpen || itemCount === 0) {
      if (!currentOpen || itemCount === 0) hasAutoScrolledRef.current = false;
      return;
    }
    const frame = requestAnimationFrame(() => {
      const shouldSmooth = !hasAutoScrolledRef.current && !reduced;
      hasAutoScrolledRef.current = true;
      if (viewport.scrollHeight <= viewport.clientHeight) return;
      if (shouldSmooth && typeof viewport.scrollTo === "function") {
        viewport.scrollTo({ top: viewport.scrollHeight, behavior: "smooth" });
      } else {
        viewport.scrollTop = viewport.scrollHeight;
      }
    });
    return () => cancelAnimationFrame(frame);
  }, [currentOpen, itemCount, reduced]);

  return (
    <section
      data-slot="todo-list"
      aria-label="任务清单"
      className={cn(
        "ui-surface-ring w-full overflow-hidden rounded-[12px] bg-cx-elevated",
        className,
      )}
    >
      <button
        id={triggerId}
        type="button"
        aria-expanded={currentOpen}
        aria-controls={contentId}
        onClick={() => setOpen(!currentOpen)}
        className="group flex h-11 w-full items-center gap-2.5 rounded-[12px] px-3.5 text-left outline-none focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--cx-focus)]"
      >
        <TodoHeaderIcon complete={allComplete} />
        <h3 className="min-w-0 flex-1 truncate text-cx-sm font-medium text-cx-fg/90">{title}</h3>
        <span className={cn("cx-tabular shrink-0 text-cx-xs font-medium text-cx-fg-3", allComplete && "text-cx-success")}>
          <span className="sr-only">已完成 {completed}/{items.length} 项</span>
          <span aria-hidden="true" className="inline-flex">
            <ActionSwapRollText value={completed}>{completed}</ActionSwapRollText>
            <span>/</span>
            <span>{items.length}</span>
          </span>
        </span>
        <Icon
          name="chevronDown"
          size={14}
          className={cn(
            "shrink-0 text-cx-fg-4 transition-[transform,color] duration-150 ease-cx-out group-hover:text-cx-fg-2",
            currentOpen && "rotate-180",
          )}
        />
      </button>

      <AgentDisclosure open={currentOpen}>
        <div
          id={contentId}
          role="region"
          aria-labelledby={triggerId}
          ref={viewportRef}
          className="cx-scroll overflow-y-auto px-2 pb-2"
          style={{ maxHeight }}
        >
          {items.length ? (
            <ol aria-live="polite" className="m-0 list-none space-y-0 p-0">
              <AnimatePresence initial={false} mode="popLayout">
                {items.map((item) => {
                  const status = item.status ?? "pending";
                  return (
                    <motion.li
                      key={item.id}
                      initial={reduced ? { opacity: 1 } : { opacity: 0, transform: "translateY(6px)" }}
                      animate={{ opacity: 1, transform: "translateY(0px)" }}
                      exit={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(-3px)" }}
                      transition={
                        reduced
                          ? { duration: 0 }
                          : { opacity: { duration: 0.18, ease: EASE_OUT }, transform: SPRING_LAYOUT }
                      }
                      className="flex min-h-9 items-center gap-2.5 rounded-[8px] px-1.5 py-1"
                    >
                      <TodoStatusIcon status={status} progress={item.progress} />
                      <span className="sr-only">{statusLabel(status)}: </span>
                      <span
                        className={cn(
                          "min-w-0 flex-1 truncate text-cx-sm leading-5",
                          status === "pending" && "text-cx-fg-3/65",
                          status === "in-progress" && "text-cx-fg",
                          status === "completed" && "text-cx-fg-3/60",
                          status === "cancelled" && "text-cx-fg-3/55",
                        )}
                      >
                        <span className="relative inline-block max-w-full">
                          {item.title}
                          <motion.span
                            aria-hidden="true"
                            initial={false}
                            animate={{
                              transform: status === "completed" ? "scaleX(1)" : "scaleX(0)",
                              opacity: status === "completed" ? 1 : 0,
                            }}
                            transition={
                              reduced
                                ? { duration: 0 }
                                : { duration: 0.28, ease: EASE_OUT, delay: 0.06 }
                            }
                            className="absolute inset-x-0 top-1/2 h-px origin-left bg-current"
                          />
                        </span>
                      </span>
                      {item.detail ? (
                        <span className="shrink-0 text-cx-xs text-cx-fg-4">{item.detail}</span>
                      ) : null}
                    </motion.li>
                  );
                })}
              </AnimatePresence>
            </ol>
          ) : (
            <p className="px-1.5 py-2 text-cx-sm text-cx-fg-3">暂无任务</p>
          )}
        </div>
      </AgentDisclosure>
    </section>
  );
}
