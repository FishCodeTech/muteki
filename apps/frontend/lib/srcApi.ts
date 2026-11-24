import { createReadCache } from "./readCache";
import { srcFetch as apiFetch } from "./srcFetch";
import { parseSrcDebugLogsMetadata, srcDebugLogsFilename, type SrcDebugLogsMetadata } from "./srcDebugLogs";
import type {
  ExecutionApproval, ExecutionCapabilities, ExecutionPolicy, ExecutionReceipt,
  PolicyPreview, SrcCommandReceipt, SrcRecord, SrcRun, SrcTask, TaskExecutionGrant,
  WorkbenchSlicePayload, WorkbenchSliceSaveResult, WorkbenchSnapshot,
} from "./srcTypes";

export type { SrcDebugLogsMetadata };

export class SrcApiError extends Error {
  code: string;
  status: number;
  detail?: Record<string, unknown>;

  constructor(message: string, code = "src.request_failed", status = 0, detail?: Record<string, unknown>) {
    super(message);
    this.name = "SrcApiError";
    this.code = code;
    this.status = status;
    this.detail = detail;
  }
}

function idem(prefix: string): string {
  const value = typeof crypto !== "undefined" && crypto.randomUUID ? crypto.randomUUID() : `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  return `${prefix}-${value}`;
}

async function parse(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) return {};
  try {
    return JSON.parse(text);
  } catch {
    if (response.ok) {
      throw new SrcApiError(
        `SRC 响应不是合法 JSON（HTTP ${response.status}）`,
        "src.protocol_invalid",
        response.status,
      );
    }
    return {};
  }
}

async function expect<T>(input: Promise<Response> | Response): Promise<T> {
  const response = await input;
  const data = await parse(response) as {
    error?: { code?: string; message?: string; detail?: Record<string, unknown> } | null;
  };
  if (!response.ok || data.error) {
    throw new SrcApiError(
      data.error?.message || `SRC 请求失败（HTTP ${response.status}）`,
      data.error?.code || "src.request_failed", response.status, data.error?.detail,
    );
  }
  return data as T;
}

async function expectReceipt<T>(input: Promise<Response> | Response): Promise<SrcCommandReceipt<T>> {
  const response = await input;
  const receipt = await parse(response) as SrcCommandReceipt<T>;
  if (!response.ok || receipt.error) {
    throw new SrcApiError(
      receipt.error?.message || `SRC 命令失败（HTTP ${response.status}）`,
      receipt.error?.code || "src.command_failed", response.status, receipt.error?.detail,
    );
  }
  return receipt;
}

const taskPath = (taskId: string) => `/api/src/tasks/${encodeURIComponent(taskId)}`;
const commandPath = (taskId: string, type: string) => `${taskPath(taskId)}/commands/${encodeURIComponent(type)}`;

export const fetchSrcTask = (taskId: string) => expect<SrcTask>(apiFetch(taskPath(taskId)));
const taskListCache = createReadCache<SrcTask[]>(30_000, 1);
export const srcTasksSnapshot = () => taskListCache.snapshot("tasks");
export const listSrcTasks = (refresh = false) => taskListCache.read("tasks", () => expect<SrcTask[]>(apiFetch("/api/src/tasks")), refresh);
export const prefetchSrcTasks = () => listSrcTasks();
export const listSrcRuns = (taskId: string) => expect<SrcRun[]>(apiFetch(`${taskPath(taskId)}/runs`));
export const fetchSrcPolicy = (taskId: string) => expect<ExecutionPolicy>(apiFetch(`${taskPath(taskId)}/policy`));
export const fetchSrcCapabilities = (taskId: string) => expect<ExecutionCapabilities>(apiFetch(`${taskPath(taskId)}/capabilities`));
export const fetchExecutionReceipt = (taskId: string, invocationId: string) => expect<ExecutionReceipt>(apiFetch(`${taskPath(taskId)}/executions/${encodeURIComponent(invocationId)}`));

type SrcRecordPage<T> = {
  records: SrcRecord<T>[]; total: number; returned: number;
  next_cursor: string | null; complete: boolean; partial_reason?: string | null;
  watermarks: unknown[]; snapshot: { event_watermark: number };
};

export async function listSrcRecordPage<T = Record<string, unknown>>(taskId: string, kind: string, parentId?: string, limit = 500, cursor = "") {
  const params = new URLSearchParams({ kind, limit: String(limit) });
  if (parentId !== undefined) params.set("parent_id", parentId);
  if (cursor) params.set("cursor", cursor);
  const page = await expect<SrcRecordPage<T> | SrcRecord<T>[]>(apiFetch(`${taskPath(taskId)}/records?${params}`));
  return Array.isArray(page) ? { records: page, total: page.length, returned: page.length, next_cursor: null, complete: false, partial_reason: "missing_page_envelope", watermarks: [], snapshot: { event_watermark: 0 } } : page;
}

export async function listSrcRecords<T = Record<string, unknown>>(taskId: string, kind: string, parentId?: string, limit = 500) {
  const page = await listSrcRecordPage<T>(taskId, kind, parentId, limit);
  return page.records;
}

export function getSrcRecord<T = Record<string, unknown>>(taskId: string, kind: string, recordId: string) {
  return expect<SrcRecord<T>>(apiFetch(`${taskPath(taskId)}/records/${encodeURIComponent(kind)}/${encodeURIComponent(recordId)}`));
}

export function ensureSrcRun(taskId: string, input: {
  target_url: string; question: string; binding_key?: string; binding_revision?: number;
  plan_ref?: string; authorization_ref?: string; evidence_refs?: string[];
  identity?: Record<string, unknown>; object_state?: Record<string, unknown>;
  budget?: Record<string, unknown>; target_version?: string; source_message_id?: string;
}) {
  return expectReceipt<SrcRun>(apiFetch(`${taskPath(taskId)}/runs`, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ ...input, idempotency_key: idem("src-run") }),
  }));
}

export function startSrcRun(taskId: string, runId: string, input: { target_url: string; goal?: string; prompt?: string; scope?: string }) {
  return expectReceipt(apiFetch(`${taskPath(taskId)}/runs/${encodeURIComponent(runId)}/start`, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ ...input, idempotency_key: idem("src-start") }),
  }));
}

export async function resolveSrcRun(taskId: string, runId: string, description?: string, generations?: { executionGeneration: number; controlGeneration: number }) {
  const response = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/resolve?src_task_id=${encodeURIComponent(taskId)}`, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ command_id: crypto.randomUUID(), expected_execution_generation: generations?.executionGeneration, expected_generation: generations?.controlGeneration, ...(description?.trim() ? { guidance: { action: "hint", text: description.trim() } } : {}) }),
  });
  const data = await response.json();
  if (!response.ok || data?.ok === false) throw new Error(data?.error?.message || data?.detail || `resolve failed (${response.status})`);
  return data;
}

export function previewSrcPolicy(taskId: string, request: Record<string, unknown>) {
  return expect<PolicyPreview>(apiFetch(`${taskPath(taskId)}/policy/preview`, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(request),
  }));
}

export function updateSrcPolicy(taskId: string, input: Partial<ExecutionPolicy> & { base_version?: number }) {
  return expectReceipt<ExecutionPolicy>(apiFetch(commandPath(taskId, "src.policy.update"), {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ ...input, idempotency_key: idem("src-policy") }),
  }));
}

export function decideSrcApproval(taskId: string, approval: SrcRecord<ExecutionApproval>, decision: "approve_once" | "deny" | "approve_task_scope", scope?: {
  resource_selector?: Record<string, unknown>; actions?: string[];
  identity_constraints?: Record<string, unknown>; environment_constraints?: Record<string, unknown>;
  input_constraints?: Record<string, unknown>;
}) {
  return expectReceipt<ExecutionApproval>(apiFetch(commandPath(taskId, "src.approval.decide"), {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({
      approval_id: approval.record_id, decision, base_revision: approval.revision,
      scope, idempotency_key: idem("src-approval"),
    }),
  }));
}

export function revokeSrcGrant(taskId: string, grant: SrcRecord<TaskExecutionGrant>) {
  return expectReceipt<{ grant_id: string; revoked: boolean }>(apiFetch(commandPath(taskId, "src.grant.revoke"), {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({
      grant_id: grant.record_id, base_revision: grant.revision, idempotency_key: idem("src-revoke"),
    }),
  }));
}

export async function createSrcTask(input: {
  title: string;
  goal: string;
  authorization?: {
    targets?: string[];
    excluded_targets?: string[];
    notes?: string;
    scope_fuse_override_reason?: string;
  };
  project_id?: string;
  runtime?: {
    adapter_id: string;
    instance_id: string;
    credential_id?: string;
    credential_ref?: string;
    model?: string;
    effort?: string;
    service_tier?: string;
    access_mode?: string;
    interaction_mode?: string;
  };
}, options: { idempotencyKey?: string; signal?: AbortSignal } = {}) {
  const receipt = await expectReceipt<{ task_id: string; thread_id: string }>(apiFetch("/api/src/tasks", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, idempotency_key: options.idempotencyKey || idem("src-create") }),
    signal: options.signal,
  }));
  taskListCache.invalidate("tasks");
  return receipt;
}

export async function renewSrcAuthorization(
  taskId: string,
  input: {
    base_policy_version: number;
    targets: string[];
    excluded_targets?: string[];
    exclusions?: Record<string, unknown>[];
    notes?: string;
    raw_policy_text?: string;
    diff_summary?: string;
    scope_fuse_override_reason?: string;
  },
) {
  const receipt = await expectReceipt<{
    policy_version: number;
    policy_digest: string;
    snapshot_id: string;
    authorized_targets: string[];
    excluded_targets: string[];
    added_targets: string[];
    removed_targets: string[];
  }>(apiFetch(commandPath(taskId, "src.task.authorization.renew"), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, idempotency_key: idem("src-auth-renew") }),
  }));
  taskListCache.invalidate("tasks");
  return receipt;
}

export async function lifecycleSrcTask(
  taskId: string,
  action: "archive" | "restore" | "pause" | "resume" | "destroy",
) {
  const receipt = await expectReceipt<Record<string, unknown>>(apiFetch(commandPath(taskId, `src.task.${action}`), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(
      action === "pause" || action === "resume" ? { idempotency_key: idem(`src-task-${action}`) } : {},
    ),
  }));
  taskListCache.invalidate("tasks");
  return receipt;
}

export async function takeoverHistoricalSrcTask(
  histTaskId: string,
  input: {
    title?: string;
    goal?: string;
    project_id?: string;
    targets: string[];
    excluded_targets?: string[];
    notes?: string;
    scope_fuse_override_reason?: string;
  },
) {
  const res = await expect<{
    task_id: string;
    thread_id: string;
    title: string;
    project_id?: string | null;
    source_historical_task_id: string;
    authorized_targets: string[];
  }>(apiFetch(`${taskPath(histTaskId)}/takeover`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, idempotency_key: idem("src-takeover") }),
  }));
  taskListCache.invalidate("tasks");
  return res;
}

export function controlSrcRun(taskId: string, runId: string, action: "pause" | "resume" | "stop") {
  return expect(apiFetch(`/api/runs/${encodeURIComponent(runId)}/control?src_task_id=${encodeURIComponent(taskId)}`, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ target: "coordinator", action, text: "" }),
  }));
}

const queryPath = (taskId: string, type: string) => `${taskPath(taskId)}/queries/${encodeURIComponent(type)}`;

export const fetchWorkbenchSnapshot = (taskId: string) =>
  expect<WorkbenchSnapshot>(apiFetch(queryPath(taskId, "src.workbench.snapshot"), {
    method: "POST", headers: { "Content-Type": "application/json" }, body: "{}",
  }));

export const fetchWorkbenchSlice = (taskId: string, slice: string) =>
  expect<WorkbenchSlicePayload>(apiFetch(queryPath(taskId, "src.workbench.slice.get"), {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ slice }),
  }));

export function saveWorkbenchSlice(
  taskId: string, slice: string, data: Record<string, unknown>, baseRevision: number, clientId: string,
) {
  return expectReceipt<{ result: WorkbenchSliceSaveResult }>(apiFetch(commandPath(taskId, "src.workbench.slice.save"), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ slice, data, base_revision: baseRevision, client_id: clientId, idempotency_key: idem("src-wb") }),
  }));
}

export async function downloadSrcDebugLogs(
  taskId: string,
  options: { signal?: AbortSignal } = {},
): Promise<{ filename: string; sizeBytes: number; metadata: SrcDebugLogsMetadata | null }> {
  if (taskId.startsWith("src-demo-")) {
    throw new SrcApiError("演示模式不包含真实本地日志，无法打包。", "src.demo_read_only", 409);
  }
  const response = await apiFetch(`${taskPath(taskId)}/debug-logs`, { signal: options.signal });
  if (!response.ok) {
    let body: { error?: { code?: string; message?: string; detail?: Record<string, unknown> } } = {};
    try {
      const text = await response.text();
      if (text) body = JSON.parse(text) as typeof body;
    } catch {
      body = {};
    }
    throw new SrcApiError(
      body.error?.message || `打包 SRC 日志失败（HTTP ${response.status}）`,
      body.error?.code || "src.debug_logs_failed",
      response.status,
      body.error?.detail,
    );
  }
  const blob = await response.blob();
  if (!blob.size) {
    throw new SrcApiError("导出的日志包为空，未保存。", "src.debug_logs_empty", response.status);
  }
  const filename = srcDebugLogsFilename({
    contentDisposition: response.headers.get("Content-Disposition"),
    taskId,
    date: new Date(),
  });
  const metadata = parseSrcDebugLogsMetadata(response.headers.get("X-Muteki-Export-Metadata"));
  const objectUrl = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = objectUrl;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  window.setTimeout(() => URL.revokeObjectURL(objectUrl), 60_000);
  return { filename, sizeBytes: blob.size, metadata };
}
