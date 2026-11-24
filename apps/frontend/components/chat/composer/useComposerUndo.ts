"use client";

import { useEffect, useLayoutEffect, useRef, type RefObject } from "react";
import type { PromptDocument } from "@/lib/composerContextDoc";
import { isMacPlatform } from "@/lib/shortcutBindings";
import {
  classifyInputType,
  emptyUndoState,
  recordUndo,
  redoStep,
  sealUndo,
  undoSnapshotKey,
  undoStep,
  type UndoEdit,
  type UndoEditKind,
  type UndoSelection,
  type UndoSnapshot,
  type UndoState,
} from "@/lib/composerUndoHistory";

interface PendingIntent {
  kind: UndoEditKind;
  at: number;
  data: string | null;
  selection: UndoSelection;
}

export interface UseComposerUndoOptions<A> {
  scope: string;
  doc: PromptDocument;
  attachments?: A[];
  rootRef: RefObject<HTMLElement | null>;
  editorElement: () => HTMLElement | null;
  readSelection: () => UndoSelection | null;
  applySnapshot: (snapshot: UndoSnapshot<A>) => void;
  placeSelection: (selection: UndoSelection) => void;
  composingRef: RefObject<boolean>;
}

function attachmentKey<A>(
  attachment: A,
  synthetic: WeakMap<object, string>,
  counter: { current: number },
): string {
  const id = (attachment as { id?: unknown } | null | undefined)?.id;
  if (typeof id === "string" && id) return id;
  if (typeof attachment === "object" && attachment !== null) {
    const existing = synthetic.get(attachment);
    if (existing) return existing;
    const key = `~${(counter.current += 1)}`;
    synthetic.set(attachment, key);
    return key;
  }
  return String(attachment);
}

const FALLBACK_SELECTION: UndoSelection = { start: 0, end: 0 };

export function useComposerUndo<A>(options: UseComposerUndoOptions<A>): void {
  const {
    scope,
    doc,
    attachments,
    rootRef,
    editorElement,
    composingRef,
  } = options;

  const latestRef = useRef(options);
  const historyRef = useRef<UndoState<A>>(emptyUndoState());
  const lastSeenRef = useRef<UndoSnapshot<A> | null>(null);
  const lastSelectionRef = useRef<UndoSelection>(FALLBACK_SELECTION);
  const pendingIntentRef = useRef<PendingIntent | null>(null);
  const restoringKeyRef = useRef<string | null>(null);
  const pendingSelectionRef = useRef<{ key: string; selection: UndoSelection } | null>(null);
  const scopeRef = useRef(scope);
  const latestByIdRef = useRef(new Map<string, A>());
  const syntheticKeysRef = useRef(new WeakMap<object, string>());
  const syntheticCounterRef = useRef(0);

  useLayoutEffect(() => {
    latestRef.current = options;
  });

  const trackedAttachments = attachments ?? [];
  const attachmentKeys = trackedAttachments.map((attachment) =>
    attachmentKey(attachment, syntheticKeysRef.current, syntheticCounterRef));
  const key = undoSnapshotKey(doc, attachmentKeys);

  useLayoutEffect(() => {
    const latest = latestRef.current;
    const tracked = latest.attachments ?? [];
    const snapshotKeys = tracked.map((attachment) =>
      attachmentKey(attachment, syntheticKeysRef.current, syntheticCounterRef));
    const buildSnapshot = (
      snapshotDoc: PromptDocument,
      snapshotKey: string,
      selection: UndoSelection,
    ): UndoSnapshot<A> => ({
      doc: snapshotDoc,
      key: snapshotKey,
      selection,
      attachments: tracked.slice(),
      attachmentKeys: snapshotKeys,
    });
    const currentSelection = latest.readSelection() ?? lastSelectionRef.current;

    if (scopeRef.current !== scope) {
      scopeRef.current = scope;
      historyRef.current = emptyUndoState();
      lastSeenRef.current = null;
      lastSelectionRef.current = currentSelection;
      pendingIntentRef.current = null;
      restoringKeyRef.current = null;
      pendingSelectionRef.current = null;
    }

    const restoreKey = restoringKeyRef.current;
    if (restoreKey !== null) {
      restoringKeyRef.current = null;
      const restoredSelection = pendingSelectionRef.current?.selection ?? currentSelection;
      if (key === restoreKey) {
        lastSeenRef.current = buildSnapshot(latest.doc, key, restoredSelection);
        lastSelectionRef.current = restoredSelection;
        return;
      }
      pendingSelectionRef.current = null;
      lastSeenRef.current = buildSnapshot(latest.doc, key, currentSelection);
      lastSelectionRef.current = currentSelection;
      return;
    }

    const lastSeen = lastSeenRef.current;
    if (lastSeen === null) {
      lastSeenRef.current = buildSnapshot(latest.doc, key, currentSelection);
      lastSelectionRef.current = currentSelection;
      return;
    }
    if (lastSeen.key === key) {
      pendingIntentRef.current = null;
      return;
    }

    const intent = pendingIntentRef.current;
    pendingIntentRef.current = null;
    const before: UndoSnapshot<A> = {
      ...lastSeen,
      selection: intent?.selection ?? lastSelectionRef.current,
    };
    const afterSelection = latest.readSelection() ?? before.selection;
    const after = buildSnapshot(latest.doc, key, afterSelection);
    const edit: UndoEdit = intent
      ? { kind: intent.kind, at: intent.at, data: intent.data }
      : { kind: "atomic", at: performance.now() };
    historyRef.current = recordUndo(historyRef.current, before, after, edit);
    lastSeenRef.current = after;
    lastSelectionRef.current = afterSelection;
    pendingSelectionRef.current = null;
  }, [key, scope]);

  useLayoutEffect(() => {
    const latestById = latestByIdRef.current;
    const tracked = latestRef.current.attachments ?? [];
    tracked.forEach((attachment) => {
      const attachmentKeyValue = attachmentKey(
        attachment,
        syntheticKeysRef.current,
        syntheticCounterRef,
      );
      latestById.set(attachmentKeyValue, attachment);
    });
  });

  useEffect(() => {
    const pending = pendingSelectionRef.current;
    if (!pending || pending.key !== key) return;
    pendingSelectionRef.current = null;
    latestRef.current.placeSelection(pending.selection);
  }, [key]);

  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;

    const inEditor = (target: EventTarget | null): boolean => {
      const editor = latestRef.current.editorElement();
      return editor !== null && target instanceof Node && editor.contains(target);
    };
    const selectionFor = (): UndoSelection =>
      latestRef.current.readSelection() ?? lastSelectionRef.current;

    const perform = (direction: "undo" | "redo") => {
      historyRef.current = sealUndo(historyRef.current);
      const lastSeen = lastSeenRef.current;
      if (!lastSeen) return;
      const trackedNow = latestRef.current.attachments ?? [];
      const currentKeys = trackedNow.map((attachment) =>
        attachmentKey(attachment, syntheticKeysRef.current, syntheticCounterRef));
      const current: UndoSnapshot<A> = {
        doc: latestRef.current.doc,
        key: lastSeen.key,
        selection: latestRef.current.readSelection() ?? lastSelectionRef.current,
        attachments: trackedNow.slice(),
        attachmentKeys: currentKeys,
      };
      const result = direction === "undo"
        ? undoStep(historyRef.current, current)
        : redoStep(historyRef.current, current);
      if (!result) return;
      historyRef.current = result.state;
      restoringKeyRef.current = result.snapshot.key;
      pendingSelectionRef.current = {
        key: result.snapshot.key,
        selection: result.snapshot.selection,
      };
      const restored: UndoSnapshot<A> = {
        ...result.snapshot,
        attachments: result.snapshot.attachmentKeys.map((attachmentKeyValue, index) =>
          latestByIdRef.current.get(attachmentKeyValue) ?? result.snapshot.attachments[index]),
      };
      latestRef.current.applySnapshot(restored);
    };

    const onKeyDown = (event: KeyboardEvent) => {
      if (!inEditor(event.target)) return;
      const composing = composingRef.current || event.isComposing;
      if (!composing) pendingIntentRef.current = null;
      if (composing) return;
      const keyName = event.key.toLowerCase();
      const mod = event.metaKey || event.ctrlKey;
      const undo = mod && !event.shiftKey && !event.altKey && keyName === "z";
      const redo = (mod && event.shiftKey && !event.altKey && keyName === "z")
        || (!isMacPlatform() && event.ctrlKey && !event.metaKey
          && !event.shiftKey && !event.altKey && keyName === "y");
      if (!undo && !redo) return;
      event.preventDefault();
      event.stopPropagation();
      perform(undo ? "undo" : "redo");
    };

    const onBeforeInput = (event: Event) => {
      const input = event as InputEvent;
      if (!inEditor(event.target)) return;
      const classified = classifyInputType(input.inputType || "", input.data ?? null);
      if (classified === "undo" || classified === "redo") {
        event.preventDefault();
        perform(classified);
        return;
      }
      if (classified === "ignore") return;
      if (composingRef.current || input.isComposing) return;
      pendingIntentRef.current = {
        kind: classified,
        at: event.timeStamp,
        data: input.data ?? null,
        selection: selectionFor(),
      };
    };

    const onCompositionStart = (event: Event) => {
      if (!inEditor(event.target)) return;
      pendingIntentRef.current = {
        kind: "atomic",
        at: event.timeStamp,
        data: null,
        selection: selectionFor(),
      };
    };

    const onAtomicIntent = (event: Event) => {
      if (!inEditor(event.target)) return;
      if (composingRef.current) return;
      pendingIntentRef.current = {
        kind: "atomic",
        at: event.timeStamp,
        data: null,
        selection: selectionFor(),
      };
    };

    const onFocusOut = (event: Event) => {
      if (!inEditor(event.target)) return;
      historyRef.current = sealUndo(historyRef.current);
    };

    const onSelectionChange = () => {
      if (composingRef.current || pendingIntentRef.current) return;
      const editor = editorElement();
      const active = document.activeElement;
      if (!editor || !active || !editor.contains(active)) return;
      const selection = latestRef.current.readSelection();
      if (selection) lastSelectionRef.current = selection;
    };

    root.addEventListener("keydown", onKeyDown, true);
    root.addEventListener("beforeinput", onBeforeInput, true);
    root.addEventListener("compositionstart", onCompositionStart, true);
    root.addEventListener("paste", onAtomicIntent, true);
    root.addEventListener("cut", onAtomicIntent, true);
    root.addEventListener("drop", onAtomicIntent, true);
    root.addEventListener("focusout", onFocusOut, true);
    document.addEventListener("selectionchange", onSelectionChange);
    return () => {
      root.removeEventListener("keydown", onKeyDown, true);
      root.removeEventListener("beforeinput", onBeforeInput, true);
      root.removeEventListener("compositionstart", onCompositionStart, true);
      root.removeEventListener("paste", onAtomicIntent, true);
      root.removeEventListener("cut", onAtomicIntent, true);
      root.removeEventListener("drop", onAtomicIntent, true);
      root.removeEventListener("focusout", onFocusOut, true);
      document.removeEventListener("selectionchange", onSelectionChange);
    };
  }, [rootRef, editorElement, composingRef]);
}
