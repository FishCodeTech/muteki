/**
 * Map backend `external_side_effects` semantic enum → user-facing hint.
 *
 * Backend (impact.py) currently emits `cannot_undo`. The impact confirm
 * main row already states irreversibility, so that enum must not reappear
 * as a raw hint. Unknown values get an understandable fallback; the raw
 * token is retained only for diagnostics.
 */

export type ExternalSideEffectsMapping = {
  /** Extra hint under the main irreversible copy; omit when redundant. */
  hint?: string;
  /** Raw enum for diagnostics / data attributes only — never primary UI. */
  diagnostic?: string;
};

const KNOWN: Record<string, ExternalSideEffectsMapping> = {
  none: {},
  cannot_undo: {
    // Main copy already says 不能撤销 — do not duplicate.
    diagnostic: "cannot_undo",
  },
};

const UNKNOWN_FALLBACK = "外部影响状态暂未识别，请按不可撤销处理";

export function mapExternalSideEffects(
  value: string | null | undefined,
): ExternalSideEffectsMapping {
  const raw = String(value ?? "").trim();
  if (!raw) return {};
  if (Object.prototype.hasOwnProperty.call(KNOWN, raw)) {
    return KNOWN[raw];
  }
  return { hint: UNKNOWN_FALLBACK, diagnostic: raw };
}

/** Chinese label for known enums (for docs / diagnostics); empty for none. */
export function externalSideEffectsLabel(
  value: string | null | undefined,
): string {
  const raw = String(value ?? "").trim();
  if (!raw || raw === "none") return "";
  if (raw === "cannot_undo") return "已执行的外部操作无法撤销";
  return UNKNOWN_FALLBACK;
}
