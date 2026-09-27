"use client";

/**
 * C24 — single-source interaction capability matrix helpers for Conversation chrome.
 * Menu (`/` `@` `$`) and in-body controls must read the same revision.
 */

export type InteractionCapabilityLevel =
  | "supported"
  | "limited"
  | "unsupported"
  | "unknown"
  | "expired";

export type InteractionCapabilityKey =
  | "steer"
  | "interrupt"
  | "attachments"
  | "approval"
  | "user_input"
  | "plan"
  | "native_history"
  | "rewind"
  | "fork"
  | "command_catalog";

export interface InteractionCapabilityRow {
  key: InteractionCapabilityKey | string;
  level: InteractionCapabilityLevel | string;
  reason?: string;
  alternative?: string;
  source?: string;
  invocable?: boolean;
}

export interface InteractionCapabilityMatrix {
  adapter_id?: string;
  instance_id?: string;
  revision: number;
  stale: boolean;
  rows: InteractionCapabilityRow[];
  diagnostics?: string[];
}

export interface ProviderSwitchDelta {
  key: string;
  from_level: string;
  to_level: string;
  reason?: string;
  alternative?: string;
}

const DEFINITE = new Set(["supported", "limited"]);

export function matrixFromRuntimeConnection(
  runtimeConnection?: {
    matrix?: {
      adapter_id?: string;
      instance_id?: string;
      revision?: number;
      stale?: boolean;
      rows?: InteractionCapabilityRow[];
      diagnostics?: string[];
    } | null;
    capability_revision?: number;
    capability_stale?: boolean;
    capabilities?: Record<string, unknown>;
  } | null,
): InteractionCapabilityMatrix | null {
  if (!runtimeConnection) return null;
  if (runtimeConnection.matrix && Array.isArray(runtimeConnection.matrix.rows)) {
    return {
      adapter_id: runtimeConnection.matrix.adapter_id,
      instance_id: runtimeConnection.matrix.instance_id,
      revision: Number(
        runtimeConnection.matrix.revision
          ?? runtimeConnection.capability_revision
          ?? 0,
      ),
      stale: Boolean(
        runtimeConnection.matrix.stale
          ?? runtimeConnection.capability_stale
          ?? false,
      ),
      rows: runtimeConnection.matrix.rows,
      diagnostics: runtimeConnection.matrix.diagnostics || [],
    };
  }
  return null;
}

export function rowMap(
  matrix: InteractionCapabilityMatrix | null | undefined,
): Record<string, InteractionCapabilityRow> {
  const out: Record<string, InteractionCapabilityRow> = {};
  for (const row of matrix?.rows || []) {
    if (row?.key) out[String(row.key)] = row;
  }
  return out;
}

export function levelOf(
  matrix: InteractionCapabilityMatrix | null | undefined,
  key: InteractionCapabilityKey | string,
): InteractionCapabilityLevel {
  const level = String(rowMap(matrix)[key]?.level || "unknown");
  if (
    level === "supported"
    || level === "limited"
    || level === "unsupported"
    || level === "unknown"
    || level === "expired"
  ) {
    return level;
  }
  return "unknown";
}

export function isDefiniteSupport(
  matrix: InteractionCapabilityMatrix | null | undefined,
  key: InteractionCapabilityKey | string,
): boolean {
  if (!matrix || matrix.stale) return false;
  return DEFINITE.has(levelOf(matrix, key));
}

export function canInvoke(
  matrix: InteractionCapabilityMatrix | null | undefined,
  key: InteractionCapabilityKey | string,
): boolean {
  if (!matrix || matrix.stale) return false;
  const row = rowMap(matrix)[key];
  if (!row) return false;
  if (row.invocable === false) return false;
  return levelOf(matrix, key) === "supported";
}

export function disableCopy(
  matrix: InteractionCapabilityMatrix | null | undefined,
  key: InteractionCapabilityKey | string,
): { reason: string; alternative: string } {
  const row = rowMap(matrix)[key];
  return {
    reason: String(row?.reason || "当前能力尚未确认为支持"),
    alternative: String(row?.alternative || "请改用其他可用操作或等待能力刷新"),
  };
}

export function revisionsAligned(
  chromeRevision: number,
  menuRevision: number,
  options?: { allowZero?: boolean },
): boolean {
  const allowZero = options?.allowZero ?? true;
  if (allowZero && (chromeRevision === 0 || menuRevision === 0)) {
    return chromeRevision === menuRevision;
  }
  return chromeRevision === menuRevision;
}

const SWITCH_KEYS: InteractionCapabilityKey[] = [
  "native_history",
  "attachments",
  "approval",
  "plan",
  "rewind",
];

export function computeProviderSwitchDelta(
  before: InteractionCapabilityMatrix | null | undefined,
  after: InteractionCapabilityMatrix | null | undefined,
  keys: InteractionCapabilityKey[] = SWITCH_KEYS,
): ProviderSwitchDelta[] {
  const left = rowMap(before);
  const right = rowMap(after);
  const changes: ProviderSwitchDelta[] = [];
  for (const key of keys) {
    const fromLevel = String(left[key]?.level || "unknown");
    const toLevel = String(right[key]?.level || "unknown");
    if (fromLevel === toLevel) continue;
    changes.push({
      key,
      from_level: fromLevel,
      to_level: toLevel,
      reason: right[key]?.reason,
      alternative: right[key]?.alternative,
    });
  }
  return changes;
}

export function summarizeProviderSwitchDelta(
  deltas: ProviderSwitchDelta[],
): string {
  if (!deltas.length) return "";
  const labels: Record<string, string> = {
    native_history: "原生历史连续性",
    attachments: "附件 / 多模态",
    approval: "审批",
    plan: "计划",
    rewind: "回退 / 原生兜底",
  };
  return deltas
    .map((item) => {
      const label = labels[item.key] || item.key;
      return `${label}：${item.from_level} → ${item.to_level}`;
    })
    .join("；");
}
