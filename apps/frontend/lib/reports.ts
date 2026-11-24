import { apiFetch } from "@/lib/useRun";

export const RUN_DRAG_MIME = "application/x-muteki-run";

let activeRunDragId = "";

export function beginRunDrag(runId: string): void {
  activeRunDragId = runId;
}

export function endRunDrag(runId?: string): void {
  if (!runId || activeRunDragId === runId) {
    activeRunDragId = "";
  }
}

export function hasRunDrag(dt?: DataTransfer | null): boolean {
  return Boolean(activeRunDragId) || Array.from(dt?.types ?? []).includes(RUN_DRAG_MIME);
}

export function readRunDragId(dt?: DataTransfer | null): string {
  return dt?.getData(RUN_DRAG_MIME) || activeRunDragId || "";
}

export interface RunPointerDragEvent {
  phase: "start" | "move" | "drop" | "cancel";
  runId: string;
  x: number;
  y: number;
}

const runPointerDragListeners = new Set<(event: RunPointerDragEvent) => boolean | void>();

export function subscribeRunPointerDrag(
  listener: (event: RunPointerDragEvent) => boolean | void,
): () => void {
  runPointerDragListeners.add(listener);
  return () => {
    runPointerDragListeners.delete(listener);
  };
}

export function emitRunPointerDrag(event: RunPointerDragEvent): boolean {
  let handled = false;
  for (const listener of [...runPointerDragListeners]) {
    if (listener(event) === true) handled = true;
  }
  return handled;
}

export type ReportSourceKind = "manual" | "ai_writeup" | "pentest_report" | "import";

export interface ReportGeneration {
  job_id: string;
  state: "queued" | "running" | "failed";
  base_sha256?: string;
  started_at: number;
  finished_at?: number;
  error?: { code: string; message: string };
  queue_position?: number;
  progress?: { phase?: string; tool_name?: string | null; summary?: string; turn?: number; elapsed_ms?: number };
  updated_at?: number;
}

export interface ReportPendingUpdate {
  job_id: string;
  created_at: number;
  bytes: number;
}

export interface RunReport {
  id: string;
  run_id: string | null;
  mode: "ctf" | "pentest";
  title: string;
  markdown: string;
  status: "draft" | "final";
  tags: string[];
  source: { kind: ReportSourceKind; ref: string | null };
  folder_id?: string | null;
  order?: number;
  import_source?: { path: string; sha256: string } | null;
  ai_sha256?: string | null;
  generation?: ReportGeneration | null;
  pending_update?: ReportPendingUpdate | null;
  created_at: number;
  updated_at: number;
  revision: number;
}

export interface ReportFolder {
  id: string;
  mode: "ctf" | "pentest";
  name: string;
  parent_id: string | null;
  order: number;
  revision: number;
  created_at: number;
  updated_at: number;
}

export interface ReportCandidate {
  run_id: string;
  name?: string;
  category?: string;
  status?: string;
  solved?: boolean;
  finished?: boolean;
  running?: boolean;
  archived?: boolean;
  folder_id?: string | null;
  latest_writeup?: { job_id: string; updated_at: number; bytes: number; model?: string | null } | null;
  active_job?: { id: string; state: string; created_at: number } | null;
  last_failed_job?: { id: string; updated_at: number; error?: { code: string; message: string } } | null;
  report_ids?: string[];
  unavailable_reason?: "running" | "draft" | null;
}

export interface ReportBatchResult {
  run_id: string;
  ok: boolean;
  report_id?: string;
  job_id?: string;
  skipped?: string;
  error?: { code: string; message: string };
}

export interface WriteupTemplate {
  id: string;
  name: string;
  description: string;
  markdown: string;
  prompt?: string;
  builtin?: boolean;
}

export interface ReportGenerationSettings {
  max_concurrent: number;
  running: number;
  queued: number;
  writeup_prompt: string;
  default_writeup_prompt: string;
  default_template_id: string;
  templates: WriteupTemplate[];
  builtin_templates: WriteupTemplate[];
}

export type PentestSeverity = "critical" | "high" | "medium" | "low" | "informational" | "unrated";
export type PentestReviewStatus = "accepted" | "pending" | "rejected" | "needs_more_evidence";
export type PentestDeliveryStatus = "draft" | "pending" | "generated" | "failed" | "stale";

export interface PentestSeverityCounts {
  critical: number;
  high: number;
  medium: number;
  low: number;
  informational: number;
  unrated: number;
}

export interface PentestHubFinding {
  key: string;
  id: string;
  run_id: string;
  run_name: string;
  target: string;
  scope: string[];
  report_seq: number | null;
  delivery_status: PentestDeliveryStatus;
  updated_at: number;
  has_reader_prose: boolean;
  title: string;
  raw_title?: string;
  finding_class?: string;
  resource_id?: string;
  severity: PentestSeverity;
  review_status: PentestReviewStatus;
  review_note?: string;
  affected_assets: string[];
  summary: string;
  observed_impact: string;
  potential_impact?: string;
  preconditions?: string;
  severity_rationale: string;
  remediation: string;
  evidence_fact_seqs: number[];
  screenshot_count: number;
}

export interface PentestHubRun {
  run_id: string;
  name: string;
  report_title: string;
  target: string;
  scope: string[];
  status?: string;
  running: boolean;
  finished: boolean;
  started_at?: number | null;
  finished_at?: number | null;
  updated_at: number;
  delivery_status: PentestDeliveryStatus;
  report_seq: number | null;
  version_count: number;
  objective: string;
  objective_status: "met" | "unproven";
  report_goal_mode: "automatic" | "count";
  expected_findings: number | null;
  qualified_findings: number;
  finding_count: number;
  total_submissions: number;
  unaccepted_count: number;
  executive_summary: string;
  severity_counts: PentestSeverityCounts;
}

export function renderWriteupTemplate(
  templateMarkdown: string,
  vars: { title?: string; category?: string; target?: string | null; flag?: string | null },
): string {
  return templateMarkdown
    .replaceAll("{{title}}", vars.title?.trim() || "未命名题目")
    .replaceAll("{{category}}", vars.category?.trim() || "未分类")
    .replaceAll("{{target}}", vars.target?.trim() || "无（本地附件题）")
    .replaceAll("{{flag}}", vars.flag?.trim() || "flag{...}");
}

export interface ReportRunSummary {
  name?: string;
  category?: string;
  mode?: string;
  status?: string;
  solved?: boolean;
  finished?: boolean;
  flag?: string | null;
  folder_id?: string | null;
}

export class ReportError extends Error {
  code: string;
  status: number;
  report?: RunReport;
  constructor(message: string, code: string, status: number, report?: RunReport) {
    super(message);
    this.code = code;
    this.status = status;
    this.report = report;
  }
}

async function parseOrThrow(response: Response): Promise<Record<string, any>> {
  const data = await response.json().catch(() => ({}));
  if (!response.ok || data.ok === false) {
    throw new ReportError(
      typeof data?.error?.message === "string"
        ? data.error.message
        : typeof data?.detail === "string"
          ? data.detail
          : `HTTP ${response.status}`,
      typeof data?.error?.code === "string" ? data.error.code : "",
      response.status,
      data?.report,
    );
  }
  return data;
}

const jsonHeaders = { "Content-Type": "application/json" };

export function generationActive(report: Pick<RunReport, "generation">): boolean {
  const state = report.generation?.state;
  return state === "queued" || state === "running";
}

export async function listReports(mode: "ctf" | "pentest"): Promise<{
  reports: RunReport[];
  runs: Record<string, ReportRunSummary>;
  folders: ReportFolder[];
}> {
  const res = await apiFetch(`/api/reports?mode=${mode}`);
  const data = await parseOrThrow(res);
  return { reports: data.reports ?? [], runs: data.runs ?? {}, folders: data.folders ?? [] };
}

export async function listRunReports(runId: string): Promise<RunReport[]> {
  const res = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/reports`);
  const data = await parseOrThrow(res);
  return data.reports ?? [];
}

export async function getReport(id: string): Promise<RunReport> {
  const res = await apiFetch(`/api/reports/${encodeURIComponent(id)}`);
  const data = await parseOrThrow(res);
  return data.report;
}

export async function createReport(body: {
  mode?: "ctf" | "pentest";
  run_id?: string | null;
  title: string;
  markdown?: string;
  status?: "draft" | "final";
  tags?: string[];
  source?: { kind: ReportSourceKind; ref?: string | null };
  folder_id?: string | null;
  order?: number;
  import_source?: { path: string; sha256: string } | null;
}): Promise<RunReport> {
  const res = await apiFetch(`/api/reports`, {
    method: "POST",
    headers: jsonHeaders,
    body: JSON.stringify(body),
  });
  const data = await parseOrThrow(res);
  return data.report;
}

export async function updateReport(
  id: string,
  patch: {
    title?: string;
    markdown?: string;
    status?: "draft" | "final";
    tags?: string[];
    folder_id?: string | null;
    order?: number;
    expected_revision?: number;
  },
): Promise<RunReport> {
  const res = await apiFetch(`/api/reports/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: jsonHeaders,
    body: JSON.stringify(patch),
  });
  const data = await parseOrThrow(res);
  return data.report;
}

export async function deleteReport(id: string): Promise<void> {
  const res = await apiFetch(`/api/reports/${encodeURIComponent(id)}`, { method: "DELETE" });
  await parseOrThrow(res);
}

export async function generateReport(
  id: string,
  opts?: { templateId?: string; customPrompt?: string; customTemplate?: string },
): Promise<{ report: RunReport; job: Record<string, any> | null }> {
  const payload: Record<string, unknown> = {};
  if (opts?.templateId) payload.template_id = opts.templateId;
  if (opts?.customPrompt) payload.custom_prompt = opts.customPrompt;
  if (opts?.customTemplate) payload.custom_template = opts.customTemplate;
  const res = await apiFetch(`/api/reports/${encodeURIComponent(id)}/generate`, {
    method: "POST",
    headers: jsonHeaders,
    body: JSON.stringify(payload),
  });
  const data = await parseOrThrow(res);
  return { report: data.report, job: data.job ?? null };
}

export async function cancelReportGeneration(id: string): Promise<RunReport> {
  const res = await apiFetch(`/api/reports/${encodeURIComponent(id)}/generation`, { method: "DELETE" });
  const data = await parseOrThrow(res);
  return data.report;
}

export async function resolvePendingUpdate(
  id: string,
  action: "apply" | "dismiss",
  expectedRevision: number,
): Promise<RunReport> {
  const res = await apiFetch(`/api/reports/${encodeURIComponent(id)}/pending-update`, {
    method: "POST",
    headers: jsonHeaders,
    body: JSON.stringify({ action, expected_revision: expectedRevision }),
  });
  const data = await parseOrThrow(res);
  return data.report;
}

export async function fetchFollowupArtifact(runId: string, jobId: string): Promise<string> {
  const res = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/followups/${encodeURIComponent(jobId)}/artifact`);
  const data = await res.json().catch(() => ({}));
  if (!res.ok || data.ok === false) {
    throw new ReportError(data?.error?.message || `HTTP ${res.status}`, data?.error?.code ?? "", res.status);
  }
  return data.markdown ?? "";
}

export async function fetchReportCandidates(mode: "ctf" | "pentest" = "ctf"): Promise<ReportCandidate[]> {
  const res = await apiFetch(`/api/reports/candidates?mode=${mode}`);
  const data = await parseOrThrow(res);
  return data.candidates ?? [];
}

export async function batchReports(
  action: "generate" | "import_writeups",
  runIds: string[],
  folderId?: string | null,
  opts?: { templateId?: string; customPrompt?: string; customTemplate?: string },
): Promise<ReportBatchResult[]> {
  const res = await apiFetch(`/api/reports/batch`, {
    method: "POST",
    headers: jsonHeaders,
    body: JSON.stringify({
      action,
      run_ids: runIds,
      ...(folderId !== undefined ? { folder_id: folderId } : {}),
      ...(opts?.templateId ? { template_id: opts.templateId } : {}),
      ...(opts?.customPrompt ? { custom_prompt: opts.customPrompt } : {}),
      ...(opts?.customTemplate ? { custom_template: opts.customTemplate } : {}),
    }),
  });
  const data = await parseOrThrow(res);
  return data.results ?? [];
}

function normalizeGenerationSettings(data: Record<string, any>): ReportGenerationSettings {
  return {
    max_concurrent: data.max_concurrent ?? 3,
    running: data.running ?? 0,
    queued: data.queued ?? 0,
    writeup_prompt: typeof data.writeup_prompt === "string" ? data.writeup_prompt : "",
    default_writeup_prompt: typeof data.default_writeup_prompt === "string" ? data.default_writeup_prompt : "",
    default_template_id: typeof data.default_template_id === "string" ? data.default_template_id : "builtin-standard",
    templates: Array.isArray(data.templates) ? data.templates : [],
    builtin_templates: Array.isArray(data.builtin_templates) ? data.builtin_templates : [],
  };
}

export async function getGenerationSettings(): Promise<ReportGenerationSettings> {
  const res = await apiFetch(`/api/reports/settings`);
  const data = await parseOrThrow(res);
  return normalizeGenerationSettings(data);
}

export async function putGenerationSettings(
  patch:
    | number
    | {
        max_concurrent?: number;
        writeup_prompt?: string | null;
        default_template_id?: string | null;
        templates?: WriteupTemplate[] | null;
      },
): Promise<ReportGenerationSettings> {
  const body = typeof patch === "number" ? { max_concurrent: patch } : patch;
  const res = await apiFetch(`/api/reports/settings`, {
    method: "PUT",
    headers: jsonHeaders,
    body: JSON.stringify(body),
  });
  const data = await parseOrThrow(res);
  return normalizeGenerationSettings(data);
}

export type PentestHubReportError = {
  run_id: string;
  run_name: string;
  code: string;
  message: string;
};

export async function fetchPentestReportsHub(): Promise<{
  runs: PentestHubRun[];
  findings: PentestHubFinding[];
  reportErrors: PentestHubReportError[];
}> {
  const res = await apiFetch(`/api/pentest/reports`);
  const data = await parseOrThrow(res);
  return { runs: data.runs ?? [], findings: data.findings ?? [], reportErrors: data.report_errors ?? [] };
}

export async function fetchPentestMarkdown(
  runId: string,
  opts?: { findingId?: string | null; reportSeq?: number | null },
): Promise<string> {
  const params = new URLSearchParams();
  if (opts?.reportSeq != null) params.set("report_seq", String(opts.reportSeq));
  if (opts?.findingId) params.set("finding", opts.findingId);
  const qs = params.toString();
  const res = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/pentest-report.md${qs ? `?${qs}` : ""}`);
  if (!res.ok) {
    const data = await res.json().catch(() => ({}));
    throw new Error(data?.error?.message || data?.detail || `HTTP ${res.status}`);
  }
  return await res.text();
}

export async function createReportFolder(body: {
  mode: "ctf" | "pentest";
  name: string;
  parent_id?: string | null;
  order?: number;
}): Promise<ReportFolder> {
  const res = await apiFetch(`/api/report-folders`, {
    method: "POST",
    headers: jsonHeaders,
    body: JSON.stringify(body),
  });
  const data = await parseOrThrow(res);
  return data.folder;
}

export async function updateReportFolder(
  id: string,
  patch: { name?: string; parent_id?: string | null; order?: number; expected_revision?: number },
): Promise<ReportFolder> {
  const res = await apiFetch(`/api/report-folders/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: jsonHeaders,
    body: JSON.stringify(patch),
  });
  const data = await parseOrThrow(res);
  return data.folder;
}

export async function deleteReportFolder(
  id: string,
  strategy: "move_up" | "delete_all" = "move_up",
): Promise<{ deleted_reports: number; deleted_folders: number }> {
  const res = await apiFetch(`/api/report-folders/${encodeURIComponent(id)}?strategy=${strategy}`, { method: "DELETE" });
  const data = await parseOrThrow(res);
  return { deleted_reports: data.deleted_reports ?? 0, deleted_folders: data.deleted_folders ?? 0 };
}
