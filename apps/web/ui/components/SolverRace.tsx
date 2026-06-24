"use client";

import { SolverLane } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { Icon } from "@/components/Icon";

const RECENT_TOOL_LIMIT = 8;

/** Side-by-side solver lanes (§14.3 #3): streaming reasoning + tool results;
 *  the lane that found the flag is highlighted. */
export function SolverRace({ lanes }: { lanes: SolverLane[] }) {
  const t = useT();
  if (lanes.length === 0) return <div style={{ color: "var(--dim)" }}>{t("empty.noSolvers")}</div>;
  return (
    <div role="list" aria-label={t("panel.solverRace")}>
      {lanes.map((l) => (
        <article key={l.solverId} className={`lane ${l.solved ? "solved" : ""}`} role="listitem">
          <div className="head">
            <span className="sid">{l.solverId}</span>
            <span className="status">{l.status}</span>
            <span className="status">{t("solverRace.tools", { n: l.toolLines.length })}</span>
            {l.solved && <span className="flag"><Icon name="flag" size={13} /> {l.flag}</span>}
          </div>
          <div className="reasoning">
            {l.reasoning || <span style={{ color: "var(--dim)" }}>{t("empty.noOutput")}</span>}
          </div>
          {hiddenToolCount(l) > 0 && (
            <div className="tool" style={{ color: "var(--dim)" }}>
              ↳ {t("solverRace.earlierTools", { n: hiddenToolCount(l) })}
            </div>
          )}
          {recentTools(l).map((line, i) => (
            <div className="tool" key={`${l.solverId}-tool-${i}`}>↳ {line}</div>
          ))}
        </article>
      ))}
    </div>
  );
}

function recentTools(lane: SolverLane) {
  return lane.toolLines.slice(-RECENT_TOOL_LIMIT);
}

function hiddenToolCount(lane: SolverLane) {
  return Math.max(0, lane.toolLines.length - RECENT_TOOL_LIMIT);
}
