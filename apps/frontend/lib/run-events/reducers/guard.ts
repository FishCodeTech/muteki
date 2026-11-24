import {
  EventType,
  type DeckState,
  type GuardActionReceipt,
  type GuardCronusAssessment,
  type GuardDeliveryStatus,
  type GuardEgressState,
  type GuardEmergencyStopReceipt,
  type GuardMetrics,
  type GuardPendingApproval,
  type GuardStopStatus,
  type MutekiEvent,
  type ProductionGuardState,
} from "../types";

const RECEIPT_CAP = 50;

export function emptyGuardMetrics(): GuardMetrics {
  return {
    total_evaluated: 0,
    allowed: 0,
    escalated: 0,
    denied: 0,
    cronus_errors: 0,
    avg_cronus_ms: 0,
  };
}

export function defaultProductionGuardState(enabled = true): ProductionGuardState {
  return {
    enabled,
    mode: enabled ? "protected" : "off",
    egress_state: "open",
    pending_approvals: [],
    recent_receipts: [],
    emergency_stop_receipt: null,
    metrics: emptyGuardMetrics(),
    stop_status: "idle",
  };
}

function asRecord(value: unknown): Record<string, any> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, any>
    : null;
}

function asFiniteNumber(value: unknown): number | undefined {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && value.trim()) {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return undefined;
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : value != null ? String(value) : "";
}

export function normalizeEgressState(raw: unknown, fallback: GuardEgressState = "open"): GuardEgressState {
  const token = asString(raw).toLowerCase();
  if (token === "open" || token === "held" || token === "revoked") return token;
  return fallback;
}

export function normalizeDeliveryStatus(raw: unknown, fallback: GuardDeliveryStatus = "not_sent"): GuardDeliveryStatus {
  const token = asString(raw).toLowerCase();
  if (token === "not_sent" || token === "sent" || token === "outcome_unknown" || token === "completed") {
    return token;
  }
  if (token === "unknown") return "outcome_unknown";
  if (token === "done" || token === "finished") return "completed";
  return fallback;
}

export function normalizeStopStatus(raw: unknown, fallback: GuardStopStatus = "idle"): GuardStopStatus {
  const token = asString(raw).toLowerCase();
  if (
    token === "idle"
    || token === "emergency_stopping"
    || token === "stopped"
    || token === "stop_partial"
    || token === "stop_unknown"
  ) {
    return token;
  }
  if (token === "stopping") return "emergency_stopping";
  if (token === "partial" || token === "projection_conflict") return "stop_partial";
  if (token === "unknown") return "stop_unknown";
  if (token === "applied" || token === "effect_observed" || token === "complete" || token === "completed" || token === "ok") return "stopped";
  return fallback;
}

export function normalizeCronus(raw: unknown): GuardCronusAssessment | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const probsRaw = asRecord(obj.decision_probs ?? obj.probabilities ?? obj.probs ?? obj.calibrated_probs);
  let decisionProbs: Record<string, number> | undefined;
  if (probsRaw) {
    const entries: Array<[string, number]> = [];
    for (const [k, v] of Object.entries(probsRaw)) {
      const num = asFiniteNumber(v);
      if (num != null) entries.push([k, num]);
    }
    if (entries.length) decisionProbs = Object.fromEntries(entries);
  }
  const error = asString(obj.error ?? obj.cronus_error).trim() || undefined;
  const failClosedReason = asString(obj.fail_closed_reason ?? obj.fallback_reason).trim() || undefined;
  const available = obj.available === false || Boolean(error || failClosedReason) ? false : true;
  const proceedProb = asFiniteNumber(obj.proceed_prob ?? obj.p_proceed ?? decisionProbs?.proceed ?? decisionProbs?.allow);
  const askProb = asFiniteNumber(obj.ask_prob ?? obj.p_ask ?? decisionProbs?.ask ?? decisionProbs?.escalate);
  const rejectProb = asFiniteNumber(obj.reject_prob ?? obj.p_reject ?? decisionProbs?.reject ?? decisionProbs?.deny);
  const scopePTrue = asFiniteNumber(obj.scope_p_true ?? obj.p_scope_true ?? obj.scope_prob ?? asRecord(obj.scope)?.p_true);
  const latencyMs = asFiniteNumber(obj.latency_ms ?? obj.cronus_ms ?? obj.elapsed_ms);
  const decision = asString(obj.decision ?? obj.verdict ?? obj.label).trim() || undefined;
  const modelName = asString(obj.model_name ?? obj.model).trim() || undefined;
  return {
    available,
    decision,
    decision_probs: decisionProbs,
    proceed_prob: proceedProb,
    ask_prob: askProb,
    reject_prob: rejectProb,
    scope_p_true: scopePTrue,
    latency_ms: latencyMs,
    fail_closed_reason: failClosedReason,
    error,
    model_name: modelName,
  };
}

function normalizeApprovalStatus(raw: unknown): GuardPendingApproval["status"] {
  const token = asString(raw).toLowerCase();
  if (
    token === "pending"
    || token === "resolving"
    || token === "approved"
    || token === "denied"
    || token === "expired"
    || token === "stopped"
  ) {
    return token;
  }
  if (token === "allow_once" || token === "allowed") return "approved";
  if (token === "deny_once" || token === "rejected") return "denied";
  if (token === "emergency_stop") return "stopped";
  return "pending";
}

export function normalizePendingApproval(raw: unknown, fallbackWorker?: string): GuardPendingApproval | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const approvalId = asString(obj.approval_id ?? obj.id).trim();
  if (!approvalId) return null;
  const toolName = asString(obj.tool_name ?? obj.tool ?? obj.action_type ?? "tool").trim() || "tool";
  const command = asString(obj.command ?? obj.command_redacted ?? obj.redacted_command).trim() || undefined;
  const httpSummary = asString(obj.http_summary ?? obj.http_action ?? obj.request_summary).trim() || undefined;
  const scriptSummary = asString(obj.script_summary ?? obj.script_redacted).trim() || undefined;
  const actionSummary = asString(
    obj.action_summary ?? obj.summary ?? obj.redacted_summary ?? command ?? httpSummary ?? scriptSummary ?? toolName,
  ).trim();
  const argsRedacted = typeof obj.args_redacted === "string"
    ? obj.args_redacted
    : obj.args_redacted != null
      ? JSON.stringify(obj.args_redacted, null, 2)
      : typeof obj.redacted_args === "string"
        ? obj.redacted_args
        : obj.redacted_args != null
          ? JSON.stringify(obj.redacted_args, null, 2)
          : undefined;
  const cronus = normalizeCronus(obj.cronus ?? obj.cronus_assessment ?? obj.assessment ?? (
    obj.decision_probs != null || obj.scope_p_true != null || obj.cronus_error != null || obj.fail_closed_reason != null
      ? obj
      : null
  ));
  return {
    approval_id: approvalId,
    run_id: asString(obj.run_id).trim() || undefined,
    tool_name: toolName,
    action_summary: actionSummary,
    command,
    http_summary: httpSummary,
    script_summary: scriptSummary,
    args_redacted: argsRedacted,
    worker_id: asString(obj.worker_id ?? obj.solver_id ?? fallbackWorker).trim() || undefined,
    worker_role: asString(obj.worker_role ?? obj.role).trim() || undefined,
    delivery_status: normalizeDeliveryStatus(obj.delivery_status ?? obj.send_state ?? obj.dispatch_state, "not_sent"),
    status: normalizeApprovalStatus(obj.status),
    reason: asString(obj.reason ?? obj.escalation_reason ?? obj.detail).trim() || undefined,
    risk_level: asString(obj.risk_level ?? obj.risk).trim() || undefined,
    cronus,
    requested_at: typeof obj.requested_at === "number" || typeof obj.requested_at === "string"
      ? obj.requested_at
      : typeof obj.created_at_ms === "number" || typeof obj.created_at_ms === "string"
        ? obj.created_at_ms
        : typeof obj.ts === "number" || typeof obj.ts === "string"
          ? obj.ts
          : undefined,
    expires_at: typeof obj.expires_at === "number" || typeof obj.expires_at === "string"
      ? obj.expires_at
      : undefined,
  };
}

export function normalizeReceipt(raw: unknown, fallbackTs: number, fallbackWorker?: string): GuardActionReceipt | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const approvalId = asString(obj.approval_id).trim() || undefined;
  const receiptId = asString(obj.receipt_id ?? obj.action_id ?? obj.id ?? approvalId).trim()
    || `receipt-${fallbackTs}`;
  const toolName = asString(obj.tool_name ?? obj.tool ?? "tool").trim() || "tool";
  const actionSummary = asString(
    obj.action_summary ?? obj.summary ?? obj.command ?? obj.http_summary ?? obj.script_summary ?? toolName,
  ).trim();
  const cronus = normalizeCronus(obj.cronus ?? obj.cronus_assessment ?? obj.assessment);
  return {
    receipt_id: receiptId,
    approval_id: approvalId,
    tool_name: toolName,
    action_summary: actionSummary,
    worker_id: asString(obj.worker_id ?? obj.solver_id ?? fallbackWorker).trim() || undefined,
    worker_role: asString(obj.worker_role ?? obj.role).trim() || undefined,
    decision: asString(obj.decision ?? obj.action ?? obj.outcome).trim() || undefined,
    delivery_status: normalizeDeliveryStatus(
      obj.delivery_status ?? obj.send_state ?? obj.settlement_state ?? obj.status,
      "completed",
    ),
    note: asString(obj.note ?? obj.detail ?? obj.reason).trim() || undefined,
    cronus,
    ts: typeof obj.ts === "number" || typeof obj.ts === "string"
      ? obj.ts
      : typeof obj.settled_at === "number" || typeof obj.settled_at === "string"
        ? obj.settled_at
        : typeof obj.created_at_ms === "number" || typeof obj.created_at_ms === "string"
          ? obj.created_at_ms
          : fallbackTs,
  };
}

export function normalizeEmergencyStopReceipt(raw: unknown, fallbackTs?: number): GuardEmergencyStopReceipt | null {
  const obj = asRecord(raw);
  if (!obj) return null;
  const workersKilled = Array.isArray(obj.workers_killed) ? obj.workers_killed.map(asString).filter(Boolean) : undefined;
  const workersRemaining = Array.isArray(obj.workers_remaining) ? obj.workers_remaining.map(asString).filter(Boolean) : undefined;
  const unknownActions = Array.isArray(obj.unknown_actions) ? obj.unknown_actions.map(asString).filter(Boolean) : undefined;
  return {
    stop_id: asString(obj.stop_id ?? obj.command_id ?? obj.id).trim() || undefined,
    status: asString(obj.status ?? obj.stop_status ?? "stopped").trim() || "stopped",
    egress_revoked: typeof obj.egress_revoked === "boolean" ? obj.egress_revoked : undefined,
    workers_killed: workersKilled,
    workers_remaining: workersRemaining,
    unknown_actions: unknownActions,
    detail: asString(obj.detail ?? obj.message ?? obj.reason).trim() || undefined,
    ts: typeof obj.ts === "number" || typeof obj.ts === "string" ? obj.ts : fallbackTs,
  };
}

export function normalizeGuardMetrics(raw: unknown, prev?: GuardMetrics): GuardMetrics {
  const base = prev ?? emptyGuardMetrics();
  const obj = asRecord(raw);
  if (!obj) return base;
  return {
    total_evaluated: asFiniteNumber(obj.total_evaluated) ?? base.total_evaluated,
    allowed: asFiniteNumber(obj.allowed) ?? base.allowed,
    escalated: asFiniteNumber(obj.escalated) ?? base.escalated,
    denied: asFiniteNumber(obj.denied) ?? base.denied,
    cronus_errors: asFiniteNumber(obj.cronus_errors) ?? base.cronus_errors,
    avg_cronus_ms: asFiniteNumber(obj.avg_cronus_ms) ?? base.avg_cronus_ms,
  };
}

export function normalizeGuardStateSnapshot(raw: unknown, prev?: ProductionGuardState): ProductionGuardState | null {
  const outer = asRecord(raw);
  if (!outer) return null;
  const obj = asRecord(outer.guard ?? outer.state) ?? outer;
  const enabled = typeof obj.enabled === "boolean"
    ? obj.enabled
    : obj.mode === "protected"
      ? true
      : (prev?.enabled ?? false);
  const mode: "off" | "protected" = obj.mode === "protected" || enabled ? "protected" : "off";
  const pendingRaw = Array.isArray(obj.pending_approvals) ? obj.pending_approvals : null;
  const receiptsRaw = Array.isArray(obj.recent_receipts) ? obj.recent_receipts : null;
  const stopReceipt = obj.emergency_stop_receipt !== undefined
    ? normalizeEmergencyStopReceipt(obj.emergency_stop_receipt)
    : (prev?.emergency_stop_receipt ?? null);
  const derivedStop = stopReceipt
    ? normalizeStopStatus(stopReceipt.status, prev?.stop_status ?? "stopped")
    : normalizeStopStatus(obj.stop_status, prev?.stop_status ?? "idle");
  return {
    enabled,
    mode,
    egress_state: normalizeEgressState(obj.egress_state, prev?.egress_state ?? "open"),
    pending_approvals: pendingRaw
      ? pendingRaw.map((row) => normalizePendingApproval(row)).filter((row): row is GuardPendingApproval => Boolean(row))
      : (prev?.pending_approvals ?? []),
    recent_receipts: receiptsRaw
      ? receiptsRaw.map((row) => normalizeReceipt(row, Date.now())).filter((row): row is GuardActionReceipt => Boolean(row)).slice(-RECEIPT_CAP)
      : (prev?.recent_receipts ?? []),
    emergency_stop_receipt: stopReceipt,
    metrics: normalizeGuardMetrics(obj.metrics, prev?.metrics),
    stop_status: derivedStop,
  };
}

function ensureGuard(s: DeckState): ProductionGuardState {
  if (s.productionGuard) {
    return {
      ...s.productionGuard,
      enabled: true,
      mode: "protected",
    };
  }
  return defaultProductionGuardState(true);
}

function upsertReceipt(list: GuardActionReceipt[], next: GuardActionReceipt): GuardActionReceipt[] {
  const index = list.findIndex((item) => item.receipt_id === next.receipt_id || (next.approval_id && item.approval_id === next.approval_id));
  if (index >= 0) {
    const copy = list.slice();
    copy[index] = { ...copy[index], ...next };
    return copy.slice(-RECEIPT_CAP);
  }
  return [...list, next].slice(-RECEIPT_CAP);
}

export function reduceGuard(ev: MutekiEvent, s: DeckState): DeckState | undefined {
  const p = ev.payload || {};
  switch (ev.event_type) {
    case EventType.GUARD_APPROVAL_REQUESTED: {
      const guard = ensureGuard(s);
      const approval = normalizePendingApproval(p.approval ?? p, ev.solver_id ?? undefined);
      if (approval) {
        const exists = guard.pending_approvals.some((item) => item.approval_id === approval.approval_id);
        guard.pending_approvals = exists
          ? guard.pending_approvals.map((item) => item.approval_id === approval.approval_id ? { ...item, ...approval } : item)
          : [...guard.pending_approvals, approval];
      }
      if (p.egress_state) {
        guard.egress_state = normalizeEgressState(p.egress_state, guard.egress_state);
      } else if (guard.egress_state === "open") {
        guard.egress_state = "held";
      }
      if (p.metrics) guard.metrics = normalizeGuardMetrics(p.metrics, guard.metrics);
      s.productionGuard = guard;
      return s;
    }
    case EventType.GUARD_APPROVAL_RESOLVED: {
      const guard = ensureGuard(s);
      const approvalId = asString(p.approval_id ?? p.approval?.approval_id ?? p.id).trim();
      const status = normalizeApprovalStatus(p.status ?? p.decision ?? p.action);
      const existing = approvalId ? guard.pending_approvals.find((item) => item.approval_id === approvalId) : undefined;
      if (approvalId) {
        if (status === "pending" || status === "resolving") {
          guard.pending_approvals = guard.pending_approvals.map((item) => (
            item.approval_id === approvalId
              ? {
                  ...item,
                  status,
                  delivery_status: p.delivery_status
                    ? normalizeDeliveryStatus(p.delivery_status, item.delivery_status)
                    : item.delivery_status,
                }
              : item
          ));
        } else {
          guard.pending_approvals = guard.pending_approvals.filter((item) => item.approval_id !== approvalId);
        }
      }
      if (p.egress_state) {
        guard.egress_state = normalizeEgressState(p.egress_state, guard.egress_state);
      }
      const receiptSource = asRecord(p.receipt) ?? (existing ? {
        receipt_id: asString(p.receipt_id ?? approvalId),
        approval_id: approvalId,
        tool_name: p.tool_name ?? existing.tool_name,
        action_summary: p.action_summary ?? existing.action_summary,
        worker_id: p.worker_id ?? existing.worker_id,
        worker_role: p.worker_role ?? existing.worker_role,
        decision: p.action ?? p.decision ?? status,
        delivery_status: p.delivery_status ?? (status === "denied" ? "not_sent" : existing.delivery_status),
        note: p.note ?? p.detail,
        cronus: p.cronus ?? existing.cronus,
        ts: ev.ts,
      } : null);
      if (receiptSource) {
        const receipt = normalizeReceipt(receiptSource, ev.ts, ev.solver_id ?? undefined);
        if (receipt) guard.recent_receipts = upsertReceipt(guard.recent_receipts, receipt);
      }
      if (p.metrics) guard.metrics = normalizeGuardMetrics(p.metrics, guard.metrics);
      s.productionGuard = guard;
      return s;
    }
    case EventType.GUARD_ACTION_SETTLED: {
      const guard = ensureGuard(s);
      const receipt = normalizeReceipt(p.receipt ?? p, ev.ts, ev.solver_id ?? undefined);
      if (receipt) {
        guard.recent_receipts = upsertReceipt(guard.recent_receipts, receipt);
        if (receipt.approval_id) {
          guard.pending_approvals = guard.pending_approvals.filter((item) => item.approval_id !== receipt.approval_id);
        }
      }
      if (p.egress_state) {
        guard.egress_state = normalizeEgressState(p.egress_state, guard.egress_state);
      }
      if (p.metrics) guard.metrics = normalizeGuardMetrics(p.metrics, guard.metrics);
      s.productionGuard = guard;
      return s;
    }
    case EventType.GUARD_EGRESS_STATE_CHANGED: {
      const guard = ensureGuard(s);
      guard.egress_state = normalizeEgressState(p.egress_state ?? p.state ?? p.to, guard.egress_state);
      if (p.metrics) guard.metrics = normalizeGuardMetrics(p.metrics, guard.metrics);
      s.productionGuard = guard;
      return s;
    }
    case EventType.GUARD_EMERGENCY_STOP_RECEIPT: {
      const guard = ensureGuard(s);
      const receipt = normalizeEmergencyStopReceipt(p.receipt ?? p, ev.ts);
      guard.emergency_stop_receipt = receipt;
      guard.egress_state = "revoked";
      guard.stop_status = receipt
        ? normalizeStopStatus(receipt.status, "stopped")
        : normalizeStopStatus(p.status, "stopped");
      if (guard.stop_status !== "emergency_stopping") {
        guard.pending_approvals = [];
      }
      if (p.metrics) guard.metrics = normalizeGuardMetrics(p.metrics, guard.metrics);
      s.productionGuard = guard;
      return s;
    }
    default:
      return undefined;
  }
}
