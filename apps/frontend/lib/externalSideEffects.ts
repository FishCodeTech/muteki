
export type ExternalSideEffectsMapping = {
  hint?: string;
  diagnostic?: string;
};

const KNOWN: Record<string, ExternalSideEffectsMapping> = {
  none: {},
  cannot_undo: {
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
