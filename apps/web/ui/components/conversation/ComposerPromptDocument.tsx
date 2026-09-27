"use client";

import type { LargePasteSelection } from "@/lib/composerLargePasteKeep";

/**
 * Lightweight in-flow prompt document surface for C09 context nodes.
 * Text + non-editable ref chips share one caret; Backspace deletes a chip
 * as a unit; copy/paste round-trips structured identity via markers + MIME.
 */

import React, { useEffect, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import {
  COMPOSER_CONTEXT_CLIPBOARD_MIME,
  flattenPromptDocument,
  insertPlainTextAtMarkedRange,
  isInlineContextKind,
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

export interface ComposerPromptDocumentProps {
  document: PromptDocument;
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

function buildDom(doc: PromptDocument, root: HTMLElement): void {
  root.replaceChildren();
  for (const segment of doc.segments) {
    if (segment.type === "text") {
      root.appendChild(document.createTextNode(segment.text));
      continue;
    }
    const node = doc.nodes[segment.nodeId];
    if (!node) continue;
    root.appendChild(createChipElement(node));
  }
  if (!root.childNodes.length) {
    root.appendChild(document.createTextNode(""));
  } else {
    // Trailing text node so the caret can sit after a final chip; without it,
    // Chrome often keeps selection on the root and Backspace won't delete the chip.
    const last = root.lastChild;
    if (last?.nodeType === Node.ELEMENT_NODE && (last as HTMLElement).dataset.nodeId) {
      root.appendChild(document.createTextNode("\u200B"));
    }
  }
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

function placeCaretAtMarkedOffset(root: HTMLElement, offset: number): void {
  const selection = window.getSelection();
  if (!selection) return;
  let remaining = Math.max(0, offset);
  const range = document.createRange();
  for (const child of Array.from(root.childNodes)) {
    if (child.nodeType === Node.TEXT_NODE) {
      const raw = child.textContent || "";
      const text = raw.replace(/\u200B/g, "");
      const len = text.length;
      if (remaining <= len) {
        const rawOffset = Math.min(remaining, raw.length);
        range.setStart(child, rawOffset);
        range.collapse(true);
        selection.removeAllRanges();
        selection.addRange(range);
        return;
      }
      remaining -= len;
      continue;
    }
    if (child.nodeType !== Node.ELEMENT_NODE) continue;
    const el = child as HTMLElement;
    if (!el.dataset.nodeId) continue;
    const markerLen = `⟦ref:${el.dataset.nodeId}⟧`.length;
    if (remaining <= markerLen) {
      range.setStartAfter(el);
      range.collapse(true);
      selection.removeAllRanges();
      selection.addRange(range);
      return;
    }
    remaining -= markerLen;
  }
  range.selectNodeContents(root);
  range.collapse(false);
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
  const flatLen = flattenPromptDocument(readDocumentFromDom(root, {})).length;
  if (!selection || selection.rangeCount === 0) return flatLen;
  const range = selection.getRangeAt(0);
  if (!root.contains(range.startContainer)) return flatLen;
  return markedOffsetAtPoint(root, range.startContainer, range.startOffset);
}

/** Marked-document [start, end) for the current DOM selection (collapsed = caret). */
function markedSelectionRange(root: HTMLElement): LargePasteSelection {
  const selection = window.getSelection();
  const flatLen = flattenPromptDocument(readDocumentFromDom(root, {})).length;
  if (!selection || selection.rangeCount === 0) {
    return { start: flatLen, end: flatLen };
  }
  const range = selection.getRangeAt(0);
  if (!root.contains(range.startContainer)) {
    return { start: flatLen, end: flatLen };
  }
  const start = markedOffsetAtPoint(root, range.startContainer, range.startOffset);
  const end = root.contains(range.endContainer)
    ? markedOffsetAtPoint(root, range.endContainer, range.endOffset)
    : start;
  return start <= end ? { start, end } : { start: end, end: start };
}

function tokenAtMarked(value: string, cursor: number): {
  trigger: "/" | "@" | "$";
  query: string;
  start: number;
  end: number;
} | null {
  const before = value.slice(0, Math.max(0, cursor));
  const match = /(^|\s)([/@$])([^\s⟦]*)$/.exec(before);
  if (!match) return null;
  const trigger = match[2] as "/" | "@" | "$";
  const start = match.index + match[1].length;
  return { trigger, query: match[3], start, end: cursor };
}

export function ComposerPromptDocument({
  document: doc,
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

  const applyDocument = (next: PromptDocument): PromptDocument => {
    const cleaned = pruneUnreachableInlineNodes(next);
    const root = rootRef.current;
    knownNodesRef.current = cleaned.nodes;
    // Mark external only after DOM is rebuilt so the sync effect cannot skip
    // buildDom when React state already matches but the editor is still empty.
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

  const emitFromDom = () => {
    const root = rootRef.current;
    if (!root) return;
    const next = readDocumentFromDom(root, knownNodesRef.current);
    lastExternal.current = flattenPromptDocument(next);
    knownNodesRef.current = next.nodes;
    onDocumentChange(next);
    const offset = markedOffsetBeforeCaret(root);
    onCaretMarkedOffsetChange?.(offset);
    if (!composing) {
      onActiveTokenQuery?.(tokenAtMarked(flattenPromptDocument(next), offset));
    }
  };

  const empty = !plainTextFromDocument(doc).trim()
    && !doc.segments.some((segment) => segment.type === "ref");

  return (
    <div className={cn("relative min-w-0", className)}>
      {empty ? (
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-x-0 top-0 select-none truncate px-1 text-[14px] leading-6 text-cx-fg-4"
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
        className="cx-prompt-editor cx-scroll max-h-[288px] min-h-6 w-full overflow-y-auto whitespace-pre-wrap break-words bg-transparent px-1 text-[14px] leading-6 text-cx-fg caret-cx-accent outline-none"
        onInput={() => {
          if (composing) return;
          emitFromDom();
        }}
        onKeyDown={(event) => {
          // Chip deletion is handled by the native keydown/beforeinput listeners
          // above; keep React onKeyDown for PromptBar capability-menu keys.
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
          emitFromDom();
        }}
        onCopy={(event) => {
          const root = rootRef.current;
          if (!root) return;
          const { start, end } = markedSelectionRange(root);
          // Collapsed caret / no selection: do not hijack the clipboard.
          if (start === end) return;
          const current = readDocumentFromDom(root, knownNodesRef.current);
          const flat = flattenPromptDocument(current);
          const isFullSelect = start <= 0 && end >= flat.length;
          // Full select keeps strip chips + every node; partial keeps only the
          // selected fragment (and refs that overlap the marked window).
          const fragment = isFullSelect
            ? current
            : slicePromptDocument(current, start, end);
          const hasStructuredRefs = fragment.segments.some((seg) => seg.type === "ref")
            || (isFullSelect && Object.keys(fragment.nodes).length > 0);
          // Plain-text selection: let the browser copy the DOM selection as-is.
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
                const caret = Math.min(selection.start + flattenPromptDocument(pasted).length, flattenPromptDocument(applied).length);
                placeCaretAtMarkedOffset(root, caret);
                onCaretMarkedOffsetChange?.(caret);
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
            const caret = Math.min(selection.start + normalized.length, flattenPromptDocument(applied).length);
            placeCaretAtMarkedOffset(root, caret);
            onCaretMarkedOffsetChange?.(caret);
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
          // Large plain-text paste: offer to keep as text (truncated) or convert to attachment.
          if (!hasMime && !hasMarkers && onLargePaste && plain.length > 2000) {
            event.preventDefault();
            onLargePaste(plain, markedSelectionRange(root));
            return;
          }
          // Plain text: avoid Chrome insertText/paste block wrappers; insert
          // real `\n` into the prompt document then rebuild as text nodes
          // (`whitespace-pre-wrap` keeps display multiline).
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
            const caret = Math.min(
              start + normalized.length,
              flattenPromptDocument(next).length,
            );
            placeCaretAtMarkedOffset(root, caret);
            onCaretMarkedOffsetChange?.(caret);
            return;
          }
          const pasted = parseClipboardPayload(mime || plain);
          if (!pasted || (!Object.keys(pasted.nodes).length && !hasMarkers)) return;
          event.preventDefault();
          const current = readDocumentFromDom(root, knownNodesRef.current);
          const selection = markedSelectionRange(root);
          const base = insertPlainTextAtMarkedRange(current, "", selection.start, selection.end);
          const next = mergeClipboardIntoDocument(base, pasted, selection.start);
          // One transaction: document + DOM + caret. Never mark lastExternal
          // before buildDom (that skipped chip render on empty <br> roots).
          const applied = applyDocument(next);
          const inserted = flattenPromptDocument(pasted).length;
          const caret = Math.min(
            selection.start + inserted,
            flattenPromptDocument(applied).length,
          );
          placeCaretAtMarkedOffset(root, caret);
          onCaretMarkedOffsetChange?.(caret);
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
        "inline-flex h-6 max-w-[240px] items-center gap-1 rounded-full pl-2 text-[12px] font-medium",
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
      <Icon name={capabilityIcon(node.kind)} size={11} className="shrink-0" />
      <button
        type="button"
        className="min-w-0 truncate rounded-sm text-left enabled:hover:underline enabled:hover:underline-offset-2 disabled:cursor-default"
        onClick={onJump}
        disabled={!onJump}
        aria-label={onJump ? `跳转到引用 ${node.snapshot.label || node.name}` : undefined}
      >
        @{node.snapshot.label || node.name}
      </button>
      {badge ? <span className="shrink-0 text-[10.5px] font-normal opacity-85">{badge}</span> : null}
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
