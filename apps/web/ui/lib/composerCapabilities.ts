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
export type ComposerCapabilityKind = "command" | "skill" | "mcp" | "plugin" | "file" | "thread" | "message_span" | "tool_excerpt";

export interface ComposerCapabilityContext {
  revision?: number;
  threadId?: string;
  adapterId: string;
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

export async function fetchComposerCapabilities(
  context: ComposerCapabilityContext,
  trigger: ComposerTrigger,
  query: string,
  signal?: AbortSignal,
): Promise<{ engine: string; items: ComposerCapabilityItem[]; runtime: ComposerRuntimeState; sectionErrors: ComposerCapabilitySectionError[] }> {
  const response = await apiFetch("/api/conversation/composer-capabilities", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      thread_id: context.threadId || "",
      adapter_id: context.adapterId,
      workspace_id: context.workspaceId || "",
      project_id: context.projectId || "",
      trigger,
      query,
    }),
    signal,
  });
  const rawBody = await response.text();
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
  return {
    engine: body.engine || "",
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
}
