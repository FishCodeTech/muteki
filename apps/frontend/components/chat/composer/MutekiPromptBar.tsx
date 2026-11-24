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

import React, { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { isSendChord, useChatPreferences, type SendKey } from "@/lib/chatPreferences";
import { isMacPlatform } from "@/lib/shortcutBindings";
import { isImeComposingKeyEvent } from "@/lib/imeKeyboard";
import {
  Callout,
  EASE_DRAWER,
  EASE_OUT,
  Icon,
  type IconName,
  Select,
  useReducedMotion,
  type ListOption,
} from "@muteki/ui";
import {
  cachedComposerCapabilities,
  clearComposerCapabilityCache,
  fetchComposerCapabilities,
  ComposerCapabilityError,
  type ComposerCapabilityContext,
  type ComposerCapabilityItem,
  type ComposerCapabilityRef,
  type ComposerRuntimeState,
  type ComposerTrigger,
  type ComposerCapabilitySectionError,
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
  updateNode,
  type PromptDocument,
} from "@/lib/composerContextDoc";
import type { LargePasteSelection } from "@/lib/composerLargePasteKeep";
import {
  dataTransferHasFiles,
  filesFromDataTransfer,
  inspectComposerClipboardPaste,
} from "@/lib/composerAttachments";
import { ComposerPromptDocument, type ComposerPromptDocumentHandle } from "@/components/conversation/ComposerPromptDocument";
import type { UndoSelection, UndoSnapshot } from "@/lib/composerUndoHistory";
import { useComposerUndo } from "./useComposerUndo";
import { AttachmentTray } from "./AttachmentTray";
import { CapabilityMenu } from "./CapabilityMenu";
import { ComposerAddMenu } from "./ComposerAddMenu";
import { SubmitCluster } from "./SubmitCluster";
import { captureCaret, insertTriggerAtCaret, type EditorCaret } from "./caret";
import { capabilityDisabled } from "./capabilityMeta";
import { ComposerAppDock } from "./ComposerAppDock";
import { MAC_APP_DRAG_MIME, MAC_APP_REF_PREFIX, macAppFromDataTransfer } from "@/lib/macApps";

export interface PromptBarAttachment {
  id?: string;
  name: string;
  size?: number;
  file?: File;
  type?: string;
  artifactId?: string;
  cacheId?: string;
  needsReselect?: boolean;
  uploadStatus?: "pending" | "uploading" | "done" | "error";
  uploadError?: string;
  uploadProgress?: number;
  scaledNote?: string;
}

const IME_COMMIT_ENTER_WINDOW_MS = 80;
const COMPACT_TRANSITION = { layout: { duration: 0.22, ease: EASE_DRAWER } };

export interface ModelOption {
  id: string;
  label: string;
  engine?: string;
  group?: string;
  reasoningLevels?: string[];
  defaultEffort?: string;
}

export interface ComposerLocalMentionItem {
  id: string;
  label: string;
  description?: string;
  section: string;
  icon?: IconName;
}

export interface ComposerLocalMentions {
  title: string;
  scopeLabel?: string;
  emptyLabel?: string;
  search: (query: string) => ComposerLocalMentionItem[];
  onSelect: (item: ComposerLocalMentionItem) => void;
}

export interface ComposerCustomDrop {
  types: string[];
  label: string;
  onDrop: (data: DataTransfer) => void;
}

export interface PromptBarProps {
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  onSubmitOpposite?: () => void;
  onStop?: () => void;
  running?: boolean;
  busy?: boolean;
  submitDisabled?: boolean;
  submitLabel?: string;
  submitDescriptionId?: string;
  placeholder?: string;
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
  promptDocument?: PromptDocument;
  onPromptDocumentChange?: (doc: PromptDocument) => void;
  onJumpToContext?: (payload: { messageId: string; turnId?: string; startOffset?: number; endOffset?: number }) => void;
  onComposerCommand?: (action: string, sourceDocument?: PromptDocument) => void;
  capabilityBanner?: string;
  onDismissCapabilityBanner?: () => void;
  onRetryAttachment?: (index: number) => void;
  onLargePaste?: (text: string, selection?: LargePasteSelection) => void;
  extraControls?: React.ReactNode;
  contextMeter?: React.ReactNode;
  className?: string;
  sendKey?: SendKey;
  staticLayout?: boolean;
  onHistoryNavigate?: (direction: "older" | "newer") => boolean;
  historyBrowsing?: boolean;
  historyStatus?: string;
  onStashShortcut?: () => void;
  onOpenStashPanel?: () => void;
  stashCount?: number;
  compact?: boolean;
  onRequestExpand?: () => void;
  localMentions?: ComposerLocalMentions;
  customDrop?: ComposerCustomDrop;
  undoScope?: string;
  onRestoreAttachments?: (attachments: PromptBarAttachment[]) => void;
}

interface ActiveComposerToken {
  trigger: ComposerTrigger;
  query: string;
  start: number;
  end: number;
}

function adaptSendPlaceholder(text: string, sendKey: SendKey): string {
  let out = text;
  if (sendKey === "mod-enter") {
    const mod = isMacPlatform() ? "⌘" : "Ctrl";
    out = out.replace("Shift+Enter 换行", "Enter 换行").replace(/(^|[\s，])Enter (发送|加入|引导|排队)/, `$1${mod}+Enter $2`);
  }
  return out;
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
    trailing: model.engine ? <span className="font-cx-mono text-cx-xs text-cx-fg-4">{model.engine}</span> : undefined,
  }));
  const levels = current?.reasoningLevels || [];
  const effortOptions: ListOption[] = [
    { value: "", label: "推理：默认", textValue: "默认" },
    ...levels.map((level) => ({ value: level, label: `推理：${level}`, textValue: level })),
  ];
  const pill = "cx-press inline-flex h-8 min-w-0 items-center gap-1.5 rounded-xl px-2.5 text-cx-sm font-medium text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg";
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
  onSubmitOpposite,
  onStop,
  running = false,
  busy = false,
  submitDisabled = false,
  submitLabel,
  submitDescriptionId,
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
  onJumpToContext,
  onComposerCommand,
  capabilityBanner = "",
  onDismissCapabilityBanner,
  onRetryAttachment,
  onLargePaste,
  extraControls,
  contextMeter,
  className = "",
  sendKey: sendKeyOverride,
  staticLayout = false,
  onHistoryNavigate,
  historyBrowsing = false,
  historyStatus = "",
  onStashShortcut,
  onOpenStashPanel,
  stashCount = 0,
  compact,
  onRequestExpand,
  localMentions,
  customDrop,
  undoScope,
  onRestoreAttachments,
}: PromptBarProps) {
  const rootRef = useRef<HTMLDivElement>(null);
  const cardRef = useRef<HTMLDivElement>(null);
  const submitSlotRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const docEditorRef = useRef<ComposerPromptDocumentHandle | null>(null);
  const lastCaretRef = useRef<EditorCaret | null>(null);
  const capabilityMenuId = useId();
  const reducedMotion = useReducedMotion();
  const reduced = reducedMotion || staticLayout;
  const animateCompact = compact !== undefined && !reduced;
  const compactMotion = { layoutDependency: Boolean(compact), transition: COMPACT_TRANSITION };
  const contentMotion = { ...compactMotion, layout: animateCompact && "position" as const };
  const [activeToken, setActiveToken] = useState<ActiveComposerToken | null>(null);
  const [catalogItems, setCapabilityItems] = useState<ComposerCapabilityItem[]>([]);
  const [capabilityLoading, setCapabilityLoading] = useState(false);
  const [capabilityError, setCapabilityError] = useState("");
  const [capabilityErrorDiagnostic, setCapabilityErrorDiagnostic] = useState("");
  const [capabilityResultKey, setCapabilityResultKey] = useState("");
  const [capabilityRetry, setCapabilityRetry] = useState(0);
  const [capabilitySectionErrors, setCapabilitySectionErrors] = useState<ComposerCapabilitySectionError[]>([]);
  const [capabilityRuntime, setCapabilityRuntime] = useState<ComposerRuntimeState | null>(null);
  const [activeCapabilityIndex, setActiveCapabilityIndex] = useState(0);
  const [composing, setComposing] = useState(false);
  const composingRef = useRef(false);
  const compositionEndedAtRef = useRef(-Infinity);
  const updateComposing = useCallback((next: boolean) => {
    composingRef.current = next;
    setComposing(next);
    if (!next) compositionEndedAtRef.current = performance.now();
  }, []);
  const chatPrefs = useChatPreferences();
  const sendKey = sendKeyOverride ?? chatPrefs.sendKey;
  const [dragOver, setDragOver] = useState(false);
  const [customDragOver, setCustomDragOver] = useState(false);
  const [appDragOver, setAppDragOver] = useState(false);
  const [appDockDismissed, setAppDockDismissed] = useState(false);
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
    const pluginsChanged = () => {
      clearComposerCapabilityCache();
      refresh();
    };
    window.addEventListener("muteki:chat-plugins-changed", pluginsChanged);
    window.addEventListener("focus", refresh);
    return () => {
      window.removeEventListener("muteki:chat-plugins-changed", pluginsChanged);
      window.removeEventListener("focus", refresh);
    };
  }, []);
  useEffect(() => {
    const card = cardRef.current;
    const slot = submitSlotRef.current;
    if (!card || !slot || typeof ResizeObserver === "undefined") return;
    const publish = () => card.style.setProperty("--cx-composer-submit-w", `${Math.ceil(slot.getBoundingClientRect().width)}px`);
    publish();
    const observer = new ResizeObserver(publish);
    observer.observe(slot);
    return () => observer.disconnect();
  }, []);
  const capabilityAdapterId = capabilityContext?.adapterId || "";
  const capabilityInstanceId = capabilityContext?.instanceId || "";
  const capabilityThreadId = capabilityContext?.threadId || "";
  const capabilityWorkspaceId = capabilityContext?.workspaceId || "";
  const capabilityProjectId = capabilityContext?.projectId || "";
  const capabilityQueryKey = JSON.stringify([
    capabilityAdapterId, capabilityInstanceId, capabilityThreadId, capabilityWorkspaceId, capabilityProjectId,
    activeCapabilityTrigger, activeCapabilityQuery, capabilityContext?.revision, pluginRevision,
  ]);
  const runtimeCapabilityItems = capabilityResultKey === capabilityQueryKey ? catalogItems : [];
  const localMentionActive = Boolean(localMentions && activeCapabilityTrigger === "@");
  const localMentionItems = useMemo(
    () => (localMentionActive && localMentions ? localMentions.search(activeCapabilityQuery) : []),
    [localMentionActive, localMentions, activeCapabilityQuery],
  );
  const localMentionById = useMemo(() => new Map(localMentionItems.map((item) => [item.id, item])), [localMentionItems]);
  const localCatalog = useMemo<ComposerCapabilityItem[]>(() => localMentionItems.map((item) => ({
    id: item.id,
    kind: "file",
    name: item.label,
    description: item.description ?? "",
    source: "",
    scope: "local",
  })), [localMentionItems]);
  const capabilityItems = localMentionActive ? localCatalog : runtimeCapabilityItems;
  const capabilityMenuOpen = Boolean(activeToken && (capabilityAdapterId || localMentionActive));
  const appDockEnabled = Boolean(capabilityAdapterId && (onCapabilityRefsChange || onPromptDocumentChange) && isMacPlatform());
  const computerInUse = refsFromDocument(resolvedDoc).some((ref) => {
    const id = ref.legacy_capability_id || ref.id || "";
    return id === "mcp:muteki-computer" || id.startsWith(MAC_APP_REF_PREFIX);
  });
  useEffect(() => {
    if (!computerInUse) setAppDockDismissed(false);
  }, [computerInUse]);
  const appDockVisible = appDockEnabled && computerInUse && !appDockDismissed;
  useEffect(() => {
    if (localMentionActive) setActiveCapabilityIndex(0);
  }, [localMentionActive, activeCapabilityQuery]);

  const editorElement = useCallback(
    () => rootRef.current?.querySelector<HTMLElement>("[data-c34-composer]") ?? null,
    [],
  );

  const syncDocument = (doc: PromptDocument) => {
    if (onPromptDocumentChange) { onPromptDocumentChange(doc); return; }
    onChange(plainTextFromDocument(doc));
    onCapabilityRefsChange?.(refsFromDocument(doc) as ComposerCapabilityRef[]);
  };

  const readUndoSelection = (): UndoSelection | null => {
    if (useStructuredDoc) return docEditorRef.current?.getMarkedSelection() ?? null;
    const el = textareaRef.current;
    if (!el) return null;
    return {
      start: el.selectionStart ?? 0,
      end: el.selectionEnd ?? el.selectionStart ?? 0,
    };
  };
  const placeUndoSelection = (selection: UndoSelection) => {
    if (useStructuredDoc) {
      docEditorRef.current?.setMarkedSelection(selection);
      return;
    }
    textareaRef.current?.setSelectionRange(selection.start, selection.end);
  };
  const applyUndoSnapshot = (snapshot: UndoSnapshot<PromptBarAttachment>) => {
    syncDocument(snapshot.doc);
    onRestoreAttachments?.(snapshot.attachments);
  };
  useComposerUndo<PromptBarAttachment>({
    scope: undoScope ?? "",
    doc: resolvedDoc,
    attachments: onRestoreAttachments ? attachments : undefined,
    rootRef,
    editorElement,
    readSelection: readUndoSelection,
    applySnapshot: applyUndoSnapshot,
    placeSelection: placeUndoSelection,
    composingRef,
  });
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
    setCapabilityError("");
    setCapabilityErrorDiagnostic("");
    if (!activeCapabilityTrigger || !capabilityAdapterId || localMentionActive) {
      setCapabilityItems([]);
      setCapabilityResultKey("");
      setCapabilitySectionErrors([]);
      setCapabilityLoading(false);
      setCapabilityRuntime(null);
      return;
    }
    const requestContext = {
      adapterId: capabilityAdapterId,
      instanceId: capabilityInstanceId,
      threadId: capabilityThreadId,
      workspaceId: capabilityWorkspaceId,
      projectId: capabilityProjectId,
      revision: capabilityContext?.revision,
    };
    const cached = cachedComposerCapabilities(requestContext, activeCapabilityTrigger, activeCapabilityQuery);
    if (cached) {
      setCapabilityItems(cached.items);
      setCapabilityResultKey(capabilityQueryKey);
      setCapabilityRuntime(cached.runtime);
      setCapabilitySectionErrors(cached.sectionErrors);
      setActiveCapabilityIndex(0);
    } else {
      setCapabilityItems([]);
      setCapabilityResultKey("");
      setCapabilitySectionErrors([]);
      setCapabilityRuntime(null);
    }
    const controller = new AbortController();
    setCapabilityLoading(!cached);
    const timer = window.setTimeout(() => {
      void fetchComposerCapabilities(
        requestContext,
        activeCapabilityTrigger,
        activeCapabilityQuery,
        controller.signal,
      ).then(({ items, runtime, sectionErrors }) => {
        if (controller.signal.aborted) return;
        setCapabilityItems(items);
        setCapabilityResultKey(capabilityQueryKey);
        setCapabilityRuntime(runtime);
        setCapabilitySectionErrors(sectionErrors);
        if (!cached) setActiveCapabilityIndex(0);
      }).catch((error: unknown) => {
        if (controller.signal.aborted) return;
        setCapabilityItems([]);
        setCapabilityResultKey("");
        setCapabilityError(error instanceof Error ? error.message : String(error));
        setCapabilityErrorDiagnostic(error instanceof ComposerCapabilityError ? error.diagnostic : String(error));
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
    capabilityInstanceId,
    capabilityProjectId,
    capabilityThreadId,
    capabilityWorkspaceId,
    capabilityQueryKey,
    capabilityRetry,
    localMentionActive,
  ]);

  const hasBody = useStructuredDoc
    ? Boolean(plainTextFromDocument(resolvedDoc).trim() || refsFromDocument(resolvedDoc).length)
    : Boolean(value.trim());
  const hasDraft = hasBody || attachments.length > 0;
  const canSend = hasDraft && !busy && !submitDisabled;
  const shownPlaceholder = adaptSendPlaceholder(placeholder, sendKey);
  const annotationRefs = useStructuredDoc ? stripRefs.filter((ref) => ref.kind === "message_span") : [];
  const chipStripRefs = useStructuredDoc ? stripRefs.filter((ref) => ref.kind !== "message_span") : capabilityRefs;
  const showTray = Boolean(contextPill || attachments.length > 0 || chipStripRefs.length > 0 || annotationRefs.length > 0);

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
    if (localMentionActive && localMentions) {
      const picked = localMentionById.get(item.id);
      replaceActiveToken("");
      setActiveToken(null);
      if (picked) localMentions.onSelect(picked);
      window.requestAnimationFrame(() => editorElement()?.focus());
      return;
    }
    if (capabilityResultKey !== capabilityQueryKey || capabilityLoading) return;
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
        onComposerCommand?.(action, resolvedDoc);
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
          ...(item.boundary_notice ? { boundary_notice: item.boundary_notice } : {}),
          ...(item.run_id ? { run_id: item.run_id } : {}),
          ...(item.competition_id ? { competition_id: item.competition_id } : {}),
        },
      ]);
    }
    replaceActiveToken("");
  };

  const addCatalogChip = (item: ComposerCapabilityItem) => {
    if (useStructuredDoc) {
      syncDocument(applyCatalogCapability(resolvedDoc, item, null));
    } else if (!capabilityRefs.some((ref) => ref.id === item.id)) {
      onCapabilityRefsChange?.([
        ...capabilityRefs,
        { id: item.id, kind: item.kind, name: item.name, description: item.description, source: item.source, scope: item.scope },
      ]);
    }
    window.requestAnimationFrame(() => editorElement()?.focus());
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement | HTMLDivElement>) => {
    if (composingRef.current || isImeComposingKeyEvent(e)) return;
    if (
      e.key === "Enter"
      && !e.shiftKey
      && !e.metaKey
      && !e.ctrlKey
      && !e.altKey
      && performance.now() - compositionEndedAtRef.current < IME_COMMIT_ENTER_WINDOW_MS
    ) {
      e.preventDefault();
      return;
    }
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
      && !e.shiftKey
      && (e.key === "s" || e.key === "S")
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
    ) {
      e.preventDefault();
      const item = capabilityItems[activeCapabilityIndex];
      if (item) selectCapabilityItem(item);
      return;
    }
    if (e.key === "Enter") {
      if (running && onSubmitOpposite && !e.metaKey && !e.ctrlKey) {
        if (e.shiftKey && !e.altKey) {
          e.preventDefault();
          if (canSend) onSubmitOpposite();
          return;
        }
        if (e.altKey && !e.shiftKey) {
          e.preventDefault();
          if (e.currentTarget instanceof HTMLElement && e.currentTarget.isContentEditable) {
            document.execCommand("insertLineBreak");
          } else {
            const input = e.currentTarget as HTMLTextAreaElement;
            const start = input.selectionStart;
            const end = input.selectionEnd;
            onChange(input.value.slice(0, start) + "\n" + input.value.slice(end));
            requestAnimationFrame(() => input.setSelectionRange(start + 1, start + 1));
          }
          return;
        }
      }
      if (!e.altKey && isSendChord(e, sendKey)) {
        e.preventDefault();
        if (!canSend) return;
        onSubmit();
        return;
      }
      if (sendKey === "mod-enter" && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey
        && e.currentTarget instanceof HTMLElement && e.currentTarget.isContentEditable) {
        e.preventDefault();
        document.execCommand("insertLineBreak");
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
      data-compact={compact ? "true" : "false"}
    >
      <AnimatePresence initial={false}>
        {capabilityBanner ? (
          <motion.div
            key="capability-banner"
            data-provider-switch-delta="true"
            initial={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(4px)" }}
            animate={reduced
              ? { opacity: 1, transition: { duration: 0.14, ease: EASE_OUT } }
              : { opacity: 1, transform: "translateY(0px)", transition: { duration: 0.18, ease: EASE_OUT } }}
            exit={{ opacity: 0, transition: { duration: 0.12, ease: EASE_OUT } }}
            className="mb-2"
          >
            <Callout
              tone="accent"
              role="status"
              onDismiss={onDismissCapabilityBanner}
              className="px-3 py-2 text-cx-sm leading-5"
            >
              {capabilityBanner}
            </Callout>
          </motion.div>
        ) : null}
      </AnimatePresence>

      {appDockVisible ? (
        <ComposerAppDock onPick={addCatalogChip} onClose={() => setAppDockDismissed(true)} />
      ) : null}

      <div className="relative">
        <CapabilityMenu
          id={capabilityMenuId}
          trigger={capabilityMenuOpen ? activeToken?.trigger ?? null : null}
          adapterId={capabilityAdapterId}
          items={capabilityItems}
          loading={localMentionActive ? false : capabilityLoading}
          error={localMentionActive ? "" : capabilityError}
          errorDiagnostic={localMentionActive ? "" : capabilityErrorDiagnostic}
          onRetry={localMentionActive ? undefined : () => setCapabilityRetry((revision) => revision + 1)}
          sectionErrors={localMentionActive ? [] : capabilitySectionErrors}
          runtime={localMentionActive ? null : capabilityRuntime}
          activeIndex={activeCapabilityIndex}
          onActiveIndexChange={setActiveCapabilityIndex}
          onSelect={selectCapabilityItem}
          local={localMentionActive && localMentions ? {
            title: localMentions.title,
            scopeLabel: localMentions.scopeLabel,
            emptyLabel: localMentions.emptyLabel,
            sectionOf: (item) => localMentionById.get(item.id)?.section ?? "",
            iconOf: (item) => localMentionById.get(item.id)?.icon,
          } : undefined}
        />

        <motion.div
          {...compactMotion}
          layout={animateCompact}
          ref={cardRef}
          style={{ borderRadius: 21 }}
          className="cx-composer-card relative flex flex-col rounded-3xl bg-cx-elevated"
          data-drag-over={dragOver || customDragOver || appDragOver ? "true" : undefined}
          data-compact={compact ? "true" : undefined}
          onDoubleClick={(event) => {
            if (!compact || !onRequestExpand) return;
            if ((event.target as Element | null)?.closest("button, a, [role='button'], [role='menuitem']")) return;
            onRequestExpand();
          }}
          onDragOver={(event) => {
            if (appDockEnabled && event.dataTransfer.types.includes(MAC_APP_DRAG_MIME)) {
              event.preventDefault();
              event.dataTransfer.dropEffect = "copy";
              if (!appDragOver) setAppDragOver(true);
              return;
            }
            if (customDrop && customDrop.types.some((type) => event.dataTransfer.types.includes(type))) {
              event.preventDefault();
              event.dataTransfer.dropEffect = "copy";
              if (!customDragOver) setCustomDragOver(true);
              return;
            }
            if (!onAddFiles || !dataTransferHasFiles(event.dataTransfer)) return;
            event.preventDefault();
            event.dataTransfer.dropEffect = "copy";
            if (!dragOver) setDragOver(true);
          }}
          onDragLeave={(event) => {
            if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
            setDragOver(false);
            setCustomDragOver(false);
            setAppDragOver(false);
          }}
          onDrop={(event) => {
            if (appDockEnabled && event.dataTransfer.types.includes(MAC_APP_DRAG_MIME)) {
              event.preventDefault();
              setAppDragOver(false);
              const item = macAppFromDataTransfer(event.dataTransfer);
              if (item) addCatalogChip(item);
              return;
            }
            if (customDrop && customDrop.types.some((type) => event.dataTransfer.types.includes(type))) {
              event.preventDefault();
              setCustomDragOver(false);
              customDrop.onDrop(event.dataTransfer);
              return;
            }
            if (!onAddFiles || !dataTransferHasFiles(event.dataTransfer)) return;
            event.preventDefault();
            setDragOver(false);
            ingestFiles(filesFromDataTransfer(event.dataTransfer));
          }}
        >
          <AnimatePresence>
            {dragOver || customDragOver || appDragOver ? (
              <motion.div
                key="drop"
                aria-hidden="true"
                className="cx-drop-overlay pointer-events-none absolute inset-0 z-20 flex items-center justify-center gap-2 rounded-3xl bg-cx-elevated/85 text-cx-sm font-medium text-cx-accent backdrop-blur-[1px]"
                initial={{ opacity: 0 }}
                animate={{ opacity: 1, transition: { duration: 0.12 } }}
                exit={{ opacity: 0, transition: { duration: 0.1 } }}
              >
                <Icon name={appDragOver ? "mousePointer" : customDragOver ? "at" : "upload"} size={16} />
                {appDragOver ? "释放以指定电脑操控的目标应用" : customDragOver ? customDrop?.label : "释放以添加附件"}
              </motion.div>
            ) : null}
          </AnimatePresence>

          {showTray || pasteHint || (historyBrowsing && historyStatus) ? (
            <motion.div {...contentMotion}>
              {historyBrowsing && historyStatus ? (
                <div
                  className="flex items-center gap-1.5 px-4 pt-2.5 text-cx-xs text-cx-fg-3"
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
                    className="px-3 py-2 text-cx-sm leading-5"
                  >
                    {pasteHint}
                  </Callout>
                </div>
              ) : null}

              {showTray ? (
                <AttachmentTray
                  attachments={attachments}
                  refs={chipStripRefs}
                  annotations={annotationRefs}
                  onRemoveAnnotation={(nodeIds) => syncDocument(nodeIds.reduce((doc, nodeId) => removeNode(doc, nodeId), resolvedDoc))}
                  onUpdateAnnotation={(nodeId, comment) => syncDocument(updateNode(resolvedDoc, nodeId, { comment: comment || undefined }))}
                  onJumpAnnotation={onJumpToContext ? (ref) => {
                    if (!ref.locator?.message_id) return;
                    onJumpToContext({
                      messageId: ref.locator.message_id,
                      turnId: ref.locator.turn_id,
                      startOffset: ref.locator.start_offset,
                      endOffset: ref.locator.end_offset,
                    });
                  } : undefined}
                  contextPill={contextPill}
                  onRemoveAttachment={onRemoveAttachment}
                  onRetryAttachment={onRetryAttachment}
                  onRemoveRef={onCapabilityRefsChange || onPromptDocumentChange ? (ref) => {
                    if (useStructuredDoc && ref.node_id) {
                      syncDocument(removeNode(resolvedDoc, ref.node_id));
                      return;
                    }
                    onCapabilityRefsChange?.(capabilityRefs.filter((item) => item.id !== ref.id));
                  } : undefined}
                />
              ) : null}
            </motion.div>
          ) : null}

          <div
            className="cx-composer-body cursor-text"
            data-after-chrome={showTray || pasteHint || (historyBrowsing && historyStatus) ? "true" : undefined}
            onMouseDown={(event) => {
              if (event.target !== event.currentTarget) return;
              event.preventDefault();
              focusEditorEnd();
            }}
          >
            <motion.div {...contentMotion}>
              {useStructuredDoc ? (
                <ComposerPromptDocument
                  ref={docEditorRef}
                  document={resolvedDoc}
                  onDocumentChange={syncDocument}
                  onCaretMarkedOffsetChange={setCaretMarkedOffset}
                  onComposingChange={updateComposing}
                  onKeyDown={handleKeyDown}
                  onActiveTokenQuery={setActiveToken}
                  onLargePaste={onLargePaste}
                  onAddFiles={onAddFiles ? (files) => ingestFiles(files) : undefined}
                  onPasteHint={showPasteHint}
                  placeholder={shownPlaceholder}
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
                  onCompositionStart={() => updateComposing(true)}
                  onCompositionEnd={(event) => {
                    updateComposing(false);
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
                  placeholder={shownPlaceholder}
                  rows={1}
                  role="combobox"
                  aria-label={composerLabel}
                  aria-autocomplete="list"
                  aria-haspopup="listbox"
                  aria-expanded={capabilityMenuOpen}
                  aria-controls={capabilityMenuOpen ? capabilityMenuId : undefined}
                  aria-activedescendant={activeDescendant}
                  className="cx-prompt-editor cx-scroll block w-full resize-none bg-transparent px-1 text-[length:var(--cx-msg-fs,15px)] leading-[1.6] text-cx-fg caret-cx-accent outline-none placeholder:text-cx-fg-4"
                  style={{ maxHeight: LEGACY_LINE_PX * LEGACY_MAX_LINES }}
                  data-c34-composer="true"
                />
              )}
            </motion.div>
          </div>

          <motion.div
            {...contentMotion}
            className="cx-composer-toolbar relative z-[1] flex items-center gap-1.5 px-3"
          >
            <div className="flex min-w-0 flex-1 items-center gap-0.5">
              <div className="cx-composer-add flex shrink-0">
                <ComposerAddMenu
                  onAddAttachment={onAddAttachment}
                  onInsertTrigger={capabilityAdapterId ? insertTrigger : undefined}
                  onOpenStash={onOpenStashPanel ?? onStashShortcut}
                  stashCount={stashCount}
                />
              </div>
              <div className="cx-composer-tools flex min-w-0 flex-1 items-center gap-0.5" inert={compact}>
                <LegacyModelControls
                  models={models}
                  selectedModel={selectedModel}
                  onSelectModel={onSelectModel}
                  effort={effort}
                  onSelectEffort={onSelectEffort}
                />
                {extraControls}
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-1.5">
              {contextMeter ? <div className="cx-composer-tools flex min-w-0 items-center" inert={compact}>{contextMeter}</div> : null}
              <div ref={submitSlotRef} className="cx-composer-submit flex shrink-0">
                <SubmitCluster
                  running={running}
                  busy={busy}
                  hasDraft={hasDraft}
                  sendKey={sendKey}
                  submitLabel={submitLabel}
                  submitDescriptionId={submitDescriptionId}
                  canSend={canSend}
                  onSubmit={onSubmit}
                  onStop={onStop}
                />
              </div>
            </div>
          </motion.div>
        </motion.div>
      </div>
    </div>
  );
}
