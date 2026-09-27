/** BLACKBOARD_DELTA dispatcher; domain projections live in sibling reducers. */
import { EventType, type DeckState, type MutekiEvent } from "../types";
import { gid } from "../helpers";
import { reduceBlackboardControl } from "./blackboard-control";
import { reduceBlackboardKnowledge } from "./blackboard-knowledge";
import { reduceBlackboardResults } from "./blackboard-results";
import { reduceBlackboardRuntime } from "./blackboard-runtime";
import type { BlackboardReducerContext } from "./blackboard-context";

export function reduceBlackboard(ev: MutekiEvent, s: DeckState): DeckState | undefined {
  if (ev.event_type !== EventType.BLACKBOARD_DELTA) return undefined;

  const sid = ev.solver_id || "";
  const p = ev.payload || {};
  const bb = s.blackboard;
  const actor = p.actor ?? sid ?? "";
  if (actor && !bb.workers.includes(actor)) bb.workers = [...bb.workers, actor];
  const tlabel = (txt: string, stableKey?: string) => {
    const id = stableKey ? `bbe:${stableKey}` : gid("bbe");
    const row = { id, kind: p.kind, actor, ts: ev.ts, label: txt };
    const index = stableKey ? bb.events.findIndex((item) => item.id === id) : -1;
    bb.events = index >= 0
      ? bb.events.map((item, itemIndex) => itemIndex === index ? row : item)
      : [...bb.events, row].slice(-300);
  };
  const context: BlackboardReducerContext = {
    ev, s, sid, p, bb, actor, tlabel,
  };
  if (reduceBlackboardKnowledge(context)) return s;
  if (reduceBlackboardResults(context)) return s;
  if (reduceBlackboardRuntime(context)) return s;
  reduceBlackboardControl(context);
  return s;
}
