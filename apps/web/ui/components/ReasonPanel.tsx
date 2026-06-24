"use client";

import { ReasonView } from "@/lib/events";
import { useT } from "@/lib/i18n";

/**
 * The REASON phase: the swarm's independent planner. Shows the typed intents it
 * dispatched and its EVIDENCE AUDIT — facts it doesn't trust and refuses to
 * build on, so planning stays grounded in verified evidence rather than
 * optimistic assumptions.
 */
export function ReasonPanel({ reason }: { reason: ReasonView }) {
  const t = useT();
  const empty = reason.intents.length === 0 && reason.audit.length === 0;
  return (
    <div>
      {empty && <div style={{ color: "var(--dim)" }}>{t("empty.reasonIdle")}</div>}
      {reason.goalMet && <div className="flag">{t("reason.goalMet")}</div>}
      {reason.intents.length > 0 && (
        <div className="sg-section">
          <div className="sg-head">{t("reason.intents", { n: reason.intents.length })}</div>
          {reason.intents.map((it) => (
            <div className="intent" key={it.id}>
              <span className={`wc wc-${it.workerClass}`}>{it.workerClass}</span> {it.goal}
            </div>
          ))}
        </div>
      )}
      {reason.audit.length > 0 && (
        <div className="sg-section">
          <div className="sg-head sg-candidate">{t("reason.audit", { n: reason.audit.length })}</div>
          {reason.audit.map((a, i) => (
            <div className="dead" key={`a${i}`}>? {a}</div>
          ))}
        </div>
      )}
    </div>
  );
}
