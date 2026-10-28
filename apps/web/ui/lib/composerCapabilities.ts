"use client";

import type {
  ComposerContextLocator,
  ComposerContextSnapshot,
  ComposerContextStatus,
} from "./composerContextDoc";
import { apiFetch } from "./useRun";

export type ComposerTrigger = "/" | "@" | "$";
export interface ComposerCapabilitySectionError {
  section: string;
  code?: string;
  message: string;
  [key: string]: unknown;
}
export type ComposerCapabilityKind = "command" | "skill" | "mcp" | "plugin" | "component" | "file" | "thread" | "message_span" | "tool_excerpt";

export interface ComposerCapabilityContext {
  revision?: number;
  threadId?: string;
  adapterId: string;
  instanceId?: string;
  workspaceId?: string;
  projectId?: string;
}

export interface ComposerCapabilityItem {
  id: string;
  kind: ComposerCapabilityKind;
  name: string;
  description: string;
  source: string;
  scope: string;
  engine?: string;
  action?: string;
  channel?: string;
  origin?: string;
  delivery?: string;
  verification?: string;
  status?: string;
  argument_hint?: string;
  invocation?: Record<string, unknown> & { wire_text?: string };
  support_level?: string;
  reason?: string;
  alternative?: string;
  invocable?: boolean;
  revision?: number;
  /** Built-in Muteki tool (computer-use / browser / visualize / orchestrate). */
  component?: string;
  /** Target macOS app for a computer-use item, resolved by the server. */
  target?: { type: "mac_app"; bundle_id: string; name: string; path: string };
}

export interface ComposerRuntimeState {
  adapter_id: string;
  revision: number;
  stale: boolean;
  diagnostics: string[];
  /** #188: missing | refreshing | failed | stale | fresh */
  refresh_status?: string;
  last_error?: string;
  refresh_attempts?: number;
  retry_after_seconds?: number;
  matrix?: {
    adapter_id?: string;
    instance_id?: string;
    revision?: number;
    stale?: boolean;
    rows?: Array<Record<string, unknown>>;
    diagnostics?: string[];
  } | null;
}

/**
 * Capability chip / structured context node payload.
 *
 * Legacy chips are the Pick fields only. C09 schema-v2 nodes also carry
 * node_id, locator, snapshot, and status (see composerContextDoc).
 */
export type ComposerCapabilityRef = Omit<
  Pick<ComposerCapabilityItem, "id" | "kind" | "name" | "description" | "source" | "scope">,
  "kind"
> & {
  node_id?: string;
  // Omit+widen: Pick alone intersects kind back to ComposerCapabilityKind and
  // rejects C09 message_span / tool_excerpt (blocks Next production build).
  kind: ComposerCapabilityItem["kind"] | "message_span" | "tool_excerpt";
  locator?: ComposerContextLocator;
  snapshot?: ComposerContextSnapshot;
  status?: ComposerContextStatus;
  status_reason?: string;
  legacy_capability_id?: string;
  context_schema?: 2;
};

const CAPABILITY_REQUEST_TIMEOUT_MS = 20_000;

export class ComposerCapabilityError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly rawBody: string,
    readonly details: Record<string, unknown> = {},
    readonly correlationId: string = "",
  ) {
    super(message);
    this.name = "ComposerCapabilityError";
  }

  get diagnostic(): string {
    return JSON.stringify({ status: this.status, correlationId: this.correlationId, ...this.details }, null, 2)
      + "\n\n完整响应正文：\n" + this.rawBody;
  }
}

export interface ComposerCapabilityResult {
  engine: string;
  items: ComposerCapabilityItem[];
  runtime: ComposerRuntimeState;
  sectionErrors: ComposerCapabilitySectionError[];
  accessModes: string[];
  accessModeNotes: Record<string, string>;
}

// Last successful snapshot per context/trigger/query, shown immediately while the
// menu revalidates so reopening `/` does not flash an empty skeleton.
const SNAPSHOT_CACHE_LIMIT = 64;
const snapshotCache = new Map<string, ComposerCapabilityResult>();

function snapshotKey(context: ComposerCapabilityContext, trigger: ComposerTrigger, query: string): string {
  return JSON.stringify([
    context.adapterId, context.instanceId || "", context.threadId || "", context.workspaceId || "",
    context.projectId || "", context.revision ?? null, trigger, query,
  ]);
}

export function cachedComposerCapabilities(
  context: ComposerCapabilityContext,
  trigger: ComposerTrigger,
  query: string,
): ComposerCapabilityResult | undefined {
  return snapshotCache.get(snapshotKey(context, trigger, query));
}

export function clearComposerCapabilityCache(): void {
  snapshotCache.clear();
}

function rememberSnapshot(key: string, result: ComposerCapabilityResult): void {
  snapshotCache.delete(key);
  snapshotCache.set(key, result);
  while (snapshotCache.size > SNAPSHOT_CACHE_LIMIT) {
    const oldest = snapshotCache.keys().next().value;
    if (oldest === undefined) break;
    snapshotCache.delete(oldest);
  }
}

export async function fetchComposerCapabilities(
  context: ComposerCapabilityContext,
  trigger: ComposerTrigger,
  query: string,
  signal?: AbortSignal,
): Promise<ComposerCapabilityResult> {
  // Without a deadline an unresponsive backend leaves the menu on its loading
  // skeleton forever; the typed timeout error gives the menu its retry action.
  const deadline = AbortSignal.timeout(CAPABILITY_REQUEST_TIMEOUT_MS);
  const requestSignal = signal ? AbortSignal.any([signal, deadline]) : deadline;
  let response: Response;
  let rawBody: string;
  try {
    response = await apiFetch("/api/conversation/composer-capabilities", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        thread_id: context.threadId || "",
        adapter_id: context.adapterId,
        instance_id: context.instanceId || "",
        workspace_id: context.workspaceId || "",
        project_id: context.projectId || "",
        trigger,
        query,
      }),
      signal: requestSignal,
    });
    rawBody = await response.text();
  } catch (cause) {
    if (deadline.aborted && !signal?.aborted) {
      throw new ComposerCapabilityError(
        `能力目录超过 ${CAPABILITY_REQUEST_TIMEOUT_MS / 1000} 秒没有响应；本机后端可能正忙或已停止，请重试。`,
        0, "", { code: "capability_request_timeout", timeout_ms: CAPABILITY_REQUEST_TIMEOUT_MS },
      );
    }
    throw cause;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(rawBody);
  } catch {
    throw new ComposerCapabilityError(
      `能力目录返回无法解析的响应（HTTP ${response.status}）`, response.status, rawBody,
      { code: "capability_response_invalid_json" }, response.headers.get("x-request-id") || "",
    );
  }
  const body = (parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {}) as {
    engine?: string;
    access_modes?: string[];
    access_mode_notes?: Record<string, string>;
    items?: ComposerCapabilityItem[];
    runtime?: Partial<ComposerRuntimeState>;
    error?: { message?: string; code?: string; [key: string]: unknown };
    section_errors?: ComposerCapabilitySectionError[];
  };
  if (!response.ok) {
    throw new ComposerCapabilityError(
      body.error?.message || body.error?.code || `能力目录加载失败（HTTP ${response.status}）`,
      response.status, rawBody, body.error || {}, response.headers.get("x-request-id") || "",
    );
  }
  if (!Array.isArray(body.items)) {
    throw new ComposerCapabilityError(
      "能力目录响应缺少候选列表", response.status, rawBody,
      { code: "capability_response_invalid_shape" }, response.headers.get("x-request-id") || "",
    );
  }
  const result: ComposerCapabilityResult = {
    engine: body.engine || "",
    accessModes: Array.isArray(body.access_modes) ? body.access_modes.map(String) : [],
    accessModeNotes: body.access_mode_notes || {},
    items: Array.isArray(body.items) ? body.items : [],
    sectionErrors: Array.isArray(body.section_errors) ? body.section_errors.filter((error) =>
      error && typeof error.section === "string" && typeof error.message === "string") : [],
    runtime: {
      adapter_id: String(body.runtime?.adapter_id || context.adapterId),
      revision: Number(body.runtime?.revision || 0),
      stale: Boolean(body.runtime?.stale),
      diagnostics: Array.isArray(body.runtime?.diagnostics)
        ? body.runtime.diagnostics.map(String)
        : [],
      refresh_status: body.runtime?.refresh_status
        ? String(body.runtime.refresh_status)
        : undefined,
      last_error: body.runtime?.last_error
        ? String(body.runtime.last_error)
        : undefined,
      refresh_attempts: Number(body.runtime?.refresh_attempts || 0),
      retry_after_seconds: Number(body.runtime?.retry_after_seconds || 0),
      matrix: body.runtime?.matrix ?? null,
    },
  };
  rememberSnapshot(snapshotKey(context, trigger, query), result);
  return result;
}
