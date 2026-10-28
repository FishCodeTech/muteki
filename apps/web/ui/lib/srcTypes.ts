export type SrcTask = {
  task_id: string;
  title: string;
  kind: "src.bounty";
  revision: number;
  input: {
    product_mode?: "src";
    goal?: string;
    authorization?: { targets?: string[]; exclusions?: string[] };
    company?: string;
    program?: Record<string, unknown>;
  };
  binding: { task_id: string; thread_id: string; principal_id: string; revision: number };
  run_count: number;
  watermarks: Array<Record<string, unknown>>;
};

export type SrcRecord<T = Record<string, unknown>> = {
  record_id: string;
  kind: string;
  task_id: string;
  revision: number;
  parent_id: string;
  natural_key: string;
  generation: number;
  created_at: string;
  updated_at: string;
  payload: T;
};

export type SrcRun = {
  binding: {
    binding_id: string;
    task_id: string;
    thread_id: string;
    run_id: string;
    binding_key: string;
    binding_revision: number;
    product_mode: "src";
    execution_mode: "pentest";
    target_contract_ref: string;
    plan_ref: string;
    authorization_ref: string;
    source_message_id: string;
    related_run_ids: string[];
    target_url?: string;
    contract?: { target?: string; question?: string };
  };
  run: {
    run_id: string;
    state: string;
    generation: number;
    task_kind: string;
    executor_id?: string | null;
    created_at?: string | null;
    updated_at?: string | null;
    detail: Record<string, unknown>;
  };
  deduplicated?: boolean;
};

export type ExecutionRule = {
  rule_id: string;
  version: number;
  enabled: boolean;
  effect: "allow" | "ask" | "deny";
  evaluation_point: "tool" | "filesystem" | "connection" | "request";
  mode: "enforce" | "observe";
  resource_kind: "file" | "process" | "network" | "http" | "browser" | "mcp" | "control";
  selector: Record<string, unknown>;
  actions: string[];
  conditions: Record<string, unknown>;
  reason: string;
  source_refs: string[];
  reusable_approval: boolean;
};

export type ExecutionPolicy = {
  policy_id: string;
  version: number;
  product_mode: "src";
  task_id: string;
  source_refs: string[];
  rules: ExecutionRule[];
  defaults: Record<string, unknown>;
  authorized_targets: string[];
  excluded_targets: string[];
  environment_requirements: {
    service_exceptions?: Array<{ host: string; port: number; protocol?: string; purpose?: string }>;
    [key: string]: unknown;
  };
  network_mode: "declared" | "enforced";
  inspection_requirements: { mode: "off" | "record" | "enforce"; [key: string]: unknown };
  created_by: string;
  created_at: string;
  digest: string;
  supersedes?: string | null;
  activation: Record<string, unknown>;
};

export type ExecutionCapabilities = {
  network_enforced: boolean;
  content_enforced: boolean;
  state?: string;
  container?: string;
  generation?: number;
  health?: Record<string, unknown>;
  tools: Record<string, {
    trusted_input_fields?: string[];
    observed_resource_fields?: string[];
    pre_dispatch_gate?: boolean;
    environment_boundary?: string;
    network_scope?: string[];
    content_inspection?: string[];
    cancellation?: string;
    receipt_support?: string;
  }>;
};

export type ExecutionApproval = {
  approval_id: string;
  invocation_id: string;
  input_digest: string;
  request_snapshot: {
    invocation_id: string;
    task_id: string;
    thread_id: string;
    run_id?: string | null;
    worker_id?: string | null;
    tool_name: string;
    action: string;
    arguments: Record<string, unknown>;
    arguments_hash: string;
    arguments_ref?: Record<string, unknown> | null;
    resource_kind: string;
    resource_facts: Record<string, { value?: unknown; origin: string; state: string }>;
    identity_ref: string;
    identity_revision: number;
    cwd: string;
    environment_id: string;
    environment_generation: number;
    policy_version: number;
  };
  matched_rule_refs: string[];
  policy_version: number;
  status: "pending" | "approved" | "denied" | "expired" | "cancelled" | "superseded";
  requested_at: string;
  expires_at: string;
  decided_by: string;
  decided_at?: string | null;
  decision?: "approve_once" | "deny" | "approve_task_scope" | null;
  grant_id?: string | null;
  consumed_at?: string | null;
  revision: number;
};

export type TaskExecutionGrant = {
  grant_id: string;
  task_id: string;
  rule_refs: string[];
  resource_selector: Record<string, unknown>;
  actions: string[];
  identity_constraints: Record<string, unknown>;
  environment_constraints: Record<string, unknown>;
  input_constraints: Record<string, unknown>;
  source_approval_id: string;
  created_by: string;
  created_at: string;
  expires_at?: string | null;
  revoked_at?: string | null;
  version: number;
  policy_digest: string;
};

export type ExecutionReceipt = {
  invocation_id: string;
  decision_id: string;
  dispatch_state: "created" | "waiting_approval" | "denied" | "ready" | "dispatched" | "completed" | "failed" | "outcome_unknown" | "cancelled";
  started_at?: string | null;
  ended_at?: string | null;
  external_receipt_ref: string;
  result_ref?: Record<string, unknown> | null;
  result?: unknown;
  error_code: string;
  outcome_known: boolean;
  history: Array<Record<string, unknown>>;
};

export type PolicyDecision = {
  decision_id: string;
  invocation_id: string;
  effect: "allow" | "ask" | "deny";
  status: string;
  matched_rule_refs: string[];
  observed_rule_refs: string[];
  unknown_fields: string[];
  policy_version: number;
  approval_id?: string | null;
  grant_id?: string | null;
  reason_code: string;
  timestamp: string;
};

export type SrcCommandReceipt<T = Record<string, unknown>> = {
  command_id: string;
  state: string;
  output: T;
  error?: { code: string; message: string; category?: string; recovery_hint?: string; detail?: Record<string, unknown> } | null;
  deduplicated?: boolean;
};

export type PolicyPreview = {
  effect: "allow" | "ask" | "deny";
  status: string;
  reason_code: string;
  matched_rule_refs: string[];
  observed_rule_refs: string[];
  unknown_fields: string[];
};

export type WorkbenchSliceMeta = {
  revision: number;
  updated_at: string;
  client_id: string;
  data: Record<string, unknown>;
};

export type WorkbenchSnapshot = { slices: Record<string, WorkbenchSliceMeta> };

export type WorkbenchSlicePayload = WorkbenchSliceMeta & { slice: string };

export type WorkbenchSliceSaveResult = { slice: string; revision: number; updated_at: string };
