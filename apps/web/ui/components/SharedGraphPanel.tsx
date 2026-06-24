"use client";

import { SharedGraphView } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { Icon } from "@/components/Icon";

/**
 * The SHARED, provenance-gated graph. Unlike the per-solver SolveGraph, this
 * splits VERIFIED evidence (the fact's content was located in a real artifact —
 * the witness check) from CANDIDATES (unverified, downweighted, NOT to be
 * treated as established). The split is what makes this a shared, evidence-
 * bearing solve graph: every fact carries a verdict rather than being a self-
 * reported string.
 */
export function SharedGraphPanel({ graph }: { graph: SharedGraphView }) {
  const t = useT();
  const empty = graph.verified.length === 0 && graph.candidates.length === 0;
  return (
    <div>
      {empty && <div style={{ color: "var(--dim)" }}>{t("empty.sharedEmpty")}</div>}
      {graph.verified.length > 0 && (
        <div className="sg-section">
          <div className="sg-head sg-verified">{t("sg.verified", { n: graph.verified.length })}</div>
          {graph.verified.map((e, i) => (
            <div className="evi" key={`v${i}`} title={`${e.verifier} · ${e.actor}`}>
              <Icon name="check" size={13} /> {e.fact}
            </div>
          ))}
        </div>
      )}
      {graph.candidates.length > 0 && (
        <div className="sg-section">
          <div className="sg-head sg-candidate">
            {t("sg.candidates", { n: graph.candidates.length })}
          </div>
          {graph.candidates.map((e, i) => (
            <div className="cand" key={`c${i}`} title={`${e.verifier} · ${e.actor}`}>
              <span className="conf">{e.confidence.toFixed(1)}</span> {e.fact}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
