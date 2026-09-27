/** Shared state for BLACKBOARD_DELTA domain reducers. */
import type { BlackboardView, DeckState, MutekiEvent } from "../types";

export interface BlackboardReducerContext {
  ev: MutekiEvent;
  s: DeckState;
  sid: string;
  p: Record<string, any>;
  bb: BlackboardView;
  actor: string;
  tlabel: (text: string, stableKey?: string) => void;
}
