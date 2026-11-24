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
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";
import { setPaneResizing } from "../utils/paneResize";
import { EASE_OUT } from "./motion";

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
    const flag = (key: "fadeTop" | "fadeBottom" | "fadeLeft" | "fadeRight", on: boolean) => {
      const value = String(on);
      if (el.dataset[key] !== value) el.dataset[key] = value;
    };
    if (axis === "y") {
      const top = el.scrollTop > 2;
      const bottom = el.scrollTop + el.clientHeight < el.scrollHeight - 2;
      flag("fadeTop", top);
      flag("fadeBottom", bottom);
    } else {
      const left = el.scrollLeft > 2;
      const right = el.scrollLeft + el.clientWidth < el.scrollWidth - 2;
      flag("fadeLeft", left);
      flag("fadeRight", right);
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
      data-slot="scroll-area"
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

export function Collapse({
  open,
  children,
  className,
  initial = false,
  enterDuration = 0.22,
  exitDuration = 0.14,
  stableGeometry = false,
}: {
  open: boolean;
  children: ReactNode;
  className?: string;
  initial?: boolean;
  enterDuration?: number;
  exitDuration?: number;
  stableGeometry?: boolean;
}) {
  const reduced = useReducedMotion();
  return (
    <AnimatePresence initial={initial} presenceAffectsLayout={false}>
      {open ? (
        <motion.div
          key="collapse"
          data-slot="collapse"
          initial={reduced ? { opacity: 0 } : stableGeometry ? { height: 0, opacity: 0 } : { height: 0, opacity: 0, y: -4, clipPath: "inset(0 0 100% 0)" }}
          animate={reduced ? { opacity: 1 } : { height: "auto", opacity: 1, ...(!stableGeometry && { y: 0, clipPath: "inset(0 0 0% 0)" }), transition: { duration: enterDuration, ease: EASE_OUT } }}
          exit={reduced ? { opacity: 0 } : { height: 0, opacity: 0, ...(!stableGeometry && { y: -4, clipPath: "inset(0 0 100% 0)" }), transition: { duration: exitDuration, ease: EASE_OUT } }}
          style={{ transformOrigin: "top" }}
          className={cn("overflow-hidden", className)}
        >
          {children}
        </motion.div>
      ) : null}
    </AnimatePresence>
  );
}

export function ResizeHandle({
  side,
  value,
  min,
  max,
  onChange,
  onReset,
  onDragStateChange,
  onRawChange,
  label = "拖拽调整宽度",
  className,
}: {
  side: "left" | "right";
  value: number;
  min: number;
  max: number;
  onChange: (value: number) => void;
  onReset?: () => void;
  onDragStateChange?: (dragging: boolean) => void;
  onRawChange?: (raw: number) => void;
  label?: string;
  className?: string;
}) {
  const [dragging, setDragging] = useState(false);
  const start = useRef({ x: 0, value: 0 });
  const cleanup = useRef<(() => void) | null>(null);
  const clamp = (next: number) => Math.round(Math.min(max, Math.max(min, next)));

  useEffect(() => () => cleanup.current?.(), []);

  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture?.(event.pointerId);
    start.current = { x: event.clientX, value };
    setDragging(true);
    setPaneResizing(true);
    onDragStateChange?.(true);
    let pending: number | null = null;
    let pendingRaw: number | null = null;
    let frame = 0;
    const flush = () => {
      frame = 0;
      if (pendingRaw != null) {
        onRawChange?.(pendingRaw);
        pendingRaw = null;
      }
      if (pending != null) {
        onChange(pending);
        pending = null;
      }
    };
    const move = (ev: PointerEvent) => {
      const delta = ev.clientX - start.current.x;
      const raw = Math.round(Math.min(max, side === "left" ? start.current.value - delta : start.current.value + delta));
      pendingRaw = raw;
      pending = Math.max(min, raw);
      if (!frame) frame = requestAnimationFrame(flush);
    };
    const up = () => {
      cleanup.current = null;
      setDragging(false);
      cancelAnimationFrame(frame);
      flush();
      onDragStateChange?.(false);
      setPaneResizing(false);
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
    };
    cleanup.current = up;
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  };

  const trackGuide = (event: React.PointerEvent<HTMLDivElement>) => {
    const el = event.currentTarget;
    el.style.setProperty("--guide-y", `${event.clientY - el.getBoundingClientRect().top}px`);
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
      data-slot="resize-handle"
      data-dragging={dragging || undefined}
      onPointerDown={onPointerDown}
      onPointerMove={trackGuide}
      onPointerEnter={trackGuide}
      onKeyDown={onKeyDown}
      onDoubleClick={onReset}
      className={cn(
        "group absolute inset-y-0 z-20 w-2 cursor-col-resize touch-none outline-none",
        side === "left" ? "-left-[3px]" : "-right-[3px]",
        className,
      )}
    >
      <span className="cx-resize-guide absolute left-[calc(50%_-_0.5px)] w-px" />
    </div>
  );
}

export function Divider({ vertical, className }: { vertical?: boolean; className?: string }) {
  return (
    <div
      role="separator"
      aria-orientation={vertical ? "vertical" : "horizontal"}
      data-slot="divider"
      className={cn(
        "shrink-0 bg-cx-border-subtle",
        vertical ? "h-full w-px" : "h-px w-full",
        className,
      )}
    />
  );
}

export function Surface({ className, children, ...rest }: ComponentPropsWithRef<"div">) {
  return (
    <div
      data-slot="surface"
      className={cn("ui-surface-ring rounded-[12px] bg-cx-elevated text-cx-fg", className)}
      {...rest}
    >
      {children}
    </div>
  );
}
