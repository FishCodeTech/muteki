import {
  EventType,
  type DeckState,
  type MutekiEvent,
  type SolverLane,
} from "../types";
import {
  lane,
  pushChat,
  reviewRoleFromPhase,
  roleFromWorkerRole,
  readPlatformConfirmationRequired,
  taskContractView,
  userPromptBubble,
  withLaneIdentity,
} from "../helpers";
import { defaultProductionGuardState, normalizeGuardStateSnapshot } from "./guard";

export function reduceLifecycle(prev: DeckState, ev: MutekiEvent, s: DeckState): DeckState | undefined {
  const sid = ev.solver_id || "";
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.AGENT_RUNTIME_EVENT: {
      s.runtimeEvents = [...s.runtimeEvents, {
        id: `${ev.solver_id || "runtime"}:${p.runtime_seq ?? ev.seq}:${p.agent_event_type ?? "event"}`,
        solverId: ev.solver_id || "runtime",
        eventType: String(p.agent_event_type || "runtime.event"),
        adapter: String(p.runtime_adapter || "") || undefined,
        nativeType: String(p.native_type || "") || undefined,
        payload: {
          ...(p.runtime_payload && typeof p.runtime_payload === "object"
            ? p.runtime_payload as Record<string, unknown> : {}),
          ...(p.turn_id != null ? { turn_id: p.turn_id } : {}),
          ...(p.agent_session_id != null ? { agent_session_id: p.agent_session_id } : {}),
          ...(p.execution_generation != null ? { execution_generation: p.execution_generation } : {}),
        },
        ts: ev.ts,
      }].slice(-500);
      return s;
    }
    case EventType.RUN_PREPARING: {
      const firstStart = !prev.started;
      s.started = true;
      s.preparing = true;
      s.finished = false;
      s.preflightFailures = [];
      s.racing = false;
      s.verifying = 0;
      if (firstStart) s.startedAt = ev.ts;
      if (s.activeSince == null) s.activeSince = ev.ts;
      s.finishedAt = undefined;
      if (p.challenge?.name && p.challenge.name !== s.runId) s.challengeName = p.challenge.name;
      if (typeof p.name_autogen === "boolean") s.challengeNameAutogen = p.name_autogen;
      s.category = p.challenge?.category ?? s.category;
      s.target = p.challenge?.target ?? s.target;
      if (typeof p.challenge?.expected_flags === "number") s.expectedFlags = p.challenge.expected_flags;
      if (typeof p.challenge?.multi_flag === "boolean") s.multiFlag = p.challenge.multi_flag;
      if (p.challenge?.mode === "pentest" || p.challenge?.mode === "ctf") s.mode = p.challenge.mode;
      const preparingGuard = p.production_guard ?? p.challenge?.production_guard;
      if (preparingGuard?.enabled === true || preparingGuard?.mode === "protected") {
        s.productionGuard = normalizeGuardStateSnapshot(preparingGuard, s.productionGuard) ?? defaultProductionGuardState(true);
      }
      if (p.kind === "local_pi" || p.driver_kind === "local_pi") {
        s.driverKind = "local_pi";
      } else if (p.kind === "swarm") {
        s.driverKind = "swarm";
      }
      if (readPlatformConfirmationRequired(p.challenge)) s.platformConfirmationRequired = true;
      const preparingContract = taskContractView(p.challenge);
      if (preparingContract) {
        s.taskContract = preparingContract;
        if (["outcome", "count", "coverage"].includes(preparingContract.completion.kind)) {
          s.completionKind = preparingContract.completion.kind as "outcome" | "count" | "coverage";
        }
        if (preparingContract.completion.quantity != null && preparingContract.mode === "pentest") {
          s.expectedFindings = preparingContract.completion.quantity;
        }
        if (["first_valid_report", "command_execution", "shell_access", "admin_access"].includes(preparingContract.completion.outcomePredicate)) {
          s.outcomePredicate = preparingContract.completion.outcomePredicate as DeckState["outcomePredicate"];
        }
      }
      if (typeof p.challenge?.expected_findings === "number") s.expectedFindings = p.challenge.expected_findings;
      if (typeof p.challenge?.engagement?.expected_findings === "number") {
        s.expectedFindings = p.challenge.engagement.expected_findings;
      }
      if (["outcome", "count", "coverage"].includes(String(p.challenge?.engagement?.completion_kind))) {
        s.completionKind = p.challenge.engagement.completion_kind;
      }
      if (["first_valid_report", "command_execution", "shell_access", "admin_access"].includes(String(p.challenge?.engagement?.outcome_predicate))) {
        s.outcomePredicate = p.challenge.engagement.outcome_predicate;
      }
      if (firstStart) {
        const prompt = userPromptBubble(p.challenge);
        if (prompt) pushChat(s, { role: "human", kind: "text", content: prompt, ts: ev.ts });
        pushChat(s, {
          role: "system", kind: "status", content: "Checking Worker availability…",
          ts: ev.ts, i18nKey: "sys.preparing",
        });
      }
      break;
    }
    case EventType.RUN_STARTED: {
      const firstStart = !prev.started;
      const wasPreparing = !!prev.preparing;
      s.started = true;
      s.preparing = false;
      s.finished = false;
      if (firstStart) s.startedAt = ev.ts;
      if (s.activeSince == null) s.activeSince = ev.ts;
      s.finishedAt = undefined;
      if (p.challenge?.name && p.challenge.name !== s.runId) s.challengeName = p.challenge.name;
      if (typeof p.name_autogen === "boolean") s.challengeNameAutogen = p.name_autogen;
      s.target = p.challenge?.target ?? s.target;
      if (typeof p.challenge?.expected_flags === "number") s.expectedFlags = p.challenge.expected_flags;
      if (typeof p.challenge?.multi_flag === "boolean") s.multiFlag = p.challenge.multi_flag;
      if (p.challenge?.mode === "pentest" || p.challenge?.mode === "ctf") s.mode = p.challenge.mode;
      const preparingGuard = p.production_guard ?? p.challenge?.production_guard;
      if (preparingGuard?.enabled === true || preparingGuard?.mode === "protected") {
        s.productionGuard = normalizeGuardStateSnapshot(preparingGuard, s.productionGuard) ?? defaultProductionGuardState(true);
      }
      if (p.kind === "local_pi" || p.driver_kind === "local_pi") {
        s.driverKind = "local_pi";
      } else if (p.kind === "swarm") {
        s.driverKind = "swarm";
      }
      if (readPlatformConfirmationRequired(p.challenge)) s.platformConfirmationRequired = true;
      const startedContract = taskContractView(p.challenge);
      if (startedContract) {
        s.taskContract = startedContract;
        if (["outcome", "count", "coverage"].includes(startedContract.completion.kind)) {
          s.completionKind = startedContract.completion.kind as "outcome" | "count" | "coverage";
        }
        if (startedContract.completion.quantity != null && startedContract.mode === "pentest") {
          s.expectedFindings = startedContract.completion.quantity;
        }
        if (["first_valid_report", "command_execution", "shell_access", "admin_access"].includes(startedContract.completion.outcomePredicate)) {
          s.outcomePredicate = startedContract.completion.outcomePredicate as DeckState["outcomePredicate"];
        }
      }
      if (typeof p.challenge?.expected_findings === "number") s.expectedFindings = p.challenge.expected_findings;
      if (typeof p.challenge?.engagement?.expected_findings === "number") {
        s.expectedFindings = p.challenge.engagement.expected_findings;
      }
      if (["outcome", "count", "coverage"].includes(String(p.challenge?.engagement?.completion_kind))) {
        s.completionKind = p.challenge.engagement.completion_kind;
      }
      if (["first_valid_report", "command_execution", "shell_access", "admin_access"].includes(String(p.challenge?.engagement?.outcome_predicate))) {
        s.outcomePredicate = p.challenge.engagement.outcome_predicate;
      }
      if (sid) {
        const l = lane(s, sid);
        s.lanes[l.solverId] = { ...l, online: true, status: "online", statusReason: "started" };
      }
      if (firstStart) {
        const prompt = userPromptBubble(p.challenge);
        if (prompt) pushChat(s, { role: "human", kind: "text", content: prompt, ts: ev.ts });
      }
      if (firstStart || wasPreparing) pushChat(s, { role: "system", kind: "status", content: `Run started — ${s.challengeName || s.runId}`, ts: ev.ts, i18nKey: "sys.runStarted", i18nVars: { name: s.challengeName || s.runId } });
      break;
    }
    case EventType.PROJECTION_INCOMPLETE: {
      break;
    }
    case EventType.WORKER_STATUS: {
      const l = lane(s, ev.solver_id);
      const online = p.online !== false;
      const phase = (p.phase ?? l.phase ?? "").toString() || undefined;
      const role = roleFromWorkerRole(p.worker_role) ?? reviewRoleFromPhase(phase) ?? l.role;
      s.lanes[l.solverId] = withLaneIdentity({
        ...l,
        online,
        status: (p.status ?? (online ? "online" : "offline")).toString(),
        statusReason: (p.reason ?? l.statusReason ?? "").toString(),
        engine: (p.engine ?? l.engine ?? "").toString() || undefined,
        role,
        phase,
        intentId: (p.intent_id ?? p.runtime?.intent_id ?? l.intentId ?? "").toString() || undefined,
        tokensSpent: typeof p.tokens_spent === "number" ? p.tokens_spent
          : (typeof p.runtime?.tokens_spent === "number" ? p.runtime.tokens_spent : l.tokensSpent),
        paused: p.reason === "paused" ? true : (online ? (l.paused ?? false) : false),
        session: (p.session ?? l.session ?? "").toString() || undefined,
        runtime: (p.runtime && typeof p.runtime === "object") ? p.runtime : l.runtime,
        firstSeenAt: l.firstSeenAt ?? (online ? ev.ts : undefined),
        finishedAt: online ? undefined : (l.finishedAt ?? ev.ts),
        spawnPhase: l.spawnPhase ?? phase,
      }, p);
      break;
    }
    case EventType.WORKER_LIFECYCLE: {
      const l = lane(s, ev.solver_id);
      const phaseName = (p.phase ?? "").toString();
      const next: SolverLane = { ...l };
      if (typeof p.tokens_spent === "number") next.tokensSpent = p.tokens_spent;
      if (p.intent_id) next.intentId = String(p.intent_id);
      if (typeof p.paused === "boolean") next.paused = p.paused;
      next.role = roleFromWorkerRole(p.worker_role) ?? next.role;
      if (phaseName === "spawned") {
        next.phase = (p.phase_label ?? next.phase ?? "").toString() || undefined;
        next.online = true;
        next.firstSeenAt = next.firstSeenAt ?? ev.ts;
        next.finishedAt = undefined;
        next.spawnPhase = next.spawnPhase ?? next.phase;
      } else if (phaseName === "phase_changed") {
        if (p.phase_label) next.phase = String(p.phase_label);
      } else if (phaseName === "stalled") {
        next.status = "stalled";
      } else if (phaseName === "exited") {
        next.online = false;
        next.finishedAt = next.finishedAt ?? ev.ts;
      }
      s.lanes[l.solverId] = withLaneIdentity(next, p);
      break;
    }
    case EventType.RUN_TITLED: {
      const title = p.title ?? "";
      if (title && (!s.challengeName || s.challengeNameAutogen)) {
        s.challengeName = title;
        s.challengeNameAutogen = false;
      }
      const category = typeof p.category === "string" ? p.category : "";
      if (category && !s.category) {
        s.category = category;
      }
      break;
    }
    case EventType.LOCAL_PI_SESSION: {
      s.driverKind = "local_pi";
      if (typeof p.chat_thread_id === "string" && p.chat_thread_id) s.chatThreadId = p.chat_thread_id;
      s.awaitingOperator = undefined;
      const prev = s.localPiSession || {};
      s.localPiSession = {
        ...prev,
        providerSessionId: p.provider_session_id ?? prev.providerSessionId ?? prev.provider_session_id,
        resumed: p.resumed == null ? !!prev.resumed : !!p.resumed,
        sessionState: "running",
        turnActive: true,
        turnCount: p.turn_index ?? prev.turnCount ?? prev.turn_count ?? 1,
        model: {
          ...(prev.model || {}),
          id: p.model ?? prev.model?.id ?? "default",
          readiness: "ready",
        },
      };
      break;
    }
    case EventType.LOCAL_PI_TURN_COMPLETED: {
      s.driverKind = "local_pi";
      const interrupted = !!p.interrupted;
      s.awaitingOperator = interrupted ? "turn_interrupted" : "turn_completed";
      const prev = s.localPiSession || {};
      s.localPiSession = {
        ...prev,
        sessionState: interrupted ? "interrupted" : "awaiting_operator",
        turnActive: false,
        turnCount: p.turn_index ?? prev.turnCount ?? prev.turn_count ?? 1,
        lastTurnUsage: p.usage ?? prev.lastTurnUsage ?? prev.last_turn_usage,
      };
      if (s.chat.length > 0) {
        s.chat = s.chat.map((m, idx) =>
          idx === s.chat.length - 1 && m.role === "agent" && !m.sealed
            ? { ...m, sealed: true }
            : m
        );
      }
      break;
    }
    default:
      return undefined;
  }
  return s;
}
