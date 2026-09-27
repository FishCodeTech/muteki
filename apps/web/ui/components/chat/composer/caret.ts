export type EditorCaret =
  | { kind: "range"; range: Range }
  | { kind: "offset"; start: number; end: number };

export function captureCaret(editor: HTMLElement | null): EditorCaret | null {
  if (!editor) return null;
  if (editor instanceof HTMLTextAreaElement) {
    const start = editor.selectionStart ?? editor.value.length;
    return { kind: "offset", start, end: editor.selectionEnd ?? start };
  }
  const selection = window.getSelection();
  if (!selection?.rangeCount) return null;
  const range = selection.getRangeAt(0);
  if (!editor.contains(range.startContainer)) return null;
  return { kind: "range", range: range.cloneRange() };
}

function needsLeadingSpace(before: string): boolean {
  const clean = before.replace(/\u200B/g, "");
  return clean.length > 0 && !/\s$/.test(clean);
}

/**
 * Inserts a trigger character (`/`, `@`) at the saved caret so the editor's own
 * input pipeline (token detection, document sync) runs as if the user typed it.
 * `execCommand` is the only API that keeps native undo and fires `input`.
 */
export function insertTriggerAtCaret(editor: HTMLElement, trigger: string, caret: EditorCaret | null): void {
  editor.focus({ preventScroll: true });
  if (editor instanceof HTMLTextAreaElement) {
    const start = caret?.kind === "offset" ? caret.start : editor.value.length;
    const end = caret?.kind === "offset" ? caret.end : start;
    editor.setSelectionRange(start, end);
    const text = needsLeadingSpace(editor.value.slice(0, start)) ? ` ${trigger}` : trigger;
    document.execCommand("insertText", false, text);
    return;
  }
  const selection = window.getSelection();
  if (!selection) return;
  selection.removeAllRanges();
  if (caret?.kind === "range" && editor.contains(caret.range.startContainer)) {
    selection.addRange(caret.range);
  } else {
    const end = document.createRange();
    end.selectNodeContents(editor);
    end.collapse(false);
    selection.addRange(end);
  }
  const range = selection.getRangeAt(0);
  const prefix = document.createRange();
  prefix.selectNodeContents(editor);
  prefix.setEnd(range.startContainer, range.startOffset);
  const text = needsLeadingSpace(prefix.toString()) ? ` ${trigger}` : trigger;
  document.execCommand("insertText", false, text);
}
