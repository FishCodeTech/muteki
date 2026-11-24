"use client";

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactElement,
  type ReactNode,
  type RefObject,
} from "react";
import { AnimatePresence, motion } from "motion/react";
import {
  createPopoverController,
  Popover as UiPopover,
  PopoverContent,
  PopoverTrigger,
} from "@ui-sdk/react/popover";
import {
  HoverCard as UiHoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "@ui-sdk/react/hover-card";
import { cn } from "../utils/cn";
import { useReducedMotion } from "../utils/reducedMotion";
import { EASE_OUT } from "./motion";
import { Portal } from "./Portal";
import { ensureImeCompositionTracking, guardOverlayDismiss } from "./_ime";

export type Placement =
  | "top"
  | "top-start"
  | "top-end"
  | "bottom"
  | "bottom-start"
  | "bottom-end"
  | "right"
  | "right-start"
  | "left"
  | "left-start";

type Side = "top" | "bottom" | "left" | "right";
type Align = "start" | "center" | "end";

export function splitPlacement(placement: Placement): {
  side: Side;
  align: Align;
} {
  const [side, align] = placement.split("-") as [Side, "start" | "end" | undefined];
  return { side, align: align ?? "center" };
}

export function useControllableOpen(
  value: boolean | undefined,
  defaultValue: boolean,
  onChange?: (open: boolean) => void,
) {
  const [inner, setInner] = useState(defaultValue);
  const controlled = value !== undefined;
  const open = controlled ? value : inner;
  const setOpen = useCallback(
    (next: boolean) => {
      if (!controlled) setInner(next);
      onChange?.(next);
    },
    [controlled, onChange],
  );
  return [open, setOpen] as const;
}

export interface PopoverProps {
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
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
  /** Close on outside pointer-down (defaults true). */
  closeOnOutsidePress?: boolean;
  /** Close when pressing Escape (defaults true). */
  closeOnEscape?: boolean;
  instant?: boolean;
  openOnHover?: boolean;
  delay?: number;
  closeDelay?: number;
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
  closeOnOutsidePress = true,
  closeOnEscape = true,
  instant = false,
  openOnHover,
  delay,
  closeDelay,
}: PopoverProps) {
  const [open, setOpen] = useControllableOpen(openProp, defaultOpen, onOpenChange);
  const [controller] = useState(() => createPopoverController());
  const contentRef = useRef<HTMLDivElement | null>(null);
  const { side, align } = splitPlacement(placement);

  useEffect(() => {
    ensureImeCompositionTracking();
  }, []);

  const close = useCallback(() => {
    setOpen(false);
    controller.close();
  }, [controller, setOpen]);

  const anchor = useMemo(() => {
    if (anchorPoint) return { x: anchorPoint.x, y: anchorPoint.y };
    if (externalAnchor) {
      return {
        getBoundingClientRect: () =>
          externalAnchor.current?.getBoundingClientRect() ?? new DOMRect(),
      };
    }
    return undefined;
  }, [anchorPoint, externalAnchor]);

  const resolvedInitialFocus = useCallback((): HTMLElement | null | undefined => {
    if (initialFocus === "none") return null;
    if (typeof initialFocus === "object" && "current" in initialFocus) {
      return initialFocus.current ?? contentRef.current ?? undefined;
    }
    if (initialFocus === "container") return contentRef.current ?? undefined;
    return contentRef.current ?? undefined;
  }, [initialFocus]);

  const resolvedFinalFocus = restoreFocus ? undefined : ("none" as const);
  const content = typeof children === "function" ? children({ close }) : children;

  return (
    <UiPopover
      open={openProp !== undefined ? open : undefined}
      defaultOpen={openProp === undefined ? defaultOpen : undefined}
      modal={false}
      controller={controller}
      openOnHover={openOnHover}
      delay={delay}
      closeDelay={closeDelay}
      onOpenChange={(nextOpen, details) => {
        if (!closeOnOutsidePress && !nextOpen && details?.reason === "outside-press") {
          details.cancel?.();
          return;
        }
        if (!closeOnEscape && !nextOpen && details?.reason === "escape-key") {
          details.cancel?.();
          return;
        }
        if (guardOverlayDismiss(nextOpen, details)) return;
        setOpen(nextOpen);
      }}
    >
      {trigger ? (
        <PopoverTrigger
          render={trigger}
          aria-haspopup={haspopup === "true" ? true : haspopup}
          data-state={open ? "open" : "closed"}
        />
      ) : null}
      <PopoverContent
        ref={contentRef}
        anchor={anchor}
        side={side}
        align={align}
        sideOffset={offset}
        initialFocus={resolvedInitialFocus}
        finalFocus={resolvedFinalFocus}
        role={role}
        render={role === "menu" ? <div role="menu" /> : undefined}
        aria-label={ariaLabel}
        data-cx-layer=""
        data-instant={instant ? "true" : undefined}
        onKeyDown={onKeyDown}
        onClick={role === "menu" ? (event) => event.stopPropagation() : undefined}
        onPointerDown={role === "menu" ? (event) => event.stopPropagation() : undefined}
        onContextMenu={role === "menu" ? (event) => event.stopPropagation() : undefined}
        style={{
          maxHeight: "var(--available-height)",
          transformOrigin: "var(--transform-origin)",
          ...style,
        }}
        className={cn(
          "cx-portal z-(--z-popover) m-0 flex min-w-[180px] flex-col overflow-hidden rounded-[12px] bg-cx-overlay p-0 text-cx-fg shadow-cx-pop outline-none",
          "dark:shadow-[inset_0_1px_0_color-mix(in_srgb,white_8%,transparent),var(--cx-shadow-pop)]",
          matchWidth && "min-w-(--anchor-width)",
          !instant && "muteki-popover-anim",
          className,
        )}
      >
        {content}
      </PopoverContent>
    </UiPopover>
  );
}

const VIEWPORT_PADDING = 8;

function oppositeSide(side: Side): Side {
  return side === "top" ? "bottom" : side === "bottom" ? "top" : side === "left" ? "right" : "left";
}

function computeAnchoredBox(
  anchor: DOMRect,
  floating: { width: number; height: number },
  side: Side,
  align: Align,
  offset: number,
): { top: number; left: number } {
  if (side === "bottom" || side === "top") {
    const top = side === "bottom" ? anchor.bottom + offset : anchor.top - floating.height - offset;
    const left =
      align === "start"
        ? anchor.left
        : align === "end"
          ? anchor.right - floating.width
          : anchor.left + anchor.width / 2 - floating.width / 2;
    return { top, left };
  }
  const left = side === "right" ? anchor.right + offset : anchor.left - floating.width - offset;
  const top =
    align === "start"
      ? anchor.top
      : align === "end"
        ? anchor.bottom - floating.height
        : anchor.top + anchor.height / 2 - floating.height / 2;
  return { top, left };
}

function boxFitsViewport(pos: { top: number; left: number }, size: { width: number; height: number }, side: Side): boolean {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  if (side === "bottom") return pos.top + size.height <= vh - VIEWPORT_PADDING;
  if (side === "top") return pos.top >= VIEWPORT_PADDING;
  if (side === "right") return pos.left + size.width <= vw - VIEWPORT_PADDING;
  return pos.left >= VIEWPORT_PADDING;
}

function originForSideAlign(side: Side, align: Align): string {
  const cross = align === "start" ? "0%" : align === "end" ? "100%" : "50%";
  if (side === "bottom") return `${cross} 0%`;
  if (side === "top") return `${cross} 100%`;
  if (side === "right") return `0% ${cross}`;
  return `100% ${cross}`;
}

/** Non-interactive hover preview layer with smooth row-to-row gliding across anchor changes. */
export function FloatingHoverPanel({
  open,
  anchor,
  placement = "right-start",
  offset = 10,
  className,
  role = "tooltip",
  testAttr,
  children,
}: {
  open: boolean;
  anchor: HTMLElement | null;
  placement?: Placement;
  offset?: number;
  className?: string;
  role?: string;
  testAttr?: string;
  children: ReactNode;
}) {
  const activeOpen = open && Boolean(anchor);
  const reduced = useReducedMotion();
  const floatingRef = useRef<HTMLDivElement | null>(null);
  const [floatingNode, setFloatingNode] = useState<HTMLDivElement | null>(null);
  const setFloating = useCallback((node: HTMLDivElement | null) => {
    floatingRef.current = node;
    setFloatingNode((prev) => (prev === node ? prev : node));
  }, []);

  const [pos, setPos] = useState<{ left: number; top: number; origin: string; ready: boolean }>({
    left: -9999,
    top: -9999,
    origin: "0% 0%",
    ready: false,
  });

  const update = useCallback(() => {
    const el = floatingRef.current;
    if (!el || !anchor) return;
    const anchorRect = anchor.getBoundingClientRect();
    const size = { width: el.offsetWidth, height: el.offsetHeight };
    const { side: preferred, align } = splitPlacement(placement);
    let side = preferred;
    let computed = computeAnchoredBox(anchorRect, size, side, align, offset);
    if (!boxFitsViewport(computed, size, side)) {
      const flipped = oppositeSide(side);
      const alt = computeAnchoredBox(anchorRect, size, flipped, align, offset);
      if (boxFitsViewport(alt, size, flipped)) {
        side = flipped;
        computed = alt;
      }
    }
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const left = Math.min(Math.max(computed.left, VIEWPORT_PADDING), Math.max(VIEWPORT_PADDING, vw - size.width - VIEWPORT_PADDING));
    const top = Math.min(Math.max(computed.top, VIEWPORT_PADDING), Math.max(VIEWPORT_PADDING, vh - size.height - VIEWPORT_PADDING));
    setPos({ left, top, origin: originForSideAlign(side, align), ready: true });
  }, [anchor, offset, placement]);

  useLayoutEffect(() => {
    if (!activeOpen || !floatingNode) return;
    update();
    const observer = typeof ResizeObserver !== "undefined" ? new ResizeObserver(() => update()) : null;
    observer?.observe(floatingNode);
    if (anchor) observer?.observe(anchor);
    window.addEventListener("resize", update);
    window.addEventListener("scroll", update, true);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", update, true);
    };
  }, [activeOpen, anchor, floatingNode, update]);

  const wasOpenReadyRef = useRef(false);
  const canSlidePosition = activeOpen && pos.ready && wasOpenReadyRef.current;
  useEffect(() => {
    wasOpenReadyRef.current = activeOpen && pos.ready;
  }, [activeOpen, pos.ready]);

  return (
    <Portal>
      <AnimatePresence>
        {activeOpen ? (
          <motion.div
            ref={setFloating}
            key="floating-hover-panel"
            role={role}
            data-cx-thread-hover-card={testAttr === "thread-hover-card" ? "" : undefined}
            className={cn("cx-portal pointer-events-none fixed left-0 top-0 z-(--z-hovercard)", className)}
            style={{
              visibility: pos.ready ? "visible" : "hidden",
              transform: pos.ready ? `translate3d(${pos.left}px, ${pos.top}px, 0)` : undefined,
              transition: !reduced && canSlidePosition ? "transform 150ms var(--cx-ease-out)" : undefined,
            }}
            exit={{ opacity: 0, transition: { duration: 0.08, ease: EASE_OUT } }}
          >
            <motion.div
              style={{ transformOrigin: pos.origin }}
              initial={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateX(-4px)" }}
              animate={
                reduced
                  ? { opacity: 1, transition: { duration: 0.12, ease: EASE_OUT } }
                  : { opacity: 1, transform: "translateX(0px)", transition: { duration: 0.14, ease: EASE_OUT } }
              }
            >
              {children}
            </motion.div>
          </motion.div>
        ) : null}
      </AnimatePresence>
    </Portal>
  );
}

export interface HoverCardProps {
  trigger: ReactElement;
  placement?: Placement;
  offset?: number;
  delay?: number;
  closeDelay?: number;
  disabled?: boolean;
  className?: string;
  children: ReactNode;
}

/** Interactive hover preview card using the shared `@ui-sdk` positioning & dismiss stack. */
export function HoverCard({
  trigger,
  placement = "top",
  offset = 8,
  delay = 320,
  closeDelay = 80,
  disabled,
  className,
  children,
}: HoverCardProps) {
  const { side, align } = splitPlacement(placement);
  return (
    <UiHoverCard delay={delay} closeDelay={closeDelay} disabled={disabled}>
      <HoverCardTrigger render={trigger} />
      <HoverCardContent
        side={side}
        align={align}
        sideOffset={offset}
        style={{ transformOrigin: "var(--transform-origin)" }}
        className={cn(
          "cx-portal pointer-events-auto z-(--z-popover) m-0 max-w-[320px] rounded-[10px] bg-cx-overlay p-2.5 text-cx-fg shadow-cx-pop outline-none",
          "dark:shadow-[var(--ui-sheen-strong),var(--cx-shadow-pop)]",
          "muteki-popover-anim",
          className,
        )}
      >
        {children}
      </HoverCardContent>
    </UiHoverCard>
  );
}
