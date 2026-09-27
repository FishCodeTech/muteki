"use client";

import { useEffect, useId, useRef, type ReactNode, type RefObject } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Portal } from "./Portal";
import { LayerContext, focusableWithin, useFocusTrap, useLayer, useRestoreFocus, useScrollLock } from "./useDismiss";
import { EASE_DRAWER, EASE_OUT, useReducedMotion } from "./motion";
import { IconButton } from "./Button";

type DialogSize = "sm" | "md" | "lg" | "xl" | "full";

const SIZE: Record<DialogSize, string> = {
  sm: "w-[min(420px,calc(100vw-32px))]",
  md: "w-[min(520px,calc(100vw-32px))]",
  lg: "w-[min(680px,calc(100vw-32px))]",
  xl: "w-[min(880px,calc(100vw-32px))]",
  full: "w-[calc(100vw-48px)] h-[calc(100vh-48px)]",
};

export interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title?: ReactNode;
  description?: ReactNode;
  icon?: IconName;
  tone?: "default" | "danger" | "warning" | "accent";
  size?: DialogSize;
  footer?: ReactNode;
  children?: ReactNode;
  className?: string;
  bodyClassName?: string;
  hideClose?: boolean;
  initialFocusRef?: RefObject<HTMLElement | null>;
  /** Prevent backdrop-click dismissal (destructive / long forms). */
  dismissable?: boolean;
  testId?: string;
}

function useOverlayFocus(open: boolean, panelRef: RefObject<HTMLElement | null>, initialFocusRef?: RefObject<HTMLElement | null>) {
  useRestoreFocus(open);
  useFocusTrap(open, panelRef);
  useScrollLock(open);
  useEffect(() => {
    if (!open) return;
    const frame = window.requestAnimationFrame(() => {
      const panel = panelRef.current;
      if (!panel) return;
      const target = initialFocusRef?.current
        ?? panel.querySelector<HTMLElement>("[data-autofocus]")
        ?? focusableWithin(panel).find((el) => !el.hasAttribute("data-dialog-close"))
        ?? panel;
      target.focus({ preventScroll: true });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [open, panelRef, initialFocusRef]);
}

const TONE_ICON: Record<NonNullable<DialogProps["tone"]>, string> = {
  default: "bg-cx-hover text-cx-fg-2",
  accent: "bg-cx-accent-soft text-cx-accent",
  warning: "bg-cx-warning-soft text-cx-warning",
  danger: "bg-cx-danger-soft text-cx-danger",
};

export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  icon,
  tone = "default",
  size = "md",
  footer,
  children,
  className,
  bodyClassName,
  hideClose,
  initialFocusRef,
  dismissable = true,
  testId,
}: DialogProps) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  const titleId = useId();
  const descId = useId();
  const reduced = useReducedMotion();
  const layer = useLayer({
    open,
    refs: [panelRef],
    outside: dismissable,
    onDismiss: () => onOpenChange(false),
  });
  useOverlayFocus(open, panelRef, initialFocusRef);

  return (
    <Portal>
      <AnimatePresence>
        {open ? (
          <LayerContext.Provider value={layer}>
            <motion.div
              key="backdrop"
              className="fixed inset-0 z-[1000] bg-[color-mix(in_srgb,black_38%,transparent)] backdrop-blur-[3px]"
              initial={{ opacity: 0 }}
              animate={{ opacity: 1, transition: { duration: 0.2, ease: EASE_OUT } }}
              exit={{ opacity: 0, transition: { duration: 0.14 } }}
            />
            <div className="pointer-events-none fixed inset-0 z-[1001] flex items-center justify-center p-4">
              <motion.div
                ref={panelRef}
                role="dialog"
                aria-modal="true"
                aria-labelledby={title ? titleId : undefined}
                aria-describedby={description ? descId : undefined}
                tabIndex={-1}
                data-cx-layer=""
                data-testid={testId}
                className={cn(
                  "pointer-events-auto relative flex max-h-[calc(100vh-48px)] flex-col overflow-hidden rounded-2xl bg-cx-overlay text-cx-fg shadow-cx-pop outline-none",
                  SIZE[size],
                  className,
                )}
                initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.965, y: 10 }}
                animate={{ opacity: 1, scale: 1, y: 0, transition: { duration: 0.24, ease: EASE_OUT } }}
                exit={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.98, y: 4, transition: { duration: 0.14, ease: EASE_OUT } }}
              >
                {title || !hideClose ? (
                  <div className="flex items-start gap-3 px-5 pb-2 pt-5">
                    {icon ? (
                      <span className={cn("mt-0.5 grid size-9 shrink-0 place-items-center rounded-xl", TONE_ICON[tone])}>
                        <Icon name={icon} size={18} />
                      </span>
                    ) : null}
                    <div className="min-w-0 flex-1">
                      {title ? <h2 id={titleId} className="text-[16px] font-semibold leading-6 tracking-[-0.01em] text-cx-fg">{title}</h2> : null}
                      {description ? <p id={descId} className="mt-1 text-[13px] leading-5 text-cx-fg-3">{description}</p> : null}
                    </div>
                    {!hideClose ? (
                      <IconButton icon="x" label="关闭" noTooltip data-dialog-close="" className="-mr-1.5 -mt-1" onClick={() => onOpenChange(false)} />
                    ) : null}
                  </div>
                ) : null}
                {children ? <div className={cn("cx-scroll min-h-0 flex-1 overflow-y-auto px-5 py-3 text-[13.5px] leading-6 text-cx-fg-2", bodyClassName)}>{children}</div> : null}
                {footer ? (
                  <div className="flex flex-wrap items-center justify-end gap-2 border-t border-cx-border-subtle bg-cx-bg-subtle px-5 py-3">
                    {footer}
                  </div>
                ) : null}
              </motion.div>
            </div>
          </LayerContext.Provider>
        ) : null}
      </AnimatePresence>
    </Portal>
  );
}

export interface SheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  side?: "right" | "left" | "bottom";
  title?: ReactNode;
  description?: ReactNode;
  headerActions?: ReactNode;
  width?: number | string;
  children?: ReactNode;
  footer?: ReactNode;
  className?: string;
  bodyClassName?: string;
  /** Render without a dimming backdrop (inspector-style side sheet). */
  modal?: boolean;
  testId?: string;
  ariaLabel?: string;
}

export function Sheet({
  open,
  onOpenChange,
  side = "right",
  title,
  description,
  headerActions,
  width = 440,
  children,
  footer,
  className,
  bodyClassName,
  modal = true,
  testId,
  ariaLabel,
}: SheetProps) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  const titleId = useId();
  const reduced = useReducedMotion();
  const layer = useLayer({ open, refs: [panelRef], outside: modal, onDismiss: () => onOpenChange(false) });
  useRestoreFocus(open);
  useFocusTrap(open && modal, panelRef);
  useScrollLock(open && modal);
  useEffect(() => {
    if (!open) return;
    const frame = window.requestAnimationFrame(() => {
      const panel = panelRef.current;
      if (!panel) return;
      (panel.querySelector<HTMLElement>("[data-autofocus]") ?? panel).focus({ preventScroll: true });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [open]);

  const offscreen = side === "right" ? { x: "100%" } : side === "left" ? { x: "-100%" } : { y: "100%" };
  const position = side === "right"
    ? "right-0 top-0 bottom-0 border-l"
    : side === "left"
      ? "left-0 top-0 bottom-0 border-r"
      : "left-0 right-0 bottom-0 border-t rounded-t-2xl max-h-[85vh]";
  const size = side === "bottom" ? {} : { width: typeof width === "number" ? `min(${width}px, 100vw)` : width };

  return (
    <Portal>
      <AnimatePresence>
        {open ? (
          <LayerContext.Provider value={layer}>
            {modal ? (
              <motion.div
                key="sheet-backdrop"
                className="fixed inset-0 z-[1000] bg-[color-mix(in_srgb,black_30%,transparent)] backdrop-blur-[2px]"
                initial={{ opacity: 0 }}
                animate={{ opacity: 1, transition: { duration: 0.22 } }}
                exit={{ opacity: 0, transition: { duration: 0.16 } }}
              />
            ) : null}
            <motion.div
              ref={panelRef}
              role="dialog"
              aria-modal={modal || undefined}
              aria-labelledby={title ? titleId : undefined}
              aria-label={title ? undefined : ariaLabel}
              tabIndex={-1}
              data-cx-layer=""
              data-testid={testId}
              className={cn(
                "fixed z-[1001] flex flex-col border-cx-border bg-cx-overlay text-cx-fg shadow-cx-pop outline-none",
                position,
                className,
              )}
              style={size}
              initial={reduced ? { opacity: 0 } : offscreen}
              animate={reduced ? { opacity: 1 } : { x: 0, y: 0, transition: { duration: 0.34, ease: EASE_DRAWER } }}
              exit={reduced ? { opacity: 0 } : { ...offscreen, transition: { duration: 0.24, ease: EASE_DRAWER } }}
            >
              {title || headerActions ? (
                <div className="flex min-h-[52px] items-center gap-2 border-b border-cx-border-subtle px-4 py-2.5">
                  <div className="min-w-0 flex-1">
                    {title ? <h2 id={titleId} className="truncate text-[14px] font-semibold text-cx-fg">{title}</h2> : null}
                    {description ? <p className="truncate text-[12px] text-cx-fg-3">{description}</p> : null}
                  </div>
                  {headerActions}
                  <IconButton icon="x" label="关闭" noTooltip onClick={() => onOpenChange(false)} />
                </div>
              ) : null}
              <div className={cn("cx-scroll min-h-0 flex-1 overflow-y-auto", bodyClassName)}>{children}</div>
              {footer ? <div className="border-t border-cx-border-subtle px-4 py-3">{footer}</div> : null}
            </motion.div>
          </LayerContext.Provider>
        ) : null}
      </AnimatePresence>
    </Portal>
  );
}
