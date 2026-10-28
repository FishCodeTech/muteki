export type CaptureState = "complete" | "partial" | "unavailable" | string;
export type ExecutionState =
  | "created" | "waiting_approval" | "denied" | "ready" | "dispatched"
  | "completed" | "failed" | "outcome_unknown" | "cancelled" | string;

export interface ArtifactRef {
  artifact_id: string;
  sha256: string;
  media_type: string;
  size: number;
  task_id: string;
  source_run_id?: string | null;
  source_event_seq?: number | null;
  version: number;
  original?: boolean;
  complete?: CaptureState;
  name?: string;
}

export interface HttpExchange {
  exchange_id: string;
  task_id: string;
  revision?: number;
  source_run_id?: string | null;
  source_event_seq?: number | null;
  source_kind: "browser" | "http_tool" | "proxy" | "import" | string;
  actor_ref: string;
  context_id: string;
  identity_revision: number;
  browser_generation: number;
  declared_target?: string;
  observed_url?: string;
  method: string;
  started_at: string;
  finished_at?: string | null;
  request_raw_ref: ArtifactRef;
  response_raw_ref?: ArtifactRef | null;
  request_headers?: Array<[string, string]>;
  response_headers?: Array<[string, string]>;
  status_code?: number | null;
  protocol?: string;
  redirect_parent?: string | null;
  replay_of?: string | null;
  execution_state: ExecutionState;
  capture_state: CaptureState;
  capture_limitations?: string[];
  authorization_ref?: string;
  decision_ref?: string;
  protocol_stream_ref?: string;
  tags?: string[];
  metadata?: Record<string, unknown>;
  request_raw?: string;
  response_raw?: string;
}

export interface RequestDraft {
  draft_id: string;
  task_id: string;
  source_exchange_id: string;
  revision: number;
  raw_request_ref: ArtifactRef;
  target_url: string;
  method: string;
  changes?: Record<string, unknown>;
  editor?: string;
  created_at?: string;
  raw_request?: string;
}

export interface ApprovalRecord {
  approval_id: string;
  invocation_id: string;
  input_digest: string;
  status: string;
  revision: number;
  matched_rule_refs?: string[];
  policy_version: number;
  expires_at: string;
  request_snapshot?: {
    arguments?: Record<string, unknown>;
    identity_ref?: string;
    resource_facts?: Record<string, { value?: unknown; origin?: string; state?: string }>;
  };
}

export interface ExecutionReceipt {
  invocation_id: string;
  decision_id: string;
  dispatch_state: ExecutionState;
  started_at?: string | null;
  ended_at?: string | null;
  error_code?: string;
  outcome_known: boolean;
  result?: unknown;
  approval?: ApprovalRecord | null;
}

export interface ExchangePage {
  items: HttpExchange[];
  total?: number | null;
  next_cursor?: string | null;
  snapshot?: string;
  watermarks?: Array<Record<string, unknown>>;
  complete?: boolean;
  partial_reason?: string;
}

export interface ComparisonResult {
  request_diff?: unknown;
  response_diff?: unknown;
  header_diff?: unknown;
  body_diff?: unknown;
  raw_diff?: string;
  normalized_diff?: string;
  non_single_variable?: boolean;
  warnings?: string[];
  ignored_rules?: Record<string, unknown>;
  baseline?: HttpExchange;
  experiment?: HttpExchange;
}

export interface IdentityContext {
  context_id: string;
  account_ref?: string;
  role?: string;
  tenant?: string;
  credential_source?: string;
  identity_revision: number;
  browser_generation: number;
  status?: string;
  last_verified_at?: string | null;
  expected?: Record<string, unknown>;
  observed?: Record<string, unknown>;
  recovery_state?: string;
  last_url?: string;
  evidence_refs?: string[];
}

export interface TestObject {
  object_id: string;
  experiment_id?: string;
  object_type: string;
  owner?: string;
  tenant?: string;
  initial_state?: Record<string, unknown>;
  current_state?: Record<string, unknown>;
  expected_change?: string | Record<string, unknown>;
  restoration?: string | { status?: string; reason?: string };
  restoration_detail?: { status?: string; reason?: string };
  residual_objects?: Array<Record<string, unknown>>;
  unrestored_fields?: string[];
  reason?: string;
  lease?: Record<string, unknown>;
}

export interface ProtocolFrame {
  seq: number;
  direction?: string;
  opcode?: string | number;
  event?: string;
  id?: string;
  retry?: number;
  timestamp?: string;
  payload?: string;
  payload_base64?: string;
  media_type?: string;
  truncated?: boolean;
}

export interface ProtocolStream {
  stream_id: string;
  protocol: "websocket" | "sse" | string;
  context_id: string;
  handshake?: Record<string, unknown>;
  identity_revision?: number;
  browser_generation?: number;
  state?: string;
  capture_state?: CaptureState;
  termination_reason?: string;
  gaps?: Array<{ after_seq?: number; before_seq?: number; reason?: string }>;
  frames: ProtocolFrame[];
}

export interface OastInteraction {
  interaction_id: string;
  token: string;
  protocols: string[];
  trigger_exchange_id?: string;
  source_run_id?: string | null;
  source_step_id?: string | null;
  service_instance?: string;
  created_at: string;
  expires_at?: string | null;
  state?: string;
  health?: string;
  cursor?: string;
  last_polled_at?: string | null;
  events?: Array<{
    event_id: string;
    protocol: string;
    received_at: string;
    source_address?: string;
    path?: string;
    raw_ref?: ArtifactRef;
    late?: boolean;
    health_probe?: boolean;
    missing_before?: boolean;
  }>;
  completeness?: CaptureState;
  error?: string;
}

export interface WorkspaceEndpoints {
  exchanges?: string;
  identities?: string;
  objects?: string;
  streams?: string;
  oast?: string;
  artifacts?: string;
}

export interface CommandReceipt<T = unknown> {
  command_id?: string;
  state?: string;
  output?: T;
  error?: { code?: string; message?: string; detail?: Record<string, unknown> } | null;
}

export function receiptOutput<T>(value: unknown): T {
  const row = value as CommandReceipt<T> | T;
  if (row && typeof row === "object" && "output" in row) {
    const output = (row as CommandReceipt<T>).output;
    if (output && typeof output === "object" && "result" in output) return (output as { result: T }).result;
    return output as T;
  }
  return row as T;
}

export function byteText(value: unknown): string {
  if (value === undefined || value === null) return "";
  if (typeof value === "string") return value;
  if (typeof value !== "object") return String(value);
  const row = value as { encoding?: string; data?: string; base64?: string };
  const encoded = row.encoding === "base64" ? row.data : row.base64;
  if (!encoded) return "";
  return atob(encoded);
}

export function textBase64(value: string): string {
  const chunks: number[] = [];
  for (let i = 0; i < value.length; i += 1) {
    const code = value.charCodeAt(i);
    if (code <= 0xff) {
      chunks.push(code);
    } else {
      const encoded = new TextEncoder().encode(value[i]);
      encoded.forEach(b => chunks.push(b));
    }
  }
  let binary = "";
  for (let i = 0; i < chunks.length; i += 1) {
    binary += String.fromCharCode(chunks[i]);
  }
  return btoa(binary);
}

export function errorMessage(value: unknown, fallback: string): string {
  if (value instanceof Error) return value.message;
  if (typeof value === "string") return value;
  if (value && typeof value === "object" && "error" in value) {
    const error = (value as { error?: { message?: string } }).error;
    if (error?.message) return error.message;
  }
  return fallback;
}

export function isTextMedia(mediaType = ""): boolean {
  const value = mediaType.toLowerCase();
  return value.startsWith("text/") || /json|xml|javascript|x-www-form-urlencoded|html|yaml|csv/.test(value);
}

export function exchangeLabel(exchange: HttpExchange): string {
  return `${exchange.method} ${exchange.observed_url || exchange.declared_target || "目标未知"}`;
}
