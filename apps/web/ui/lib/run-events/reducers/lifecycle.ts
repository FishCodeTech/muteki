/** Domain reducer extracted from ../reduce.ts; behavior intentionally unchanged. */
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
        payload: (p.runtime_payload && typeof p.runtime_payload === "object")
          ? p.runtime_payload as Record<string, unknown>
          : {},
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
      // New execution generation boundary: reset the race pill. If this generation
      // actually races, race_started will set it again; a resolve generation never
      // races, and a hard-stopped race may never have emitted race_concluded.
      s.racing = false;
      s.verifying = 0;
      if (firstStart) { s.startedAt = ev.ts; s.finishedAt = undefined; }
      if (p.challenge?.name && p.challenge.name !== s.runId) s.challengeName = p.challenge.name;
      if (typeof p.name_autogen === "boolean") s.challengeNameAutogen = p.name_autogen;
      s.category = p.challenge?.category ?? s.category;
      s.target = p.challenge?.target ?? s.target;
      if (typeof p.challenge?.expected_flags === "number") s.expectedFlags = p.challenge.expected_flags;
      if (typeof p.challenge?.multi_flag === "boolean") s.multiFlag = p.challenge.multi_flag;
      if (p.challenge?.mode === "pentest" || p.challenge?.mode === "ctf") s.mode = p.challenge.mode;
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
      // RUN_STARTED fires once PER worker; only the first one opens the thread.
      const firstStart = !prev.started;
      const wasPreparing = !!prev.preparing;
      s.started = true;
      s.preparing = false;
      s.finished = false;
      if (firstStart) { s.startedAt = ev.ts; s.finishedAt = undefined; }
      if (p.challenge?.name && p.challenge.name !== s.runId) s.challengeName = p.challenge.name;
      if (typeof p.name_autogen === "boolean") s.challengeNameAutogen = p.name_autogen;
      s.category = p.challenge?.category ?? s.category;
      s.target = p.challenge?.target ?? s.target;
      // multi-flag: pick up the target flag count + mode bit so "collecting" works
      // from the start of the run, not just at RUN_FINISHED.
      if (typeof p.challenge?.expected_flags === "number") s.expectedFlags = p.challenge.expected_flags;
      if (typeof p.challenge?.multi_flag === "boolean") s.multiFlag = p.challenge.multi_flag;
      if (p.challenge?.mode === "pentest" || p.challenge?.mode === "ctf") s.mode = p.challenge.mode;
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
      // relabel the root with the real challenge name
      if (firstStart) {
        // surface the operator's launch prompt as the opening "you" bubble, so
        // the thread shows what kicked the run off — not just a system line.
        const prompt = userPromptBubble(p.challenge);
        if (prompt) pushChat(s, { role: "human", kind: "text", content: prompt, ts: ev.ts });
      }
      if (firstStart || wasPreparing) pushChat(s, { role: "system", kind: "status", content: `Run started — ${s.challengeName || s.runId}`, ts: ev.ts, i18nKey: "sys.runStarted", i18nVars: { name: s.challengeName || s.runId } });
      break;
    }
    case EventType.PROJECTION_INCOMPLETE: {
      // Redacted, non-terminal startup diagnostic. It intentionally has no visible
      // progress/success side effect; operators can inspect durable raw history.
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
        // I: carry intent/tokens/paused when the status payload (or its runtime
        // block) supplies them; sticky otherwise.
        intentId: (p.intent_id ?? p.runtime?.intent_id ?? l.intentId ?? "").toString() || undefined,
        tokensSpent: typeof p.tokens_spent === "number" ? p.tokens_spent
          : (typeof p.runtime?.tokens_spent === "number" ? p.runtime.tokens_spent : l.tokensSpent),
        paused: p.reason === "paused" ? true : (online ? (l.paused ?? false) : false),
        // sticky: a later status emit with no session must not wipe a known id.
        session: (p.session ?? l.session ?? "").toString() || undefined,
        runtime: (p.runtime && typeof p.runtime === "object") ? p.runtime : l.runtime,
        firstSeenAt: l.firstSeenAt ?? (online ? ev.ts : undefined),
        finishedAt: online ? undefined : (l.finishedAt ?? ev.ts),
        spawnPhase: l.spawnPhase ?? phase,
      }, p);
      break;
    }
    case EventType.WORKER_LIFECYCLE: {
      // I: granular lifecycle — spawned | phase_changed | stalled | exited. Updates
      // this worker's lane without disturbing its WORKER_STATUS online/offline state.
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
      // Display-only rail labels. Adopt a title when the operator did not
      // supply one, or when an older run still has an autogenerated slug.
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
    default:
      return undefined;
  }
  return s;
}
