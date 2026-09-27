"use client";

import { useState } from "react";

import { useT } from "@/lib/i18n";

/**
 * 22×10 event-rate polyline. Click swaps to token-rate when that series has
 * any samples. A dashed stroke marks a window that the 500-event cap clipped.
 */
export function AgentSparkline({
  events,
  tokens,
  partial,
}: {
  events: readonly number[];
  tokens: readonly number[];
  partial: boolean;
}) {
  const t = useT();
  const [kind, setKind] = useState<"events" | "tokens">("events");
  const hasTokens = tokens.some((value) => value > 0);
  const series = kind === "tokens" && hasTokens ? tokens : events;
  const max = Math.max(1, ...series);
  const last = Math.max(0, series.length - 1);
  const points = series.map((value, index) => {
    const x = last === 0 ? 0 : (index / last) * 21;
    const y = 9 - (value / max) * 8;
    return `${x},${y}`;
  }).join(" ");
  const label = t(kind === "tokens" && hasTokens ? "collab.sparkline.tokens" : "collab.sparkline.events");
  const title = partial ? `${label} · ${t("collab.sparkline.partial")}` : label;
  return (
    <button
      type="button"
      className={`collab-sparkline ${partial ? "partial" : ""}`}
      aria-label={title}
      title={title}
      onPointerDown={(event) => event.stopPropagation()}
      onClick={(event) => {
        event.stopPropagation();
        if (hasTokens) setKind((current) => (current === "events" ? "tokens" : "events"));
      }}
    >
      <svg viewBox="0 0 22 10" width={22} height={10} aria-hidden="true">
        <polyline
          points={points}
          fill="none"
          stroke="var(--agent-color)"
          strokeWidth="1.25"
          strokeLinejoin="round"
          strokeLinecap="round"
        />
      </svg>
    </button>
  );
}
