"use client";

import {
  Compartment,
  EditorState,
  EditorView,
  history,
  historyKeymap,
  isolateHistory,
  keymap,
  placeholder,
  Prec,
  redo,
  standardKeymap,
  Transaction,
  undo,
} from "@muteki/ui/engines/code-editor";
import { useLayoutEffect, useRef, useState, type RefObject } from "react";
import type { Edit, ReportMarkdownInput } from "@/lib/reportMarkdown";

export interface ReportEditorInput extends ReportMarkdownInput {
  readonly scrollElement: HTMLElement;
  focus(): void;
  undo(): void;
  redo(): void;
}

interface ReportTextEditorProps {
  inputRef: RefObject<ReportEditorInput | null>;
  value: string;
  label: string;
  placeholder: string;
  disabled: boolean;
  centered?: boolean;
  onChange: (value: string) => void;
  onScroll: (element: HTMLElement) => void;
  onKeyDown: (event: KeyboardEvent) => void;
  onPaste: (event: ClipboardEvent) => void;
  onDragOver: (event: DragEvent) => void;
  onDragLeave: (event: DragEvent) => void;
  onDrop: (event: DragEvent) => void;
}

function editorOptions({ disabled, label, placeholder: prompt }: ReportTextEditorProps) {
  return [
    EditorState.readOnly.of(disabled),
    EditorView.editable.of(!disabled),
    EditorView.contentAttributes.of({ "aria-label": label, "aria-disabled": String(disabled), spellcheck: "false" }),
    placeholder(prompt),
  ];
}

export function ReportTextEditor(props: ReportTextEditorProps) {
  const { inputRef, value, disabled, centered, label, placeholder: prompt } = props;
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const latest = useRef(props);
  latest.current = props;
  const lastValue = useRef(value);
  const [options] = useState(() => new Compartment());

  useLayoutEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    lastValue.current = latest.current.value;
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: lastValue.current,
        extensions: [
          EditorView.lineWrapping,
          EditorState.tabSize.of(2),
          history(),
          keymap.of([...standardKeymap, ...historyKeymap]),
          options.of(editorOptions(latest.current)),
          EditorView.updateListener.of((update) => {
            if (!update.docChanged) return;
            const next = update.state.doc.toString();
            if (next === lastValue.current) return;
            lastValue.current = next;
            latest.current.onChange(next);
          }),
          Prec.highest(EditorView.domEventHandlers({
            keydown: (event) => { latest.current.onKeyDown(event); return event.defaultPrevented; },
            paste: (event) => { latest.current.onPaste(event); return event.defaultPrevented; },
            dragover: (event) => { latest.current.onDragOver(event); return event.defaultPrevented; },
            dragleave: (event) => { latest.current.onDragLeave(event); },
            drop: (event) => { latest.current.onDrop(event); return event.defaultPrevented; },
          })),
        ],
      }),
    });
    view.scrollDOM.classList.add("cx-scroll");
    const onScroll = () => latest.current.onScroll(view.scrollDOM);
    const onHorizontalNavigation = (event: KeyboardEvent) => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      const { state } = view;
      if (state.doc.lineAt(state.selection.main.head).length > 4096) event.stopImmediatePropagation();
    };
    view.scrollDOM.addEventListener("scroll", onScroll, { passive: true });
    view.contentDOM.addEventListener("keydown", onHorizontalNavigation, true);
    viewRef.current = view;
    inputRef.current = {
      get value() { return lastValue.current; },
      get selectionStart() { return view.state.selection.main.from; },
      get selectionEnd() { return view.state.selection.main.to; },
      scrollElement: view.scrollDOM,
      focus() { view.focus(); },
      applyEdit({ start, end, text, select }: Edit) {
        const [anchor, head] = select ?? [start + text.length, start + text.length];
        view.focus();
        view.dispatch({
          changes: { from: start, to: end, insert: text },
          selection: { anchor, head },
          scrollIntoView: true,
          annotations: isolateHistory.of("full"),
        });
      },
      undo() { view.focus(); undo(view); },
      redo() { view.focus(); redo(view); },
    };
    return () => {
      inputRef.current = null;
      viewRef.current = null;
      view.scrollDOM.removeEventListener("scroll", onScroll);
      view.contentDOM.removeEventListener("keydown", onHorizontalNavigation, true);
      view.destroy();
    };
  }, [inputRef, options]);

  useLayoutEffect(() => {
    const view = viewRef.current;
    if (!view || value === lastValue.current) return;
    lastValue.current = value;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: value },
      annotations: Transaction.addToHistory.of(false),
    });
  }, [value]);

  useLayoutEffect(() => {
    viewRef.current?.dispatch({ effects: options.reconfigure(editorOptions(latest.current)) });
  }, [disabled, label, options, prompt]);

  return <div ref={hostRef} className="report-editor-area" data-centered={centered ? "true" : undefined} data-disabled={disabled || undefined} />;
}
