"use client";

import type { LargePasteSelection } from "@/lib/composerLargePasteKeep";


import React, { useEffect, useImperativeHandle, useRef, useState, type Ref } from "react";
import { cn } from "@/lib/cn";
import type { UndoSelection } from "@/lib/composerUndoHistory";
import { Icon } from "@muteki/ui";
import {
  COMPOSER_CONTEXT_CLIPBOARD_MIME,
  flattenPromptDocument,
  insertPlainTextAtMarkedRange,
  mergeClipboardIntoDocument,
  parseClipboardPayload,
  plainTextFromDocument,
  pruneUnreachableInlineNodes,
  removeNode,
  serializeClipboardPayload,
  slicePromptDocument,
  type ComposerContextNode,
  type PromptDocument,
} from "@/lib/composerContextDoc";
import { inspectComposerClipboardPaste } from "@/lib/composerAttachments";
import {
  composerMarkedOffsetAtDomPoint,
  readComposerDomSegments,
} from "@/lib/composerDomSerialize";
import {
  capabilityIcon,
  refStatusLabel as statusLabel,
  refStatusTone,
} from "../chat/composer/capabilityMeta";

const CHIP_CLASS = "cx-ref-chip";

function chipClassName(status: string): string {
  return `${CHIP_CLASS} is-${refStatusTone(status)}`;
}

export interface ComposerPromptDocumentHandle {
  getMarkedSelection(): UndoSelection | null;
  setMarkedSelection(selection: UndoSelection): void;
}

export interface ComposerPromptDocumentProps {
  document: PromptDocument;
  ref?: Ref<ComposerPromptDocumentHandle>;
  onDocumentChange: (doc: PromptDocument) => void;
  onCaretMarkedOffsetChange?: (offset: number) => void;
  placeholder?: string;
  onComposingChange?: (composing: boolean) => void;
  onKeyDown?: (event: React.KeyboardEvent<HTMLDivElement>) => void;
  onActiveTokenQuery?: (payload: {
    trigger: "/" | "@" | "$";
    query: string;
    start: number;
    end: number;
  } | null) => void;
  className?: string;
  "aria-controls"?: string;
  "aria-expanded"?: boolean;
  "aria-activedescendant"?: string;
  "aria-label"?: string;
  "data-c34-composer"?: string;
  disabled?: boolean;
  onLargePaste?: (text: string, selection?: LargePasteSelection) => void;
  onAddFiles?: (files: File[]) => void;
  onPasteHint?: (message: string) => void;
}

function* editorTextRuns(text: string) {
  const targetLength = 16 * 1024;
  let start = 0;
  while (text.length - start > targetLength) {
    const boundary = start + targetLength;
    const window = text.slice(start, boundary);
    const newline = window.lastIndexOf("\n");
    const naturalBreak = newline >= 0
      ? newline
      : Math.max(window.lastIndexOf(" "), window.lastIndexOf("\t"));
    const separator = naturalBreak >= 0 ? start + naturalBreak : text.indexOf("\n", boundary);
    if (separator === -1 || separator === text.length - 1) break;
    const end = separator + 1;
    yield text.slice(start, end);
    start = end;
  }
  yield text.slice(start);
}

function buildDom(doc: PromptDocument, root: HTMLElement): void {
  const text = doc.segments.length === 1 && doc.segments[0].type === "text"
    ? doc.segments[0].text
    : undefined;
  if (text !== undefined) {
    let current = root.firstChild;
    for (const run of editorTextRuns(text)) {
      if (current?.nodeType === Node.TEXT_NODE) {
        if (current.nodeValue !== run) current.nodeValue = run;
        current = current.nextSibling;
      } else {
        root.insertBefore(root.ownerDocument.createTextNode(run), current);
      }
    }
    while (current) {
      const next = current.nextSibling;
      root.removeChild(current);
      current = next;
    }
    return;
  }
  const content = root.ownerDocument.createDocumentFragment();
  for (const segment of doc.segments) {
    if (segment.type === "text") {
      for (const run of editorTextRuns(segment.text)) {
        content.appendChild(root.ownerDocument.createTextNode(run));
      }
      continue;
    }
    const node = doc.nodes[segment.nodeId];
    if (!node) continue;
    content.appendChild(createChipElement(node));
  }
  if (!content.childNodes.length) {
    content.appendChild(root.ownerDocument.createTextNode(""));
  } else {
    const last = content.lastChild;
    if (last?.nodeType === Node.ELEMENT_NODE && (last as HTMLElement).dataset.nodeId) {
      content.appendChild(root.ownerDocument.createTextNode("\u200B"));
    }
  }
  root.replaceChildren(content);
}

function chipBeforeCaret(root: HTMLElement): HTMLElement | null {
  const selection = window.getSelection();
  if (!root || !selection?.isCollapsed || !selection.rangeCount) return null;
  const range = selection.getRangeAt(0);
  const { startContainer, startOffset } = range;
  if (startContainer === root && startOffset > 0) {
    const prev = root.childNodes[startOffset - 1];
    if (prev?.nodeType === Node.ELEMENT_NODE && (prev as HTMLElement).dataset.nodeId) {
      return prev as HTMLElement;
    }
  }
  if (startContainer.nodeType === Node.TEXT_NODE && startOffset === 0) {
    const prev = startContainer.previousSibling;
    if (prev?.nodeType === Node.ELEMENT_NODE && (prev as HTMLElement).dataset.nodeId) {
      return prev as HTMLElement;
    }
  }
  if (startContainer.nodeType === Node.TEXT_NODE && startOffset <= 1) {
    const text = startContainer.textContent || "";
    if ((text === "" || text === "\u200B") && startContainer.previousSibling?.nodeType === Node.ELEMENT_NODE) {
      const prev = startContainer.previousSibling as HTMLElement;
      if (prev.dataset.nodeId) return prev;
    }
  }
  if (
    startContainer.nodeType === Node.ELEMENT_NODE
    && (startContainer as HTMLElement).dataset.nodeId
  ) {
    return startContainer as HTMLElement;
  }
  return null;
}

function chipAfterCaret(root: HTMLElement): HTMLElement | null {
  const selection = window.getSelection();
  if (!root || !selection?.isCollapsed || !selection.rangeCount) return null;
  const range = selection.getRangeAt(0);
  const { startContainer, startOffset } = range;
  if (startContainer === root && startOffset < root.childNodes.length) {
    const next = root.childNodes[startOffset];
    if (next?.nodeType === Node.ELEMENT_NODE && (next as HTMLElement).dataset.nodeId) {
      return next as HTMLElement;
    }
  }
  if (
    startContainer.nodeType === Node.TEXT_NODE
    && startOffset >= (startContainer.textContent || "").length
  ) {
    const next = startContainer.nextSibling;
    if (next?.nodeType === Node.ELEMENT_NODE && (next as HTMLElement).dataset.nodeId) {
      return next as HTMLElement;
    }
  }
  return null;
}

function createChipElement(node: ComposerContextNode): HTMLSpanElement {
  const chip = document.createElement("span");
  chip.className = chipClassName(node.status);
  chip.contentEditable = "false";
  chip.dataset.nodeId = node.node_id;
  chip.dataset.kind = node.kind;
  chip.dataset.status = node.status;
  chip.title = [
    node.snapshot.label || node.name,
    node.status_reason || "",
    node.snapshot.text.slice(0, 240),
  ].filter(Boolean).join("\n");
  const label = document.createElement("span");
  label.className = "cx-ref-chip-label";
  label.textContent = `@${node.snapshot.label || node.name}`;
  chip.appendChild(label);
  const badge = statusLabel(node.status);
  if (badge) {
    const mark = document.createElement("span");
    mark.className = "cx-ref-chip-status";
    mark.textContent = badge;
    chip.appendChild(mark);
  }
  return chip;
}

function markedDomPointAtOffset(root: HTMLElement, offset: number): { node: Node; offset: number } {
  let remaining = Math.max(0, offset);
  const children = Array.from(root.childNodes);
  for (let index = 0; index < children.length; index += 1) {
    const child = children[index];
    if (child.nodeType === Node.TEXT_NODE) {
      const raw = child.textContent || "";
      const text = raw.replace(/\u200B/g, "");
      const len = text.length;
      if (remaining <= len) {
        return { node: child, offset: Math.min(remaining, raw.length) };
      }
      remaining -= len;
      continue;
    }
    if (child.nodeType !== Node.ELEMENT_NODE) continue;
    const el = child as HTMLElement;
    if (!el.dataset.nodeId) continue;
    const markerLen = `⟦ref:${el.dataset.nodeId}⟧`.length;
    if (remaining <= markerLen) {
      const parent = el.parentNode ?? root;
      return { node: parent, offset: Array.from(parent.childNodes).indexOf(el) + 1 };
    }
    remaining -= markerLen;
  }
  return { node: root, offset: children.length };
}

function placeCaretAtMarkedOffset(root: HTMLElement, start: number, end: number = start): void {
  const selection = window.getSelection();
  if (!selection) return;
  const from = Math.min(start, end);
  const to = Math.max(start, end);
  const startPoint = markedDomPointAtOffset(root, from);
  const endPoint = markedDomPointAtOffset(root, to);
  const range = document.createRange();
  range.setStart(startPoint.node, startPoint.offset);
  range.setEnd(endPoint.node, endPoint.offset);
  selection.removeAllRanges();
  selection.addRange(range);
}

function readDocumentFromDom(
  root: HTMLElement,
  known: Record<string, ComposerContextNode>,
): PromptDocument {
  const nodes: Record<string, ComposerContextNode> = { ...known };
  const segments = readComposerDomSegments(root, nodes);
  return pruneUnreachableInlineNodes({ segments, nodes });
}

function markedOffsetAtPoint(
  root: HTMLElement,
  container: Node,
  pointOffset: number,
): number {
  return composerMarkedOffsetAtDomPoint(root, container, pointOffset);
}

function markedOffsetBeforeCaret(root: HTMLElement): number {
  const selection = window.getSelection();
  if (selection?.rangeCount) {
    const range = selection.getRangeAt(0);
    if (root.contains(range.startContainer)) {
      return markedOffsetAtPoint(root, range.startContainer, range.startOffset);
    }
  }
  return flattenPromptDocument(readDocumentFromDom(root, {})).length;
}

function markedSelectionRange(root: HTMLElement): LargePasteSelection {
  const selection = window.getSelection();
  if (selection?.rangeCount) {
    const range = selection.getRangeAt(0);
    if (root.contains(range.startContainer)) {
      const start = markedOffsetAtPoint(root, range.startContainer, range.startOffset);
      const end = !range.collapsed && root.contains(range.endContainer)
        ? markedOffsetAtPoint(root, range.endContainer, range.endOffset)
        : start;
      return start <= end ? { start, end } : { start: end, end: start };
    }
  }
  const flatLen = flattenPromptDocument(readDocumentFromDom(root, {})).length;
  return { start: flatLen, end: flatLen };
}

const TOKEN_BOUNDARY = /[\s⟦]/;

function tokenAtMarked(value: string, cursor: number): {
  trigger: "/" | "@" | "$";
  query: string;
  start: number;
  end: number;
} | null {
  const end = Math.min(value.length, Math.max(0, cursor));
  let start = end;
  while (start > 0 && !TOKEN_BOUNDARY.test(value[start - 1])) start -= 1;
  if (start === end || value[start - 1] === "⟦") return null;
  const trigger = value[start];
  if (trigger !== "/" && trigger !== "@" && trigger !== "$") return null;
  return { trigger, query: value.slice(start + 1, end), start, end: cursor };
}

export function ComposerPromptDocument({
  document: doc,
  ref,
  onDocumentChange,
  onCaretMarkedOffsetChange,
  placeholder = "",
  onComposingChange,
  onKeyDown,
  onActiveTokenQuery,
  onLargePaste,
  onAddFiles,
  onPasteHint,
  className = "",
  disabled = false,
  "aria-label": ariaLabel,
  "data-c34-composer": dataC34Composer,
  ...aria
}: ComposerPromptDocumentProps) {
  const rootRef = useRef<HTMLDivElement>(null);
  const knownNodesRef = useRef(doc.nodes);
  const [composing, setComposing] = useState(false);
  const lastExternal = useRef("");

  useImperativeHandle(ref, () => ({
    getMarkedSelection: () => {
      const root = rootRef.current;
      if (!root) return null;
      const selection = window.getSelection();
      if (!selection?.rangeCount) return null;
      const range = selection.getRangeAt(0);
      if (!root.contains(range.startContainer)) return null;
      const { start, end } = markedSelectionRange(root);
      return { start, end };
    },
    setMarkedSelection: (selection: UndoSelection) => {
      const root = rootRef.current;
      if (!root) return;
      placeCaretAtMarkedOffset(root, selection.start, selection.end);
    },
  }), []);

  const publishCaret = (marked: string, offset: number, compositionActive = composing) => {
    onCaretMarkedOffsetChange?.(offset);
    if (!compositionActive) {
      onActiveTokenQuery?.(tokenAtMarked(marked, offset));
    }
  };

  const applyDocument = (next: PromptDocument): PromptDocument => {
    const cleaned = pruneUnreachableInlineNodes(next);
    const root = rootRef.current;
    knownNodesRef.current = cleaned.nodes;
    if (root) buildDom(cleaned, root);
    lastExternal.current = flattenPromptDocument(cleaned);
    onDocumentChange(cleaned);
    return cleaned;
  };

  useEffect(() => {
    knownNodesRef.current = doc.nodes;
    const flat = flattenPromptDocument(doc);
    const root = rootRef.current;
    if (!root) return;
    if (flat === lastExternal.current && root.childNodes.length) {
      if (!Object.keys(doc.nodes).length) return;
      root.querySelectorAll<HTMLElement>("[data-node-id]").forEach((chip) => {
        const node = doc.nodes[chip.dataset.nodeId || ""];
        if (!node) return;
        chip.dataset.status = node.status;
        chip.className = chipClassName(node.status);
        const label = chip.querySelector(".cx-ref-chip-label");
        if (label) label.textContent = `@${node.snapshot.label || node.name}`;
        let badge = chip.querySelector(".cx-ref-chip-status");
        const text = statusLabel(node.status);
        if (text) {
          if (!badge) {
            badge = document.createElement("span");
            badge.className = "cx-ref-chip-status";
            chip.appendChild(badge);
          }
          badge.textContent = text;
        } else if (badge) {
          badge.remove();
        }
      });
      return;
    }
    lastExternal.current = flat;
    buildDom(doc, root);
  }, [doc]);

  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;
    const deleteAdjacentChip = (direction: "backward" | "forward") => {
      if (composing) return false;
      const chip = direction === "backward" ? chipBeforeCaret(root) : chipAfterCaret(root);
      if (!chip?.dataset.nodeId) return false;
      const next = removeNode(
        readDocumentFromDom(root, knownNodesRef.current),
        chip.dataset.nodeId,
      );
      applyDocument(next);
      return true;
    };
    const onBeforeInput = (event: Event) => {
      const input = event as InputEvent;
      if (input.inputType === "deleteContentBackward") {
        if (deleteAdjacentChip("backward")) event.preventDefault();
        return;
      }
      if (input.inputType === "deleteContentForward") {
        if (deleteAdjacentChip("forward")) event.preventDefault();
      }
    };
    const onKeyDownNative = (event: KeyboardEvent) => {
      if (event.isComposing) return;
      if (event.key === "Backspace" && deleteAdjacentChip("backward")) {
        event.preventDefault();
        event.stopPropagation();
        return;
      }
      if (event.key === "Delete" && deleteAdjacentChip("forward")) {
        event.preventDefault();
        event.stopPropagation();
      }
    };
    root.addEventListener("beforeinput", onBeforeInput);
    root.addEventListener("keydown", onKeyDownNative);
    return () => {
      root.removeEventListener("beforeinput", onBeforeInput);
      root.removeEventListener("keydown", onKeyDownNative);
    };
  });

  const emitFromDom = (compositionActive = composing) => {
    const root = rootRef.current;
    if (!root) return;
    const next = readDocumentFromDom(root, knownNodesRef.current);
    const marked = flattenPromptDocument(next);
    lastExternal.current = marked;
    knownNodesRef.current = next.nodes;
    onDocumentChange(next);
    const offset = markedOffsetBeforeCaret(root);
    publishCaret(marked, offset, compositionActive);
  };

  const empty = !composing
    && !plainTextFromDocument(doc).trim()
    && !doc.segments.some((segment) => segment.type === "ref");

  return (
    <div className={cn("relative min-w-0", className)}>
      {empty ? (
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-x-0 top-0 select-none truncate px-1 text-[length:var(--cx-msg-fs,15px)] leading-[1.6] text-cx-fg-4"
        >
          {placeholder}
        </div>
      ) : null}
      <div
        ref={rootRef}
        role="textbox"
        aria-multiline="true"
        aria-autocomplete="list"
        aria-haspopup="listbox"
        aria-controls={aria["aria-controls"]}
        aria-expanded={aria["aria-expanded"]}
        aria-activedescendant={aria["aria-activedescendant"]}
        aria-label={ariaLabel}
        data-c34-composer={dataC34Composer}
        contentEditable={!disabled}
        suppressContentEditableWarning
        data-composer-prompt-document="1"
        aria-placeholder={placeholder || undefined}
        className="cx-prompt-editor cx-scroll max-h-[288px] min-h-[48px] w-full overflow-y-auto whitespace-pre-wrap break-words bg-transparent px-1 text-[length:var(--cx-msg-fs,15px)] leading-[1.6] text-cx-fg caret-cx-accent outline-none"
        onInput={() => {
          if (composing) return;
          emitFromDom(false);
        }}
        onKeyDown={(event) => {
          onKeyDown?.(event);
        }}
        onClick={() => {
          const root = rootRef.current;
          if (!root) return;
          const offset = markedOffsetBeforeCaret(root);
          onCaretMarkedOffsetChange?.(offset);
          onActiveTokenQuery?.(tokenAtMarked(
            flattenPromptDocument(readDocumentFromDom(root, knownNodesRef.current)),
            offset,
          ));
        }}
        onCompositionStart={() => {
          setComposing(true);
          onComposingChange?.(true);
        }}
        onCompositionEnd={() => {
          setComposing(false);
          onComposingChange?.(false);
          emitFromDom(false);
        }}
        onCopy={(event) => {
          const root = rootRef.current;
          if (!root) return;
          const { start, end } = markedSelectionRange(root);
          if (start === end) return;
          const current = readDocumentFromDom(root, knownNodesRef.current);
          const flat = flattenPromptDocument(current);
          const isFullSelect = start <= 0 && end >= flat.length;
          const fragment = isFullSelect
            ? current
            : slicePromptDocument(current, start, end);
          const hasStructuredRefs = fragment.segments.some((seg) => seg.type === "ref")
            || (isFullSelect && Object.keys(fragment.nodes).length > 0);
          if (!hasStructuredRefs) return;
          event.clipboardData.setData(
            COMPOSER_CONTEXT_CLIPBOARD_MIME,
            serializeClipboardPayload(fragment),
          );
          event.clipboardData.setData("text/plain", flattenPromptDocument(fragment));
          event.preventDefault();
        }}
        onPaste={(event) => {
          const root = rootRef.current;
          if (!root) return;
          const plan = inspectComposerClipboardPaste(event.clipboardData);
          const mime = event.clipboardData.getData(COMPOSER_CONTEXT_CLIPBOARD_MIME);
          const plain = plan.text || event.clipboardData.getData("text/plain");
          const hasMime = Boolean(mime);
          const hasMarkers = /⟦ref:/.test(plain || "");
          const applyStructuredOrPlain = () => {
            if (hasMime || hasMarkers) {
              const pasted = parseClipboardPayload(mime || plain);
              if (pasted && (Object.keys(pasted.nodes).length || hasMarkers)) {
                const current = readDocumentFromDom(root, knownNodesRef.current);
                const selection = markedSelectionRange(root);
                const base = insertPlainTextAtMarkedRange(current, "", selection.start, selection.end);
                const applied = applyDocument(mergeClipboardIntoDocument(base, pasted, selection.start));
                const marked = flattenPromptDocument(applied);
                const caret = Math.min(selection.start + flattenPromptDocument(pasted).length, marked.length);
                placeCaretAtMarkedOffset(root, caret);
                publishCaret(marked, caret);
                return;
              }
            }
            if (!plain) return;
            const selection = markedSelectionRange(root);
            if (!hasMime && !hasMarkers && onLargePaste && plain.length > 2000) {
              onLargePaste(plain, selection);
              return;
            }
            const current = readDocumentFromDom(root, knownNodesRef.current);
            const normalized = plain.replace(/\r\n?/g, "\n");
            const applied = applyDocument(insertPlainTextAtMarkedRange(current, normalized, selection.start, selection.end));
            const marked = flattenPromptDocument(applied);
            const caret = Math.min(selection.start + normalized.length, marked.length);
            placeCaretAtMarkedOffset(root, caret);
            publishCaret(marked, caret);
          };
          if (plan.files.length) {
            event.preventDefault();
            if (onAddFiles) onAddFiles(plan.files);
            else onPasteHint?.("当前无法添加附件");
            if (plain.trim() || hasMime || hasMarkers) applyStructuredOrPlain();
            return;
          }
          if (plan.hadFilePayload) {
            event.preventDefault();
            if (plan.hint) onPasteHint?.(plan.hint);
            if (plain.trim() || hasMime || hasMarkers) applyStructuredOrPlain();
            return;
          }
          if (!hasMime && !hasMarkers && onLargePaste && plain.length > 2000) {
            event.preventDefault();
            onLargePaste(plain, markedSelectionRange(root));
            return;
          }
          if (!hasMime && !hasMarkers) {
            if (!plain) return;
            event.preventDefault();
            const normalized = plain.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
            const current = readDocumentFromDom(root, knownNodesRef.current);
            const selection = window.getSelection();
            let start = markedOffsetBeforeCaret(root);
            let end = start;
            if (selection && !selection.isCollapsed && selection.rangeCount > 0) {
              const range = selection.getRangeAt(0);
              if (root.contains(range.endContainer)) {
                end = markedOffsetAtPoint(root, range.endContainer, range.endOffset);
              }
              if (end < start) {
                const tmp = start;
                start = end;
                end = tmp;
              }
            }
            const next = insertPlainTextAtMarkedRange(current, normalized, start, end);
            applyDocument(next);
            const marked = flattenPromptDocument(next);
            const caret = Math.min(start + normalized.length, marked.length);
            placeCaretAtMarkedOffset(root, caret);
            publishCaret(marked, caret);
            return;
          }
          const pasted = parseClipboardPayload(mime || plain);
          if (!pasted || (!Object.keys(pasted.nodes).length && !hasMarkers)) return;
          event.preventDefault();
          const current = readDocumentFromDom(root, knownNodesRef.current);
          const selection = markedSelectionRange(root);
          const base = insertPlainTextAtMarkedRange(current, "", selection.start, selection.end);
          const next = mergeClipboardIntoDocument(base, pasted, selection.start);
          const applied = applyDocument(next);
          const inserted = flattenPromptDocument(pasted).length;
          const marked = flattenPromptDocument(applied);
          const caret = Math.min(selection.start + inserted, marked.length);
          placeCaretAtMarkedOffset(root, caret);
          publishCaret(marked, caret);
        }}
      />
    </div>
  );
}

export function ContextNodeChip({
  node,
  onRemove,
  onJump,
}: {
  node: ComposerContextNode;
  onRemove?: () => void;
  onJump?: () => void;
}) {
  const tone = refStatusTone(node.status);
  const badge = statusLabel(node.status);
  return (
    <span
      className={cn(
        "inline-flex h-6 max-w-[240px] items-center gap-1 rounded-full pl-2 text-cx-xs font-medium",
        onRemove ? "pr-0.5" : "pr-2",
        tone === "ok" && "bg-cx-accent-soft text-cx-accent",
        tone === "stale" && "bg-cx-warning-soft text-cx-warning",
        tone === "missing" && "bg-cx-danger-soft text-cx-danger",
      )}
      title={[
        node.snapshot.label || node.name,
        node.status_reason || "",
        node.snapshot.text.slice(0, 240),
      ].filter(Boolean).join("\n")}
      data-node-id={node.node_id}
      data-ref-status={tone}
    >
      <Icon name={capabilityIcon(node.kind, node.id)} size={11} className="shrink-0" />
      <button
        type="button"
        className="min-w-0 truncate rounded-sm text-left enabled:hover:underline enabled:hover:underline-offset-2 disabled:cursor-default"
        onClick={onJump}
        disabled={!onJump}
        aria-label={onJump ? `跳转到引用 ${node.snapshot.label || node.name}` : undefined}
      >
        @{node.snapshot.label || node.name}
      </button>
      {badge ? <span className="shrink-0 text-cx-xs font-normal opacity-85">{badge}</span> : null}
      {onRemove ? (
        <button
          type="button"
          aria-label={`移除 ${node.name}`}
          onClick={onRemove}
          className="grid size-5 shrink-0 place-items-center rounded-full opacity-60 hover:bg-[color-mix(in_srgb,currentColor_14%,transparent)] hover:opacity-100"
        >
          <Icon name="x" size={10} />
        </button>
      ) : null}
    </span>
  );
}
