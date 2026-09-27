"use client";

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ComponentPropsWithRef,
  type ReactNode,
  type Ref,
} from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { EASE_OUT, useReducedMotion } from "./motion";

function assignRef<T>(ref: Ref<T> | undefined, value: T) {
  if (!ref) return;
  if (typeof ref === "function") ref(value);
  else (ref as { current: T }).current = value;
}

/** Scroll container that fades its edges only when there is more content that way. */
export function ScrollArea({
  axis = "y",
  className,
  children,
  ref,
  onScroll,
  fade = true,
  ...rest
}: ComponentPropsWithRef<"div"> & { axis?: "x" | "y"; fade?: boolean }) {
  const inner = useRef<HTMLDivElement | null>(null);
  const measure = useCallback(() => {
    const el = inner.current;
    if (!el || !fade) return;
    if (axis === "y") {
      el.dataset.fadeTop = String(el.scrollTop > 2);
      el.dataset.fadeBottom = String(el.scrollTop + el.clientHeight < el.scrollHeight - 2);
    } else {
      el.dataset.fadeLeft = String(el.scrollLeft > 2);
      el.dataset.fadeRight = String(el.scrollLeft + el.clientWidth < el.scrollWidth - 2);
    }
  }, [axis, fade]);
  useEffect(() => {
    measure();
    const el = inner.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    if (el.firstElementChild) observer.observe(el.firstElementChild);
    return () => observer.disconnect();
  }, [measure]);
  return (
    <div
      {...rest}
      ref={(node) => { inner.current = node; assignRef(ref, node); }}
      onScroll={(event) => { measure(); onScroll?.(event); }}
      className={cn(
        "cx-scroll min-h-0 min-w-0",
        axis === "y" ? "overflow-y-auto overflow-x-hidden" : "cx-no-scrollbar overflow-x-auto overflow-y-hidden",
        fade && (axis === "y" ? "cx-fade-y" : "cx-fade-x"),
        className,
      )}
    >
      {children}
    </div>
  );
}

/** Height-animated disclosure body. */
export function Collapse({ open, children, className, initial = false }: { open: boolean; children: ReactNode; className?: string; initial?: boolean }) {
  const reduced = useReducedMotion();
  return (
    <AnimatePresence initial={initial}>
      {open ? (
        <motion.div
          key="collapse"
          initial={reduced ? { opacity: 0 } : { height: 0, opacity: 0, y: -4, clipPath: "inset(0 0 100% 0)" }}
          animate={reduced ? { opacity: 1 } : { height: "auto", opacity: 1, y: 0, clipPath: "inset(0 0 0% 0)", transition: { duration: 0.22, ease: EASE_OUT } }}
          exit={reduced ? { opacity: 0 } : { height: 0, opacity: 0, y: -4, clipPath: "inset(0 0 100% 0)", transition: { duration: 0.14, ease: EASE_OUT } }}
          style={{ transformOrigin: "top" }}
          className={cn("overflow-hidden", className)}
        >
          {children}
        </motion.div>
      ) : null}
    </AnimatePresence>
  );
}

/**
 * Vertical drag handle for resizable columns: an 8px hit area around a 1px
 * hairline that lights up on hover and while dragging. Keyboard: arrows,
 * Home/End, Enter resets.
 */
export function ResizeHandle({
  side,
  value,
  min,
  max,
  onChange,
  onReset,
  onDragStateChange,
  label = "拖拽调整宽度",
  className,
}: {
  /** Edge of the panel the handle sits on. */
  side: "left" | "right";
  value: number;
  min: number;
  max: number;
  onChange: (value: number) => void;
  onReset?: () => void;
  onDragStateChange?: (dragging: boolean) => void;
  label?: string;
  className?: string;
}) {
  const [dragging, setDragging] = useState(false);
  const start = useRef({ x: 0, value: 0 });
  const clamp = (next: number) => Math.round(Math.min(max, Math.max(min, next)));

  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    start.current = { x: event.clientX, value };
    setDragging(true);
    onDragStateChange?.(true);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    const move = (ev: PointerEvent) => {
      const delta = ev.clientX - start.current.x;
      onChange(clamp(side === "left" ? start.current.value - delta : start.current.value + delta));
    };
    const up = () => {
      setDragging(false);
      onDragStateChange?.(false);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const step = event.shiftKey ? 48 : 16;
    const grow = side === "left" ? "ArrowLeft" : "ArrowRight";
    const shrink = side === "left" ? "ArrowRight" : "ArrowLeft";
    if (event.key === grow) { event.preventDefault(); onChange(clamp(value + step)); }
    else if (event.key === shrink) { event.preventDefault(); onChange(clamp(value - step)); }
    else if (event.key === "Home") { event.preventDefault(); onChange(min); }
    else if (event.key === "End") { event.preventDefault(); onChange(max); }
    else if (event.key === "Enter" && onReset) { event.preventDefault(); onReset(); }
  };

  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuenow={value}
      tabIndex={0}
      data-dragging={dragging || undefined}
      onPointerDown={onPointerDown}
      onKeyDown={onKeyDown}
      onDoubleClick={onReset}
      className={cn(
        "group absolute inset-y-0 z-20 w-2 cursor-col-resize touch-none outline-none",
        side === "left" ? "-left-1" : "-right-1",
        className,
      )}
    >
      <span
        className={cn(
          "absolute inset-y-0 left-1/2 w-px -translate-x-1/2 bg-transparent transition-colors duration-150",
          "group-hover:bg-cx-border-strong group-focus-visible:bg-cx-fg-3 group-data-[dragging=true]:bg-cx-fg-3",
        )}
      />
    </div>
  );
}
