"use client";

import { useEngines, EngineStatus } from "@/lib/useRun";
import { useT } from "@/lib/i18n";

const LABELS: Record<string, string> = {
  cursor: "Cursor",
  claude: "Claude",
  codex: "Codex",
};

/** Why an engine is degraded, or "" when it's fine. Two independent signals:
 *  · run-scoped: the current run dropped it at dispatch time (degradedEngines).
 *  · global: the /api/engines deep probe says it can't complete a turn (healthy
 *    === false) — covers the no-active-run case.
 *  The run-scoped reason wins (it's the one the operator just hit). */
function degradeReason(e: EngineStatus, runDegraded?: Record<string, string>): string {
  const fromRun = runDegraded?.[e.engine];
  if (fromRun) return fromRun;
  if (e.healthy === false) return e.health_detail || "health check failed";
  return "";
}

export function EngineBar({ degradedEngines }: { degradedEngines?: Record<string, string> } = {}) {
  const t = useT();
  const engines = useEngines();
  if (engines.length === 0) return null;
  return (
    <span className="engine-bar">
      {engines.map((e) => {
        const degraded = degradeReason(e, degradedEngines);
        const cls = degraded ? "down degraded" : e.available ? "up" : "down";
        return (
          <span key={e.engine} className={`engine-pill ${cls}`}>
            <span className="engine-dot" />
            {LABELS[e.engine] || e.engine}
            {degraded && (
              <span className="engine-pct degraded-tag">{t("engines.down")}</span>
            )}
            {degraded ? (
              <span className="engine-pop" role="tooltip">
                <span className="engine-pop-head">
                  <b>{LABELS[e.engine] || e.engine}</b>
                </span>
                <span className="engine-pop-note engine-pop-degraded">
                  {t("engines.down")} · {degraded}
                </span>
              </span>
            ) : (
              <span className="engine-pop" role="tooltip">
                <span className="engine-pop-head">
                  <b>{LABELS[e.engine] || e.engine}</b>
                </span>
                <span className="engine-pop-note">
                  {e.available ? t("engines.up") : t("engines.down")}
                </span>
              </span>
            )}
          </span>
        );
      })}
    </span>
  );
}
