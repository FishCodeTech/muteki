/**
 * #200 — narrow model-picker layout math (redesign Agent UI).
 * At ≤480px model rows wrap long gpt-5.3-codex-* ids instead of truncating
 * them and drop the ⌘1–9 hints, so near-identical ids stay distinguishable.
 */

export const NARROW_MODEL_PICKER_MAX_PX = 480;
export const NARROW_MODEL_PICKER_MQ = `(max-width: ${NARROW_MODEL_PICKER_MAX_PX}px)`;

/**
 * Popover width from ConversationModelPicker:
 * `w-[min(400px,calc(100vw-16px))]`. Agent tabs sit above the list, so the
 * model column spans the whole popover.
 */
export function modelPickerPopoverWidth(viewportPx: number): number {
  return Math.min(400, Math.max(0, viewportPx - 16));
}

/** Approximate model-list column width (px). */
export function modelPickerModelColumnWidth(viewportPx: number): number {
  return modelPickerPopoverWidth(viewportPx);
}

/** True when two ids that share a long prefix stay distinguishable at this width. */
export function modelIdsDistinguishableAtWidth(
  a: string,
  b: string,
  columnWidthPx: number,
  /** Rough average glyph width for 10.5–13px UI text. */
  avgGlyphPx = 7,
): boolean {
  if (a === b) return true;
  const usable = Math.max(0, columnWidthPx - 40); // padding + star/check
  if (usable <= 0) return false;
  const charsVisible = Math.floor(usable / avgGlyphPx);
  // With wrap, full strings are visible whenever the column can show ≥1 char.
  if (charsVisible >= Math.max(a.length, b.length)) return true;
  // Wrapped layout: a short suffix line remains readable.
  return charsVisible >= 12;
}
