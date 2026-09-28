"use client";

/* ─────────────────────────────────────────────────────────
 * PROMPT BAR — Unified composer with model picker, reasoning, attachments & controls.
 *
 * Source: https://github.com/TurboKach/ai-native-react-components
 * Pinned Commit: 05dab2d2b5f1f3e40029776e339a486d70491079
 * License: MIT
 * Adapted for Project Muteki: Full agent prompt bar with model selection,
 * reasoning intensity, permission mode, attachments, workspace pill, and stop/send states.
 * ───────────────────────────────────────────────────────── */

import React, { useCallback, useEffect, useId, useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import {
  Callout,
  IconButton,
  Select,
  useReducedMotion,
  type ListOption,
} from "@/components/chat/ui";
import {
  fetchComposerCapabilities,
  type ComposerCapabilityContext,
  type ComposerCapabilityItem,
  type ComposerCapabilityRef,
  type ComposerRuntimeState,
  type ComposerTrigger,
} from "@/lib/composerCapabilities";
import {
  applyCatalogCapability,
  createFileContextNode,
  documentFromPromptAndRefs,
  emptyPromptDocument,
  flattenPromptDocument,
  insertNodeAtCaret,
  isInlineContextKind,
  plainTextFromDocument,
  refsFromDocument,
  removeNode,
  type PromptDocument,
} from "@/lib/composerContextDoc";
import type { LargePasteSelection } from "@/lib/composerLargePasteKeep";
import {
  dataTransferHasFiles,
  filesFromDataTransfer,
  inspectComposerClipboardPaste,
} from "@/lib/composerAttachments";
import { ComposerPromptDocument } from "../conversation/ComposerPromptDocument";
import { AttachmentTray } from "../chat/composer/AttachmentTray";
import { CapabilityMenu } from "../chat/composer/CapabilityMenu";
import { ComposerAddMenu } from "../chat/composer/ComposerAddMenu";
import { SubmitCluster } from "../chat/composer/SubmitCluster";
import { captureCaret, insertTriggerAtCaret, type EditorCaret } from "../chat/composer/caret";
import { capabilityDisabled } from "../chat/composer/capabilityMeta";

export interface PromptBarAttachment {
  id?: string;
  name: string;
  size?: number;
  file?: File;
  type?: string;
  sha256?: string;
  /** Restored from storage without a File handle or upload hash. */
  needsReselect?: boolean;
  /** Per-file upload lifecycle state. */
  uploadStatus?: "pending" | "uploading" | "done" | "error";
  /** Error message when uploadStatus === 'error'. */
  uploadError?: string;
  /** Optional 0–1 upload progress; the ring is indeterminate when absent. */
  uploadProgress?: number;
}

export interface ModelOption {
  id: string;
  label: string;
  engine?: string;
  group?: string;
  reasoningLevels?: string[];
  defaultEffort?: string;
}

export interface PromptBarProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  onSteer?: () => void;
  onStop?: () => void;
  canSteer?: boolean;
  steerDisabledReason?: string;
  steerAlternative?: string;
  running?: boolean;
  busy?: boolean;
  /** When true, keep the composer editable but block send/steer submit. */
  submitDisabled?: boolean;
  placeholder?: string;
  /** Accessible label for the composer textarea (C34). */
  composerLabel?: string;
  models?: ModelOption[];
  selectedModel?: string;
  onSelectModel?: (modelId: string) => void;
  effort?: string;
  onSelectEffort?: (effort: string) => void;
  attachments?: PromptBarAttachment[];
  onAddAttachment?: () => void;
  onAddFiles?: (files: File[]) => void;
  onRemoveAttachment?: (index: number) => void;
  contextPill?: { label: string; tooltip?: string; onClick?: () => void };
  capabilityContext?: ComposerCapabilityContext;
  capabilityRefs?: ComposerCapabilityRef[];
  onCapabilityRefsChange?: (refs: ComposerCapabilityRef[]) => void;
  /** C09 structured prompt document. When set, drives the inline editor. */
  promptDocument?: PromptDocument;
  onPromptDocumentChange?: (doc: PromptDocument) => void;
  onComposerCommand?: (action: string) => void;
  capabilityBanner?: string;
  onDismissCapabilityBanner?: () => void;
  onRetryAttachment?: (index: number) => void;
  onLargePaste?: (text: string, selection?: LargePasteSelection) => void;
  extraControls?: React.ReactNode;
  /** Right-aligned toolbar slot rendered just before the send button (e.g. a context meter). */
  contextMeter?: React.ReactNode;
  className?: string;
  /** C10: empty-box ArrowUp/Down history. Return true to preventDefault. */
  onHistoryNavigate?: (direction: "older" | "newer") => boolean;
  historyBrowsing?: boolean;
  historyStatus?: string;
  onStashShortcut?: () => void;
  onOpenStashPanel?: () => void;
  stashCount?: number;
}

interface ActiveComposerToken {
  trigger: ComposerTrigger;
  query: string;
  start: number;
  end: number;
}

const LEGACY_LINE_PX = 24;
const LEGACY_MAX_LINES = 12;

function composerTokenAt(value: string, cursor: number): ActiveComposerToken | null {
  const before = value.slice(0, Math.max(0, cursor));
  const match = /(^|\s)([/@$])([^\s]*)$/.exec(before);
  if (!match) return null;
  const trigger = match[2] as ComposerTrigger;
  const start = match.index + match[1].length;
  return { trigger, query: match[3], start, end: cursor };
}

function LegacyModelControls({
  models,
  selectedModel,
  onSelectModel,
  effort,
  onSelectEffort,
}: {
  models: ModelOption[];
  selectedModel: string;
  onSelectModel?: (modelId: string) => void;
  effort: string;
  onSelectEffort?: (effort: string) => void;
}) {
  const current = models.find((m) => m.id === selectedModel) || models[0];
  const modelOptions: ListOption[] = models.map((model) => ({
    value: model.id,
    label: model.label,
    section: model.group,
    trailing: model.engine ? <span className="font-cx-mono text-[11px] text-cx-fg-4">{model.engine}</span> : undefined,
  }));
  const levels = current?.reasoningLevels || [];
  const effortOptions: ListOption[] = [
    { value: "", label: "推理：默认", textValue: "默认" },
    ...levels.map((level) => ({ value: level, label: `推理：${level}`, textValue: level })),
  ];
  const pill = "cx-press inline-flex h-8 min-w-0 items-center gap-1.5 rounded-full px-2.5 text-[12.5px] font-medium text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg";
  return (
    <>
      {models.length > 0 && onSelectModel ? (
        <Select
          value={current?.id ?? null}
          onChange={onSelectModel}
          options={modelOptions}
          placement="top-start"
          ariaLabel="可用模型"
          searchable={models.length > 8}
          popoverClassName="w-[280px]"
          trigger={(
            <button type="button" aria-label="选择模型" className={pill}>
              <span className="size-1.5 shrink-0 rounded-full bg-cx-accent" />
              <span className="max-w-[160px] truncate">{current?.label || selectedModel || "选择模型"}</span>
              <Icon name="chevronDown" size={12} className="text-cx-fg-4" />
            </button>
          )}
        />
      ) : null}
      {levels.length > 0 && onSelectEffort ? (
        <Select
          value={effort || current?.defaultEffort || ""}
          onChange={onSelectEffort}
          options={effortOptions}
          placement="top-start"
          ariaLabel="推理强度"
          popoverClassName="w-[200px]"
          trigger={(
            <button type="button" aria-label="推理强度" className={pill}>
              <Icon name="brain" size={13} className="text-cx-fg-3" />
              <span className="truncate">{effort || current?.defaultEffort || "默认"}</span>
              <Icon name="chevronDown" size={12} className="text-cx-fg-4" />
            </button>
          )}
        />
      ) : null}
    </>
  );
}

export function PromptBar({
  value,
  onChange,
  onSubmit,
  onSteer,
  onStop,
  canSteer = false,
  steerDisabledReason = "",
  steerAlternative = "",
  running = false,
  busy = false,
  submitDisabled = false,
  placeholder = "输入提示词，Enter 发送，Shift+Enter 换行",
  composerLabel = "输入消息",
  models = [],
  selectedModel = "",
  onSelectModel,
  effort = "",
  onSelectEffort,
  attachments = [],
  onAddAttachment,
  onAddFiles,
  onRemoveAttachment,
  contextPill,
  capabilityContext,
  capabilityRefs = [],
  onCapabilityRefsChange,
  promptDocument,
  onPromptDocumentChange,
  onComposerCommand,
  capabilityBanner = "",
  onDismissCapabilityBanner,
  onRetryAttachment,
  onLargePaste,
  extraControls,
  contextMeter,
  className = "",
  onHistoryNavigate,
  historyBrowsing = false,
  historyStatus = "",
  onStashShortcut,
  onOpenStashPanel,
  stashCount = 0,
}: PromptBarProps) {
  const rootRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const lastCaretRef = useRef<EditorCaret | null>(null);
  const capabilityMenuId = useId();
  const reduced = useReducedMotion();
  const [activeToken, setActiveToken] = useState<ActiveComposerToken | null>(null);
  const [capabilityItems, setCapabilityItems] = useState<ComposerCapabilityItem[]>([]);
  const [capabilityLoading, setCapabilityLoading] = useState(false);
  const [capabilityError, setCapabilityError] = useState("");
  const [capabilityRuntime, setCapabilityRuntime] = useState<ComposerRuntimeState | null>(null);
  const [activeCapabilityIndex, setActiveCapabilityIndex] = useState(0);
  const [composing, setComposing] = useState(false);
  const [dragOver, setDragOver] = useState(false);
  const [pasteHint, setPasteHint] = useState<string | null>(null);
  const [caretMarkedOffset, setCaretMarkedOffset] = useState(0);
  const useStructuredDoc = Boolean(onPromptDocumentChange);
  const resolvedDoc = promptDocument
    || documentFromPromptAndRefs(value, capabilityRefs);
  const stripRefs = refsFromDocument(resolvedDoc).filter((ref) => !isInlineContextKind(ref.kind));
  const activeCapabilityTrigger = activeToken?.trigger;
  const activeCapabilityQuery = activeToken?.query || "";
  const [pluginRevision, setPluginRevision] = useState(0);
  useEffect(() => {
    const refresh = () => setPluginRevision((n) => n + 1);
    window.addEventListener("muteki:chat-plugins-changed", refresh);
    window.addEventListener("focus", refresh);
    return () => {
      window.removeEventListener("muteki:chat-plugins-changed", refresh);
      window.removeEventListener("focus", refresh);
    };
  }, []);
  const capabilityAdapterId = capabilityContext?.adapterId || "";
  const capabilityThreadId = capabilityContext?.threadId || "";
  const capabilityWorkspaceId = capabilityContext?.workspaceId || "";
  const capabilityProjectId = capabilityContext?.projectId || "";
  const capabilityMenuOpen = Boolean(activeToken && capabilityAdapterId);

  const editorElement = useCallback(
    () => rootRef.current?.querySelector<HTMLElement>("[data-c34-composer]") ?? null,
    [],
  );

  const syncDocument = (doc: PromptDocument) => {
    onPromptDocumentChange?.(doc);
    onChange(plainTextFromDocument(doc));
    onCapabilityRefsChange?.(refsFromDocument(doc) as ComposerCapabilityRef[]);
  };
  const ingestFiles = (raw: FileList | File[]) => {
    if (!onAddFiles) return;
    const picked = Array.from(raw);
    if (!picked.length) return;
    onAddFiles(picked);
  };
  const showPasteHint = (message: string) => {
    setPasteHint(message);
    window.setTimeout(() => {
      setPasteHint((curr) => (curr === message ? null : curr));
    }, 4000);
  };

  useEffect(() => {
    if (useStructuredDoc) return;
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(Math.max(el.scrollHeight, LEGACY_LINE_PX), LEGACY_LINE_PX * LEGACY_MAX_LINES)}px`;
  }, [value, useStructuredDoc]);

  useEffect(() => {
    if (!useStructuredDoc) return;
    const onSelectionChange = () => {
      const caret = captureCaret(editorElement());
      if (caret) lastCaretRef.current = caret;
    };
    document.addEventListener("selectionchange", onSelectionChange);
    return () => document.removeEventListener("selectionchange", onSelectionChange);
  }, [editorElement, useStructuredDoc]);

  useEffect(() => {
    if (!activeCapabilityTrigger || !capabilityAdapterId) {
      setCapabilityItems([]);
      setCapabilityLoading(false);
      setCapabilityError("");
      setCapabilityRuntime(null);
      return;
    }
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      setCapabilityLoading(true);
      setCapabilityError("");
      void fetchComposerCapabilities(
        {
          adapterId: capabilityAdapterId,
          threadId: capabilityThreadId,
          workspaceId: capabilityWorkspaceId,
          projectId: capabilityProjectId,
        },
        activeCapabilityTrigger,
        activeCapabilityQuery,
        controller.signal,
      ).then(({ items, runtime }) => {
        setCapabilityItems(items);
        setCapabilityRuntime(runtime);
        setActiveCapabilityIndex(0);
      }).catch((error: unknown) => {
        if (controller.signal.aborted) return;
        setCapabilityItems([]);
        setCapabilityError(error instanceof Error ? error.message : String(error));
      }).finally(() => {
        if (!controller.signal.aborted) setCapabilityLoading(false);
      });
    }, 70);
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [
    pluginRevision,
    capabilityContext?.revision,
    activeCapabilityQuery,
    activeCapabilityTrigger,
    capabilityAdapterId,
    capabilityProjectId,
    capabilityThreadId,
    capabilityWorkspaceId,
  ]);

  const hasBody = useStructuredDoc
    ? Boolean(plainTextFromDocument(resolvedDoc).trim() || refsFromDocument(resolvedDoc).length)
    : Boolean(value.trim());
  const hasDraft = hasBody || attachments.length > 0;
  const canSend = hasDraft && !busy && !submitDisabled;
  const chipStripRefs = useStructuredDoc ? stripRefs : capabilityRefs;
  const showTray = Boolean(contextPill || attachments.length > 0 || chipStripRefs.length > 0);

  const focusEditorEnd = () => {
    const editor = editorElement();
    if (!editor) return;
    editor.focus({ preventScroll: true });
    if (editor instanceof HTMLTextAreaElement) {
      const end = editor.value.length;
      editor.setSelectionRange(end, end);
      return;
    }
    const selection = window.getSelection();
    if (!selection) return;
    const range = document.createRange();
    range.selectNodeContents(editor);
    range.collapse(false);
    selection.removeAllRanges();
    selection.addRange(range);
  };

  const insertTrigger = (trigger: "/" | "@") => {
    window.requestAnimationFrame(() => {
      const editor = editorElement();
      if (!editor) return;
      const caret = editor instanceof HTMLTextAreaElement ? captureCaret(editor) : lastCaretRef.current;
      insertTriggerAtCaret(editor, trigger, caret);
    });
  };

  const replaceActiveToken = (replacement: string) => {
    if (!activeToken) return;
    if (useStructuredDoc) {
      const flat = flattenPromptDocument(resolvedDoc);
      const nextFlat = `${flat.slice(0, activeToken.start)}${replacement}${flat.slice(activeToken.end)}`;
      const next = documentFromPromptAndRefs(nextFlat, refsFromDocument(resolvedDoc));
      syncDocument({ ...next, nodes: { ...resolvedDoc.nodes, ...next.nodes } });
      setActiveToken(composerTokenAt(nextFlat, activeToken.start + replacement.length));
      return;
    }
    const nextValue = `${value.slice(0, activeToken.start)}${replacement}${value.slice(activeToken.end)}`;
    const cursor = activeToken.start + replacement.length;
    onChange(nextValue);
    window.requestAnimationFrame(() => {
      textareaRef.current?.focus();
      textareaRef.current?.setSelectionRange(cursor, cursor);
      setActiveToken(composerTokenAt(nextValue, cursor));
    });
  };

  const selectCapabilityItem = (item: ComposerCapabilityItem) => {
    if (capabilityDisabled(item)) {
      setCapabilityError(
        [item.reason, item.alternative].filter(Boolean).join("；")
          || "该项当前不可调用",
      );
      return;
    }
    const action = item.action || "";
    if (action === "insert-runtime-invocation") {
      const wireText = String(item.invocation?.wire_text || `/${item.name}`);
      replaceActiveToken(`${wireText} `);
      return;
    }
    if (action === "inspect-runtime-status") {
      replaceActiveToken("");
      return;
    }
    if (item.kind === "command") {
      if (action === "filter:skill") replaceActiveToken("$skill:");
      else if (action === "filter:mcp") replaceActiveToken("$mcp:");
      else if (action === "filter:plugin") replaceActiveToken("$plugin:");
      else if (action === "filter:file") replaceActiveToken("@");
      else if (action === "insert-native-command") replaceActiveToken(`/${item.name} `);
      else if (action === "clear") {
        if (useStructuredDoc) syncDocument(emptyPromptDocument());
        else {
          onChange("");
          onCapabilityRefsChange?.([]);
        }
        setActiveToken(null);
      } else {
        setActiveToken(null);
        onComposerCommand?.(action);
      }
      return;
    }
    if (useStructuredDoc && item.kind === "file") {
      const node = createFileContextNode({
        id: item.id,
        name: item.name,
        relativePath: item.description || item.name,
        source: item.source,
        scope: item.scope,
        workspaceId: capabilityWorkspaceId || undefined,
        snapshotText: item.description || item.name,
      });
      let base = resolvedDoc;
      if (activeToken) {
        const flat = flattenPromptDocument(resolvedDoc);
        const nextFlat = `${flat.slice(0, activeToken.start)}${flat.slice(activeToken.end)}`;
        const rebuilt = documentFromPromptAndRefs(nextFlat, refsFromDocument(resolvedDoc));
        base = { ...rebuilt, nodes: { ...resolvedDoc.nodes, ...rebuilt.nodes } };
      }
      syncDocument(insertNodeAtCaret(base, node, activeToken?.start ?? caretMarkedOffset));
      setActiveToken(null);
      return;
    }
    if (useStructuredDoc) {
      syncDocument(applyCatalogCapability(resolvedDoc, item, activeToken));
      setActiveToken(null);
      window.requestAnimationFrame(() => editorElement()?.focus());
      return;
    }
    if (!capabilityRefs.some((ref) => ref.id === item.id)) {
      onCapabilityRefsChange?.([
        ...capabilityRefs,
        {
          id: item.id,
          kind: item.kind,
          name: item.name,
          description: item.description,
          source: item.source,
          scope: item.scope,
        },
      ]);
    }
    replaceActiveToken("");
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement | HTMLDivElement>) => {
    if (capabilityMenuOpen && e.key === "ArrowDown") {
      e.preventDefault();
      if (capabilityItems.length) {
        setActiveCapabilityIndex((current) => Math.min(capabilityItems.length - 1, current + 1));
      }
      return;
    }
    if (capabilityMenuOpen && e.key === "ArrowUp") {
      e.preventDefault();
      setActiveCapabilityIndex((current) => Math.max(0, current - 1));
      return;
    }
    if (capabilityMenuOpen && e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      setActiveToken(null);
      return;
    }
    if (
      (e.metaKey || e.ctrlKey)
      && (e.key === "s" || e.key === "S")
      && !e.nativeEvent.isComposing
      && !composing
    ) {
      e.preventDefault();
      onStashShortcut?.();
      return;
    }
    if (
      !capabilityMenuOpen
      && (e.key === "ArrowUp" || e.key === "ArrowDown")
      && !e.altKey
      && !e.metaKey
      && !e.ctrlKey
      && !e.nativeEvent.isComposing
      && !composing
    ) {
      const direction = e.key === "ArrowUp" ? "older" : "newer";
      if (onHistoryNavigate?.(direction)) {
        e.preventDefault();
        return;
      }
    }
    if (
      capabilityMenuOpen
      && (e.key === "Enter" || e.key === "Tab")
      && !e.shiftKey
      && !e.nativeEvent.isComposing
    ) {
      e.preventDefault();
      const item = capabilityItems[activeCapabilityIndex];
      if (item) selectCapabilityItem(item);
      return;
    }
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing && !composing) {
      e.preventDefault();
      if (canSend) {
        onSubmit();
      }
    }
  };

  const activeDescendant = capabilityMenuOpen && capabilityItems[activeCapabilityIndex]
    ? `${capabilityMenuId}-${activeCapabilityIndex}`
    : undefined;

  return (
    <div
      ref={rootRef}
      className={cn("cx-composer relative z-[1] w-full", className)}
      data-testid="conversation-prompt-bar"
      data-history-browsing={historyBrowsing ? "true" : "false"}
    >
      <AnimatePresence initial={false}>
        {capabilityBanner ? (
          <motion.div
            key="capability-banner"
            data-provider-switch-delta="true"
            initial={reduced ? { opacity: 0 } : { opacity: 0, y: 4 }}
            animate={{ opacity: 1, y: 0, transition: { duration: 0.18 } }}
            exit={{ opacity: 0, transition: { duration: 0.12 } }}
            className="mb-2"
          >
            <Callout
              tone="accent"
              role="status"
              onDismiss={onDismissCapabilityBanner}
              className="px-3 py-2 text-[12.5px] leading-5"
            >
              {capabilityBanner}
            </Callout>
          </motion.div>
        ) : null}
      </AnimatePresence>

      <div className="relative">
        <CapabilityMenu
          id={capabilityMenuId}
          trigger={capabilityMenuOpen ? activeToken?.trigger ?? null : null}
          adapterId={capabilityAdapterId}
          items={capabilityItems}
          loading={capabilityLoading}
          error={capabilityError}
          runtime={capabilityRuntime}
          activeIndex={activeCapabilityIndex}
          onActiveIndexChange={setActiveCapabilityIndex}
          onSelect={selectCapabilityItem}
        />

        <div
          className="cx-composer-card relative flex flex-col rounded-[22px] bg-cx-elevated"
          data-drag-over={dragOver ? "true" : undefined}
          onDragOver={(event) => {
            if (!onAddFiles || !dataTransferHasFiles(event.dataTransfer)) return;
            event.preventDefault();
            event.dataTransfer.dropEffect = "copy";
            if (!dragOver) setDragOver(true);
          }}
          onDragLeave={(event) => {
            if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
            setDragOver(false);
          }}
          onDrop={(event) => {
            if (!onAddFiles) return;
            event.preventDefault();
            setDragOver(false);
            ingestFiles(filesFromDataTransfer(event.dataTransfer));
          }}
        >
          <AnimatePresence>
            {dragOver ? (
              <motion.div
                key="drop"
                aria-hidden="true"
                className="cx-drop-overlay pointer-events-none absolute inset-0 z-20 flex items-center justify-center gap-2 rounded-[22px] bg-cx-elevated/85 text-[13px] font-medium text-cx-accent backdrop-blur-[1px]"
                initial={{ opacity: 0 }}
                animate={{ opacity: 1, transition: { duration: 0.12 } }}
                exit={{ opacity: 0, transition: { duration: 0.1 } }}
              >
                <Icon name="upload" size={16} />
                释放以添加附件
              </motion.div>
            ) : null}
          </AnimatePresence>

          {historyBrowsing && historyStatus ? (
            <div
              className="flex items-center gap-1.5 px-4 pt-2.5 text-[12px] text-cx-fg-3"
              role="status"
              data-testid="composer-history-status"
            >
              <Icon name="history" size={13} className="text-cx-fg-4" />
              <span className="truncate">{historyStatus}</span>
            </div>
          ) : null}

          {pasteHint ? (
            <div className="px-3.5 pt-2.5">
              <Callout
                tone="warning"
                role="status"
                testId="composer-paste-hint"
                onDismiss={() => setPasteHint(null)}
                className="px-3 py-2 text-[12.5px] leading-5"
              >
                {pasteHint}
              </Callout>
            </div>
          ) : null}

          {showTray ? (
            <AttachmentTray
              attachments={attachments}
              refs={chipStripRefs}
              contextPill={contextPill}
              onRemoveAttachment={onRemoveAttachment}
              onRetryAttachment={onRetryAttachment}
              onRemoveRef={onCapabilityRefsChange ? (ref) => {
                if (useStructuredDoc && ref.node_id) {
                  syncDocument(removeNode(resolvedDoc, ref.node_id));
                  return;
                }
                onCapabilityRefsChange(capabilityRefs.filter((item) => item.id !== ref.id));
              } : undefined}
            />
          ) : null}

          <div
            className={cn("cursor-text px-3.5", showTray || pasteHint || (historyBrowsing && historyStatus) ? "pt-2" : "pt-3.5")}
            onMouseDown={(event) => {
              if (event.target !== event.currentTarget) return;
              event.preventDefault();
              focusEditorEnd();
            }}
          >
            {useStructuredDoc ? (
              <ComposerPromptDocument
                document={resolvedDoc}
                onDocumentChange={syncDocument}
                onCaretMarkedOffsetChange={setCaretMarkedOffset}
                onComposingChange={setComposing}
                onKeyDown={handleKeyDown}
                onActiveTokenQuery={setActiveToken}
                onLargePaste={onLargePaste}
                onAddFiles={onAddFiles ? (files) => ingestFiles(files) : undefined}
                onPasteHint={showPasteHint}
                placeholder={placeholder}
                aria-label={composerLabel}
                data-c34-composer="true"
                aria-controls={capabilityMenuOpen ? capabilityMenuId : undefined}
                aria-expanded={capabilityMenuOpen}
                aria-activedescendant={activeDescendant}
              />
            ) : (
              <textarea
                ref={textareaRef}
                value={value}
                onChange={(event) => {
                  const next = event.target.value;
                  const cursor = event.target.selectionStart ?? next.length;
                  onChange(next);
                  if (!composing) setActiveToken(composerTokenAt(next, cursor));
                }}
                onKeyDown={handleKeyDown}
                onClick={(event) => {
                  const target = event.currentTarget;
                  setActiveToken(composerTokenAt(target.value, target.selectionStart ?? target.value.length));
                }}
                onCompositionStart={() => setComposing(true)}
                onCompositionEnd={(event) => {
                  setComposing(false);
                  const target = event.currentTarget;
                  setActiveToken(composerTokenAt(target.value, target.selectionStart ?? target.value.length));
                }}
                onPaste={(event) => {
                  const plan = inspectComposerClipboardPaste(event.clipboardData);
                  if (plan.files.length) {
                    event.preventDefault();
                    if (onAddFiles) {
                      ingestFiles(plan.files);
                    } else {
                      showPasteHint("当前无法添加附件");
                    }
                    // Mixed rule: keep non-empty text alongside attachments.
                    const pastedText = plan.text;
                    if (pastedText) {
                      if (onLargePaste && pastedText.length > 2000) {
                        onLargePaste(pastedText);
                      } else {
                        const target = event.currentTarget;
                        const start = target.selectionStart ?? target.value.length;
                        const end = target.selectionEnd ?? start;
                        const next = `${value.slice(0, start)}${pastedText}${value.slice(end)}`;
                        onChange(next);
                        window.requestAnimationFrame(() => {
                          const el = textareaRef.current;
                          if (!el) return;
                          const caret = start + pastedText.length;
                          el.setSelectionRange(caret, caret);
                          if (!composing) setActiveToken(composerTokenAt(next, caret));
                        });
                      }
                    }
                    return;
                  }
                  if (plan.hadFilePayload) {
                    event.preventDefault();
                    if (plan.hint) showPasteHint(plan.hint);
                    const pastedText = plan.text;
                    if (!pastedText) return;
                    if (onLargePaste && pastedText.length > 2000) {
                      onLargePaste(pastedText);
                      return;
                    }
                    const target = event.currentTarget;
                    const start = target.selectionStart ?? target.value.length;
                    const end = target.selectionEnd ?? start;
                    const next = `${value.slice(0, start)}${pastedText}${value.slice(end)}`;
                    onChange(next);
                    window.requestAnimationFrame(() => {
                      const el = textareaRef.current;
                      if (!el) return;
                      const caret = start + pastedText.length;
                      el.setSelectionRange(caret, caret);
                      if (!composing) setActiveToken(composerTokenAt(next, caret));
                    });
                    return;
                  }
                  if (onLargePaste) {
                    const pastedText = plan.text || event.clipboardData.getData("text");
                    if (pastedText.length > 2000) {
                      event.preventDefault();
                      const target = event.currentTarget;
                      const start = target.selectionStart ?? target.value.length;
                      onLargePaste(pastedText, { start, end: target.selectionEnd ?? start });
                    }
                  }
                }}
                placeholder={placeholder}
                rows={1}
                role="combobox"
                aria-label={composerLabel}
                aria-autocomplete="list"
                aria-haspopup="listbox"
                aria-expanded={capabilityMenuOpen}
                aria-controls={capabilityMenuOpen ? capabilityMenuId : undefined}
                aria-activedescendant={activeDescendant}
                className="cx-prompt-editor cx-scroll block w-full resize-none bg-transparent px-1 text-[14px] leading-6 text-cx-fg caret-cx-accent outline-none placeholder:text-cx-fg-4"
                style={{ maxHeight: LEGACY_LINE_PX * LEGACY_MAX_LINES }}
                data-c34-composer="true"
              />
            )}
          </div>

          <div className="flex items-center gap-1 px-2.5 pb-2.5 pt-2">
            <div className="flex min-w-0 flex-1 items-center gap-0.5">
              <ComposerAddMenu
                onAddAttachment={onAddAttachment}
                onInsertTrigger={capabilityAdapterId ? insertTrigger : undefined}
                onOpenStash={onOpenStashPanel ?? onStashShortcut}
                stashCount={stashCount}
              />
              {onOpenStashPanel ? (
                <IconButton
                  icon="archive"
                  label="打开草稿暂存"
                  tooltip="草稿暂存"
                  shortcut="mod+s"
                  size="md"
                  data-testid="composer-stash-button"
                  onClick={onOpenStashPanel}
                  className="rounded-full"
                  badge={stashCount > 0 ? <span data-testid="composer-stash-count">{stashCount}</span> : undefined}
                />
              ) : null}
              <LegacyModelControls
                models={models}
                selectedModel={selectedModel}
                onSelectModel={onSelectModel}
                effort={effort}
                onSelectEffort={onSelectEffort}
              />
              {extraControls}
            </div>
            <div className="flex shrink-0 items-center gap-1.5">
              {contextMeter}
              <SubmitCluster
                running={running}
                busy={busy}
                hasDraft={hasDraft}
                canSend={canSend}
                canSteer={canSteer}
                steerDisabledReason={steerDisabledReason}
                steerAlternative={steerAlternative}
                onSubmit={onSubmit}
                onSteer={onSteer}
                onStop={onStop}
              />
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
