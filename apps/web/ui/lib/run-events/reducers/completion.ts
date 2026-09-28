/** Domain reducer extracted from ../reduce.ts; behavior intentionally unchanged. */
import {
  EventType,
  type DeckState,
  type IntentDispatchState,
  type MutekiEvent,
} from "../types";
import {
  invalidateFlag,
  isInvalidatedFlag,
  lane,
  mergeFlags,
  pushChat,
} from "../helpers";

export function reduceCompletion(ev: MutekiEvent, s: DeckState): DeckState | undefined {
  const sid = ev.solver_id || "";
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.WORKER_FINISHED: {
      // ONE swarm sub-worker ended — worker-level, NOT the run. The coordinator
      // keeps re-bootstrapping until solved/stopped, so we must NOT set s.finished
      // here (that was the run-7345 "怎么又结束了" bug: a worker ending made the
      // whole deck read 'finished' while the run was still going). Only update THIS
      // worker's lane; presence already flipped via WORKER_STATUS. If this worker
      // actually found the flag, reflect that on its lane + graft the flag node, and
      // record the flag/solved on the deck — but leave s.finished to RUN_FINISHED.
      const wfRaw = (p.flags as string[] | undefined)
        ?? (typeof p.flag === "string" ? [p.flag] : []);
      const wf = wfRaw.filter((f) => !isInvalidatedFlag(s, f));
      if (wf.length) {
        mergeFlags(s, wf);
        s.solved = !!p.solved || s.solved;
      }
      if (ev.solver_id) {
        const l = lane(s, ev.solver_id);
        s.lanes[l.solverId] = {
          ...l,
          solved: (wf.length > 0 && !!p.solved) || l.solved,
          flag: (typeof p.flag === "string" && !isInvalidatedFlag(s, p.flag))
            ? p.flag : l.flag,
          status: (wf.length > 0 && p.solved) ? "SOLVED" : l.solved ? "SOLVED" : "done",
          online: false,
          statusReason: (wf.length > 0 && p.solved)
            ? "solved"
            : String(p.reason || l.statusReason || "finished"),
          finishedAt: l.finishedAt ?? ev.ts,
        };
        if (l.phase === "verifier" || l.role === "verifier") {
          s.verifying = Math.max(0, (s.verifying ?? 0) - 1);
        }
        if (s.racing && (l.raceScout || l.phase === "race")) {
          s.lanes[l.solverId] = { ...s.lanes[l.solverId], raceScout: true };
        }
      }
      if (p.solved && wf.length > 0) {
        pushChat(s, { role: "system", kind: "status", content: `SOLVED — ${p.flag ?? ""}`, ts: ev.ts, i18nKey: "sys.solved", i18nVars: { flag: p.flag ?? "" } });
      }
      break;
    }
    case EventType.RUN_FINISHED: {
      if (!s.started) {
        s.started = true;
        if (s.startedAt == null) s.startedAt = ev.ts;
      }
      s.finished = true;
      s.preparing = false;
      // A hard stop can kill the coordinator before it ever emits race_concluded
      // (operator stop cancels the whole run task mid-race). Clear the pill here so
      // the UI never sticks on "racing" past the terminal event.
      s.racing = false;
      s.verifying = 0;
      s.awaitingOperator = undefined;
      s.hitlRequests = [];
      // Keep the FIRST finish ts of the current run cycle — the backend re-emits
      // run.finished (with a fresh ts) when a finished run is reloaded/reconnected,
      // which would otherwise stretch the duration to "load time". A genuine
      // re-open fires RUN_STARTED first, which clears finishedAt, so the next
      // finish is captured correctly.
      if (s.finishedAt == null) s.finishedAt = ev.ts;
      const finishFlagsRaw = (p.flags as string[] | undefined)
        ?? (typeof p.flag === "string" ? [p.flag] : []);
      const finishFlags = finishFlagsRaw.filter((f) => !isInvalidatedFlag(s, f));
      mergeFlags(s, finishFlags);
      if (typeof p.expected_flags === "number") s.expectedFlags = p.expected_flags;
      if (typeof p.multi_flag === "boolean") s.multiFlag = p.multi_flag;
      const solvedByPayload = !!p.solved && (finishFlagsRaw.length === 0 || finishFlags.length > 0);
      s.solved = solvedByPayload || s.solved;
      // classify the outcome so the UI shows the right thing: a gated flag →
      // "solved"; solved without a flag → pentest "goal_met"; else "finished".
      // The backend may also send payload.reason; goal_complete may have set it
      // already, in which case we don't downgrade it.
      const finishReason = String(p.reason ?? "");
      const finishDetail = String(p.detail ?? p.error ?? "").trim();
      const unsuccessfulFinish = (
        finishReason === "operator_stop"
        || finishReason === "budget_exhausted"
        || finishReason === "runtime_failure"
        || finishReason === "preflight_failed"
        || finishReason === "no_progress"
      );
      s.preflightFailures = Array.isArray(p.profile_failures)
        ? p.profile_failures.map((failure: Record<string, any>) => ({
          profileId: String(failure.profile_id ?? ""),
          errorId: String(failure.error_id ?? "") || undefined,
          engine: String(failure.engine ?? ""),
          model: String(failure.model ?? "") || undefined,
          backend: String(failure.backend ?? "") || undefined,
          runtime: String(failure.runtime ?? "") || undefined,
          stage: String(failure.stage ?? "") || undefined,
          layer: String(failure.layer ?? "") || undefined,
          code: String(failure.code ?? "") || undefined,
          detail: String(failure.detail ?? "预检失败"),
        }))
        : [];
      s.outcomeReason = unsuccessfulFinish
        ? finishReason
        : finishReason === "solved"
          ? "solved"
          : (finishReason === "goal_met" || (solvedByPayload && finishFlags.length === 0) || s.outcomeReason === "goal_met")
            ? "goal_met"
            : (solvedByPayload || finishReason === "solved")
              ? "solved"
              : "finished";
      s.outcomeDetail = finishDetail || s.outcomeDetail;
      s.outcomeErrorId = String(p.error_id ?? "") || undefined;
      s.outcomeFailureCode = String(p.failure_code ?? "") || undefined;
      s.outcomeFailurePhase = String(p.failure_phase ?? "") || undefined;
      if (ev.solver_id) {
        const l = lane(s, ev.solver_id);
        // Don't let a run-level RUN_FINISHED (which the coordinator may emit with
        // p.solved=false even after a lane already solved) wipe a lane's solved
        // boolean — preserve it with `|| l.solved` (mock/race/standby finishes).
        const stillSolved = solvedByPayload || l.solved;
        s.lanes[l.solverId] = {
          ...l,
          solved: stillSolved,
          flag: (typeof p.flag === "string" && !isInvalidatedFlag(s, p.flag))
            ? p.flag : l.flag,
          status: stillSolved ? "SOLVED" : "done",
          online: false,
          statusReason: stillSolved ? "solved" : (finishReason || "finished"),
        };
      }
      for (const id of Object.keys(s.lanes)) {
        const l = s.lanes[id];
        const isWinner = !!ev.solver_id && id === ev.solver_id;
        s.lanes[id] = {
          ...l,
          status: (isWinner && solvedByPayload) || l.solved ? "SOLVED" : "done",
          online: false,
          statusReason: isWinner
            ? (solvedByPayload ? "solved" : (finishReason || "finished"))
            : (l.statusReason && l.online === false ? l.statusReason : (finishReason || "finished")),
          // A lane swept offline here never saw its own exit event; the run end is its end.
          finishedAt: l.finishedAt ?? ev.ts,
        };
      }
      if (solvedByPayload && s.flags.length > 0) {
        pushChat(s, {
          role: "system", kind: "status", content: `SOLVED — ${p.flag ?? ""}`,
          ts: ev.ts, i18nKey: "sys.solved", i18nVars: { flag: p.flag ?? "" },
        });
      } else if (solvedByPayload) {
        pushChat(s, {
          role: "system", kind: "status",
          content: s.mode === "pentest" ? "Goal met — supported by evidence" : `Goal met — ${s.goalWhy ?? "engagement objective reached"}`,
          ts: ev.ts, i18nKey: s.mode === "pentest" ? "sys.pentestGoalMet" : "sys.goalMet", i18nVars: { why: s.goalWhy ?? "" },
        });
      } else if (s.outcomeReason === "runtime_failure") {
        pushChat(s, {
          role: "system", kind: "status",
          content: `Run failed — ${s.outcomeDetail || "runtime failure"}`,
          ts: ev.ts, i18nKey: "sys.runtimeFailure",
          i18nVars: { detail: s.outcomeDetail || "runtime failure" },
        });
      } else if (s.outcomeReason === "no_progress") {
        pushChat(s, {
          role: "system", kind: "status",
          content: `Run stopped without progress — ${s.outcomeDetail || "no progress"}`,
          ts: ev.ts, i18nKey: "sys.noProgress",
          i18nVars: { detail: s.outcomeDetail || "no progress" },
        });
      } else if (s.outcomeReason === "preflight_failed") {
        pushChat(s, {
          role: "system", kind: "status",
          content: `Worker preflight failed — ${s.outcomeDetail || "profile unavailable"}`,
          ts: ev.ts, i18nKey: "sys.preflightFailed",
          i18nVars: { detail: s.outcomeDetail || "profile unavailable" },
        });
      } else {
        pushChat(s, {
          role: "system", kind: "status", content: "Run finished (no flag)",
          ts: ev.ts, i18nKey: "sys.finishedNoFlag",
        });
      }
      // 刀2: defensively demote any still-active open intents at run finish so they
      // stop reading as "in flight". The backend now emits intent_state_changed for
      // finalize, but this guards missed deltas and older runs replayed from JSONL.
      // Solved → closed; otherwise held as resume (matches the DB finalize sweep).
      if (s.blackboard?.intents?.length) {
        const ds: IntentDispatchState = p.solved ? "closed" : "resume";
        s.blackboard.intents = s.blackboard.intents.map((i) =>
          i.status !== "done" && (i.dispatchState ?? "active") === "active"
            ? { ...i, dispatchState: ds }
            : i);
      }
      break;
    }
    case EventType.FOLLOWUP_STARTED: {
      const kind = String(p.kind || "ask");
      const followupId = String(p.followup_id || "");
      if (followupId && s.chat.some((message) =>
        message.role === "system" && message.followupId === followupId)) {
        break;
      }
      s.followupPending = true;
      const question = String(p.question || "").trim();
      if (kind === "ask" && question) {
        pushChat(s, {
          role: "human", kind: "text", content: question, ts: ev.ts,
        });
      }
      pushChat(s, {
        role: "system", kind: "status",
        content: kind === "writeup" ? "正在生成报告…" : "正在回答追问…",
        ts: ev.ts, followupId, followupKind: kind,
      });
      break;
    }
    case EventType.FOLLOWUP_COMPLETED: {
      s.followupPending = false;
      const kind = String(p.kind || "ask");
      const followupId = String(p.followup_id || "");
      const pendingStatus = kind === "writeup" ? "正在生成报告…" : "正在回答追问…";
      const completedStatus = kind === "writeup" ? "报告已生成" : "追问已回答";
      const pendingIndex = [...s.chat].reverse().findIndex(
        (message) => message.role === "system"
          && (followupId
            ? message.followupId === followupId
            : message.content === pendingStatus),
      );
      if (pendingIndex >= 0) {
        const index = s.chat.length - pendingIndex - 1;
        s.chat = s.chat.map((message, messageIndex) => messageIndex === index
          ? { ...message, content: completedStatus, ts: ev.ts }
          : message);
      }
      const text = String(p.text || "").trim();
      if (text) {
        pushChat(s, {
          role: "agent", solverId: sid || "standby", mainThread: true,
          kind: "text", content: text, ts: ev.ts, sealed: true,
        });
      }
      break;
    }
    case EventType.FOLLOWUP_FAILED: {
      s.followupPending = false;
      const followupId = String(p.followup_id || "");
      const kind = String(p.kind || "ask");
      const pendingStatus = kind === "writeup" ? "正在生成报告…" : "正在回答追问…";
      const failedStatus = `后续操作失败：${String(p.detail || "原因未查明")}`;
      const pendingIndex = [...s.chat].reverse().findIndex(
        (message) => message.role === "system"
          && (followupId
            ? message.followupId === followupId
            : message.content === pendingStatus),
      );
      if (pendingIndex >= 0) {
        const index = s.chat.length - pendingIndex - 1;
        s.chat = s.chat.map((message, messageIndex) => messageIndex === index
          ? { ...message, content: failedStatus, ts: ev.ts }
          : message);
      } else {
        pushChat(s, {
          role: "system", kind: "status", content: failedStatus, ts: ev.ts,
          followupId, followupKind: kind,
        });
      }
      break;
    }
    case EventType.RUN_REOPENED: {
      // The same lifecycle event reopens a run for either "continue solving" or a
      // false-positive flag invalidation. Keep the operator copy precise.
      s.finished = false;
      s.preparing = false;
      s.solved = false;
      s.finishedAt = undefined;
      // A reopen starts a new generation outside race-scout (resolve skips the
      // race); the previous generation's racing pill must not carry over.
      s.racing = false;
      s.verifying = 0;
      s.awaitingOperator = undefined;
      s.hitlRequests = [];
      const reopenedForResolve = p.reason === "resolve";
      if (reopenedForResolve) {
        // Continue solving from the same evidence graph: keep already recovered
        // flags visible and let new worker prompts inherit them.
      } else if (typeof p.flag === "string") {
        invalidateFlag(s, p.flag);
      } else {
        invalidateFlag(s);
      }
      s.flag = s.flags[0];
      s.outcomeReason = undefined;
      s.outcomeDetail = undefined;
      s.outcomeErrorId = undefined;
      s.outcomeFailureCode = undefined;
      s.outcomeFailurePhase = undefined;
      s.preflightFailures = [];
      s.goalWhy = undefined;
      pushChat(s, { role: "system", kind: "status",
        content: reopenedForResolve ? "↻ continuing solve" : "↻ flag marked false — re-solving",
        ts: ev.ts,
        i18nKey: reopenedForResolve ? "sys.resolveReopened" : "sys.reopened",
        i18nVars: { flag: p.flag ?? "" } });
      break;
    }
    default:
      return undefined;
  }
  return s;
}
