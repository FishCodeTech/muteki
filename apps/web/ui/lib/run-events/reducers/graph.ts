/** Domain reducer extracted from ../reduce.ts; behavior intentionally unchanged. */
import {
  EventType,
  type DeckState,
  type MutekiEvent,
  type SharedEvidence,
} from "../types";
import {
  isInvalidatedFlag,
  mergeFlags,
  pushChat,
} from "../helpers";

export function reduceGraph(ev: MutekiEvent, s: DeckState): DeckState | undefined {
  const sid = ev.solver_id || "";
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.SOLVE_GRAPH_DELTA:
      if (p.kind === "evidence_added") s.graph.evidence = [...s.graph.evidence, p.fact].slice(-50);
      else if (p.kind === "flag" && !isInvalidatedFlag(s, p.flag)) {
        s.graph.flag = p.flag;
        mergeFlags(s, p.flag);
      } else if (p.kind === "dead_end") {
        s.graph.deadEnds = [...s.graph.deadEnds, p.reason];
      }
      break;
    case EventType.INSIGHT_BUS_EVENT: {
      const line = `${p.kind}: ${p.flag ?? p.text ?? ""}${sid ? " (" + sid + ")" : ""}`;
      s.insights = [...s.insights, line].slice(-50);
      pushChat(s, { role: "system", kind: "insight", content: `insight · ${line}`, ts: ev.ts });
      break;
    }
    case EventType.SHARED_GRAPH_DELTA: {
      const row: SharedEvidence = {
        fact: p.fact ?? "", verified: !!p.verified, confidence: p.confidence ?? 0,
        actor: p.actor ?? sid ?? "", verifier: p.verifier ?? "",
      };
      if (row.verified) s.sharedGraph.verified = [...s.sharedGraph.verified, row].slice(-50);
      else s.sharedGraph.candidates = [...s.sharedGraph.candidates, row].slice(-50);
      break;
    }
    case EventType.REASON_INTENT: {
      const intents = (p.intents ?? []).map((it: any) => ({
        id: it.id ?? it.intent_id ?? "", goal: it.goal ?? "",
        workerClass: it.worker_class ?? "code",
      }));
      s.reason = { goalMet: !!p.goal_met, intents, audit: p.audit ?? [] };
      break;
    }
    default:
      return undefined;
  }
  return s;
}
