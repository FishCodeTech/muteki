/**
 * Large-paste "keep as text" — truncate and merge into the structured prompt
 * document at the paste-time marked selection (#186).
 */
import {
  insertPlainTextAtMarkedRange,
  type PromptDocument,
} from "./composerContextDoc";

/** Matches ConversationShell dialog copy: 保留为文本（截至 5000）. */
export const LARGE_PASTE_KEEP_CHAR_LIMIT = 5000;

export type LargePasteSelection = {
  start: number;
  end: number;
};

/**
 * Truncate pasted text to the UI keep limit and insert/replace at the captured
 * marked selection. Missing selection appends at end (safe fallback).
 */
export function keepLargePasteAsText(
  doc: PromptDocument,
  pastedText: string,
  selection?: LargePasteSelection | null,
): PromptDocument {
  const trimmed = String(pastedText || "").slice(0, LARGE_PASTE_KEEP_CHAR_LIMIT);
  return insertPlainTextAtMarkedRange(
    doc,
    trimmed,
    selection?.start,
    selection?.end,
  );
}
