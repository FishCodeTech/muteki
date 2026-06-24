"use client";

import { ContextGauge } from "@/lib/events";
import { useT } from "@/lib/i18n";

const COLORS = ["#4ea1ff", "#d29922", "#3fb950", "#bc8cff", "#6b7385"];

/** Context window "fuel gauge" (§14.3 #2): stacked zones vs the model limit,
 *  driven by CONTEXT_STATE events. */
export function ContextGaugeBar({ gauge }: { gauge: ContextGauge }) {
  const t = useT();
  const limit = gauge.limit || Math.max(gauge.total, 1);
  return (
    <div>
      <div className="gauge">
        {gauge.zones.map((z, i) => {
          const pct = Math.min(100, (z.tokens / limit) * 100);
          return (
            <span
              key={i}
              title={`${z.label}: ${z.tokens}`}
              style={{ width: `${pct}%`, background: COLORS[i % COLORS.length] }}
            />
          );
        })}
      </div>
      <div style={{ color: "var(--dim)", marginTop: 4 }}>
        {t("empty.tokens", { total: gauge.total, limit: gauge.limit || "?" })}
      </div>
    </div>
  );
}
