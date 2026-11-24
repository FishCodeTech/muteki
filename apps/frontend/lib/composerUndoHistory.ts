import { flattenPromptDocument, type PromptDocument } from "@/lib/composerContextDoc";

export type UndoSelection = { start: number; end: number };
export type UndoEditKind = "type" | "delete" | "atomic";

export interface UndoSnapshot<A = unknown> {
  doc: PromptDocument;
  key: string;
  selection: UndoSelection;
  attachments: A[];
  attachmentKeys: string[];
}

export interface UndoEdit {
  kind: UndoEditKind;
  at: number;
  data?: string | null;
}

export interface UndoState<A = unknown> {
  undo: UndoSnapshot<A>[];
  redo: UndoSnapshot<A>[];
  open: {
    kind: "type" | "delete";
    lastAt: number;
    lastSelection: UndoSelection;
    endsWithSpace: boolean;
  } | null;
}

export const UNDO_GROUP_DELAY_MS = 500;
export const UNDO_MAX_DEPTH = 100;

type UndoOpenGroup = NonNullable<UndoState["open"]>;

export function emptyUndoState<A>(): UndoState<A> {
  return { undo: [], redo: [], open: null };
}

export function undoSnapshotKey(doc: PromptDocument, attachmentKeys: string[]): string {
  return `${flattenPromptDocument(doc)}\u0000${attachmentKeys.join("\u0001")}`;
}

function startsWithNonSpace(data: string | null | undefined): boolean {
  return data != null && data.length > 0 && !/^\s/.test(data);
}

export function recordUndo<A>(
  state: UndoState<A>,
  before: UndoSnapshot<A>,
  after: UndoSnapshot<A>,
  edit: UndoEdit,
): UndoState<A> {
  if (before.key === after.key) return state;
  const grouping = edit.kind === "type" || edit.kind === "delete" ? edit.kind : null;
  const open = state.open;
  const merge = open !== null
    && grouping !== null
    && grouping === open.kind
    && edit.at - open.lastAt <= UNDO_GROUP_DELAY_MS
    && before.selection.start === before.selection.end
    && before.selection.start === open.lastSelection.start
    && before.selection.end === open.lastSelection.end
    && !(grouping === "type" && open.endsWithSpace && startsWithNonSpace(edit.data));
  const nextOpen: UndoOpenGroup | null = grouping !== null
    ? {
        kind: grouping,
        lastAt: edit.at,
        lastSelection: after.selection,
        endsWithSpace: /\s$/.test(edit.data ?? ""),
      }
    : null;
  if (merge) return { undo: state.undo, redo: [], open: nextOpen };
  const undo = [...state.undo, before];
  if (undo.length > UNDO_MAX_DEPTH) undo.shift();
  return { undo, redo: [], open: nextOpen };
}

export function undoStep<A>(
  state: UndoState<A>,
  current: UndoSnapshot<A>,
): { state: UndoState<A>; snapshot: UndoSnapshot<A> } | null {
  const snapshot = state.undo[state.undo.length - 1];
  if (!snapshot) return null;
  return {
    state: {
      undo: state.undo.slice(0, -1),
      redo: [...state.redo, current],
      open: null,
    },
    snapshot,
  };
}

export function redoStep<A>(
  state: UndoState<A>,
  current: UndoSnapshot<A>,
): { state: UndoState<A>; snapshot: UndoSnapshot<A> } | null {
  const snapshot = state.redo[state.redo.length - 1];
  if (!snapshot) return null;
  return {
    state: {
      undo: [...state.undo, current],
      redo: state.redo.slice(0, -1),
      open: null,
    },
    snapshot,
  };
}

export function sealUndo<A>(state: UndoState<A>): UndoState<A> {
  return { undo: state.undo, redo: state.redo, open: null };
}

export function classifyInputType(
  inputType: string,
  data: string | null,
): UndoEditKind | "undo" | "redo" | "ignore" {
  if (inputType === "historyUndo") return "undo";
  if (inputType === "historyRedo") return "redo";
  if (inputType === "insertText") return data != null && data.includes("\n") ? "atomic" : "type";
  if (inputType === "deleteContentBackward" || inputType === "deleteContentForward") return "delete";
  if (
    inputType === "insertCompositionText"
    || inputType === "deleteCompositionText"
    || inputType === "insertFromComposition"
  ) return "ignore";
  return "atomic";
}
