import type {
  StudioPlatformFeedback, StudioReport, StudioRetestRecord, StudioRef,
  ReportTemplateId, SubmissionStatus, RewardStatus, ReadinessLevel, RetestVerdict,
} from "../model";
import type { HistoryEntry } from "../records";
import { randomUuid } from "@/lib/browserCrypto";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import { recordListAll, type DomainCtx, type RealDomain } from "./domain";

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
  reward_pending: "accepted", reward_confirmed: "accepted", reward_paid: "accepted",
};
const REWARD_BY_STATE: Record<string, RewardStatus> = {
  reward_pending: "reward_pending",
  reward_confirmed: "confirmed",
  reward_paid: "paid",
};

const FEEDBACK_KIND_FROM_SERVER: Record<string, StudioPlatformFeedback["kind"]> = {
  supplement_request: "needs_more_evidence",
  needs_more_evidence: "needs_more_evidence",
  duplicate_dispute: "duplicate_notice",
  duplicate_notice: "duplicate_notice",
  retest_notice: "fix_claim",
  fix_claim: "fix_claim",
  triage_question: "scope_question",
  scope_question: "scope_question",
  general: "accepted_notice",
  accepted_notice: "accepted_notice",
};

const FEEDBACK_KIND_TO_SERVER: Record<string, string> = {
  supplement_request: "supplement_request",
  duplicate_dispute: "duplicate_dispute",
  retest_notice: "retest_notice",
  triage_question: "triage_question",
  general: "general",
  needs_more_evidence: "supplement_request",
  duplicate_notice: "duplicate_dispute",
  fix_claim: "retest_notice",
  scope_question: "triage_question",
  accepted_notice: "general",
};

export interface FeedbackMeta {
  backendKind: string;
  notes: string;
  links: string[];
}

const feedbackMetaById = new Map<string, FeedbackMeta>();

export function getFeedbackMeta(feedbackId: string): FeedbackMeta | null {
  return feedbackId ? (feedbackMetaById.get(feedbackId) ?? null) : null;
}

async function recordHistory(ctx: DomainCtx, undo: { op: string; params?: Record<string, unknown> }, fallback: { action: string; summary: string; refs?: StudioRef[] }) {
  const entry = (ctx.proposal?.() ?? ctx.state()).history[0];
  const payload = entry && entry.summary === fallback.summary
    ? { id: entry.id, action: entry.action, summary: entry.summary, refs: entry.refs, at: entry.at }
    : { id: `wb-${randomUuid().slice(0, 8)}`, action: fallback.action, summary: fallback.summary, refs: fallback.refs ?? [], at: new Date().toISOString() };
  await postCommand(ctx.taskId, "src.workbench.history.record", { entry: { ...payload, undo, domain: "reports" } });
}

function docSections(doc: Api): StudioReport["sections"] {
  const rawSteps = Array.isArray(doc.steps) ? doc.steps : Array.isArray(doc.reproduction_steps) ? doc.reproduction_steps : [];
  const steps = rawSteps.map((step) => typeof step === "string" ? step : text((step as Api)?.action)).filter(Boolean);
  const cvss = (doc.cvss ?? {}) as Api;
  return {
    scopeAndTarget: text(doc.scope_and_target) || text(doc.scope) || text(doc.target),
    preconditions: text(doc.preconditions) || "",
    mechanism: text(doc.mechanism) || text(doc.overview),
    normalVsActual: text(doc.normal_vs_actual) || [text(doc.normal_behavior), text(doc.actual_behavior)].filter(Boolean).join("\n"),
    steps,
    pocScript: text(doc.poc_script) || "",
    pocVerified: doc.poc_verified === true,
    confirmedImpact: text(doc.confirmed_impact) || text(doc.impact) || "",
    conditionalImpact: text(doc.conditional_impact) || "",
    counterEvidence: text(doc.counterevidence) || text(doc.counter_evidence) || text(doc.counterEvidence) || "",
    cvssVector: text(doc.cvss_vector) || text(cvss.vector),
    cvssScore: num(doc.cvss_score ?? cvss.score),
    remediation: text(doc.remediation) || "",
    limitations: text(doc.limitations) || "",
  };
}

const SECTION_DOC_KEYS: Record<string, string> = {
  scopeAndTarget: "scope_and_target",
  preconditions: "preconditions",
  mechanism: "mechanism",
  normalVsActual: "normal_vs_actual",
  steps: "steps",
  pocScript: "poc_script",
  pocVerified: "poc_verified",
  confirmedImpact: "confirmed_impact",
  conditionalImpact: "conditional_impact",
  counterEvidence: "counterevidence",
  cvssVector: "cvss_vector",
  cvssScore: "cvss_score",
  remediation: "remediation",
  limitations: "limitations",
};

function sectionsToDoc(sections: StudioReport["sections"], existingDoc: Api = {}): Api {
  const doc: Api = { ...existingDoc };
  for (const [field, docKey] of Object.entries(SECTION_DOC_KEYS)) {
    if (!(field in (sections as Api))) continue;
    const value = (sections as Api)[field];
    if (value !== undefined) doc[docKey] = value;
  }
  return doc;
}

function collectFrozenEvidenceIds(row: Api, doc: Api): string[] {
  const ids = new Set<string>();
  for (const ex of (doc.exchanges as Api[] | undefined) ?? []) {
    const eid = text(ex.exchange_id);
    if (eid) ids.add(eid);
  }
  for (const r of (row.evidence_refs as Api[] | undefined) ?? []) {
    const eid = text(r.exchange_id);
    const aid = text(r.artifact_id);
    if (eid) ids.add(eid);
    if (aid) ids.add(aid);
  }
  return [...ids];
}

function frozenToReport(row: Api, ledger: Api | undefined, audit: Api[], projectId: string): StudioReport {
  const doc = (row.document ?? {}) as Api;
  const status = ledger ? (SUBMIT_BACK[text(ledger.status)] ?? "submitted") : "not_submitted";
  const amount = typeof ledger?.amount === "number" ? ledger.amount : undefined;
  const payment = audit.findLast((entry) => entry.status === "reward_paid" && entry.from_status !== "reward_paid");
  return {
    id: text(row.report_id) || text(row.record_id),
    projectId, findingId: text(row.finding_id),
    title: text(doc.title) || text(row.report_id),
    target: text(doc.target),
    evidenceExchangeId: text(((doc.exchanges as Api[]) ?? [])[0]?.exchange_id),
    version: num(row.version) || num(row.report_seq) || 1,
    status: row.lifecycle === "superseded" ? "superseded" : "frozen",
    supersedesReportId: text(row.supersedes) || undefined,
    templateId: (text(typeof row.template === "string" ? row.template : (row.template as Api)?.id) || "general_src") as ReportTemplateId,
    createdAt: iso(row.frozen_at) || iso(row.created_at), updatedAt: iso(row.frozen_at) || iso(row.created_at),
    frozenAt: iso(row.frozen_at) || undefined, frozenBy: text(row.actor_id) || undefined,
    targetVersion: text(row.target_version) || text(doc.target_version) || "",
    scopeRef: text((row.authorization_snapshot as Api)?.record_id) || "",
    planRef: text((row.plan_snapshot as Api)?.record_id) || "",
    readiness: (text((row.review as Api)?.readiness) || "RED") as ReadinessLevel,
    sections: docSections(doc),
    evidenceIds: collectFrozenEvidenceIds(row, doc),
    materialGaps: ((row.review as Api)?.missing as string[]) ?? [],
    strangerChecklist: [],
    submission: {
      status,
      externalTicketId: text(ledger?.platform_reference),
      platformName: text(ledger?.platform) || "",
      submittedAt: status === "not_submitted" ? undefined : iso(ledger?.at) || iso(ledger?.created_at) || undefined,
      submittedBy: status === "not_submitted" ? undefined : text(ledger?.actor_id) || undefined,
      rewardStatus: (REWARD_BY_STATE[text(ledger?.status)] ?? "none") as RewardStatus,
      rewardAmount: amount,
      rewardCurrency: text(ledger?.currency) || "CNY",
      paidAt: text(ledger?.paid_at) || (text(ledger?.status) === "reward_paid" ? iso(payment?.at) || iso(payment?.created_at) || undefined : undefined),
      notes: text(ledger?.notes),
      auditTrail: audit.map((a) => ({ at: iso(a.at) || iso(a.created_at), by: text(a.actor_id), fromStatus: text(a.from_status), toStatus: text(a.status), note: text(a.notes) })),
    },
  } as StudioReport;
}

function draftToReport(row: Api, projectId: string): StudioReport {
  return {
    id: text(row.draft_id), projectId, findingId: text(row.finding_id),
    title: text(row.title) || "未命名草稿",
    target: text(row.target),
    evidenceExchangeId: text(row.evidence_exchange_id),
    draftSummary: text(row.summary),
    draftReviewNote: text(row.review_note),
    version: num(row.version) || 1, status: "draft",
    supersedesReportId: text(row.base_report_id) || text(row.supersedes) || undefined,
    templateId: (text(row.template_id) || "general_src") as ReportTemplateId,
    createdAt: iso(row.created_at) || iso(row.updated_at), updatedAt: iso(row.updated_at),
    targetVersion: text(row.target_version) || "",
    scopeRef: text(row.scope_ref), planRef: text(row.plan_ref),
    readiness: (text(row.readiness) || "RED") as ReadinessLevel,
    sections: docSections((row.sections ?? {}) as Api),
    evidenceIds: (row.evidence_ids as string[]) ?? [],
    materialGaps: (row.material_gaps as string[]) ?? [],
    strangerChecklist: [],
    submission: { status: "not_submitted", externalTicketId: "", platformName: "", rewardStatus: "none", rewardCurrency: "CNY", notes: "", auditTrail: [] },
    _revision: num(row.revision),
  } as unknown as StudioReport;
}

function feedbackToItem(row: Api, projectId: string): StudioPlatformFeedback {
  const feedbackId = text(row.feedback_id) || text(row.record_id);
  const backendKind = text(row.kind) || "general";
  const rawLinks = Array.isArray(row.links) ? row.links : [];
  const normalizedLinks = rawLinks.map((item) => {
    if (typeof item === "string") return item;
    if (item && typeof item === "object") {
      const obj = item as Api;
      return text(obj.url) || text(obj.label) || (text(obj.kind) && text(obj.id) ? `${text(obj.kind)}:${text(obj.id)}` : JSON.stringify(item));
    }
    return String(item ?? "");
  }).filter(Boolean);
  if (feedbackId) {
    feedbackMetaById.set(feedbackId, {
      backendKind,
      notes: text(row.notes),
      links: normalizedLinks,
    });
  }
  return {
    id: feedbackId,
    projectId,
    reportId: text(row.report_id),
    findingId: text(row.finding_id),
    externalTicketId: text(row.platform_reference),
    platformAuthor: text(row.source) || text(row.actor_id),
    receivedAt: iso(row.received_at) || iso(row.created_at) || iso(row._created_at),
    kind: FEEDBACK_KIND_FROM_SERVER[backendKind] ?? "accepted_notice",
    rawText: text(row.text),
    attachments: ((row.attachments as Api[]) ?? []).map((a) => ({ name: text(a.name) || text(a.artifact_id), mediaType: text(a.media_type), size: num(a.size), summary: "", evidenceId: undefined })),
    status: "pending",
    followUpTaskId: text(row.follow_up_task_id) || undefined,
    updatedReportId: undefined,
  };
}

function retestToItem(row: Api, projectId: string): StudioRetestRecord {
  const verdict = (row.verdict_input ?? {}) as Api;
  const result: RetestVerdict = row.result === "fixed" || row.result === "still_vulnerable"
    ? row.result : "inconclusive";
  return {
    id: text(row.retest_id) || text(row.record_id), projectId,
    status: text(row.status),
    findingId: text(row.finding_id), baseReportId: text(row.report_id),
    newReportId: undefined, feedbackId: undefined, runId: text(row.source_run_id) || undefined,
    targetFixVersion: text(row.expected_target_version), result,
    prerequisiteCheck: verdict.preconditions_valid === true ? "前置条件成立" : verdict.preconditions_valid === false ? "前置条件不成立" : "未验证",
    normalControlCheck: verdict.normal_control_works === true ? "正常流程不受影响" : verdict.normal_control_works === false ? "正常流程受影响" : "未验证",
    basisAndEvidence: text(row.repair_statement),
    evidenceIds: ((verdict.evidence_refs as Api[]) ?? []).map((r) => text(r.exchange_id) || text(r.artifact_id)).filter(Boolean),
    reviewer: text(verdict.reviewer) || text(row.requester), reviewedAt: iso(row.completed_at) || iso(row.created_at),
  };
}

export function createReportsDomain(): RealDomain {
  const draftRevisions = new Map<string, number>();
  const draftRawSections = new Map<string, Api>();
  const draftBaseReport = new Map<string, string>();
  const ledgerByReport = new Map<string, Api>();
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
      const [frozenRows, drafts, ledgerRows, feedbackRows, retestRows, histPage, todos] = await Promise.all([
        recordListAll(taskId, "frozen_report"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.report.drafts", {}),
        recordListAll(taskId, "platform_status"),
        recordListAll(taskId, "platform_feedback"),
        recordListAll(taskId, "finding_retest"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.history", { limit: 1000 }),
        querySrc<{ items: Api[] }>(taskId, "src.workbench.todo.list", {}),
      ]);
      const projectId = ctx.projectId;
      ledgerByReport.clear();
      const auditByReport = new Map<string, Api[]>();
      for (const row of ledgerRows.sort((a, b) => num(a.revision) - num(b.revision))) {
        const rid = text(row.report_id);
        auditByReport.set(rid, [...(auditByReport.get(rid) ?? []), row]);
        ledgerByReport.set(rid, row);
      }
      draftRevisions.clear();
      draftRawSections.clear();
      draftBaseReport.clear();
      const reports: StudioReport[] = [
        ...frozenRows.map((r) => frozenToReport(r, ledgerByReport.get(text(r.report_id) || text(r.record_id)), auditByReport.get(text(r.report_id) || text(r.record_id)) ?? [], projectId)),
        ...(drafts.items ?? []).map((d) => {
          const draftId = text(d.draft_id);
          draftRevisions.set(draftId, num(d.revision));
          if (d.sections && typeof d.sections === "object") {
            draftRawSections.set(draftId, d.sections as Api);
          }
          const baseRep = text(d.base_report_id) || text(d.supersedes);
          if (baseRep) draftBaseReport.set(draftId, baseRep);
          return draftToReport(d, projectId);
        }),
      ];
      const followUpByFeedback = new Map(todos.items
        .filter((todo) => todo.feedback_id && todo.status !== "deleted")
        .map((todo) => [text(todo.feedback_id), text(todo.todo_id)] as const));
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
        feedbacks: await Promise.all(feedbackRows.map(async (f) => {
          const item = feedbackToItem(f, projectId);
          item.followUpTaskId = followUpByFeedback.get(item.id);
          const ref = f.text_ref as Api | undefined;
          if (ref && text(ref.artifact_id)) {
            try {
              const env = await querySrc<Api>(taskId, "src.artifact.read", { ref_data: ref });
              const body = Array.isArray(env) ? env[0] as Api : env;
              if (body?.encoding === "base64" && typeof body.data === "string") {
                item.rawText = new TextDecoder("utf-8").decode(Uint8Array.from(atob(body.data), (c) => c.charCodeAt(0)));
              }
            } catch (readError) {
              item.rawText = `（反馈正文读取失败：${readError instanceof Error ? readError.message : String(readError)}）`;
            }
          }
          return item;
        })),
        retests: retestRows.map((r) => retestToItem(r, projectId)),
        history,
      };
    },
    mutators: {
      async createReportDraft(ctx, ...args) {
        const [findingId, options] = args as [string, { title?: string; templateId?: ReportTemplateId; target?: string } | undefined];
        const state = ctx.state();
        const clue = state.clues.find((c) => c.id === findingId);
        const target = options?.target?.trim() || state.sources.find((s) => s.id === clue?.sourceId)?.canonicalKey || "";
        const latestPlan = [...state.planVersions].sort((a, b) => b.version - a.version)[0];
        const latestScope = [...state.scopeSnapshots].sort((a, b) => b.version - a.version)[0];
        const payload: Api = {
          finding_id: findingId,
          title: options?.title?.trim() || (clue ? `${clue.title}（草稿）` : `报告草稿 ${findingId}`),
          target,
          template_id: options?.templateId ?? "general_src",
          evidence_exchange_id: clue?.supportingEvidenceIds?.[0] ?? "",
          evidence_ids: clue?.supportingEvidenceIds ?? [],
          ...(latestPlan?.id ? { plan_ref: latestPlan.id } : {}),
          ...(latestScope?.id ? { scope_ref: latestScope.id } : {}),
          sections: {
            mechanism: clue?.quote ?? "",
            confirmed_impact: clue?.impactSummary ?? "",
            conditional_impact: clue?.conditionalImpact ?? "",
            ...(clue?.counterEvidence?.length ? { counterevidence: clue.counterEvidence.join("\n") } : {}),
          },
          material_gaps: clue?.missingMaterials ?? [],
        };
        const saved = await postCommand<Api>(ctx.taskId, "src.workbench.report.draft.save", { payload });
        const createdId = text(saved.draft_id);
        const localDraft = ctx.proposal?.().reports.find((r) => r.status === "draft" && r.findingId === findingId && r.id !== createdId);
        return createdId ? { createdId, idMap: localDraft ? { [localDraft.id]: createdId } : undefined } : undefined;
      },
      async updateReportDraft(ctx, ...args) {
        const [reportId, patch] = args as [string, Record<string, unknown>];
        const current = ctx.state().reports.find((r) => r.id === reportId);
        const existingSections = draftRawSections.get(reportId) ?? {};
        const mergedSections = patch.sections
          ? sectionsToDoc(patch.sections as StudioReport["sections"], existingSections)
          : sectionsToDoc((current?.sections ?? {}) as StudioReport["sections"], existingSections);
        const payload: Api = {
          title: patch.title ?? current?.title,
          target: patch.target ?? current?.target,
          evidence_exchange_id: current?.evidenceExchangeId,
          template_id: patch.templateId ?? current?.templateId,
          target_version: patch.targetVersion ?? current?.targetVersion,
          finding_id: current?.findingId,
          ...(current?.planRef ? { plan_ref: current.planRef } : {}),
          ...(current?.scopeRef ? { scope_ref: current.scopeRef } : {}),
          ...(current?.supersedesReportId ? { base_report_id: current.supersedesReportId } : {}),
          sections: mergedSections,
          evidence_ids: patch.evidenceIds ?? current?.evidenceIds,
          material_gaps: current?.materialGaps ?? [],
        };
        const saved = await postCommand<Api>(ctx.taskId, "src.workbench.report.draft.save", {
          draft_id: current ? reportId : undefined,
          payload,
          ...(current ? { base_revision: draftRevisions.get(reportId) ?? 0 } : {}),
        });
        const createdId = text(saved.draft_id);
        return createdId && !current ? { createdId, idMap: { [reportId]: createdId } } : undefined;
      },
      async freezeOrForkReport(ctx, ...args) {
        const [reportId, mode] = args as [string, "freeze" | "new_version"];
        const report = ctx.state().reports.find((r) => r.id === reportId);
        if (!report) return;
        if (mode === "freeze") {
          const state = ctx.state();
          if (!report.target?.trim()) throw new Error("冻结报告前必须填写授权范围内的目标 URL");
          const [httpExchanges, importExchanges, screenshots] = await Promise.all([
            recordListAll(ctx.taskId, "http_exchange"),
            recordListAll(ctx.taskId, "import_exchange"),
            recordListAll(ctx.taskId, "browser_screenshot"),
          ]);
          const selectedIds = new Set([...report.evidenceIds, report.evidenceExchangeId].filter(Boolean));
          const seenArtifactIds = new Set<string>();
          const evidenceRefs: Api[] = [];
          const pushRef = (ref: Api | undefined) => {
            if (!ref || typeof ref !== "object") return;
            const aid = text(ref.artifact_id);
            if (!aid || seenArtifactIds.has(aid)) return;
            seenArtifactIds.add(aid);
            evidenceRefs.push(ref);
          };
          const allExchanges = [...httpExchanges, ...importExchanges];
          const matchedExchanges = allExchanges.filter((ex) => {
            const exchangeSelected = selectedIds.has(text(ex.exchange_id));
            let included = exchangeSelected;
            for (const ref of [ex.request_raw_ref, ex.response_raw_ref] as (Api | undefined)[]) {
              if (ref && (exchangeSelected || selectedIds.has(text(ref.artifact_id)))) {
                pushRef(ref);
                included = true;
              }
            }
            return included;
          });
          for (const shot of screenshots) {
            const sid = text(shot.screenshot_id) || text(shot._record_id);
            const art = (shot.artifact && typeof shot.artifact === "object" ? (shot.artifact as Api) : undefined);
            const aid = text(art?.artifact_id);
            if ((sid && selectedIds.has(sid)) || (aid && selectedIds.has(aid))) {
              pushRef(art);
            }
          }
          const exchanges = matchedExchanges.map((exchange) => ({
            exchange_id: text(exchange.exchange_id), observed_url: text(exchange.observed_url),
            method: text(exchange.method), protocol: text(exchange.protocol), status_code: exchange.status_code,
            started_at: exchange.started_at, duration_ms: exchange.duration_ms,
            query: exchange.query, request_headers: exchange.request_headers, response_headers: exchange.response_headers,
            request_raw_ref: exchange.request_raw_ref, response_raw_ref: exchange.response_raw_ref,
            execution_state: text(exchange.execution_state), capture_state: text(exchange.capture_state),
            context_id: text(exchange.context_id), source_run_id: text(exchange.source_run_id),
          }));
          const claims = (state.clues.find((c) => c.id === report.findingId)?.claims ?? []).map((c) => ({
            claim_id: c.id,
            text: c.text,
            status: c.kind === "observed" ? "observed" : c.kind === "inferred" ? "supported_inference" : "unverified",
          }));
          const rawExistingDoc = draftRawSections.get(report.id) ?? {};
          const supersedes = report.supersedesReportId || draftBaseReport.get(report.id) || undefined;
          const latestPlanId = report.planRef || [...state.planVersions].sort((a, b) => b.version - a.version)[0]?.id || "";
          const latestScopeId = report.scopeRef || [...state.scopeSnapshots].sort((a, b) => b.version - a.version)[0]?.id || "";
          const frozen = await postCommand<Api>(ctx.taskId, "src.report.freeze", {
            finding_id: report.findingId, source_run_id: `workbench-${ctx.actor}`,
            document: { title: report.title, ...sectionsToDoc(report.sections, rawExistingDoc), target: report.target, target_version: report.targetVersion, exchanges },
            claims, review: { readiness: report.readiness, missing: report.materialGaps },
            evidence_refs: evidenceRefs, cvss: { vector: report.sections.cvssVector, score: report.sections.cvssScore },
            deduplication: {}, template: report.templateId,
            ...(supersedes ? { supersedes } : {}),
            plan_snapshot: latestPlanId ? { record_id: latestPlanId } : {},
            authorization_snapshot: latestScopeId ? { record_id: latestScopeId } : {},
            source_watermarks: {}, idempotency_key: `wb-freeze-${reportId}-${Date.now().toString(36)}`,
            actor_id: ctx.actor,
          });
          const frozenId = text(frozen.report_id);
          return frozenId ? { createdId: frozenId, idMap: { [reportId]: frozenId } } : undefined;
        }
        const saved = await postCommand<Api>(ctx.taskId, "src.workbench.report.draft.save", {
          payload: {
            title: `${report.title}（v${report.version + 1} 草稿）`, template_id: report.templateId,
            target: report.target, target_version: report.targetVersion, finding_id: report.findingId,
            evidence_exchange_id: report.evidenceExchangeId,
            sections: sectionsToDoc(report.sections, draftRawSections.get(report.id) ?? {}), evidence_ids: report.evidenceIds,
            scope_ref: report.scopeRef, plan_ref: report.planRef,
            base_report_id: reportId, version: report.version + 1,
          },
        });
        const local = ctx.proposal?.().reports.find((r) => r.status === "draft" && r.id !== saved.draft_id && r.findingId === report.findingId);
        return { createdId: text(saved.draft_id), idMap: local ? { [local.id]: text(saved.draft_id) } : undefined };
      },
      async updateSubmissionLedger(ctx, ...args) {
        const [reportId, patch] = args as [string, Record<string, unknown>];
        const current = (ctx.state().reports.find((r) => r.id === reportId)?.submission ?? {}) as Record<string, unknown>;
        const status = patch.rewardStatus !== undefined
          ? (patch.rewardStatus === "paid" ? "reward_paid"
            : patch.rewardStatus === "confirmed" ? "reward_confirmed"
            : patch.rewardStatus === "reward_pending" ? "reward_pending"
            : (SUBMIT_MAP[text(patch.status ?? current.status)] ?? "submitted"))
          : patch.status !== undefined && patch.status !== current.status
            ? SUBMIT_MAP[text(patch.status)]
            : text(ledgerByReport.get(reportId)?.status) || SUBMIT_MAP[text(current.status)];
        await postCommand(ctx.taskId, "src.platform.status.record", {
          report_id: reportId, status, actor_id: ctx.actor, actor_kind: "operator",
          base_revision: num(ledgerByReport.get(reportId)?.revision),
          platform_reference: patch.externalTicketId !== undefined ? text(patch.externalTicketId) : text(current.externalTicketId),
          ...(patch.platformName !== undefined ? { platform: text(patch.platformName) } : text(current.platformName) ? { platform: text(current.platformName) } : {}),
          amount: patch.rewardAmount !== undefined ? patch.rewardAmount : (current.rewardAmount ?? undefined),
          currency: patch.rewardCurrency !== undefined ? text(patch.rewardCurrency) : (text(current.rewardCurrency) || "CNY"),
          ...(patch.paidAt !== undefined ? { paid_at: text(patch.paidAt) || null } : {}),
          notes: patch.notes !== undefined ? text(patch.notes) : text(current.notes),
        });
        await recordHistory(ctx, { op: "noop" }, { action: "status", summary: `更新提交台账 ${reportId} → ${status}`, refs: [{ kind: "report", id: reportId }] });
      },
      async addPlatformFeedback(ctx, ...args) {
        const [input] = args as [{
          reportId?: string;
          findingId?: string;
          externalTicketId?: string;
          platformAuthor?: string;
          rawText?: string;
          kind?: string;
          links?: string[];
          notes?: string;
          receivedAt?: string;
        }];
        if (!input.rawText?.trim()) throw new Error("platform feedback requires text or attachments");
        const backendKind = FEEDBACK_KIND_TO_SERVER[text(input.kind)] ?? "general";
        const saved = await postCommand<Api>(ctx.taskId, "src.feedback.record", {
          ...(input.reportId ? { report_id: input.reportId } : {}),
          ...(input.findingId ? { finding_id: input.findingId } : {}),
          kind: backendKind,
          text: input.rawText ?? "",
          ...(input.notes?.trim() ? { notes: input.notes.trim() } : {}),
          ...(input.links?.length ? { links: input.links } : {}),
          source: input.platformAuthor || "platform",
          platform_reference: input.externalTicketId ?? "",
          ...(input.receivedAt ? { received_at: input.receivedAt } : {}),
        });
        const createdId = text(saved.feedback_id);
        const local = ctx.proposal?.().feedbacks.find((f) => f.rawText === input.rawText && f.id !== createdId);
        return {
          createdId: createdId || undefined,
          idMap: local && createdId ? { [local.id]: createdId } : undefined,
        };
      },
      async createTaskFromFeedback(ctx, ...args) {
        const [feedbackId] = args as [string];
        const feedback = ctx.state().feedbacks.find((f) => f.id === feedbackId);
        const meta = feedbackMetaById.get(feedbackId);
        const links: StudioRef[] = [
          { kind: "feedback", id: feedbackId },
          ...(feedback?.reportId ? [{ kind: "report" as const, id: feedback.reportId }] : []),
          ...(feedback?.findingId ? [{ kind: "clue" as const, id: feedback.findingId }] : []),
        ];
        const saved = await postCommand<Api>(ctx.taskId, "src.workbench.todo.save", {
          payload: {
            title: `补件跟进：${feedback?.externalTicketId || feedbackId}`,
            status: "open",
            origin_kind: "feedback",
            feedback_id: feedbackId,
            ...(feedback?.reportId ? { report_id: feedback.reportId } : {}),
            ...(feedback?.findingId ? { finding_id: feedback.findingId } : {}),
            links,
            notes: [feedback?.rawText, meta?.notes, ...(meta?.links ?? [])].filter(Boolean).join("\n"),
          },
        });
        const createdId = text(saved.todo_id);
        const local = ctx.proposal?.().tasks.find((t) => t.originKind === "feedback" && t.links.some((l) => l.id === feedbackId) && t.id !== createdId);
        return createdId ? { createdId, idMap: local ? { [local.id]: createdId } : undefined } : undefined;
      },
      async recordRetest(ctx, ...args) {
        const [input] = args as [{ findingId: string; baseReportId: string; targetFixVersion: string; result: string; prerequisiteCheck?: string; normalControlCheck?: string; basisAndEvidence?: string; evidenceIds?: string[] }];
        const currentPolicy = await querySrc<Api>(ctx.taskId, "src.policy.current", {});
        const policyId = text(currentPolicy.policy_id) || ctx.taskId;
        const policyVersion = num(currentPolicy.version);
        if (!policyVersion) throw new Error("当前 SRC 授权策略尚未就绪");
        const requested = await postCommand<Api>(ctx.taskId, "src.retest.request", {
          report_id: input.baseReportId, finding_id: input.findingId,
          repair_statement: input.basisAndEvidence, expected_target_version: input.targetFixVersion,
          authorization_ref: `${policyId}:v${policyVersion}`, requester: ctx.actor,
          idempotency_key: `wb-retest-${input.baseReportId}-${Date.now().toString(36)}`,
        });
        const retestId = text(requested.retest_id);
        if (!retestId) throw new Error("复测请求未返回记录 ID");
        await postCommand(ctx.taskId, "src.retest.dispatch", { retest_id: retestId });
        const local = ctx.proposal?.().retests.find((rt) => rt.baseReportId === input.baseReportId && rt.id !== retestId);
        return {
          createdId: retestId,
          idMap: local ? { [local.id]: retestId } : undefined,
        };
      },
      async completeRetest(ctx, ...args) {
        const [input] = args as [{
          retestId: string; preconditionsValid: boolean; targetVersionConfirmed: boolean;
          originalPathImpact: boolean | null; normalControlWorks: boolean | null;
          evidenceRefs: Api[]; executionReceiptIds: string[]; outcomeUnknown: boolean;
        }];
        await postCommand(ctx.taskId, "src.retest.complete", {
          retest_id: input.retestId,
          preconditions_valid: input.preconditionsValid,
          target_version_confirmed: input.targetVersionConfirmed,
          original_path_impact: input.originalPathImpact,
          normal_control_works: input.normalControlWorks,
          evidence_refs: input.evidenceRefs,
          execution_receipt_ids: input.executionReceiptIds,
          reviewer: ctx.actor,
          outcome_unknown: input.outcomeUnknown,
        });
      },
      async revertHistory(ctx, ...args) {
        const [entryId] = args as [string];
        const entry = ctx.state().history.find((e) => e.id === entryId || e.serverId === entryId);
        if (!entry || (entry.serverDomain && entry.serverDomain !== "reports")) return;
        const undo = entry.serverUndo;
        if (!undo || undo.op === "noop") {
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
