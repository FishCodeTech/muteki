"use client";
import { readChatPreferences } from "./chatPreferences";

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";
import {
  rethrowNetworkLoadFailure,
  threadLoadFailureMessage,
  throwThreadLoadFailure,
} from "@/lib/threadLoadFailure";
import {
  acceptConversationStreamEvents,
  emptyConversationStreamState,
  type ConversationLiveTextRun,
  type ConversationStreamState,
} from "./conversationStreamReducer";
import {
  acceptConversationInboxEvent,
  applyConversationInboxPatch,
  emptyConversationInboxState,
  isInternalCtfThread,
  reconcileConversationThreads,
  sanitizeConversationInboxSnapshot,
  upsertConversationThread,
  type ConversationInboxPatch,
  type ConversationInboxSnapshot,
} from "./conversationInbox";
import { countThreadAttention } from "./threadAttention";
import { createNotificationInboxDrain, readNotificationInboxCursor, writeNotificationInboxCursor, recoverNotificationInboxSnapshot, emptyNotificationInboxCursor } from "./notificationInboxCursor";
import type { ConversationInboxEvent } from "./threadNotifications";
import { API, apiFetch } from "./useRun";
import { apiRequestUrl, currentAuthGeneration, currentAuthScope, currentServiceOrigin } from "./serviceAuth";
import { desktopChatBridge, type DesktopNativeState } from "./desktopChatBridge";
import { browserControlHost, handleBrowserRequest, rejectBrowserRequest } from "./browserControl";
import { recordTaskReceipt } from "./task-center";
import { conversationStorageScope, subscribeConversationStorageScope } from "./conversationStorageScope";
import { conversationCommandErrorFrom } from "./sendIntentStore";
import { descriptorForAdapter, isKnownEngine, loadProviderDescriptors } from "./providerDescriptors";

export type ThreadMode =
  | "conversation"
  | "src"
  | "single_task"
  | "competition"
  | "management";

export interface ThreadControlLifecycle {
  pinned?: boolean;
  settled?: boolean;
  settled_at?: string | null;
  settled_summary?: string | null;
  follow_up?: boolean;
  follow_up_note?: string | null;
  parked?: boolean;
  snoozed_until?: string | null;
  unread?: boolean;
}

export interface ConversationThread {
  thread_id: string;
  project_id?: string | null;
  workspace_id?: string | null;
  title: string;
  title_source?: "fallback" | "model" | "user" | string;
  summary?: string;
  metadata_revision?: number;
  metadata_turn_id?: string | null;
  mode: ThreadMode;
  created_at?: string;
  updated_at?: string;
  source_thread_id?: string | null;
  from_turn_id?: string | null;
  side_chat?: {
    parent_thread_id?: string;
    parent_run_id?: string;
    run_mode?: "ctf" | "pentest";
    cwd?: string | null;
    from_turn_id?: string | null;
    target_id?: string;
    target_kind?: "worker" | "agent";
    worker_id?: string | null;
    seat_id?: string | null;
    target_label?: string;
    target_engine?: string;
    target_model?: string;
    inherited_messages?: number;
    inherited_worker_events?: number;
    inherited_facts?: number;
    native_fork?: boolean;
    parent_running?: boolean;
    created_at?: string;
  } | null;
  ctf_run_id?: string | null;
  state: ConversationState;
  control_lifecycle?: ThreadControlLifecycle | null;
  runtime?: { adapter_id?: string | null; instance_id?: string | null; credential_id?: string | null; model?: string | null };
}

export interface ConversationProject {
  project_id: string;
  name: string;
  description?: string;
  scope?: "chat" | "src";
  workspace_id: string;
  workspace_kind: "local" | "unbound" | string | null;
  root_path: string;
  settings?: Record<string, unknown>;
  created_at?: string;
  updated_at?: string;
}

export interface ConversationDirectorySelection {
  path: string;
  name: string;
}

export interface ConversationScheduledTask {
  task_id: string;
  schedule?: string | null;
  recurring?: boolean | null;
  prompt?: string | null;
  origin_turn_id?: string | null;
  created_at?: string;
  updated_at?: string;
  engine?: string;
  unconfirmed?: "session_closed" | "server_restarted" | string | null;
}

export interface ConversationState {
  status: string;
  running_turn_id?: string | null;
  agent_session_id?: string | null;
  pending_approvals?: Record<string, Record<string, unknown>>;
  pending_approval?: Record<string, unknown> | null;
  pending_user_input?: Record<string, unknown> | null;
  plan?: ThreadPlanSnapshot | null;
  agents?: ThreadAgentTreeSnapshot | null;
  lineage?: ThreadLineage | null;
  subagents?: ConversationAgentNode[];
  usage?: Record<string, unknown>;
  last_error?: Record<string, unknown>;
  unread?: boolean;
  head_stream_seq?: number;
  attention_stream_seq?: number;
  read_stream_seq?: number;
  current_generation?: number;
  queue_count?: number;
  queue_paused?: boolean;
  queue_pause_reason?: string;
  queue_revision?: number;
  queue_failed_item_id?: string | null;
  queued_turn?: boolean;
  quota_resume?: { turn_id: string; resets_at: number; resume_at: number; kind?: string; scheduled_at?: number } | null;
  scheduled_tasks?: ConversationScheduledTask[];
  background_tasks?: number;
  background_task_list?: Array<{
    id: string;
    name: string;
    kind?: string;
    item_id?: string | null;
    started_at?: number;
  }>;
  quota_hold?: { turn_id: string; resets_at: number; kind?: string; handled?: boolean } | null;
  goal?: {
    objective?: string;
    status?: string;
    tokens_used?: number;
    token_budget?: number;
    checks?: number;
    last_check?: string;
  } | null;
}

export interface ConversationMessage {
  message_id: string;
  turn_id?: string | null;
  role: "user" | "assistant" | "system" | "tool" | string;
  source_provider?: string;
  source_role?: string;
  source_content?: unknown;
  kind?: "message" | "steer" | string;
  status?: string;
  phase?: "commentary" | "final_answer" | "unknown";
  text: string;
  attachments?: string[];
  capability_refs?: Array<Record<string, unknown>>;
  stream_seq?: number;
  created_at?: string;
}

export interface ConversationMessagesPage {
  limit: number;
  oldest_stream_seq?: number | null;
  newest_stream_seq?: number | null;
  oldest_message_id?: string | null;
  newest_message_id?: string | null;
  has_more_before: boolean;
  has_more_after: boolean;
  around_message_id?: string | null;
}

export interface ConversationSearchHit {
  thread_id: string;
  message_id: string;
  turn_id?: string | null;
  role: string;
  kind?: string;
  stream_seq: number;
  thread_title: string;
  project_id?: string;
  archived: boolean;
  superseded: boolean;
  snippet: string;
}

export interface ConversationSearchResult {
  query: string;
  count: number;
  hits: ConversationSearchHit[];
  reason?: string;
  offset?: number; limit?: number; has_more?: boolean; next_offset?: number | null;
}

export interface ConversationTurn {
  turn_id: string;
  seq?: number;
  kind: string;
  origin_turn_id?: string | null;
  status: string;
  text: string;
  attachments?: string[];
  capability_refs?: Array<Record<string, unknown>>;
  runtime_snapshot?: { adapter_id?: string; instance_id?: string; credential_id?: string; model?: string; effort?: string; service_tier?: string; access_mode?: string; interaction_mode?: string } | null;
  interaction_mode?: string;
  thread_disposition?: string;
  retry_of_turn_id?: string | null;
  error?: Record<string, unknown>;
  usage?: Record<string, unknown>;
  context_delivery?: TurnContextPart[];
  has_context_delivery?: boolean;
  process_recorded?: boolean;
  historical?: boolean;
  created_at?: string;
  completed_at?: string;
}

export interface TurnContextPart {
  key: string;
  kind: string;
  trust: "application" | "untrusted" | string;
  scope: "session" | "turn" | string;
  channel: string;
  artifact_id?: string;
  sha256?: string;
  chars: number;
  text?: string;
  name?: string;
  media_type?: string;
  size?: number;
  source_turn_id?: string;
}

export interface TurnContextLedger {
  thread_id: string;
  turn_id: string;
  adapter_id?: string;
  delivery_status?: "request_prepared" | "request_sent";
  availability?: "recorded" | "metadata_only" | "absent";
  user_text: string;
  parts: TurnContextPart[];
}

export interface ConversationQueueItem {
  not_before?: number | null;
  queue_id: string;
  thread_id: string;
  client_message_id?: string;
  text: string;
  attachments: string[];
  capability_refs: Array<Record<string, unknown>>;
  runtime?: Record<string, unknown>;
  position: number;
  status: "queued" | "dispatching" | "failed" | string;
  error?: Record<string, unknown>;
  created_at?: string;
  updated_at?: string;
}

export type ThreadPlanPhase =
  | "proposed"
  | "executing"
  | "awaiting_decision"
  | "completed"
  | "cleared"
  | "unsupported";

export type PlanTaskStatus =
  | "pending"
  | "in_progress"
  | "completed"
  | "blocked"
  | "cancelled";

export interface PlanTaskEvidence {
  kind: string;
  id: string;
  turn_id?: string | null;
}

export interface PlanTask {
  task_id: string;
  title: string;
  status: PlanTaskStatus | string;
  blocked_reason?: string | null;
  evidence?: PlanTaskEvidence[];
  updated_at?: string;
}

export interface ThreadPlanSnapshot {
  revision: number;
  phase: ThreadPlanPhase | string;
  source?: string;
  adapter_id?: string | null;
  agent_session_id?: string | null;
  turn_id?: string | null;
  title?: string | null;
  tasks: PlanTask[];
  awaiting?: Record<string, unknown> | null;
  pending_amendment?: Record<string, unknown> | null;
  last_change_summary?: string | null;
  unsupported_reason?: string | null;
  markdown?: string | null;
  updated_at?: string;
}

export interface ConversationAgentNode {
  execution?: string;
  report?: "none" | "expected" | "reported" | "unreported" | "failed";
  delivery?: "running" | "awaiting_settle" | "settled" | "not_delivered";
  agent_id: string;
  parent_id?: string | null;
  title: string;
  nickname?: string | null;
  role?: string | null;
  model?: string | null;
  turn_id?: string | null;
  message_id?: string | null;
  call_id?: string | null;
  session_ref?: string | null;
  status: string;
  request?: string | null;
  result?: string | null;
  error?: string | null;
  activity?: string | null;
  tool_uses?: number | null;
  total_tokens?: number | null;
  duration_ms?: number | null;
  started_at?: string | null;
  completed_at?: string | null;
  updated_at?: string;
  origin?: "provider_native" | "app_owned" | string;
  thread_id?: string | null;
  child_turn_id?: string | null;
  depth?: number | null;
  adapter_id?: string | null;
  access_mode?: string | null;
  effort?: string | null;
  acceptance_criteria?: string | null;
  routing_reason?: string | null;
  routing?: {
    source?: string;
    workbench_mode?: string | null;
    orchestration_scope?: string | null;
    role?: string | null;
    provider?: string | null;
    adapter_id?: string | null;
    instance_id?: string | null;
    model?: string | null;
    effort?: string | null;
    requested_fields?: string[];
    inherited_fields?: string[];
    resolved?: {
      adapter_id?: string | null;
      instance_id?: string | null;
      profile?: string | null;
      model?: string | null;
      effort?: string | null;
    } | null;
  } | null;
  todo_id?: string | null;
  delegation_profile?: string | null;
  isolation?: string | null;
  workspace_id?: string | null;
  worktree_path?: string | null;
  worktree_state?: string | null;
  spawn_state?: string | null;
  cancel_requested?: boolean;
  cancel_supported?: boolean;
  cancel_reason?: string | null;
  error_code?: string | null;
  principal_id?: string | null;
}

export interface ThreadLineage {
  native_subagent?: string;
  origin?: string;
  subagent_id?: string;
  parent_thread_id: string;
  parent_turn_id?: string | null;
  root_thread_id: string;
  depth: number;
  isolation?: string;
  workspace_id?: string | null;
  worktree_path?: string | null;
  spawned_at?: string;
}

export interface ThreadAgentTreeSnapshot {
  revision: number;
  source?: string;
  adapter_id?: string | null;
  turn_id?: string | null;
  agents: ConversationAgentNode[];
  unsupported?: boolean;
  unsupported_reason?: string | null;
  tool_activity_summary?: string | null;
  updated_at?: string;
}

export interface ConversationStatistics {
  latest_turn?: {
    input_tokens?: number | null;
    output_tokens?: number | null;
    cache_read_tokens?: number | null;
    reasoning_tokens?: number | null;
    ttft_ms?: number;
    total_duration_ms?: number;
    end_to_end_tokens_per_second?: number;
  };
  turn_count: number;
  step_count: number;
  total_duration_ms?: number;
  llm_duration_ms?: number;
  tool_duration_ms?: number;
  llm_duration_source?: "reported" | "residual_estimate";
  tool_duration_source?: "reported" | "lifecycle" | "mixed";
  tool_wall_duration_ms?: number;
  average_ttft_ms?: number;
  tokens_per_second?: number;
  cache_hit_rate?: number;
  input_tokens?: number;
  output_tokens?: number;
  token_coverage?: "missing" | "partial" | "complete";
}

export interface ContextWindowState {
  total: number | null;
  limit: number | null;
  zones: Array<{ label: string; tokens: number }>;
  compacted: Array<{
    turn_id?: string;
    status?: "completed" | "failed" | "interrupted";
    trigger?: "manual" | "auto";
    confirmed?: boolean;
    summary?: string;
    message?: string;
    tokens_before?: number;
    tokens_after?: number;
    occurred_at?: string;
  }>;
  compact_status: "idle" | "running" | "done" | "failed" | "interrupted";
  updated_at?: string;
  source?: string;
}

export interface ConversationView {
  thread: Omit<ConversationThread, "state">;
  lineage?: (ThreadLineage & { parent_title?: string | null }) | null;
  workspace?: {
    workspace_id: string;
    project_id?: string | null;
    kind: "local" | "git" | "isolated" | string;
    root_path: string;
    settings?: Record<string, unknown>;
    created_at?: string;
  } | null;
  runtime: {
    adapter_id: string;
    instance_id: string;
    credential_id?: string;
    credential_ref?: string;
    model?: string;
    effort?: string;
    service_tier?: string;
    access_mode?: string;
    interaction_mode?: string;
    permission_mode?: string;
    sandbox_mode?: string;
    updated_at?: string;
  };
  state: ConversationState;
  binding?: {
    binding_id: string;
    binding_version: number;
    mode: ThreadMode;
    tool_set: string[];
    revoked_at?: string | null;
  } | null;
  grants?: Array<{
    grant_id: string;
    injection_kind: string;
    expires_at?: string | null;
    revoked_at?: string | null;
  }>;
  agent_session?: {
    agent_session_id: string;
    external_session_id?: string | null;
    runtime_instance_id?: string | null;
  } | null;
  runtime_connection?: {
    configured: boolean;
    connected: boolean;
    session_state?: string;
    injection_kind?: string;
    binding_version?: number | null;
    grant_expires_at?: string | null;
    gateway_endpoint?: string;
    degradation?: string;
    capabilities?: Record<string, unknown>;
    capability_revision?: number;
    capability_stale?: boolean;
    matrix?: {
      adapter_id?: string;
      instance_id?: string;
      revision?: number;
      stale?: boolean;
      rows?: Array<{
        key: string;
        level: string;
        reason?: string;
        alternative?: string;
        source?: string;
        invocable?: boolean;
      }>;
      diagnostics?: string[];
    } | null;
    matrix_diagnostics?: string[];
    capability_refresh_status?: string;
    capability_last_error?: string;
    capability_refresh_attempts?: number;
    capability_retry_after_seconds?: number;
    background_turn_id?: string | null;
  };
  rewind_capability?: {
    rewind_level?: string;
    invocable?: boolean;
    reason?: string;
    alternative?: string;
    source?: string;
    revision?: number;
    stale?: boolean;
  };
  artifacts?: Array<{
    artifact_id?: string;
    sha256?: string;
    needs_reselect?: boolean;
    name?: string;
    kind?: string;
    media_type?: string;
    size?: number;
    turn_id?: string;
    created_at?: string;
  }>;
  messages: ConversationMessage[];
  messages_page?: ConversationMessagesPage;
  turns: ConversationTurn[];
  tools?: Array<{ tool_id: string; turn_id: string; name: string; status: string }>;
  superseded_turns?: ConversationTurn[];
  superseded_messages?: ConversationMessage[];
  queue: ConversationQueueItem[];
  statistics?: ConversationStatistics;
  context_window?: ContextWindowState | null;
  watermark: number;
  event_watermark: number;
}

export interface RuntimeAuthView {
  code?: string;
  status?: string;
  detail?: string;
  login_command?: string;
  note?: string;
  account_id?: string;
}

export interface RuntimeHealth {
  healthy?: boolean;
  capabilities?: Record<string, unknown>;
  detail?: string;
  auth?: RuntimeAuthView;
  binary_path?: string;
  degradations?: string[];
  source?: string;
  probed_at?: string;
  version_check?: {
    installed_version?: string;
    baseline_version?: string;
    min_supported_version?: string;
    meets_min_version?: boolean;
    below_minimum?: boolean;
    policy_enforced?: boolean;
    policy_modes?: { ctf?: boolean; pentest?: boolean; src?: boolean };
    upgrade_command?: string;
    upgrade_note?: string;
  };
}

export interface RuntimeInstance {
  key: string;
  origin?: string;
  adapter_id: string;
  instance_id: string;
  engine?: string;
  transport_kind?: "cli" | "structured";
  default_for_engine?: boolean;
  label?: string;
  models?: string[];
  default_model?: string;
  enabled?: boolean;
  configured?: boolean;
  access_modes?: string[];
  binary_path?: string;
  health?: RuntimeHealth;
  auth?: RuntimeAuthView;
}

export interface ConversationCredentialModel {
  id: string;
  label: string;
  variant?: {
    family_id: string;
    family_label: string;
    parameters: Record<string, string>;
    default?: boolean;
  };
  reasoning?: {
    supported?: boolean;
    levels?: string[];
    default?: string;
    kind?: string;
    source?: string;
  };
  service_tiers?: ConversationServiceTier[];
  default_service_tier?: string;
}

export interface ConversationServiceTier {
  id: string;
  name: string;
  description: string;
}


export interface ConversationCredential {
  id: string;
  label: string;
  engine: string;
  runtime_instance?: string;
  model_catalogs?: Record<string, ConversationCredentialModel[]>;
  verified_models_by_runtime?: Record<string, string[]>;
  source: "stored" | "system";
  account_id?: string;
  connection?: string;
  provider?: string;
  base_url?: string;
  present: boolean | null;
  detection_status?: "present" | "absent" | "unknown" | "disabled";
  status: string;
  status_detail?: string;
  discovery_code?: string;
  models: ConversationCredentialModel[];
  candidate_models: ConversationCredentialModel[];
  default_model?: string;
  last_test?: Record<string, unknown> | null;
  usage?: Array<Record<string, unknown>>;
}

export interface ConversationEvent {
  event_type: string;
  payload: Record<string, unknown>;
  seq: number;
  stream_seq?: number;
  occurred_at?: string;
  event_id?: string;
}

function carriesContextWindow(event: ConversationEvent): boolean {
  if (event.event_type !== "core.usage.updated") return false;
  const usage = event.payload?.usage as Record<string, unknown> | undefined;
  return usage?.context_used != null || usage?.model_context_window != null;
}

export interface CommandReceiptView {
  command_id: string;
  state: string;
  aggregate?: { type?: string; id?: string };
  run_id?: string | null;
  event_cursor?: string | null;
  output?: Record<string, unknown>;
  deduplicated?: boolean;
  error?: {
    code?: string;
    message?: string;
    category?: string;
    recovery_hint?: string;
    retryable?: boolean;
    correlation_id?: string;
    detail?: Record<string, unknown>;
  } | null;
}

export interface ConversationMemory {
  memory_id: string;
  thread_id: string;
  kind: string;
  content: string;
  source: {
    actor?: string;
    thread_id?: string;
    via?: string;
  };
  created_seq: number;
  updated_seq: number;
  created_at: string;
  updated_at: string;
}

export interface ConversationMemorySnapshot {
  memories: ConversationMemory[];
  tombstones?: Array<{ memory_id: string; deleted_seq: number; deleted_at: string; reason: string }>;
  deleted_count: number;
  watermark: number;
  thread_id: string;
  scope: string;
  query: string;
}

function commandId(): string {
  return `cmd_chat_${Date.now().toString(36)}_${Math.random()
    .toString(36)
    .slice(2, 9)}`;
}

export type CommandIdOptions = {
  commandId?: string;
  idempotencyKey?: string;
  signal?: AbortSignal;
};

function requestSignal(signal?: AbortSignal, timeoutMs = 15_000): AbortSignal {
  const timeout = AbortSignal.timeout(Math.max(1, timeoutMs));
  return signal ? AbortSignal.any([signal, timeout]) : timeout;
}

const RECEIPT_TERMINAL_STATES = new Set([
  "completed",
  "failed",
  "conflict",
  "cancelled",
]);
const receiptOwnerScopes = new WeakMap<CommandReceiptView, string>();

async function receiptOf(res: Response, ownerScope: string): Promise<CommandReceiptView> {
  const body = (await res.json().catch(() => ({}))) as {
    receipt?: CommandReceiptView;
    error?: CommandReceiptView["error"];
  };
  if (body.receipt) {
    receiptOwnerScopes.set(body.receipt, ownerScope);
    return body.receipt;
  }
  throw conversationCommandErrorFrom(
    body.error,
    `请求失败（HTTP ${res.status}）`,
    { httpStatus: res.status },
  );
}

export async function fetchCommandReceipt(
  commandId: string,
  options: { signal?: AbortSignal; timeoutMs?: number } = {},
): Promise<CommandReceiptView | null> {
  const ownerScope = conversationStorageScope();
  const id = String(commandId || "").trim();
  if (!id) return null;
  const res = await apiFetch(`/api/receipts/${encodeURIComponent(id)}`, {
    signal: requestSignal(options.signal, options.timeoutMs),
  });
  if (res.status === 404) return null;
  const raw = await res.text();
  let body: { receipt?: CommandReceiptView; error?: CommandReceiptView["error"] };
  try { body = JSON.parse(raw); }
  catch {
    throw conversationCommandErrorFrom({ code: "conversation.receipt.protocol_invalid",
      category: "protocol", message: "回执服务没有返回有效 JSON", retryable: false,
      detail: { response_body: raw, command_id: id } }, "回执格式无效", { httpStatus: res.status });
  }
  if (!res.ok) throw conversationCommandErrorFrom(body?.error,
    `查询回执失败（HTTP ${res.status}）`, { httpStatus: res.status });
  const receipt = body?.receipt;
  if (!receipt || receipt.command_id !== id || !["accepted", "running", "waiting", "processing", "unknown", ...RECEIPT_TERMINAL_STATES].includes(receipt.state)) {
    throw conversationCommandErrorFrom({ code: "conversation.receipt.protocol_invalid",
      category: "protocol", message: "回执响应缺少匹配的命令身份或状态", retryable: false,
      detail: { response_body: raw, command_id: id } }, "回执格式无效", { httpStatus: res.status });
  }
  receiptOwnerScopes.set(receipt, ownerScope);
  return receipt;
}

export async function recoverCommandReceipt(
  commandId: string,
  attempts = 8,
  delayMs = 200,
  options: { signal?: AbortSignal; ownerScope?: string } = {},
): Promise<CommandReceiptView | null> {
  const ownerScope = options.ownerScope ?? conversationStorageScope();
  const id = String(commandId || "").trim();
  if (!id) return null;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    options.signal?.throwIfAborted();
    assertReceiptOwnerScope(id, ownerScope);
    const receipt = await fetchCommandReceipt(id, options);
    if (receipt) return receipt;
    if (attempt + 1 < attempts) {
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }
  }
  return null;
}

function assertReceiptOwnerScope(commandId: string, ownerScope: string): void {
  if (conversationStorageScope() === ownerScope) return;
  throw conversationCommandErrorFrom({ code: "conversation.receipt.scope_changed", category: "state",
    message: "工作台身份已改变，请回到原工作台恢复这条命令的回执", retryable: true,
    detail: { command_id: commandId, owner_scope: ownerScope, outcome_unknown: true } }, "回执所属工作台已改变");
}

export async function waitForTerminalReceipt(
  initial: CommandReceiptView,
  label: string,
  options: { signal?: AbortSignal; ownerScope?: string } = {},
): Promise<CommandReceiptView> {
  const ownerScope = options.ownerScope ?? receiptOwnerScopes.get(initial) ?? conversationStorageScope();
  recordTaskReceipt(initial, label, "conversation", ownerScope);
  if (!initial.command_id || RECEIPT_TERMINAL_STATES.has(initial.state || "")) {
    return initial;
  }
  const deadline = Date.now() + 10_000;
  for (let attempt = 0; attempt < 40 && Date.now() < deadline; attempt += 1) {
    options.signal?.throwIfAborted();
    await new Promise((resolve) => setTimeout(resolve, 250));
    if (Date.now() >= deadline) break;
    assertReceiptOwnerScope(initial.command_id, ownerScope);
    const receipt = await fetchCommandReceipt(initial.command_id, {
      signal: options.signal, timeoutMs: deadline - Date.now(),
    });
    if (!receipt) continue;
    recordTaskReceipt(receipt, label, "conversation", ownerScope);
    if (RECEIPT_TERMINAL_STATES.has(receipt.state || "")) return receipt;
  }
  return initial;
}

export async function fetchConversationThreads(view?: "inbox"): Promise<ConversationThread[]> {
  const res = await apiFetch(view ? "/api/threads?view=inbox" : "/api/threads", { cache: "no-store", signal: requestSignal() });
  if (!res.ok) throw new Error(`加载对话失败（HTTP ${res.status}）`);
  const body = (await res.json()) as { threads?: ConversationThread[] };
  return (body.threads ?? []).filter((thread) => !isInternalCtfThread(thread));
}

function messageSeq(message: ConversationMessage): number {
  return typeof message.stream_seq === "number" ? message.stream_seq : 0;
}

function mergeMessageLists(
  existing: ConversationMessage[],
  incoming: ConversationMessage[],
): ConversationMessage[] {
  const byId = new Map<string, ConversationMessage>();
  for (const message of existing) {
    if (message.message_id) byId.set(message.message_id, message);
  }
  for (const message of incoming) {
    if (!message.message_id) continue;
    const prior = byId.get(message.message_id);
    if (!prior || messageSeq(message) >= messageSeq(prior)) {
      byId.set(message.message_id, message);
    }
  }
  return Array.from(byId.values()).sort((a, b) => {
    const seqDiff = messageSeq(a) - messageSeq(b);
    if (seqDiff !== 0) return seqDiff;
    return a.message_id.localeCompare(b.message_id);
  });
}

function mergeTurnLists(
  existing: ConversationTurn[],
  incoming: ConversationTurn[],
): ConversationTurn[] {
  const byId = new Map<string, ConversationTurn>();
  for (const turn of existing) byId.set(turn.turn_id, turn);
  for (const turn of incoming) byId.set(turn.turn_id, turn);
  return Array.from(byId.values()).sort((a, b) => (a.seq ?? 0) - (b.seq ?? 0));
}

export function collectSupersededTurnIds(
  turns: readonly ConversationTurn[],
  explicitlySuperseded: readonly ConversationTurn[] = [],
): Set<string> {
  const byId = new Map<string, ConversationTurn>();
  for (const turn of turns) {
    if (turn.turn_id) byId.set(turn.turn_id, turn);
  }
  const superseded = new Set<string>(
    explicitlySuperseded.map((turn) => turn.turn_id).filter(Boolean),
  );
  for (const turn of turns) {
    if (turn.status === "superseded" && turn.turn_id) superseded.add(turn.turn_id);
    let cursor = String(turn.retry_of_turn_id || "");
    const seen = new Set<string>();
    while (cursor && !seen.has(cursor)) {
      seen.add(cursor);
      superseded.add(cursor);
      const row = byId.get(cursor);
      cursor = row ? String(row.retry_of_turn_id || "") : "";
    }
  }
  return superseded;
}

export function omitSupersededBranchRows(
  messages: ConversationMessage[],
  turns: ConversationTurn[],
  superseded: Set<string>,
): { messages: ConversationMessage[]; turns: ConversationTurn[] } {
  if (superseded.size === 0) return { messages, turns };
  return {
    turns: turns.filter((turn) => !superseded.has(turn.turn_id)),
    messages: messages.filter((message) => {
      const turnId = String(message.turn_id || "");
      return !turnId || !superseded.has(turnId);
    }),
  };
}

export function pruneSupersededBranch(
  previousTurns: ConversationTurn[],
  previousMessages: ConversationMessage[],
  nextTurns: ConversationTurn[],
  nextMessages: ConversationMessage[],
  turnCount?: number | null,
): { turns: ConversationTurn[]; messages: ConversationMessage[]; keepTurnIds: Set<string> } {
  const superseded = collectSupersededTurnIds([...previousTurns, ...nextTurns]);
  for (const id of [...superseded]) {
    const original = previousTurns.find((row) => row.turn_id === id);
    if (!original) continue;
    const cut = original.seq ?? 0;
    for (const row of previousTurns) {
      if ((row.seq ?? 0) >= cut) superseded.add(row.turn_id);
    }
  }

  const nextTurnIds = new Set<string>();
  for (const turn of nextTurns) {
    if (turn.turn_id && !superseded.has(turn.turn_id)) nextTurnIds.add(turn.turn_id);
  }
  for (const message of nextMessages) {
    const turnId = String(message.turn_id || "");
    if (turnId && !superseded.has(turnId)) nextTurnIds.add(turnId);
  }

  const nextSeqs = nextTurns
    .filter((turn) => turn.turn_id && !superseded.has(turn.turn_id))
    .map((turn) => turn.seq ?? 0)
    .filter((seq) => Number.isFinite(seq));
  const tipMinSeq = nextSeqs.length ? Math.min(...nextSeqs) : Number.POSITIVE_INFINITY;
  const tipMaxSeq = nextSeqs.length ? Math.max(...nextSeqs) : Number.NEGATIVE_INFINITY;
  const fullTip = turnCount != null && nextTurnIds.size >= turnCount;

  const keepTurnIds = new Set<string>();
  for (const id of nextTurnIds) keepTurnIds.add(id);
  for (const turn of previousTurns) {
    const id = turn.turn_id;
    if (!id || nextTurnIds.has(id)) continue;
    if (superseded.has(id)) continue;
    if (fullTip) continue;
    const seq = turn.seq ?? 0;
    if (seq > tipMaxSeq) continue;
    if (seq < tipMinSeq) keepTurnIds.add(id);
  }

  const turns = previousTurns.filter((turn) => keepTurnIds.has(turn.turn_id));
  const messages = previousMessages.filter((message) => {
    const turnId = String(message.turn_id || "");
    if (!turnId) return !fullTip;
    return keepTurnIds.has(turnId);
  });
  return { turns, messages, keepTurnIds };
}

function deriveMessagesPage(
  messages: ConversationMessage[],
  page?: ConversationMessagesPage,
): ConversationMessagesPage {
  const oldest = messages[0];
  const newest = messages[messages.length - 1];
  return {
    limit: page?.limit ?? 50,
    oldest_stream_seq: oldest ? messageSeq(oldest) : page?.oldest_stream_seq ?? null,
    newest_stream_seq: newest ? messageSeq(newest) : page?.newest_stream_seq ?? null,
    oldest_message_id: oldest?.message_id ?? page?.oldest_message_id ?? null,
    newest_message_id: newest?.message_id ?? page?.newest_message_id ?? null,
    has_more_before: Boolean(page?.has_more_before),
    has_more_after: Boolean(page?.has_more_after),
  };
}

export function mergeConversationView(
  previous: ConversationView | null,
  next: ConversationView,
): ConversationView {
  if (!previous || previous.thread.thread_id !== next.thread.thread_id) {
    return {
      ...next,
      messages_page: next.messages_page ?? deriveMessagesPage(next.messages, next.messages_page),
    };
  }
  const prevGen = previous.state?.current_generation ?? 0;
  const nextGen = next.state?.current_generation ?? 0;
  if (nextGen < prevGen) return previous;
  const prevWatermark = previous.watermark ?? previous.state?.head_stream_seq ?? 0;
  const nextWatermark = next.watermark ?? next.state?.head_stream_seq ?? 0;
  if (nextGen === prevGen && nextWatermark < prevWatermark) {
    const superseded = collectSupersededTurnIds(previous.turns, previous.superseded_turns || []);
    const cleaned = omitSupersededBranchRows(next.messages, next.turns, superseded);
    const messages = mergeMessageLists(cleaned.messages, previous.messages);
    return { ...previous, messages,
      turns: mergeTurnLists(cleaned.turns, previous.turns),
      messages_page: deriveMessagesPage(messages, previous.messages_page) };
  }
  const knownSuperseded = collectSupersededTurnIds([
    ...previous.turns,
    ...next.turns,
    ...(next.superseded_turns || []),
  ], [...(previous.superseded_turns || []), ...(next.superseded_turns || [])]);
  const cleanedPrev = omitSupersededBranchRows(
    previous.messages,
    previous.turns,
    knownSuperseded,
  );
  const cleanedNext = omitSupersededBranchRows(
    next.messages,
    next.turns,
    knownSuperseded,
  );
  let baseMessages = cleanedPrev.messages;
  let baseTurns = cleanedPrev.turns;
  let baseArtifacts = previous.artifacts || [];
  if (knownSuperseded.size > 0) {
    baseArtifacts = baseArtifacts.filter((item) => {
      const turnId = String(item.turn_id || "");
      return !turnId || !knownSuperseded.has(turnId);
    });
  }
  if (nextGen > prevGen) {
    const pruned = pruneSupersededBranch(
      baseTurns,
      baseMessages,
      cleanedNext.turns,
      cleanedNext.messages,
      next.statistics?.turn_count,
    );
    baseMessages = pruned.messages;
    baseTurns = pruned.turns;
    baseArtifacts = baseArtifacts.filter((item) => {
      const turnId = String(item.turn_id || "");
      return !turnId || pruned.keepTurnIds.has(turnId);
    });
  }
  const messages = mergeMessageLists(baseMessages, cleanedNext.messages);
  const turns = mergeTurnLists(baseTurns, cleanedNext.turns);
  const prevPage = previous.messages_page;
  const nextPage = next.messages_page;
  let hasMoreBefore = Boolean(nextPage?.has_more_before);
  if (
    prevPage
    && nextPage
    && prevPage.oldest_stream_seq != null
    && nextPage.oldest_stream_seq != null
    && prevPage.oldest_stream_seq < nextPage.oldest_stream_seq
  ) {
    hasMoreBefore = Boolean(prevPage.has_more_before);
  } else if (prevPage && !nextPage) {
    hasMoreBefore = Boolean(prevPage.has_more_before);
  }
  let hasMoreAfter = Boolean(nextPage?.has_more_after);
  if (
    prevPage
    && nextPage
    && prevPage.newest_stream_seq != null
    && nextPage.oldest_stream_seq != null
    && prevPage.newest_stream_seq < nextPage.oldest_stream_seq
  ) {
    hasMoreAfter = true;
  } else if (prevPage?.has_more_after && nextPage && !nextPage.has_more_after) {
    const prevNewest = prevPage.newest_stream_seq;
    const nextNewest = nextPage.newest_stream_seq;
    if (
      prevNewest != null
      && nextNewest != null
      && prevNewest < nextNewest
      && nextPage.oldest_stream_seq != null
      && prevNewest < nextPage.oldest_stream_seq
    ) {
      hasMoreAfter = true;
    } else if (Boolean(prevPage.around_message_id) && prevNewest != null) {
      const tipOldest = nextPage.oldest_stream_seq;
      if (tipOldest != null && prevNewest < tipOldest) {
        hasMoreAfter = true;
      }
    }
  } else if (prevPage && !nextPage) {
    hasMoreAfter = Boolean(prevPage.has_more_after);
  }
  const aroundCandidate = nextPage?.around_message_id
    ?? (hasMoreAfter ? prevPage?.around_message_id : undefined)
    ?? null;
  const aroundStillOnMain = Boolean(
    aroundCandidate
    && messages.some((message) => message.message_id === aroundCandidate),
  );
  const aroundMessageId = aroundStillOnMain ? aroundCandidate : null;
  const messages_page: ConversationMessagesPage = {
    limit: nextPage?.limit ?? prevPage?.limit ?? 50,
    oldest_stream_seq: messages[0] ? messageSeq(messages[0]) : null,
    newest_stream_seq: messages.length
      ? messageSeq(messages[messages.length - 1])
      : null,
    oldest_message_id: messages[0]?.message_id ?? null,
    newest_message_id: messages.length
      ? messages[messages.length - 1].message_id
      : null,
    has_more_before: hasMoreBefore,
    has_more_after: hasMoreAfter,
    around_message_id: aroundMessageId || undefined,
  };
  const artifactsByDigest = new Map<
    string,
    NonNullable<ConversationView["artifacts"]>[number]
  >();
  for (const item of baseArtifacts) {
    const key = item.artifact_id || (item.needs_reselect && item.sha256 ? `source:${item.sha256}` : "");
    if (key) artifactsByDigest.set(key, item);
  }
  for (const item of next.artifacts || []) {
    const key = item.artifact_id || (item.needs_reselect && item.sha256 ? `source:${item.sha256}` : "");
    if (key) artifactsByDigest.set(key, item);
  }
  return {
    ...next,
    messages,
    turns,
    messages_page,
    artifacts: Array.from(artifactsByDigest.values()),
  };
}

function notifyThreadsChanged(): void {
  if (typeof window !== "undefined") {
    window.dispatchEvent(new Event("muteki:threads-changed"));
  }
}

function notifyThreadsRead(threadIds: string[]): void {
  if (typeof window !== "undefined" && threadIds.length > 0) {
    window.dispatchEvent(new CustomEvent<string[]>("muteki:threads-read", { detail: threadIds }));
  }
}

export async function fetchConversationView(
  threadId: string,
  markRead = false,
  messagesLimit = 50,
  options?: { signal?: AbortSignal; timeline?: boolean },
): Promise<ConversationView> {
  const query = new URLSearchParams({
    mark_read: markRead ? "true" : "false",
    messages_limit: String(messagesLimit),
  });
  if (options?.timeline) query.set("view", "timeline");
  try {
    const res = await apiFetch(
      `/api/threads/${encodeURIComponent(threadId)}?${query}`,
      { signal: requestSignal(options?.signal) },
    );
    if (!res.ok) throwThreadLoadFailure(res.status);
    const view = (await res.json()) as ConversationView;
    if (markRead) {
      notifyThreadsRead([threadId]);
      notifyThreadsChanged();
    }
    return view;
  } catch (exc) {
    rethrowNetworkLoadFailure(exc);
  }
}

export async function markConversationThreadRead(threadId: string, signal?: AbortSignal, through?: number): Promise<void> {
  const query = new URLSearchParams({ mark_read: "true", messages_limit: "1", view: "read-receipt" });
  if (through !== undefined) query.set("read_through", String(through));
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}?${query}`,
    { keepalive: !signal, signal: signal ? requestSignal(signal) : undefined },
  );
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const receipt = await res.json() as { unread?: boolean };
  if (receipt?.unread !== true) notifyThreadsRead([threadId]);
  notifyThreadsChanged();
}

function isConversationForeground(nativeState?: DesktopNativeState | null): boolean {
  return typeof document !== "undefined"
    && document.visibilityState === "visible"
    && (document.hasFocus() || Boolean(nativeState?.previewFocused));
}

function nativeThreadVisible(state: DesktopNativeState | null, threadId: string): boolean {
  const owner = currentAuthScope();
  return Boolean(owner && owner === conversationStorageScope()
    && state && Number.isSafeInteger(state.connectionVersion) && (state.connectionVersion || 0) > 0
    && state.transportOrigin === API && `${state.serviceId}:${state.identityId}` === owner
    && Array.isArray(state.notificationVisibleThreadIds)
    && state.notificationVisibleThreadIds.includes(threadId));
}

export async function fetchConversationMessagesPage(
  threadId: string,
  options: {
    limit?: number;
    beforeStreamSeq?: number | null;
    afterStreamSeq?: number | null;
    aroundMessageId?: string | null;
    signal?: AbortSignal;
  } = {},
): Promise<{
  messages: ConversationMessage[];
  messages_page: ConversationMessagesPage;
  turns: ConversationTurn[];
}> {
  const query = new URLSearchParams({
    limit: String(options.limit ?? 50),
  });
  if (options.beforeStreamSeq != null) {
    query.set("before_stream_seq", String(options.beforeStreamSeq));
  }
  if (options.afterStreamSeq != null) {
    query.set("after_stream_seq", String(options.afterStreamSeq));
  }
  if (options.aroundMessageId) {
    query.set("around_message_id", options.aroundMessageId);
  }
  try {
    const res = await apiFetch(
      `/api/threads/${encodeURIComponent(threadId)}/messages?${query}`,
      { signal: requestSignal(options.signal) },
    );
    if (!res.ok) {
      const error = new Error(
        res.status === 0
          ? threadLoadFailureMessage(0)
          : res.status === 404
            ? "找不到更早的消息（会话可能已删除）。"
            : res.status === 401 || res.status === 403
              ? "没有权限加载历史消息，请重新登录后重试。"
              : res.status >= 500
                ? `加载历史消息失败，服务暂时不可用（HTTP ${res.status}）。`
                : `加载历史消息失败（HTTP ${res.status}）。`,
      ) as Error & { httpStatus?: number };
      error.httpStatus = res.status;
      throw error;
    }
    return (await res.json()) as {
      messages: ConversationMessage[];
      messages_page: ConversationMessagesPage;
      turns: ConversationTurn[];
    };
  } catch (exc) {
    if (exc instanceof DOMException && exc.name === "AbortError") throw exc;
    rethrowNetworkLoadFailure(exc);
  }
}

export function queryLooksSearchable(raw: string): boolean {
  const q = raw.trim();
  if (!q) return false;
  const hasCjk = /[\u4e00-\u9fff]/.test(q);
  return hasCjk ? q.length >= 1 : q.length >= 2;
}

export async function fetchConversationSearch(
  query: string,
  options: {
    projectId?: string;
    includeArchived?: boolean;
    includeSuperseded?: boolean;
    includeSrc?: boolean;
    limit?: number;
    offset?: number;
    signal?: AbortSignal;
  } = {},
): Promise<ConversationSearchResult> {
  const q = query.trim();
  if (!queryLooksSearchable(q)) {
    return { query: q, count: 0, hits: [], reason: "query_too_short" };
  }
  const params = new URLSearchParams({
    q,
    limit: String(options.limit ?? 30),
    offset: String(options.offset ?? 0),
  });
  if (options.projectId) params.set("project_id", options.projectId);
  if (options.includeArchived) params.set("include_archived", "true");
  if (options.includeSuperseded) params.set("include_superseded", "true");
  if (options.includeSrc) params.set("include_src", "true");
  const res = await apiFetch(`/api/threads/search?${params}`, {
    signal: requestSignal(options.signal),
  });
  if (!res.ok) {
    const raw = await res.text();
    let serverError: CommandReceiptView["error"];
    try { serverError = (JSON.parse(raw) as { error?: CommandReceiptView["error"] })?.error; }
    catch {  }
    throw conversationCommandErrorFrom({ ...serverError,
      code: serverError?.code || "conversation.search.http_error",
      category: serverError?.category || "transport",
      message: serverError?.message || `搜索对话失败（HTTP ${res.status}）：${raw}`,
      retryable: serverError?.retryable ?? (res.status >= 500 || res.status === 429),
      detail: { ...serverError?.detail, response_body: raw },
    }, "搜索对话失败", { httpStatus: res.status });
  }
  const body = await res.json() as ConversationSearchResult;
  if (!body || !Array.isArray(body.hits) || body.hits.some((hit) => (
    !hit || typeof hit.message_id !== "string" || !hit.message_id
    || typeof hit.thread_id !== "string" || !hit.thread_id
    || typeof hit.snippet !== "string" || typeof hit.thread_title !== "string"
    || typeof hit.role !== "string" || typeof hit.archived !== "boolean"
    || typeof hit.superseded !== "boolean" || !Number.isSafeInteger(hit.stream_seq)
  )) || (body.has_more != null && typeof body.has_more !== "boolean")
    || (body.next_offset != null && (!Number.isSafeInteger(body.next_offset) || body.next_offset < 0))
    || (body.has_more && (body.next_offset == null || body.next_offset <= (options.offset ?? 0)))) {
    throw conversationCommandErrorFrom({ code: "conversation.search.protocol_invalid", category: "protocol",
      message: "conversation.search.protocol_invalid：搜索响应缺少合法命中或分页游标", retryable: false,
      detail: { response_body: body } }, "搜索响应格式无效");
  }
  return body;
}

export async function fetchTurnContext(threadId: string, turnId: string, signal?: AbortSignal): Promise<TurnContextLedger> {
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}/turns/${encodeURIComponent(turnId)}/context`,
    { signal },
  );
  if (!res.ok) throw new Error(`加载实际发送内容失败（HTTP ${res.status}）：${await res.text()}`);
  const body = await res.json() as TurnContextLedger;
  if (!body || body.turn_id !== turnId || !Array.isArray(body.parts)) {
    throw new Error("加载实际发送内容失败：响应格式无效");
  }
  return body;
}

export async function fetchTurnProcess(
  threadId: string,
  turnId: string,
  options: { limit?: number; afterSeq?: number; beforeSeq?: number; watermark?: number; signal?: AbortSignal } = {},
): Promise<{
  turn_id: string;
  events: ConversationEvent[];
  count: number;
  truncated: boolean;
  has_more: boolean;
  next_after_seq?: number | null;
  next_before_seq?: number | null;
  watermark: number;
}> {
  const query = new URLSearchParams({
    limit: String(options.limit ?? 2000),
  });
  if (options.afterSeq != null) query.set("after_seq", String(options.afterSeq));
  if (options.beforeSeq != null) query.set("before_seq", String(options.beforeSeq));
  if (options.watermark != null) query.set("watermark", String(options.watermark));
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}/turns/${encodeURIComponent(turnId)}/process?${query}`,
    { signal: requestSignal(options.signal) },
  );
  if (!res.ok) throw new Error(`加载回合过程失败（HTTP ${res.status}）：${await res.text()}`);
  const body = await res.json() as {
    turn_id: string; events: Array<ConversationEvent & { stream_seq?: number }>;
    count: number; truncated: boolean; has_more: boolean;
    next_after_seq?: number | null; next_before_seq?: number | null; watermark: number;
  };
  const after = options.afterSeq ?? 0;
  const before = options.beforeSeq;
  if (!body || body.turn_id !== turnId || !Array.isArray(body.events)
    || !Number.isSafeInteger(body.watermark) || body.watermark < after
    || (options.watermark != null && body.watermark !== options.watermark)
    || typeof body.has_more !== "boolean" || typeof body.truncated !== "boolean"
    || body.has_more !== body.truncated
    || body.count !== body.events.length
    || body.events.some(event => !event || !Number.isSafeInteger(event.seq ?? event.stream_seq)
      || Number(event.seq ?? event.stream_seq) <= after || Number(event.seq ?? event.stream_seq) > body.watermark
      || (before != null && Number(event.seq ?? event.stream_seq) >= before)
      || typeof event.event_type !== "string" || !event.payload || typeof event.payload !== "object")
    || body.events.some((event, index) => index > 0
      && Number(event.seq ?? event.stream_seq) <= Number(body.events[index - 1].seq ?? body.events[index - 1].stream_seq))
    || (body.has_more && (before == null
      ? body.next_after_seq !== Number(body.events.at(-1)?.seq ?? body.events.at(-1)?.stream_seq)
      : body.next_before_seq !== Number(body.events[0]?.seq ?? body.events[0]?.stream_seq)))) {
    throw conversationCommandErrorFrom({ code: "conversation.process.protocol_invalid", category: "protocol",
      message: "conversation.process.protocol_invalid：回合过程响应身份、事件或后续游标无效", retryable: false,
      detail: { response_body: body, thread_id: threadId, turn_id: turnId } }, "回合过程响应格式无效");
  }
  return { ...body, events: body.events.map(event => ({
    ...event, seq: Number(event.seq ?? event.stream_seq ?? 0),
  })) };

}

export async function fetchRuntimeInstances(): Promise<RuntimeInstance[]> {
  const res = await apiFetch("/api/agent-runtimes").catch(() => null);
  if (!res?.ok) {
    const error = new Error(
      `加载 Runtime 失败（HTTP ${res?.status ?? "network"}）`,
    ) as Error & { httpStatus?: number };
    error.httpStatus = res?.status ?? 0;
    throw error;
  }
  const body = (await res.json()) as { instances?: RuntimeInstance[] };
  const rows = body.instances ?? [];
  const descriptors = await loadProviderDescriptors();
  const unprobed = rows.filter((row) => {
    const descriptor = descriptorForAdapter(descriptors, row.adapter_id);
    return Boolean(descriptor?.login.system_login_only)
      && descriptor?.identity.default_adapter_id === row.adapter_id
      && row.enabled !== false && !row.health;
  });
  if (unprobed.length) {
    await Promise.all(unprobed.map((row) => apiFetch(
      `/api/agent-runtimes/${encodeURIComponent(`${row.adapter_id}:${row.instance_id}`)}/probe`,
      { method: "POST" },
    )));
    const refreshed = await apiFetch("/api/agent-runtimes");
    if (refreshed.ok) return ((await refreshed.json()) as { instances?: RuntimeInstance[] }).instances ?? rows;
  }
  return rows;
}

export async function probeRuntimeInstance(
  key: string,
  options: { credentialId?: string; environment?: "local" } = {},
): Promise<void> {
  const ownerScope = conversationStorageScope();
  const res = await apiFetch(
    `/api/agent-runtimes/${encodeURIComponent(key)}/probe`,
    { method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ credential_id: options.credentialId || "", environment: options.environment || "local" }) },
  );
  const initial = await receiptOf(res, ownerScope);
  const receipt = await waitForTerminalReceipt(initial, "验证 Agent 与模型目录", { ownerScope });
  if (receipt.error || ["failed", "conflict", "cancelled"].includes(receipt.state)) {
    throw conversationCommandErrorFrom(receipt.error, "探测 Runtime 失败", { httpStatus: res.status });
  }
  if (receipt.state !== "completed") {
    throw conversationCommandErrorFrom({ code: "runtime.instance.probe_outcome_unknown", category: "state",
      message: "Agent 探测仍在进行，请稍后刷新接入状态", retryable: true,
      detail: { command_id: receipt.command_id, outcome_unknown: true } }, "探测结果待确认");
  }
}

function credentialModels(value: unknown): ConversationCredentialModel[] {
  if (!Array.isArray(value)) return [];
  const seen = new Set<string>();
  const rows: ConversationCredentialModel[] = [];
  for (const item of value) {
    const record = item && typeof item === "object"
      ? item as Record<string, unknown>
      : null;
    const id = String(record?.id ?? record?.model ?? item ?? "").trim();
    if (!id || seen.has(id)) continue;
    seen.add(id);
    const rawReasoning = record?.reasoning;
    const rawVariant = record?.variant;
    const variant = rawVariant && typeof rawVariant === "object"
      ? rawVariant as Record<string, unknown> : null;
    const familyId = typeof variant?.family_id === "string" ? variant.family_id.trim() : "";
    const reasoning = rawReasoning && typeof rawReasoning === "object"
      ? rawReasoning as Record<string, unknown>
      : null;
    const serviceTiers = Array.isArray(record?.service_tiers)
      ? record.service_tiers.flatMap((tier): ConversationServiceTier[] => {
        const row = tier && typeof tier === "object" ? tier as Record<string, unknown> : null;
        const tierId = String(row?.id ?? "").trim();
        return tierId ? [{ id: tierId, name: String(row?.name || tierId), description: String(row?.description ?? "") }] : [];
      })
      : [];
    rows.push({
      id,
      label: String(record?.label ?? record?.name ?? id),
      ...(familyId ? {
        variant: {
          family_id: familyId,
          family_label: String(variant?.family_label || familyId),
          parameters: variant?.parameters && typeof variant.parameters === "object" && !Array.isArray(variant.parameters)
            ? Object.fromEntries(Object.entries(variant.parameters).filter((entry): entry is [string, string] => typeof entry[1] === "string"))
            : {},
          ...(variant?.default === true ? { default: true } : {}),
        },
      } : {}),
      ...(reasoning ? {
        reasoning: {
          supported: reasoning.supported !== false && Array.isArray(reasoning.levels) && reasoning.levels.length > 0,
          levels: Array.isArray(reasoning.levels)
            ? reasoning.levels.map(String)
            : [],
          default: String(reasoning.default ?? ""),
          kind: String(reasoning.kind ?? "effort"),
          source: String(reasoning.source ?? ""),
        },
      } : {}),
      ...(serviceTiers.length ? {
        service_tiers: serviceTiers,
        default_service_tier: String(record?.default_service_tier ?? ""),
      } : {}),
    });
  }
  return rows;
}

function normalizeCredential(
  value: unknown,
): ConversationCredential | null {
  if (!value || typeof value !== "object") return null;
  const row = value as Record<string, unknown>;
  const accountId = String(row.account_id ?? "").trim();
  const hintedId = String(row.id ?? row.credential_id ?? "").trim();
  const engine = String(
    row.engine ?? row.worker_engine
      ?? (row.details && typeof row.details === "object"
        ? (row.details as Record<string, unknown>).target_engine
        : "")
      ?? "",
  ).trim().toLowerCase();
  if (!engine) return null;
  const source: "stored" | "system" = row.source === "system"
    || hintedId.startsWith("system:")
    ? "system"
    : "stored";
  const id = hintedId || (
    source === "system" ? `system:${engine}` : accountId ? `account:${accountId}` : ""
  );
  if (!id) return null;
  const catalog = row.model_catalog && typeof row.model_catalog === "object"
    ? row.model_catalog as Record<string, unknown> : null;
  const discovered = new Map(credentialModels(catalog?.discovered_models).map(model => [model.id, model]));
  const enrichModels = (value: unknown) => credentialModels(value).map(model => {
    const metadata = discovered.get(model.id);
    return metadata ? {
      ...model, label: metadata.label || model.label, reasoning: metadata.reasoning, variant: metadata.variant,
      service_tiers: metadata.service_tiers, default_service_tier: metadata.default_service_tier,
    } : model;
  });
  const models = enrichModels(row.models ?? row.available_models);
  const verifiedIds = new Set(models.map((item) => item.id));
  const candidateModels = enrichModels(
    row.candidate_models ?? row.models ?? row.available_models,
  ).filter((item) => !verifiedIds.has(item.id));
  const present = row.present === null ? null : row.present !== false;
  const statusDetail = String(row.status_detail ?? "").trim();
  return {
    id,
    label: String(
      row.label
        ?? (source === "system" ? `${engine} 宿主登录` : accountId || id),
    ),
    engine,
    runtime_instance: String(catalog?.runtime_instance ?? ""),
    model_catalogs: row.model_catalogs && typeof row.model_catalogs === "object"
      ? Object.fromEntries(Object.entries(row.model_catalogs).map(([key, value]) => [
          key, credentialModels(value && typeof value === "object"
            ? (value as Record<string, unknown>).discovered_models : []),
        ])) : {},
    verified_models_by_runtime: row.model_catalogs && typeof row.model_catalogs === "object"
      ? Object.fromEntries(Object.entries(row.model_catalogs).map(([key, value]) => [
          key, credentialModels(value && typeof value === "object"
            ? (value as Record<string, unknown>).verified_models : []).map((model) => model.id),
        ])) : undefined,
    source,
    ...(accountId ? { account_id: accountId } : {}),
    connection: String(row.connection ?? ""),
    provider: String(row.provider ?? ""),
    base_url: String(row.base_url ?? ""),
    present,
    status: String(row.status ?? (present === null ? "unknown" : present ? "ready" : "missing")),
    ...(typeof row.detection_status === "string"
      ? { detection_status: row.detection_status as ConversationCredential["detection_status"] } : {}),
    ...(statusDetail ? { status_detail: statusDetail } : {}),
    ...(typeof row.discovery_code === "string" ? { discovery_code: row.discovery_code } : {}),
    models,
    candidate_models: candidateModels,
    default_model: String(row.default_model ?? row.suggested_model ?? ""),
    last_test: row.last_test && typeof row.last_test === "object"
      ? row.last_test as Record<string, unknown>
      : null,
    usage: Array.isArray(row.usage)
      ? row.usage.filter((item): item is Record<string, unknown> => (
        Boolean(item) && typeof item === "object"
      ))
      : [],
  };
}

export function normalizeConversationCredential(
  value: unknown,
): ConversationCredential | null {
  return normalizeCredential(value);
}

export function allCredentialModels(
  credential: ConversationCredential,
): ConversationCredentialModel[] {
  const seen = new Set<string>();
  const rows: ConversationCredentialModel[] = [];
  for (const model of [...credential.models, ...credential.candidate_models]) {
    if (!model.id || seen.has(model.id)) continue;
    seen.add(model.id);
    rows.push(model);
  }
  return rows;
}

export async function requestThreadCompact(threadId: string): Promise<CommandReceiptView> {
  const commandId = `cmd_compact_${typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
    ? crypto.randomUUID() : `${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`}`;
  return sendConversationCommand(threadId, "conversation.turn.send", {
    text: "/compact",
    client_message_id: commandId,
    attachments: [],
    capability_refs: [],
  }, { commandId, idempotencyKey: commandId });
}

export async function fetchConversationCredentials(options?: {
  fresh?: boolean;
}): Promise<ConversationCredential[]> {
  const fresh = options?.fresh ? "&fresh=1" : "";
  const canonical = await apiFetch(
    `/api/settings/credentials?environment=local${fresh}`,
  ).catch(() => null);
  if (!canonical?.ok) {
    const error = new Error(
      `加载全局凭据失败（HTTP ${canonical?.status ?? "network"}）`,
    ) as Error & { httpStatus?: number };
    error.httpStatus = canonical?.status ?? 0;
    throw error;
  }
  const body = (await canonical.json()) as { credentials?: unknown[] };
  const [descriptors, runtimes] = await Promise.all([
    loadProviderDescriptors(), fetchRuntimeInstances().catch(() => [] as RuntimeInstance[]),
  ]);
  const extensionEngines = new Set(runtimes.filter((row) => row.origin === "extension" && row.enabled !== false).map((row) => row.engine));
  return (body.credentials ?? [])
    .map((item) => normalizeCredential(item))
    .filter((item): item is ConversationCredential => (
      item !== null && (isKnownEngine(descriptors, item.engine) || extensionEngines.has(item.engine))
    ));
}

export async function fetchConversationProjects(options?: {
  includeUnbound?: boolean;
  scope?: "chat" | "src";
}): Promise<ConversationProject[]> {
  const url = options?.scope
    ? `/api/projects?scope=${encodeURIComponent(options.scope)}`
    : options?.includeUnbound
      ? "/api/projects?include_unbound=true"
      : "/api/projects";
  const res = await apiFetch(url, { cache: "no-store" }).catch(() => null);
  if (!res?.ok) {
    const error = new Error(
      `加载 Project 失败（HTTP ${res?.status ?? "network"}）`,
    ) as Error & { httpStatus?: number };
    error.httpStatus = res?.status ?? 0;
    throw error;
  }
  const body = await res.json() as { projects?: Array<Partial<ConversationProject>> };
  const rows = Array.isArray(body.projects) ? body.projects : [];
  return rows.map((row) => ({
    ...row,
    project_id: String(row.project_id || ""),
    name: String(row.name || ""),
    scope: row.scope === "src" || row.scope === "chat" ? row.scope : undefined,
    workspace_id: typeof row.workspace_id === "string" ? row.workspace_id : "",
    workspace_kind: row.workspace_kind ?? null,
    root_path: typeof row.root_path === "string" ? row.root_path : "",
  }));
}

export async function selectConversationDirectory(): Promise<ConversationDirectorySelection | null> {
  const res = await apiFetch("/api/directories/select", { method: "POST" });
  const body = await res.json().catch(() => ({})) as {
    cancelled?: boolean;
    path?: string;
    name?: string;
    error?: { message?: string; code?: string };
  };
  if (!res.ok) {
    const error = new Error(
      body.error?.message || body.error?.code || `打开目录选择器失败（HTTP ${res.status}）`,
    ) as Error & { code?: string; httpStatus?: number };
    error.code = String(body.error?.code || "");
    error.httpStatus = res.status;
    throw error;
  }
  if (body.cancelled) return null;
  const path = String(body.path || "").trim();
  const name = String(body.name || "").trim();
  if (!path || !name) throw new Error("目录选择器没有返回有效的工作目录");
  return { path, name };
}

export async function createConversationProject(input: {
  name?: string;
  root_path?: string;
  description?: string;
  scope?: "chat" | "src";
}): Promise<{ projectId: string; workspaceId: string; rootPath?: string; reused?: boolean }> {
  const ownerScope = conversationStorageScope();
  const payload: Record<string, unknown> = { command_id: commandId() };
  if (typeof input.name === "string" && input.name.trim()) payload.name = input.name.trim();
  if (typeof input.root_path === "string" && input.root_path.trim()) payload.root_path = input.root_path.trim();
  if (typeof input.description === "string" && input.description.trim()) payload.description = input.description.trim();
  if (input.scope) payload.scope = input.scope;
  const res = await apiFetch("/api/projects", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  const receipt = await receiptOf(res, ownerScope);
  if (receipt.error?.code === "conversation.project.directory_already_bound") {
    const projectId = String(receipt.error.detail?.project_id || "").trim();
    const workspaceId = String(receipt.error.detail?.workspace_id || "").trim();
    if (projectId && workspaceId) {
      return {
        projectId,
        workspaceId,
        rootPath: String(receipt.error.detail?.root_path || "").trim(),
        reused: true,
      };
    }
  }
  recordTaskReceipt(receipt, "创建项目", "conversation", ownerScope);
  if (receipt.error) {
    throw conversationCommandErrorFrom(receipt.error, "创建项目失败", {
      httpStatus: res.status,
    });
  }
  if (!receipt.aggregate?.id) throw new Error("Project 回执缺少 aggregate id");
  const workspaceId = String(receipt.output?.workspace_id || "").trim();
  const rootPath = String(receipt.output?.root_path || "").trim();
  return {
    projectId: receipt.aggregate.id,
    workspaceId,
    ...(rootPath ? { rootPath } : {}),
  };
}

export async function updateConversationProjectSettings(
  projectId: string,
  settings: Record<string, string>,
): Promise<CommandReceiptView> {
  const ownerScope = conversationStorageScope();
  const res = await apiFetch(`/api/projects/${encodeURIComponent(projectId)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ settings, command_id: commandId() }),
  });
  const receipt = await receiptOf(res, ownerScope);
  recordTaskReceipt(receipt, "更新项目设置", "conversation", ownerScope);
  if (receipt.error || (RECEIPT_TERMINAL_STATES.has(receipt.state || "") && receipt.state !== "completed")) {
    throw conversationCommandErrorFrom(receipt.error, `更新项目设置未完成（${receipt.state}），原设置仍然有效`, { httpStatus: res.status });
  }
  return receipt;
}

export async function deleteConversationProject(projectId: string): Promise<{ deletedThreads: number }> {
  const res = await apiFetch(`/api/projects/${encodeURIComponent(projectId)}`, { method: "DELETE" });
  const body = await res.json().catch(() => ({})) as {
    deleted_threads?: number;
    error?: { code?: string; message?: string; detail?: { active_threads?: number } };
  };
  if (!res.ok) {
    const code = String(body.error?.code || "");
    const active = Number(body.error?.detail?.active_threads || 0);
    const message = code === "project.in_use"
      ? (active ? `项目内还有 ${active} 个未归档对话，请先归档再删除` : "项目内还有未归档对话，请先归档再删除")
      : body.error?.message || code || `删除项目失败（HTTP ${res.status}）`;
    const error = new Error(message) as Error & { code?: string; httpStatus?: number };
    error.code = code;
    error.httpStatus = res.status;
    throw error;
  }
  return { deletedThreads: Number(body.deleted_threads || 0) };
}

export interface ProjectGitStatus {
  read_only?: boolean;
  pull_requests_supported?: boolean;
  project_id?: string;
  thread_id?: string;
  workspace_id?: string | null;
  is_repo: boolean;
  root_path: string;
  current_branch: string | null;
  detached_head?: boolean;
  detached_sha?: string | null;
  branches: string[];
  dirty: boolean;
  mode?: string;
  configured_branch?: string | null;
  occupied?: boolean;
  occupant_count?: number;
  occupant_thread_ids?: string[];
  branch_switch_blocked?: boolean;
  branch_switch_blockers?: Array<{ thread_id: string; title: string }>;
  git_common_dir?: string | null;
  changed_file_count?: number;
  remotes?: string[];
  upstream?: string | null;
  ahead?: number | null;
  behind?: number | null;
  head_sha?: string | null;
  head_subject?: string | null;
}

export type WorkspaceBindMode = "shared_checkout" | "existing_worktree" | "new_worktree";

export interface GitWorktreeRow {
  path: string;
  branch: string | null;
  detached?: boolean;
  occupied?: boolean;
  occupant_count?: number;
  occupant_thread_ids?: string[];
}

export function parseGitStatus(body: any, fallback: Partial<ProjectGitStatus> = {}): ProjectGitStatus {
  return {
    read_only: body.read_only === true,
    pull_requests_supported: body.pull_requests_supported !== false,
    project_id: body.project_id ? String(body.project_id) : fallback.project_id,
    thread_id: body.thread_id ? String(body.thread_id) : fallback.thread_id,
    workspace_id: body.workspace_id ? String(body.workspace_id) : (fallback.workspace_id ?? null),
    is_repo: Boolean(body.is_repo),
    root_path: String(body.root_path || ""),
    current_branch: body.current_branch ? String(body.current_branch) : null,
    detached_head: Boolean(body.detached_head),
    detached_sha: body.detached_sha ? String(body.detached_sha) : null,
    branches: Array.isArray(body.branches) ? body.branches.map(String) : [],
    dirty: Boolean(body.dirty),
    mode: body.mode ? String(body.mode) : fallback.mode,
    configured_branch: body.configured_branch ? String(body.configured_branch) : null,
    occupied: Boolean(body.occupied),
    occupant_count: Number(body.occupant_count || 0),
    occupant_thread_ids: Array.isArray(body.occupant_thread_ids)
      ? body.occupant_thread_ids.map(String)
      : [],
    branch_switch_blocked: Boolean(body.branch_switch_blocked),
    branch_switch_blockers: Array.isArray(body.branch_switch_blockers)
      ? body.branch_switch_blockers.map((thread: any) => ({ thread_id: String(thread.thread_id), title: String(thread.title || thread.thread_id) }))
      : [],
    git_common_dir: body.git_common_dir ? String(body.git_common_dir) : null,
    changed_file_count: Number(body.changed_file_count || 0),
    remotes: Array.isArray(body.remotes) ? body.remotes.map(String) : [],
    upstream: body.upstream ? String(body.upstream) : null,
    ahead: typeof body.ahead === "number" ? body.ahead : null,
    behind: typeof body.behind === "number" ? body.behind : null,
    head_sha: body.head_sha ? String(body.head_sha) : null,
    head_subject: body.head_subject ? String(body.head_subject) : null,
  };
}

export async function fetchProjectGitStatus(projectId: string): Promise<ProjectGitStatus> {
  const res = await apiFetch(`/api/projects/${encodeURIComponent(projectId)}/git`);
  const body = await res.json().catch(() => ({} as any));
  if (!res.ok) {
    throw new Error(
      body.error?.message || body.error?.code || `读取 Git 状态失败（HTTP ${res.status}）`,
    );
  }
  return parseGitStatus(body, { project_id: projectId });
}

export async function fetchProjectWorktrees(projectId: string): Promise<GitWorktreeRow[]> {
  const res = await apiFetch(`/api/projects/${encodeURIComponent(projectId)}/git/worktrees`);
  const body = await res.json().catch(() => ({} as any));
  if (!res.ok) {
    throw new Error(
      body.error?.message || body.error?.code || `读取 worktree 列表失败（HTTP ${res.status}）`,
    );
  }
  const rows = Array.isArray(body.worktrees) ? body.worktrees : [];
  return rows.map((row: any) => ({
    path: String(row.path || ""),
    branch: row.branch ? String(row.branch) : null,
    detached: Boolean(row.detached),
    occupied: Boolean(row.occupied),
    occupant_count: Number(row.occupant_count || 0),
    occupant_thread_ids: Array.isArray(row.occupant_thread_ids)
      ? row.occupant_thread_ids.map(String)
      : [],
  }));
}

export async function checkoutProjectBranch(
  projectId: string,
  branch: string,
  options: { create?: boolean } = {},
): Promise<ProjectGitStatus> {
  const res = await apiFetch(`/api/projects/${encodeURIComponent(projectId)}/git/checkout`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ branch, create: Boolean(options.create) }),
  });
  const body = await res.json().catch(() => ({} as any));
  if (!res.ok) {
    throw new Error(
      body.error?.message || body.error?.code || `切换分支失败（HTTP ${res.status}）`,
    );
  }
  return parseGitStatus(body, { project_id: projectId });
}

export async function fetchThreadGitStatus(threadId: string): Promise<ProjectGitStatus> {
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/git`);
  const body = await res.json().catch(() => ({} as any));
  if (!res.ok) {
    throw new Error(
      body.error?.message || body.error?.code || `读取会话 Git 状态失败（HTTP ${res.status}）`,
    );
  }
  return parseGitStatus(body, { thread_id: threadId });
}

export async function checkoutThreadBranch(
  threadId: string,
  branch: string,
  options: { create?: boolean } = {},
): Promise<ProjectGitStatus> {
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/git/checkout`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ branch, create: Boolean(options.create) }),
  });
  const body = await res.json().catch(() => ({} as any));
  if (!res.ok) {
    throw new Error(
      body.error?.message || body.error?.code || `切换分支失败（HTTP ${res.status}）`,
    );
  }
  return parseGitStatus(body, { thread_id: threadId });
}

export async function bindConversationWorkspace(input: {
  project_id: string;
  mode: WorkspaceBindMode;
  kind?: string;
  root_path?: string;
  branch?: string;
  base_ref?: string;
  parent_root?: string;
}, options: CommandIdOptions = {}): Promise<{ workspaceId: string; rootPath: string; settings: Record<string, unknown> }> {
  const ownerScope = conversationStorageScope();
  const requestId = String(options.commandId || "").trim() || commandId();
  const idempotencyKey = String(options.idempotencyKey || "").trim() || requestId;
  let receipt: CommandReceiptView;
  let httpStatus: number | undefined;
  try {
    const res = await apiFetch("/api/workspaces", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      project_id: input.project_id,
      mode: input.mode,
      kind: input.kind || (input.mode === "shared_checkout" ? "local" : "git"),
      root_path: input.root_path || "",
      branch: input.branch || "",
      base_ref: input.base_ref || "HEAD",
      parent_root: input.parent_root || "",
      command_id: requestId,
      idempotency_key: idempotencyKey,
    }),
    signal: requestSignal(options.signal),
    });
    httpStatus = res.status;
    receipt = await receiptOf(res, ownerScope);
  } catch (exc) {
    if (options.signal?.aborted) throw exc;
    const recovered = await recoverCommandReceipt(requestId, 8, 200, { ...options, ownerScope });
    if (!recovered) throw exc;
    receipt = recovered;
  }
  receipt = await waitForTerminalReceipt(receipt, "绑定工作区", { ...options, ownerScope });
  recordTaskReceipt(receipt, "绑定工作区", "conversation", ownerScope);
  if (receipt.error || (RECEIPT_TERMINAL_STATES.has(receipt.state) && receipt.state !== "completed")) {
    throw conversationCommandErrorFrom(receipt.error, "绑定工作区失败", {
      httpStatus, deduplicated: Boolean(receipt.deduplicated),
    });
  }
  if (receipt.state !== "completed") throw conversationCommandErrorFrom({
    code: "conversation.workspace.pending", category: "state", retryable: true,
    message: "工作区绑定尚未确认完成，请继续恢复原命令；不会创建新工作区",
    detail: { command_id: receipt.command_id, receipt_state: receipt.state, outcome_unknown: true },
  }, "工作区绑定尚未确认");
  const workspaceId = String(
    receipt.aggregate?.id || receipt.output?.workspace_id || "",
  ).trim();
  if (!workspaceId) throw new Error("Workspace 回执缺少 workspace_id");
  return {
    workspaceId,
    rootPath: String(receipt.output?.root_path || ""),
    settings: (receipt.output?.settings as Record<string, unknown>) || {},
  };
}

export async function createConversationThread(
  input: {
    title: string;
    title_source?: "fallback" | "user";
    mode: ThreadMode;
    project_id?: string;
    workspace_id?: string;
    runtime: {
      adapter_id: string;
      instance_id: string;
      credential_id?: string;
      credential_ref?: string;
      model?: string;
      effort?: string;
      service_tier?: string;
      access_mode?: string;
      interaction_mode?: string;
      permission_mode?: string;
      sandbox_mode?: string;
    };
  },
  options: CommandIdOptions = {},
): Promise<string> {
  const ownerScope = conversationStorageScope();
  const requestId = String(options.commandId || "").trim() || commandId();
  const idempotencyKey = String(options.idempotencyKey || "").trim() || requestId;
  let receipt: CommandReceiptView;
  let httpStatus: number | undefined;
  try {
    const res = await apiFetch("/api/threads", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        ...input,
        command_id: requestId,
        idempotency_key: idempotencyKey,
      }),
      signal: requestSignal(options.signal),
    });
    httpStatus = res.status;
    receipt = await receiptOf(res, ownerScope);
  } catch (exc) {
    if (options.signal?.aborted) throw exc;
    const recovered = await recoverCommandReceipt(requestId, 8, 200, { ...options, ownerScope });
    if (!recovered) throw exc;
    receipt = recovered;
  }
  receipt = await waitForTerminalReceipt(receipt, "创建对话", { ...options, ownerScope });
  recordTaskReceipt(receipt, "创建对话", "conversation", ownerScope);
  if (receipt.error || (RECEIPT_TERMINAL_STATES.has(receipt.state) && receipt.state !== "completed")) {
    throw conversationCommandErrorFrom(
      receipt.error,
      "创建对话失败",
      {
        httpStatus,
        deduplicated: Boolean(receipt.deduplicated),
      },
    );
  }
  if (receipt.state !== "completed") throw conversationCommandErrorFrom({
    code: "conversation.thread.pending", category: "state", retryable: true,
    message: "对话创建尚未确认完成，请继续恢复原命令；计划中的 ID 暂不能发送",
    detail: { command_id: receipt.command_id, receipt_state: receipt.state, outcome_unknown: true },
  }, "对话创建尚未确认");
  if (!receipt.aggregate?.id) throw new Error("Thread 回执缺少 aggregate id");
  return receipt.aggregate.id;
}

export async function prewarmConversationRuntime(
  input: { adapter_id: string; instance_id: string; credential_id: string; model: string; workspace_id: string },
  signal?: AbortSignal,
): Promise<boolean> {
  const res = await apiFetch("/api/conversation/runtime-prewarm", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(input),
    signal,
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    const error = (body as { error?: { code?: string; message?: string } }).error;
    throw new Error(`${error?.code || `HTTP ${res.status}`}: ${error?.message || "Runtime 预启动失败"}`);
  }
  return Boolean((body as { started?: unknown }).started);
}

export async function sendConversationCommand(
  threadId: string,
  commandType: string,
  payload: Record<string, unknown> = {},
  options: CommandIdOptions = {},
): Promise<CommandReceiptView> {
  const ownerScope = conversationStorageScope();
  const requestId = String(options.commandId || "").trim() || commandId();
  const idempotencyKey = String(options.idempotencyKey || "").trim() || requestId;
  let receipt: CommandReceiptView;
  let httpStatus: number | undefined;
  try {
    const res = await apiFetch(
      `/api/threads/${encodeURIComponent(threadId)}/commands`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          command_type: commandType,
          command_id: requestId,
          idempotency_key: idempotencyKey,
          payload,
        }),
        signal: requestSignal(options.signal),
      },
    );
    httpStatus = res.status;
    receipt = await receiptOf(res, ownerScope);
  } catch (exc) {
    if (options.signal?.aborted) throw exc;
    const recovered = await recoverCommandReceipt(requestId, 8, 200, { ...options, ownerScope });
    if (!recovered) throw exc;
    receipt = recovered;
  }
  recordTaskReceipt(receipt, commandType, "conversation", ownerScope);
  if (receipt.error) {
    throw conversationCommandErrorFrom(
      receipt.error,
      `${commandType} 执行失败`,
      {
        httpStatus,
        deduplicated: Boolean(receipt.deduplicated),
      },
    );
  }
  return receipt;
}

export type ImpactPreviewResponse = {
  mode: string;
  target_turn_id: string;
  target_seq?: number;
  target_text_preview?: string;
  edited?: boolean;
  superseded_turn_ids: string[];
  superseded_seqs?: number[];
  superseded_message_count?: number;
  attachments_affected?: Array<{ artifact_id?: string; sha256?: string; name: string; needs_reselect?: boolean }>;
  workspace?: Record<string, unknown>;
  external_side_effects?: string;
  provider?: Record<string, unknown>;
  guarantees?: Record<string, unknown>;
};

export async function fetchImpactPreview(
  threadId: string,
  turnId: string,
  mode: string,
  options?: { text?: string; fileMode?: string },
): Promise<ImpactPreviewResponse> {
  const params = new URLSearchParams({
    turn_id: turnId,
    mode,
    file_mode: options?.fileMode || "keep_files",
  });
  if (options?.text) params.set("text", options.text);
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}/impact-preview?${params}`,
  );
  if (!res.ok) {
    const body = await res.json().catch(() => ({})) as {
      error?: { message?: string; code?: string };
    };
    throw new Error(
      body.error?.message || body.error?.code || `影响范围预览失败（HTTP ${res.status}）`,
    );
  }
  return (await res.json()) as ImpactPreviewResponse;
}

export async function fetchThreadAudit(
  threadId: string,
): Promise<ConversationView> {
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}?mark_read=false&include_superseded=true`,
    { signal: requestSignal() },
  );
  if (!res.ok) {
    throw new Error(`加载被替代历史失败（HTTP ${res.status}）`);
  }
  return (await res.json()) as ConversationView;
}

export async function uploadConversationFile(
  threadId: string,
  file: File,
  options: CommandIdOptions = {},
): Promise<CommandReceiptView> {
  const ownerScope = conversationStorageScope();
  const requestId = String(options.commandId || "").trim() || commandId();
  const idempotencyKey = String(options.idempotencyKey || "").trim() || requestId;
  const body = new FormData();
  body.append("file", file);
  body.append("command_id", requestId);
  body.append("idempotency_key", idempotencyKey);
  let receipt: CommandReceiptView;
  try {
    const res = await apiFetch(
      `/api/threads/${encodeURIComponent(threadId)}/uploads`,
      { method: "POST", body, signal: requestSignal(options.signal, 60_000) },
    );
    receipt = await receiptOf(res, ownerScope);
  } catch (exc) {
    if (options.signal?.aborted) throw exc;
    const recovered = await recoverCommandReceipt(requestId, 8, 200, { ...options, ownerScope });
    if (!recovered) throw exc;
    receipt = recovered;
  }
  recordTaskReceipt(receipt, `上传文件：${file.name}`, "conversation", ownerScope);
  if (receipt.error) {
    throw conversationCommandErrorFrom(
      receipt.error,
      `上传文件失败：${file.name}`,
      { deduplicated: Boolean(receipt.deduplicated) },
    );
  }
  return receipt;
}

export async function fetchConversationMemory(
  threadId: string,
  query = "",
  includeDeleted = false,
): Promise<ConversationMemorySnapshot> {
  const params = new URLSearchParams({
    include_deleted: includeDeleted ? "true" : "false",
  });
  if (query.trim()) params.set("q", query.trim());
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}/memory?${params}`,
  );
  if (!res.ok) throw new Error(`加载长期记忆失败（HTTP ${res.status}）`);
  return (await res.json()) as ConversationMemorySnapshot;
}

export async function recordConversationMemory(
  threadId: string,
  content: string,
  kind = "note",
): Promise<CommandReceiptView> {
  const ownerScope = conversationStorageScope();
  const command = commandId();
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}/memory`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        content,
        kind,
        consent: true,
        command_id: command,
        idempotency_key: command,
      }),
    },
  );
  const receipt = await receiptOf(res, ownerScope);
  recordTaskReceipt(receipt, "写入长期记忆", "conversation", ownerScope);
  return receipt;
}

export async function deleteConversationMemory(
  threadId: string,
  memoryId: string,
): Promise<CommandReceiptView> {
  const ownerScope = conversationStorageScope();
  const command = commandId();
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}/memory/${encodeURIComponent(memoryId)}`,
    {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        confirm: true,
        reason: "user_requested",
        command_id: command,
        idempotency_key: command,
      }),
    },
  );
  const receipt = await receiptOf(res, ownerScope);
  recordTaskReceipt(receipt, "删除长期记忆", "conversation", ownerScope);
  return receipt;
}


export interface ProviderSessionScan {
  session_id: string;
  adapter_id: string;
  source_path: string;
  title: string;
  message_count: number;
  created_at: string | null;
  import_status: "continuable" | "read_only" | "missing_tools";
  missing_fields: string[];
}

export interface ImportScanResult {
  scans: ProviderSessionScan[];
  base_path: string;
  exists: boolean;
}

export interface ImportApplyResult {
  imported: { session_id: string; thread_id: string }[];
  skipped: { session_id: string; thread_id: string }[];
  failed: { session_id: string; error: string }[];
  total: number;
}

export async function fetchImportScan(params: {
  adapter: "claude" | "codex";
  path?: string;
  limit?: number;
}): Promise<ImportScanResult> {
  const q = new URLSearchParams({ adapter: params.adapter });
  if (params.path) q.set("path", params.path);
  if (params.limit) q.set("limit", String(params.limit));
  const res = await apiFetch(`/api/import/scan?${q.toString()}`);
  if (!res.ok) {
    const payload = await res.json();
    throw new Error(payload.error?.message || payload.detail?.message || payload.detail || `扫描失败（HTTP ${res.status}）`);
  }
  return res.json() as Promise<ImportScanResult>;
}

export async function applyImport(params: {
  adapter_id: "claude" | "codex";
  source_path: string;
  sessions: string[];
  project_id?: string;
  dry_run?: boolean;
}): Promise<ImportApplyResult> {
  const res = await apiFetch("/api/import/apply", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(params),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`import apply failed: ${res.status} ${text}`);
  }
  return res.json() as Promise<ImportApplyResult>;
}

export interface SidebarPreferences {
  version: number;
  pinned_ids: string[];
  thread_order: string[];
  project_order: string[];
  sort_mode: "updated" | "priority" | "manual";
  pinned_sort_mode: "updated" | "priority" | "manual";
  group_mode: "project" | "list";
  settled_at: Record<string, number>;
  snoozed_until: Record<string, number>;
}

function normalizeTimeMap(value: unknown): Record<string, number> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  const result: Record<string, number> = {};
  for (const [key, raw] of Object.entries(value as Record<string, unknown>)) {
    if (typeof raw === "number" && Number.isFinite(raw) && raw > 0) result[key] = raw;
  }
  return result;
}

export function normalizeSidebarPreferences(raw: unknown): SidebarPreferences {
  const data = (raw && typeof raw === "object" ? raw : {}) as Partial<SidebarPreferences>;
  const list = (value: unknown) => (Array.isArray(value) ? value.filter((id): id is string => typeof id === "string") : []);
  const mode = (value: unknown, fallback: SidebarPreferences["sort_mode"]) => (
    value === "updated" || value === "priority" || value === "manual" ? value : fallback
  );
  return {
    version: typeof data.version === "number" ? data.version : 0,
    pinned_ids: list(data.pinned_ids),
    thread_order: list(data.thread_order),
    project_order: list(data.project_order),
    sort_mode: mode(data.sort_mode, "updated"),
    pinned_sort_mode: mode(data.pinned_sort_mode, "manual"),
    group_mode: data.group_mode === "list" ? "list" : "project",
    settled_at: normalizeTimeMap(data.settled_at),
    snoozed_until: normalizeTimeMap(data.snoozed_until),
  };
}

function sidebarPreferencesUrl(scope?: "chat" | "src"): string {
  return scope === "src" ? "/api/sidebar-preferences?scope=src" : "/api/sidebar-preferences";
}

export async function fetchSidebarPreferences(scope?: "chat" | "src"): Promise<SidebarPreferences> {
  const res = await apiFetch(sidebarPreferencesUrl(scope));
  if (!res.ok) throw new Error(`sidebar-preferences GET failed: ${res.status}`);
  return normalizeSidebarPreferences(await res.json());
}

export async function saveSidebarPreferences(
  prefs: Partial<SidebarPreferences> & { version: number },
  scope?: "chat" | "src",
): Promise<{ ok: boolean; prefs: SidebarPreferences }> {
  const res = await apiFetch(sidebarPreferencesUrl(scope), {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(prefs),
  });
  return { ok: res.ok, prefs: normalizeSidebarPreferences(await res.json()) };
}

export type ConversationStreamStatus =
  | "idle"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "auth_required";

type SharedInboxListener = {
  admissionScopeKey: string;
  onStatus: (status: ConversationStreamStatus) => void;
  onSnapshot: (snapshot: ConversationInboxSnapshot) => void;
  onError: (error: unknown) => void;
};

class SharedInboxHub {
  readonly key: string;
  private readonly listeners = new Set<SharedInboxListener>();
  private es: EventSource | null = null;
  private connecting = false;
  private reconnectDelay = 500;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private connectionVersion = 0;
  private status: ConversationStreamStatus = "idle";
  private lastSnapshot: ConversationInboxSnapshot | null = null;
  private legacyEventState = emptyConversationInboxState();
  private lastAdmissionScopeKey = "";
  private disposed = false;

  constructor(key: string) {
    this.key = key;
  }

  private setStatus(next: ConversationStreamStatus) {
    this.status = next;
    for (const listener of this.listeners) {
      listener.onStatus(next);
    }
  }

  private publish(snapshot: ConversationInboxSnapshot) {
    const listChanged = snapshot.threads !== this.lastSnapshot?.threads;
    this.lastSnapshot = snapshot;
    for (const listener of this.listeners) listener.onSnapshot(snapshot);
    if (listChanged) notifyThreadsChanged();
  }

  private async mintTicket(): Promise<
    { status: "ok"; ticket: string } | { status: "auth_required" } | { status: "empty" }
  > {
    try {
      const res = await apiFetch("/api/auth/ticket", { method: "POST" });
      if (res.status === 401 || res.status === 403) return { status: "auth_required" };
      if (!res.ok) return { status: "empty" };
      const data = await res.json().catch(() => ({}));
      const ticket = data?.ticket ? String(data.ticket) : "";
      return ticket ? { status: "ok", ticket } : { status: "empty" };
    } catch {
      return { status: "empty" };
    }
  }

  private async connect(isReconnect: boolean) {
    if (this.disposed || this.listeners.size === 0) return;
    const version = ++this.connectionVersion;
    this.connecting = true;
    this.setStatus(isReconnect ? "reconnecting" : "connecting");
    const ticketResult = await this.mintTicket();
    if (this.disposed || this.listeners.size === 0 || version !== this.connectionVersion) {
      if (version === this.connectionVersion) this.connecting = false;
      return;
    }
    if (ticketResult.status === "auth_required") {
      this.connecting = false;
      this.setStatus("auth_required");
      return;
    }
    const ticket = ticketResult.status === "ok" ? ticketResult.ticket : "";
    const query = new URLSearchParams({ snapshot: "true", updates: "delta" });
    if (ticket) query.set("ticket", ticket);

    this.es?.close();
    const nextEs = new EventSource(apiRequestUrl(`/api/threads/inbox/events?${query}`));
    this.es = nextEs;
    this.connecting = false;

    nextEs.onopen = () => {
      if (this.disposed || this.es !== nextEs) return;
      this.reconnectDelay = 500;
      this.setStatus("connected");
    };
    nextEs.onerror = () => {
      if (this.disposed || this.es !== nextEs) return;
      nextEs.close();
      if (this.es === nextEs) this.es = null;
      this.setStatus("reconnecting");
      if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
      const delay = this.reconnectDelay;
      this.reconnectDelay = Math.min(this.reconnectDelay * 2, 10_000);
      this.reconnectTimer = setTimeout(() => {
        this.reconnectTimer = null;
        void this.connect(true);
      }, delay);
    };
    nextEs.addEventListener("snapshot", (raw) => {
      if (this.disposed || this.es !== nextEs) return;
      try {
        const parsed = JSON.parse((raw as MessageEvent).data) as ConversationInboxSnapshot;
        if (!Array.isArray(parsed.threads) || !Array.isArray(parsed.summaries)) {
          throw new Error("收件箱快照缺少会话或通知摘要");
        }
        const snapshot = sanitizeConversationInboxSnapshot(parsed);
        if (this.lastSnapshot) snapshot.threads = reconcileConversationThreads(this.lastSnapshot.threads, snapshot.threads);
        this.legacyEventState = { ...emptyConversationInboxState(), appliedSeq: snapshot.inbox_seq, brokerEpoch: snapshot.broker_epoch };
        this.publish(snapshot);
      } catch (error) {
        for (const listener of this.listeners) listener.onError(error);
      }
    });
    nextEs.addEventListener("patch", (raw) => {
      if (this.disposed || this.es !== nextEs) return;
      try {
        const patch = JSON.parse((raw as MessageEvent).data) as ConversationInboxPatch;
        const snapshot = applyConversationInboxPatch(this.lastSnapshot, patch);
        if (!snapshot) throw new Error("收件箱变更缺少对应的快照");
        this.publish(snapshot);
      } catch (error) {
        for (const listener of this.listeners) listener.onError(error);
        this.reconnectNow();
      }
    });
    nextEs.addEventListener("event", (raw) => {
      if (this.disposed || this.es !== nextEs || !this.lastSnapshot) return;
      try {
        const event = JSON.parse((raw as MessageEvent).data) as ConversationInboxEvent & { thread?: ConversationThread };
        if (
          isInternalCtfThread(event.thread)
          || isInternalCtfThread({ thread_id: String(event.summary?.thread_id || "") })
        ) {
          return;
        }
        const result = acceptConversationInboxEvent(this.legacyEventState, event);
        if (!result.accepted) return;
        this.legacyEventState = result.state;
        const summaries = this.lastSnapshot.summaries.filter((summary) => summary.thread_id !== event.summary.thread_id);
        summaries.push(event.summary);
        this.publish({
          inbox_seq: result.state.appliedSeq,
          broker_epoch: result.state.brokerEpoch,
          threads: event.thread?.thread_id ? upsertConversationThread(this.lastSnapshot.threads, event.thread) : this.lastSnapshot.threads,
          summaries,
        });
      } catch (error) {
        for (const listener of this.listeners) listener.onError(error);
      }
    });
  }

  subscribe(listener: SharedInboxListener): () => void {
    const scopeChanged = Boolean(
      listener.admissionScopeKey
      && this.lastAdmissionScopeKey
      && listener.admissionScopeKey !== this.lastAdmissionScopeKey,
    );
    if (listener.admissionScopeKey) {
      this.lastAdmissionScopeKey = listener.admissionScopeKey;
    }
    this.listeners.add(listener);
    listener.onStatus(this.status);

    if (scopeChanged) {
      this.reconnectNow();
    } else if (!this.es && !this.connecting && this.reconnectTimer === null && this.status !== "auth_required") {
      void this.connect(false);
    } else if (this.lastSnapshot !== null) {
      queueMicrotask(() => {
        if (!this.disposed && this.listeners.has(listener) && this.lastSnapshot) {
          listener.onSnapshot(this.lastSnapshot);
        }
      });
    }

    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0) {
        this.dispose();
        if (sharedInboxHubs.get(this.key) === this) {
          sharedInboxHubs.delete(this.key);
        }
      }
    };
  }

  retrySnapshot(listener: SharedInboxListener) {
    if (this.disposed || !this.listeners.has(listener)) return;
    listener.onStatus(this.status);
    if (this.status === "connected" && this.lastSnapshot !== null) {
      listener.onSnapshot(this.lastSnapshot);
      return;
    }
    if (!this.es && !this.connecting && this.reconnectTimer === null) {
      void this.connect(true);
    }
  }

  private reconnectNow() {
    this.connectionVersion += 1;
    this.connecting = false;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.es?.close();
    this.es = null;
    this.lastSnapshot = null;
    this.reconnectDelay = 500;
    void this.connect(true);
  }

  private dispose() {
    this.disposed = true;
    this.connectionVersion += 1;
    this.connecting = false;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.es?.close();
    this.es = null;
    this.lastSnapshot = null;
  }
}

const sharedInboxHubs = new Map<string, SharedInboxHub>();

function getSharedInboxHub(key: string): SharedInboxHub {
  let hub = sharedInboxHubs.get(key);
  if (!hub) {
    hub = new SharedInboxHub(key);
    sharedInboxHubs.set(key, hub);
  }
  return hub;
}

export function useConversationThreads(options?: {
  enabled?: boolean;
  cursorStorageKey?: string;
  admissionScopeKey?: string;
  canAdmitInboxEvent?: (cursorKey: string, admissionScopeKey: string) => boolean;
  activeThreadId?: string;
  onInboxEvent?: (event: ConversationInboxEvent) => void | boolean | Promise<void | boolean>;
}) {
  const [threads, setThreads] = useState<ConversationThread[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const enabled = options?.enabled !== false;
  const cursorStorageKey = options?.cursorStorageKey || "";
  const admissionScopeKey = options?.admissionScopeKey || "";
  const [inboxStatus, setInboxStatus] = useState<ConversationStreamStatus>("idle");
  const canAdmitRef = useRef(options?.canAdmitInboxEvent);
  canAdmitRef.current = options?.canAdmitInboxEvent;
  const onInboxEventRef = useRef(options?.onInboxEvent);
  onInboxEventRef.current = options?.onInboxEvent;
  const activeThreadId = options?.activeThreadId || "";
  const listRequestRef = useRef(0);
  const inboxRevisionRef = useRef(0);
  const listMountedRef = useRef(true);

  const refresh = useCallback(async () => {
    const request = ++listRequestRef.current;
    const inboxRevision = inboxRevisionRef.current;
    const isCurrent = () => listMountedRef.current
      && request === listRequestRef.current && inboxRevision === inboxRevisionRef.current;
    try {
      const rows = await fetchConversationThreads("inbox");
      if (!isCurrent()) return;
      setThreads((current) => reconcileConversationThreads(current, rows));
      setError("");
    } catch (exc) {
      if (!isCurrent()) return;
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      if (isCurrent()) setLoading(false);
    }
  }, []);

  useEffect(() => {
    listMountedRef.current = true;
    if (enabled) void refresh();
    return () => { listMountedRef.current = false; listRequestRef.current += 1; };
  }, [refresh, enabled]);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    let saved = emptyNotificationInboxCursor();
    try { if (cursorStorageKey) saved = readNotificationInboxCursor(cursorStorageKey); }
    catch (err) { setError(`通知游标读取失败：${String(err)}`); return; }
    let admitted = saved;
    let receivedThreads: ConversationThread[] | null = null;
    let admissionBlocked = false;
    const hubKey = `${apiRequestUrl("/api/threads/inbox/events")}|${String(currentAuthGeneration())}`;
    const hub = getSharedInboxHub(hubKey);
    const canAdmit = () => !cancelled && canAdmitRef.current?.(cursorStorageKey, admissionScopeKey) !== false;

    const notifications = createNotificationInboxDrain(async (snapshot) => {
      if (!canAdmit() || admissionBlocked) return;
      const { inbox_seq: seq, broker_epoch: epoch, summaries, threads } = snapshot;
      if (epoch === admitted.epoch && seq < admitted.seq) return;
      try {
        for (const event of recoverNotificationInboxSnapshot(admitted, epoch, seq, summaries, threads)) {
          if (!canAdmit()) return;
          if (await onInboxEventRef.current?.(event) === false) {
            throw new Error("notification.admission_incomplete: Snapshot attention was not handed to its owner.");
          }
        }
        if (!canAdmit()) return;
        const cursor = { seq, epoch, summaries: Object.fromEntries(summaries.map(summary => [summary.thread_id, summary])) };
        writeNotificationInboxCursor(cursorStorageKey, cursor);
        admitted = cursor;
      } catch (error) {
        if (!canAdmit()) return;
        admissionBlocked = true;
        notifications.clear();
        setError(`通知交付未完成，游标未提交：${String(error)}`);
        setInboxStatus("reconnecting");
        retryTimer = setTimeout(() => {
          retryTimer = null;
          if (!canAdmit()) return;
          admissionBlocked = false;
          setError("");
          hub.retrySnapshot(listener);
        }, 1000);
      }
    });

    const listener: SharedInboxListener = {
      admissionScopeKey,
      onStatus: (status) => {
        if (!cancelled && !admissionBlocked) setInboxStatus(status);
      },
      onSnapshot: (snapshot) => {
        if (cancelled) return;
        if (receivedThreads !== snapshot.threads) {
          receivedThreads = snapshot.threads;
          inboxRevisionRef.current += 1;
          setThreads((current) => reconcileConversationThreads(current, snapshot.threads));
          setLoading(false);
          if (!admissionBlocked) setError("");
        }
        if (cursorStorageKey && !admissionBlocked && canAdmit()) notifications.offer(snapshot);
      },
      onError: (error) => {
        if (!cancelled && !admissionBlocked) setError(`对话 inbox 无法同步：${String(error)}`);
      },
    };

    const unsubscribe = hub.subscribe(listener);

    return () => {
      cancelled = true;
      notifications.clear();
      if (retryTimer !== null) clearTimeout(retryTimer);
      unsubscribe();
    };
  }, [enabled, cursorStorageKey, admissionScopeKey]);

  useEffect(() => {
    if (!enabled || typeof window === "undefined") return;
    const onThreadsRead = (event: Event) => {
      const detail = (event as CustomEvent<string[]>).detail;
      if (!Array.isArray(detail) || detail.length === 0) return;
      const ids = new Set(detail);
      setThreads((current) => {
        let changed = false;
        const next = current.map((thread) => {
          if (!ids.has(thread.thread_id) || !thread.state.unread) return thread;
          changed = true;
          return {
            ...thread,
            state: {
              ...thread.state,
              unread: false,
              read_stream_seq: thread.state.head_stream_seq,
            },
          };
        });
        return changed ? next : current;
      });
    };
    window.addEventListener("muteki:threads-read", onThreadsRead);
    return () => window.removeEventListener("muteki:threads-read", onThreadsRead);
  }, [enabled]);

  useEffect(() => {
    if (!enabled || typeof window === "undefined") return;
    const onLifecycle = (event: Event) => {
      const detail = (event as CustomEvent<{ threadId?: string; lifecycle?: ThreadControlLifecycle; unread?: boolean }>).detail;
      if (!detail?.threadId || !detail.lifecycle) return;
      setThreads((current) => {
        let changed = false;
        const next = current.map((thread) => {
          if (thread.thread_id !== detail.threadId) return thread;
          changed = true;
          return {
            ...thread,
            control_lifecycle: detail.lifecycle,
            state: typeof detail.unread === "boolean" ? { ...thread.state, unread: detail.unread } : thread.state,
          };
        });
        return changed ? next : current;
      });
    };
    window.addEventListener("muteki:thread-lifecycle", onLifecycle);
    return () => window.removeEventListener("muteki:thread-lifecycle", onLifecycle);
  }, [enabled]);

  useEffect(() => {
    if (!enabled || typeof window === "undefined") return;
    const onRemoved = (event: Event) => {
      const threadId = (event as CustomEvent<{ threadId?: string }>).detail?.threadId;
      if (!threadId) return;
      setThreads((current) => current.some((thread) => thread.thread_id === threadId)
        ? current.filter((thread) => thread.thread_id !== threadId)
        : current);
    };
    window.addEventListener("muteki:thread-removed", onRemoved);
    return () => window.removeEventListener("muteki:thread-removed", onRemoved);
  }, [enabled]);

  useEffect(() => {
    if (!enabled || typeof window === "undefined") return;
    const onPatched = (event: Event) => {
      const detail = (event as CustomEvent<{ threadId?: string; patch?: Partial<ConversationThread> }>).detail;
      if (!detail?.threadId || !detail.patch) return;
      setThreads((current) => current.map((thread) => (
        thread.thread_id === detail.threadId ? { ...thread, ...detail.patch } : thread
      )));
    };
    window.addEventListener("muteki:thread-patched", onPatched);
    return () => window.removeEventListener("muteki:thread-patched", onPatched);
  }, [enabled]);

  const markThreadsRead = useCallback((threadIds: string[]) => {
    const ids = new Set(threadIds);
    if (!ids.size) return;
    setThreads((current) => current.map((thread) => (
      ids.has(thread.thread_id) && thread.state.unread
        ? { ...thread, state: { ...thread.state, unread: false, read_stream_seq: thread.state.head_stream_seq } }
        : thread
    )));
    notifyThreadsRead([...ids]);
    void Promise.allSettled([...ids].map((id) => markConversationThreadRead(id))).then((results) => {
      const failed = results.filter((result) => result.status === "rejected").length;
      if (failed) {
        setError(`${failed} 个对话的已读状态未同步`);
        void refresh();
      }
    });
  }, [refresh]);

  const attentionCount = countThreadAttention(threads, activeThreadId);

  return {
    threads,
    loading,
    error,
    refresh,
    attentionCount,
    inboxStatus,
    markThreadsRead,
  };
}

const RECENT_VIEW_LIMIT = 8;
const recentViews = new Map<string, ConversationView>();
subscribeConversationStorageScope(() => recentViews.clear());

function rememberView(key: string, view: ConversationView) {
  recentViews.delete(key);
  recentViews.set(key, view);
  if (recentViews.size > RECENT_VIEW_LIMIT) {
    const oldest = recentViews.keys().next().value;
    if (oldest !== undefined) recentViews.delete(oldest);
  }
}

function seedConversationStream(view: ConversationView): ConversationStreamState {
  const state = emptyConversationStreamState();
  state.appliedSeq = view.event_watermark;
  if (!view.state.running_turn_id) return state;
  state.events = (view.tools ?? [])
    .filter((tool) => tool.turn_id === view.state.running_turn_id && tool.status === "running")
    .map((tool) => ({
      event_type: "core.tool.started",
      event_id: `persisted-tool:${tool.tool_id}`,
      seq: 0,
      payload: { turn_id: tool.turn_id, tool_call_id: tool.tool_id, name: tool.name, status: tool.status },
    }));
  for (const row of view.messages) {
    if (row.turn_id !== view.state.running_turn_id || row.role !== "assistant" || row.status !== "streaming") continue;
    const start = state.liveText.length;
    state.liveText += row.text;
    state.liveTextRuns.push({ start, end: state.liveText.length, messageId: row.message_id, phase: row.phase ?? "unknown" });
  }
  return state;
}

const STREAM_PUBLISH_INTERVAL_MS = 50;
const REFRESH_DEBOUNCE_MS = 100;
const REFRESH_MAX_WAIT_MS = 1000;
const NO_EVENTS: ConversationEvent[] = [];
const NO_LIVE_TEXT_RUNS: ConversationLiveTextRun[] = [];

export interface ConversationStreamSnapshot {
  threadId: string;
  resumedTurnId: string | null;
  events: ConversationEvent[];
  liveText: string;
  liveTextRuns: ConversationLiveTextRun[];
}

export interface ConversationStreamStore {
  subscribe(listener: () => void): () => void;
  getSnapshot(): ConversationStreamSnapshot;
}

const EMPTY_STREAM_SNAPSHOT: ConversationStreamSnapshot = {
  threadId: "",
  resumedTurnId: null,
  events: NO_EVENTS,
  liveText: "",
  liveTextRuns: NO_LIVE_TEXT_RUNS,
};

function createConversationStreamStore() {
  let snapshot = EMPTY_STREAM_SNAPSHOT;
  const listeners = new Set<() => void>();
  return {
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    getSnapshot: () => snapshot,
    publish(next: ConversationStreamSnapshot) {
      if (
        next.threadId === snapshot.threadId
        && next.resumedTurnId === snapshot.resumedTurnId
        && next.events === snapshot.events
        && next.liveText === snapshot.liveText
        && next.liveTextRuns === snapshot.liveTextRuns
      ) return;
      snapshot = next;
      for (const listener of [...listeners]) listener();
    },
  };
}

export function useConversationStream(
  store: ConversationStreamStore,
  viewThreadId: string | null | undefined,
): ConversationStreamSnapshot {
  const snapshot = useSyncExternalStore(store.subscribe, store.getSnapshot, store.getSnapshot);
  if (viewThreadId && snapshot.threadId !== viewThreadId) return EMPTY_STREAM_SNAPSHOT;
  return snapshot;
}

export function useConversationStreamSelector<T>(
  store: ConversationStreamStore,
  viewThreadId: string | null | undefined,
  select: (snapshot: ConversationStreamSnapshot) => T,
): T {
  const read = () => {
    const snapshot = store.getSnapshot();
    return select(viewThreadId && snapshot.threadId !== viewThreadId ? EMPTY_STREAM_SNAPSHOT : snapshot);
  };
  return useSyncExternalStore(store.subscribe, read, read);
}

export function useConversation(threadId: string) {
  const storageScope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => "");
  const owner = currentAuthGeneration();
  const key = threadId ? JSON.stringify([owner, storageScope, currentAuthScope(), currentServiceOrigin(), API, threadId]) : "";
  const activeKeyRef = useRef(key);
  activeKeyRef.current = key;
  const [entry, setEntry] = useState<{ key: string; view: ConversationView | null } | null>(null);
  const view = entry?.key === key ? entry.view : key ? recentViews.get(key) ?? null : null;
  const setView = useCallback((update: (previous: ConversationView | null) => ConversationView | null) => {
    setEntry((previous) => {
      if (activeKeyRef.current !== key) return previous;
      const prior = previous?.key === key ? previous.view : recentViews.get(key) ?? null;
      const next = update(prior);
      return previous?.key === key && prior === next ? previous : { key, view: next };
    });
  }, [key]);
  const [streamStore] = useState(createConversationStreamStore);
  const [connection, setConnection] = useState<{ key: string; status: ConversationStreamStatus } | null>(null);
  const streamStatus = connection?.key === key ? connection.status : threadId ? "connecting" : "idle";
  const setStreamStatus = useCallback((status: ConversationStreamStatus) => {
    setConnection((previous) => previous?.key === key && previous.status === status ? previous : { key, status });
  }, [key]);
  const connected = streamStatus === "connected";
  const [failure, setFailure] = useState({ key: "", message: "" });
  const error = failure.key === key ? failure.message : "";
  const loading = Boolean(threadId && !view && !error);
  const setError = useCallback((message: string) => {
    if (activeKeyRef.current === key) setFailure((previous) => previous.key === key && previous.message === message ? previous : { key, message });
  }, [key]);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [auditLanding, setAuditLanding] = useState<{
    threadId: string; messageId: string;
    messages: ConversationMessage[]; turns: ConversationTurn[];
  } | null>(null);
  const refreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const olderAbortRef = useRef<AbortController | null>(null);
  const pendingAroundRef = useRef<{
    threadId: string;
    messageId: string;
    messages: ConversationMessage[];
    turns: ConversationTurn[];
    messages_page: ConversationMessagesPage;
  } | null>(null);
  const aroundEpochRef = useRef(0);
  const pendingRefresh = useRef<{ key: string; controller: AbortController; promise: Promise<void> } | null>(null);
  const installStreamBase = useRef<((view: ConversationView) => void) | null>(null);
  const freshView = useRef<{ key: string; view: ConversationView } | null>(null);
  const readMark = useRef<{ key: string; through: number; controller: AbortController } | null>(null);
  const nativeReadRef = useRef<{ bridge: ReturnType<typeof desktopChatBridge>; state: DesktopNativeState | null; authGeneration: number }>({ bridge: undefined, state: null, authGeneration: -1 });
  const activeThreadIdRef = useRef(threadId);
  activeThreadIdRef.current = threadId;

  const consumePendingAround = useCallback((base: ConversationView | null): ConversationView | null => {
    if (!base) return base;
    const pending = pendingAroundRef.current;
    if (!pending || pending.threadId !== base.thread.thread_id) return base;
    pendingAroundRef.current = null;
    const superseded = collectSupersededTurnIds([
      ...base.turns,
      ...pending.turns,
      ...(base.superseded_turns || []),
    ], base.superseded_turns || []);
    const cleaned = omitSupersededBranchRows(
      pending.messages,
      pending.turns,
      superseded,
    );
    if (
      cleaned.messages.every((message) => message.message_id !== pending.messageId)
    ) {
      return base;
    }
    const messages = cleaned.messages;
    const turns = mergeTurnLists(
      omitSupersededBranchRows(base.messages, base.turns, superseded).turns,
      cleaned.turns,
    );
    return {
      ...base,
      messages,
      turns,
      messages_page: {
        ...pending.messages_page,
        oldest_stream_seq: messages[0] ? messageSeq(messages[0]) : null,
        newest_stream_seq: messages.length
          ? messageSeq(messages[messages.length - 1])
          : null,
        oldest_message_id: messages[0]?.message_id ?? null,
        newest_message_id: messages.length
          ? messages[messages.length - 1].message_id
          : null,
        around_message_id: pending.messageId,
      },
    };
  }, []);

  const markVisibleRead = useCallback(() => {
    const entry = freshView.current;
    if (entry?.key !== key || !entry.view.state.unread) return;
    const native = nativeReadRef.current;
    const bridge = desktopChatBridge();
    if (!isConversationForeground(native.state) || (bridge && (native.bridge !== bridge
      || native.authGeneration !== owner || !nativeThreadVisible(native.state, threadId)))) return;
    const through = entry.view.watermark;
    if (readMark.current?.key === key && readMark.current.through >= through) return;
    readMark.current?.controller.abort();
    const controller = new AbortController();
    const mark = { key, through, controller };
    readMark.current = mark;
    void markConversationThreadRead(threadId, controller.signal, through).catch((exc) => {
      if (readMark.current === mark) readMark.current = null;
      if (!controller.signal.aborted) setError(exc instanceof Error ? exc.message : String(exc));
    });
  }, [key, owner, setError, threadId]);

  const refresh = useCallback(() => {
    if (!threadId) return Promise.resolve();
    if (pendingRefresh.current?.key === key) return pendingRefresh.current.promise;
    setError("");
    const controller = new AbortController();
    const isCurrent = () => !controller.signal.aborted && activeKeyRef.current === key && owner === currentAuthGeneration();
    const promise = (async () => {
      try {
        const next = await fetchConversationView(threadId, false, 24, { signal: controller.signal, timeline: true });
        if (!isCurrent()) return;
        freshView.current = { key, view: next };
        rememberView(key, next);
        setView((previous) => consumePendingAround(previous === next ? next : mergeConversationView(previous, next)));
        setError("");
        installStreamBase.current?.(next);
        markVisibleRead();
      } catch (exc) {
        if (!isCurrent()) return;
        const status = (exc as { httpStatus?: number })?.httpStatus;
        if (status === 404 || status === 403 || status === 401) {
          recentViews.delete(key);
          freshView.current = null;
          setView(() => null);
        }
        setError(exc instanceof Error ? exc.message : String(exc));
      } finally {
        if (pendingRefresh.current?.controller === controller) pendingRefresh.current = null;
      }
    })();
    pendingRefresh.current = { key, controller, promise };
    return promise;
  }, [consumePendingAround, key, markVisibleRead, owner, setError, setView, threadId]);

  useEffect(() => {
    if (!threadId) return;
    const bridge = desktopChatBridge();
    let cancelled = false;
    let nativeRevision = 0;
    nativeReadRef.current = { bridge, state: null, authGeneration: owner };
    const abortReadMark = () => {
      readMark.current?.controller.abort();
      readMark.current = null;
    };
    const acceptNativeState = (state: DesktopNativeState | null) => {
      if (cancelled || owner !== currentAuthGeneration()) return;
      const prior = nativeReadRef.current.state;
      if (state && prior?.transportOrigin === state.transportOrigin
        && prior?.serviceId === state.serviceId && prior?.identityId === state.identityId
        && Number.isSafeInteger(prior?.connectionVersion) && Number.isSafeInteger(state.connectionVersion)
        && (state.connectionVersion || 0) < (prior?.connectionVersion || 0)) return;
      nativeRevision += 1;
      nativeReadRef.current = { bridge, state, authGeneration: owner };
      if (!nativeThreadVisible(state, threadId) || !isConversationForeground(state)) abortReadMark();
      else markVisibleRead();
    };
    const handleVisibility = () => {
      if (isConversationForeground(nativeReadRef.current.state)) markVisibleRead();
      else abortReadMark();
    };
    window.addEventListener("focus", handleVisibility);
    window.addEventListener("blur", handleVisibility);
    document.addEventListener("visibilitychange", handleVisibility);
    const unsubscribeNative = bridge?.onState?.(acceptNativeState);
    if (bridge) {
      if (!bridge.getState || !bridge.onState) {
        setError("桌面前台状态接口不可用，已暂停自动标记已读。");
      } else {
        const revision = nativeRevision;
        void bridge.getState().then((state) => {
          if (revision === nativeRevision) acceptNativeState(state);
        }).catch((exc) => {
          if (!cancelled && revision === nativeRevision) setError(`桌面前台状态获取失败：${exc instanceof Error ? exc.message : String(exc)}`);
        });
      }
    }
    return () => {
      cancelled = true;
      abortReadMark();
      nativeReadRef.current = { bridge: undefined, state: null, authGeneration: -1 };
      window.removeEventListener("focus", handleVisibility);
      window.removeEventListener("blur", handleVisibility);
      document.removeEventListener("visibilitychange", handleVisibility);
      unsubscribeNative?.();
    };
  }, [markVisibleRead, owner, setError, threadId]);

  const viewRef = useRef<ConversationView | null>(null);
  viewRef.current = view;
  const loadingOlderRef = useRef(false);

  const loadOlderMessages = useCallback(async () => {
    if (!threadId || loadingOlderRef.current) return false;
    const current = viewRef.current;
    if (!current?.messages_page?.has_more_before) return false;
    const oldest = current.messages_page.oldest_stream_seq;
    if (oldest == null) return false;
    const requestedThreadId = threadId;
    const epoch = aroundEpochRef.current;
    olderAbortRef.current?.abort();
    const controller = new AbortController();
    olderAbortRef.current = controller;
    loadingOlderRef.current = true;
    setLoadingOlder(true);
    try {
      const page = await fetchConversationMessagesPage(requestedThreadId, {
        beforeStreamSeq: oldest,
        limit: current.messages_page.limit || 50,
        signal: controller.signal,
      });
      if (activeThreadIdRef.current !== requestedThreadId || aroundEpochRef.current !== epoch || controller.signal.aborted) return false;
      setView((previous) => {
        if (!previous || previous.thread.thread_id !== requestedThreadId || aroundEpochRef.current !== epoch) {
          return previous;
        }
        const superseded = collectSupersededTurnIds([
          ...previous.turns,
          ...page.turns,
          ...(previous.superseded_turns || []),
        ], previous.superseded_turns || []);
        const cleanedPage = omitSupersededBranchRows(
          page.messages,
          page.turns,
          superseded,
        );
        const cleanedPrev = omitSupersededBranchRows(
          previous.messages,
          previous.turns,
          superseded,
        );
        const messages = mergeMessageLists(cleanedPage.messages, cleanedPrev.messages);
        const turns = mergeTurnLists(cleanedPrev.turns, cleanedPage.turns);
        return {
          ...previous,
          messages,
          turns,
          messages_page: {
            limit: page.messages_page.limit,
            oldest_stream_seq: messages[0] ? messageSeq(messages[0]) : null,
            newest_stream_seq: messages.length
              ? messageSeq(messages[messages.length - 1])
              : null,
            oldest_message_id: messages[0]?.message_id ?? null,
            newest_message_id: messages.length
              ? messages[messages.length - 1].message_id
              : null,
            has_more_before: Boolean(page.messages_page.has_more_before),
            has_more_after: Boolean(previous.messages_page?.has_more_after),
          },
        };
      });
      return true;
    } catch (exc) {
      if (controller.signal.aborted) return false;
      if (activeThreadIdRef.current !== requestedThreadId) return false;
      if (aroundEpochRef.current !== epoch) return false;
      setError(exc instanceof Error ? exc.message : String(exc));
      return false;
    } finally {
      if (olderAbortRef.current === controller) {
        olderAbortRef.current = null;
        loadingOlderRef.current = false;
        if (activeThreadIdRef.current === requestedThreadId) setLoadingOlder(false);
      }
    }
  }, [setError, setView, threadId]);

  useEffect(() => {
    olderAbortRef.current?.abort();
    olderAbortRef.current = null;
    loadingOlderRef.current = false;
    pendingAroundRef.current = null;
    aroundEpochRef.current += 1;
    setAuditLanding(null);
    setLoadingOlder(false);
    const cached = recentViews.get(key);
    const streamRef = { current: cached ? seedConversationStream(cached) : emptyConversationStreamState() };
    let resumedTurnId: string | null = null;
    const publish = () => streamStore.publish({
      threadId,
      resumedTurnId,
      events: streamRef.current.events as ConversationEvent[],
      liveText: streamRef.current.liveText,
      liveTextRuns: streamRef.current.liveTextRuns,
    });
    publish();
    if (!threadId) return;

    let cancelled = false;
    let es: EventSource | null = null;
    let reconnectDelay = 500;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let connectionVersion = 0;
    let snapshotEventWatermark = 0;
    let pendingEvents: ConversationEvent[] = [];
    let publishFrame: number | null = null;
    let publishTimer: ReturnType<typeof setTimeout> | null = null;
    let lastPublishAt = 0;
    let refreshInFlight = false;
    let refreshQueued = false;
    let refreshRequestedAt = 0;

    const isCurrentThread = () => (
      !cancelled && activeKeyRef.current === key
    );

    const runRefresh = () => {
      refreshTimer.current = null;
      refreshRequestedAt = 0;
      if (cancelled) return;
      if (refreshInFlight) {
        refreshQueued = true;
        return;
      }
      const pending = pendingRefresh.current?.key === key ? pendingRefresh.current.promise : null;
      refreshInFlight = true;
      const update = pending ? pending.then(() => { if (isCurrentThread()) return refresh(); }) : refresh();
      void update.finally(() => {
        refreshInFlight = false;
        if (refreshQueued && !cancelled) {
          refreshQueued = false;
          scheduleRefresh();
        }
      });
    };
    function scheduleRefresh() {
      const now = Date.now();
      if (!refreshRequestedAt) refreshRequestedAt = now;
      if (refreshTimer.current) clearTimeout(refreshTimer.current);
      const wait = Math.max(0, Math.min(REFRESH_DEBOUNCE_MS, refreshRequestedAt + REFRESH_MAX_WAIT_MS - now));
      refreshTimer.current = setTimeout(runRefresh, wait);
    }

    const flushStream = () => {
      publishFrame = null;
      publishTimer = null;
      if (!isCurrentThread()) return;
      const batch = pendingEvents;
      pendingEvents = [];
      if (!batch.length) return;
      lastPublishAt = performance.now();
      const result = acceptConversationStreamEvents(streamRef.current, batch);
      if (!result.acceptedCount) return;
      streamRef.current = result.state;
      publish();
      if (
        result.refreshEvents.some((event) => (Number(event.seq) || 0) > snapshotEventWatermark)
        || batch.some((event) => (Number(event.seq) || 0) > snapshotEventWatermark && carriesContextWindow(event))
      ) {
        scheduleRefresh();
      }
    };
    const requestFlushFrame = () => {
      publishTimer = null;
      if (document.hidden) flushStream();
      else publishFrame = requestAnimationFrame(flushStream);
    };
    const queueStreamEvent = (event: ConversationEvent) => {
      pendingEvents.push(event);
      if (document.hidden && publishFrame !== null) {
        cancelAnimationFrame(publishFrame);
        publishFrame = null;
      }
      if (publishFrame !== null || publishTimer !== null) return;
      const wait = document.hidden && readChatPreferences().inactiveFrameThrottleDisabled ? 0 : STREAM_PUBLISH_INTERVAL_MS - (performance.now() - lastPublishAt);
      if (wait > 0 || document.hidden) publishTimer = setTimeout(requestFlushFrame, Math.max(0, wait));
      else publishFrame = requestAnimationFrame(flushStream);
    };

    const mintTicket = async (): Promise<
      { status: "ok"; ticket: string } | { status: "auth_required" } | { status: "empty" }
    > => {
      try {
        const res = await apiFetch("/api/auth/ticket", { method: "POST" });
        if (res.status === 401 || res.status === 403) return { status: "auth_required" };
        if (!res.ok) return { status: "empty" };
        const data = await res.json().catch(() => ({}));
        const ticket = data?.ticket ? String(data.ticket) : "";
        return ticket ? { status: "ok", ticket } : { status: "empty" };
      } catch {
        return { status: "empty" };
      }
    };

    const connect = async (isReconnect: boolean) => {
      const version = ++connectionVersion;
      if (isCurrentThread()) {
        setStreamStatus(isReconnect ? "reconnecting" : "connecting");

      }
      const ticketResult = await mintTicket();
      if (cancelled || version !== connectionVersion) return;
      if (!isCurrentThread()) return;
      if (ticketResult.status === "auth_required") {

        setStreamStatus("auth_required");
        return;
      }
      const ticket = ticketResult.status === "ok" ? ticketResult.ticket : "";

      const query = new URLSearchParams({ snapshot: "false", view: "timeline", browser_host: browserControlHost() });
      const resumeAfter = pendingEvents.reduce(
        (max, event) => Math.max(max, Number(event.seq) || 0),
        streamRef.current.appliedSeq,
      );
      query.set("after", String(resumeAfter));
      if (ticket) query.set("ticket", ticket);

      const nextEs = new EventSource(
        `${API}/api/threads/${encodeURIComponent(threadId)}/events?${query}`,
      );
      nextEs.addEventListener("browser_request", (raw) => {
        if (es !== nextEs) return;
        if (!isCurrentThread()) {
          rejectBrowserRequest(threadId, (raw as MessageEvent).data, "browser.thread_inactive", "此客户端已不再显示这个对话");
          return;
        }
        handleBrowserRequest(threadId, (raw as MessageEvent).data);
      });
      es = nextEs;

      nextEs.onopen = () => {
        if (!isCurrentThread() || es !== nextEs) return;
        reconnectDelay = 500;

        setStreamStatus("connected");
        if (isReconnect) void refresh();
      };
      nextEs.onerror = () => {
        if (cancelled || es !== nextEs) return;
        nextEs.close();
        if (es === nextEs) es = null;

        setStreamStatus("reconnecting");
        const delay = reconnectDelay;
        reconnectDelay = Math.min(reconnectDelay * 2, 10_000);
        reconnectTimer = setTimeout(() => {
          reconnectTimer = null;
          void connect(true);
        }, delay);
      };
      const onViewChanged = () => {
        if (isCurrentThread() && es === nextEs) scheduleRefresh();
      };
      nextEs.addEventListener("changed", onViewChanged);
      nextEs.addEventListener("snapshot", onViewChanged);
      nextEs.addEventListener("deleted", () => {
        if (!isCurrentThread() || es !== nextEs) return;
        nextEs.close();
        es = null;
        recentViews.delete(key);
        freshView.current = null;
        pendingRefresh.current?.controller.abort();
        setView(() => null);
        setError(threadLoadFailureMessage(404));
        setStreamStatus("idle");
      });
      nextEs.addEventListener("event", (raw) => {
        if (!isCurrentThread() || es !== nextEs) return;
        try {
          queueStreamEvent(JSON.parse((raw as MessageEvent).data) as ConversationEvent);
        } catch {
          setError("对话事件无法解析");
        }
      });
    };

    const install = (next: ConversationView) => {
      if (!isCurrentThread()) return;
      snapshotEventWatermark = Math.max(snapshotEventWatermark, next.event_watermark);
      if (connectionVersion > 0) return;
      resumedTurnId = next.state.running_turn_id || null;
      streamRef.current = seedConversationStream(next);
      publish();
      void connect(false);
    };
    installStreamBase.current = install;
    void refresh();

    return () => {
      cancelled = true;
      if (installStreamBase.current === install) installStreamBase.current = null;
      if (pendingRefresh.current?.key === key) {
        pendingRefresh.current.controller.abort();
        pendingRefresh.current = null;
      }
      connectionVersion += 1;
      if (reconnectTimer !== null) clearTimeout(reconnectTimer);
      if (publishFrame !== null) cancelAnimationFrame(publishFrame);
      if (publishTimer !== null) clearTimeout(publishTimer);
      es?.close();
      es = null;
      pendingEvents = [];
      streamRef.current = emptyConversationStreamState();
      streamStore.publish(EMPTY_STREAM_SNAPSHOT);
      if (refreshTimer.current) clearTimeout(refreshTimer.current);
      refreshTimer.current = null;
      olderAbortRef.current?.abort();
      olderAbortRef.current = null;
    };
  }, [key, refresh, setError, setStreamStatus, setView, streamStore, threadId]);

  const jumpToLatestMessages = useCallback(async () => {
    if (!threadId) return false;
    const epoch = ++aroundEpochRef.current;
    olderAbortRef.current?.abort();
    pendingAroundRef.current = null;
    const current = viewRef.current;
    const requestedThreadId = threadId;
    try {
      const page = await fetchConversationMessagesPage(requestedThreadId, {
        limit: current?.messages_page?.limit || 50,
      });
      if (activeThreadIdRef.current !== requestedThreadId) return false;
      if (aroundEpochRef.current !== epoch) return false;
      setView((previous) => {
        if (aroundEpochRef.current !== epoch) return previous;
        if (!previous || previous.thread.thread_id !== requestedThreadId) {
          return previous;
        }
        const pruned = pruneSupersededBranch(
          previous.turns,
          previous.messages,
          page.turns,
          page.messages,
          previous.statistics?.turn_count,
        );
        return {
          ...previous,
          messages: page.messages,
          turns: mergeTurnLists(pruned.turns, page.turns),
          messages_page: {
            ...page.messages_page,
            around_message_id: undefined,
            has_more_after: Boolean(page.messages_page.has_more_after),
          },
        };
      });
      return true;
    } catch (exc) {
      if (activeThreadIdRef.current !== requestedThreadId) return false;
      if (aroundEpochRef.current !== epoch) return false;
      setError(exc instanceof Error ? exc.message : String(exc));
      return false;
    }
  }, [setError, setView, threadId]);

  const ensureMessageVisible = useCallback(async (messageId: string): Promise<"ok" | "missing" | "cancelled" | "superseded"> => {
    const target = String(messageId || "").trim();
    if (!threadId || !target) return "missing";
    const epoch = ++aroundEpochRef.current;
    olderAbortRef.current?.abort();
    pendingAroundRef.current = null;
    const isLive = (): boolean => (
      aroundEpochRef.current === epoch
      && activeThreadIdRef.current === threadId
    );

    const messageInActiveView = (): boolean => {
      const active = viewRef.current;
      return Boolean(
        active
        && active.thread.thread_id === threadId
        && active.messages.some((message) => message.message_id === target),
      );
    };

    if (messageInActiveView()) return "ok";

    const requestedThreadId = threadId;
    try {
      const page = await fetchConversationMessagesPage(requestedThreadId, {
        aroundMessageId: target,
        limit: viewRef.current?.messages_page?.limit || 50,
      });
      if (!isLive()) {
        return activeThreadIdRef.current !== requestedThreadId ? "missing" : "cancelled";
      }
      const foundRaw = page.messages.some((message) => message.message_id === target);
      if (!foundRaw) {
        console.warn("[ensureMessageVisible] around page missing target", { requestedThreadId, target });
        return "missing";
      }
      const active = viewRef.current;
      const superseded = collectSupersededTurnIds([
        ...(active && active.thread.thread_id === requestedThreadId ? active.turns : []),
        ...page.turns,
        ...((active && active.thread.thread_id === requestedThreadId
          ? active.superseded_turns
          : undefined) || []),
      ], active?.thread.thread_id === requestedThreadId ? active.superseded_turns || [] : []);
      const cleaned = omitSupersededBranchRows(page.messages, page.turns, superseded);
      const found = cleaned.messages.some((message) => message.message_id === target);
      if (!found) {
        console.warn("[ensureMessageVisible] around target superseded; skip remix", {
          requestedThreadId,
          target,
        });
        if (!isLive()) return "cancelled";
        setAuditLanding({ threadId: requestedThreadId, messageId: target,
          messages: page.messages.filter((message) => superseded.has(String(message.turn_id || ""))),
          turns: page.turns.filter((turn) => superseded.has(turn.turn_id)) });
        return "superseded";
      }

      const pending = {
        threadId: requestedThreadId,
        messageId: target,
        messages: cleaned.messages,
        turns: cleaned.turns,
        messages_page: {
          ...page.messages_page,
          oldest_stream_seq: cleaned.messages[0]
            ? messageSeq(cleaned.messages[0])
            : page.messages_page.oldest_stream_seq,
          newest_stream_seq: cleaned.messages.length
            ? messageSeq(cleaned.messages[cleaned.messages.length - 1])
            : page.messages_page.newest_stream_seq,
          oldest_message_id: cleaned.messages[0]?.message_id
            ?? page.messages_page.oldest_message_id,
          newest_message_id: cleaned.messages.length
            ? cleaned.messages[cleaned.messages.length - 1].message_id
            : page.messages_page.newest_message_id,
        },
      };
      pendingAroundRef.current = pending;

      for (let i = 0; i < 40; i++) {
        if (!isLive()) {
          if (pendingAroundRef.current === pending) pendingAroundRef.current = null;
          return activeThreadIdRef.current !== requestedThreadId ? "missing" : "cancelled";
        }
        if (viewRef.current?.thread.thread_id === requestedThreadId) {
          setView((currentView) => {
            if (aroundEpochRef.current !== epoch) return currentView;
            if (!currentView || currentView.thread.thread_id !== requestedThreadId) {
              return currentView;
            }
            if (pendingAroundRef.current === pending) {
              pendingAroundRef.current = null;
            }
            const liveSuperseded = collectSupersededTurnIds([
              ...currentView.turns,
              ...pending.turns,
              ...(currentView.superseded_turns || []),
            ], currentView.superseded_turns || []);
            const liveClean = omitSupersededBranchRows(
              pending.messages,
              pending.turns,
              liveSuperseded,
            );
            if (
              liveClean.messages.every((message) => message.message_id !== target)
            ) {
              return currentView;
            }
            return {
              ...currentView,
              messages: liveClean.messages,
              turns: mergeTurnLists(
                omitSupersededBranchRows(
                  currentView.messages,
                  currentView.turns,
                  liveSuperseded,
                ).turns,
                liveClean.turns,
              ),
              messages_page: {
                ...pending.messages_page,
                around_message_id: target,
              },
            };
          });
          break;
        }
        await new Promise((resolve) => setTimeout(resolve, 50));
      }

      for (let i = 0; i < 40; i++) {
        if (!isLive()) {
          if (pendingAroundRef.current === pending) pendingAroundRef.current = null;
          return activeThreadIdRef.current !== requestedThreadId ? "missing" : "cancelled";
        }
        if (messageInActiveView()) return "ok";
        if (
          pendingAroundRef.current === pending
          && viewRef.current?.thread.thread_id === requestedThreadId
        ) {
          setView((currentView) => (
            aroundEpochRef.current === epoch
              ? consumePendingAround(currentView)
              : currentView
          ));
        }
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      console.warn("[ensureMessageVisible] target not in view after apply", {
        requestedThreadId,
        target,
        viewThread: viewRef.current?.thread.thread_id,
        count: viewRef.current?.messages.length,
        has: viewRef.current?.messages.some((m) => m.message_id === target),
        pending: Boolean(pendingAroundRef.current),
      });
      return messageInActiveView() ? "ok" : "missing";
    } catch (exc) {
      console.warn("[ensureMessageVisible] error", exc);
      if (pendingAroundRef.current?.messageId === target) {
        pendingAroundRef.current = null;
      }
      if (activeThreadIdRef.current !== requestedThreadId) return "missing";
      if (aroundEpochRef.current !== epoch) return "cancelled";
      return "missing";
    }
  }, [consumePendingAround, setView, threadId]);

  return {
    view,
    stream: streamStore as ConversationStreamStore,
    connected,
    streamStatus,
    loading,
    loadingOlder,
    error,
    auditLanding,
    refresh,
    loadOlderMessages,
    jumpToLatestMessages,
    ensureMessageVisible,
  };
}
