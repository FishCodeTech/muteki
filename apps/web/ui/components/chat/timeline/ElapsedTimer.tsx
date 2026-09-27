"use client";

import { useEffect, useRef } from "react";
import { cn } from "@/lib/cn";
import { formatElapsedClock } from "./toolPresentation";

/**
 * Live elapsed clock that writes straight into its text node once a second,
 * so a running turn never re-renders the timeline just to tick.
 */
export function ElapsedTimer({
  startMs,
  className,
  format = formatElapsedClock,
}: {
  startMs: number;
  className?: string;
  format?: (ms: number) => string;
}) {
  const ref = useRef<HTMLSpanElement>(null);
  const formatRef = useRef(format);
  formatRef.current = format;

  useEffect(() => {
    const tick = () => {
      if (ref.current) ref.current.textContent = formatRef.current(Date.now() - startMs);
    };
    tick();
    const id = window.setInterval(tick, 1000);
    return () => window.clearInterval(id);
  }, [startMs]);

  return (
    <span ref={ref} className={cn("cx-tabular", className)} aria-hidden suppressHydrationWarning>
      {format(Math.max(0, Date.now() - startMs))}
    </span>
  );
}
