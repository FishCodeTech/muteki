/**
 * The real reports domain: binds the 报告 workbench (frozen reports, server
 * drafts, submission ledger, platform feedback, retests) to the SRC backend.
 * Ledger corrections are new audit entries — never history rewrites.
 */
import type {
  StudioPlatformFeedback, StudioReport, StudioRetestRecord, StudioRef,
  ReportTemplateId, SubmissionStatus, RewardStatus, ReadinessLevel, RetestVerdict,
} from "../model";
import type { HistoryEntry } from "../records";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import type { DomainCtx, RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";

const SUBMIT_MAP: Record<string, string> = {
  not_submitted: "not_submitted", submitted: "submitted", needs_info: "pending_info",
  accepted: "accepted", duplicate: "duplicate", rejected: "rejected",
};
const SUBMIT_BACK: Record<string, SubmissionStatus> = {
  not_submitted: "not_submitted", submitted: "submitted", pending_info: "needs_info",
  accepted: "accepted", duplicate: "duplicate", rejected: "rejected",
  reward_confirmed: "accepted", reward_paid: "accepted",
};
const REWARD_BY_STATE: Record<string, RewardStatus> = {
  reward_confirmed: "confirmed", reward_paid: "paid",
};

async function recordList(ctx: DomainCtx, kind: string, limit = 500) {
  const page = await querySrc<{ records?: Array<{ payload: Api; revision: number }> }>(ctx.taskId, "src.record.list", { kind, limit });
  return (page.records ?? []).map((r) => ({ ...r.payload, _revision: r.revision })) as Array<Api & { _revision: number }>;
}

async function recordHistory(ctx: DomainCtx, undo: { op: string; params?: Record<string, unknown> }, fallback: { action: string; summary: string; refs?: StudioRef[] }) {
  const entry = ctx.state().history[0];
  const payload = entry && entry.summary === fallback.summary
    ? { id: entry.id, action: entry.action, summary: entry.summary, refs: entry.refs, at: entry.at }
    : { id: `wb-${Date.now().toString(36)}`, action: fallback.action, summary: fallback.summary, refs: fallback.refs ?? [], at: new Date().toISOString() };
  await postCommand(ctx.taskId, "src.workbench.history.record", { entry: { ...payload, undo, domain: "reports" } });
}

function docSections(doc: Api): StudioReport["sections"] {
  const steps = (doc.steps as string[]) ?? (doc.reproduction_steps as string[]) ?? [];
  return {
    scopeAndTarget: text(doc.scope_and_target) || text(doc.target) || "",
    preconditions: text(doc.preconditions) || "",
    mechanism: text(doc.mechanism) || "",
    normalVsActual: text(doc.normal_vs_actual) || "",
    steps,
    pocScript: text(doc.poc_script) || "",
    pocVerified: doc.poc_verified === true,
    confirmedImpact: text(doc.confirmed_impact) || text(doc.impact) || "",
    conditionalImpact: text(doc.conditional_impact) || "",
    cvssVector: text(doc.cvss_vector) || "",
    cvssScore: num(doc.cvss_score),
    remediation: text(doc.remediation) || "",
    limitations: text(doc.limitations) || "",
  };
}

function frozenToReport(row: Api, ledger: Api | undefined, audit: Api[], projectId: string): StudioReport {
  const doc = (row.document ?? {}) as Api;
  const status = ledger ? (SUBMIT_BACK[text(ledger.status)] ?? "submitted") : "not_submitted";
  const amount = num(ledger?.amount) || undefined;
  return {
    id: text(row.report_id) || text(row.record_id),
    projectId, findingId: text(row.finding_id),
    title: text(doc.title) || text(row.report_id),
    version: num(row.version) || 1,
    status: row.supersedes || row.lifecycle === "superseded" ? (num(row.version) > 1 ? "frozen" : "frozen") : "frozen",
    supersedesReportId: text(row.supersedes) || undefined,
    templateId: (text(typeof row.template === "string" ? row.template : (row.template as Api)?.id) || "general_src") as ReportTemplateId,
    createdAt: iso(row.frozen_at) || iso(row.created_at), updatedAt: iso(row.frozen_at) || iso(row.created_at),
    frozenAt: iso(row.frozen_at) || undefined, frozenBy: text(row.actor_id) || undefined,
    targetVersion: text(row.target_version) || text(doc.target_version) || "",
    scopeRef: text((row.authorization_snapshot as Api)?.record_id) || "",
    planRef: text((row.plan_snapshot as Api)?.record_id) || "",
    readiness: (text((row.review as Api)?.readiness) || "RED") as ReadinessLevel,
    sections: docSections(doc),
    evidenceIds: ((row.evidence_refs as Api[]) ?? []).map((r) => text(r.artifact_id) || text(r.sha256)).filter(Boolean),
    materialGaps: ((row.review as Api)?.missing as string[]) ?? [],
    strangerChecklist: [],
    submission: {
      status,
      externalTicketId: text(ledger?.platform_reference),
      platformName: text(ledger?.platform) || "",
      submittedAt: status === "not_submitted" ? undefined : iso(ledger?.at) || iso(ledger?.created_at) || undefined,
      submittedBy: status === "not_submitted" ? undefined : text(ledger?.actor_id) || undefined,
      rewardStatus: (REWARD_BY_STATE[text(ledger?.status ?? "")] ?? (amount != null ? "confirmed" : "none")) as RewardStatus,
      rewardAmount: amount, rewardCurrency: "CNY",
      paidAt: text(ledger?.status) === "reward_paid" ? iso(ledger?.at) || iso(ledger?.created_at) || undefined : undefined,
      notes: text(ledger?.notes),
      auditTrail: audit.map((a) => ({ at: iso(a.at) || iso(a.created_at), by: text(a.actor_id), fromStatus: text(a.from_status), toStatus: text(a.status), note: text(a.notes) })),
    },
  } as StudioReport;
}

function draftToReport(row: Api, projectId: string): StudioReport {
  return {
    id: text(row.draft_id), projectId, findingId: text(row.finding_id),
    title: text(row.title) || "未命名草稿",
    version: num(row.version) || 1, status: "draft",
    templateId: (text(row.template_id) || "general_src") as ReportTemplateId,
    createdAt: iso(row.created_at) || iso(row.updated_at), updatedAt: iso(row.updated_at),
    targetVersion: text(row.target_version) || "",
    scopeRef: "", planRef: "",
    readiness: "RED" as ReadinessLevel,
    sections: docSections((row.sections ?? {}) as Api),
    evidenceIds: (row.evidence_ids as string[]) ?? [],
    materialGaps: (row.material_gaps as string[]) ?? [],
    strangerChecklist: [],
    submission: { status: "not_submitted", externalTicketId: "", platformName: "", rewardStatus: "none", rewardCurrency: "CNY", notes: "", auditTrail: [] },
    _revision: num(row.revision),
  } as unknown as StudioReport;
}

function feedbackToItem(row: Api, projectId: string): StudioPlatformFeedback {
  return {
    id: text(row.feedback_id) || text(row.record_id), projectId,
    reportId: text(row.report_id), findingId: text(row.finding_id),
    externalTicketId: text(row.platform_reference), platformAuthor: text(row.source) || text(row.actor_id),
    receivedAt: iso(row.received_at) || iso(row.created_at),
    kind: "fix_claim",
    rawText: text(row.text),
    attachments: ((row.attachments as Api[]) ?? []).map((a) => ({ name: text(a.name) || text(a.artifact_id), mediaType: text(a.media_type), size: num(a.size), summary: "", evidenceId: undefined })),
    status: "pending", followUpTaskId: text(row.follow_up_task_id) || undefined, updatedReportId: undefined,
  };
}

function retestToItem(row: Api, projectId: string): StudioRetestRecord {
  const verdict = (row.verdict_input ?? {}) as Api;
  const result: RetestVerdict = row.result === "fixed" || verdict.original_path_impact === false ? "fixed" : row.result === "inconclusive" || verdict.outcome_unknown ? "inconclusive" : verdict.original_path_impact === true ? "still_vulnerable" : "inconclusive";
  return {
    id: text(row.retest_id) || text(row.record_id), projectId,
    findingId: text(row.finding_id), baseReportId: text(row.report_id),
    newReportId: undefined, feedbackId: undefined, runId: text(row.source_run_id) || undefined,
    targetFixVersion: text(row.expected_target_version), result,
    prerequisiteCheck: verdict.preconditions_valid === true ? "前置条件成立" : verdict.preconditions_valid === false ? "前置条件不成立" : "未验证",
    normalControlCheck: verdict.normal_control_works === true ? "正常流程不受影响" : verdict.normal_control_works === false ? "正常流程受影响" : "未验证",
    basisAndEvidence: text(row.repair_statement), evidenceIds: ((verdict.evidence_refs as Api[]) ?? []).map((r) => text(r.artifact_id)).filter(Boolean),
    reviewer: text(verdict.reviewer) || text(row.requester), reviewedAt: iso(row.completed_at) || iso(row.created_at),
  };
}

export function createReportsDomain(): RealDomain {
  const draftRevisions = new Map<string, number>();
  const ledgerRevisions = new Map<string, number>();
  return {
    id: "reports",
    pollMs: 45000,
    owns: ["reports", "feedbacks", "retests"],
    combines: {
      history: (local, server) => {
        const serverIds = new Set((server as HistoryEntry[]).map((e) => e.id));
        return [...(server as HistoryEntry[]), ...(local as HistoryEntry[]).filter((e) => e.serverDomain !== "reports" && !serverIds.has(e.id))];
      },
    },
    async load(taskId, ctx) {
      const [frozenRows, drafts, ledgerRows, feedbackRows, retestRows, histPage] = await Promise.all([
        recordList(ctx, "frozen_report"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.report.drafts", {}),
        recordList(ctx, "platform_status"),
        recordList(ctx, "platform_feedback"),
        recordList(ctx, "finding_retest"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.history", { limit: 1000 }),
      ]);
      const projectId = `task-${taskId}`;
      const ledgerByReport = new Map<string, Api>();
      const auditByReport = new Map<string, Api[]>();
      for (const row of ledgerRows.sort((a, b) => iso(a.at || a.created_at).localeCompare(iso(b.at || b.created_at)))) {
        const rid = text(row.report_id);
        auditByReport.set(rid, [...(auditByReport.get(rid) ?? []), row]);
        ledgerByReport.set(rid, row); // newest wins
        ledgerRevisions.set(rid, Math.max(ledgerRevisions.get(rid) ?? 0, num(row.revision)));
      }
      draftRevisions.clear();
      const reports: StudioReport[] = [
        ...frozenRows.map((r) => frozenToReport(r, ledgerByReport.get(text(r.report_id) || text(r.record_id)), auditByReport.get(text(r.report_id) || text(r.record_id)) ?? [], projectId)),
        ...(drafts.items ?? []).map((d) => { draftRevisions.set(text(d.draft_id), num(d.revision)); return draftToReport(d, projectId); }),
      ];
      const history = (histPage.items ?? []).filter((row) => text(row.domain) === "reports").map((row): HistoryEntry => ({
        id: text(row.entry_id), at: iso(row.at),
        action: (text(row.action) || "status") as HistoryEntry["action"],
        summary: text(row.summary), refs: (row.refs as StudioRef[]) ?? [],
        serverId: text(row.entry_id), serverDomain: "reports",
        serverUndo: (row.undo as HistoryEntry["serverUndo"]) ?? undefined,
        revertedBy: row.reverted_at ? "server" : undefined,
      }));
      return {
        reports,
        feedbacks: feedbackRows.map((f) => feedbackToItem(f, projectId)),
        retests: retestRows.map((r) => retestToItem(r, projectId)),
        history,
      };
    },
    mutators: {
      async updateReportDraft(ctx, ...args) {
        const [reportId, patch] = args as [string, Record<string, unknown>];
        const current = ctx.state().reports.find((r) => r.id === reportId);
        const payload: Api = {
          title: patch.title ?? current?.title,
          template_id: patch.templateId ?? current?.templateId,
          target_version: patch.targetVersion ?? current?.targetVersion,
          finding_id: current?.findingId,
          sections: patch.sections ? { ...current?.sections, ...(patch.sections as Api) } : current?.sections,
          evidence_ids: patch.evidenceIds ?? current?.evidenceIds,
          material_gaps: current?.materialGaps ?? [],
        };
        await postCommand(ctx.taskId, "src.workbench.report.draft.save", {
          draft_id: reportId, payload, base_revision: draftRevisions.get(reportId) ?? 0,
        });
      },
      async freezeOrForkReport(ctx, ...args) {
        const [reportId, mode] = args as [string, "freeze" | "new_version"];
        const report = ctx.state().reports.find((r) => r.id === reportId);
        if (!report) return;
        if (mode === "freeze") {
          const state = ctx.state();
          const claims = (state.clues.find((c) => c.id === report.findingId)?.claims ?? []).map((c) => ({ claim_id: c.id, text: c.text, status: c.kind === "observed" ? "observed" : c.kind === "inferred" ? "supported_inference" : "unverified" }));
          await postCommand(ctx.taskId, "src.report.freeze", {
            finding_id: report.findingId, source_run_id: `workbench-${ctx.actor}`,
            document: { title: report.title, ...report.sections, target_version: report.targetVersion },
            claims, review: { readiness: report.readiness, missing: report.materialGaps },
            evidence_refs: [], cvss: { vector: report.sections.cvssVector, score: report.sections.cvssScore },
            deduplication: {}, template: report.templateId,
            plan_snapshot: report.planRef ? { record_id: report.planRef } : {},
            authorization_snapshot: report.scopeRef ? { record_id: report.scopeRef } : {},
            source_watermarks: {}, idempotency_key: `wb-freeze-${reportId}-${Date.now().toString(36)}`,
            actor_id: ctx.actor,
          });
        } else {
          const saved = await postCommand<Api>(ctx.taskId, "src.workbench.report.draft.save", {
            payload: { title: `${report.title}（v${report.version + 1} 草稿）`, template_id: report.templateId,
                       target_version: report.targetVersion, finding_id: report.findingId,
                       sections: report.sections, evidence_ids: report.evidenceIds,
                       base_report_id: reportId, version: report.version + 1 },
          });
          const local = ctx.state().reports.find((r) => r.status === "draft" && r.id !== saved.draft_id && r.findingId === report.findingId);
          return { idMap: local ? { [local.id]: text(saved.draft_id) } : undefined };
        }
      },
      async updateSubmissionLedger(ctx, ...args) {
        const [reportId, patch] = args as [string, Record<string, unknown>];
        // Ledger corrections are NEW audit entries, never history rewrites.
        const status = SUBMIT_MAP[text(patch.status) || "submitted"] ?? "submitted";
        await postCommand(ctx.taskId, "src.platform.status.record", {
          report_id: reportId, status, actor_id: ctx.actor, actor_kind: "operator",
          base_revision: ledgerRevisions.get(reportId) ?? 0,
          platform_reference: text(patch.externalTicketId), amount: patch.rewardAmount ?? undefined,
          notes: text(patch.notes),
        });
        await recordHistory(ctx, { op: "noop" }, { action: "status", summary: `更新提交台账 ${reportId} → ${status}`, refs: [{ kind: "report", id: reportId }] });
      },
      async addPlatformFeedback(ctx, ...args) {
        const [input] = args as [{ reportId?: string; findingId?: string; externalTicketId?: string; platformAuthor?: string; rawText?: string; kind?: string; receivedAt?: string }];
        await postCommand(ctx.taskId, "src.feedback.record", {
          report_id: input.reportId ?? "", text: input.rawText ?? "", source: input.platformAuthor || "platform",
          platform_reference: input.externalTicketId ?? "", received_at: input.receivedAt ?? "",
        });
      },
      async createTaskFromFeedback(ctx, ...args) {
        const [feedbackId] = args as [string];
        const feedback = ctx.state().feedbacks.find((f) => f.id === feedbackId);
        await postCommand(ctx.taskId, "src.workbench.todo.save", {
          payload: { title: `跟进平台反馈 ${feedbackId}${feedback?.externalTicketId ? `（${feedback.externalTicketId}）` : ""}`, status: "open", origin_kind: "feedback", feedback_id: feedbackId, report_id: feedback?.reportId },
        });
      },
      async recordRetest(ctx, ...args) {
        const [input] = args as [{ findingId: string; baseReportId: string; targetFixVersion: string; result: string; prerequisiteCheck?: string; normalControlCheck?: string; basisAndEvidence?: string; evidenceIds?: string[] }];
        const requested = await postCommand<Api>(ctx.taskId, "src.retest.request", {
          report_id: input.baseReportId, finding_id: input.findingId,
          repair_statement: input.basisAndEvidence || "工作台复测", expected_target_version: input.targetFixVersion,
          authorization_ref: "workbench", requester: ctx.actor,
          idempotency_key: `wb-retest-${input.baseReportId}-${Date.now().toString(36)}`,
        });
        const retestId = text(requested.retest_id);
        if (retestId && input.result) {
          await postCommand(ctx.taskId, "src.retest.complete", {
            retest_id: retestId,
            preconditions_valid: input.result !== "inconclusive",
            target_version_confirmed: input.result === "fixed",
            original_path_impact: input.result === "still_vulnerable" ? true : input.result === "fixed" ? false : undefined,
            normal_control_works: input.normalControlCheck ? !input.normalControlCheck.includes("受影响") : undefined,
            evidence_refs: [], reviewer: ctx.actor,
            outcome_unknown: input.result === "inconclusive",
          });
        }
      },
      async revertHistory(ctx, ...args) {
        const [entryId] = args as [string];
        const entry = ctx.state().history.find((e) => e.id === entryId || e.serverId === entryId);
        if (!entry || (entry.serverDomain && entry.serverDomain !== "reports")) return;
        const undo = entry.serverUndo;
        if (!undo || undo.op === "noop") {
          // ledger entries are deliberately non-revertible (audit, not undo)
          if (undo) await postCommand(ctx.taskId, "src.workbench.history.revert", { entry_id: entry.serverId ?? entryId });
          return;
        }
        const params = (undo.params ?? {}) as Api;
        if (undo.op === "draft_restore") {
          await postCommand(ctx.taskId, "src.workbench.report.draft.save", { draft_id: params.draft_id, payload: params.previous ?? {}, base_revision: params.base_revision ?? 0 });
        }
        await postCommand(ctx.taskId, "src.workbench.history.revert", { entry_id: entry.serverId ?? entryId });
      },
    },
  };
}
