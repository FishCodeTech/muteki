import {
  EventType,
  type DeckState,
  type IntentDispatchState,
  type MutekiEvent,
} from "../types";
import { archiveHitlRequest } from "../hitl";
import {
  invalidateFlag,
  isInvalidatedFlag,
  lane,
  mergeFlags,
  pushChat,
} from "../helpers";
import { toEpochMs } from "@/lib/format";

const PROGRESS_SUMMARY_LABELS: Record<string, string> = {
  "Waiting for model or tool response": "等待模型或工具响应",
  "Writeup generation started": "已开始整理证据与解题过程",
  "Inspecting Run evidence": "正在检索运行证据",
  "Reading evidence": "正在读取证据",
  "Evidence read completed": "已读取证据",
  "Requesting model analysis": "正在生成内容",
  "Answer complete": "已完成正文整理",
};

const PROGRESS_PHASE_LABELS: Record<string, string> = {
  starting: "正在准备生成上下文",
  investigating: "正在整理证据与分析",
  writing: "正在撰写正文",
};

export function formatFollowupProgressDetail(progress?: {
  phase?: string;
  summary?: string;
  tool_name?: string;
}): string {
  if (!progress) return "";
  const rawSummary = typeof progress.summary === "string" ? progress.summary.trim() : "";
  const rawPhase = typeof progress.phase === "string" ? progress.phase.trim() : "";
  const summary = PROGRESS_SUMMARY_LABELS[rawSummary]
    || rawSummary
    || PROGRESS_PHASE_LABELS[rawPhase]
    || rawPhase;
  const toolName = typeof progress.tool_name === "string" ? progress.tool_name.trim() : "";
  return [summary, toolName].filter(Boolean).join(" · ");
}

export function reduceCompletion(ev: MutekiEvent, s: DeckState): DeckState | undefined {
  const sid = ev.solver_id || "";
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.WORKER_FINISHED: {
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
      s.racing = false;
      s.verifying = 0;
      if (s.blackboard.challenger.status === "running") {
        s.blackboard.challenger = { ...s.blackboard.challenger, status: "stopped", finishedTs: ev.ts };
      }
      s.awaitingOperator = undefined;
      for (const req of s.hitlRequests) {
        s.hitlHistory = archiveHitlRequest(s.hitlHistory, req, {
          resolvedTs: ev.ts,
          resolutionAction: req.deliveryAction || "run_finished",
          deliveryCommandId: req.deliveryCommandId,
          deliveryStatus: req.deliveryStatus,
          deliveryDetail: req.deliveryDetail,
        });
      }
      s.hitlRequests = [];
      if (s.finishedAt == null) s.finishedAt = ev.ts;
      if (s.activeSince != null) {
        s.activeDurationMs += Math.max(0, toEpochMs(ev.ts) - toEpochMs(s.activeSince));
        s.activeSince = undefined;
      }
      const finishFlagsRaw = (p.flags as string[] | undefined)
        ?? (typeof p.flag === "string" ? [p.flag] : []);
      const finishFlags = finishFlagsRaw.filter((f) => !isInvalidatedFlag(s, f));
      mergeFlags(s, finishFlags);
      if (typeof p.expected_flags === "number") s.expectedFlags = p.expected_flags;
      if (typeof p.multi_flag === "boolean") s.multiFlag = p.multi_flag;
      const finishReason = String(p.reason ?? "");
      const hardFailure = finishReason === "runtime_failure" || finishReason === "preflight_failed";
      const collectUnknown = !!s.multiFlag && (s.expectedFlags || 1) <= 1;
      const solvedByPayload =
        (!hardFailure && !!p.solved && (finishFlagsRaw.length === 0 || finishFlags.length > 0))
        || (!hardFailure && s.mode !== "pentest" && !s.platformConfirmationRequired && collectUnknown && s.flags.length > 0);
      s.solved = solvedByPayload || s.solved;
      const finishDetail = String(p.detail ?? p.error ?? "").trim();
      const unsuccessfulFinish = (
        hardFailure
        || finishReason === "operator_stop"
        || finishReason === "budget_exhausted"
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
      s.outcomeReason = hardFailure
        ? finishReason
        : (s.solved && s.mode !== "pentest" && s.flags.length > 0) || finishReason === "solved"
          ? "solved"
          : (finishReason === "goal_met" || (solvedByPayload && finishFlags.length === 0) || s.outcomeReason === "goal_met")
            ? "goal_met"
            : unsuccessfulFinish
              ? finishReason
              : "finished";
      if (s.driverKind === "local_pi" || s.localPiSession) {
        const prevPi = s.localPiSession || {};
        s.localPiSession = {
          ...prevPi,
          sessionState: hardFailure ? "failed" : "stopped",
          turnActive: false,
        };
      }
      s.outcomeDetail = finishDetail || s.outcomeDetail;
      s.outcomeErrorId = String(p.error_id ?? "") || undefined;
      s.outcomeFailureCode = String(p.failure_code ?? "") || undefined;
      s.outcomeFailurePhase = String(p.failure_phase ?? "") || undefined;
      if (ev.solver_id) {
        const l = lane(s, ev.solver_id);
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
        const winnerFlag = solvedByPayload
          ? finishFlags.find((flag) => s.blackboard.flagOrigins[flag]?.actor === id)
          : undefined;
        const isWinner = !!winnerFlag || (!!ev.solver_id && id === ev.solver_id);
        const laneSolved = (isWinner && solvedByPayload) || l.solved;
        s.lanes[id] = {
          ...l,
          solved: laneSolved,
          flag: winnerFlag || l.flag,
          status: laneSolved ? "SOLVED" : "done",
          online: false,
          statusReason: isWinner
            ? (solvedByPayload ? "solved" : (finishReason || "finished"))
            : (l.statusReason && l.online === false ? l.statusReason : (finishReason || "finished")),
          finishedAt: l.finishedAt ?? ev.ts,
          runtime: l.runtime ? { ...l.runtime, status: "exited", finished_at: l.runtime.finished_at ?? ev.ts } : undefined,
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
      if (p.kind === "btw") break;
      const followupId = String(p.followup_id || ev.seq || "legacy");
      if (s.chat.some(m => m.followupId === followupId)) break;
      s.followupPending = true;
      s.activeFollowupJobId = followupId;
      s.activeFollowupKind = typeof p.kind === "string" ? p.kind : undefined;
      s.followupProgress = undefined;
      if (p.question) pushChat(s, { role: "human", kind: "text", content: p.question, ts: ev.ts, followupId });
      pushChat(s, {
        role: "system",
        kind: "status",
        content: p.kind === "writeup" ? "正在生成 Writeup…" : "正在回答追问…",
        followupDetail: p.kind === "writeup" ? "已加入生成队列，等待模型响应" : undefined,
        ts: ev.ts,
        followupId,
        followupKind: p.kind,
        actor: p.actor,
      });
      break;
    }
    case EventType.FOLLOWUP_PROGRESS: {
      if (p.kind === "btw") break;
      const followupId = String(p.followup_id || s.activeFollowupJobId || ev.seq || "legacy");
      if (s.activeFollowupJobId === followupId) {
        s.followupProgress = { phase: p.phase, summary: p.summary, tool_name: p.tool_name, turn: p.turn, elapsed_ms: p.elapsed_ms };
        if (!s.activeFollowupKind && typeof p.kind === "string") s.activeFollowupKind = p.kind;
      }
      const detail = formatFollowupProgressDetail({ phase: p.phase, summary: p.summary, tool_name: p.tool_name });
      if (s.chat.some((m) => m.role === "system" && m.followupId === followupId)) {
        s.chat = s.chat.map((m) => (
          m.role === "system" && m.followupId === followupId
            ? { ...m, followupDetail: detail || m.followupDetail, ts: ev.ts }
            : m
        ));
      } else {
        pushChat(s, {
          role: "system",
          kind: "status",
          content: p.kind === "writeup" ? "正在生成 Writeup…" : "正在回答追问…",
          followupDetail: detail || undefined,
          ts: ev.ts,
          followupId,
          followupKind: p.kind,
        });
      }
      break;
    }
    case EventType.FOLLOWUP_COMPLETED:
    case EventType.FOLLOWUP_FAILED:
    case EventType.FOLLOWUP_CANCELLED: {
      if (p.kind === "btw") break;
      const followupId = String(p.followup_id || s.activeFollowupJobId || ev.seq || "legacy");
      if (s.activeFollowupJobId === followupId) {
        s.followupPending = false;
        s.activeFollowupJobId = undefined;
        s.activeFollowupKind = undefined;
        s.followupProgress = undefined;
      }
      const isWriteup = p.kind === "writeup"
        || s.chat.some((m) => m.followupId === followupId && m.followupKind === "writeup");
      const resolvedKind = isWriteup ? "writeup" : (typeof p.kind === "string" ? p.kind : undefined);
      const reportId = typeof p.report_id === "string" && p.report_id ? p.report_id : undefined;
      const completed = ev.event_type === EventType.FOLLOWUP_COMPLETED;
      const content = completed ? (isWriteup ? "Writeup 已生成" : "追问已回答")
        : ev.event_type === EventType.FOLLOWUP_CANCELLED ? "已取消"
        : `后续操作失败：${p.error?.message || p.detail || p.error || p.code || "原因未查明"}`;
      s.chat = s.chat
        .filter((m) => !(isWriteup && m.role === "agent" && m.followupId === followupId))
        .map((m) => m.followupId !== followupId ? m : m.role === "system"
          ? {
              ...m,
              content,
              followupDetail: undefined,
              sealed: true,
              followupKind: resolvedKind || m.followupKind,
              followupSucceeded: completed,
              reportId: reportId ?? m.reportId,
              actor: p.actor || m.actor,
              ts: ev.ts,
            }
          : m.role === "agent" ? { ...m, sealed: true, actor: p.actor || m.actor } : m);
      if (!s.chat.some(m => m.role === "system" && m.followupId === followupId)) {
        pushChat(s, {
          role: "system",
          kind: "status",
          content,
          followupId,
          followupKind: resolvedKind,
          followupSucceeded: completed,
          reportId,
          actor: p.actor,
          sealed: true,
          ts: ev.ts,
        });
      }
      if (completed) {
        if (isWriteup || p.has_writeup) s.hasWriteup = true;
        if (!isWriteup && p.text && !s.chat.some(m => m.role === "agent" && m.followupId === followupId)) {
          pushChat(s, { role: "agent", kind: "text", solverId: "followup", mainThread: true, followupId,
            content: p.text, actor: p.actor, ts: ev.ts, sealed: true });
        }
      }
      break;
    }
    case EventType.PENTEST_REPORT_STARTED: {
      const reportJobId = String(p.report_job_id || ev.seq || "pentest-report");
      s.reportGenerating = true;
      s.activeReportJobId = reportJobId;
      if (!s.chat.some((m) => m.role === "system" && m.followupId === reportJobId)) {
        pushChat(s, {
          role: "system",
          kind: "status",
          content: "正在生成测试报告…",
          followupDetail: "正在汇总已验证证据并撰写正式报告",
          ts: ev.ts,
          followupId: reportJobId,
          followupKind: "pentest_report",
        });
      }
      break;
    }
    case EventType.PENTEST_REPORT_COMPLETED:
    case EventType.PENTEST_REPORT_FAILED: {
      const reportJobId = String(p.report_job_id || s.activeReportJobId || ev.seq || "pentest-report");
      if (!s.activeReportJobId || s.activeReportJobId === reportJobId) {
        s.reportGenerating = false;
        s.activeReportJobId = undefined;
      }
      const completed = ev.event_type === EventType.PENTEST_REPORT_COMPLETED;
      const content = completed
        ? "测试报告已生成"
        : `测试报告生成失败：${p.detail || p.error_type || p.reason || p.code || "原因未查明"}`;
      if (s.chat.some((m) => m.role === "system" && m.followupId === reportJobId)) {
        s.chat = s.chat.map((m) => (
          m.role === "system" && m.followupId === reportJobId
            ? {
                ...m,
                content,
                followupDetail: undefined,
                sealed: true,
                followupKind: "pentest_report",
                followupSucceeded: completed,
                ts: ev.ts,
              }
            : m
        ));
      } else {
        pushChat(s, {
          role: "system",
          kind: "status",
          content,
          followupId: reportJobId,
          followupKind: "pentest_report",
          followupSucceeded: completed,
          sealed: true,
          ts: ev.ts,
        });
      }
      break;
    }
    case EventType.RUN_REOPENED: {
      s.followupPending = false;
      s.activeFollowupJobId = undefined;
      s.activeFollowupKind = undefined;
      s.followupProgress = undefined;
      s.reportGenerating = false;
      s.activeReportJobId = undefined;
      s.finished = false;
      s.preparing = false;
      s.solved = false;
      s.finishedAt = undefined;
      if (s.activeSince == null) s.activeSince = ev.ts;
      s.racing = false;
      s.verifying = 0;
      s.awaitingOperator = undefined;
      for (const req of s.hitlRequests) {
        s.hitlHistory = archiveHitlRequest(s.hitlHistory, req, {
          resolvedTs: ev.ts,
          resolutionAction: req.deliveryAction || "run_reopened",
          deliveryCommandId: req.deliveryCommandId,
          deliveryStatus: req.deliveryStatus,
          deliveryDetail: req.deliveryDetail,
        });
      }
      s.hitlRequests = [];
      const reopenedForResolve = p.reason === "resolve";
      if (reopenedForResolve) {
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
