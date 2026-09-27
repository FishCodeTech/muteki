/** Vulnerability-report projection helpers extracted from the event type model. */
import type {
  BlackboardTruncation,
  BlackboardView,
  BlackboardVulnReport,
  MutekiEvent,
  VulnReportStatus,
} from "./types";

export const REPORT_CAP = 80;
export const REVIEW_CAP = 80;
export const POC_CAP = 80;
export const ROUTE_CAP = 60;
export const DIRECTIVE_CAP = 60;
export const FACT_CAP = 200;
export const DEAD_END_CAP = 50;

const REPORT_RANK: Record<VulnReportStatus, number> = {
  submitted: 1,
  repro_failed: 2,
  reproduced: 3,
  accepted: 4,
  rejected: 4,
};

export function canAdvanceReportStatus(from: VulnReportStatus, to: VulnReportStatus): boolean {
  if (from === to) return true;
  if (from === "accepted" || from === "rejected") return false;
  if (from === "repro_failed" && to === "reproduced") return true;
  return REPORT_RANK[to] > REPORT_RANK[from];
}

export function stringField(value: unknown): string | undefined {
  if (value == null) return undefined;
  const text = String(value).trim();
  return text ? text : undefined;
}

export function stringListField(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const items = value.map((item) => String(item ?? "").trim()).filter(Boolean);
  return items.length ? items : undefined;
}

export function vulnReportPatch(
  p: Record<string, unknown>,
  actor: string,
  ts: number,
  status: VulnReportStatus,
): BlackboardVulnReport {
  const id = stringField(p.report_id) || "";
  return {
    id,
    title: stringField(p.title) || id,
    findingClass: stringField(p.finding_class) || "",
    resourceId: stringField(p.resource_id) || "",
    impactWho: stringField(p.impact_who),
    impactWhat: stringField(p.impact_what),
    witness: stringField(p.witness),
    vector: stringField(p.vector),
    goalQualified: typeof p.goal_qualified === "boolean" ? p.goal_qualified : undefined,
    goalCode: stringField(p.goal_code),
    goalDetail: stringField(p.goal_detail),
    reproVerifier: stringField(p.repro_verifier),
    reproCommand: stringField(p.repro_command),
    reproTarget: stringField(p.repro_target),
    reproResponseSummary: stringField(p.repro_response_summary),
    reproEvidence: p.repro_evidence && typeof p.repro_evidence === "object"
      ? p.repro_evidence as Record<string, unknown> : undefined,
    replayCommand: stringField(p.replay_command),
    steps: stringListField(p.steps),
    preconditions: stringField(p.preconditions),
    affectedRole: stringField(p.affected_role),
    narrative: stringField(p.narrative),
    markdown: stringField(p.markdown),
    status,
    code: stringField(p.code),
    reason: stringField(p.reason) ?? stringField(p.detail),
    actor,
    ts,
    intentId: stringField(p.intent_id),
    submitter: stringField(p.submitter),
    history: [],
  };
}

export function mergeVulnReportFields(existing: BlackboardVulnReport, patch: BlackboardVulnReport): BlackboardVulnReport {
  const next: BlackboardVulnReport = { ...existing, history: [...(existing.history ?? [])] };
  (Object.keys(patch) as (keyof BlackboardVulnReport)[]).forEach((key) => {
    if (key === "status" || key === "history" || key === "eventSeq") return;
    const value = patch[key];
    if (value === undefined) return;
    if (typeof value === "string" && value === "") return;
    (next as unknown as Record<string, unknown>)[key] = value;
  });
  if (patch.title === patch.id && existing.title && existing.title !== existing.id) {
    next.title = existing.title;
  }
  return next;
}

export function applyReportTransition(
  existing: BlackboardVulnReport | undefined,
  patch: BlackboardVulnReport,
  eventSeq: number,
): BlackboardVulnReport {
  if (!existing) {
    return {
      ...patch,
      eventSeq,
      history: [{
        status: patch.status,
        ts: patch.ts,
        actor: patch.actor,
        eventSeq,
        reason: patch.reason,
      }],
    };
  }
  const merged = mergeVulnReportFields(existing, patch);
  // Stable report ids may be submitted again after a rejected duplicate. A later
  // authoritative transition opens a new review of that same identity.
  if (existing.status === "rejected" && patch.status === "submitted" && eventSeq > (existing.eventSeq ?? 0)) {
    return {
      ...merged,
      status: "submitted",
      ts: patch.ts,
      eventSeq,
      actor: patch.actor || existing.actor,
      code: patch.code,
      reason: patch.reason,
      history: [...(existing.history ?? []), {
        status: "submitted",
        ts: patch.ts,
        actor: patch.actor,
        eventSeq,
        reason: patch.reason,
      }],
    };
  }
  if (existing.status === "rejected" && patch.status === "reproduced" && eventSeq > (existing.eventSeq ?? 0)) {
    return {
      ...merged,
      status: "reproduced",
      ts: patch.ts,
      eventSeq,
      actor: patch.actor || existing.actor,
      code: undefined,
      reason: undefined,
      history: [...(existing.history ?? []), {
        status: "reproduced",
        ts: patch.ts,
        actor: patch.actor,
        eventSeq,
      }],
    };
  }
  if (existing.status === "rejected" && patch.status === "accepted" && eventSeq > (existing.eventSeq ?? 0)) {
    return {
      ...merged,
      status: "accepted",
      ts: patch.ts,
      eventSeq,
      actor: patch.actor || existing.actor,
      code: undefined,
      reason: undefined,
      history: [...(existing.history ?? []), {
        status: "accepted",
        ts: patch.ts,
        actor: patch.actor,
        eventSeq,
        reason: patch.reason,
      }],
    };
  }
  if (!canAdvanceReportStatus(existing.status, patch.status)) {
    return merged;
  }
  if (existing.status !== patch.status) {
    merged.status = patch.status;
    merged.ts = patch.ts;
    merged.eventSeq = eventSeq;
    if (patch.actor) merged.actor = patch.actor;
    if (patch.reason) merged.reason = patch.reason;
    if (patch.code) merged.code = patch.code;
    merged.history = [...(existing.history ?? []), {
      status: patch.status,
      ts: patch.ts,
      actor: patch.actor,
      eventSeq,
      reason: patch.reason,
    }];
  }
  return merged;
}

export function markTruncated(bb: BlackboardView, key: keyof BlackboardTruncation): void {
  bb.truncated = { ...(bb.truncated ?? {}), [key]: true };
}

export function capPush<T>(list: T[], item: T, cap: number): { list: T[]; truncated: boolean } {
  const next = [...list, item];
  if (next.length <= cap) return { list: next, truncated: false };
  return { list: next.slice(-cap), truncated: true };
}

export function upsertVulnReport(bb: BlackboardView, row: BlackboardVulnReport, eventSeq: number): void {
  if (!row.id) return;
  const existing = bb.vulnReports.find((item) => item.id === row.id);
  const nextRow = applyReportTransition(existing, row, eventSeq);
  if (existing) {
    bb.vulnReports = bb.vulnReports.map((item) => item.id === row.id ? nextRow : item);
    return;
  }
  const pushed = capPush(bb.vulnReports, nextRow, REPORT_CAP);
  bb.vulnReports = pushed.list;
  if (pushed.truncated) markTruncated(bb, "reports");
}

export function patchReportStatus(
  bb: BlackboardView,
  ev: MutekiEvent,
  id: string,
  status: VulnReportStatus,
  extra: Partial<BlackboardVulnReport> = {},
): void {
  if (!id) return;
  const existing = bb.vulnReports.find((item) => item.id === id);
  if (!existing) return;
  const patch: BlackboardVulnReport = {
    ...existing,
    ...extra,
    id,
    status,
    actor: extra.actor || ev.solver_id || existing.actor,
    ts: ev.ts,
    history: existing.history ?? [],
  };
  const next = applyReportTransition(existing, patch, ev.seq || 0);
  bb.vulnReports = bb.vulnReports.map((item) => item.id === id ? next : item);
}
