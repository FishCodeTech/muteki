import { EventType, type DeckState, type MutekiEvent } from "../types";
import { lane, upsertStatusChat } from "../helpers";

const text = (value: unknown) => (typeof value === "string" ? value : value == null ? "" : JSON.stringify(value));

export function reduceRunner(ev: MutekiEvent, s: DeckState): DeckState | undefined {
  const sid = ev.solver_id || "";
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.WORKER_QUEUED: {
      if (sid) {
        const l = lane(s, sid);
        s.lanes[l.solverId] = { ...l, online: true, status: "queued", statusReason: "queued", firstSeenAt: l.firstSeenAt ?? ev.ts };
      }
      upsertStatusChat(s, `worker-queued:${sid}`, {
        role: "system", kind: "status", solverId: sid || undefined, ts: ev.ts,
        content: `Worker ${sid} is waiting for execution node capacity`,
        i18nKey: p.reason === "node_concurrency_limit" ? "sys.workerQueuedNode" : "sys.workerQueued",
        i18nVars: { worker: sid, reason: text(p.reason) },
      });
      return s;
    }
    case EventType.WORKER_ERROR: {
      const reason = text(p.reason) || "error";
      const detail = text(p.detail);
      if (sid && !reason.startsWith("auxiliary_")) {
        const l = lane(s, sid);
        s.lanes[l.solverId] = { ...l, online: false, status: "error", statusReason: ["timeout", "oom", "budget"].includes(reason) ? reason : "error", finishedAt: l.finishedAt ?? ev.ts };
      }
      const unknown = /outcome_unknown|execution_unknown|transport/.test(`${reason} ${detail}`);
      upsertStatusChat(s, `worker-error:${sid}:${reason}`, {
        role: "system", kind: "status", solverId: sid || undefined, ts: ev.ts,
        content: `Worker ${sid} error (${reason})${detail ? `: ${detail}` : ""}`,
        i18nKey: unknown ? "sys.workerOutcomeUnknown" : "sys.workerError",
        i18nVars: { worker: sid, reason, detail: detail || "—" },
      });
      return s;
    }
    case EventType.WORKER_CREDENTIAL_REVOKED: {
      upsertStatusChat(s, `credential-revoked:${sid}`, {
        role: "system", kind: "status", solverId: sid || undefined, ts: ev.ts,
        content: `Credential for worker ${sid} was revoked`,
        i18nKey: p.stop_requested ? "sys.credentialRevokedStopped" : "sys.credentialRevokedUnconfirmed",
        i18nVars: { worker: sid, account: text(p.account_id) },
      });
      return s;
    }
    case EventType.RUN_RESTART_RECOVERY: {
      const state = text(p.state);
      upsertStatusChat(s, "restart-recovery", {
        role: "system", kind: "status", ts: ev.ts,
        content: text(p.detail) || `Restart recovery: ${state}`,
        i18nKey: state === "reattaching" ? "sys.restartReattaching"
          : state === "coordinator_resumed" ? "sys.restartResumed"
          : state === "remote_reattaching" ? "sys.restartRemoteReattaching"
          : state === "remote_settled" ? (p.solved === true ? "sys.restartRemoteSolved" : "sys.restartRemoteSettled")
          : "sys.restartRecovery",
        i18nVars: { workers: text(p.live_workers ?? (Array.isArray(p.seats) ? p.seats.length : "")), state, detail: text(p.detail), node: text(p.node_name ?? p.node_id) },
      });
      return s;
    }
    case EventType.RUN_QUEUE_RECOVERED: {
      const state = text(p.state);
      upsertStatusChat(s, `queue-recovered:${text(p.generation)}`, {
        role: "system", kind: "status", ts: ev.ts,
        content: `Queued start after Web restart: ${state}`,
        i18nKey: state === "resumed" ? "sys.queueResumed" : state === "interrupted_exit_confirmed" ? "sys.queueInterrupted" : "sys.queueResumeRequired",
        i18nVars: { reason: text(p.reason) || text(p.stop_error) || (p.http_status ? `HTTP ${p.http_status}` : "—") },
      });
      return s;
    }
    default:
      return undefined;
  }
}
