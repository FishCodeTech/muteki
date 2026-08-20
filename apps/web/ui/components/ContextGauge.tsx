"use client";

import { ContextGauge } from "@/lib/events";
import { useT } from "@/lib/i18n";

// zone colours are palette tokens (resolved inline so the stacked bar tracks
// the active scheme); the last slot is the quiet "free/headroom" zone.
const ZONE_TOKENS = ["--blue", "--amber", "--green", "--violet", "--dim"];

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
              style={{ width: `${pct}%`, background: `var(${ZONE_TOKENS[i % ZONE_TOKENS.length]})` }}
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
