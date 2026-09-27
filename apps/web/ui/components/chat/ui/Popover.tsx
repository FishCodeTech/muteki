"use client";

import {
  cloneElement,
  isValidElement,
  useCallback,
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactElement,
  type ReactNode,
  type Ref,
  type RefObject,
} from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Portal } from "./Portal";
import { useFloating, type Placement } from "./floating";
import { LayerContext, focusableWithin, useLayer } from "./useDismiss";
import { EASE_OUT, useReducedMotion } from "./motion";

type TriggerProps = {
  ref?: Ref<HTMLElement>;
  onClick?: (event: React.MouseEvent<HTMLElement>) => void;
  onKeyDown?: (event: React.KeyboardEvent<HTMLElement>) => void;
  "aria-expanded"?: boolean;
  "aria-haspopup"?: string | boolean;
  "aria-controls"?: string;
  "data-state"?: string;
};

function assignRef<T>(ref: Ref<T> | undefined, value: T) {
  if (!ref) return;
  if (typeof ref === "function") ref(value);
  else (ref as { current: T }).current = value;
}

export function useControllableOpen(open: boolean | undefined, defaultOpen: boolean, onOpenChange?: (open: boolean) => void) {
  const [inner, setInner] = useState(defaultOpen);
  const controlled = open !== undefined;
  const value = controlled ? open : inner;
  const setValue = useCallback((next: boolean) => {
    if (!controlled) setInner(next);
    onOpenChange?.(next);
  }, [controlled, onOpenChange]);
  return [value, setValue] as const;
}

export interface PopoverProps {
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  /** Element that toggles the popover. Omit when anchoring via `anchorRef`/`anchorPoint`. */
  trigger?: ReactElement;
  anchorRef?: RefObject<HTMLElement | null>;
  anchorPoint?: { x: number; y: number } | null;
  placement?: Placement;
  offset?: number;
  matchWidth?: boolean;
  /** "first" focuses the first focusable child, "container" the panel, "none" keeps focus on the trigger. */
  initialFocus?: "first" | "container" | "none" | RefObject<HTMLElement | null>;
  className?: string;
  style?: CSSProperties;
  role?: string;
  ariaLabel?: string;
  haspopup?: "menu" | "listbox" | "dialog" | "true";
  children: ReactNode | ((api: { close: () => void }) => ReactNode);
  onKeyDown?: (event: React.KeyboardEvent<HTMLDivElement>) => void;
  /** Keep focus returning to the trigger when closed via keyboard. */
  restoreFocus?: boolean;
}

export function Popover({
  open: openProp,
  defaultOpen = false,
  onOpenChange,
  trigger,
  anchorRef: externalAnchor,
  anchorPoint,
  placement = "bottom-start",
  offset = 6,
  matchWidth,
  initialFocus = "first",
  className,
  style,
  role = "dialog",
  ariaLabel,
  haspopup = "dialog",
  children,
  onKeyDown,
  restoreFocus = true,
}: PopoverProps) {
  const [open, setOpen] = useControllableOpen(openProp, defaultOpen, onOpenChange);
  const triggerRef = useRef<HTMLElement | null>(null);
  const anchorRef = externalAnchor ?? triggerRef;
  const reduced = useReducedMotion();
  const closedViaKeyboard = useRef(false);
  const pos = useFloating({ open, anchorRef, anchorPoint, placement, offset, matchWidth });
  const floatingRef = pos.floatingRef;

  const close = useCallback(() => setOpen(false), [setOpen]);
  const layer = useLayer({
    open,
    refs: [anchorRef, floatingRef],
    onDismiss: (reason) => {
      closedViaKeyboard.current = reason === "escape";
      setOpen(false);
    },
  });

  useEffect(() => {
    if (!open || !pos.ready) return;
    const panel = floatingRef.current;
    if (!panel) return;
    if (initialFocus === "none") return;
    if (typeof initialFocus === "object") {
      initialFocus.current?.focus({ preventScroll: true });
      return;
    }
    if (initialFocus === "container") {
      panel.focus({ preventScroll: true });
      return;
    }
    const first = panel.querySelector<HTMLElement>("[data-autofocus]")
      ?? (role === "menu" ? panel.querySelector<HTMLElement>("[role^='menuitem']:not([aria-disabled='true'])") : null)
      ?? focusableWithin(panel)[0];
    (first ?? panel).focus({ preventScroll: true });
    // Only on the first ready frame of each open.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, pos.ready]);

  useEffect(() => {
    if (open || !restoreFocus) return;
    const panelHadFocus = closedViaKeyboard.current || floatingRef.current?.contains(document.activeElement);
    if (panelHadFocus) triggerRef.current?.focus({ preventScroll: true });
    closedViaKeyboard.current = false;
  }, [open, restoreFocus]);

  let triggerNode: ReactNode = null;
  if (trigger && isValidElement(trigger)) {
    const props = trigger.props as TriggerProps;
    triggerNode = cloneElement(trigger as ReactElement<TriggerProps>, {
      ref: (node: HTMLElement | null) => {
        triggerRef.current = node;
        assignRef(props.ref, node);
      },
      onClick: (event) => {
        props.onClick?.(event);
        if (!event.defaultPrevented) setOpen(!open);
      },
      onKeyDown: (event) => {
        props.onKeyDown?.(event);
        if (!open && (event.key === "ArrowDown" || event.key === "ArrowUp") && haspopup !== "dialog") {
          event.preventDefault();
          setOpen(true);
        }
      },
      "aria-expanded": open,
      "aria-haspopup": haspopup === "true" ? true : haspopup,
      "data-state": open ? "open" : "closed",
    });
  }

  const content = typeof children === "function" ? children({ close }) : children;

  return (
    <>
      {triggerNode}
      <Portal>
        <AnimatePresence>
          {open ? (
            <LayerContext.Provider value={layer}>
              <motion.div
                ref={pos.setFloating}
                role={role}
                aria-label={ariaLabel}
                tabIndex={-1}
                data-cx-layer=""
                onKeyDown={onKeyDown}
                className={cn(
                  "fixed z-[1100] flex min-w-[180px] flex-col overflow-hidden rounded-xl bg-cx-overlay text-cx-fg shadow-cx-pop outline-none",
                  className,
                )}
                style={{
                  top: pos.top,
                  left: pos.left,
                  maxHeight: pos.maxHeight,
                  transformOrigin: pos.origin,
                  visibility: pos.ready ? "visible" : "hidden",
                  ...(matchWidth && pos.anchorWidth ? { minWidth: pos.anchorWidth } : null),
                  ...style,
                }}
                initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.96, y: pos.side === "top" ? 4 : -4 }}
                animate={{ opacity: 1, scale: 1, y: 0, transition: { duration: 0.16, ease: EASE_OUT } }}
                exit={reduced ? { opacity: 0, transition: { duration: 0.08 } } : { opacity: 0, scale: 0.97, transition: { duration: 0.1, ease: EASE_OUT } }}
              >
                {content}
              </motion.div>
            </LayerContext.Provider>
          ) : null}
        </AnimatePresence>
      </Portal>
    </>
  );
}
