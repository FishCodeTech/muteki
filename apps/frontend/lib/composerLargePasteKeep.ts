import {
  insertPlainTextAtMarkedRange,
  type PromptDocument,
} from "./composerContextDoc";

export type LargePasteSelection = {
  start: number;
  end: number;
};

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
