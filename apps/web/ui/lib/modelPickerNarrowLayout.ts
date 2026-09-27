/**
 * #200 — narrow model-picker layout math (redesign Agent UI).
 * At ≤480px the endpoint/agent rail stacks above the model list so long
 * gpt-5.3-codex-* ids stay distinguishable; desktop dual-pane unchanged.
 */

export const NARROW_MODEL_PICKER_MAX_PX = 480;
export const NARROW_MODEL_PICKER_MQ = `(max-width: ${NARROW_MODEL_PICKER_MAX_PX}px)`;

/** Redesign left rail width (credentials + favorites). */
export const MODEL_PICKER_RAIL_WIDTH_PX = 172;

/**
 * Popover width from ConversationModelPicker:
 * `w-[min(480px,calc(100vw-16px))]`.
 */
export function modelPickerPopoverWidth(viewportPx: number): number {
  return Math.min(480, Math.max(0, viewportPx - 16));
}

/**
 * Approximate model-list column width (px).
 * Before #200, ≤480 kept the side-by-side 172px rail; after, stacked rail
 * frees the full popover width for the model column.
 */
export function modelPickerModelColumnWidth(
  viewportPx: number,
  opts: { narrowStacked?: boolean } = {},
): number {
  const popover = modelPickerPopoverWidth(viewportPx);
  const narrowStacked = opts.narrowStacked ?? viewportPx <= NARROW_MODEL_PICKER_MAX_PX;
  if (narrowStacked) return Math.max(0, popover);
  return Math.max(0, popover - MODEL_PICKER_RAIL_WIDTH_PX);
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
