"use client";

import { useCallback, useLayoutEffect, useRef, useState, type RefObject } from "react";

export type Side = "top" | "bottom" | "left" | "right";
export type Align = "start" | "center" | "end";
export type Placement = Side | `${Side}-${Align}`;

export interface FloatingPosition {
  top: number;
  left: number;
  side: Side;
  align: Align;
  /** Transform origin that points back at the anchor, for scale animations. */
  origin: string;
  /** Max height that keeps the floating box inside the viewport. */
  maxHeight: number;
  ready: boolean;
}

const VIEWPORT_PADDING = 8;

function parsePlacement(placement: Placement): { side: Side; align: Align } {
  const [side, align = "center"] = placement.split("-") as [Side, Align | undefined];
  return { side, align };
}

function opposite(side: Side): Side {
  return side === "top" ? "bottom" : side === "bottom" ? "top" : side === "left" ? "right" : "left";
}

function compute(
  anchor: DOMRect,
  floating: { width: number; height: number },
  side: Side,
  align: Align,
  offset: number,
): { top: number; left: number } {
  let top = 0;
  let left = 0;
  if (side === "bottom" || side === "top") {
    top = side === "bottom" ? anchor.bottom + offset : anchor.top - floating.height - offset;
    left = align === "start"
      ? anchor.left
      : align === "end"
        ? anchor.right - floating.width
        : anchor.left + anchor.width / 2 - floating.width / 2;
  } else {
    left = side === "right" ? anchor.right + offset : anchor.left - floating.width - offset;
    top = align === "start"
      ? anchor.top
      : align === "end"
        ? anchor.bottom - floating.height
        : anchor.top + anchor.height / 2 - floating.height / 2;
  }
  return { top, left };
}

function fits(pos: { top: number; left: number }, size: { width: number; height: number }, side: Side): boolean {
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  if (side === "bottom") return pos.top + size.height <= vh - VIEWPORT_PADDING;
  if (side === "top") return pos.top >= VIEWPORT_PADDING;
  if (side === "right") return pos.left + size.width <= vw - VIEWPORT_PADDING;
  return pos.left >= VIEWPORT_PADDING;
}

function originFor(side: Side, align: Align): string {
  const cross = align === "start" ? "0%" : align === "end" ? "100%" : "50%";
  if (side === "bottom") return `${cross} 0%`;
  if (side === "top") return `${cross} 100%`;
  if (side === "right") return `0% ${cross}`;
  return `100% ${cross}`;
}

/**
 * Anchored positioning with flip + viewport clamping. Anchor can be an element
 * or a virtual point (context menus). Recomputes on scroll, resize and when the
 * floating box changes size.
 *
 * The floating node is tracked via a callback ref (`setFloating`) so measurement
 * re-runs when a deferred Portal host finally mounts the panel (open=true from
 * the first render). Passing only a RefObject would miss that mount because
 * assigning `ref.current` does not invalidate effect deps.
 */
export function useFloating({
  open,
  anchorRef,
  anchorPoint,
  placement = "bottom-start",
  offset = 6,
  matchWidth = false,
}: {
  open: boolean;
  anchorRef?: RefObject<HTMLElement | null>;
  anchorPoint?: { x: number; y: number } | null;
  placement?: Placement;
  offset?: number;
  matchWidth?: boolean;
}): FloatingPosition & {
  anchorWidth: number;
  update: () => void;
  floatingRef: RefObject<HTMLElement | null>;
  setFloating: (node: HTMLElement | null) => void;
} {
  const floatingRef = useRef<HTMLElement | null>(null);
  const [floatingNode, setFloatingNode] = useState<HTMLElement | null>(null);
  const [state, setState] = useState<FloatingPosition & { anchorWidth: number }>({
    top: -9999,
    left: -9999,
    side: parsePlacement(placement).side,
    align: parsePlacement(placement).align,
    origin: "50% 0%",
    maxHeight: 480,
    ready: false,
    anchorWidth: 0,
  });

  const setFloating = useCallback((node: HTMLElement | null) => {
    floatingRef.current = node;
    setFloatingNode((prev) => (prev === node ? prev : node));
  }, []);

  const update = useCallback(() => {
    const floating = floatingRef.current;
    if (!floating) return;
    const anchorRect = anchorPoint
      ? new DOMRect(anchorPoint.x, anchorPoint.y, 0, 0)
      : anchorRef?.current?.getBoundingClientRect();
    if (!anchorRect) return;
    const size = { width: floating.offsetWidth, height: floating.offsetHeight };
    const { side: preferred, align } = parsePlacement(placement);
    let side = preferred;
    let pos = compute(anchorRect, size, side, align, offset);
    if (!fits(pos, size, side)) {
      const flipped = opposite(side);
      const alt = compute(anchorRect, size, flipped, align, offset);
      if (fits(alt, size, flipped)) {
        side = flipped;
        pos = alt;
      }
    }
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const left = Math.min(Math.max(pos.left, VIEWPORT_PADDING), Math.max(VIEWPORT_PADDING, vw - size.width - VIEWPORT_PADDING));
    const top = Math.min(Math.max(pos.top, VIEWPORT_PADDING), Math.max(VIEWPORT_PADDING, vh - size.height - VIEWPORT_PADDING));
    const maxHeight = side === "bottom"
      ? vh - anchorRect.bottom - offset - VIEWPORT_PADDING
      : side === "top"
        ? anchorRect.top - offset - VIEWPORT_PADDING
        : vh - VIEWPORT_PADDING * 2;
    setState({
      top,
      left,
      side,
      align,
      origin: originFor(side, align),
      maxHeight: Math.max(160, maxHeight),
      ready: true,
      anchorWidth: matchWidth ? anchorRect.width : 0,
    });
  }, [anchorPoint, anchorRef, matchWidth, offset, placement]);

  useLayoutEffect(() => {
    if (!open) {
      setState((prev) => (prev.ready ? { ...prev, ready: false } : prev));
      return;
    }
    // Wait for the floating node (Portal may mount it after the first layout pass).
    if (!floatingNode) {
      setState((prev) => (prev.ready ? { ...prev, ready: false } : prev));
      return;
    }
    update();
    const observer = typeof ResizeObserver !== "undefined" ? new ResizeObserver(() => update()) : null;
    observer?.observe(floatingNode);
    const anchor = anchorRef?.current;
    if (anchor) observer?.observe(anchor);
    window.addEventListener("resize", update);
    window.addEventListener("scroll", update, true);
    return () => {
      observer?.disconnect();
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", update, true);
    };
  }, [open, update, floatingNode, anchorRef]);

  return { ...state, update, floatingRef, setFloating };
}
