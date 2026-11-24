import { EventType, type DeckState, type MutekiEvent, type WorkerPromptRecord } from "../types";
import { pushChat } from "../helpers";

export function reduceWorkerPrompt(ev: MutekiEvent, state: DeckState): DeckState | undefined {
  if (ev.event_type !== EventType.WORKER_PROMPT) return undefined;
  const workerId = ev.solver_id;
  const payload = ev.payload;
  if (
    state.driverKind === "local_pi"
    && payload?.main_thread === true
    && (payload.kind === "followup" || payload.kind === "continue")
    && typeof payload.prompt === "string"
    && payload.prompt.trim()
  ) {
    pushChat(state, {
      role: "human",
      mainThread: true,
      kind: "text",
      content: payload.prompt.trim(),
      ts: ev.ts,
    });
  }
  const id = payload?.prompt_id;
  if (!workerId || typeof id !== "string" || !id) return state;
  const rows = state.workerPrompts[workerId] ?? [];
  const previous = rows.find((row) => row.id === id);
  if (!previous && typeof payload.prompt !== "string") return state;
  const status: WorkerPromptRecord["status"] = ["prepared", "sent", "not_sent", "unknown"].includes(payload.status)
    ? payload.status : previous?.status ?? "prepared";
  if (previous && previous.status !== "prepared" && status === "prepared") return state;
  const record: WorkerPromptRecord = previous ? {
    ...previous, status, updatedAt: ev.ts,
  } : {
    id, workerId, prompt: payload.prompt,
    kind: payload.kind === "resume" ? "resume" : "execute",
    status, transport: payload.transport === "stdin" ? "stdin" : "argv",
    redacted: payload.redacted === true,
    session: typeof payload.session === "string" ? payload.session : undefined,
    engine: typeof payload.engine === "string" ? payload.engine : undefined,
    model: typeof payload.model === "string" ? payload.model : undefined,
    intentId: typeof payload.intent_id === "string" ? payload.intent_id : undefined,
    phase: typeof payload.phase === "string" ? payload.phase : undefined,
    ts: ev.ts, updatedAt: ev.ts,
  };
  state.workerPrompts = {
    ...state.workerPrompts,
    [workerId]: previous ? rows.map((row) => row.id === id ? record : row) : [...rows, record],
  };
  return state;
}
