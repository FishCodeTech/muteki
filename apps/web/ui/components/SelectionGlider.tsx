"use client";

import { useLayoutEffect, useRef } from "react";

type SelectionRect = {
  x: number;
  y: number;
  width: number;
  height: number;
};

/** One shared, measured selection surface for a single-choice list. */
export function SelectionGlider({
  selectedKey,
  selector,
  className = "",
  ensureVisible = false,
  duration = 300,
}: {
  selectedKey: string | number | boolean | null | undefined;
  selector: string;
  className?: "" | "rail" | "compact" | "palette" | "settings" | "grid";
  ensureVisible?: boolean;
  duration?: number;
}) {
  const markerRef = useRef<HTMLSpanElement | null>(null);
  const lastRectRef = useRef<SelectionRect | null>(null);
  const initializedRef = useRef(false);

  useLayoutEffect(() => {
    const marker = markerRef.current;
    const host = marker?.parentElement;
    if (!marker || !host) return;

    let frame = 0;
    let ignoreResizeUntil = 0;
    const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

    const measure = (animate: boolean) => {
      const target = host.querySelector<HTMLElement>(selector);
      if (!target) {
        marker.getAnimations().forEach((animation) => animation.cancel());
        marker.style.opacity = "0";
        lastRectRef.current = null;
        return;
      }

      const hostRect = host.getBoundingClientRect();
      const targetRect = target.getBoundingClientRect();
      const next: SelectionRect = {
        x: targetRect.left - hostRect.left + host.scrollLeft,
        y: targetRect.top - hostRect.top + host.scrollTop,
        width: targetRect.width,
        height: targetRect.height,
      };
      const previous = lastRectRef.current;
      const finalTransform = `translate3d(${next.x}px, ${next.y}px, 0)`;

      marker.getAnimations().forEach((animation) => animation.cancel());
      marker.style.width = `${next.width}px`;
      marker.style.height = `${next.height}px`;
      marker.style.transform = finalTransform;
      marker.style.opacity = "1";

      if (animate && previous && !reduceMotion) {
        ignoreResizeUntil = performance.now() + duration + 32;
        const animation = marker.animate([
          {
            transform: `translate3d(${previous.x}px, ${previous.y}px, 0) scale(${previous.width / next.width}, ${previous.height / next.height})`,
            opacity: 1,
          },
          { transform: finalTransform, opacity: 1 },
        ], {
          duration,
          easing: "cubic-bezier(.22, 1, .36, 1)",
          fill: "both",
        });
        animation.onfinish = () => animation.cancel();
      } else if (!initializedRef.current && !reduceMotion) {
        const animation = marker.animate([
          { opacity: 0, transform: `${finalTransform} scale(.985)` },
          { opacity: 1, transform: finalTransform },
        ], {
          duration: Math.min(180, duration),
          easing: "cubic-bezier(.22, 1, .36, 1)",
          fill: "both",
        });
        animation.onfinish = () => animation.cancel();
      }

      lastRectRef.current = next;
    };

    frame = window.requestAnimationFrame(() => {
      const target = host.querySelector<HTMLElement>(selector);
      const shouldAnimate = initializedRef.current;
      if (ensureVisible && shouldAnimate && target) {
        target.scrollIntoView({
          block: "nearest",
          inline: "nearest",
          behavior: reduceMotion ? "auto" : "smooth",
        });
      }
      measure(shouldAnimate);
      initializedRef.current = true;
    });

    const scheduleMeasure = () => {
      if (performance.now() < ignoreResizeUntil) return;
      window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(() => measure(false));
    };
    const resizeObserver = new ResizeObserver(() => {
      // ResizeObserver delivers one initial notification after observe(). Do
      // not let that bookkeeping notification cancel a selection transition
      // that has just started.
      scheduleMeasure();
    });
    resizeObserver.observe(host);
    const target = host.querySelector<HTMLElement>(selector);
    if (target) resizeObserver.observe(target);
    // Async lists can insert section headers or groups above the active item
    // without changing either the host or row size. Track structural/class
    // changes so the shared layer remains aligned after those updates.
    const mutationObserver = new MutationObserver(scheduleMeasure);
    mutationObserver.observe(host, {
      subtree: true,
      childList: true,
      attributes: true,
      attributeFilter: ["class", "aria-selected", "aria-current"],
    });

    // Hydration, font metrics and asynchronously inserted groups may settle
    // without resizing the selected row. Re-measure a few times during that
    // short settling window so the shared layer cannot retain stale geometry.
    const settleTimers = [120, 420, 1000].map((delay) =>
      window.setTimeout(scheduleMeasure, delay),
    );
    void document.fonts?.ready.then(scheduleMeasure);

    return () => {
      window.cancelAnimationFrame(frame);
      settleTimers.forEach((timer) => window.clearTimeout(timer));
      resizeObserver.disconnect();
      mutationObserver.disconnect();
      marker.getAnimations().forEach((animation) => animation.cancel());
    };
  }, [duration, ensureVisible, selectedKey, selector]);

  return (
    <span
      ref={markerRef}
      className={`selection-glider ${className ? `selection-glider-${className}` : ""}`.trim()}
      aria-hidden="true"
    >
      <span className="selection-glider-surface" />
    </span>
  );
}

export default SelectionGlider;
