/** runtime BLACKBOARD_DELTA projection; event behavior is unchanged. */
import {
  lane,
  pushChat,
  reviewRoleFromPhase,
  roleFromWorkerRole,
  upsertStatusChat,
  withLaneIdentity,
} from "../helpers";

import type { BlackboardReducerContext } from "./blackboard-context";

export function reduceBlackboardRuntime({ ev, s, p, bb, actor, tlabel }: BlackboardReducerContext): boolean {
  switch (p.kind) {
        case "runtime_degraded":
        case "worker_backend_degraded": {
          tlabel(`${actor} runtime degraded: ${p.reason ?? p.backend ?? "unknown"}`);
          const sid = p.solver_id ?? p.engine ?? actor;
          if (sid) {
            const l = lane(s, sid);
            s.lanes[l.solverId] = { ...l, runtime: {
              backend: p.backend ?? p.requested_backend ?? "local",
              status: p.status ?? "degraded",
            } };
          }
          break;
        }
        case "engine_degraded": {
          // an engine was dropped from (or restored to) the run roster by a
          // dispatch-time health check. Track engine → reason so the worker panel /
          // engine bar can show "cursor degraded: Authentication required" instead
          // of the engine silently never appearing. status="recovered" clears it.
          const eng = String(p.engine ?? "");
          if (eng) {
            const next = { ...s.degradedEngines };
            if (p.status === "recovered") {
              delete next[eng];
              tlabel(`engine ${eng} recovered`);
            } else {
              next[eng] = String(p.reason ?? "unavailable");
              tlabel(`engine ${eng} degraded: ${p.reason ?? "unavailable"}`);
            }
            s.degradedEngines = next;
          }
          break;
        }
        case "phase_transition": {
          tlabel(`${p.from ?? "phase"} → ${p.to ?? "phase"}`);
          break;
        }
        case "worker_budget_exhausted":
        case "cost_budget_exhausted": {
          tlabel(`${p.kind ?? "budget"} exhausted`);
          break;
        }
        case "need_input": {
          // a worker flagged it needs operator input (mirror of the HITL_REQUEST).
          tlabel(`needs input: ${(p.need ?? "").toString().slice(0, 80)}`);
          break;
        }
        case "awaiting_operator": {
          // the coordinator PAUSED — it will not re-spawn until the operator acts.
          const ask = (p.reason ?? "waiting for operator input").toString();
          s.awaitingOperator = ask;
          tlabel(`awaiting operator: ${ask.slice(0, 80)}`);
          pushChat(s, {
            role: "system", kind: "status",
            content: `Paused — waiting for you: ${ask}`, ts: ev.ts,
          });
          break;
        }
        case "operator_paused": {
          const ask = (p.reason ?? "operator paused").toString();
          s.awaitingOperator = ask;
          tlabel(`operator paused: ${ask.slice(0, 80)}`);
          pushChat(s, {
            role: "system", kind: "status",
            content: "Paused by operator.", ts: ev.ts,
          });
          break;
        }
        case "operator_resumed": {
          s.awaitingOperator = undefined;
          tlabel("▶ operator responded — resuming");
          pushChat(s, { role: "system", kind: "status",
            content: "▶ Resuming with your input.", ts: ev.ts });
          break;
        }
        case "worker_spawned": {
          // a worker joined (bootstrap / explore / rebootstrap / operator). The
          // real worker id is in `worker`; register it for lanes/legend. Lane
          // presence flips via WORKER_STATUS; here we just narrate + remember it.
          const w = p.worker ?? actor;
          const phase = (p.phase ?? "").toString() || undefined;
          if (w && !bb.workers.includes(w)) bb.workers = [...bb.workers, w];
          if (w) {
            const l = lane(s, w);
            s.lanes[l.solverId] = withLaneIdentity({
              ...l,
              role: roleFromWorkerRole(p.worker_role) ?? reviewRoleFromPhase(phase) ?? l.role,
              phase: phase ?? l.phase,
              intentId: (p.intent_id ?? l.intentId ?? "").toString() || undefined,
              statusReason: phase ?? l.statusReason,
              raceScout: phase === "race" ? true : l.raceScout,
              firstSeenAt: l.firstSeenAt ?? ev.ts,
              finishedAt: undefined,
              spawnPhase: phase ?? l.spawnPhase,
              spawnedBy: actor || l.spawnedBy,
            }, p);
          }
          if (phase === "verifier") {
            s.verifying = (s.verifying ?? 0) + 1;
          }
          tlabel(`+ ${w} spawned${p.phase ? ` (${p.phase})` : ""}`);
          break;
        }
        case "worker_killed": {
          // operator stopped a specific worker — mark its lane offline so the dock
          // greys it immediately (WORKER_FINISHED also lands, but may lag the kill).
          const w = p.worker;
          if (w && s.lanes[w]) {
            s.lanes[w] = { ...s.lanes[w], online: false, status: "killed", statusReason: "killed", finishedAt: ev.ts };
          }
          tlabel(`${w ?? actor} killed`);
          break;
        }
        case "worker_spawn_rejected": {
          const why = p.reason === "max_workers"
            ? "at max workers" : p.reason === "unknown_engine"
            ? `engine not in roster${p.engine ? ` (${p.engine})` : ""}` : (p.reason ?? "rejected");
          tlabel(`spawn rejected: ${why}`);
          pushChat(s, { role: "system", kind: "status",
            content: `Could not add worker — ${why}.`, ts: ev.ts });
          break;
        }
        case "worker_finished": {
          // coordinator-level reap narration (the lane itself is handled by the
          // WORKER_FINISHED event); just add a timeline line.
          const w = (p.worker ?? actor)?.toString();
          if (w) {
            const l = lane(s, w);
            s.lanes[l.solverId] = {
              ...l,
              raceScout: p.phase === "race" || l.raceScout ? true : l.raceScout,
              online: false,
              status: p.result === "solved" ? "SOLVED" : "done",
              statusReason: p.result === "solved" ? "solved" : p.result === "error" ? "error" : "finished",
              finishedAt: l.finishedAt ?? ev.ts,
            };
          }
          tlabel(`− ${p.worker ?? actor} finished${p.result ? ` (${p.result})` : ""}`);
          break;
        }
        case "race_worker_finished": {
          const w = (p.worker ?? actor)?.toString();
          const finished = Number(p.finished ?? 0);
          const total = Number(p.total ?? 0);
          if (total > 0) s.raceTotal = total;
          if (finished > 0) s.raceFinished = finished;
          if (w) {
            const l = lane(s, w);
            s.lanes[l.solverId] = {
              ...l,
              raceScout: true,
              online: false,
              status: p.result === "solved" ? "SOLVED" : "done",
              statusReason: p.result === "solved" ? "solved" : "finished",
              finishedAt: l.finishedAt ?? ev.ts,
            };
          }
          tlabel(`race worker finished ${finished}/${total || "?"}`);
          break;
        }
        case "budget_exhausted": {
          tlabel(`budget exhausted (${p.elapsed ?? "?"}s)`);
          pushChat(s, { role: "system", kind: "status",
            content: "Wall-clock budget exhausted.", ts: ev.ts,
            i18nKey: "sys.budgetExhausted" });
          break;
        }
        case "reason_start": {
          tlabel(`reasoning${p.trigger ? ` (${p.trigger})` : ""}`);
          break;
        }
        case "reason_failed": {
          const detail = String(p.error ?? "Planner request failed");
          tlabel(`planner failed: ${detail.slice(0, 120)}`);
          upsertStatusChat(s, "planner-failure", {
            role: "system", kind: "status",
            content: `Planner failed — ${detail}. Retrying…`, ts: ev.ts,
            i18nKey: "sys.plannerFailed", i18nVars: { detail },
          });
          break;
        }
        case "reason_retry_scheduled": {
          const detail = String(p.detail ?? p.planner_failure ?? "Planner request failed");
          const retryIndex = Number(p.retry_index) || 0;
          const delay = Number(p.delay_s) || 0;
          tlabel(`planner retry ${retryIndex}${delay ? ` in ${delay}s` : ""}: ${detail.slice(0, 100)}`);
          upsertStatusChat(s, "planner-failure", {
            role: "system", kind: "status",
            content: `Planner failed — ${detail}. Retry ${retryIndex}${delay ? ` in ${delay}s` : ""}.`, ts: ev.ts,
            i18nKey: "sys.plannerRetry",
            i18nVars: { detail, attempt: String(retryIndex), delay: String(delay) },
          });
          break;
        }
        case "planner_unavailable": {
          const detail = String(p.detail ?? p.planner_failure ?? "Planner unavailable");
          const attempts = Number(p.attempts) || 0;
          tlabel(`planner unavailable after ${attempts || "?"} attempts: ${detail.slice(0, 120)}`);
          upsertStatusChat(s, "planner-failure", {
            role: "system", kind: "status",
            content: `Planner unavailable after ${attempts || "multiple"} attempts — ${detail}. The run has stopped.`, ts: ev.ts,
            i18nKey: "sys.plannerUnavailable",
            i18nVars: { detail, attempts: attempts ? String(attempts) : "multiple" },
          });
          break;
        }
        case "reason_done": {
          const dup = Number(p.dropped_dup ?? 0);
          const attempts = (Array.isArray(p.reason_attempts) ? p.reason_attempts : []).map((item: any) => ({
            attemptIndex: Number(item.attempt_index) || 0,
            responseStatus: String(item.response_status || "not_recorded"),
            timedOut: !!item.timed_out,
            finishReason: String(item.finish_reason || ""),
            rawResponseArtifactId: item.raw_response_artifact_id ? String(item.raw_response_artifact_id) : undefined,
            rawResponseSha256: item.raw_response_sha256 ? String(item.raw_response_sha256) : undefined,
            rawResponseChars: Number(item.raw_response_chars) || 0,
            parseStatus: String(item.parse_status || "not_recorded"),
            parseDetail: item.parse_detail ? String(item.parse_detail) : undefined,
            rawIntentCount: Number(item.raw_intent_count) || 0,
            parsedIntentCount: Number(item.parsed_intent_count) || 0,
          }));
          const intentDecisions = (Array.isArray(p.intent_decisions) ? p.intent_decisions : []).map((item: any) => ({
            rawIndex: Number(item.raw_index),
            modelIntentId: String(item.model_intent_id || ""),
            resolvedIntentId: item.resolved_intent_id ? String(item.resolved_intent_id) : undefined,
            goal: item.goal ? String(item.goal) : undefined,
            outcome: String(item.outcome || ""),
            stage: String(item.stage || "dispatch"),
            reasonCode: String(item.reason_code || "unknown"),
          }));
          bb.reasonRuns = [...(bb.reasonRuns ?? []), {
            ts: ev.ts,
            proposed: Number(p.proposed) || 0,
            droppedTotal: Number(p.dropped_total) || 0,
            plannerFailure: p.status === "failed" && p.planner_failure
              ? String(p.planner_failure)
              : undefined,
            attempts,
            intentDecisions,
          }].slice(-100);
          if (!p.planner_failure && Number(p.proposed) > 0) {
            s.chat = s.chat.filter((message) => message.statusKey !== "planner-failure");
          }
          tlabel(
            `reason → ${p.proposed ?? 0} intent(s)` +
              (dup > 0 ? ` (${dup} dup dropped)` : "")
          );
          break;
        }
        case "race_started": {
          // the front race-scout round started: N engines probe the whole challenge
          // in parallel (single-shot) before the main coordinator loop. Show a status pill.
          s.racing = true;
          s.raceFinished = 0;
          const engineList = Array.isArray(p.engines) ? p.engines : [];
          if (engineList.length > 0) s.raceTotal = engineList.length;
          const engines = engineList.join(", ");
          tlabel(`race scout started${engines ? `: ${engines}` : ""}`);
          pushChat(s, { role: "system", kind: "status",
            content: `Race scout — ${engines || "engines"} probing in parallel`,
            ts: ev.ts, i18nKey: "sys.raceStarted", i18nVars: { engines } });
          break;
        }
        case "race_concluded": {
          // the race round ended: either it captured the flag (fast path) or its
          // facts go to the main coordinator loop. Clear the pill; the run-level events narrate
          // the outcome (solved / continuing).
          s.racing = false;
          const n = Number(p.flags ?? 0);
          tlabel(`race scout concluded (${p.solved ? "flag" : `${n} flag(s)`})`);
          if (!p.solved) {
            pushChat(s, { role: "system", kind: "status",
              content: "Race scout found no flag — handing facts to the planner",
              ts: ev.ts, i18nKey: "sys.raceHandoff" });
          }
          break;
        }
    default:
      return false;
  }
  return true;
}
