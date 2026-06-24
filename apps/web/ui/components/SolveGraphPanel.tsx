"use client";

import { SolveGraphView } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { Icon } from "@/components/Icon";

/** Evidence / hypotheses / dead-ends, the live solve graph (§14.3 #5). */
export function SolveGraphPanel({ graph }: { graph: SolveGraphView }) {
  const t = useT();
  return (
    <div>
      {graph.flag && <div className="flag"><Icon name="flag" size={14} /> {graph.flag}</div>}
      {graph.evidence.length === 0 && graph.deadEnds.length === 0 && (
        <div style={{ color: "var(--dim)" }}>{t("empty.noEvidence")}</div>
      )}
      {graph.evidence.map((e, i) => (
        <div className="evi" key={`e${i}`}><Icon name="check" size={13} /> {e}</div>
      ))}
      {graph.hypotheses.map((h) => (
        <div key={h.id}>
          [{h.id} {h.status}] {h.statement}
        </div>
      ))}
      {graph.deadEnds.map((d, i) => (
        <div className="dead" key={`d${i}`}><Icon name="xCircle" size={13} /> {d}</div>
      ))}
    </div>
  );
}
