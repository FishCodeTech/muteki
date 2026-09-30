/**
 * Large-paste "keep as text" — preserve and merge into the structured prompt
 * document at the paste-time marked selection (#186).
 */
import {
  insertPlainTextAtMarkedRange,
  type PromptDocument,
} from "./composerContextDoc";

export type LargePasteSelection = {
  start: number;
  end: number;
};

/**
 * Preserve pasted text and insert/replace at the captured
 * marked selection. Missing selection appends at end (safe fallback).
 */
export function keepLargePasteAsText(
  doc: PromptDocument,
  pastedText: string,
  selection?: LargePasteSelection | null,
): PromptDocument {
  const trimmed = String(pastedText || "");
  return insertPlainTextAtMarkedRange(
    doc,
    trimmed,
    selection?.start,
    selection?.end,
  );
}
