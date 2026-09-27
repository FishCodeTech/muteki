"use client";

import {
  cloneElement,
  isValidElement,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Portal } from "./Portal";
import { useFloating, type Placement } from "./floating";
import { useReducedMotion } from "./motion";
import { Kbd } from "./Kbd";

let lastTooltipClosedAt = 0;
const OPEN_DELAY = 420;
const WARM_WINDOW = 500;

function assignRef<T>(ref: Ref<T> | undefined, value: T) {
  if (!ref) return;
  if (typeof ref === "function") ref(value);
  else (ref as { current: T }).current = value;
}

type TriggerProps = {
  ref?: Ref<HTMLElement>;
  onPointerEnter?: (event: React.PointerEvent<HTMLElement>) => void;
  onPointerLeave?: (event: React.PointerEvent<HTMLElement>) => void;
  onPointerDown?: (event: React.PointerEvent<HTMLElement>) => void;
  onFocus?: (event: React.FocusEvent<HTMLElement>) => void;
  onBlur?: (event: React.FocusEvent<HTMLElement>) => void;
  "aria-describedby"?: string;
};

export interface TooltipProps {
  content: ReactNode;
  shortcut?: string | string[];
  placement?: Placement;
  disabled?: boolean;
  children: ReactElement;
  className?: string;
}

/**
 * Hover/focus tooltip with a warm-up window: once one tooltip has shown,
 * neighbours open instantly (toolbar scanning feels immediate).
 */
export function Tooltip({ content, shortcut, placement = "top", disabled, children, className }: TooltipProps) {
  const [open, setOpen] = useState(false);
  const anchorRef = useRef<HTMLElement | null>(null);
  const timer = useRef<number | undefined>(undefined);
  const id = useId();
  const reduced = useReducedMotion();
  const pos = useFloating({ open, anchorRef, placement, offset: 7 });

  const show = useCallback(() => {
    if (disabled || !content) return;
    window.clearTimeout(timer.current);
    const warm = Date.now() - lastTooltipClosedAt < WARM_WINDOW;
    if (warm) setOpen(true);
    else timer.current = window.setTimeout(() => setOpen(true), OPEN_DELAY);
  }, [content, disabled]);

  const hide = useCallback(() => {
    window.clearTimeout(timer.current);
    setOpen((current) => {
      if (current) lastTooltipClosedAt = Date.now();
      return false;
    });
  }, []);

  useEffect(() => () => window.clearTimeout(timer.current), []);
  useEffect(() => {
    if (disabled) hide();
  }, [disabled, hide]);

  if (!isValidElement(children)) return children;
  const childProps = children.props as TriggerProps;
  const trigger = cloneElement(children as ReactElement<TriggerProps>, {
    ref: (node: HTMLElement | null) => {
      anchorRef.current = node;
      assignRef(childProps.ref, node);
    },
    onPointerEnter: (event) => {
      childProps.onPointerEnter?.(event);
      if (event.pointerType === "mouse") show();
    },
    onPointerLeave: (event) => {
      childProps.onPointerLeave?.(event);
      hide();
    },
    onPointerDown: (event) => {
      childProps.onPointerDown?.(event);
      hide();
    },
    onFocus: (event) => {
      childProps.onFocus?.(event);
      if (event.currentTarget.matches(":focus-visible")) show();
    },
    onBlur: (event) => {
      childProps.onBlur?.(event);
      hide();
    },
    "aria-describedby": open ? id : childProps["aria-describedby"],
  });

  const keys = shortcut ? (Array.isArray(shortcut) ? shortcut : [shortcut]) : [];

  return (
    <>
      {trigger}
      <Portal>
        <AnimatePresence>
          {open ? (
            <motion.div
              ref={pos.setFloating}
              id={id}
              role="tooltip"
              className={cn(
                "pointer-events-none fixed z-[1200] flex max-w-[280px] items-center gap-2 rounded-lg px-2 py-1 text-[12px] font-medium leading-[18px]",
                "bg-[color-mix(in_srgb,var(--ink)_92%,var(--page))] text-[var(--page)] shadow-cx-md",
                className,
              )}
              style={{ top: pos.top, left: pos.left, transformOrigin: pos.origin, visibility: pos.ready ? "visible" : "hidden" }}
              initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.94 }}
              animate={{ opacity: 1, scale: 1, transition: { duration: 0.12, ease: [0.16, 1, 0.3, 1] } }}
              exit={{ opacity: 0, transition: { duration: 0.08 } }}
            >
              <span className="min-w-0">{content}</span>
              {keys.length ? (
                <span className="flex items-center gap-0.5">
                  {keys.map((key) => <Kbd key={key} tone="inverse">{key}</Kbd>)}
                </span>
              ) : null}
            </motion.div>
          ) : null}
        </AnimatePresence>
      </Portal>
    </>
  );
}
