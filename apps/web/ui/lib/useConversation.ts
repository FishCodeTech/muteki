"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  rethrowNetworkLoadFailure,
  threadLoadFailureMessage,
  throwThreadLoadFailure,
} from "@/lib/threadLoadFailure";
import {
  acceptConversationStreamEvent,
  emptyConversationStreamState,
} from "./conversationStreamReducer";
import {
  acceptConversationInboxEvent,
  emptyConversationInboxState,
  upsertConversationThread,
} from "./conversationInbox";
import { countThreadAttention } from "./threadAttention";
import type { ConversationInboxEvent } from "./threadNotifications";
import { API, apiFetch } from "./useRun";
import { recordTaskReceipt } from "./task-center";
import { conversationStorageScope } from "./conversationStorageScope";
import { conversationCommandErrorFrom } from "./sendIntentStore";

export type ThreadMode =
  | "conversation"
  | "single_task"
  | "competition"
  | "management";

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
  state: ConversationState;
}

export interface ConversationProject {
  project_id: string;
  name: string;
  description?: string;
  workspace_id: string;
  workspace_kind: "local";
  root_path: string;
  settings?: Record<string, unknown>;
  created_at?: string;
  updated_at?: string;
}

export interface ConversationDirectorySelection {
  path: string;
  name: string;
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
  usage?: Record<string, unknown>;
  last_error?: Record<string, unknown>;
  unread?: boolean;
  head_stream_seq?: number;
  read_stream_seq?: number;
  current_generation?: number;
  queue_count?: number;
  queue_paused?: boolean;
  queue_pause_reason?: string;
  queue_revision?: number;
  queue_failed_item_id?: string | null;
}

export interface ConversationMessage {
  message_id: string;
  turn_id?: string | null;
  role: "user" | "assistant" | "system" | "tool" | string;
  source_provider?: string;
  source_role?: string;
  source_content?: unknown;
  kind?: "message" | "steer" | string;
  text: string;
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
  status: string;
  text: string;
  attachments?: string[];
  capability_refs?: Array<Record<string, unknown>>;
  runtime_snapshot?: { adapter_id?: string; instance_id?: string; credential_id?: string; model?: string; effort?: string; access_mode?: string } | null;
  retry_of_turn_id?: string | null;
  error?: Record<string, unknown>;
  usage?: Record<string, unknown>;
  created_at?: string;
  completed_at?: string;
}

export interface ConversationQueueItem {
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
  updated_at?: string;
}

export interface ConversationAgentNode {
  agent_id: string;
  parent_id?: string | null;
  title: string;
  model?: string | null;
  turn_id?: string | null;
  message_id?: string | null;
  call_id?: string | null;
  status: string;
  request?: string | null;
  result?: string | null;
  error?: string | null;
  updated_at?: string;
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
  /** Consumption coverage: missing ≠ explicit zero. Independent of context window. */
  token_coverage?: "missing" | "partial" | "complete";
}

/** C27: Context-window fuel gauge reported by the active Runtime. */
export interface ContextWindowState {
  /** Tokens currently in the context window. null = unknown. */
  total: number | null;
  /** Model context-window limit. null = unknown / not reported. */
  limit: number | null;
  /** Breakdown zones: [{label, tokens}] */
  zones: Array<{ label: string; tokens: number }>;
  /** Compaction records: [{summary, tokens_before, tokens_after, occurred_at}] */
  compacted: Array<{
    summary?: string;
    tokens_before?: number;
    tokens_after?: number;
    occurred_at?: string;
  }>;
  /** idle | running | done | failed */
  compact_status: "idle" | "running" | "done" | "failed";
  /** ISO timestamp of last update. */
  updated_at?: string;
  /** Event source tag: usage_event | event | manual_compact | … */
  source?: string;
}

export interface ConversationView {
  thread: Omit<ConversationThread, "state">;
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
    access_mode?: string;
    /** Legacy fields are returned only for historical threads. */
    permission_mode?: string;
    sandbox_mode?: string;
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
    sha256: string;
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
  superseded_turns?: ConversationTurn[];
  superseded_messages?: ConversationMessage[];
  queue: ConversationQueueItem[];
  statistics?: ConversationStatistics;
  /** C27: Latest known context-window fuel gauge from the active Runtime. */
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
}

export interface RuntimeInstance {
  key: string;
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
  reasoning?: {
    supported?: boolean;
    levels?: string[];
    default?: string;
    kind?: string;
    source?: string;
  };
}

/**
 * Global credential projection consumed by Conversation.
 *
 * The canonical backend endpoint is ``GET /api/settings/credentials``.  Host
 * logins are first-class virtual rows whose id is ``system:<engine>``; stored
 * accounts use ``account:<account_id>``.  Secret material is intentionally not
 * part of this browser contract.
 *
 * Conversation only selects Worker CLI engines. HTTP ``api`` endpoints and
 * empty ``unknown`` account dirs are not Agents.
 */
export const CONVERSATION_WORKER_ENGINES = [
  "codex", "claude", "cursor", "grok", "opencode", "pi", "kimi", "omp", "devin",
] as const;

export type ConversationWorkerEngine = (typeof CONVERSATION_WORKER_ENGINES)[number];

const CONVERSATION_WORKER_ENGINE_SET = new Set<string>(CONVERSATION_WORKER_ENGINES);

export function isConversationWorkerEngine(engine: string): boolean {
  return CONVERSATION_WORKER_ENGINE_SET.has(engine);
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
  present: boolean;
  status: string;
  status_detail?: string;
  discovery_code?: string;
  /** Models proven by a successful chat or an explicit credential test. */
  models: ConversationCredentialModel[];
  /** Catalog / declared models not yet proven with this credential. */
  candidate_models: ConversationCredentialModel[];
  default_model?: string;
  last_test?: Record<string, unknown> | null;
  usage?: Array<Record<string, unknown>>;
}

export interface ConversationEvent {
  event_type: string;
  payload: Record<string, unknown>;
  seq: number;
  occurred_at?: string;
  event_id?: string;
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

/** Look up a prior acceptance/terminal receipt by stable command_id (C02). */
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
  if (!receipt || receipt.command_id !== id || !["accepted", "running", "waiting", ...RECEIPT_TERMINAL_STATES].includes(receipt.state)) {
    throw conversationCommandErrorFrom({ code: "conversation.receipt.protocol_invalid",
      category: "protocol", message: "回执响应缺少匹配的命令身份或状态", retryable: false,
      detail: { response_body: raw, command_id: id } }, "回执格式无效", { httpStatus: res.status });
  }
  receiptOwnerScopes.set(receipt, ownerScope);
  return receipt;
}

/**
 * After a transport failure, poll the receipt store so a lost HTTP body does
 * not force a second logical command when the server already accepted.
 */
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

export async function fetchConversationThreads(): Promise<ConversationThread[]> {
  const res = await apiFetch("/api/threads", { cache: "no-store", signal: requestSignal() });
  if (!res.ok) throw new Error(`加载对话失败（HTTP ${res.status}）`);
  const body = (await res.json()) as { threads?: ConversationThread[] };
  return body.threads ?? [];
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

/**
 * Collect turn ids replaced by edit-resend / retry / rewind (full retry_of chain).
 * Used so around-page merges cannot reintroduce superseded rows onto main (#210).
 */
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

/** Drop messages/turns whose turn_id is in the superseded set. */
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

/**
 * After edit-resend / retry / rewind, server tip omits superseded turns.
 * Drop client-cached rows that left the current branch so union-merge cannot
 * resurrect them beside audit history (#210).
 */
export function pruneSupersededBranch(
  previousTurns: ConversationTurn[],
  previousMessages: ConversationMessage[],
  nextTurns: ConversationTurn[],
  nextMessages: ConversationMessage[],
  turnCount?: number | null,
): { turns: ConversationTurn[]; messages: ConversationMessage[]; keepTurnIds: Set<string> } {
  const superseded = collectSupersededTurnIds([...previousTurns, ...nextTurns]);
  // Cascade: once a turn is superseded, later same-branch rows from the cut
  // onward on the client cache are also off the tip.
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

/** Merge a slim snapshot/refresh into client history without clobbering older pages. */
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
    // An older same-branch snapshot may fill history; it cannot replace the
    // already admitted controls, lifecycle, queue or metadata.
    const superseded = collectSupersededTurnIds(previous.turns, previous.superseded_turns || []);
    const cleaned = omitSupersededBranchRows(next.messages, next.turns, superseded);
    const messages = mergeMessageLists(cleaned.messages, previous.messages);
    return { ...previous, messages,
      turns: mergeTurnLists(cleaned.turns, previous.turns),
      messages_page: deriveMessagesPage(messages, previous.messages_page) };
  }
  // Around pages intentionally include superseded hits for search landing.
  // Strip them before union-merge so a stale scroll-anchor cannot remix the
  // old branch onto main after generation already advanced (or same-gen tip).
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
    // Client already holds older history than this snapshot page.
    hasMoreBefore = Boolean(prevPage.has_more_before);
  } else if (prevPage && !nextPage) {
    hasMoreBefore = Boolean(prevPage.has_more_before);
  }
  // Around-window replace can sit below a later tip snapshot. Merging the tip
  // page into that window leaves a hole; keep has_more_after so newer pages
  // (or jump-to-latest) can close it. Do not inherit next's false tip flag.
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
    if (item.sha256) artifactsByDigest.set(item.sha256, item);
  }
  for (const item of next.artifacts || []) {
    if (item.sha256) artifactsByDigest.set(item.sha256, item);
  }
  return {
    ...next,
    messages,
    turns,
    messages_page,
    artifacts: Array.from(artifactsByDigest.values()),
  };
}

export async function fetchConversationView(
  threadId: string,
  markRead = true,
  messagesLimit = 50,
): Promise<ConversationView> {
  const query = new URLSearchParams({
    mark_read: markRead ? "true" : "false",
    messages_limit: String(messagesLimit),
  });
  try {
    const res = await apiFetch(
      `/api/threads/${encodeURIComponent(threadId)}?${query}`,
      { signal: requestSignal() },
    );
    if (!res.ok) throwThreadLoadFailure(res.status);
    return (await res.json()) as ConversationView;
  } catch (exc) {
    rethrowNetworkLoadFailure(exc);
  }
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
  const res = await apiFetch(`/api/threads/search?${params}`, {
    signal: requestSignal(options.signal),
  });
  if (!res.ok) {
    const raw = await res.text();
    let serverError: CommandReceiptView["error"];
    try { serverError = (JSON.parse(raw) as { error?: CommandReceiptView["error"] })?.error; }
    catch { /* The original non-JSON body remains in the structured diagnostic. */ }
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

export async function fetchTurnProcess(
  threadId: string,
  turnId: string,
  options: { limit?: number; afterSeq?: number; watermark?: number; signal?: AbortSignal } = {},
): Promise<{
  turn_id: string;
  events: ConversationEvent[];
  count: number;
  truncated: boolean;
  has_more: boolean;
  next_after_seq?: number | null;
  watermark: number;
}> {
  const query = new URLSearchParams({
    limit: String(options.limit ?? 2000),
  });
  if (options.afterSeq != null) query.set("after_seq", String(options.afterSeq));
  if (options.watermark != null) query.set("watermark", String(options.watermark));
  const res = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}/turns/${encodeURIComponent(turnId)}/process?${query}`,
    { signal: requestSignal(options.signal) },
  );
  if (!res.ok) throw new Error(`加载回合过程失败（HTTP ${res.status}）：${await res.text()}`);
  const body = await res.json() as {
    turn_id: string; events: Array<ConversationEvent & { stream_seq?: number }>;
    count: number; truncated: boolean; has_more: boolean;
    next_after_seq?: number | null; watermark: number;
  };
  const after = options.afterSeq ?? 0;
  if (!body || body.turn_id !== turnId || !Array.isArray(body.events)
    || !Number.isSafeInteger(body.watermark) || body.watermark < after
    || (options.watermark != null && body.watermark !== options.watermark)
    || typeof body.has_more !== "boolean" || typeof body.truncated !== "boolean"
    || body.has_more !== body.truncated
    || body.count !== body.events.length
    || body.events.some(event => !event || !Number.isSafeInteger(event.seq ?? event.stream_seq)
      || Number(event.seq ?? event.stream_seq) <= after || Number(event.seq ?? event.stream_seq) > body.watermark
      || typeof event.event_type !== "string" || !event.payload || typeof event.payload !== "object")
    || body.events.some((event, index) => index > 0
      && Number(event.seq ?? event.stream_seq) <= Number(body.events[index - 1].seq ?? body.events[index - 1].stream_seq))
    || (body.has_more && (body.next_after_seq == null || !Number.isSafeInteger(body.next_after_seq)
      || body.next_after_seq <= after || body.next_after_seq > body.watermark
      || body.next_after_seq !== Number(body.events.at(-1)?.seq ?? body.events.at(-1)?.stream_seq)))) {
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
  // Conversation-only local engines have no Worker settings page to run their
  // first health check. Probe a newly discovered Devin once before selection.
  const unprobed = rows.filter((row) => (
    row.adapter_id === "devin.acp" && row.enabled !== false && !row.health
  ));
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
    const reasoning = rawReasoning && typeof rawReasoning === "object"
      ? rawReasoning as Record<string, unknown>
      : null;
    rows.push({
      id,
      label: String(record?.label ?? record?.name ?? id),
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
    return metadata ? { ...model, reasoning: metadata.reasoning } : model;
  });
  const models = enrichModels(row.models ?? row.available_models);
  const verifiedIds = new Set(models.map((item) => item.id));
  const candidateModels = enrichModels(
    row.candidate_models ?? row.models ?? row.available_models,
  ).filter((item) => !verifiedIds.has(item.id));
  const present = row.present !== false;
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
    status: String(row.status ?? (present ? "ready" : "missing")),
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

/** Reuse the conversation credential projection on settings surfaces. */
export function normalizeConversationCredential(
  value: unknown,
): ConversationCredential | null {
  return normalizeCredential(value);
}

/** Conversation picker / send path: verified models plus catalog candidates. */
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

/** C27: Request native context compaction for the thread. */
export async function triggerThreadCompact(threadId: string): Promise<{ status: string }> {
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/compact`, {
    method: "POST",
  });
  const body = await res.json().catch(() => ({} as Record<string, unknown>));
  if (!res.ok) {
    throw new Error(
      (body.error as Record<string, unknown> | undefined)?.message as string
      || `压缩请求失败（HTTP ${res.status}）`,
    );
  }
  return body as { status: string };
}

/** Load the canonical credential projection (verified + candidate models). */
export async function fetchConversationCredentials(options?: {
  fresh?: boolean;
}): Promise<ConversationCredential[]> {
  // Conversation Runtime 当前在 Web 主机启动，与 Worker 的容器后端选择无关。
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
  return (body.credentials ?? [])
    .map((item) => normalizeCredential(item))
    .filter((item): item is ConversationCredential => (
      item !== null && isConversationWorkerEngine(item.engine)
    ));
}

export async function fetchConversationProjects(): Promise<ConversationProject[]> {
  const res = await apiFetch("/api/projects", { cache: "no-store" }).catch(() => null);
  if (!res?.ok) {
    const error = new Error(
      `加载 Project 失败（HTTP ${res?.status ?? "network"}）`,
    ) as Error & { httpStatus?: number };
    error.httpStatus = res?.status ?? 0;
    throw error;
  }
  const body = await res.json() as { projects?: ConversationProject[] };
  return Array.isArray(body.projects) ? body.projects : [];
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
  name: string;
  root_path: string;
  description?: string;
}): Promise<{ projectId: string; workspaceId: string; rootPath?: string; reused?: boolean }> {
  const ownerScope = conversationStorageScope();
  const res = await apiFetch("/api/projects", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, command_id: commandId() }),
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
  if (!workspaceId) throw new Error("Project 回执缺少 workspace_id");
  return { projectId: receipt.aggregate.id, workspaceId };
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

export interface ProjectGitStatus {
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
  git_common_dir?: string | null;
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

function parseGitStatus(body: any, fallback: Partial<ProjectGitStatus> = {}): ProjectGitStatus {
  return {
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
    git_common_dir: body.git_common_dir ? String(body.git_common_dir) : null,
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
      /** One-release bridge for a backend that still resolves account refs. */
      credential_ref?: string;
      model?: string;
      effort?: string;
      access_mode?: string;
      /** Historical payload compatibility only. */
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
  attachments_affected?: Array<{ sha256: string; name: string }>;
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

// -- C33: Provider session import helpers ------------------------------------

export interface ProviderSessionScan {
  source_fingerprint: string;
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
  source_versions?: Record<string, string>;
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
}

export async function fetchSidebarPreferences(): Promise<SidebarPreferences> {
  const res = await apiFetch("/api/sidebar-preferences");
  if (!res.ok) throw new Error(`sidebar-preferences GET failed: ${res.status}`);
  return res.json() as Promise<SidebarPreferences>;
}

export async function saveSidebarPreferences(
  prefs: Partial<SidebarPreferences> & { version: number },
): Promise<{ ok: boolean; prefs: SidebarPreferences }> {
  const res = await apiFetch("/api/sidebar-preferences", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(prefs),
  });
  const data = (await res.json()) as SidebarPreferences;
  return { ok: res.ok, prefs: data };
}

export type ConversationStreamStatus =
  | "idle"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "auth_required";

export function useConversationThreads(options?: {
  activeThreadId?: string;
  onInboxEvent?: (event: ConversationInboxEvent) => void;
}) {
  const [threads, setThreads] = useState<ConversationThread[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [inboxStatus, setInboxStatus] = useState<ConversationStreamStatus>("idle");
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
      const rows = await fetchConversationThreads();
      if (!isCurrent()) return;
      setThreads(rows);
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
    void refresh();
    return () => { listMountedRef.current = false; listRequestRef.current += 1; };
  }, [refresh]);

  useEffect(() => {
    let cancelled = false;
    let es: EventSource | null = null;
    let reconnectDelay = 500;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let connectionVersion = 0;
    const streamRef = { current: emptyConversationInboxState() };

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
      if (!cancelled) {
        setInboxStatus(isReconnect ? "reconnecting" : "connecting");
      }
      const ticketResult = await mintTicket();
      if (cancelled || version !== connectionVersion) return;
      if (ticketResult.status === "auth_required") {
        setInboxStatus("auth_required");
        return;
      }
      const ticket = ticketResult.status === "ok" ? ticketResult.ticket : "";
      const query = new URLSearchParams({ snapshot: "true" });
      if (streamRef.current.appliedSeq > 0) {
        query.set("after", String(streamRef.current.appliedSeq));
      }
      if (streamRef.current.brokerEpoch) query.set("broker_epoch", streamRef.current.brokerEpoch);
      if (ticket) query.set("ticket", ticket);

      const nextEs = new EventSource(
        `${API}/api/threads/inbox/events?${query}`,
      );
      es = nextEs;

      nextEs.onopen = () => {
        if (cancelled || es !== nextEs) return;
        reconnectDelay = 500;
        setInboxStatus("connected");
      };
      nextEs.onerror = () => {
        if (cancelled || es !== nextEs) return;
        nextEs.close();
        if (es === nextEs) es = null;
        setInboxStatus("reconnecting");
        const delay = reconnectDelay;
        reconnectDelay = Math.min(reconnectDelay * 2, 10_000);
        reconnectTimer = setTimeout(() => {
          reconnectTimer = null;
          void connect(true);
        }, delay);
      };
      nextEs.addEventListener("snapshot", (raw) => {
        if (cancelled || es !== nextEs) return;
        try {
          const payload = JSON.parse((raw as MessageEvent).data) as {
            threads?: ConversationThread[];
            inbox_seq?: number;
            broker_epoch?: string;
          };
          const epoch = String(payload.broker_epoch || "");
          const inboxSeq = Number(payload.inbox_seq || 0);
          if (epoch === streamRef.current.brokerEpoch && inboxSeq < streamRef.current.appliedSeq) return;
          inboxRevisionRef.current += 1;
          if (Array.isArray(payload.threads)) {
            setThreads(payload.threads);
            setLoading(false);
            setError("");
          }
          if (epoch && epoch !== streamRef.current.brokerEpoch) streamRef.current = { ...emptyConversationInboxState(), brokerEpoch: epoch };
          if (inboxSeq > 0) {
            streamRef.current = {
              ...streamRef.current,
              appliedSeq: Math.max(streamRef.current.appliedSeq, inboxSeq),
            };
          }
        } catch {
          setError("对话 inbox snapshot 无法解析");
        }
      });
      nextEs.addEventListener("event", (raw) => {
        if (cancelled || es !== nextEs) return;
        try {
          const event = JSON.parse(
            (raw as MessageEvent).data,
          ) as ConversationInboxEvent & { thread?: ConversationThread };
          const result = acceptConversationInboxEvent(streamRef.current, event);
          if (!result.accepted) return;
          streamRef.current = result.state;
          inboxRevisionRef.current += 1;
          if (event.thread && event.thread.thread_id) {
            setThreads((current) => upsertConversationThread(current, event.thread as ConversationThread));
          }
          try { onInboxEventRef.current?.(event); }
          catch (error) { setError(`待办已更新，但通知处理失败：${error instanceof Error ? error.message : String(error)}`); }
        } catch {
          setError("对话 inbox 事件无法解析");
        }
      });
    };

    void connect(false);

    return () => {
      cancelled = true;
      connectionVersion += 1;
      if (reconnectTimer !== null) clearTimeout(reconnectTimer);
      es?.close();
      es = null;
    };
  }, []);

  const attentionCount = countThreadAttention(threads, activeThreadId);

  return {
    threads,
    loading,
    error,
    refresh,
    attentionCount,
    inboxStatus,
  };
}

export function useConversation(threadId: string) {
  const [view, setView] = useState<ConversationView | null>(null);
  const [events, setEvents] = useState<ConversationEvent[]>([]);
  const [liveText, setLiveText] = useState("");
  const [liveTextRuns, setLiveTextRuns] = useState<import("./conversationStreamReducer").ConversationLiveTextRun[]>([]);
  const [connected, setConnected] = useState(false);
  const [streamStatus, setStreamStatus] = useState<ConversationStreamStatus>(
    threadId ? "connecting" : "idle",
  );
  const [loading, setLoading] = useState(Boolean(threadId));
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [error, setError] = useState("");
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
  const refreshRequestRef = useRef(0);
  const activeThreadIdRef = useRef(threadId);
  activeThreadIdRef.current = threadId;

  const consumePendingAround = (
    base: ConversationView | null,
  ): ConversationView | null => {
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
    // Superseded scroll-anchor: keep tip/main current-only; do not install remix.
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
  };

  const refresh = useCallback(async () => {
    if (!threadId) return;
    const request = ++refreshRequestRef.current;
    const requestedThreadId = threadId;
    const isCurrent = () => activeThreadIdRef.current === requestedThreadId && request === refreshRequestRef.current;
    setError("");
    try {
      const nextView = await fetchConversationView(requestedThreadId);
      if (!isCurrent()) return;
      setView((previous) => consumePendingAround(
        mergeConversationView(previous, nextView),
      ));
      setError("");
    } catch (exc) {
      if (!isCurrent()) return;
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      if (isCurrent()) {
        setLoading(false);
      }
    }
  }, [threadId]);

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
  }, [threadId]);

  useEffect(() => {
    // Keep the previous snapshot mounted while another thread loads. The
    // persistent /chat shell can then update only its message stream instead
    // of tearing down the header, composer and workspace panel on every route
    // change. Navigating to the new-chat route still clears thread state.
    olderAbortRef.current?.abort();
    olderAbortRef.current = null;
    loadingOlderRef.current = false;
    pendingAroundRef.current = null;
    aroundEpochRef.current += 1;
    refreshRequestRef.current += 1;
    setAuditLanding(null);
    setLoadingOlder(false);
    setView((current) => (threadId ? current : null));
    setEvents([]);
    setLiveText("");
    setLiveTextRuns([]);
    setConnected(false);
    setStreamStatus(threadId ? "connecting" : "idle");
    setError("");
    setLoading(Boolean(threadId));
    if (!threadId) return;
    void refresh();

    let cancelled = false;
    let es: EventSource | null = null;
    let reconnectDelay = 500;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let connectionVersion = 0;
    const streamRef = { current: emptyConversationStreamState() };
    const publishStream = (next: ReturnType<typeof emptyConversationStreamState>) => {
      streamRef.current = next;
      setEvents(next.events as ConversationEvent[]);
      setLiveText(next.liveText);
      setLiveTextRuns(next.liveTextRuns);
    };

    const isCurrentThread = () => (
      !cancelled && activeThreadIdRef.current === threadId
    );

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
        setConnected(false);
      }
      const ticketResult = await mintTicket();
      if (cancelled || version !== connectionVersion) return;
      if (activeThreadIdRef.current !== threadId) return;
      if (ticketResult.status === "auth_required") {
        setConnected(false);
        setStreamStatus("auth_required");
        return;
      }
      const ticket = ticketResult.status === "ok" ? ticketResult.ticket : "";

      // snapshot keeps the Thread view fresh; after=appliedSeq skips already
      // admitted events so reconnect cannot replay deltas into liveText.
      const query = new URLSearchParams({ snapshot: "true" });
      if (streamRef.current.appliedSeq > 0) {
        query.set("after", String(streamRef.current.appliedSeq));
      }
      if (ticket) query.set("ticket", ticket);

      const nextEs = new EventSource(
        `${API}/api/threads/${encodeURIComponent(threadId)}/events?${query}`,
      );
      es = nextEs;

      nextEs.onopen = () => {
        if (!isCurrentThread() || es !== nextEs) return;
        reconnectDelay = 500;
        setConnected(true);
        setStreamStatus("connected");
      };
      nextEs.onerror = () => {
        if (cancelled || es !== nextEs) return;
        nextEs.close();
        if (es === nextEs) es = null;
        setConnected(false);
        setStreamStatus("reconnecting");
        const delay = reconnectDelay;
        reconnectDelay = Math.min(reconnectDelay * 2, 10_000);
        reconnectTimer = setTimeout(() => {
          reconnectTimer = null;
          void connect(true);
        }, delay);
      };
      nextEs.addEventListener("snapshot", (raw) => {
        if (!isCurrentThread() || es !== nextEs) return;
        try {
          const snapshot = JSON.parse((raw as MessageEvent).data) as ConversationView;
          setView((previous) => consumePendingAround(
            mergeConversationView(previous, snapshot),
          ));
          setLoading(false);
        } catch {
          setError("对话 snapshot 无法解析");
        }
      });
      nextEs.addEventListener("event", (raw) => {
        if (!isCurrentThread() || es !== nextEs) return;
        try {
          const event = JSON.parse(
            (raw as MessageEvent).data,
          ) as ConversationEvent;
          const result = acceptConversationStreamEvent(streamRef.current, event);
          if (!result.accepted) return;
          publishStream(result.state);
          if (result.scheduleRefresh) {
            if (refreshTimer.current) clearTimeout(refreshTimer.current);
            refreshTimer.current = setTimeout(() => void refresh(), 100);
          }
        } catch {
          setError("对话事件无法解析");
        }
      });
    };

    void connect(false);

    return () => {
      cancelled = true;
      refreshRequestRef.current += 1;
      connectionVersion += 1;
      if (reconnectTimer !== null) clearTimeout(reconnectTimer);
      es?.close();
      es = null;
      if (refreshTimer.current) clearTimeout(refreshTimer.current);
      olderAbortRef.current?.abort();
      olderAbortRef.current = null;
      // Drop in-flight stream so a late ticket/open cannot publish into the
      // next thread's React state after this effect is replaced.
      streamRef.current = emptyConversationStreamState();
    };
  }, [refresh, threadId]);

  const jumpToLatestMessages = useCallback(async () => {
    if (!threadId) return false;
    const epoch = ++aroundEpochRef.current;
    olderAbortRef.current?.abort();
    pendingAroundRef.current = null;
    const current = viewRef.current;
    const requestedThreadId = threadId;
    try {
      // Always fetch the tip page on explicit 回到最新. After an around-window
      // deep link, mergeConversationView can absorb tip rows and clear
      // has_more_after / around_message_id while the virtualizer is still off
      // the live edge — scroll-only then leaves a permanent gap (Bugbot High).
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
  }, [threadId]);

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
        // Around included a superseded anchor (search/C08). Do not union the
        // old branch onto main — leave tip current-only (#210 residual).
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
      // Queue first: refresh/snapshot merge will install this once the shell
      // matches. Avoid relying on a synchronous setState updater flag (React 18
      // may defer the updater, which previously returned a false "missing").
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
        // If a refresh landed the matching shell but left pending, consume it.
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
  }, [threadId]);

  return {
    view,
    events,
    liveText,
    liveTextRuns,
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
