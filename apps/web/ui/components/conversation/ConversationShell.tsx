"use client";

/* ─────────────────────────────────────────────────────────
 * CONVERSATION SHELL — Core orchestrator for the redesigned /chat UI.
 *
 * Implements the full AI-native layout:
 * - Left: Conversation & Project navigation (collapsible)
 * - Middle: Centered chat timeline (760-860px max width)
 * - Bottom: Integrated Prompt Bar, with a toggleable terminal panel below it
 * - Right: On-demand review/terminal/browser/files workspace panel
 * - Top: Compact title and three workspace display controls
 * ───────────────────────────────────────────────────────── */

import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button, Callout, Dialog, IconButton, Popover, TextField, Toaster, toast } from "@/components/chat/ui";
import { RightPanel } from "@/components/chat/panel/RightPanel";
import {
  PANEL_SHEET_BREAKPOINT,
  chatPanel,
  useChatPanel,
} from "@/lib/chatPanelStore";
import { cn } from "@/lib/cn";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import {
  allCredentialModels,
  createConversationProject,
  createConversationThread,
  recoverCommandReceipt,
  bindConversationWorkspace,
  deleteConversationMemory,
  fetchConversationCredentials,
  fetchConversationProjects,
  fetchConversationMemory,
  fetchRuntimeInstances,
  probeRuntimeInstance,
  recordConversationMemory,
  selectConversationDirectory,
  sendConversationCommand,
  updateConversationProjectSettings,
  uploadConversationFile,
  fetchConversationView,
  fetchImpactPreview,
  fetchThreadAudit,
  useConversation,
  useConversationThreads,
  type ConversationCredential,
  type ConversationProject,
  type RuntimeInstance,
  type ConversationMemorySnapshot,
  type ConversationThread,
  type ConversationTurn,
  type ConversationMessage,
  type ThreadMode,
  type WorkspaceBindMode,
  type ImpactPreviewResponse,
} from "@/lib/useConversation";
import {
  freezeForkSource,
  impactCommandThreadId,
  resolveForkTurnId,
  type ForkSourceFreeze,
} from "@/lib/forkSource";
import {
  canBindExistingWorktree,
  normalizeWorktreePath,
  worktreeMissingSelectionCopy,
} from "@/lib/existingWorktreePicker";
import {
  buildThreadMessageHref,
  readMessageDeepLink,
  replaceMessageDeepLink,
} from "@/lib/conversationDeepLink";
import {
  projectConvDefaultsPayload,
  readChatDefaultModel,
  readChatLastSelection,
  readProjectConvDefaults,
  writeChatLastSelection,
} from "@/lib/conversationDefaults";
import { resolveNewConversationSelection } from "@/lib/newConversationSelection";
import { projectDefaultPersistFeedback } from "@/lib/projectDefaultPersistFeedback";
import { resolveComposerRuntimeBind } from "@/lib/conversationComposerBind";
import { upsertConversationThread } from "@/lib/conversationInbox";
import { resolveComposerBindingRestore } from "@/lib/composerRestoreBinding";
import { providerNoticeScope, providerSwitchPreview, runtimeHandoffNotice } from "@/lib/providerSwitchNotice";
import {
  pushJumpBackFromThread,
  shouldRestoreReadingPosition,
} from "@/lib/conversationReadingPosition";
import { fetchComposerCapabilities } from "@/lib/composerCapabilities";
import { copyToClipboard } from "@/lib/clipboard";
import type {
  ComposerCapabilityContext,
  ComposerCapabilityRef,
} from "@/lib/composerCapabilities";
import {
  canInvoke,
  disableCopy,
  matrixFromRuntimeConnection,
} from "@/lib/interactionCapabilityMatrix";
import {
  classifyConversationReadiness,
  GUIDE_DISMISSED_STORAGE_KEY,
  isConnectionSourceFailure,
  isDirectoryInaccessibleMessage,
  isDirectoryPickerUnavailable,
  isRuntimeUnprobed,
  parseHttpStatus,
  pickRuntimeForEngine,
  primarySendBlock,
  RECENT_PATHS_STORAGE_KEY,
  sourceError,
  sourceLoading,
  sourceOk,
  type SourceState,
} from "@/lib/conversationReadiness";
import {
  createFileContextNode,
  createMessageSpanNode,
  documentFromPromptAndRefs,
  emptyPromptDocument,
  flattenPromptDocument,
  insertNodeAtCaret,
  insertPlainTextAtMarkedRange,
  plainTextFromDocument,
  promptDocumentFromDraft,
  refsFromDocument,
  wireCapabilityRefs,
  type PromptDocument,
} from "@/lib/composerContextDoc";
import {
  keepLargePasteAsText,
  type LargePasteSelection,
} from "@/lib/composerLargePasteKeep";
import type { DiffLineAnnotation } from "@/lib/conversationDiff";
import {
  clearComposerDraft,
  composerDraftKey,
  flushComposerDraftStore,
  hydrateComposerDraft,
  readComposerDraft,
  writeComposerDraft,
} from "@/lib/composerDraftStore";
import { composerAttachmentRecoveryAction } from "@/lib/composerAttachmentRecovery";
import { attachmentUploadAbortMessage } from "@/lib/composerAttachmentSendGuard";
import {
  captureSendDraftSnapshot,
  filterAttachmentsAfterSend,
  fingerprintPromptDocument,
  planComposerAfterSendSuccess,
  shouldClearDraftStorageAfterSend,
} from "@/lib/composerSendDraftGuard";
import {
  composerRecallBodyEmpty,
  defaultStashName,
  deleteComposerStash,
  flushComposerRecallStore,
  hydrateComposerStash,
  listComposerStashes,
  listPromptHistory,
  recordSentPrompt,
  saveComposerStash,
  stepHistoryIndex,
  type ComposerStashEntry,
} from "@/lib/composerRecallStore";
import {
  clearRetryIntent,
  clearSendIntent,
  ensureRetryIntent,
  prepareSendIntent,
  classifySendFailure,
  formatSendFailureMessage,
  markSendUploadSha,
  phaseNotice,
  readSendIntent,
  shouldOfferSendRetry,
  updateRetryIntent,
  updateSendIntent,
} from "@/lib/sendIntentStore";
import {
  dispatchThreadNotification,
  unlockNotificationAudio,
  type ConversationInboxEvent,
} from "@/lib/threadNotifications";
import {
  useThreadScopedNotice,
  useThreadScopedError,
  useThreadScopedString,
  shouldScheduleNoticeAutoClear,
} from "@/lib/threadTransientUi";
import {
  clearEditBuffer,
  writeEditBuffer,
} from "@/lib/editResendBuffer";
import type { PromptBarAttachment } from "../ai-native/prompt-bar";

import { ConversationSidebar } from "./ConversationSidebar";
import { ConversationHeader } from "./ConversationHeader";
import { ConversationHome } from "./ConversationHome";
import { ConversationTimeline } from "./ConversationTimeline";
import { ConversationQueue } from "./ConversationQueue";
import { ConversationDetailsDrawer, type DrawerDetailPayload } from "./ConversationDetailsDrawer";
import { ConversationInfoDrawer } from "./ConversationInfoDrawer";
import { ExportDialog } from "./ExportDialog";
import {
  ImpactConfirmModal,
  type ImpactMode,
  type ImpactPreview,
} from "./ImpactConfirmModal";
import { useConversationChrome } from "@/components/conversationChrome";
import { ConversationModelPicker } from "./ConversationModelPicker";
import { credentialForRuntime, validModelEffort } from "@/lib/modelReasoning";
import { ComposerContextStrip } from "./ComposerContextStrip";
import { ComposerStashModal } from "./ComposerStashModal";
import { ConversationReadinessBanner } from "./ConversationReadinessBanner";
import { ConversationStatsBar } from "./ConversationStatsBar";
import { ContextWindowMeter } from "./ContextWindowMeter";
import { ConversationReadingPrefsPanel } from "./ConversationReadingPrefsPanel";
import {
  applyConversationReadingPrefsToElement,
  readConversationReadingPrefs,
  subscribeConversationReadingPrefs,
} from "@/lib/conversationReadingPrefs";
import { ConversationBottomPanel } from "./ConversationWorkspaceDock";
import type { DiffBaselineRequest } from "./ConversationDiffSurface";
import { collectConversationTools } from "./conversationEventViews";
import { PromptBar } from "../ai-native/prompt-bar";
import { LoadingState } from "../ai-native/loading-state";
import { Icon } from "../Icon";
import { useTransitionState } from "@/lib/useTransitionState";
import { CONVERSATION_MAIN_CONTENT_ID } from "@/lib/workspaceSkipTarget";
import {
  listMobileSidebarFocusables,
  mobileSidebarTabRoot,
  nextFocusableIndex,
} from "@/lib/mobileSidebarFocus";
import { ConversationShortcutsHelp } from "./ConversationShortcutsHelp";
import { useSpeechInput } from "@/lib/useSpeechInput";
import type { ResourceLinkTarget } from "@/lib/resourcePreview";
import type { MessageAttachmentChip } from "./ConversationMessage";

function credentialAvailable(credential: ConversationCredential): boolean {
  return credential.engine !== "dsh"
    && credential.present !== false
    && !["missing", "absent", "failed", "invalid", "error", "unavailable"].includes(
      credential.status.toLowerCase(),
    );
}

function resolvePreferredChatSelection(
  credentials: ConversationCredential[],
): { credentialId: string; modelId: string } | null {
  const available = credentials.filter(credentialAvailable);
  if (!available.length) return null;
  const saved = readChatLastSelection() || readChatDefaultModel();
  const preferred =
    (saved && available.find((credential) => credential.id === saved.credentialId))
    || available[0];
  const modelIds = allCredentialModels(preferred).map((item) => item.id);
  const modelId =
    (saved
      && preferred.id === saved.credentialId
      && modelIds.includes(saved.modelId)
      && saved.modelId)
    || (
      preferred.default_model
      && preferred.default_model !== "default"
      && modelIds.includes(preferred.default_model)
      && preferred.default_model
    )
    || modelIds[0]
    || "";
  return { credentialId: preferred.id, modelId };
}

function runtimeUsable(runtime: RuntimeInstance | undefined): boolean {
  return Boolean(
    runtime
    && runtime.enabled !== false
    && runtime.health?.healthy === true,
  );
}

function runtimeForEngine(
  engine: string,
  runtimes: RuntimeInstance[],
): RuntimeInstance | undefined {
  return pickRuntimeForEngine(engine, runtimes) as RuntimeInstance | undefined;
}

function pathBasename(path: string): string {
  const trimmed = path.replace(/\/$/, "");
  if (!trimmed) return "";
  return trimmed.split("/").filter(Boolean).at(-1) || trimmed;
}

function readRecentPaths(): string[] {
  try {
    const raw = JSON.parse(window.localStorage.getItem(RECENT_PATHS_STORAGE_KEY) || "[]");
    if (!Array.isArray(raw)) return [];
    return raw.map(String).map((item) => item.trim()).filter(Boolean).slice(0, 8);
  } catch {
    return [];
  }
}

function rememberRecentPath(path: string, aliases: string[] = []): string[] {
  const skip = new Set([path, ...aliases]);
  const next = [path, ...readRecentPaths().filter((item) => !skip.has(item))].slice(0, 8);
  try {
    window.localStorage.setItem(RECENT_PATHS_STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Optional preference.
  }
  return next;
}

function newAttachmentId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `att_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

function composerRefs(doc: PromptDocument): ComposerCapabilityRef[] {
  return refsFromDocument(doc) as ComposerCapabilityRef[];
}

export function ConversationShell({ threadId = "" }: { threadId?: string }) {
  const router = useRouter();
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const handleInboxEvent = useCallback((event: ConversationInboxEvent) => {
    dispatchThreadNotification(event, {
      activeThreadId: threadId,
      onOpenThread: (id) => {
        router.push(`/chat/${encodeURIComponent(id)}`);
      },
    });
  }, [router, threadId]);
  const { threads, refresh: refreshList, attentionCount } = useConversationThreads({
    activeThreadId: threadId,
    onInboxEvent: handleInboxEvent,
  });
  const conversation = useConversation(threadId);
  const draftParam = searchParams.get("draft") || "";
  const draftKey = composerDraftKey(threadId, draftParam);
  // Retry awaits a configuration refresh. Keep the active route/view current
  // during render so an old async callback cannot submit after navigation.
  const activeSendContextRef = useRef({ draftKey, viewThreadId: "" });
  activeSendContextRef.current = {
    draftKey,
    viewThreadId: conversation.view?.thread.thread_id || "",
  };
  // Start true so the persist effect cannot write an empty composer over a
  // stored draft before the hydrate effect runs on first mount / remount.
  const draftHydratingRef = useRef(true);
  const [hydratedDraftKey, setHydratedDraftKey] = useState("");
  const appliedNewChatDefaultsRef = useRef("");
  const composerSnapshotRef = useRef({
    draftKey,
    prompt: "",
    promptDocument: emptyPromptDocument() as PromptDocument,
    capabilityRefs: [] as ComposerCapabilityRef[],
    attachments: [] as PromptBarAttachment[],
    credentialId: "",
    runtimeKey: "",
    model: "",
    effort: "",
    accessMode: "supervised",
    projectId: "",
  });

  useEffect(() => {
    const unlock = () => unlockNotificationAudio();
    window.addEventListener("pointerdown", unlock, { once: true });
    window.addEventListener("keydown", unlock, { once: true });
    return () => {
      window.removeEventListener("pointerdown", unlock);
      window.removeEventListener("keydown", unlock);
    };
  }, []);

  const latestConversationEvent = conversation.events.at(-1);
  const latestConversationEventId = latestConversationEvent?.event_id;
  const latestConversationEventType = latestConversationEvent?.event_type;
  useEffect(() => {
    if (!latestConversationEventType) return;
    if (![
      "core.turn.completed",
      "core.turn.failed",
      "core.turn.interrupted",
      "core.approval.requested",
      "core.user_input.requested",
    ].includes(latestConversationEventType)) return;
    void refreshList();
  }, [latestConversationEventId, latestConversationEventType, refreshList]);

  // Global settings & credentials
  const [credentialRows, setCredentials] = useState<ConversationCredential[]>([]);
  const [credentialSource, setCredentialSource] = useState<SourceState>(sourceLoading());
  const credentialLoadSeqRef = useRef(0);
  const [credentialId, setCredentialId] = useState("");

  // Runtimes
  const [runtimes, setRuntimes] = useState<RuntimeInstance[]>([]);
  const [runtimeSource, setRuntimeSource] = useState<SourceState>(sourceLoading());
  const [runtimeKey, setRuntimeKey] = useState("");
  const credentials = useMemo(() => credentialRows.map(credential => {
    const selected = credential.id === credentialId
      ? runtimes.find(runtime => runtime.key === runtimeKey && runtime.engine === credential.engine)
      : undefined;
    const runtime = selected || runtimeForEngine(credential.engine, runtimes);
    return credentialForRuntime(credential, runtime?.key || "");
  }), [credentialRows, credentialId, runtimes, runtimeKey]);
  const restoredBindingContextRef = useRef<{ draftKey: string; projectId: string } | null>(null);
  const [probing, setProbing] = useState(false);

  // Model & settings selection
  const [model, setModel] = useState("");
  const [effort, setEffort] = useState("");
  const [accessMode, setAccessMode] = useState("supervised");

  // Projects & workspace
  const [projects, setProjects] = useState<ConversationProject[]>([]);
  const [projectSource, setProjectSource] = useState<SourceState>(sourceLoading());
  const [projectId, setProjectId] = useState("");
  const [savingProjectDefault, setSavingProjectDefault] = useState(false);
  const [projectCreating, setProjectCreating] = useState(false);
  const [mode, setMode] = useState<ThreadMode>("conversation");
  const [workspaceMode, setWorkspaceMode] = useState<WorkspaceBindMode>("shared_checkout");
  const [worktreeBranch, setWorktreeBranch] = useState("");
  const [existingWorktreePath, setExistingWorktreePath] = useState("");
  const [directoryIssue, setDirectoryIssue] = useState<{ message: string } | null>(null);
  const [preferPathInput, setPreferPathInput] = useState(false);
  const [requestPathInput, setRequestPathInput] = useState(false);
  const [recentPaths, setRecentPaths] = useState<string[]>([]);
  const [guideDismissed, setGuideDismissed] = useState(false);

  // Draft text & states
  const [prompt, setPrompt] = useState("");
  const [promptDocument, setPromptDocument] = useState<PromptDocument>(emptyPromptDocument);
  const [capabilityRefs, setCapabilityRefs] = useState<ComposerCapabilityRef[]>([]);
  const [busy, setBusy] = useState(false);
  // Layout-persistent /chat shell: key transient banners by draft/thread so a
  // leftover send error or memory notice cannot follow the user to another chat.
  const [notice, setNotice, noticeControls] = useThreadScopedNotice(draftKey);

  // C36: speech-to-text — inserts transcript after current prompt, never auto-sends.
  const speech = useSpeechInput((transcript) => {
    setPrompt((current) => {
      const trimmed = current.trimEnd();
      return trimmed ? `${trimmed} ${transcript}` : transcript;
    });
    setPromptDocument((current) => {
      const currentText = flattenPromptDocument(current).trimEnd();
      const next = currentText ? `${currentText} ${transcript}` : transcript;
      return documentFromPromptAndRefs(next, refsFromDocument(current));
    });
  });
  const [{ message: error, recoveryAction: errorRetryKind }, setError] = useThreadScopedError(draftKey);

  // Old success toast must not reappear after an error banner is dismissed.
  useEffect(() => {
    if (!error) return;
    if (shouldScheduleNoticeAutoClear(notice)) setNotice("");
  }, [error, notice, setNotice]);
  const [jumpTarget, setJumpTarget] = useState<{
    messageId: string;
    startOffset?: number;
    endOffset?: number;
  } | null>(null);

  // Sidebar search
  const [searchQuery, setSearchQuery] = useState("");
  const rawMessageParam = readMessageDeepLink(searchParams);
  const [deepLinkDismissed, setDeepLinkDismissed] = useState(false);
  const messageParam = deepLinkDismissed ? "" : rawMessageParam;
  const [highlightedMessageId, setHighlightedMessageId] = useState("");
  const ensureMessageVisible = conversation.ensureMessageVisible;

  useEffect(() => {
    setDeepLinkDismissed(false);
  }, [threadId]);

  useEffect(() => {
    // A new ?message= re-arms the deep-link. Clearing the query (回到最新)
    // must keep dismissed so C08 restore stays off for this visit.
    if (rawMessageParam) setDeepLinkDismissed(false);
  }, [rawMessageParam]);

  useEffect(() => {
    if (!threadId || !messageParam) {
      setHighlightedMessageId("");
      return;
    }
    // useConversation keeps the previous thread view while switching, so
    // `loading && view` can still be the *stale* thread. Wait until the active
    // view matches this route before jumping.
    if (
      conversation.loading
      || conversation.view?.thread.thread_id !== threadId
    ) {
      return;
    }
    let cancelled = false;
    let clearTimer: ReturnType<typeof setTimeout> | null = null;
    void (async () => {
      // Set highlight early so Timeline suppresses loadOlder / stick-to-bottom
      // while the around-message page is fetched and committed.
      setHighlightedMessageId(messageParam);
      const result = await ensureMessageVisible(messageParam);
      if (cancelled || result === "cancelled") return;
      if (result !== "ok") {
        setNotice("消息不可用");
        setHighlightedMessageId("");
        replaceMessageDeepLink(null);
        return;
      }
      setHighlightedMessageId(messageParam);
      clearTimer = setTimeout(() => {
        if (!cancelled) setHighlightedMessageId("");
      }, 4000);
    })();
    return () => {
      cancelled = true;
      if (clearTimer) clearTimeout(clearTimer);
    };
  }, [
    threadId,
    messageParam,
    conversation.loading,
    conversation.view?.thread.thread_id,
    ensureMessageVisible,
  ]);

  // Drawers & Modals
  const [detailsPayload, setDetailsPayload] = useState<DrawerDetailPayload | null>(null);
  const [infoDrawerOpen, setInfoDrawerOpen] = useState(false);
  const [readingPrefsOpen, setReadingPrefsOpen] = useState(false);
  const shellRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = shellRef.current;
    if (!el) return;
    applyConversationReadingPrefsToElement(el, readConversationReadingPrefs());
    return subscribeConversationReadingPrefs((prefs) => {
      applyConversationReadingPrefsToElement(el, prefs);
    });
  }, []);
  const [exportDialogOpen, setExportDialogOpen] = useState(false);
  const [commandSessionsOpen, setCommandSessionsOpen] = useState(false);
  const [bottomPanelOpen, setBottomPanelOpen] = useState(false);
  const panel = useChatPanel(threadId);
  const rightPanelOpen = Boolean(threadId) && panel.isOpen;
  const [panelSheet, setPanelSheet] = useState(false);
  useEffect(() => {
    const mq = window.matchMedia(`(max-width: ${PANEL_SHEET_BREAKPOINT}px)`);
    const sync = () => setPanelSheet(mq.matches);
    sync();
    mq.addEventListener("change", sync);
    return () => mq.removeEventListener("change", sync);
  }, []);
  const [modelPickerOpen, setModelPickerOpen] = useState(false);
  const panelControlsRef = useRef<HTMLDivElement | null>(null);
  const mainRef = useRef<HTMLElement | null>(null);
  const [mainLeft, setMainLeft] = useState(0);
  useEffect(() => {
    const shell = shellRef.current;
    const main = mainRef.current;
    if (!shell || !main || typeof ResizeObserver === "undefined") return;
    const measure = () => setMainLeft(main.offsetLeft);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(shell);
    observer.observe(main);
    return () => observer.disconnect();
  }, []);
  const panelTools = useMemo(() => {
    const view = conversation.view;
    if (!view) return [];
    const statuses = Object.fromEntries((view.turns || []).map((turn) => [turn.turn_id, turn.status]));
    return collectConversationTools(conversation.events, statuses);
  }, [conversation.events, conversation.view]);
  const runningToolCount = useMemo(() => panelTools.filter((tool) => tool.status === "running").length, [panelTools]);

  const bottomPanelPresence = useTransitionState(bottomPanelOpen, { durationVar: "--panel-close-dur", durationMs: 180 });
  useEffect(() => {
    const onOpenLog = (event: Event) => {
      event.preventDefault();
      setBottomPanelOpen(true);
    };
    window.addEventListener("muteki:open-execution-log", onOpenLog);
    return () => window.removeEventListener("muteki:open-execution-log", onOpenLog);
  }, []);
  const [renameTarget, setRenameTarget] = useState<ConversationThread | null>(null);
  const [renameInput, setRenameInput] = useState("");
  const [archiveImpact, setArchiveImpact] = useState<{
    threads: ConversationThread[];
    mode: "single" | "batch";
  } | null>(null);

  // C11 impact confirm (Retry / Edit-resend / Fork / Rewind)
  const [impactOpen, setImpactOpen] = useState(false);
  const [impactPreview, setImpactPreview] = useState<ImpactPreview | null>(null);
  const [impactLoading, setImpactLoading] = useState(false);
  const [impactEditedText, setImpactEditedText] = useState("");
  const [impactCommandId, setImpactCommandId] = useState("");
  /** Frozen fork source across preview/confirm (Pane #209). */
  const [impactForkSource, setImpactForkSource] = useState<ForkSourceFreeze | null>(null);
  const [showSuperseded, setShowSuperseded] = useState(false);
  const [auditTurns, setAuditTurns] = useState<ConversationTurn[]>([]);
  const [auditMessages, setAuditMessages] = useState<ConversationMessage[]>([]);

  // Attachments (ids stable across thread switches for draft persistence)
  const [attachments, setAttachments] = useState<PromptBarAttachment[]>([]);
  const {
    sidebarCollapsed,
    sidebarWidth,
    setSidebarWidth,
    mobileSidebarOpen,
    setMobileSidebarOpen,
  } = useConversationChrome();
  // #199: mobile sidebar overlay acts like a modal Sheet — trap Tab + inert main.
  // Desktop persistent sidebar never sets mobileSidebarOpen, so it stays non-modal.
  useEffect(() => {
    if (!mobileSidebarOpen) return;
    const shell = shellRef.current;
    const frame = window.requestAnimationFrame(() => {
      shell?.querySelector<HTMLButtonElement>("[data-cx-new-chat]")?.focus();
    });
    const desktop = window.matchMedia("(min-width: 769px)");
    const closeOnDesktop = () => { if (desktop.matches) setMobileSidebarOpen(false); };
    desktop.addEventListener("change", closeOnDesktop);
    closeOnDesktop();
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      const sidebar = document.getElementById("conversation-sidebar");
      const active = document.activeElement as HTMLElement | null;
      const focusRoot = mobileSidebarTabRoot(sidebar, active);
      // Dialogs already own their Tab loop; allow their document handler to run.
      if (focusRoot !== sidebar && focusRoot?.getAttribute("role") === "dialog") return;
      const controls = listMobileSidebarFocusables(focusRoot);
      if (!controls.length) {
        event.preventDefault();
        focusRoot?.focus();
        return;
      }
      const current = active && controls.includes(active) ? controls.indexOf(active) : -1;
      const next = nextFocusableIndex(controls.length, current, event.shiftKey);
      if (next < 0) return;
      event.preventDefault();
      controls[next]?.focus();
    };
    // Capture so Tab cannot land on inert background / workspace chrome first.
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.cancelAnimationFrame(frame);
      desktop.removeEventListener("change", closeOnDesktop);
      window.removeEventListener("keydown", onKey, true);
      document.querySelector<HTMLButtonElement>(".workspace-brand-sidebar-toggle")?.focus();
    };
  }, [mobileSidebarOpen]);

  // Large-paste dialog
  const [largePasteText, setLargePasteText] = useState<string | null>(null);
  const [largePasteSelection, setLargePasteSelection] = useState<LargePasteSelection | null>(null);
  const [shortcutsHelpOpen, setShortcutsHelpOpen] = useState(false);
  const shortcutsHelpWasOpenRef = useRef(false);
  const providerScope = providerNoticeScope(
    draftKey, runtimeKey, credentialId, model, conversation.view?.state.agent_session_id || "",
  );
  const [handoffNotice, setHandoffNotice] = useThreadScopedString(providerScope);
  const [dismissedProviderPreview, setDismissedProviderPreview] = useThreadScopedString(draftKey);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const reselectInputRef = useRef<HTMLInputElement | null>(null);
  const reselectIndexRef = useRef<number | null>(null);
  const pendingProjectIdRef = useRef("");
  const commandBusyRef = useRef(false);
  const applyingHistoryRef = useRef(false);
  const historyIndexRef = useRef<number | null>(null);
  const historyScratchRef = useRef<{
    promptDocument: ReturnType<typeof emptyPromptDocument>;
  } | null>(null);
  const [historyIndex, setHistoryIndex] = useState<number | null>(null);
  const [stashOpen, setStashOpen] = useState(false);
  const [stashName, setStashName] = useState("");
  const [stashes, setStashes] = useState<ComposerStashEntry[]>([]);

  // Memory
  const [memory, setMemory] = useState<ConversationMemorySnapshot | null>(null);
  const [memoryLoading, setMemoryLoading] = useState(false);

  // Auto-close details drawer on threadId change; the right panel is per-thread state.
  useEffect(() => {
    setDetailsPayload(null);
  }, [threadId]);

  // Bound workspace means directory readiness is satisfied; clear sticky picker errors
  // (headless / no File System Access API) so they do not obscure Plan reconnect UX.
  useEffect(() => {
    if (conversation.view?.workspace?.root_path) {
      setDirectoryIssue(null);
    }
  }, [conversation.view?.workspace?.root_path]);

  useEffect(() => {
    try {
      setGuideDismissed(window.localStorage.getItem(GUIDE_DISMISSED_STORAGE_KEY) === "1");
      setRecentPaths(readRecentPaths());
    } catch {
      // localStorage may be blocked; keep the default guide and paths.
    }
  }, []);

  useEffect(() => {
    if (!requestPathInput) return;
    const timer = window.setTimeout(() => setRequestPathInput(false), 0);
    return () => window.clearTimeout(timer);
  }, [requestPathInput]);

  const startNewChat = useCallback((nextProjectId = "", preserveRuntime = false) => {
    const nextDraft = String(Date.now());
    pendingProjectIdRef.current = nextProjectId;
    const current = composerSnapshotRef.current;
    writeChatLastSelection({
      credentialId: current.credentialId, modelId: current.model,
      runtimeKey: current.runtimeKey, effort: current.effort, accessMode: current.accessMode,
    });
    if (preserveRuntime) {
      writeComposerDraft(composerDraftKey("", nextDraft), {
        prompt: "", promptSegments: [], capabilityRefs: [], attachments: [],
        credentialId: current.credentialId, runtimeKey: current.runtimeKey,
        model: current.model, effort: current.effort, accessMode: current.accessMode,
        projectId: nextProjectId,
      });
      flushComposerDraftStore();
    }
    setMobileSidebarOpen(false);
    router.push(`/chat?draft=${nextDraft}`);
  }, [router]);

  // C34 — Global conversation keyboard shortcuts.
  // Guard: skip when focus is inside a text input, contenteditable, or the xterm terminal.
  useEffect(() => {
    const focusComposer = () => {
      document.querySelector<HTMLElement>("[data-c34-composer]")?.focus();
    };
    const inTextField = (t: EventTarget | null): boolean => {
      if (!(t instanceof Element)) return false;
      const tag = (t as HTMLElement).tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return true;
      if ((t as HTMLElement).isContentEditable) return true;
      if (t.closest("[data-c34-terminal]")) return true;
      return false;
    };
    const handler = (e: KeyboardEvent) => {
      if (e.isComposing) return;
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.shiftKey && e.key === "N") {
        e.preventDefault();
        startNewChat();
        return;
      }
      if (mod && e.shiftKey && !e.altKey && e.code === "KeyM") {
        e.preventDefault();
        setModelPickerOpen((open) => !open);
        return;
      }
      if (mod && !e.shiftKey && !e.altKey && e.code === "KeyJ" && threadId) {
        e.preventDefault();
        setBottomPanelOpen((open) => !open);
        return;
      }
      // Option/Alt changes e.key on macOS, so panel shortcuts match on e.code.
      if (mod && e.altKey && !e.shiftKey && threadId) {
        const surfaces: Record<string, () => void> = {
          KeyB: () => chatPanel.toggle(threadId),
          KeyD: () => chatPanel.toggle(threadId, "diff"),
          KeyP: () => {
            const current = panel.surfaces.find((item) => item.id === panel.activeId);
            if (panel.isOpen && current?.kind === "preview") chatPanel.close(threadId);
            else chatPanel.openPreview(threadId);
          },
          KeyF: () => chatPanel.toggle(threadId, "files"),
          KeyT: () => chatPanel.toggle(threadId, "terminal"),
          KeyO: () => chatPanel.toggle(threadId, "overview"),
        };
        const action = surfaces[e.code];
        if (action) {
          e.preventDefault();
          setDetailsPayload(null);
          action();
          return;
        }
      }
      if (inTextField(e.target)) return;
      if (e.key === "Escape" && !mod && !e.altKey) {
        if (mobileSidebarOpen) {
          setMobileSidebarOpen(false);
          document.querySelector<HTMLButtonElement>(".workspace-brand-sidebar-toggle")?.focus();
          return;
        }
        if (shortcutsHelpOpen) { setShortcutsHelpOpen(false); return; }
        if (detailsPayload) { setDetailsPayload(null); focusComposer(); return; }
        if (readingPrefsOpen) { setReadingPrefsOpen(false); focusComposer(); return; }
        if (infoDrawerOpen) { setInfoDrawerOpen(false); focusComposer(); return; }
        if (bottomPanelOpen) { setBottomPanelOpen(false); focusComposer(); return; }
        if (rightPanelOpen && panel.maximized) { chatPanel.setMaximized(threadId, false); return; }
        focusComposer();
        return;
      }
      if (e.key === "/" && !mod && !e.altKey) {
        e.preventDefault();
        focusComposer();
        return;
      }
      if (e.key === "?" && !mod && !e.altKey) {
        e.preventDefault();
        setShortcutsHelpOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [startNewChat, shortcutsHelpOpen, detailsPayload, infoDrawerOpen, readingPrefsOpen, rightPanelOpen, bottomPanelOpen, mobileSidebarOpen, threadId, panel]);

  // #134: the dialog restores focus to its opener on the next frame; land on the
  // composer one frame later so closing help always returns to typing.
  useEffect(() => {
    const wasOpen = shortcutsHelpWasOpenRef.current;
    shortcutsHelpWasOpenRef.current = shortcutsHelpOpen;
    if (!wasOpen || shortcutsHelpOpen) return;
    let inner = 0;
    const outer = window.requestAnimationFrame(() => {
      inner = window.requestAnimationFrame(() => {
        document.querySelector<HTMLElement>("[data-c34-composer]")?.focus();
      });
    });
    return () => {
      window.cancelAnimationFrame(outer);
      window.cancelAnimationFrame(inner);
    };
  }, [shortcutsHelpOpen]);

  useEffect(() => {
    const onUnload = () => {
      flushComposerDraftStore();
      flushComposerRecallStore();
    };
    window.addEventListener("beforeunload", onUnload);
    return () => {
      window.removeEventListener("beforeunload", onUnload);
      onUnload();
    };
  }, []);

  // Restore the composer for the active draft key (thread or new-chat id).
  useEffect(() => {
    const previous = composerSnapshotRef.current;
    if (previous.draftKey && previous.draftKey !== draftKey) {
      writeComposerDraft(previous.draftKey, {
        prompt: previous.prompt,
        promptSegments: previous.promptDocument.segments,
        capabilityRefs: previous.capabilityRefs,
        attachments: previous.attachments,
        credentialId: previous.credentialId,
        runtimeKey: previous.runtimeKey,
        model: previous.model,
        effort: previous.effort,
        accessMode: previous.accessMode,
        projectId: previous.projectId,
      });
      flushComposerDraftStore();
    }
    draftHydratingRef.current = true;
    appliedNewChatDefaultsRef.current = "";
    restoredBindingContextRef.current = null;
    const { draft, attachments: nextAttachments, restoredNeedsReselect } = hydrateComposerDraft(draftKey);
    if (draft) {
      const doc = promptDocumentFromDraft(draft);
      setPromptDocument(doc);
      setPrompt(plainTextFromDocument(doc));
      setCapabilityRefs(composerRefs(doc));
      setAttachments(nextAttachments);
      if (draft.credentialId) setCredentialId(draft.credentialId);
      if (draft.runtimeKey) setRuntimeKey(draft.runtimeKey);
      if (draft.model) setModel(draft.model);
      if (draft.effort !== undefined) setEffort(draft.effort);
      if (draft.accessMode) setAccessMode(draft.accessMode);
      // A saved thread's workspace is immutable in the composer. A local draft
      // may restore text/model choices, but must not override its persisted home.
      if (!threadId) {
        setProjectId(draft.projectId || "");
        pendingProjectIdRef.current = "";
        restoredBindingContextRef.current = { draftKey, projectId: draft.projectId || "" };
      }
      if (restoredNeedsReselect) {
        setNotice("部分附件无法恢复本地文件，请重新选择后再发送");
      }
    } else {
      setPrompt("");
      setPromptDocument(emptyPromptDocument());
      setCapabilityRefs([]);
      setAttachments([]);
      if (!threadId) {
        const pendingProjectId = pendingProjectIdRef.current;
        if (pendingProjectId) {
          setProjectId(pendingProjectId);
          pendingProjectIdRef.current = "";
        } else {
          setProjectId("");
        }
        setMode("conversation");
        const recent = readChatLastSelection();
        setCredentialId(recent?.credentialId || "");
        setRuntimeKey(recent?.runtimeKey || "");
        setModel(recent?.modelId || "");
        setEffort(recent?.effort || "");
        setAccessMode(recent?.accessMode || "supervised");
        setError("");
        setNotice("");
        setMemory(null);
      }
    }
    composerSnapshotRef.current = { ...composerSnapshotRef.current, draftKey };
    setHistoryIndex(null);
    historyIndexRef.current = null;
    historyScratchRef.current = null;
    const release = window.setTimeout(() => {
      draftHydratingRef.current = false;
      setHydratedDraftKey(draftKey);
    }, 0);
    return () => window.clearTimeout(release);
  }, [draftKey, threadId]);

  // Persist composer state per draft key (debounced inside the store).
  useEffect(() => {
    composerSnapshotRef.current = {
      draftKey,
      prompt,
      promptDocument,
      capabilityRefs,
      attachments,
      credentialId,
      runtimeKey,
      model,
      effort,
      accessMode,
      projectId,
    };
    if (draftHydratingRef.current) return;
    writeComposerDraft(draftKey, {
      prompt,
      promptSegments: promptDocument.segments,
      capabilityRefs,
      attachments,
      credentialId,
      runtimeKey,
      model,
      effort,
      accessMode,
      projectId,
    });
  }, [
    accessMode,
    attachments,
    capabilityRefs,
    credentialId,
    runtimeKey,
    draftKey,
    effort,
    model,
    projectId,
    prompt,
    promptDocument,
  ]);

  const historySessionKey = threadId ? composerDraftKey(threadId) : draftKey;

  const refreshStashes = useCallback(() => {
    setStashes(listComposerStashes());
  }, []);

  useEffect(() => {
    refreshStashes();
  }, [refreshStashes]);

  const exitHistoryBrowse = useCallback(() => {
    if (historyIndexRef.current === null && historyIndex === null) return;
    historyIndexRef.current = null;
    setHistoryIndex(null);
    historyScratchRef.current = null;
  }, [historyIndex]);

  const applyComposerSnapshot = useCallback((input: {
    prompt: string;
    promptSegments?: PromptDocument["segments"];
    capabilityRefs: ComposerCapabilityRef[];
  }) => {
    // Keep the guard synchronous only. ComposerPromptDocument is controlled and
    // does not echo prop updates via onDocumentChange; a setTimeout(0) window
    // would let the first real keystroke skip exit-history (Bugbot Medium).
    applyingHistoryRef.current = true;
    const doc = promptDocumentFromDraft({
      prompt: input.prompt,
      promptSegments: input.promptSegments,
      capabilityRefs: input.capabilityRefs,
    });
    setPromptDocument(doc);
    setPrompt(plainTextFromDocument(doc));
    setCapabilityRefs(composerRefs(doc));
    applyingHistoryRef.current = false;
  }, []);

  const handlePromptChange = useCallback((next: string) => {
    if (!applyingHistoryRef.current) exitHistoryBrowse();
    setPrompt(next);
  }, [exitHistoryBrowse]);

  const handlePromptDocumentChange = useCallback((doc: PromptDocument) => {
    if (!applyingHistoryRef.current) exitHistoryBrowse();
    setPromptDocument(doc);
    setPrompt(plainTextFromDocument(doc));
    setCapabilityRefs(composerRefs(doc));
  }, [exitHistoryBrowse]);

  const handleHistoryNavigate = useCallback((direction: "older" | "newer"): boolean => {
    const history = listPromptHistory(historySessionKey);
    const canStart = composerRecallBodyEmpty({
      prompt,
      promptSegments: promptDocument.segments,
      capabilityRefs,
      attachments,
    });
    const stepped = stepHistoryIndex(direction, historyIndexRef.current, history.length, canStart);
    if (!stepped.handled) return false;
    if (historyIndexRef.current === null && stepped.index === 0) {
      historyScratchRef.current = { promptDocument };
    }
    if (stepped.index === null) {
      const scratch = historyScratchRef.current?.promptDocument || emptyPromptDocument();
      applyComposerSnapshot({
        prompt: plainTextFromDocument(scratch),
        promptSegments: scratch.segments,
        capabilityRefs: composerRefs(scratch),
      });
      historyScratchRef.current = null;
      historyIndexRef.current = null;
      setHistoryIndex(null);
      return true;
    }
    const entry = history[stepped.index];
    if (!entry) return false;
    applyComposerSnapshot(entry);
    historyIndexRef.current = stepped.index;
    setHistoryIndex(stepped.index);
    return true;
  }, [
    applyComposerSnapshot,
    attachments,
    capabilityRefs,
    historySessionKey,
    prompt,
    promptDocument,
  ]);

  const openStashPanel = useCallback(() => {
    refreshStashes();
    setStashName(defaultStashName(plainTextFromDocument(promptDocument)));
    setStashOpen(true);
  }, [promptDocument, refreshStashes]);

  const handleStashSave = useCallback(() => {
    const saved = saveComposerStash({
      name: stashName,
      draftKey,
      projectId,
      snapshot: {
        prompt,
        promptSegments: promptDocument.segments,
        capabilityRefs,
        attachments,
        credentialId,
        runtimeKey,
        model,
        effort,
        accessMode,
        projectId,
      },
    });
    if (!saved) {
      setNotice("当前草稿为空，无法暂存");
      return;
    }
    refreshStashes();
    setNotice(`已暂存「${saved.name}」`);
  }, [
    accessMode,
    attachments,
    capabilityRefs,
    credentialId,
    runtimeKey,
    draftKey,
    effort,
    model,
    projectId,
    prompt,
    promptDocument,
    refreshStashes,
    stashName,
  ]);

  const handleStashRestore = useCallback((stashId: string) => {
    const restored = hydrateComposerStash(stashId, { draftKey, projectId });
    if (!restored) return;
    applyingHistoryRef.current = true;
    historyIndexRef.current = null;
    setHistoryIndex(null);
    historyScratchRef.current = null;
    const doc = promptDocumentFromDraft(restored.entry.snapshot);
    setPromptDocument(doc);
    setPrompt(plainTextFromDocument(doc));
    setCapabilityRefs(composerRefs(doc));
    setAttachments(restored.attachments);
    const targetProject = restored.entry.projectId || restored.entry.snapshot.projectId || "";
    const projectStillThere = Boolean(
      targetProject && projects.some((row) => row.project_id === targetProject),
    );
    if (projectStillThere) setProjectId(targetProject);
    const restoredBinding = resolveComposerBindingRestore({
      snapshot: restored.entry.snapshot,
      current: { credentialId, runtimeKey, model, effort, accessMode },
      credentials,
      runtimes,
    });
    // An explicit restore outranks project defaults for this draft/project.
    restoredBindingContextRef.current = { draftKey, projectId: projectStillThere ? targetProject : projectId };
    if (restoredBinding.restored) {
      const selection = restoredBinding.selection;
      setCredentialId(selection.credentialId);
      setRuntimeKey(selection.runtimeKey);
      setModel(selection.model);
      setEffort(selection.effort);
      setAccessMode(selection.accessMode);
    }
    const warnings: string[] = [];
    if (targetProject && !projectStillThere) warnings.push("原项目已不可用，附件需重新关联后再发送");
    else if (restored.environmentMismatch && !projectStillThere) warnings.push("暂存来自其他项目，附件需确认后重新关联");
    if (restored.restoredNeedsReselect) warnings.push("部分附件需重新选择后再发送");
    if (restoredBinding.legacyRuntime) warnings.push("旧暂存未记录实例，已使用该引擎当前可用的 Runtime");
    const bindingNotice = restoredBinding.restored
      ? "已恢复接入点、Runtime 实例、模型、推理强度和权限设置"
      : `无法恢复运行设置：${restoredBinding.reason}，完整保留当前运行设置（包括推理强度和权限）`;
    setNotice(`已恢复暂存「${restored.entry.name}」；${bindingNotice}${warnings.length ? `；${warnings.join("；")}` : ""}`);
    setStashOpen(false);
    applyingHistoryRef.current = false;
  }, [draftKey, projectId, projects, credentials, runtimes, credentialId, runtimeKey, model, effort, accessMode, setNotice]);

  const handleStashDelete = useCallback((stashId: string) => {
    deleteComposerStash(stashId);
    refreshStashes();
  }, [refreshStashes]);

  const historyStatus = historyIndex === null
    ? ""
    : `历史 ${historyIndex + 1}/${Math.max(listPromptHistory(historySessionKey).length, historyIndex + 1)} · 编辑后使用方向键移动光标`;
  const stashRecallProps = {
    onHistoryNavigate: handleHistoryNavigate,
    historyBrowsing: historyIndex !== null,
    historyStatus,
    onStashShortcut: openStashPanel,
    onOpenStashPanel: openStashPanel,
    stashCount: stashes.length,
  };

  const openDetails = useCallback((payload: DrawerDetailPayload) => {
    setDetailsPayload(payload);
  }, []);

  const openDiffBaseline = useCallback((request: DiffBaselineRequest) => {
    if (!threadId) return;
    setDetailsPayload(null);
    chatPanel.openDiff(threadId, request);
  }, [threadId]);

  const openFilePreview = useCallback((request: { path: string; line?: number }) => {
    if (!threadId) return;
    setDetailsPayload(null);
    chatPanel.openFile(threadId, request.path, request.line);
  }, [threadId]);

  const openAttachmentPreview = useCallback((attachment: MessageAttachmentChip) => {
    if (!attachment.sha256) return;
    openDetails({
      type: "artifact",
      title: attachment.name || "附件",
      subtitle: attachment.sha256,
      artifact: {
        sha256: attachment.sha256,
        name: attachment.name,
        kind: attachment.kind,
        mediaType: attachment.mediaType,
        size: attachment.size,
      },
    });
  }, [openDetails]);

  const handleResourceLink = useCallback((target: ResourceLinkTarget) => {
    if (target.kind === "workspace") {
      openFilePreview({ path: target.path, line: target.line });
      return;
    }
    if (target.kind === "artifact") {
      const matched = (conversation.view?.artifacts || []).find(
        (item) => item.sha256 === target.sha256
          || item.sha256.startsWith(target.sha256)
          || target.sha256.startsWith(item.sha256.slice(0, 16)),
      );
      openDetails({
        type: "artifact",
        title: matched?.name || target.sha256.slice(0, 12),
        subtitle: target.sha256,
        artifact: {
          sha256: matched?.sha256 || target.sha256,
          name: matched?.name,
          kind: matched?.kind,
          mediaType: matched?.media_type,
          size: matched?.size,
        },
      });
      return;
    }
    if (target.kind === "message") {
      setHighlightedMessageId(target.messageId.replace(/^#/, ""));
    }
  }, [conversation.view?.artifacts, openDetails, openFilePreview]);

  // Load Runtimes, Credentials, Projects
  const loadRuntimes = useCallback(async (opts?: { silent?: boolean }) => {
    if (!opts?.silent) setRuntimeSource(sourceLoading());
    try {
      const rows = await fetchRuntimeInstances();
      setRuntimes(rows.filter((row) => row.enabled !== false && row.adapter_id !== "cli.dsh" && row.adapter_id !== "deepseek.harness"));
      setRuntimeSource(sourceOk());
    } catch (exc) {
      const message = exc instanceof Error ? exc.message : String(exc);
      const httpStatus = (exc as { httpStatus?: number })?.httpStatus ?? parseHttpStatus(message);
      setRuntimeSource(sourceError(message, httpStatus));
    }
  }, []);

  const loadCredentials = useCallback(async (opts?: { fresh?: boolean; silent?: boolean }) => {
    const requestSeq = ++credentialLoadSeqRef.current;
    if (!opts?.silent) setCredentialSource(sourceLoading());
    try {
      const rows = (await fetchConversationCredentials({ fresh: opts?.fresh })).filter((row) => row.engine !== "dsh");
      if (requestSeq !== credentialLoadSeqRef.current) return;
      setCredentials(rows);
      setCredentialId((curr) => {
        if (curr && rows.some((r) => r.id === curr)) return curr;
        return resolvePreferredChatSelection(rows)?.credentialId || rows[0]?.id || "";
      });
      setCredentialSource(sourceOk());
    } catch (exc) {
      if (requestSeq !== credentialLoadSeqRef.current) return;
      const message = exc instanceof Error ? exc.message : String(exc);
      const httpStatus = (exc as { httpStatus?: number })?.httpStatus ?? parseHttpStatus(message);
      setCredentialSource(sourceError(message, httpStatus));
    }
  }, []);

  const loadProjects = useCallback(async (opts?: { silent?: boolean }) => {
    if (!opts?.silent) setProjectSource(sourceLoading());
    try {
      const rows = await fetchConversationProjects();
      setProjects(rows);
      setProjectSource(sourceOk());
    } catch (exc) {
      const message = exc instanceof Error ? exc.message : String(exc);
      const httpStatus = (exc as { httpStatus?: number })?.httpStatus ?? parseHttpStatus(message);
      setProjectSource(sourceError(message, httpStatus));
    }
  }, []);

  const reloadReadiness = useCallback(async (opts?: { fresh?: boolean; silent?: boolean }) => {
    await Promise.all([
      loadCredentials({ fresh: opts?.fresh, silent: opts?.silent }),
      loadRuntimes({ silent: opts?.silent }),
      loadProjects({ silent: opts?.silent }),
    ]);
  }, [loadCredentials, loadProjects, loadRuntimes]);

  // A batch may contain completion followed by usage or the next queued turn.
  // Refresh from any newly observed completion, not only the final SSE event.
  const completedModelTurnSeq = conversation.events.findLast(
    (event) => event.event_type === "core.turn.completed",
  )?.seq;
  useEffect(() => {
    if (completedModelTurnSeq === undefined) return;
    void loadCredentials({ silent: true });
  }, [threadId, completedModelTurnSeq, loadCredentials]);

  useEffect(() => {
    void loadRuntimes();
  }, [loadRuntimes]);
  useEffect(() => {
    void loadCredentials();
  }, [loadCredentials]);
  useEffect(() => {
    void loadProjects();
  }, [loadProjects]);

  // Recheck after returning from Agents / host CLI login without wiping drafts.
  useEffect(() => {
    let timer: number | undefined;
    const schedule = () => {
      if (document.visibilityState === "hidden") return;
      if (commandBusyRef.current || probing) return;
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        void reloadReadiness({ fresh: true, silent: true });
      }, 1200);
    };
    const onFocus = () => schedule();
    const onVisibility = () => {
      if (document.visibilityState === "visible") schedule();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.clearTimeout(timer);
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [probing, reloadReadiness]);

  // Sync composer controls from the persisted thread runtime.
  // Depend on the concrete runtime fields, not the whole view object: SSE
  // snapshots and refresh() replace `conversation.view` often, and re-applying
  // the same saved access_mode would wipe a local selection the user just made.
  const viewThreadId = conversation.view?.thread.thread_id || "";
  const viewRuntime = conversation.view?.runtime;
  const viewRuntimeKey = viewRuntime
    ? `${viewRuntime.adapter_id}:${viewRuntime.instance_id}`
    : "";
  const viewCredentialId = viewRuntime?.credential_id || viewRuntime?.credential_ref || "";
  const viewModel = viewRuntime?.model || "";
  const viewEffort = viewRuntime?.effort || "";
  const viewAccessMode = viewRuntime?.access_mode || "supervised";
  const viewMode = conversation.view?.thread.mode;
  const viewProjectId = conversation.view?.thread.project_id || "";
  useEffect(() => {
    if (!viewThreadId || viewThreadId !== threadId) return;
    const stored = readComposerDraft(composerDraftKey(viewThreadId));
    // Imported / unbound threads have empty view runtime. Prefer draft, then
    // view binding, then the same preferred credential the picker would show
    // so display and send readiness stay aligned (#136).
    const bind = resolveComposerRuntimeBind({
      draftCredentialId: stored?.credentialId,
      draftModel: stored?.model,
      draftRuntimeKey: stored?.runtimeKey,
      viewCredentialId,
      viewModel,
      viewRuntimeKey,
      credentials,
      runtimes,
      savedDefault: readChatDefaultModel(),
    });
    setRuntimeKey(bind.runtimeKey);
    setCredentialId(bind.credentialId);
    setModel(bind.modelId);
    setEffort(stored?.effort ?? viewEffort);
    setAccessMode(stored?.accessMode || viewAccessMode);
    if (viewMode) setMode(viewMode);
    setProjectId(viewProjectId);
  }, [
    threadId,
    credentials,
    runtimes,
    viewAccessMode,
    viewCredentialId,
    viewEffort,
    viewMode,
    viewModel,
    viewProjectId,
    viewRuntimeKey,
    viewThreadId,
  ]);

  // Load Memory
  const loadMemory = useCallback(async () => {
    if (!threadId) return;
    setMemoryLoading(true);
    try {
      setMemory(await fetchConversationMemory(threadId));
    } catch {
      // Non-blocking
    } finally {
      setMemoryLoading(false);
    }
  }, [threadId]);

  useEffect(() => {
    if (threadId) void loadMemory();
  }, [loadMemory, threadId]);

  const selectedCredential = useMemo(
    () => credentials.find((c) => c.id === credentialId),
    [credentialId, credentials],
  );

  const selectedRuntime = useMemo(
    () => runtimes.find((r) => r.key === runtimeKey),
    [runtimeKey, runtimes],
  );

  const selectedProject = useMemo(
    () => projects.find((project) => project.project_id === projectId),
    [projectId, projects],
  );

  const readiness = useMemo(() => classifyConversationReadiness({
    sources: {
      credentials: credentialSource,
      runtimes: runtimeSource,
      projects: projectSource,
    },
    credentials,
    runtimes,
    projects,
    selected: {
      credentialId,
      runtimeKey,
      projectId,
      modelId: model,
    },
    probing,
    directoryIssue,
    guideDismissed,
    hasThread: Boolean(threadId),
  }), [
    credentialId,
    credentialSource,
    credentials,
    directoryIssue,
    guideDismissed,
    model,
    probing,
    projectId,
    projectSource,
    projects,
    runtimeKey,
    runtimeSource,
    runtimes,
    threadId,
  ]);

  const handleDismissGuide = useCallback(() => {
    setGuideDismissed(true);
    try {
      window.localStorage.setItem(GUIDE_DISMISSED_STORAGE_KEY, "1");
    } catch {
      // Optional preference.
    }
  }, []);

  const handleProbeAgent = useCallback(async () => {
    const target = selectedRuntime
      || (selectedCredential ? runtimeForEngine(selectedCredential.engine, runtimes) : undefined);
    if (!target?.key || probing) return;
    setProbing(true);
    setError("");
    try {
      await probeRuntimeInstance(target.key);
      await loadRuntimes({ silent: true });
      setRuntimeKey(target.key);
    } catch (exc) {
      setError(exc instanceof Error ? exc.message : String(exc));
      await loadRuntimes({ silent: true });
    } finally {
      setProbing(false);
    }
  }, [loadRuntimes, probing, runtimes, selectedCredential, selectedRuntime, setError]);

  const handleCreateProjectFromPath = useCallback(async (rawPath: string): Promise<string | null> => {
    if (projectCreating) return null;
    const path = rawPath.trim();
    if (!path) {
      // Throw so ConversationContextPicker can show inline field feedback
      // instead of the outer readiness banner behind the open modal.
      throw new Error("请输入有效的工作目录路径");
    }
    setProjectCreating(true);
    setError("");
    setNotice("");
    setDirectoryIssue(null);
    try {
      const name = pathBasename(path) || path;
      const existing = projects.find((project) => project.root_path === path);
      if (existing) {
        setProjectId(existing.project_id);
        setNotice(`已选择项目 ${existing.name}`);
        setRecentPaths(rememberRecentPath(existing.root_path, [path]));
        return existing.project_id;
      }
      const { projectId: createdProjectId, rootPath, reused } = await createConversationProject({
        name,
        root_path: path,
      });
      const rows = await fetchConversationProjects();
      const selectedProject = rows.find((project) => project.project_id === createdProjectId);
      setProjects(rows);
      setProjectSource(sourceOk());
      setProjectId(createdProjectId);
      setNotice(reused
        ? `已选择项目 ${selectedProject?.name || name}`
        : `项目 ${name} 已添加`);
      setRecentPaths(rememberRecentPath(rootPath || selectedProject?.root_path || path, [path]));
      setPreferPathInput(false);
      return createdProjectId;
    } finally {
      setProjectCreating(false);
    }
  }, [projectCreating, projects, setError, setNotice]);

  const composerCapabilityContext = useMemo<ComposerCapabilityContext | undefined>(() => {
    const adapterId = selectedRuntime?.adapter_id || "";
    if (!adapterId) return undefined;
    return {
      threadId: threadId || undefined,
      adapterId,
      revision: conversation.view?.runtime_connection?.capability_revision ?? 0,
      workspaceId: (threadId ? conversation.view?.thread.workspace_id : selectedProject?.workspace_id) || undefined,
      projectId: (threadId ? conversation.view?.thread.project_id : selectedProject?.project_id) || undefined,
    };
  }, [
    conversation.view?.runtime_connection?.capability_revision,
    conversation.view?.thread.project_id,
    conversation.view?.thread.workspace_id,
    selectedProject?.project_id,
    selectedProject?.workspace_id,
    selectedRuntime?.adapter_id,
    threadId,
  ]);

  const handleSelectModelParams = useCallback((params: {
    credentialId: string;
    model: string;
    effort?: string;
    accessMode?: string;
  }) => {
    const credential = credentials.find((item) => item.id === params.credentialId);
    const runtime = credential
      ? runtimes.find((row) => row.key === runtimeKey && row.engine === credential.engine && row.enabled !== false)
        || runtimeForEngine(credential.engine, runtimes)
      : undefined;
    const engineChanged = Boolean(
      credential
      && selectedCredential
      && credential.engine !== selectedCredential.engine,
    );
    setCredentialId(params.credentialId);
    setModel(params.model);
    setRuntimeKey(runtime?.key || "");
    if (params.effort !== undefined) setEffort(params.effort);
    if (params.accessMode !== undefined) setAccessMode(params.accessMode);
    restoredBindingContextRef.current = { draftKey, projectId };
    writeChatLastSelection({
      credentialId: params.credentialId, modelId: params.model,
      runtimeKey: runtime?.key || "", effort: params.effort ?? effort,
      accessMode: params.accessMode ?? accessMode,
    });
    if (engineChanged) {
      const next = documentFromPromptAndRefs(plainTextFromDocument(promptDocument), []);
      setPromptDocument(next);
      setPrompt(plainTextFromDocument(next));
      setCapabilityRefs([]);
    }
  }, [credentials, runtimes, runtimeKey, selectedCredential, promptDocument, draftKey, projectId, effort, accessMode]);

  const handleProjectChange = useCallback((nextProjectId: string) => {
    restoredBindingContextRef.current = null;
    appliedNewChatDefaultsRef.current = "";
    setProjectId(nextProjectId);
    setCapabilityRefs([]);
    setWorkspaceMode("shared_checkout");
    setWorktreeBranch("");
    setExistingWorktreePath("");
    // Project defaults are applied by the dedicated effect below so all paths
    // (handleProjectChange, pendingProjectIdRef, draft-hydrate, leave-thread)
    // are handled uniformly.
  }, []);

  const handleWorkspaceModeChange = useCallback((mode: WorkspaceBindMode) => {
    setWorkspaceMode(mode);
    setExistingWorktreePath("");
    if (mode !== "new_worktree") setWorktreeBranch("");
  }, []);

  const projectConvDefaults = useMemo(
    () => readProjectConvDefaults(selectedProject?.settings as Record<string, unknown> | undefined),
    [selectedProject],
  );

  const projectDefaultActive = Boolean(
    !threadId
    && projectId
    && (projectConvDefaults.credentialId || projectConvDefaults.modelId),
  );

  const handleSetProjectDefault = useCallback(async () => {
    if (!projectId || savingProjectDefault) return;
    setSavingProjectDefault(true);
    setError("");
    setNotice("");
    try {
      await updateConversationProjectSettings(
        projectId,
        projectConvDefaultsPayload({ credentialId, modelId: model, effort, accessMode }),
      );
      const feedback = projectDefaultPersistFeedback("set", { ok: true });
      setError(feedback.error);
      setNotice(feedback.notice);
      if (feedback.applyLocalProjects) {
        restoredBindingContextRef.current = null;
        setProjects((prev) => prev.map((p) =>
          p.project_id === projectId
            ? {
                ...p,
                settings: {
                  ...(p.settings || {}),
                  conv_default_credential_id: credentialId,
                  conv_default_model: model,
                  conv_default_effort: effort,
                  conv_default_access_mode: accessMode,
                },
              }
            : p,
        ));
      }
    } catch (exc) {
      const feedback = projectDefaultPersistFeedback("set", { ok: false, error: exc });
      setError(feedback.error);
      setNotice(feedback.notice);
    } finally {
      setSavingProjectDefault(false);
    }
  }, [accessMode, credentialId, effort, model, projectId, savingProjectDefault, setError, setNotice]);

  const handleClearProjectDefault = useCallback(async () => {
    if (!projectId || savingProjectDefault) return;
    setSavingProjectDefault(true);
    setError("");
    setNotice("");
    try {
      await updateConversationProjectSettings(projectId, projectConvDefaultsPayload(null));
      const feedback = projectDefaultPersistFeedback("clear", { ok: true });
      setError(feedback.error);
      setNotice(feedback.notice);
      if (feedback.applyLocalProjects) {
        restoredBindingContextRef.current = null;
        setProjects((prev) => prev.map((p) => {
          if (p.project_id !== projectId) return p;
          const next = { ...(p.settings || {}) };
          delete next.conv_default_credential_id;
          delete next.conv_default_model;
          delete next.conv_default_effort;
          delete next.conv_default_access_mode;
          return { ...p, settings: next };
        }));
      }
    } catch (exc) {
      const feedback = projectDefaultPersistFeedback("clear", { ok: false, error: exc });
      setError(feedback.error);
      setNotice(feedback.notice);
    } finally {
      setSavingProjectDefault(false);
    }
  }, [projectId, savingProjectDefault, setError, setNotice]);

  const accessModes = useMemo(() => {
    const raw = selectedRuntime?.access_modes
      || selectedRuntime?.health?.capabilities?.access_modes;
    return Array.isArray(raw) ? raw.map(String).filter(Boolean) : [];
  }, [selectedRuntime]);

  useEffect(() => {
    if (draftHydratingRef.current || hydratedDraftKey !== draftKey) return;
    if (!accessModes.length || accessModes.includes(accessMode)) return;
    setAccessMode(
      accessModes.includes("supervised") ? "supervised" : accessModes[0],
    );
  }, [accessMode, accessModes, draftKey, hydratedDraftKey]);

  // Sync default runtime & model when credential changes in new-thread mode
  useEffect(() => {
    if (threadId || !selectedCredential || draftHydratingRef.current || hydratedDraftKey !== draftKey) return;
    setRuntimeKey((current) => {
      const exact = runtimes.find((row) => row.key === current && row.engine === selectedCredential.engine && row.enabled !== false);
      return exact?.key || runtimeForEngine(selectedCredential.engine, runtimes)?.key || "";
    });
    const modelIds = allCredentialModels(selectedCredential).map((m) => m.id);
    const saved = readChatLastSelection() || readChatDefaultModel();
    setModel((curr) =>
      curr && modelIds.includes(curr)
        ? curr
        : saved
          && saved.credentialId === selectedCredential.id
          && modelIds.includes(saved.modelId)
          ? saved.modelId
        : selectedCredential.default_model &&
          selectedCredential.default_model !== "default" &&
          modelIds.includes(selectedCredential.default_model)
        ? selectedCredential.default_model
        : modelIds[0] || "",
    );
  }, [draftKey, hydratedDraftKey, runtimes, selectedCredential, threadId]);

  // Initialize once per draft/project/default change, after asynchronous catalogs
  // and draft hydration finish. Background refreshes must not reset the picker.
  useEffect(() => {
    if (threadId || hydratedDraftKey !== draftKey || draftHydratingRef.current) return;
    if (credentialSource.phase !== "ok" || runtimeSource.phase !== "ok") return;
    if (projectId && projectSource.phase !== "ok") return;
    const restoredContext = restoredBindingContextRef.current;
    if (restoredContext?.draftKey === draftKey && restoredContext.projectId === projectId) return;
    const project = projects.find((p) => p.project_id === projectId);
    const projDefaults = readProjectConvDefaults(project?.settings as Record<string, unknown> | undefined);
    const contextKey = JSON.stringify([draftKey, projectId, projDefaults]);
    if (appliedNewChatDefaultsRef.current === contextKey) return;
    const selection = resolveNewConversationSelection({
      credentials: credentialRows, runtimes, project: projDefaults,
      recent: readChatLastSelection(), configured: readChatDefaultModel(),
    });
    if (!selection) return;
    appliedNewChatDefaultsRef.current = contextKey;
    setCredentialId(selection.credentialId);
    setRuntimeKey(selection.runtimeKey);
    setModel(selection.model);
    setEffort(selection.effort);
    setAccessMode(selection.accessMode);
  }, [credentialRows, credentialSource.phase, draftKey, hydratedDraftKey, projectId, projects, projectSource.phase, runtimes, runtimeSource.phase, threadId]);

  const threadMatrix = useMemo(
    () => matrixFromRuntimeConnection(conversation.view?.runtime_connection),
    [conversation.view?.runtime_connection],
  );

  // Preview belongs to the current selection, not the picker action that last
  // changed it. Returning to the bound runtime (including stash restore) clears it.
  const providerPreview = providerSwitchPreview({
    threadId,
    viewThreadId,
    boundRuntime: viewRuntime,
    credentialId,
    runtime: selectedRuntime,
    matrix: threadMatrix,
  });
  const providerPreviewKey = JSON.stringify([providerScope, viewRuntimeKey, viewCredentialId, providerPreview]);
  const providerSwitchNotice = providerPreview
    ? (dismissedProviderPreview === providerPreviewKey ? "" : providerPreview)
    : (viewThreadId === threadId ? handoffNotice : "");

  // C38: only show a session rebuild response for the selection/session that
  // produced it; a late response from the previous provider cannot overwrite it.
  useEffect(() => {
    if (latestConversationEventType !== "core.thread.runtime_switched") return;
    const text = runtimeHandoffNotice({
      threadId,
      viewThreadId,
      runtimeKey,
      credentialId,
      model,
      sessionId: conversation.view?.state.agent_session_id,
      payload: (latestConversationEvent?.payload || {}) as Record<string, unknown>,
    });
    if (text) setHandoffNotice(text);
  }, [
    latestConversationEvent, latestConversationEventId, latestConversationEventType,
    threadId, viewThreadId, runtimeKey, credentialId, model,
    conversation.view?.state.agent_session_id, setHandoffNotice,
  ]);

  const running = Boolean(conversation.view?.state.running_turn_id);
  const steerEnabled = canInvoke(threadMatrix, "steer");
  const canSteer = Boolean(running && steerEnabled);
  const steerDisable = disableCopy(threadMatrix, "steer");
  const capabilityRevision = Number(
    threadMatrix?.revision
      ?? conversation.view?.runtime_connection?.capability_revision
      ?? 0,
  );
  const viewMatchesThread = Boolean(
    threadId
    && conversation.view
    && conversation.view.thread.thread_id === threadId,
  );
  const switchingThread = Boolean(
    threadId
    && conversation.view
    && conversation.view.thread.thread_id !== threadId,
  );
  // URL has a threadId but the first detail fetch failed with no cached view —
  // never treat that as the new-chat welcome home.
  const firstThreadLoadFailed = Boolean(
    threadId && !conversation.loading && !conversation.view,
  );
  const interactionBusy = busy || switchingThread;
  // Full-stream overlay only while switching away from a still-mounted snapshot.
  const threadLoadError = switchingThread ? conversation.error : "";
  // Same-thread refresh / load-more failures keep content and show a soft banner.
  const sessionSyncError = viewMatchesThread ? conversation.error : "";
  const lastTurnStatus = conversation.view?.turns.at(-1)?.status || "";
  const activeProjectName = useMemo(
    () => projects.find((project) => project.project_id === conversation.view?.thread.project_id)?.name,
    [conversation.view?.thread.project_id, projects],
  );
  const sidebarThreads = useMemo(() => {
    const view = conversation.view;
    if (!view || view.thread.thread_id !== threadId) return threads;
    // The new thread view can arrive before the inbox/list refresh. Insert it
    // immediately with its persisted project so its folder and count agree.
    return upsertConversationThread(threads, { ...view.thread, state: view.state });
  }, [conversation.view, threadId, threads]);

  // 打开对话时后端会 mark_read；回合结束后同步侧栏转圈 / 未读点。
  useEffect(() => {
    if (!threadId || conversation.loading) return;
    if (conversation.view?.thread.thread_id !== threadId) return;
    void refreshList();
  }, [
    threadId,
    conversation.loading,
    conversation.view?.thread.thread_id,
    conversation.view?.state.unread,
    conversation.view?.state.running_turn_id,
    conversation.view?.thread.metadata_revision,
    lastTurnStatus,
    refreshList,
  ]);

  // Command executor helper
  const execute = useCallback(
    async (action: () => Promise<unknown>, failureKind: "command" | "send" = "command") => {
      if (commandBusyRef.current) return false;
      commandBusyRef.current = true;
      setBusy(true);
      setError("");
      setNotice("");
      let actionCompleted = false;
      try {
        await action();
        actionCompleted = true;
        await Promise.all([conversation.refresh(), refreshList()]);
        return true;
      } catch (exc) {
        if (failureKind === "send" && !actionCompleted) {
          setError(
            formatSendFailureMessage(exc, { keepDraft: true }),
            shouldOfferSendRetry(exc) ? "send" : null,
          );
        } else {
          setError(exc instanceof Error ? exc.message : String(exc));
        }
        return false;
      } finally {
        commandBusyRef.current = false;
        setBusy(false);
      }
    },
    [conversation, refreshList, setError, setNotice],
  );

  const handleCreateProjectFromDirectory = useCallback(async (): Promise<string | null> => {
    if (projectCreating) return null;
    setProjectCreating(true);
    setError("");
    setNotice("");
    setDirectoryIssue(null);
    try {
      const selection = await selectConversationDirectory();
      if (!selection) return null;

      const existing = projects.find((project) => project.root_path === selection.path);
      if (existing) {
        setProjectId(existing.project_id);
        setNotice(`已选择项目 ${existing.name}`);
        setRecentPaths(rememberRecentPath(existing.root_path, [selection.path]));
        return existing.project_id;
      }

      const { projectId: createdProjectId, rootPath, reused } = await createConversationProject({
        name: selection.name,
        root_path: selection.path,
      });
      const rows = await fetchConversationProjects();
      const selectedProject = rows.find((project) => project.project_id === createdProjectId);
      setProjects(rows);
      setProjectSource(sourceOk());
      setProjectId(createdProjectId);
      setNotice(reused
        ? `已选择项目 ${selectedProject?.name || selection.name}`
        : `项目 ${selection.name} 已添加`);
      setRecentPaths(rememberRecentPath(
        rootPath || selectedProject?.root_path || selection.path,
        [selection.path],
      ));
      return createdProjectId;
    } catch (exc) {
      const message = exc instanceof Error ? exc.message : String(exc);
      if (isDirectoryPickerUnavailable(exc)) {
        setPreferPathInput(true);
        setRequestPathInput(true);
        setDirectoryIssue({
          message: "系统目录选择器不可用，请输入工作目录路径",
        });
        return null;
      }
      if (isDirectoryInaccessibleMessage(message)) {
        setDirectoryIssue({ message });
      } else {
        setError(message);
      }
      return null;
    } finally {
      setProjectCreating(false);
    }
  }, [projectCreating, projects, setError, setNotice]);

  const handleClientCommand = async (action: string, args = "") => {
    const name = action.replace(/^ui:/, "");
    setPromptDocument(emptyPromptDocument()); setPrompt(""); setCapabilityRefs([]);
    if (name === "new") { startNewChat(projectId, true); return; }
    if (name === "help") {
      const text = `/${args}`;
      setPrompt(text); setPromptDocument(documentFromPromptAndRefs(text, []));
      requestAnimationFrame(() => document.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
      return;
    }
    if (["model", "effort", "permissions"].includes(name)) {
      if (args && name === "model" && selectedCredential && allCredentialModels(selectedCredential).some((m) => m.id === args)) {
        handleSelectModelParams({ credentialId, model: args }); setNotice(`已选择模型 ${args}`); return;
      }
      if (args && name === "effort" && selectedCredential && validModelEffort(allCredentialModels(selectedCredential).find((m) => m.id === model), args)) {
        setEffort(args); setNotice(`已设置思考强度 ${args}`); return;
      }
      setModelPickerOpen(true);
      if (args) setNotice("请在选择器中确认当前引擎支持的设置");
      return;
    }
    if (["skills", "plugins", "mcp"].includes(name)) {
      const engine = selectedCredential?.engine || "codex";
      const params = new URLSearchParams({ engine, source: name === "plugins" ? "managed" : "native", type: name === "skills" ? "skill" : name === "mcp" ? "mcp" : "all" });
      router.push(`/settings/chat-plugins?${params}`); return;
    }
    if (name === "sessions") { setSearchQuery(args); setCommandSessionsOpen(true); return; }
    if (!threadId || !conversation.view) { setNotice("请先开始一段聊天"); return; }
    if (name === "rename") {
      if (args) { await handleRename(threadId, args); return; }
      setRenameTarget({ ...conversation.view.thread, state: conversation.view.state }); setRenameInput(conversation.view.thread.title); return;
    }
    if (name === "fork") { handleFork(threadId); return; }
    if (name === "export") { setExportDialogOpen(true); return; }
    if (name === "rewind") {
      const turn = conversation.view.turns.at(-1);
      if (turn) handleRewindTurn(turn.turn_id); else setNotice("当前没有可回退的轮次");
      return;
    }
    if (name === "diff") { chatPanel.open(threadId, "diff"); return; }
    if (name === "details") { setBottomPanelOpen(true); return; }
    if (name === "copy") {
      const message = [...conversation.view.messages].reverse().find((m) => m.role === "assistant" && m.text);
      setNotice(message && await copyToClipboard(message.text) ? "已复制助手回复" : "当前没有可复制的回复"); return;
    }
    setDetailsPayload({ type: "tool", title: "聊天状态与用量", output: {
      runtime: conversation.view.runtime, statistics: conversation.view.statistics, context: conversation.view.context_window,
    }});
  };

  // Send message or Create Thread + Send (C02: stable idempotent intent)
  const handleSend = async () => {
    const activeContext = activeSendContextRef.current;
    if (activeContext.draftKey !== draftKey) return;
    if (threadId && activeContext.viewThreadId !== threadId) return;
    const text = plainTextFromDocument(promptDocument).trim();
    if (["/clear", "/new", "/clear-input"].includes(text)) {
      setPromptDocument(emptyPromptDocument());
      setPrompt("");
      setCapabilityRefs([]);
      if (text !== "/clear-input") startNewChat(projectId, true);
      return;
    }
    const localCommand = /^\/([a-zA-Z][\w:.-]*)(?:\s+([\s\S]*))?$/.exec(text);
    if (localCommand && composerCapabilityContext) {
      try {
        const catalog = await fetchComposerCapabilities(composerCapabilityContext, "/", localCommand[1]);
        if (activeSendContextRef.current.draftKey !== draftKey) return;
        const item = catalog.items.find((row) => row.kind === "command" && row.name.toLowerCase() === localCommand[1].toLowerCase());
        if (item?.action?.startsWith("ui:")) { await handleClientCommand(item.action, localCommand[2] || ""); return; }
        if (item?.invocable === false) { setError(item.reason || "当前引擎不支持此命令"); return; }
      } catch (error) { setError(error instanceof Error ? error.message : "命令目录加载失败"); return; }
    }
    const structuredRefs = wireCapabilityRefs(promptDocument);
    if ((!text && !structuredRefs.length && !attachments.length) || busy) return;
    if (threadId && isThreadArchived(threadId)) {
      setError("已归档的对话不能发送消息，请先取消归档");
      return;
    }

    const block = primarySendBlock(readiness);
    if (block) {
      // Site-down / proxy 502 readiness block → same disconnect send strip (with retry).
      const siteDownBlock = block.kind === "config_read_failed"
        && (
          block.message.includes("连接已断开")
          || isConnectionSourceFailure(credentialSource)
          || isConnectionSourceFailure(runtimeSource)
        );
      if (siteDownBlock) {
        setNotice("");
        setError(formatSendFailureMessage(new Error("Failed to fetch"), { keepDraft: true }), "send");
        return;
      }
      setError(block.message);
      return;
    }

    await execute(async () => {
      let target = threadId;
      const cred = selectedCredential;
      if (!cred || !credentialAvailable(cred)) {
        throw new Error("请选择可用的 Agent 接入点；可前往设置 → Agents 配置");
      }
      if (!model || !allCredentialModels(cred).some((m) => m.id === model)) {
        throw new Error("请选择模型；可前往设置 → Agents 补充模型列表");
      }
      if (!validModelEffort(allCredentialModels(cred).find((item) => item.id === model), effort)) {
        throw new Error("当前 Agent 和模型不支持已选的思考程度，请重新选择或切回默认");
      }
      if (selectedRuntime && selectedRuntime.engine !== cred.engine) {
        throw new Error("模型接入点与 Runtime 引擎不一致，请重新选择模型后发送");
      }
      let rt = selectedRuntime || runtimeForEngine(cred.engine, runtimes);
      if (!rt) {
        throw new Error(`${cred.engine} 没有可用的 Agent 接入；请前往设置检查 Runtime`);
      }
      if (!runtimeUsable(rt) && isRuntimeUnprobed(rt)) {
        setProbing(true);
        try {
          await probeRuntimeInstance(rt.key);
          const rows = await fetchRuntimeInstances();
          const filtered = rows.filter((row) => (
            row.enabled !== false
            && row.adapter_id !== "cli.dsh"
            && row.adapter_id !== "deepseek.harness"
          ));
          setRuntimes(filtered);
          setRuntimeSource(sourceOk());
          rt = filtered.find((row) => row.key === rt!.key) || rt;
        } finally {
          setProbing(false);
        }
      }
      if (!runtimeUsable(rt)) {
        throw new Error(
          rt.health?.detail
            ? `${cred.engine} 接入异常：${rt.health.detail}`
            : `${cred.engine} 没有可用的 Agent 接入；请前往设置检查 Runtime`,
        );
      }
      setRuntimeKey(rt.key);
      const runtimePayload = {
        adapter_id: rt.adapter_id,
        instance_id: rt.instance_id,
        credential_id: cred.id,
        model,
        // Empty is an explicit reset. Omitting the field retains the previous
        // thread setting when the backend merges runtime selections.
        effort: effort === "default" ? "" : effort,
        access_mode: accessMode,
      };
      writeChatLastSelection({
        credentialId: cred.id, modelId: model, runtimeKey: rt.key,
        effort: runtimePayload.effort, accessMode,
      });

      const intent = await prepareSendIntent(
        draftKey,
        text,
        attachments.map((item) => String(item.id || "").trim()).filter(Boolean),
        structuredRefs,
        runtimePayload,
        recoverCommandReceipt,
        !threadId ? {
          project_id: projectId,
          workspace_id: selectedProject?.workspace_id || "",
          mode,
          workspace_mode: workspaceMode,
          worktree_branch: workspaceMode === "new_worktree" ? worktreeBranch.trim() : "",
          existing_worktree_path: workspaceMode === "existing_worktree" ? normalizeWorktreePath(existingWorktreePath) : "",
        } : undefined,
      );
      // #201 — Snapshot this submit so a late success/failure callback cannot
      // wipe a newer draft typed while the request was in flight.
      const submittedDraft = captureSendDraftSnapshot({
        draftKey,
        promptText: text,
        promptDocument,
        attachmentIds: attachments.map((item) => item.id),
      });
      // Failed intents are being retried — show a progress phase, not the failure copy.
      setNotice(phaseNotice(intent.phase === "failed" ? "sending" : intent.phase));

      try {
        if (!target) {
          if (projectId && !selectedProject) {
            throw new Error("所选项目的工作目录不可用，请重新选择项目");
          }
          if (intent.threadId) {
            target = intent.threadId;
          } else {
            updateSendIntent(draftKey, { phase: "creating_thread" });
            setNotice(phaseNotice("creating_thread"));

            let workspaceId = selectedProject?.workspace_id;
            if (selectedProject && workspaceMode === "new_worktree") {
              const branch = worktreeBranch.trim() || `muteki-${Date.now().toString(36)}`;
              const bound = await bindConversationWorkspace({
                project_id: selectedProject.project_id,
                mode: "new_worktree",
                branch,
                base_ref: "HEAD",
                parent_root: selectedProject.root_path,
              });
              workspaceId = bound.workspaceId;
            } else if (selectedProject && workspaceMode === "existing_worktree") {
              const rootPath = normalizeWorktreePath(existingWorktreePath);
              if (!canBindExistingWorktree(rootPath)) {
                throw new Error(worktreeMissingSelectionCopy());
              }
              const bound = await bindConversationWorkspace({
                project_id: selectedProject.project_id,
                mode: "existing_worktree",
                root_path: rootPath,
                parent_root: selectedProject.root_path,
              });
              workspaceId = bound.workspaceId;
            }

            target = await createConversationThread(
              {
                title: "新对话",
                title_source: "fallback",
                mode,
                ...(selectedProject ? {
                  project_id: selectedProject.project_id,
                  workspace_id: workspaceId,
                } : {}),
                runtime: runtimePayload,
              },
              {
                commandId: intent.createCommandId,
                idempotencyKey: intent.createCommandId,
              },
            );
            updateSendIntent(draftKey, { threadId: target, phase: "uploading" });
          }
        } else if (!intent.threadId) {
          updateSendIntent(draftKey, { threadId: target });
        }

        // Upload pending attachments and bind their content hashes to this
        // message. Queue items keep those hashes until they are promoted.
        // Per-file status is tracked independently: a single failure marks
        // that file as error but does not abort the remaining uploads.
        const attachmentHashes: string[] = [];
        const latestIntent = readSendIntent(draftKey) || intent;
        const uploadByAttachment = new Map(
          latestIntent.uploads.map((row) => [row.attachmentId, row]),
        );
        const uploadErrors: string[] = [];
        if (attachments.length > 0) {
          updateSendIntent(draftKey, { phase: "uploading" });
          setNotice(phaseNotice("uploading"));
          for (const a of attachments) {
            const attachmentId = String(a.id || "").trim();
            const uploadMeta = uploadByAttachment.get(attachmentId);
            if (a.sha256) {
              attachmentHashes.push(a.sha256);
              if (attachmentId && uploadMeta && !uploadMeta.sha256) {
                markSendUploadSha(draftKey, attachmentId, a.sha256);
              }
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id ? { ...row, uploadStatus: "done" as const } : row
              )));
              continue;
            }
            if (uploadMeta?.sha256) {
              attachmentHashes.push(uploadMeta.sha256);
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id ? { ...row, sha256: uploadMeta.sha256, uploadStatus: "done" as const } : row
              )));
              continue;
            }
            if (a.needsReselect || !a.file) {
              const msg = `附件 ${a.name} 需重新选择后再发送`;
              uploadErrors.push(msg);
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id ? { ...row, uploadStatus: "error" as const, uploadError: msg } : row
              )));
              continue;
            }
            // Mark uploading before the network call so the chip shows spinner.
            setAttachments((prev) => prev.map((row) => (
              row.id === a.id ? { ...row, uploadStatus: "uploading" as const, uploadError: undefined } : row
            )));
            try {
              const receipt = await uploadConversationFile(target, a.file, {
                commandId: uploadMeta?.commandId,
                idempotencyKey: uploadMeta?.commandId,
              });
              const sha256 = String(receipt.output?.sha256 || "").trim();
              if (!sha256) throw new Error(`附件 ${a.name} 上传回执缺少 sha256`);
              attachmentHashes.push(sha256);
              if (attachmentId) {
                markSendUploadSha(draftKey, attachmentId, sha256);
              }
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id ? { ...row, sha256, uploadStatus: "done" as const } : row
              )));
            } catch (uploadErr) {
              const errMsg = uploadErr instanceof Error ? uploadErr.message : `附件 ${a.name} 上传失败`;
              uploadErrors.push(errMsg);
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id
                  ? { ...row, uploadStatus: "error" as const, uploadError: errMsg }
                  : row
              )));
            }
          }
          // #184 — Any attachment failure blocks send. Successful hashes stay
          // on attachment rows for retry without re-upload; failed chips and
          // draft remain until the user fixes or removes them.
          const abortMessage = attachmentUploadAbortMessage(uploadErrors);
          if (abortMessage) {
            throw new Error(abortMessage);
          }
        }

        updateSendIntent(draftKey, { phase: "sending" });
        setNotice(phaseNotice("sending"));
        // Send turn with stable client_message_id + command/idempotency ids
        const sendReceipt = await sendConversationCommand(
          target,
          "conversation.turn.send",
          {
            text,
            client_message_id: intent.clientMessageId,
            attachments: attachmentHashes,
            runtime: runtimePayload,
            capability_refs: intent.capabilityRefs?.length
              ? intent.capabilityRefs
              : structuredRefs,
          },
          {
            commandId: intent.sendCommandId,
            idempotencyKey: intent.sendCommandId,
          },
        );
        const receiptState = String(sendReceipt.state || "accepted");
        updateSendIntent(draftKey, {
          phase: "accepted",
          lastReceiptState: receiptState,
          lastError: "",
        });
        // Accepted receipt ≠ Agent completed — always use the recoverable wording.
        const runtimeOperation = String(sendReceipt.output?.runtime_operation || "").trim();
        if (runtimeOperation) {
          const result = sendReceipt.output?.result as { message?: string } | undefined;
          setNotice(result?.message || `已完成 /${runtimeOperation}`);
          if (runtimeOperation !== "compact" && sendReceipt.output?.result) {
            setDetailsPayload({ type: "tool", title: `/${runtimeOperation}`, output: sendReceipt.output.result });
          }
        } else {
          setNotice(phaseNotice("accepted"));
        }
        recordSentPrompt(composerDraftKey(target), {
          prompt: text,
          promptSegments: promptDocument.segments,
          capabilityRefs: refsFromDocument(promptDocument),
          projectId,
        });
        historyIndexRef.current = null;
        setHistoryIndex(null);

        // #201 — Only clear composer when still on the same draft and content
        // still matches this submit; otherwise preserve newer input / attachments.
        const live = composerSnapshotRef.current;
        const liveSlice = {
          draftKey: live.draftKey,
          promptText: plainTextFromDocument(live.promptDocument).trim(),
          documentFingerprint: fingerprintPromptDocument(live.promptDocument),
          attachmentIds: live.attachments.map((row) => String(row.id || "").trim()).filter(Boolean),
        };
        const plan = planComposerAfterSendSuccess(liveSlice, submittedDraft);
        if (plan.action === "clear_all") {
          setPrompt("");
          setPromptDocument(emptyPromptDocument());
          setAttachments([]);
          setCapabilityRefs([]);
          clearComposerDraft(submittedDraft.draftKey);
        } else if (plan.action === "preserve_edits") {
          setAttachments((curr) => filterAttachmentsAfterSend(curr, plan.removeAttachmentIds));
        } else {
          const stored = readComposerDraft(submittedDraft.draftKey);
          if (shouldClearDraftStorageAfterSend(stored, submittedDraft)) {
            clearComposerDraft(submittedDraft.draftKey);
          }
        }
        clearSendIntent(submittedDraft.draftKey);

        // Do not steal navigation if the user already left this draft.
        if (!threadId && live.draftKey === submittedDraft.draftKey) {
          if (plan.action === "preserve_edits") {
            const remaining = filterAttachmentsAfterSend(
              live.attachments,
              plan.removeAttachmentIds,
            );
            writeComposerDraft(composerDraftKey(target), {
              prompt: live.prompt,
              promptSegments: live.promptDocument.segments,
              capabilityRefs: live.capabilityRefs,
              attachments: remaining,
              credentialId: live.credentialId,
              runtimeKey: live.runtimeKey,
              model: live.model,
              effort: live.effort,
              accessMode: live.accessMode,
              projectId: live.projectId,
            });
            flushComposerDraftStore();
          }
          router.push(`/chat/${encodeURIComponent(target)}`);
        }
      } catch (exc) {
        const message = formatSendFailureMessage(exc, { keepDraft: true });
        const pending = readSendIntent(draftKey)?.phase;
        const uncertain = (pending === "creating_thread" || pending === "sending")
          && ["connection", "server"].includes(classifySendFailure(exc));
        updateSendIntent(draftKey, {
          phase: uncertain ? "unknown" : "failed",
          pendingPhase: uncertain ? pending as "creating_thread" | "sending" : undefined,
          lastError: message,
        });
        // One failure banner only. Permanent validation (retryable=false) keeps
        // the draft but must not invite identical retry of a terminal receipt (#191).
        setNotice("");
        throw exc;
      }
    }, "send");
  };

  // Steer active run
  const handleSteer = async () => {
    const text = plainTextFromDocument(promptDocument).trim();
    if (!text || !threadId || busy) return;
    if (isThreadArchived(threadId)) {
      setError("已归档的对话不能发送消息，请先取消归档");
      return;
    }
    await execute(async () => {
      const expectedTurnId = conversation.view?.state.running_turn_id;
      if (!expectedTurnId) throw new Error("当前回答已经结束");
      if (!canSteer) {
        throw new Error(
          `${steerDisable.reason}${steerDisable.alternative ? `；替代：${steerDisable.alternative}` : ""}`,
        );
      }
      if (attachments.length || refsFromDocument(promptDocument).length) {
        throw new Error("含附件或上下文引用的消息会作为下一轮执行，请使用发送按钮");
      }
      await sendConversationCommand(threadId, "conversation.turn.steer", {
        text,
        expected_turn_id: expectedTurnId,
        client_message_id: `steer_${Date.now().toString(36)}`,
        capability_revision: capabilityRevision,
      });
      recordSentPrompt(composerDraftKey(threadId), {
        prompt: text,
        promptSegments: promptDocument.segments,
        capabilityRefs: refsFromDocument(promptDocument),
        projectId,
      });
      historyIndexRef.current = null;
      setHistoryIndex(null);
      setPrompt("");
      setPromptDocument(emptyPromptDocument());
      setCapabilityRefs([]);
      clearComposerDraft(draftKey);
    });
  };

  const handleCiteMessageSpan = useCallback((payload: {
    messageId: string;
    turnId?: string;
    text: string;
    startOffset: number;
    endOffset: number;
    streaming?: boolean;
  }) => {
    const node = createMessageSpanNode({
      threadId: threadId || "",
      messageId: payload.messageId,
      turnId: payload.turnId,
      startOffset: payload.startOffset,
      endOffset: payload.endOffset,
      text: payload.text,
      streaming: payload.streaming,
    });
    setPromptDocument((current) => {
      const next = insertNodeAtCaret(current, node);
      setPrompt(plainTextFromDocument(next));
      setCapabilityRefs(composerRefs(next));
      return next;
    });
    if (payload.streaming) {
      setNotice("流式中引用可能不完整");
    }
  }, [threadId]);

  const handleDiffAnnotationSend = useCallback((annotations: DiffLineAnnotation[]) => {
    // Last-line guard: never silently cite reviews whose baseline no longer matches.
    const fresh = annotations.filter((annotation) => !annotation.stale);
    if (!fresh.length) return;
    setPromptDocument((current) => {
      let next = current;
      for (const annotation of fresh) {
        const sidePrefix = annotation.side === "old" ? "[旧版本] " : annotation.side === "new" ? "[新版本] " : "";
        next = insertPlainTextAtMarkedRange(
          next,
          `${flattenPromptDocument(next).trim() ? "\n" : ""}${sidePrefix}${annotation.comment.trim()}\n`,
        );
        const node = createFileContextNode({
          id: `diff-ann:${annotation.path}:${annotation.side}:${annotation.lineNumber}`,
          name: `${annotation.path}:${annotation.lineNumber}`,
          relativePath: annotation.path,
          startLine: annotation.lineNumber,
          endLine: annotation.lineNumber,
          snapshotText: `${sidePrefix}${annotation.snapshot}`.trim(),
        });
        next = insertNodeAtCaret(next, node);
        next = insertPlainTextAtMarkedRange(next, "\n");
      }
      setPrompt(plainTextFromDocument(next));
      setCapabilityRefs(refsFromDocument(next));
      return next;
    });
    if (panelSheet && threadId) chatPanel.close(threadId);
    window.requestAnimationFrame(() => document.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
  }, [panelSheet, threadId]);

  const handleJumpToContext = useCallback((payload: {
    messageId: string;
    startOffset?: number;
    endOffset?: number;
  }) => {
    // Capture current reading point before C09 cite jump (do not rewrite cite UI).
    pushJumpBackFromThread(threadId);
    setJumpTarget(payload);
  }, [threadId]);

  const handleQueueUpdate = async (queueId: string, text: string): Promise<boolean> => {
    if (!threadId) return false;
    return execute(async () => {
      await sendConversationCommand(threadId, "conversation.queue.update", {
        queue_id: queueId,
        text,
      });
    });
  };

  const handleQueueRebind = (queueId: string) => {
    if (!threadId) return;
    void execute(async () => {
      const cred = selectedCredential;
      const rt = selectedRuntime;
      const item = conversation.view?.queue?.find((row) => row.queue_id === queueId);
      if (!item || !cred || !runtimeUsable(rt) || rt?.engine !== cred.engine
        || !allCredentialModels(cred).some((row) => row.id === model)) {
        throw new Error("请先在输入框选择可用且匹配的接入点和模型，再重新绑定失败消息");
      }
      await sendConversationCommand(threadId, "conversation.queue.update", {
        queue_id: queueId,
        text: item.text,
        runtime: {
          adapter_id: rt.adapter_id, instance_id: rt.instance_id,
          credential_id: cred.id, model, effort, access_mode: accessMode,
        },
      });
      setNotice("已按当前模型重新绑定队列消息，正文、附件和引用已保留；点击继续发送");
    });
  };

  const handleQueueDelete = (queueId: string) => {
    if (!threadId) return;
    void execute(async () => {
      await sendConversationCommand(threadId, "conversation.queue.delete", {
        queue_id: queueId,
      });
    });
  };

  const handleQueueReorder = (queueIds: string[]) => {
    if (!threadId) return;
    void execute(async () => {
      await sendConversationCommand(threadId, "conversation.queue.reorder", {
        queue_ids: queueIds,
        expected_revision: conversation.view?.state.queue_revision ?? 0,
      });
    });
  };

  const handleQueueResume = () => {
    if (!threadId) return;
    void execute(async () => {
      await sendConversationCommand(threadId, "conversation.queue.resume");
      setNotice("后续消息将继续按顺序发送");
    });
  };

  const handleQueuePause = () => {
    if (!threadId) return;
    void execute(async () => {
      await sendConversationCommand(threadId, "conversation.queue.pause", {
        reason: "user_paused",
      });
      setNotice("已暂停自动发送后续消息");
    });
  };

  const handleQueueSteer = (queueId: string) => {
    if (!threadId) return;
    void execute(async () => {
      const expectedTurnId = conversation.view?.state.running_turn_id;
      if (!expectedTurnId) throw new Error("当前回答已经结束");
      if (!canSteer) throw new Error("当前 Agent 接入不支持引导正在执行的回答");
      await sendConversationCommand(threadId, "conversation.queue.steer", {
        queue_id: queueId,
        expected_turn_id: expectedTurnId,
      });
      setNotice("已将该消息用于引导当前回答");
    });
  };

  // Stop / Interrupt turn
  const handleStop = () => {
    const targetThreadId = threadId;
    const expectedTurnId = conversation.view?.state.running_turn_id || "";
    if (
      !targetThreadId
      || !expectedTurnId
      || interactionBusy
      || conversation.view?.thread.thread_id !== targetThreadId
    ) return;
    void execute(async () => {
      await sendConversationCommand(targetThreadId, "conversation.turn.interrupt", {
        expected_turn_id: expectedTurnId,
      });
      setNotice("已发送中断指令");
    });
  };

  // Resume turn
  const handleResume = () => {
    if (!threadId) return;
    void execute(async () => {
      // The picker can change autonomy after a failed/interrupted Turn. Carry
      // that choice into the continuation so the server creates a fresh
      // Runtime session when needed, instead of replaying the old supervised
      // session and asking for each already-authorized tool again.
      await sendConversationCommand(threadId, "conversation.thread.resume", {
        runtime: { access_mode: accessMode },
      });
      setNotice("已恢复执行");
    });
  };

  const rewindDisable = disableCopy(threadMatrix, "rewind");
  const rewindDisabled = !canInvoke(threadMatrix, "rewind");
  const rewindReason = [
    rewindDisable.reason || conversation.view?.rewind_capability?.reason || "原生回退不可用",
    (rewindDisable.alternative
      || conversation.view?.rewind_capability?.alternative
      || "可改用 Fork（保留源历史）或重新执行本轮（保留文件）")
      ? `替代：${rewindDisable.alternative
        || conversation.view?.rewind_capability?.alternative
        || "可改用 Fork（保留源历史）或重新执行本轮（保留文件）"}`
      : "",
  ].filter(Boolean).join("；");

  const openImpact = async (
    mode: ImpactMode,
    turnId: string,
    options?: { text?: string; sourceThreadId?: string },
  ) => {
    const previewThreadId = mode === "fork"
      ? (options?.sourceThreadId || impactForkSource?.threadId || threadId)
      : threadId;
    if (!previewThreadId || !turnId) return;
    if (mode === "fork") {
      const frozen = freezeForkSource({
        sourceThreadId: previewThreadId,
        threads: sidebarThreads,
        projects,
        fallbackTitle: previewThreadId === threadId
          ? conversation.view?.thread.title
          : undefined,
        fallbackProjectId: previewThreadId === threadId
          ? conversation.view?.thread.project_id
          : undefined,
        fallbackRootPath: previewThreadId === threadId
          ? conversation.view?.workspace?.root_path
          : undefined,
        fallbackProjectName: previewThreadId === threadId
          ? activeProjectName
          : undefined,
      });
      setImpactForkSource(frozen);
    } else {
      setImpactForkSource(null);
    }
    setImpactLoading(true);
    try {
      const preview = await fetchImpactPreview(previewThreadId, turnId, mode, {
        text: options?.text,
      }) as ImpactPreviewResponse;
      setImpactPreview(preview as ImpactPreview);
      if (mode === "edit_resend") {
        const seed = options?.text ?? preview.target_text_preview ?? "";
        setImpactEditedText(seed);
        writeEditBuffer(previewThreadId, turnId, seed);
      } else {
        setImpactEditedText("");
      }
      // Classic retry reuses C02 retry intent id; edit/fork/rewind mint a fresh id.
      if (mode === "retry") {
        const retryIntent = ensureRetryIntent(previewThreadId, turnId);
        setImpactCommandId(retryIntent.commandId);
      } else {
        setImpactCommandId(
          `cmd_c11_${mode}_${turnId}_${Date.now().toString(36)}`,
        );
      }
      setImpactOpen(true);
    } catch (exc) {
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      setImpactLoading(false);
    }
  };

  // Retry opens C11 impact confirm first; confirm reuses C02 retry intent.
  const handleRetry = (turnId: string) => {
    void openImpact("retry", turnId);
  };

  const handleEditTurn = (turnId: string, text: string) => {
    void openImpact("edit_resend", turnId, { text });
  };

  const handleRewindTurn = (turnId: string) => {
    void openImpact("native_rewind", turnId);
  };

  // Fork thread — explicit sourceThreadId (Pane #209); never drop sidebar target.
  const handleFork = (sourceThreadId: string, sourceTurnId?: string) => {
    if (!sourceThreadId) {
      setError("无法分叉：未指定源会话");
      return;
    }
    void (async () => {
      let sourceLastTurnId = "";
      if (!sourceTurnId && sourceThreadId !== threadId) {
        try {
          const sourceView = await fetchConversationView(sourceThreadId, false, 1);
          sourceLastTurnId = sourceView.turns.at(-1)?.turn_id || "";
        } catch (exc) {
          setError(exc instanceof Error ? exc.message : String(exc));
          return;
        }
      }
      const target = resolveForkTurnId({
        sourceThreadId,
        sourceTurnId,
        currentThreadId: threadId,
        currentLastTurnId: conversation.view?.turns.at(-1)?.turn_id,
        sourceLastTurnId,
      });
      const frozen = freezeForkSource({
        sourceThreadId,
        threads: sidebarThreads,
        projects,
        fallbackTitle: sourceThreadId === threadId
          ? conversation.view?.thread.title
          : undefined,
        fallbackProjectId: sourceThreadId === threadId
          ? conversation.view?.thread.project_id
          : undefined,
        fallbackRootPath: sourceThreadId === threadId
          ? conversation.view?.workspace?.root_path
          : undefined,
        fallbackProjectName: sourceThreadId === threadId
          ? activeProjectName
          : undefined,
      });
      setImpactForkSource(frozen);
      if (!target) {
        void execute(async () => {
          const receipt = await sendConversationCommand(
            sourceThreadId,
            "conversation.thread.fork",
            { from_turn_id: "" },
          );
          const forkedId = receipt.aggregate?.id;
          if (forkedId) {
            router.push(`/chat/${encodeURIComponent(forkedId)}`);
          }
        });
        return;
      }
      void openImpact("fork", target, { sourceThreadId });
    })();
  };

  const closeImpact = () => {
    setImpactOpen(false);
    setImpactPreview(null);
    setImpactEditedText("");
    setImpactCommandId("");
    setImpactForkSource(null);
  };

  const confirmImpact = () => {
    if (!impactPreview) return;
    const mode = String(impactPreview.mode || "retry") as ImpactMode;
    const turnId = impactPreview.target_turn_id;
    const commandId = impactCommandId || undefined;
    const commandThreadId = impactCommandThreadId({
      mode,
      currentThreadId: threadId,
      frozenSourceThreadId: impactForkSource?.threadId,
    });
    if (!commandThreadId) return;
    void execute(async () => {
      if (mode === "fork") {
        const receipt = await sendConversationCommand(
          commandThreadId,
          "conversation.thread.fork",
          { from_turn_id: turnId },
          commandId ? { commandId, idempotencyKey: commandId } : {},
        );
        closeImpact();
        const forkedId = receipt.aggregate?.id;
        if (forkedId) {
          router.push(`/chat/${encodeURIComponent(forkedId)}`);
        }
        setNotice("已 Fork；源 Thread 保留，当前共享工作区");
        return;
      }
      if (mode === "native_rewind") {
        await sendConversationCommand(
          threadId,
          "conversation.turn.native_rewind",
          { turn_id: turnId, file_mode: "keep_files" },
          commandId ? { commandId, idempotencyKey: commandId } : {},
        );
        closeImpact();
        setNotice("聊天历史已回退，工作区文件保持原状");
        return;
      }
      if (mode === "edit_resend") {
        const text = impactEditedText;
        writeEditBuffer(threadId, turnId, text);
        await sendConversationCommand(
          threadId,
          "conversation.turn.edit_resend",
          { turn_id: turnId, text },
          commandId ? { commandId, idempotencyKey: commandId } : {},
        );
        clearEditBuffer(threadId, turnId);
        closeImpact();
        setNotice("已编辑重发（文件保持原状）");
        return;
      }
      const retryIntent = ensureRetryIntent(threadId, turnId);
      setNotice(phaseNotice(retryIntent.phase));
      try {
        updateRetryIntent(threadId, turnId, { phase: "sending" });
        const receipt = await sendConversationCommand(
          threadId,
          "conversation.turn.retry",
          { turn_id: turnId },
          {
            commandId: commandId || retryIntent.commandId,
            idempotencyKey: commandId || retryIntent.commandId,
          },
        );
        updateRetryIntent(threadId, turnId, {
          phase: "accepted",
          lastReceiptState: String(receipt.state || "accepted"),
          lastError: "",
        });
        clearRetryIntent(threadId, turnId);
        closeImpact();
        setNotice("已从所选轮次之前重新执行（文件保持原状）");
      } catch (exc) {
        const message = exc instanceof Error ? exc.message : String(exc);
        updateRetryIntent(threadId, turnId, {
          phase: "failed",
          lastError: message,
        });
        setNotice("");
        throw new Error(formatSendFailureMessage(exc, { keepDraft: false }));
      }
    });
  };

  const handleToggleSuperseded = () => {
    if (!threadId) return;
    if (showSuperseded) {
      setShowSuperseded(false);
      return;
    }
    void execute(async () => {
      const audit = await fetchThreadAudit(threadId);
      setAuditTurns(audit.superseded_turns || []);
      setAuditMessages(audit.superseded_messages || []);
      setShowSuperseded(true);
    });
  };

  const archiveImpactLines = (thread: ConversationThread): string[] => {
    const lines: string[] = [];
    const state = thread.state;
    if (state.running_turn_id) lines.push("正在运行的回合将被停止");
    if (state.pending_approval) lines.push("待审批请求将随归档关闭会话");
    if (state.pending_user_input) lines.push("待用户输入将无法继续提交");
    if ((state.queue_count || 0) > 0) {
      lines.push(`队列中还有 ${state.queue_count} 条消息，归档后将暂停且不会自动发送`);
    }
    return lines;
  };

  const isThreadArchived = (target: string): boolean => {
    // Prefer the open conversation view — sidebar list rows can lag after
    // archive/unarchive and would mis-route the next toggle.
    if (target === threadId && conversation.view?.state.status) {
      return conversation.view.state.status === "archived";
    }
    const listed = threads.find((thread) => thread.thread_id === target);
    if (listed) return listed.state.status === "archived";
    return false;
  };

  const threadArchived = Boolean(threadId && isThreadArchived(threadId));

  const runArchiveCommand = async (target: string, archived: boolean) => {
    const commandType = archived
      ? "conversation.thread.unarchive"
      : "conversation.thread.archive";
    // Per-attempt keys: a lifetime-stable `unarchive:${id}` would dedupe the
    // second unarchive after archive→unarchive→archive→unarchive and leave
    // status stuck on archived (Command API returns the old receipt).
    const commandKey = `${archived ? "unarchive" : "archive"}:${target}:${Date.now().toString(36)}`;
    await sendConversationCommand(target, commandType, {}, {
      commandId: commandKey,
      idempotencyKey: commandKey,
    });
  };

  const runArchiveBatch = async (targets: ConversationThread[]) => {
    if (!targets.length) return;
    const batchId = `archive-batch-${Date.now().toString(36)}`;
    let okCount = 0;
    const failures: string[] = [];
    for (const thread of targets) {
      const commandKey = `${batchId}:${thread.thread_id}`;
      const ok = await execute(async () => {
        await sendConversationCommand(
          thread.thread_id,
          "conversation.thread.archive",
          {},
          { commandId: commandKey, idempotencyKey: commandKey },
        );
      });
      if (ok) okCount += 1;
      else failures.push(thread.title || thread.thread_id);
    }
    if (failures.length) {
      setNotice(`已归档 ${okCount}/${targets.length}；失败：${failures.join("、")}`);
    } else {
      setNotice(`已归档 ${okCount}/${targets.length}`);
    }
  };

  // Archive / Unarchive thread (restore ≠ resume)
  const handleArchive = (tId?: string) => {
    const target = tId || threadId;
    if (!target) return;
    const archived = isThreadArchived(target);
    if (!archived) {
      const thread = threads.find((item) => item.thread_id === target)
        || (target === threadId && conversation.view
          ? {
              ...conversation.view.thread,
              state: conversation.view.state,
            } as ConversationThread
          : undefined);
      if (thread) {
        const impact = archiveImpactLines(thread);
        if (impact.length) {
          setArchiveImpact({ threads: [thread], mode: "single" });
          return;
        }
      }
    }
    void execute(async () => {
      await runArchiveCommand(target, archived);
      setNotice(archived ? "已取消归档（未恢复执行）" : "已归档");
    });
  };

  const handleArchiveProjectThreads = (projectThreads: ConversationThread[]) => {
    const targets = projectThreads.filter((thread) => thread.state.status !== "archived");
    if (!targets.length) {
      setNotice("该项目没有可归档的对话");
      return;
    }
    const hot = targets.filter((thread) => archiveImpactLines(thread).length > 0);
    if (hot.length) {
      setArchiveImpact({ threads: targets, mode: "batch" });
      return;
    }
    void runArchiveBatch(targets);
  };

  const confirmArchiveImpact = () => {
    const pending = archiveImpact;
    setArchiveImpact(null);
    if (!pending) return;
    if (pending.mode === "single") {
      const target = pending.threads[0]?.thread_id;
      if (!target) return;
      void execute(async () => {
        await runArchiveCommand(target, false);
        setNotice("已归档");
      });
      return;
    }
    void runArchiveBatch(pending.threads);
  };

  // Rename thread
  const handleRename = (tId: string, newTitle: string) => {
    if (!tId || !newTitle.trim()) return;
    void execute(async () => {
      await sendConversationCommand(tId, "conversation.thread.rename", {
        title: newTitle.trim(),
      });
      setRenameTarget(null);
      setNotice("标题已更新");
    });
  };

    // Approval decision — always bind to the card's approval_id (C23).
  const handleApprovalDecision = (
    approvalId: string,
    decision: "allow" | "deny",
    scopeMode?: "once" | "session",
  ) => {
    if (!threadId || !approvalId) return;

    void execute(async () => {
      await sendConversationCommand(threadId, "conversation.approval.resolve", {
        approval_id: approvalId,
        decision,
        scope: scopeMode === "session" ? "session" : "once",
      });
      setNotice(decision === "allow" ? "已允许操作" : "已拒绝操作");
    });
  };

  // User input resolve (structured answers / cancel)
  const handleUserInputResolve = (payload: {
    request_id: string;
    decision: "submit" | "cancel";
    answers?: Record<string, { values: string[]; text?: string }>;
    text?: string;
  }) => {
    const pendingId = conversation.view?.state.pending_user_input?.request_id;
    if (!threadId) return;
    if (!pendingId || pendingId !== payload.request_id) {
      setError("该输入请求已过期，请刷新后重试");
      return;
    }

    void execute(async () => {
      await sendConversationCommand(threadId, "conversation.user_input.resolve", {
        request_id: payload.request_id,
        decision: payload.decision,
        answers: payload.answers || {},
        text: payload.text || "",
      });
      setNotice(payload.decision === "cancel" ? "已取消输入" : "回答已提交");
    });
  };

  // Add memory
  const handleAddMemory = async (content: string) => {
    if (!threadId) return;
    await recordConversationMemory(threadId, content);
    await loadMemory();
    setNotice("长期记忆已保存");
  };

  // Delete memory
  const handleDeleteMemory = async (memId: string) => {
    if (!threadId) return;
    await deleteConversationMemory(threadId, memId);
    await loadMemory();
    setNotice("长期记忆已删除");
  };

  // File upload trigger
  const handleAddFiles = (files: FileList | File[]) => {
    const picked = Array.from(files);
    if (!picked.length) return;
    setAttachments((curr) => [
      ...curr,
      ...picked.map((file) => ({
        id: newAttachmentId(),
        name: file.name,
        size: file.size,
        file,
        type: file.type,
        needsReselect: false,
        uploadStatus: "pending" as const,
      })),
    ]);
  };

  const handleRetryAttachment = (index: number) => {
    const attachment = attachments[index];
    if (!attachment) return;
    const action = composerAttachmentRecoveryAction(attachment);
    // Lost File after refresh: open picker to replace. Fake "retry" cannot upload.
    if (action === "reselect") {
      reselectIndexRef.current = index;
      reselectInputRef.current?.click();
      return;
    }
    if (action !== "retry") return;
    setAttachments((prev) => prev.map((a, i) =>
      i === index
        ? { ...a, uploadStatus: "pending" as const, uploadError: undefined, sha256: undefined }
        : a,
    ));
  };

  const handleReselectFile = (e: React.ChangeEvent<HTMLInputElement>) => {
    const picked = e.target.files?.[0];
    e.target.value = "";
    const index = reselectIndexRef.current;
    reselectIndexRef.current = null;
    if (!picked || index == null) return;
    setAttachments((prev) => prev.map((a, i) =>
      i === index
        ? {
            ...a,
            id: a.id || newAttachmentId(),
            name: picked.name,
            size: picked.size,
            file: picked,
            type: picked.type,
            needsReselect: false,
            uploadStatus: "pending" as const,
            uploadError: undefined,
            sha256: undefined,
          }
        : a,
    ));
    setNotice("");
  };

  const handleLargePaste = (text: string, selection?: LargePasteSelection) => {
    setLargePasteText(text);
    setLargePasteSelection(selection ?? null);
  };

  const handleLargePasteKeepText = () => {
    if (!largePasteText) return;
    // Unified document path: insert/replace at paste-time selection, sync
    // promptDocument + prompt + citations (do not only setPrompt — #186).
    const next = keepLargePasteAsText(promptDocument, largePasteText, largePasteSelection);
    handlePromptDocumentChange(next);
    setLargePasteText(null);
    setLargePasteSelection(null);
  };

  const handleLargePasteAsAttachment = () => {
    if (!largePasteText) return;
    const blob = new Blob([largePasteText], { type: "text/plain" });
    const file = new File([blob], `粘贴内容-${Date.now()}.txt`, { type: "text/plain" });
    handleAddFiles([file]);
    setLargePasteText(null);
    setLargePasteSelection(null);
  };

  const handleFileUpload = (e: React.ChangeEvent<HTMLInputElement>) => {
    const picked = e.target.files ? Array.from(e.target.files) : [];
    e.target.value = "";
    if (picked.length) handleAddFiles(picked);
  };

  const siteDownSources = (
    isConnectionSourceFailure(credentialSource)
    && isConnectionSourceFailure(runtimeSource)
  );
  /** Send-failure strip owns the UI — hide readiness / picker 502 noise. */
  const suppressConfigFlood = errorRetryKind === "send" || siteDownSources;
  const readinessForBanner = suppressConfigFlood && errorRetryKind === "send"
    ? { ...readiness, blockers: [] as typeof readiness.blockers, guide: "hidden" as const }
    : readiness;
  const credentialsErrorText = suppressConfigFlood
    ? ""
    : (credentialSource.phase === "error" ? (credentialSource.message || "凭据加载失败") : "");
  const projectsErrorText = suppressConfigFlood
    ? ""
    : (projectSource.phase === "error" ? (projectSource.message || "项目加载失败") : "");

  const activeSurface = panel.surfaces.find((item) => item.id === panel.activeId);
  const panelMaximized = rightPanelOpen && panel.maximized && !panelSheet;
  const hasWorkspace = Boolean(conversation.view?.workspace?.root_path);
  const panelControls = conversation.view && threadId ? (
    <div ref={panelControlsRef} className="flex items-center gap-0.5" data-testid="conversation-panel-controls">
      <IconButton
        icon="panelBottom"
        label="执行日志"
        shortcut="mod+j"
        active={bottomPanelOpen}
        onClick={() => setBottomPanelOpen((open) => !open)}
      />
      <IconButton
        icon="gitCompare"
        label="变更"
        shortcut="mod+alt+d"
        active={rightPanelOpen && activeSurface?.kind === "diff"}
        disabled={!hasWorkspace && !(conversation.view.artifacts || []).length}
        onClick={() => { setDetailsPayload(null); chatPanel.toggle(threadId, "diff"); }}
      />
      <IconButton
        icon={rightPanelOpen ? "panelRightClose" : "panelRightOpen"}
        label={rightPanelOpen ? "关闭工作面板" : "打开工作面板"}
        shortcut="mod+alt+b"
        active={rightPanelOpen}
        badge={runningToolCount > 0 && !rightPanelOpen ? runningToolCount : undefined}
        onClick={() => { setDetailsPayload(null); chatPanel.toggle(threadId); }}
      />
    </div>
  ) : null;

  return (
    <>
    <div
      ref={shellRef}
      className="cx-root ai-conv-shell relative flex h-full w-full overflow-hidden"
      data-sidebar-collapsed={sidebarCollapsed ? "true" : "false"}
      data-mobile-sidebar-open={mobileSidebarOpen ? "true" : "false"}
      data-panel-open={rightPanelOpen ? "true" : "false"}
    >
      {/* Hidden file input */}
      <input
        ref={fileInputRef}
        type="file"
        multiple
        className="hidden"
        onChange={handleFileUpload}
      />
      {/* Single-file replace for attachments that lost their local File (refresh). */}
      <input
        ref={reselectInputRef}
        type="file"
        className="hidden"
        data-testid="composer-reselect-file"
        onChange={handleReselectFile}
      />

      {/* Left Sidebar */}
      <ConversationSidebar
        threads={sidebarThreads}
        projects={projects}
        activeThreadId={threadId}
        attentionCount={attentionCount}
        onSelectThread={(id) => {
          setMobileSidebarOpen(false);
          router.push(`/chat/${encodeURIComponent(id)}`);
        }}
        onNewChat={() => {
          startNewChat();
        }}
        onNewChatForProject={(nextProjectId) => {
          startNewChat(nextProjectId);
        }}
        onCreateFolder={() => void handleCreateProjectFromDirectory()}
        searchQuery={searchQuery}
        onSearchChange={setSearchQuery}
        onRenameThread={(t) => {
          setRenameTarget(t);
          setRenameInput(t.title);
        }}
        onForkThread={(t) => handleFork(t.thread_id, t.state.running_turn_id || undefined)}
        onArchiveThread={(t) => handleArchive(t.thread_id)}
        onArchiveProjectThreads={handleArchiveProjectThreads}
        onSelectMessageHit={(nextThreadId, messageId) => {
          // Push jump-back before C07 deep-link navigation; leave search/href intact.
          pushJumpBackFromThread(threadId);
          setMobileSidebarOpen(false);
          router.push(buildThreadMessageHref(nextThreadId, messageId));
        }}
        collapsed={sidebarCollapsed}
        width={sidebarWidth}
        onWidthChange={setSidebarWidth}
        className={mobileSidebarOpen ? "open" : ""}
      />

      {mobileSidebarOpen ? (
        <>
          <button
            type="button"
            className="conversation-mobile-sidebar-backdrop"
            aria-label="关闭对话导航"
            tabIndex={-1}
            onClick={() => setMobileSidebarOpen(false)}
          />
        </>
      ) : null}

      {/* Central Chat Area — inert while mobile sidebar overlay is open (#199) */}
      <main
        ref={mainRef}
        id={CONVERSATION_MAIN_CONTENT_ID}
        tabIndex={-1}
        className="ai-conv-main relative flex min-w-0 flex-1 flex-col overflow-hidden bg-cx-bg"
        aria-label="对话主内容"
        inert={mobileSidebarOpen || panelMaximized || undefined}
        aria-hidden={mobileSidebarOpen || panelMaximized || undefined}
      >
        {conversation.view ? (
          <>
            <ConversationHeader
              view={conversation.view}
              projectName={activeProjectName}
              panelControls={panelControls}
              onOpenInfo={() => setInfoDrawerOpen(true)}
              onOpenReadingPrefs={() => setReadingPrefsOpen((v) => !v)}
              readingPrefsOpen={readingPrefsOpen}
              onOpenShortcuts={() => setShortcutsHelpOpen(true)}
              onExport={() => setExportDialogOpen(true)}
              onFork={() => handleFork(threadId)}
              onRename={(newT) => handleRename(threadId, newT)}
              onArchive={() => handleArchive(threadId)}
              busy={interactionBusy}
            />
            <Popover
              open={readingPrefsOpen}
              onOpenChange={setReadingPrefsOpen}
              anchorRef={panelControlsRef}
              placement="bottom-end"
              ariaLabel="阅读偏好"
              className="w-[320px]"
            >
              <div className="conversation-reading-prefs-popover p-4" data-testid="c39-reading-prefs-popover">
                <ConversationReadingPrefsPanel compact />
              </div>
            </Popover>
          </>
        ) : null}

        {/* Shell owns command errors; Timeline owns runtime failures.
            Success notices auto-dismiss (pausable); progress/sticky stay until handled. */}
        {error || notice ? (
          <div className={cn(
            "flex justify-center px-4",
            error
              ? "shrink-0 py-2"
              : "pointer-events-none absolute inset-x-0 top-[calc(var(--cx-header-h)+8px)] z-30",
          )}>
            {error ? (
              <Callout
                role="alert"
                testId="send-error-banner"
                tone="danger"
                className="cx-animate-in pointer-events-auto w-full max-w-[640px] shadow-cx-md"
                action={(
                  <>
                    {errorRetryKind === "send" ? (
                      <Button
                        size="xs"
                        variant="danger-soft"
                        aria-label="重试发送"
                        data-testid="send-error-retry"
                        disabled={interactionBusy}
                        onClick={() => {
                          void (async () => {
                            await reloadReadiness({ fresh: true });
                            await handleSend();
                          })();
                        }}
                      >
                        重试发送
                      </Button>
                    ) : null}
                    <IconButton
                      size="xs"
                      icon="x"
                      label="关闭错误提示"
                      noTooltip
                      data-testid="send-error-dismiss"
                      onClick={() => setError("")}
                    />
                  </>
                )}
              >
                <span className="break-words">{error}</span>
                {/credential|auth|token|凭据|认证/i.test(error) ? (
                  <a
                    href="/settings/agents"
                    className="mt-1 flex w-fit items-center gap-1.5 text-cx-fg-2 hover:text-cx-fg hover:underline"
                  >
                    <Icon name="plug" size={13} />
                    检查 Agent 接入
                  </a>
                ) : null}
              </Callout>
            ) : (
              <div
                className="pointer-events-auto w-full max-w-[640px]"
                data-notice-kind={noticeControls.kind}
                data-notice-autodismiss={noticeControls.kind === "success" ? "true" : "false"}
                onMouseEnter={noticeControls.kind === "success" ? noticeControls.pauseAutoDismiss : undefined}
                onMouseLeave={noticeControls.kind === "success" ? noticeControls.resumeAutoDismiss : undefined}
                onFocusCapture={noticeControls.kind === "success" ? noticeControls.pauseAutoDismiss : undefined}
                onBlurCapture={noticeControls.kind === "success" ? (event) => {
                  if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
                    noticeControls.resumeAutoDismiss();
                  }
                } : undefined}
              >
                <Callout
                  role="status"
                  testId="send-phase-notice"
                  tone={notice.includes("失败") ? "danger" : noticeControls.kind === "progress" ? "running" : "success"}
                  className="cx-animate-in w-full shadow-cx-md"
                  action={(
                    <IconButton
                      size="xs"
                      icon="x"
                      label="关闭提示"
                      noTooltip
                      data-testid="send-phase-dismiss"
                      onClick={() => setNotice("")}
                    />
                  )}
                >
                  <span data-send-phase={notice.includes("已受理") ? "accepted" : notice.includes("失败") ? "failed" : "info"}>{notice}</span>
                </Callout>
              </div>
            )}
          </div>
        ) : null}

        {/* Main Content Area: Landing Home / first-load error / Message Timeline */}
        {!threadId ? (
          <ConversationHome
            prompt={prompt}
            onPromptChange={handlePromptChange}
            promptDocument={promptDocument}
            onPromptDocumentChange={handlePromptDocumentChange}
            onSubmit={handleSend}
            busy={busy}
            credentials={credentials}
            selectedCredentialId={credentialId}
            selectedModel={model}
            selectedEffort={effort}
            selectedAccessMode={accessMode}
            accessModes={accessModes}
            onSelectModelParams={handleSelectModelParams}
            projects={projects}
            selectedProjectId={projectId}
            onProjectChange={handleProjectChange}
            onCreateProject={handleCreateProjectFromDirectory}
            onCreateFromPath={handleCreateProjectFromPath}
            creatingProject={projectCreating}
            credentialsLoading={credentialSource.phase === "loading"}
            credentialsError={credentialsErrorText}
            onRetryCredentials={() => void reloadReadiness({ fresh: true })}
            projectsLoading={projectSource.phase === "loading"}
            projectsError={projectsErrorText}
            onRetryProjects={() => void loadProjects()}
            preferPathInput={preferPathInput}
            requestPathInput={requestPathInput}
            recentPaths={recentPaths}
            readiness={readinessForBanner}
            probing={probing}
            onRetryReadiness={() => void reloadReadiness({ fresh: true })}
            onProbeAgent={() => void handleProbeAgent()}
            onDismissGuide={handleDismissGuide}
            onRequestPathInput={() => {
              setPreferPathInput(true);
              setRequestPathInput(true);
            }}
            workspaceMode={workspaceMode}
            onWorkspaceModeChange={handleWorkspaceModeChange}
            worktreeBranch={worktreeBranch}
            onWorktreeBranchChange={setWorktreeBranch}
            existingWorktreePath={existingWorktreePath}
            onExistingWorktreePathChange={setExistingWorktreePath}
            attachments={attachments}
            onAddAttachment={() => fileInputRef.current?.click()}
            onAddFiles={handleAddFiles}
            onRemoveAttachment={(i) => setAttachments((curr) => curr.filter((_, idx) => idx !== i))}
            onRetryAttachment={handleRetryAttachment}
            onLargePaste={handleLargePaste}
            capabilityContext={composerCapabilityContext}
            capabilityRefs={capabilityRefs}
            onCapabilityRefsChange={setCapabilityRefs}
            onComposerCommand={(action) => {
              if (action === "new") startNewChat(projectId, true);
              else if (action.startsWith("ui:")) void handleClientCommand(action);
            }}
            projectHasDefault={Boolean(
              projectId && (projectConvDefaults.credentialId || projectConvDefaults.modelId),
            )}
            savingProjectDefault={savingProjectDefault}
            onSetProjectDefault={() => void handleSetProjectDefault()}
            onClearProjectDefault={() => void handleClearProjectDefault()}
            modelPickerOpen={modelPickerOpen}
            onModelPickerOpenChange={setModelPickerOpen}
            {...stashRecallProps}
          />
        ) : firstThreadLoadFailed ? (
          <div
            className="mx-auto flex w-full max-w-md flex-1 flex-col items-center justify-center gap-4 px-6 py-16 text-center"
            role="alert"
            data-testid="thread-load-error"
            data-thread-id={threadId}
          >
            <span className="grid size-11 place-items-center rounded-2xl bg-cx-danger-soft text-cx-danger">
              <Icon name="circleAlert" size={22} />
            </span>
            <div className="flex flex-col gap-2">
              <h2 className="text-[15px] font-semibold text-cx-fg">无法打开对话</h2>
              <p className="text-[12.5px] leading-relaxed text-cx-fg-2">
                {conversation.error || "加载对话失败，请重试。"}
              </p>
              <p className="font-mono text-[11px] text-cx-fg-4 break-all">{threadId}</p>
            </div>
            <div className="flex flex-wrap items-center justify-center gap-2">
              <Button
                type="button"
                size="sm"
                variant="primary"
                data-testid="thread-load-error-retry"
                disabled={conversation.loading}
                onClick={() => void conversation.refresh()}
              >
                重试
              </Button>
              <Button
                type="button"
                size="sm"
                variant="outline"
                data-testid="thread-load-error-back"
                onClick={() => startNewChat()}
              >
                返回新对话
              </Button>
            </div>
          </div>
        ) : conversation.loading && !conversation.view ? (
          <div className="flex flex-1 items-center justify-center text-cx-fg-3">
            <LoadingState label="正在载入对话…" variant="Orbit" />
          </div>
        ) : conversation.view ? (
          <>
          {sessionSyncError ? (
            <div
              role="alert"
              data-testid="thread-session-error-banner"
              className="flex items-center justify-between gap-3 border-b border-cx-danger/30 bg-cx-danger-soft px-4 py-2 text-[12.5px] text-cx-danger animate-fade-in"
            >
              <span className="min-w-0">{sessionSyncError}</span>
              <div className="flex shrink-0 items-center gap-1">
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  aria-label="重试加载"
                  data-testid="thread-session-error-retry"
                  disabled={conversation.loading || conversation.loadingOlder}
                  onClick={() => void conversation.refresh()}
                  className="font-bold hover:underline"
                >
                  重试
                </Button>
              </div>
            </div>
          ) : null}
          <ConversationTimeline
            className="min-h-0 flex-1"
            view={conversation.view}
            credentials={credentials}
            events={conversation.events}
            liveText={conversation.liveText}
            running={running}
            connected={conversation.connected}
            streamStatus={conversation.streamStatus}
            busy={interactionBusy}
            contentLoading={switchingThread && !threadLoadError}
            contentError={threadLoadError}
            onReloadContent={() => void conversation.refresh()}
            onLoadOlder={conversation.loadOlderMessages}
            onJumpToLatest={async () => {
              // Disarm the deep-link effect immediately. history.replaceState
              // does not update Next.js useSearchParams, so a local dismiss
              // plus router.replace are required before tip replace.
              // Only dismiss when a C07 `?message=` is present — an ordinary
              // 回到最新 must not flip suppressRestore and unpin the live edge.
              if (rawMessageParam) setDeepLinkDismissed(true);
              setHighlightedMessageId("");
              const params = new URLSearchParams(searchParams.toString());
              params.delete("message");
              const qs = params.toString();
              router.replace(qs ? `${pathname}?${qs}` : pathname, { scroll: false });
              return conversation.jumpToLatestMessages();
            }}
            loadingOlder={conversation.loadingOlder}
            onOpenDrawer={openDetails}
            onOpenDiffBaseline={openDiffBaseline}
            onOpenAttachment={openAttachmentPreview}
            onResourceLink={handleResourceLink}
            onApprovalDecision={handleApprovalDecision}
            onUserInputResolve={handleUserInputResolve}
            threadId={threadId || ""}
            onRetryTurn={handleRetry}
            onEditTurn={handleEditTurn}
            onRewindTurn={handleRewindTurn}
            rewindDisabled={rewindDisabled}
            rewindReason={rewindReason}
            showSuperseded={showSuperseded}
            onToggleSuperseded={handleToggleSuperseded}
            supersededTurns={auditTurns}
            supersededMessages={auditMessages}
            onContinueTurn={handleResume}
            onForkTurn={(tId) => handleFork(threadId, tId)}
            onCiteMessageSpan={handleCiteMessageSpan}
            jumpTarget={jumpTarget}
            onJumpTargetConsumed={() => setJumpTarget(null)}
            onJumpToContext={handleJumpToContext}
            onOpenPlan={() => {
              setDetailsPayload(null);
              chatPanel.open(threadId, "plan");
            }}
            highlightedMessageId={highlightedMessageId}
            suppressRestore={!shouldRestoreReadingPosition(rawMessageParam) || deepLinkDismissed}
            onEnsureMessageVisible={ensureMessageVisible}
            onOpenThread={(nextThreadId) => {
              router.push(`/chat/${encodeURIComponent(nextThreadId)}`);
            }}
            footer={
              <div className="w-full">
                {readinessForBanner.blockers.length ? (
                  <ConversationReadinessBanner
                    readiness={readinessForBanner}
                    probing={probing}
                    onRetry={() => void reloadReadiness({ fresh: true })}
                    onProbe={() => void handleProbeAgent()}
                    onDismissGuide={handleDismissGuide}
                  />
                ) : null}
                <ConversationQueue
                  items={conversation.view.queue || []}
                  paused={conversation.view.state.queue_paused}
                  pauseReason={conversation.view.state.queue_pause_reason}
                  busy={interactionBusy}
                  canSteer={canSteer}
                  runningTurnId={conversation.view.state.running_turn_id}
                  onUpdate={handleQueueUpdate}
                  onRebind={handleQueueRebind}
                  onDelete={handleQueueDelete}
                  onReorder={handleQueueReorder}
                  onPause={handleQueuePause}
                  onResume={handleQueueResume}
                  onSteer={handleQueueSteer}
                  onJumpToContext={handleJumpToContext}
                />
                {threadArchived ? (
                  <Callout
                    role="status"
                    tone="warning"
                    icon="archive"
                    testId="archived-composer-banner"
                    className="mb-2"
                    action={(
                      <Button
                        size="xs"
                        variant="secondary"
                        onClick={() => handleArchive(threadId)}
                        data-testid="archived-composer-unarchive"
                      >
                        取消归档
                      </Button>
                    )}
                  >
                    对话已归档，发送已禁用；草稿仍保留。
                  </Callout>
                ) : null}
                <PromptBar
                  value={prompt}
                  onChange={handlePromptChange}
                  promptDocument={promptDocument}
                  onPromptDocumentChange={handlePromptDocumentChange}
                  onSubmit={handleSend}
                  onSteer={handleSteer}
                  onStop={handleStop}
                  running={running}
                  canSteer={canSteer && !threadArchived}
                  steerDisabledReason={threadArchived ? "对话已归档" : steerDisable.reason}
                  steerAlternative={steerDisable.alternative}
                  capabilityBanner={providerSwitchNotice}
                  onDismissCapabilityBanner={() => {
                    setDismissedProviderPreview(providerPreviewKey);
                    setHandoffNotice("");
                  }}
                  busy={interactionBusy}
                  submitDisabled={threadArchived}
                  placeholder={
                    threadArchived
                      ? "已归档 — 取消归档后可继续发送；草稿已保留"
                      : running
                        ? "输入消息，按 Enter 加入后续队列…"
                        : "输入消息，按 Enter 发送…"
                  }
                  attachments={attachments}
                  onAddAttachment={() => fileInputRef.current?.click()}
                  onAddFiles={handleAddFiles}
                  onRemoveAttachment={(i) =>
                    setAttachments((curr) => curr.filter((_, idx) => idx !== i))
                  }
                  onRetryAttachment={handleRetryAttachment}
                  onLargePaste={handleLargePaste}
                  capabilityContext={composerCapabilityContext}
                  capabilityRefs={capabilityRefs}
                  onCapabilityRefsChange={setCapabilityRefs}
                  onComposerCommand={(action) => {
                    if (action === "new") startNewChat(projectId, true);
                    else if (action.startsWith("ui:")) void handleClientCommand(action);
                  }}
                  {...stashRecallProps}
                  extraControls={
                    <div className="flex min-w-0 flex-1 items-center gap-1">
                      <ConversationModelPicker
                        credentials={credentials}
                        selectedCredentialId={credentialId}
                        selectedModel={model}
                        selectedEffort={effort}
                        selectedAccessMode={accessMode}
                        accessModes={accessModes}
                        loading={credentialSource.phase === "loading"}
                        error={credentialsErrorText}
                        onRetry={() => void reloadReadiness({ fresh: true })}
                        onSelect={handleSelectModelParams}
                        open={modelPickerOpen}
                        onOpenChange={setModelPickerOpen}
                      />
                      {/* C36: speech-to-text mic button */}
                      {speech.state !== "unsupported" && (
                        <IconButton
                          icon={speech.state === "listening" ? "pause" : "mic"}
                          label={speech.state === "listening" ? "取消录音" : "语音输入"}
                          tooltip={
                            speech.state === "error" && speech.errorMessage
                              ? speech.errorMessage
                              : speech.state === "listening"
                                ? "取消录音（不清空草稿）"
                                : "语音转文字：结果插入光标处，不会自动发送"
                          }
                          onClick={() => speech.state === "listening" ? speech.cancel() : speech.start()}
                          className={cn(
                            "ml-auto",
                            speech.state === "listening" && "bg-cx-danger-soft text-cx-danger hover:text-cx-danger animate-pulse",
                            speech.state === "error" && "text-cx-warning hover:text-cx-warning",
                          )}
                          data-testid="composer-mic-button"
                        />
                      )}
                    </div>
                  }
                />
                {speech.state === "error" && speech.errorMessage ? (
                  <p role="alert" className="mt-2 text-xs text-cx-warning" data-testid="composer-speech-error">
                    {speech.errorMessage}
                  </p>
                ) : null}
                <ComposerContextStrip
                  projects={projects}
                  selectedProjectId={conversation.view.thread.project_id || ""}
                  onProjectChange={handleProjectChange}
                  projectsLoading={projectSource.phase === "loading"}
                  projectsError={projectsErrorText}
                  onRetryProjects={() => void loadProjects()}
                  workspaceRootPath={conversation.view.workspace?.root_path || ""}
                  threadId={threadId}
                  workspaceMode={
                    (String(conversation.view.workspace?.settings?.mode || "") as WorkspaceBindMode)
                    || workspaceMode
                  }
                  projectDisabled={true}
                  modeDisabled={true}
                  trailing={(
                    <>
                      <ConversationStatsBar
                        variant="inline"
                        statistics={conversation.view.statistics}
                        contextWindow={conversation.view.context_window}
                      />
                      <ContextWindowMeter
                        variant="ring"
                        threadId={threadId}
                        contextWindow={conversation.view.context_window}
                        compactionSupported={Boolean(conversation.view.runtime_connection?.capabilities?.compaction)}
                        onRefresh={() => void conversation.refresh()}
                      />
                    </>
                  )}
                />
                {!hasWorkspace ? (
                  <Callout
                    tone="warning"
                    className="mt-2"
                    testId="unbound-workspace-recovery"
                    role="status"
                    action={(
                      <Button
                        size="xs"
                        variant="secondary"
                        data-testid="unbound-workspace-new-chat"
                        onClick={() => startNewChat()}
                      >
                        新建会话
                      </Button>
                    )}
                  >
                    当前会话未绑定工作目录，文件/终端/变更不可用。运行中的会话不能补绑目录，请新建会话并先选择目录。
                  </Callout>
                ) : null}
              </div>
            }
          />
          {bottomPanelPresence.present ? (
            <ConversationBottomPanel
              events={conversation.events}
              onClose={() => setBottomPanelOpen(false)}
              onOpenDetails={openDetails}
              transitionPhase={bottomPanelPresence.phase}
            />
          ) : null}
          </>
        ) : null}
      </main>

      {conversation.view && threadId ? (
        <RightPanel
          threadId={threadId}
          view={conversation.view}
          events={conversation.events}
          tools={panelTools}
          hasWorkspace={hasWorkspace}
          sheet={panelSheet}
          sidebarWidth={mainLeft}
          onOpenDetails={openDetails}
          onCiteToComposer={(excerpt) => {
            setPrompt((current) => (current.trim() ? `${current.trimEnd()}\n\n${excerpt}` : excerpt));
            setPromptDocument((current) => {
              const currentText = flattenPromptDocument(current).trimEnd();
              const next = currentText ? `${currentText}\n\n${excerpt}` : excerpt;
              return documentFromPromptAndRefs(next, refsFromDocument(current));
            });
            toast({ title: "已引用到输入框", tone: "success", duration: 1800 });
          }}
          onDiffAnnotationSend={handleDiffAnnotationSend}
        />
      ) : null}

      {/* Right Details Drawer (on-demand) */}
      <ConversationDetailsDrawer
        payload={detailsPayload}
        onClose={() => setDetailsPayload(null)}
        threadId={threadId}
      />

      {/* Top Info Drawer (on-demand) */}
      <ConversationInfoDrawer
        open={infoDrawerOpen}
        onClose={() => setInfoDrawerOpen(false)}
        view={conversation.view}
        memory={memory}
        memoryLoading={memoryLoading}
        onAddMemory={handleAddMemory}
        onDeleteMemory={handleDeleteMemory}
      />

      {/* C32: Export Dialog */}
      {exportDialogOpen && conversation.view ? (
        <ExportDialog
          open={exportDialogOpen}
          onClose={() => setExportDialogOpen(false)}
          view={conversation.view}
        />
      ) : null}

      <Dialog
        open={Boolean(renameTarget)}
        onOpenChange={(open) => { if (!open) setRenameTarget(null); }}
        title="重命名对话"
        size="sm"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setRenameTarget(null)}>取消</Button>
            <Button
              variant="primary"
              disabled={!renameInput.trim()}
              onClick={() => { if (renameTarget && renameInput.trim()) handleRename(renameTarget.thread_id, renameInput.trim()); }}
            >
              保存
            </Button>
          </>
        )}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (renameTarget && renameInput.trim()) handleRename(renameTarget.thread_id, renameInput.trim());
          }}
        >
          <TextField
            label="对话标题"
            value={renameInput}
            onChange={(event) => setRenameInput(event.target.value)}
            onFocus={(event) => event.currentTarget.select()}
            data-autofocus
          />
        </form>
      </Dialog>

      <Dialog open={commandSessionsOpen} onOpenChange={setCommandSessionsOpen} title="切换聊天" description="仅列出 Muteki 中的聊天，原有聊天记录保持不变。">
        <TextField label="搜索聊天" value={searchQuery} onChange={(event) => setSearchQuery(event.target.value)} />
        <div className="mt-3 max-h-80 space-y-1 overflow-auto">
          {threads.filter((thread) => thread.title.toLowerCase().includes(searchQuery.toLowerCase())).map((thread) => <button key={thread.thread_id} type="button" className="block w-full rounded-lg px-3 py-2 text-left text-sm hover:bg-cx-hover" onClick={() => { setCommandSessionsOpen(false); router.push(`/chat/${encodeURIComponent(thread.thread_id)}`); }}>{thread.title || "未命名聊天"}</button>)}
        </div>
      </Dialog>

      <ImpactConfirmModal
        threadId={threadId}
        open={impactOpen}
        preview={impactPreview}
        loading={impactLoading || busy}
        editedText={impactEditedText}
        onEditedTextChange={setImpactEditedText}
        rewindDisabled={rewindDisabled}
        rewindReason={rewindReason}
        forkSource={impactForkSource}
        onConfirm={confirmImpact}
        onCancel={closeImpact}
        onSwitchMode={(mode) => {
          if (!impactPreview?.target_turn_id) return;
          void openImpact(mode, impactPreview.target_turn_id, {
            text: mode === "edit_resend" ? impactEditedText : undefined,
            sourceThreadId: mode === "fork"
              ? (impactForkSource?.threadId || threadId || undefined)
              : undefined,
          });
        }}
      />

      {/* Archive impact disclosure (C31) */}
      <Dialog
        open={Boolean(archiveImpact)}
        onOpenChange={(open) => { if (!open) setArchiveImpact(null); }}
        title={archiveImpact?.mode === "batch" ? "确认归档项目对话" : "确认归档对话"}
        description="以下对话仍有运行中、待审批/待输入或排队的消息。归档会停止会话并暂停队列，不会删除消息或 worktree。"
        icon="archive"
        tone="warning"
        size="md"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setArchiveImpact(null)}>取消</Button>
            <Button variant="primary" onClick={confirmArchiveImpact}>确认归档</Button>
          </>
        )}
      >
        <ul className="flex flex-col gap-2">
          {(archiveImpact?.threads || []).map((thread) => {
            const lines = archiveImpactLines(thread);
            return (
              <li key={thread.thread_id} className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2.5">
                <p className="truncate text-[13px] font-medium text-cx-fg">{thread.title || thread.thread_id}</p>
                {lines.length ? (
                  <ul className="mt-1 flex flex-col gap-0.5">
                    {lines.map((line) => (
                      <li key={line} className="flex items-center gap-2 text-[12.5px] text-cx-fg-3">
                        <span className="size-1 shrink-0 rounded-full bg-cx-warning" />
                        {line}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="mt-0.5 text-[12.5px] text-cx-fg-3">空闲，可安全归档</p>
                )}
              </li>
            );
          })}
        </ul>
      </Dialog>

      <Dialog
        open={largePasteText !== null}
        onOpenChange={(open) => { if (!open) { setLargePasteText(null); setLargePasteSelection(null); } }}
        title="粘贴内容较长"
        description={`共 ${largePasteText?.length ?? 0} 个字符，超过 2000 字上限。请选择处理方式：`}
        icon="paperclip"
        tone="accent"
        size="md"
        footer={(
          <>
            <Button variant="ghost" onClick={() => { setLargePasteText(null); setLargePasteSelection(null); }}>取消</Button>
            <Button variant="secondary" onClick={handleLargePasteKeepText}>保留为文本（前 5000 字）</Button>
            <Button variant="primary" onClick={handleLargePasteAsAttachment}>转为附件</Button>
          </>
        )}
      >
        <div className="grid gap-2">
          <div className="rounded-xl border border-cx-border-subtle px-3 py-2.5">
            <p className="text-[13px] font-medium text-cx-fg">保留为文本</p>
            <p className="text-[12.5px] text-cx-fg-3">截取前 5000 个字符插入输入框。</p>
          </div>
          <div className="rounded-xl border border-cx-accent-line bg-cx-accent-soft px-3 py-2.5">
            <p className="text-[13px] font-medium text-cx-fg">转为附件（推荐）</p>
            <p className="text-[12.5px] text-cx-fg-3">完整内容保存为 .txt 附件，原文不截断。</p>
          </div>
        </div>
      </Dialog>

      <ComposerStashModal
        open={stashOpen}
        name={stashName}
        onNameChange={setStashName}
        stashes={stashes}
        canSave={!composerRecallBodyEmpty({
          prompt,
          promptSegments: promptDocument.segments,
          capabilityRefs,
          attachments,
        })}
        onClose={() => setStashOpen(false)}
        onSave={handleStashSave}
        onRestore={handleStashRestore}
        onDelete={handleStashDelete}
      />

    </div>

    <ConversationShortcutsHelp
      open={shortcutsHelpOpen}
      onClose={() => {
        setShortcutsHelpOpen(false);
      }}
    />
    <Toaster />
    </>
  );
}
