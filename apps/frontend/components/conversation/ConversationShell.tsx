"use client";

import { classifyCommandReceipt } from "@/lib/commandReceiptState";
import { flushConversationPersistence } from "@/lib/conversationPersistence";


import { AgentPageContext, useAgentPage } from "./AgentPageContext";
import { ConversationRouteLink } from "@/components/conversation/ConversationNavigation";
import { reportVisibleThread } from "@/lib/desktopShell";
import React, { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { createPortal } from "react-dom";
import { Button, Callout, Checkbox, Dialog, IconButton, Popover, TextArea, TextField, Toaster, toast, ResizeHandle } from "@muteki/ui";
import {
  PANEL_SHEET_BREAKPOINT,
  chatPanel,
  useChatPanel,
} from "@/lib/chatPanelStore";
import { countPanes, paneLayoutStore, usePaneLayout } from "@/lib/paneLayoutStore";
import {
  ConversationPaneWorkspace,
  PaneLayoutMenu,
  type PaneChildContext,
} from "./ConversationPaneWorkspace";
import { cn } from "@/lib/cn";
import {
  allCredentialModels,
  createConversationProject,
  deleteConversationProject,
  createConversationThread,
  prewarmConversationRuntime,
  recoverCommandReceipt,
  fetchCommandReceipt,
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
  useConversationStreamSelector,
  useConversationThreads,
  type ConversationCredential,
  type ConversationProject,
  type RuntimeInstance,
  type ConversationMemorySnapshot,
  type ConversationThread,
  type ConversationTurn,
  type ConversationMessage,
  type ConversationView,
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
import { useChatPreferences, sendKeyHint, type RunningSendDefault } from "@/lib/chatPreferences";
import { projectDefaultPersistFeedback } from "@/lib/projectDefaultPersistFeedback";
import { resolveComposerRuntimeBind } from "@/lib/conversationComposerBind";
import { isInternalCtfThread, upsertConversationThread } from "@/lib/conversationInbox";
import { resolveComposerBindingRestore } from "@/lib/composerRestoreBinding";
import { providerNoticeScope, providerSwitchPreview, runtimeHandoffNoticeUpdate } from "@/lib/providerSwitchNotice";
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
  runtimeAvailableForSelection,
  RECENT_PATHS_STORAGE_KEY,
  sourceError,
  sourceLoading,
  sourceOk,
  type SourceState,
} from "@/lib/conversationReadiness";
import {
  createFileContextNode,
  addStripNode,
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
import { ScheduledTasksPill } from "./ScheduledTasksPill";
import { ComposerLaterDialog } from "./ComposerLaterDialog";
import { ComposerGoalStrip } from "./ComposerGoalStrip";
import { ComposerAsyncUserInput } from "./ComposerAsyncUserInput";
import { formatScheduledTime, laterErrorMessage, parseLaterCommand } from "@/lib/composerLater";
import { scaleImageForUpload } from "@/lib/imageScale";
import {
  keepLargePasteAsText,
  type LargePasteSelection,
} from "@/lib/composerLargePasteKeep";
import type { DiffLineAnnotation } from "@/lib/conversationDiff";
import {
  clearComposerDraft,
  latestNewComposerDraftId,
  composerDraftKey,
  flushComposerDraftStore,
  restoreComposerDraftAttachments,
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
  ConversationCommandError,
  clearRetryIntent,
  clearSendIntent,
  ensureRetryIntent,
  readRetryIntent,
  prepareSendIntent,
  classifySendFailure,
  formatSendFailureMessage,
  markSendUploadRef,
  phaseNotice,
  readSendIntent,
  shouldOfferSendRetry,
  updateRetryIntent,
  transitionSendIntent,
} from "@/lib/sendIntentStore";
import {
  unlockNotificationAudio,
} from "@/lib/threadNotifications";
import {
  useThreadScopedNotice,
  useThreadScopedError,
  useThreadScopedString,
  shouldScheduleNoticeAutoClear,
} from "@/lib/threadTransientUi";
import {
  clearEditBuffer,
  readEditBuffer,
  writeEditBuffer,
} from "@/lib/editResendBuffer";
import type { ComposerCustomDrop, ComposerLocalMentions, PromptBarAttachment } from "@/components/chat/composer/MutekiPromptBar";
import {
  PanelExtensionContext,
  togglePanelWithExtensions,
  type PanelExtensionConfig,
} from "@/components/chat/panel/extensions";

import { SIDE_CHAT_KEY, createSideChat, discardSideChat, queueSideChatQuote, requestSideChatFocus, sendSideChatQuestion, sideChatIdOf, sideChatParentStatus, sideChatSurface, takeSideChatFocus, useSideChatQuotes, visibleSideChatId, type SideChatParentStatus, type SideChatQuote } from "./ConversationSideChat";
import type { RunSideChatTarget } from "@/components/run-side/runSideChatStore";
import { ConversationSidebar, type ConversationSidebarLabels } from "./ConversationSidebar";
import { ConversationHeader } from "./ConversationHeader";
import { ConversationThreadDetails, useThreadDetailsState } from "./ConversationThreadDetails";
import { ConversationHome, type ConversationHomeProps, type ConversationHomeSuggestion } from "./ConversationHome";
import {
  ConversationStreamBottomPanel,
  ConversationStreamQuotaResume,
  ConversationStreamRightPanel,
  ConversationStreamTimeline,
  useConversationRunningToolCount,
} from "./ConversationStreamBindings";
import { ConversationQueue } from "./ConversationQueue";
import { SubagentActivityPill } from "./ConversationSubagentGroup";
import { AgentActivityDock } from "./AgentActivityDock";
import { threadSubagentCounts } from "@/lib/threadAttention";
import { ConversationDetailsDrawer, type DrawerDetailPayload } from "./ConversationDetailsDrawer";
import { ConversationInfoDrawer } from "./ConversationInfoDrawer";
import { ConversationSearchDialog } from "./ConversationSearchDialog";
import { ExportDialog } from "./ExportDialog";
import {
  ImpactConfirmModal,
  type ImpactFileMode,
  type ImpactMode,
  type ImpactPreview,
  type RewindScopeMode,
} from "./ImpactConfirmModal";
import { useConversationChrome } from "@/components/conversationChrome";
import { onPaneResizeEnd, paneResizing } from "@/lib/paneResize";
import { ConversationNavigationContext, type ConversationNavigation } from "./ConversationNavigation";
import { ConversationLocalPreviewContext } from "@/lib/conversationPreviewMode";
export type { ConversationNavigation } from "./ConversationNavigation";
import { ConversationModelPicker, type ProjectDefaultControl } from "./ConversationModelPicker";
import { ConversationComposerModes, type ConversationComposerModesHandle } from "./ConversationComposerModes";
import { credentialForRuntime, effectiveServiceTier, modelThinkingControls, readPreferredServiceTier, rememberedModelEffort, rememberModelEffort, validModelEffort } from "@/lib/modelReasoning";
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
import type { DiffBaselineRequest } from "./ConversationDiffSurface";
import { PromptBar } from "@/components/chat/composer/MutekiPromptBar";
import { LoadingState } from "@muteki/ui/agent";
import { Icon, type IconName } from "@muteki/ui";
import { useTransitionState } from "@/lib/useTransitionState";
import { runInstantTransition } from "@/lib/instantTransition";
import { CONVERSATION_MAIN_CONTENT_ID } from "@/lib/workspaceSkipTarget";
import {
  listMobileSidebarFocusables,
  mobileSidebarTabRoot,
  nextFocusableIndex,
} from "@/lib/mobileSidebarFocus";
import { ConversationShortcutsHelp } from "./ConversationShortcutsHelp";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import {
  capabilitiesFor, descriptorForEngine, loadProviderDescriptors, readyCatalog, runtimeScopesModelCatalog,
  useProviderDescriptors, type ProviderDescriptorCatalog,
} from "@/lib/providerDescriptors";
import { normalizeInteractionMode, planModeSupportForRuntime, type InteractionMode } from "@/lib/planModeAvailability";
import { saveThreadInteractionMode } from "@/lib/conversationServerPreferences";
import { handledByDesktopMenu, isMacPlatform, matchesBinding, readShortcutBindings, useDesktopMenuAcceleratorSync, type ShortcutActionId } from "@/lib/shortcutBindings";
import { conversationStorageScope, subscribeConversationStorageScope } from "@/lib/conversationStorageScope";
import { beginConversationControl, clearConversationControl, listConversationControls, subscribeConversationControls, updateConversationControl, type ControlIntent } from "@/lib/conversationControlState";
import { archiveUnconfirmedSend, listArchivedSendIntents, archiveUnconfirmedRetry, listArchivedRetryIntents, type ArchivedSendIntent, type ArchivedRetryIntent, type PendingReceiptCheck } from "@/lib/unconfirmedSendArchive";
import { listLegacyComposerEntries, prepareLegacyComposerCopy, type LegacyComposerEntry } from "@/lib/legacyConversationRecovery";
import { useSpeechInput } from "@/lib/useSpeechInput";
import { toolFileUrl, type ResourceLinkTarget, type ToolFile } from "@/lib/resourcePreview";
import { OpenToolFile } from "@/components/chat/timeline/ToolFileLinks";
import type { MessageAnnotationMark, MessageAttachmentChip } from "./ConversationMessage";
import { ThreadEditorProvider } from "./threadEditorContext";

const SIDEBAR_LIST_EVENT_TYPES = new Set([
  "core.turn.completed",
  "core.turn.failed",
  "core.turn.interrupted",
  "core.approval.requested",
  "core.user_input.requested",
]);

function useLatestCallback<A extends unknown[], R>(fn: (...args: A) => R): (...args: A) => R {
  const ref = useRef(fn);
  useLayoutEffect(() => {
    ref.current = fn;
  });
  return useCallback((...args: A) => ref.current(...args), []);
}

function credentialAvailable(credential: ConversationCredential): boolean {
  return credential.present !== false
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
  descriptors: ProviderDescriptorCatalog | null,
): RuntimeInstance | undefined {
  const available = runtimes.filter((runtime) => runtimeAvailableForSelection(runtime, descriptors));
  const defaultAdapter = desktopChatBridge() ? descriptorForEngine(descriptors, engine)?.identity.default_adapter_id : "";
  if (defaultAdapter) {
    const native = available.filter((row) => row.adapter_id === defaultAdapter);
    if (native.length) return pickRuntimeForEngine(engine, native) as RuntimeInstance | undefined;
  }
  return pickRuntimeForEngine(engine, available) as RuntimeInstance | undefined;
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

export interface ConversationExperience {
  defaultMode?: ThreadMode;
  draftScope?: string;
  hideProjects?: boolean;
  projectNavigationOnly?: boolean;
  allowFork?: boolean;
  threadFilter?: (thread: ConversationThread) => boolean;
  threadHref?: (threadId: string) => string;
  newThreadHref?: (draftId: string) => string;
  home?: Pick<ConversationHomeProps, "title" | "subtitle" | "introAccessory" | "placeholder" | "suggestions" | "hideWorkspaceControls" | "projectSelectionOnly" | "hideReadinessGuide">;
  emptyThread?: { icon?: IconName; title: string; hint?: string; suggestions?: ConversationHomeSuggestion[] };
  sidebarLabels?: ConversationSidebarLabels;
  headerActions?: React.ReactNode;
  composerAccessory?: React.ReactNode;
  panelExtensions?: PanelExtensionConfig;
  turnAccessory?: (turnId: string) => React.ReactNode;
  composerRefs?: () => ComposerCapabilityRef[];
  onSent?: () => void;
  localMentions?: ComposerLocalMentions;
  composerDrop?: ComposerCustomDrop;
  composerControls?: React.ReactNode | ((props: {
    open: boolean;
    onOpenChange: (open: boolean) => void;
    disabled: boolean;
  }) => React.ReactNode);
  composerPlaceholder?: string;
  composerHandle?: React.MutableRefObject<ConversationComposerHandle | null>;
  preview?: {
    threads: ConversationThread[];
    view: ConversationView;
    notice?: string;
    onSend?: (text: string) => void;
  };
  createThread?: (
    input: Parameters<typeof createConversationThread>[0],
    options: Parameters<typeof createConversationThread>[1],
    context: { promptText: string },
  ) => Promise<{ threadId: string; href?: string }>;
}

export interface ConversationComposerHandle {
  insertText: (text: string, options?: { focus?: boolean }) => void;
  focus: () => void;
  showReplies: () => void;
}

function defaultThreadHref(threadId: string): string {
  return `/chat/${encodeURIComponent(threadId)}`;
}

function defaultThreadFilter(thread: ConversationThread): boolean {
  return thread.mode !== "src" && !isInternalCtfThread(thread);
}

function defaultNewThreadHref(draftId: string): string {
  return `/chat?draft=${encodeURIComponent(draftId)}`;
}

const SIDE_CHAT_EXPERIENCE: ConversationExperience = {
  allowFork: false,
  emptyThread: {
    icon: "messages",
    title: "侧边聊天",
    hint: "就主对话提一个侧边问题。这里带着主对话的完整上下文，只作参考，不会打断或改变主对话；关闭标签即丢弃。",
  },
  composerPlaceholder: "提出侧边问题…",
};

function experienceDraftKey(threadId: string, draftId: string, scope?: string): string {
  if (!threadId && scope) return `experience:${encodeURIComponent(scope)}:${composerDraftKey("", draftId)}`;
  return composerDraftKey(threadId, draftId);
}

export function ConversationShell({ threadId = "", navigation, experience, mainContentId = CONVERSATION_MAIN_CONTENT_ID, sidebarId = "conversation-sidebar", sideOf, paneChild }: {
  threadId?: string;
  navigation: ConversationNavigation;
  experience?: ConversationExperience;
  mainContentId?: string;
  sidebarId?: string;
  sideOf?: { threadId: string; title: string; overlay: boolean; onClose: () => void; sideChat?: { parentStatus: SideChatParentStatus; runOwned?: boolean; runTargets?: RunSideChatTarget[] } };
  paneChild?: PaneChildContext;
}) {
  const sideChat = sideOf?.sideChat;
  const runSideChat = Boolean(sideChat?.runOwned);
  const isolatedChrome = Boolean(sideOf || paneChild);
  const paneLayout = usePaneLayout();
  const paneCount = countPanes(paneLayout.root);
  useEffect(() => {
    if (!isolatedChrome) {
      paneLayoutStore.setPrimaryThread(threadId);
    }
  }, [isolatedChrome, threadId]);
  const { router, pathname, searchParams } = navigation;
  const parentAgentPage = useAgentPage();
  const [agentPage, setAgentPage] = useState<string | null>(null);
  const [agentPageWidth, setAgentPageWidth] = useState(520);
  const [agentPageSheet, setAgentPageSheet] = useState(false);
  const [agentPageMax, setAgentPageMax] = useState(720);
  const agentPageQuery = searchParams.get("agent");
  useEffect(() => { if (!sideOf) setAgentPage(agentPageQuery); }, [threadId, agentPageQuery, sideOf?.threadId]);
  const threadHref = experience?.threadHref ?? defaultThreadHref;
  const threadFilter = experience?.threadFilter ?? defaultThreadFilter;
  const newThreadHref = experience?.newThreadHref ?? defaultNewThreadHref;
  const defaultMode = experience?.defaultMode ?? "conversation";
  const threadMessageHref = (id: string, messageId: string) => {
    const href = threadHref(id);
    return `${href}${href.includes("?") ? "&" : "?"}message=${encodeURIComponent(messageId)}`;
  };
  const preview = experience?.preview;
  const panelExtensions: PanelExtensionConfig = {
    ...experience?.panelExtensions,
    extensions: [
      ...(experience?.panelExtensions?.extensions ?? []),
      ...(!preview && !sideOf ? [{
        key: SIDE_CHAT_KEY,
        label: "侧边聊天",
        icon: "messages" as const,
        description: "带着主对话的上下文提问，不打断主对话",
        matches: (key: string) => Boolean(sideChatIdOf(key)),
        open: (ownerId: string) => { void startSideChat(ownerId); },
        onClose: (surface: { key: string }) => {
          const sideId = sideChatIdOf(surface.key);
          if (sideId) void discardSideChat(sideId).catch((failure) => setNotice(`侧边聊天未能丢弃：${failure instanceof Error ? failure.message : String(failure)}`, "warning"));
        },
        render: ({ surface, active, threadId: ownerId }: { surface: { id: string; key: string }; active: boolean; threadId: string }) => {
          const view = conversation.view;
          if (!view || view.thread.thread_id !== ownerId) return null;
          const sideId = sideChatIdOf(surface.key);
          if (sideId && !active) return null;
          if (!sideId) {
            return <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center">
              <Icon name="messages" size={22} className="text-cx-fg-3" />
              <p className="text-cx-sm text-cx-fg-3">这个标签来自旧版侧边聊天，不包含主对话上下文。</p>
              <Button size="md" onClick={() => { chatPanel.closeSurface(ownerId, surface.id); void startSideChat(ownerId); }}>开启新的侧边聊天</Button>
            </div>;
          }
          return <ConversationShell key={`${storageScope}::${sideId}`} threadId={sideId} navigation={navigation}
            mainContentId={`${mainContentId}-side-${sideId}`} sidebarId={`${sidebarId}-side-${sideId}`}
            experience={SIDE_CHAT_EXPERIENCE}
            sideOf={{ threadId: ownerId, title: view.thread.title, overlay: false, onClose: () => chatPanel.closeSurface(ownerId, surface.id),
              sideChat: { parentStatus: sideChatParentStatus(view) } }} />;
        },
      }] : []),
    ],
  };
  const previewHost = experience;
  const liveThreads = useConversationThreads({
    enabled: !preview && !sideChat,
    activeThreadId: threadId,
  });
  const liveRefreshList = liveThreads.refresh;
  const liveMarkThreadsRead = liveThreads.markThreadsRead;
  const liveConversation = useConversation(preview ? "" : threadId);
  useEffect(() => {
    if (!threadId || preview) return;
    return reportVisibleThread(threadId);
  }, [preview, threadId]);
  const [previewView, setPreviewView] = useState<ConversationView | null>(preview?.view || null);
  useEffect(() => setPreviewView(preview?.view || null), [preview]);
  const threads = preview?.threads || liveThreads.threads;
  const refreshList = useCallback(async () => {
    if (!preview) await liveRefreshList();
  }, [liveRefreshList, preview]);
  const markThreadsRead = useCallback((...args: Parameters<typeof liveMarkThreadsRead>) => {
    if (preview) return Promise.resolve();
    return liveMarkThreadsRead(...args);
  }, [liveMarkThreadsRead, preview]);
  const conversation = useMemo(() => preview ? {
    ...liveConversation,
    view: previewView,
    loading: false,
    loadingOlder: false,
    error: "",
    connected: false,
    streamStatus: "idle" as const,
    refresh: async () => undefined,
    loadOlderMessages: async () => false,
    jumpToLatestMessages: async () => false,
  } : liveConversation, [liveConversation, preview, previewView]);
  const draftParam = searchParams.get("draft") || "";
  const draftScope = experience?.draftScope;
  const draftKey = experienceDraftKey(threadId, draftParam, draftScope);
  const activeSendContextRef = useRef({ draftKey, viewThreadId: "" });
  activeSendContextRef.current = {
    draftKey,
    viewThreadId: conversation.view?.thread.thread_id || "",
  };
  const draftHydratingRef = useRef(true);
  const [hydratedDraftKey, setHydratedDraftKey] = useState("");
  const hydratedDraftRef = useRef<{ draftKey: string; storageScope: string } | null>(null);
  const threadIdRef = useRef(threadId);
  threadIdRef.current = threadId;
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
    interactionMode: "default" as InteractionMode,
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

  const streamThreadId = conversation.view?.thread.thread_id;
  const latestListEventKey = useConversationStreamSelector(
    conversation.stream,
    streamThreadId,
    (snapshot) => {
      const event = snapshot.events.findLast((row) => SIDEBAR_LIST_EVENT_TYPES.has(row.event_type));
      return event ? String(event.event_id || `${event.seq}:${event.event_type}`) : "";
    },
  );
  useEffect(() => {
    if (!latestListEventKey) return;
    void refreshList();
  }, [latestListEventKey, refreshList]);

  const [credentialRows, setCredentials] = useState<ConversationCredential[]>([]);
  const [credentialSource, setCredentialSource] = useState<SourceState>(sourceLoading());
  const credentialLoadSeqRef = useRef(0);
  const [credentialId, setCredentialId] = useState("");

  const [runtimes, setRuntimes] = useState<RuntimeInstance[]>([]);
  const [runtimeSource, setRuntimeSource] = useState<SourceState>(sourceLoading());
  const [runtimeKey, setRuntimeKey] = useState("");
  const descriptorState = useProviderDescriptors({ enabled: !preview });
  const descriptors = readyCatalog(descriptorState);
  const descriptorSource = useMemo<SourceState>(() => (
    descriptorState.status === "ready" ? sourceOk()
      : descriptorState.status === "error" ? sourceError(descriptorState.message, descriptorState.httpStatus)
        : sourceLoading()
  ), [descriptorState]);
  const runTargets = useMemo(
    () => (runSideChat ? sideChat?.runTargets || [] : []),
    [runSideChat, sideChat?.runTargets],
  );
  const effectiveRuntimes = useMemo(() => {
    if (!runSideChat || !runTargets.length) return runtimes;
    const byKey = new Map(runtimes.map((row) => [row.key, row]));
    for (const target of runTargets) {
      const key = `${target.adapter_id}:${target.instance_id}`;
      const existing = byKey.get(key);
      byKey.set(key, {
        ...(existing || {
          key,
          adapter_id: target.adapter_id,
          instance_id: target.instance_id,
          label: target.label,
        }),
        key,
        adapter_id: target.adapter_id,
        instance_id: target.instance_id,
        engine: target.engine || existing?.engine,
        enabled: true,
        configured: true,
        health: existing?.health?.healthy
          ? existing.health
          : { ...(existing?.health || {}), healthy: true, probed_at: existing?.health?.probed_at || "run-dispatched" },
      });
    }
    return Array.from(byKey.values());
  }, [runSideChat, runTargets, runtimes]);
  const effectiveCredentialRows = useMemo(() => {
    if (!runSideChat || !runTargets.length) return credentialRows;
    const byId = new Map(credentialRows.map((row) => [row.id, row]));
    for (const target of runTargets) {
      if (!target.credential_id) continue;
      const existing = byId.get(target.credential_id);
      if (!existing) {
        byId.set(target.credential_id, {
          id: target.credential_id,
          label: target.label || target.engine,
          engine: target.engine,
          source: target.credential_id.startsWith("account:") ? "stored" : "system",
          present: true,
          detection_status: "present",
          status: "ready",
          models: target.model ? [{ id: target.model, label: target.model }] : [],
          candidate_models: [],
          default_model: target.model || undefined,
        });
        continue;
      }
      const needsModel = Boolean(
        target.model && !allCredentialModels(existing).some((m) => m.id === target.model),
      );
      const needsAvailability = !credentialAvailable(existing);
      if (needsModel || needsAvailability) {
        byId.set(target.credential_id, {
          ...existing,
          ...(needsAvailability ? { present: true, detection_status: "present" as const, status: "ready" } : {}),
          ...(needsModel ? { models: [...existing.models, { id: target.model, label: target.model }] } : {}),
        });
      }
    }
    return Array.from(byId.values());
  }, [runSideChat, runTargets, credentialRows]);
  const credentials = useMemo(() => effectiveCredentialRows.map((credential) => {
    const selected = effectiveRuntimes.find((runtime) => runtime.key === runtimeKey && runtime.engine === credential.engine
      && (threadId ? runtime.enabled !== false : runtimeAvailableForSelection(runtime, descriptors)));
    const runtime = selected || runtimeForEngine(credential.engine, effectiveRuntimes, descriptors);
    const projected = credentialForRuntime(credential, runtime?.key || "", runtimeScopesModelCatalog(descriptors, runtime?.key));
    if (!runSideChat) return projected;
    const dispatched = runTargets.filter(t => t.credential_id === credential.id && `${t.adapter_id}:${t.instance_id}` === runtime?.key && t.model);
    return { ...projected, models: [...projected.models, ...dispatched.filter(t => !allCredentialModels(projected).some(m => m.id === t.model)).map(t => ({ id: t.model, label: t.model }))] };
  }), [effectiveCredentialRows, descriptors, effectiveRuntimes, runtimeKey, threadId, runSideChat, runTargets]);
  const restoredBindingContextRef = useRef<{ draftKey: string; projectId: string } | null>(null);
  const [probing, setProbing] = useState(false);

  const [model, setModel] = useState("");
  const [effort, setEffort] = useState("");
  const [serviceTier, setServiceTier] = useState("");
  useEffect(() => { setServiceTier(readPreferredServiceTier()); }, []);
  const [accessMode, setAccessMode] = useState("supervised");
  const { defaultAccessMode, runningSend, sendKey } = useChatPreferences();
  const [interactionMode, setInteractionMode] = useState<InteractionMode>("default");
  const runtimeSyncedKeyRef = useRef("");
  const draftRuntimeFields = useCallback((key: string, live: {
    credentialId: string; runtimeKey: string; model: string; effort: string; accessMode: string;
  }) => {
    if (!key.startsWith("thread:") || runtimeSyncedKeyRef.current === key) {
      return { credentialId: live.credentialId, runtimeKey: live.runtimeKey, model: live.model, effort: live.effort, accessMode: live.accessMode };
    }
    const stored = readComposerDraft(key);
    return { credentialId: stored?.credentialId, runtimeKey: stored?.runtimeKey, model: stored?.model, effort: stored?.effort, accessMode: stored?.accessMode };
  }, []);

  const projectScope: "src" | "chat" = defaultMode === "src" ? "src" : "chat";
  const projectNoun = projectScope === "src" ? "研究项目" : "项目文件夹";
  const [projects, setProjects] = useState<ConversationProject[]>([]);
  const [projectSource, setProjectSource] = useState<SourceState>(sourceLoading());
  const [projectId, setProjectId] = useState("");
  const [savingProjectDefault, setSavingProjectDefault] = useState(false);
  const [projectCreating, setProjectCreating] = useState(false);
  const [repoLessCreateOpen, setRepoLessCreateOpen] = useState(false);
  const [repoLessCreateName, setRepoLessCreateName] = useState("");
  const [repoLessCreateDesc, setRepoLessCreateDesc] = useState("");
  const [repoLessCreateError, setRepoLessCreateError] = useState("");
  const [mode, setMode] = useState<ThreadMode>(defaultMode);
  const [workspaceMode, setWorkspaceMode] = useState<WorkspaceBindMode>("shared_checkout");
  const [worktreeBranch, setWorktreeBranch] = useState("");
  const [existingWorktreePath, setExistingWorktreePath] = useState("");
  const [directoryIssue, setDirectoryIssue] = useState<{ message: string } | null>(null);
  const [preferPathInput, setPreferPathInput] = useState(false);
  const [requestPathInput, setRequestPathInput] = useState(false);
  const [recentPaths, setRecentPaths] = useState<string[]>([]);
  const [guideDismissed, setGuideDismissed] = useState(false);

  const [promptDocument, setPromptDocument] = useState<PromptDocument>(emptyPromptDocument);
  const prompt = plainTextFromDocument(promptDocument);
  const capabilityRefs = useMemo(() => composerRefs(promptDocument), [promptDocument]);
  const commandScopesRef = useRef(new Set<string>());
  const [, setCommandRevision] = useState(0);
  const storageScope = useSyncExternalStore(subscribeConversationStorageScope, conversationStorageScope, () => "");
  const commandScope = `${storageScope}::${draftKey}`;
  const composerStorageScopeRef = useRef(storageScope);
  const [controlRevision, setControlRevision] = useState(0);
  useEffect(() => subscribeConversationControls(() => setControlRevision((value) => value + 1)), []);
  const pendingControls = useMemo(() => listConversationControls(threadId), [threadId, commandScope, controlRevision]);
  useEffect(() => {
    const view = conversation.view;
    if (view?.thread.thread_id !== threadId || conversation.loading) return;
    for (const intent of pendingControls) {
      if (intent.kind === "steer") continue;
      const pending = intent.kind === "input" ? view.state.pending_user_input?.request_id === intent.requestId ? view.state.pending_user_input : null : view.state.pending_approvals?.[intent.requestId] || (view.state.pending_approval?.approval_id === intent.requestId ? view.state.pending_approval : null);
      if ((!pending || ["resolved", "expired"].includes(String(pending.status))) && intent.state !== "sending") clearConversationControl(intent);
    }
  }, [threadId, conversation.view, conversation.loading, pendingControls]);
  const controlView = useMemo(() => {
    const view = conversation.view;
    if (!view || view.thread.thread_id !== threadId || !pendingControls.length) return view;
    const pendingApprovals = { ...view.state.pending_approvals };
    let pendingApproval = view.state.pending_approval, pendingInput = view.state.pending_user_input;
    for (const intent of pendingControls) {
      if (intent.kind === "approval") {
        if (pendingApprovals[intent.requestId]) pendingApprovals[intent.requestId] = { ...pendingApprovals[intent.requestId], status: "resolving" };
        if (pendingApproval?.approval_id === intent.requestId) pendingApproval = { ...pendingApproval, status: "resolving" };
      } else if (intent.kind === "input" && pendingInput?.request_id === intent.requestId) pendingInput = { ...pendingInput, status: "resolving" };
    }
    return { ...view, state: { ...view.state, pending_approvals: pendingApprovals, pending_approval: pendingApproval, pending_user_input: pendingInput } };
  }, [conversation.view, threadId, pendingControls]);
  const busy = commandScopesRef.current.has(commandScope);
  const [notice, writeNotice, noticeControls] = useThreadScopedNotice(commandScope);
  const acceptedCommandsRef = useRef(new Map<string, string>());
  const [noticeTrackingRevision, setNoticeTrackingRevision] = useState(0);
  const setNotice = useCallback((text: string, kind: Parameters<typeof writeNotice>[1] = "sticky", commandId?: string) => {
    if (kind === "accepted" && commandId) acceptedCommandsRef.current.set(commandScope, commandId);
    else acceptedCommandsRef.current.delete(commandScope);
    setNoticeTrackingRevision((value) => value + 1);
    writeNotice(text, kind);
  }, [commandScope, writeNotice]);
  useEffect(() => {
    if (noticeControls.kind !== "accepted") return;
    const commandId = acceptedCommandsRef.current.get(commandScope);
    if (!commandId) return;
    const controller = new AbortController();
    let cancelled = false, timer: ReturnType<typeof setTimeout> | null = null, attempts = 0;
    const ownerScope = conversationStorageScope();
    const poll = async () => {
      try {
        const receipt = await recoverCommandReceipt(commandId, 1, 200, { signal: controller.signal });
        if (cancelled || ownerScope !== conversationStorageScope() || acceptedCommandsRef.current.get(commandScope) !== commandId) return;
        if (classifyCommandReceipt(receipt) === "completed") { acceptedCommandsRef.current.delete(commandScope); writeNotice(""); return; }
        if (receipt && classifyCommandReceipt(receipt) === "failed") { acceptedCommandsRef.current.delete(commandScope); writeNotice(receipt.error?.message || `原操作未完成（${receipt.state}）`, "warning"); return; }
        if (++attempts < 10) timer = setTimeout(() => void poll(), 1500);
      } catch (failure) { if (!cancelled && ownerScope === conversationStorageScope()) writeNotice(`原回执核对失败，受理结果仍未确认：${failure instanceof Error ? failure.message : String(failure)}\n原命令 ${commandId}`, "warning"); }
    };
    void poll();
    return () => { cancelled = true; controller.abort(); if (timer) clearTimeout(timer); };
  }, [commandScope, notice, noticeControls.kind, noticeTrackingRevision, writeNotice]);

  const speech = useSpeechInput((transcript) => {
    setPromptDocument((current) => {
      const currentText = flattenPromptDocument(current).trimEnd();
      const next = currentText ? `${currentText} ${transcript}` : transcript;
      return documentFromPromptAndRefs(next, refsFromDocument(current));
    });
  }, commandScope);
  const [{ message: error, recoveryAction: errorRetryKind }, setError] = useThreadScopedError(commandScope);

  useEffect(() => {
    if (!error) return;
    if (shouldScheduleNoticeAutoClear(notice, noticeControls.kind)) setNotice("");
  }, [error, notice, setNotice]);
  const [jumpTarget, setJumpTarget] = useState<{
    key: string;
    messageId: string;
    turnId?: string;
    startOffset?: number;
    endOffset?: number;
  } | null>(null);

  const [searchQuery, setSearchQuery] = useState("");
  const rawMessageParam = readMessageDeepLink(searchParams);
  const [deepLinkDismissed, setDeepLinkDismissed] = useState(false);
  const messageParam = deepLinkDismissed ? "" : rawMessageParam;
  const [highlightedMessageId, setHighlightedMessageId] = useState("");
  const ensureMessageVisible = conversation.ensureMessageVisible;

  const deepLinkThreadRef = useRef(threadId);
  useEffect(() => {
    if (deepLinkThreadRef.current === threadId) return;
    deepLinkThreadRef.current = threadId;
    setDeepLinkDismissed(false);
  }, [threadId]);

  const deepLinkParamRef = useRef(rawMessageParam);
  useEffect(() => {
    if (deepLinkParamRef.current === rawMessageParam) return;
    deepLinkParamRef.current = rawMessageParam;
    if (rawMessageParam) setDeepLinkDismissed(false);
  }, [rawMessageParam]);

  const appliedMessageDeepLinkRef = useRef("");
  useEffect(() => {
    if (!threadId || !messageParam) {
      appliedMessageDeepLinkRef.current = "";
      setHighlightedMessageId("");
      return;
    }
    if (
      conversation.loading
      || conversation.view?.thread.thread_id !== threadId
    ) {
      return;
    }
    const requestKey = JSON.stringify([threadId, messageParam]);
    if (appliedMessageDeepLinkRef.current === requestKey) {
      setHighlightedMessageId("");
      return;
    }
    let cancelled = false;
    let clearTimer: ReturnType<typeof setTimeout> | null = null;
    void (async () => {
      setHighlightedMessageId(messageParam);
      const result = await ensureMessageVisible(messageParam);
      if (cancelled || result === "cancelled") return;
      if (result !== "ok") {
        setNotice("消息不可用");
        setHighlightedMessageId("");
        replaceMessageDeepLink(null);
        return;
      }
      appliedMessageDeepLinkRef.current = requestKey;
      setHighlightedMessageId(messageParam);
      clearTimer = setTimeout(() => {
        if (!cancelled) setHighlightedMessageId("");
      }, 4000);
    })();
    return () => {
      cancelled = true;
      if (clearTimer) clearTimeout(clearTimer);
      setHighlightedMessageId("");
    };
  }, [
    threadId,
    messageParam,
    conversation.loading,
    conversation.view?.thread.thread_id,
    ensureMessageVisible,
  ]);

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
  const [laterDialogOpen, setLaterDialogOpen] = useState(false);
  const [relayConfirm, setRelayConfirm] = useState<{ sendMode: RunningSendDefault } | null>(null);
  const relayConfirmedRef = useRef(false);
  const [pendingSendDialog, setPendingSendDialog] = useState<(
    | { kind: "send"; draftKey: string; scope: string; receiptCheck?: PendingReceiptCheck }
    | { kind: "retry"; draftKey: string; scope: string; threadId: string; turnId: string; commandId: string; receiptCheck?: PendingReceiptCheck }
  ) | null>(null);
  const [pendingSendQuery, setPendingSendQuery] = useState(false);
  const [pendingSendStatus, setPendingSendStatus] = useState("");
  const [legacyRecovery, setLegacyRecovery] = useState<{ scope: string; draftKey: string; entries: LegacyComposerEntry[] } | null>(null);
  const [legacyConfirmed, setLegacyConfirmed] = useState(false);
  const [legacyRecoveryError, setLegacyRecoveryError] = useState("");
  const [archivedSends, setArchivedSends] = useState<ArchivedSendIntent[]>([]);
  const [archivedRetries, setArchivedRetries] = useState<ArchivedRetryIntent[]>([]);
  const [commandSessionsOpen, setCommandSessionsOpen] = useState(false);
  const [commandSessionsInstant, setCommandSessionsInstant] = useState(false);
  const [bottomPanelOpen, setBottomPanelOpen] = useState(false);
  const [bottomPanelInstant, setBottomPanelInstant] = useState(false);
  const [rightPanelInstant, setRightPanelInstant] = useState(false);
  const panel = useChatPanel(threadId);
  const rightPanelOpen = Boolean(threadId) && panel.isOpen;
  useEffect(() => {
    if (!rightPanelInstant) return;
    const clear = () => setRightPanelInstant(false);
    window.addEventListener("pointerdown", clear, true);
    return () => window.removeEventListener("pointerdown", clear, true);
  }, [rightPanelInstant]);
  const [panelSheet, setPanelSheet] = useState(false);
  const sheetBreakpoint = panelExtensions?.sheetBreakpoint ?? PANEL_SHEET_BREAKPOINT;
  useEffect(() => {
    const mq = window.matchMedia(`(max-width: ${sheetBreakpoint}px)`);
    const sync = () => setPanelSheet(mq.matches);
    sync();
    mq.addEventListener("change", sync);
    return () => mq.removeEventListener("change", sync);
  }, [sheetBreakpoint]);
  const dockFocus = panelExtensions?.focusLayout === "dock" && rightPanelOpen && panel.maximized && !panelSheet;
  const [dockRepliesOpen, setDockRepliesOpen] = useState(false);
  const composerDockRef = useRef<HTMLDivElement | null>(null);
  const [dockInset, setDockInset] = useState(0);
  const [dockReplyHeight, setDockReplyHeight] = useState(320);
  useEffect(() => {
    if (!dockFocus) {
      setDockRepliesOpen(false);
      return;
    }
    const footer = composerDockRef.current;
    const shell = shellRef.current;
    if (!footer || !shell || typeof ResizeObserver === "undefined") return;
    const measure = () => {
      const shellRect = shell.getBoundingClientRect();
      setDockInset(Math.max(0, Math.round(shellRect.bottom - footer.getBoundingClientRect().top)));
      setDockReplyHeight(Math.round(Math.min(520, Math.max(220, shellRect.height * 0.45))));
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(footer);
    observer.observe(shell);
    return () => observer.disconnect();
  }, [dockFocus]);
  const dockMessages = conversation.view?.messages;
  const dockSnippet = useMemo(() => {
    if (!dockFocus || !dockMessages) return "";
    const last = [...dockMessages].reverse().find((message) => message.role === "assistant" && message.kind === "message" && message.text);
    return last?.text ? `最新回复：${last.text.replace(/[*#>`_]/g, "").replace(/\s+/g, " ").trim().slice(0, 80)}` : "";
  }, [dockFocus, dockMessages]);
  const dockTurnCount = conversation.view?.turns?.length ?? 0;
  const dockTurnCountRef = useRef(dockTurnCount);
  useEffect(() => {
    if (dockFocus && dockTurnCount > dockTurnCountRef.current) setDockRepliesOpen(true);
    dockTurnCountRef.current = dockTurnCount;
  }, [dockFocus, dockTurnCount]);
  const [modelPickerOpen, setModelPickerOpen] = useState(false);
  const [modelPickerInstant, setModelPickerInstant] = useState(false);
  const handleModelPickerOpenChange = useCallback((open: boolean) => {
    if (open) setModelPickerInstant(false);
    setModelPickerOpen(open);
  }, []);
  const composerModesRef = useRef<ConversationComposerModesHandle>(null);
  const [compactComposerScope, setCompactComposerScope] = useState("");
  const composerCompact = compactComposerScope === commandScope;
  const handleComposerCompactChange = useCallback((compact: boolean) => {
    setCompactComposerScope(compact ? commandScope : "");
  }, [commandScope]);
  useEffect(() => {
    if (modelPickerOpen) setCompactComposerScope("");
  }, [modelPickerOpen]);
  const panelControlsRef = useRef<HTMLDivElement | null>(null);
  const detailsButtonRef = useRef<HTMLButtonElement | null>(null);
  const mainRef = useRef<HTMLElement | null>(null);
  const threadDetails = useThreadDetailsState(mainRef);
  const [mainLeft, setMainLeft] = useState(0);
  useEffect(() => {
    const shell = shellRef.current;
    const main = mainRef.current;
    if (!shell || !main || typeof ResizeObserver === "undefined") return;
    const measure = () => { if (!paneResizing()) setMainLeft(main.offsetLeft); };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(shell);
    observer.observe(main);
    const offResizeEnd = onPaneResizeEnd(measure);
    return () => { observer.disconnect(); offResizeEnd(); };
  }, []);
  const runningToolCount = useConversationRunningToolCount(conversation.stream, conversation.view);

  const bottomPanelPresence = useTransitionState(bottomPanelOpen, {
    durationVar: bottomPanelInstant ? undefined : "--panel-close-dur",
    durationMs: bottomPanelInstant ? 0 : 180,
  });
  useEffect(() => {
    const onOpenLog = (event: Event) => {
      event.preventDefault();
      setBottomPanelInstant(false);
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
  const [projectDeletion, setProjectDeletion] = useState<ConversationProject[] | null>(null);
  const projectDeletionArchived = useMemo(() => {
    if (!projectDeletion) return 0;
    const ids = new Set(projectDeletion.map((project) => project.project_id));
    return threads.filter((thread) => thread.state.status === "archived" && ids.has(thread.project_id || "")).length;
  }, [projectDeletion, threads]);

  const [impactOpen, setImpactOpen] = useState(false);
  const [impactPreview, setImpactPreview] = useState<ImpactPreview | null>(null);
  const [impactLoading, setImpactLoading] = useState(false);
  const [impactEditedText, setImpactEditedText] = useState("");
  const [impactFileMode, setImpactFileMode] = useState<ImpactFileMode>("keep_files");
  const [impactRewindMode, setImpactRewindMode] = useState<RewindScopeMode>("conversation");
  const [impactCommandId, setImpactCommandId] = useState("");
  const [impactForkSource, setImpactForkSource] = useState<ForkSourceFreeze | null>(null);
  const [showSuperseded, setShowSuperseded] = useState(false);
  const [auditTurns, setAuditTurns] = useState<ConversationTurn[]>([]);
  const [auditMessages, setAuditMessages] = useState<ConversationMessage[]>([]);
  useEffect(() => {
    const landing = conversation.auditLanding;
    if (!landing || landing.threadId !== threadId) return;
    setAuditTurns(landing.turns); setAuditMessages(landing.messages); setShowSuperseded(true);
    setHighlightedMessageId(landing.messageId);
  }, [conversation.auditLanding, threadId]);


  const [attachments, setAttachments] = useState<PromptBarAttachment[]>([]);
  const [composerUndoEpoch, setComposerUndoEpoch] = useState(0);
  const {
    sidebarCollapsed,
    toggleSidebarCollapsed,
    sidebarWidth,
    setSidebarWidth,
    mobileSidebarOpen,
    setMobileSidebarOpen,
    headerSlot,
    previewSidebarWidth,
  } = useConversationChrome();
  const renderHeader = (header: React.ReactNode) => (headerSlot && !isolatedChrome && paneCount <= 1 ? createPortal(header, headerSlot) : header);
  const openAgentPage = useCallback((id: string) => {
    if (sideOf && parentAgentPage) { parentAgentPage(id); return; }
    if (id === threadId) return;
    const width = shellRef.current?.clientWidth || window.innerWidth;
    if (panelSheet || width - (sidebarCollapsed ? 0 : sidebarWidth) < 1280) chatPanel.close(threadId);
    setAgentPage(id);
    const params = new URLSearchParams(searchParams.toString()); params.set("agent", id);
    router.replace(`${pathname}?${params}`, {scroll: false});
  }, [sideOf, parentAgentPage, threadId, panelSheet, sidebarCollapsed, sidebarWidth, searchParams, pathname, router]);
  const closeAgentPage = useCallback(() => {
    setAgentPage(null);
    const params = new URLSearchParams(searchParams.toString()); params.delete("agent");
    router.replace(`${pathname}${params.size ? `?${params}` : ""}`, {scroll: false});
    requestAnimationFrame(() => shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
  }, [searchParams, pathname, router]);
  useEffect(() => {
    if (sideOf) return;
    const root = shellRef.current;
    if (!root) return;
    const measure = () => {
      const body = root.clientWidth - (sidebarCollapsed ? 0 : sidebarWidth);
      setAgentPageSheet(body < 760);
      setAgentPageMax(Math.max(360, body - 360));
    };
    let frame = 0;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(measure);
    });
    observer.observe(root); measure();
    return () => { observer.disconnect(); cancelAnimationFrame(frame); };
  }, [sideOf, sidebarCollapsed, sidebarWidth]);
  useEffect(() => {
    if (!sideOf || !conversation.view) return;
    if (sideChat && !takeSideChatFocus(threadId)) return;
    const frame = requestAnimationFrame(() => (shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]") || mainRef.current)?.focus());
    return () => cancelAnimationFrame(frame);
  }, [sideOf?.threadId, conversation.view?.thread.thread_id]);
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
      const sidebar = document.getElementById(sidebarId);
      const active = document.activeElement as HTMLElement | null;
      const focusRoot = mobileSidebarTabRoot(sidebar, active);
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
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.cancelAnimationFrame(frame);
      desktop.removeEventListener("change", closeOnDesktop);
      window.removeEventListener("keydown", onKey, true);
      document.querySelector<HTMLButtonElement>(".workspace-rail-sidebar-toggle")?.focus();
    };
  }, [mobileSidebarOpen, sidebarId]);

  const [largePasteText, setLargePasteText] = useState<string | null>(null);
  const [largePasteSelection, setLargePasteSelection] = useState<LargePasteSelection | null>(null);
  const [shortcutsHelpOpen, setShortcutsHelpOpen] = useState(false);
  const [shortcutsHelpInstant, setShortcutsHelpInstant] = useState(false);
  const shortcutsHelpWasOpenRef = useRef(false);
  const providerScope = providerNoticeScope(
    commandScope, runtimeKey, credentialId, model, conversation.view?.state.agent_session_id || "",
  );
  const [handoffNotice, setHandoffNotice] = useThreadScopedString(providerScope);
  const [dismissedProviderPreview, setDismissedProviderPreview] = useThreadScopedString(commandScope);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const reselectInputRef = useRef<HTMLInputElement | null>(null);
  const reselectSourceRef = useRef<{ attachmentId: string; draftKey: string; scope: string } | null>(null);
  const pendingProjectIdRef = useRef("");
  const commandBusyRef = useRef(false);
  commandBusyRef.current = busy;
  const applyingHistoryRef = useRef(false);
  const historyIndexRef = useRef<number | null>(null);
  const historyScratchRef = useRef<{
    promptDocument: ReturnType<typeof emptyPromptDocument>;
  } | null>(null);
  const [historyIndex, setHistoryIndex] = useState<number | null>(null);
  const [stashOpen, setStashOpen] = useState(false);
  const [stashName, setStashName] = useState("");
  const [stashes, setStashes] = useState<ComposerStashEntry[]>([]);

  const [memory, setMemory] = useState<ConversationMemorySnapshot | null>(null);
  const [memoryLoading, setMemoryLoading] = useState(false);

  const detailsThreadRef = useRef(threadId);
  useEffect(() => {
    if (detailsThreadRef.current === threadId) return;
    detailsThreadRef.current = threadId;
    setDetailsPayload(null);
  }, [threadId]);

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
      writeComposerDraft(experienceDraftKey("", nextDraft, draftScope), {
        prompt: "", promptSegments: [], capabilityRefs: [], attachments: [],
        credentialId: current.credentialId, runtimeKey: current.runtimeKey,
        model: current.model, effort: current.effort, accessMode: current.accessMode,
        interactionMode: current.interactionMode, projectId: nextProjectId,
      });
      flushComposerDraftStore();
    }
    setMobileSidebarOpen(false);
    router.push(newThreadHref(nextDraft));
  }, [router, newThreadHref, draftScope]);

  useDesktopMenuAcceleratorSync();

  useEffect(() => {
    const focusComposer = () => {
      shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus();
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
      const owner = (e.target instanceof Element ? e.target : document.activeElement)?.closest(".ai-conv-shell");
      if ((owner && owner !== shellRef.current) || (!owner && isolatedChrome)) return;
      const mod = e.metaKey || e.ctrlKey;
      if (!isolatedChrome && mod && !e.altKey && (e.key === "\\" || e.code === "Backslash")) {
        e.preventDefault();
        const activeId = paneLayoutStore.getSnapshot().activePaneId;
        paneLayoutStore.splitPane(activeId, e.shiftKey ? "vertical" : "horizontal");
        return;
      }
      if (!isolatedChrome && !inTextField(e.target) && mod && e.shiftKey && !e.altKey && e.key.toLowerCase() === "z") {
        e.preventDefault();
        paneLayoutStore.toggleZoomPane();
        return;
      }
      if (!isolatedChrome && mod && e.shiftKey && !e.altKey && (e.key === "[" || e.key === "]")) {
        e.preventDefault();
        paneLayoutStore.focusDirection(e.key === "]" ? "next" : "prev");
        return;
      }
      const mac = isMacPlatform();
      const desktop = Boolean(desktopChatBridge());
      const bindings = readShortcutBindings();
      const hit = (id: ShortcutActionId) => matchesBinding(e, bindings[id], mac);
      if (hit("newChat") && !handledByDesktopMenu("newChat", desktop)) {
        e.preventDefault();
        startNewChat();
        return;
      }
      if (desktop && hit("search") && !handledByDesktopMenu("search", desktop)) {
        e.preventDefault();
        setCommandSessionsInstant(true);
        setCommandSessionsOpen(true);
        return;
      }
      if (hit("toggleSidebar") && !handledByDesktopMenu("toggleSidebar", desktop)) {
        e.preventDefault();
        runInstantTransition(
          () => toggleSidebarCollapsed(),
          shellRef.current?.querySelector<HTMLElement>(".ai-conv-sidebar"),
        );
        return;
      }
      if (hit("modelPicker")) {
        e.preventDefault();
        setModelPickerInstant(true);
        setModelPickerOpen((open) => !open);
        return;
      }
      if (hit("toggleLog") && threadId) {
        e.preventDefault();
        setBottomPanelInstant(true);
        setBottomPanelOpen((open) => !open);
        return;
      }
      if (threadId) {
        const builtInSurfaces: Array<[ShortcutActionId, () => void]> = panelExtensions?.exclusive ? [] : [
          ["panelDiff", () => chatPanel.toggle(threadId, "diff")],
          ["panelPreview", () => {
            const current = panel.surfaces.find((item) => item.id === panel.activeId);
            if (panel.isOpen && current?.kind === "preview") chatPanel.close(threadId);
            else chatPanel.openPreview(threadId);
          }],
          ["panelFiles", () => chatPanel.toggle(threadId, "files")],
          ["panelTerminal", () => chatPanel.toggle(threadId, "terminal")],
          ["panelOverview", () => chatPanel.toggle(threadId, "overview")],
          ["panelAgents", () => chatPanel.toggle(threadId, "agents")],
        ];
        const surfaces: Array<[ShortcutActionId, () => void]> = [
          ["togglePanel", () => (experience?.panelExtensions ? togglePanelWithExtensions(threadId, panelExtensions) : chatPanel.toggle(threadId))],
          ...builtInSurfaces,
        ];
        const action = surfaces.find(([id]) => hit(id))?.[1];
        if (action) {
          e.preventDefault();
          setDetailsPayload(null);
          setRightPanelInstant(true);
          runInstantTransition(
            action,
            shellRef.current?.querySelector<HTMLElement>(".cx-panel-aside"),
          );
          return;
        }
      }
      if (inTextField(e.target)) return;
      if (e.key === "Escape" && !mod && !e.altKey) {
        if (mobileSidebarOpen) {
          setMobileSidebarOpen(false);
          document.querySelector<HTMLButtonElement>(".workspace-rail-sidebar-toggle")?.focus();
          return;
        }
        if (shortcutsHelpOpen) { setShortcutsHelpOpen(false); return; }
        if (detailsPayload) { setDetailsPayload(null); focusComposer(); return; }
        if (readingPrefsOpen) { setReadingPrefsOpen(false); focusComposer(); return; }
        if (!isolatedChrome && paneLayoutStore.getSnapshot().zoomedPaneId) {
          paneLayoutStore.unzoom();
          return;
        }
        if (infoDrawerOpen) { setInfoDrawerOpen(false); focusComposer(); return; }
        if (bottomPanelOpen) { setBottomPanelOpen(false); focusComposer(); return; }
        if (dockRepliesOpen) { setDockRepliesOpen(false); return; }
        if (rightPanelOpen && panel.maximized) { chatPanel.setMaximized(threadId, false); return; }
        focusComposer();
        return;
      }
      if (hit("focusComposer")) {
        e.preventDefault();
        focusComposer();
        return;
      }
      if (hit("help")) {
        e.preventDefault();
        setShortcutsHelpInstant(true);
        setShortcutsHelpOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [startNewChat, shortcutsHelpOpen, detailsPayload, infoDrawerOpen, readingPrefsOpen, rightPanelOpen, bottomPanelOpen, mobileSidebarOpen, threadId, panel, toggleSidebarCollapsed, panelExtensions, experience?.panelExtensions, dockRepliesOpen]);

  useEffect(() => {
    const wasOpen = shortcutsHelpWasOpenRef.current;
    shortcutsHelpWasOpenRef.current = shortcutsHelpOpen;
    if (!wasOpen || shortcutsHelpOpen) return;
    let inner = 0;
    const outer = window.requestAnimationFrame(() => {
      inner = window.requestAnimationFrame(() => {
        shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus();
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

  useEffect(() => {
    const onCommand = (event: Event) => {
      if (isolatedChrome) return;
      const name = (event as CustomEvent<{ name?: string }>).detail?.name;
      if (name === "search") {
        setCommandSessionsInstant(true);
        setCommandSessionsOpen(true);
      }
      if (name === "new-chat") startNewChat();
    };
    const onClose = (event: Event) => {
      if (isolatedChrome) return;
      const detail = (event as CustomEvent<{ token?: string; handled?: boolean }>).detail;
      const token = detail?.token;
      const bridge = desktopChatBridge();
      if (!token || !bridge?.acknowledgeClose) return;
      const snapshot = composerSnapshotRef.current;
      if (conversationStorageScope() && composerStorageScopeRef.current === conversationStorageScope()) writeComposerDraft(snapshot.draftKey, { ...snapshot, ...draftRuntimeFields(snapshot.draftKey, snapshot), promptSegments: snapshot.promptDocument.segments });
      if (detail.handled) return;
      detail.handled = true;
      void flushConversationPersistence().then((result) => {
        if (!result.persisted) setError(result.error || "本地恢复记录保存失败");
        void bridge.acknowledgeClose!({ token, ...result }).catch((error) => setError(error instanceof Error ? error.message : String(error)));
      }).catch((error) => {
        const message = error instanceof Error ? error.message : String(error);
        setError(message);
        void bridge.acknowledgeClose!({ token, persisted: false, error: message }).catch((failure) => setError(failure instanceof Error ? failure.message : String(failure)));
      });
    };
    const onCache = (event: Event) => {
      const detail = (event as CustomEvent<{ draftKey: string; error?: string }>).detail;
      if (detail?.draftKey === composerSnapshotRef.current.draftKey && detail.error) setNotice(`附件仍保留在当前输入中，刷新前需要重新选择。${detail.error}`);
    };
    const onPersistenceError = (event: Event) => {
      const detail = (event as CustomEvent<{ source: string; error: string; key?: string }>).detail;
      if (!detail?.error || (detail.key && detail.key !== composerSnapshotRef.current.draftKey)) return;
      setError(`本地内容尚未保存，当前窗口仍保留编辑；关闭或刷新前请重试。${detail.error}`);
    };
    const onVisualization = (event: Event) => {
      const request = (event as CustomEvent<{ threadId: string; scope: string; text: string; accepted: boolean; error: string }>).detail;
      if (!request || request.threadId !== composerSnapshotRef.current.draftKey.replace(/^thread:/, "") || request.scope !== conversationStorageScope()) { if (request) request.error = "visualization.composer.scope_changed: 当前聊天已改变，未插入内容"; return; }
      const next = insertPlainTextAtMarkedRange(composerSnapshotRef.current.promptDocument, `${plainTextFromDocument(composerSnapshotRef.current.promptDocument) ? "\n" : ""}${request.text}`);
      setPromptDocument(next);
      request.accepted = true; setNotice("图形后续内容已放入输入框，请确认模型和权限后发送");
    };
    window.addEventListener("muteki:visualization-followup", onVisualization);
    window.addEventListener("muteki:persistence-error", onPersistenceError);
    window.addEventListener("muteki:command", onCommand);
    window.addEventListener("muteki:before-close", onClose);
    window.addEventListener("muteki:attachment-cache", onCache);
    return () => {
      window.removeEventListener("muteki:visualization-followup", onVisualization);
      window.removeEventListener("muteki:persistence-error", onPersistenceError);
      window.removeEventListener("muteki:command", onCommand);
      window.removeEventListener("muteki:before-close", onClose);
      window.removeEventListener("muteki:attachment-cache", onCache);
    };
  }, [startNewChat, setError, setNotice, draftRuntimeFields]);

  useEffect(() => {
    if (hydratedDraftRef.current?.draftKey === draftKey && hydratedDraftRef.current.storageScope === storageScope) return;
    const previous = composerSnapshotRef.current;
    if (composerStorageScopeRef.current === storageScope && previous.draftKey && previous.draftKey !== draftKey) {
      writeComposerDraft(previous.draftKey, {
        prompt: previous.prompt,
        promptSegments: previous.promptDocument.segments,
        capabilityRefs: previous.capabilityRefs,
        attachments: previous.attachments,
        ...draftRuntimeFields(previous.draftKey, previous),
        interactionMode: previous.interactionMode,
        projectId: previous.projectId,
      });
      flushComposerDraftStore();
    }
    runtimeSyncedKeyRef.current = "";
    if (composerStorageScopeRef.current !== storageScope) {
      setCredentialId(""); setRuntimeKey(""); setModel(""); setEffort(""); setAccessMode("supervised"); setInteractionMode("default"); setProjectId("");
      pendingProjectIdRef.current = "";
    }
    composerStorageScopeRef.current = storageScope;
    draftHydratingRef.current = true;
    appliedNewChatDefaultsRef.current = "";
    restoredBindingContextRef.current = null;
    setCredentialId(""); setRuntimeKey(""); setModel(""); setEffort(""); setAccessMode("supervised"); setInteractionMode("default");
    if (!threadId) setMode(defaultMode);
    const { draft, attachments: nextAttachments, restoredNeedsReselect } = hydrateComposerDraft(draftKey);
    if (draft) {
      const doc = promptDocumentFromDraft(draft);
      setPromptDocument(doc);
      setComposerUndoEpoch((n) => n + 1);
      setAttachments(nextAttachments);
      if (draft.credentialId) setCredentialId(draft.credentialId);
      if (draft.runtimeKey) setRuntimeKey(draft.runtimeKey);
      if (draft.model) setModel(draft.model);
      if (draft.effort !== undefined) setEffort(draft.effort);
      if (draft.accessMode) setAccessMode(draft.accessMode);
      if (draft.interactionMode) setInteractionMode(normalizeInteractionMode(draft.interactionMode));
      if (!threadId) {
        setProjectId(draft.projectId || "");
        pendingProjectIdRef.current = "";
        restoredBindingContextRef.current = { draftKey, projectId: draft.projectId || "" };
      }
      if (restoredNeedsReselect && !desktopChatBridge()) {
        setNotice("部分附件无法恢复本地文件，请重新选择后再发送");
      }
    } else {

      setPromptDocument(emptyPromptDocument());
      setComposerUndoEpoch((n) => n + 1);

      setAttachments([]);
      if (!threadId) {
        const pendingProjectId = pendingProjectIdRef.current;
        if (pendingProjectId) {
          setProjectId(pendingProjectId);
          pendingProjectIdRef.current = "";
        } else {
          setProjectId("");
        }
        const recent = readChatLastSelection();
        setCredentialId(recent?.credentialId || "");
        setRuntimeKey(recent?.runtimeKey || "");
        setModel(recent?.modelId || "");
        setEffort(recent?.effort || "");
        setAccessMode("supervised");
        setInteractionMode("default");
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
      hydratedDraftRef.current = { draftKey, storageScope };
      setHydratedDraftKey(draftKey);
    }, 0);
    return () => window.clearTimeout(release);
  }, [draftKey, threadId, storageScope, draftRuntimeFields, defaultMode]);

  useEffect(() => {
    if (!desktopChatBridge()) return;
    const { attachments: nextAttachments } = hydrateComposerDraft(draftKey);
    let restoring = true;
    void restoreComposerDraftAttachments(draftKey).then((restored) => {
      if (!restoring || composerSnapshotRef.current.draftKey !== draftKey) return;
      if (restored.restoredNeedsReselect) setNotice("部分附件无法从桌面缓存恢复，请重新选择后再发送");
      setAttachments((current) => current.map((item) => {
        const original = nextAttachments.find((entry) => entry.id === item.id);
        const replacement = restored.attachments.find((entry) => entry.id === item.id);
        return original && !item.file && replacement?.file && item.name === original.name ? { ...item, ...replacement } : item;
      }));
    });
    return () => { restoring = false; };
  }, [draftKey, storageScope, setNotice]);

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
      interactionMode,
      projectId,
    };
    if (draftHydratingRef.current || !storageScope || composerStorageScopeRef.current !== conversationStorageScope()) return;
    writeComposerDraft(draftKey, {
      prompt,
      promptSegments: promptDocument.segments,
      capabilityRefs,
      attachments,
      ...draftRuntimeFields(draftKey, { credentialId, runtimeKey, model, effort, accessMode }),
      interactionMode,
      projectId,
    });
  }, [
    draftRuntimeFields,
    accessMode,
    interactionMode,
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
    storageScope,
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
    applyingHistoryRef.current = true;
    const doc = promptDocumentFromDraft({
      prompt: input.prompt,
      promptSegments: input.promptSegments,
      capabilityRefs: input.capabilityRefs,
    });
    setPromptDocument(doc);
    applyingHistoryRef.current = false;
  }, []);

  const handlePromptChange = useCallback((next: string) => {
    if (!applyingHistoryRef.current) exitHistoryBrowse();
    setPromptDocument((current) => documentFromPromptAndRefs(next, refsFromDocument(current)));
  }, [exitHistoryBrowse]);

  const handlePromptDocumentChange = useCallback((doc: PromptDocument) => {
    if (!applyingHistoryRef.current) exitHistoryBrowse();
    setPromptDocument(doc);
  }, [exitHistoryBrowse]);

  const handleHistoryNavigate = useLatestCallback((direction: "older" | "newer"): boolean => {
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
  });

  const openStashPanel = useLatestCallback(() => {
    refreshStashes();
    setStashName(defaultStashName(plainTextFromDocument(promptDocument)));
    setStashOpen(true);
  });

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
    setNotice(`已暂存「${saved.name}」`, "success");
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

  const [pendingStashRestore, setPendingStashRestore] = useState<{ stashId: string; draftKey: string; scope: string } | null>(null);
  const performStashRestore = useCallback((stashId: string) => {
    const restored = hydrateComposerStash(stashId, { draftKey, projectId });
    if (!restored) return;
    applyingHistoryRef.current = true;
    historyIndexRef.current = null;
    setHistoryIndex(null);
    historyScratchRef.current = null;
    let doc = promptDocumentFromDraft(restored.entry.snapshot);
    const crossProject = Boolean(threadId && restored.environmentMismatch);
    if (crossProject) doc = { ...doc, nodes: Object.fromEntries(Object.entries(doc.nodes).map(([id, node]) => [id, { ...node, status: "stale" as const, status_reason: "暂存来自其他项目，请按当前会话工作区重新选择引用" }])) };
    setPromptDocument(doc);
    setAttachments(crossProject ? restored.attachments.map((item) => ({ ...item, artifactId: undefined, needsReselect: !item.file })) : restored.attachments);
    const targetProject = restored.entry.projectId || restored.entry.snapshot.projectId || "";
    const projectStillThere = Boolean(
      targetProject && projects.some((row) => row.project_id === targetProject),
    );
    if (!threadId && projectStillThere) setProjectId(targetProject);
    const restoredBinding = resolveComposerBindingRestore({
      snapshot: restored.entry.snapshot,
      current: { credentialId, runtimeKey, model, effort, accessMode },
      credentials: credentialRows,
      runtimes,
      descriptors,
    });
    restoredBindingContextRef.current = { draftKey, projectId: !threadId && projectStillThere ? targetProject : projectId };
    if (restoredBinding.restored) {
      const selection = restoredBinding.selection;
      setCredentialId(selection.credentialId);
      setRuntimeKey(selection.runtimeKey);
      setModel(selection.model);
      setEffort(selection.effort);
      setAccessMode(selection.accessMode);
    }
    const warnings: string[] = [];
    if (crossProject) warnings.push("当前对话工作区保持原归属；旧引用需重新选择，附件需重新上传或选择");
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
  }, [threadId, draftKey, projectId, projects, credentialRows, runtimes, descriptors, credentialId, runtimeKey, model, effort, accessMode, setNotice]);

  const handleStashRestore = useCallback((stashId: string) => {
    if (!composerRecallBodyEmpty({ prompt, promptSegments: promptDocument.segments, capabilityRefs, attachments })) {
      setPendingStashRestore({ stashId, draftKey, scope: conversationStorageScope() }); return;
    }
    performStashRestore(stashId);
  }, [prompt, promptDocument.segments, capabilityRefs, attachments, draftKey, performStashRestore]);
  const confirmStashRestore = (saveCurrent: boolean) => {
    const pending = pendingStashRestore;
    if (!pending || pending.draftKey !== draftKey || pending.scope !== conversationStorageScope()) { setPendingStashRestore(null); return; }
    if (saveCurrent) {
      const saved = saveComposerStash({ name: `恢复前草稿 · ${new Date().toLocaleString()}`, draftKey, projectId, snapshot: { ...composerSnapshotRef.current, promptSegments: promptDocument.segments } });
      if (!saved || !flushComposerRecallStore().persisted) { setError("当前草稿尚未成功暂存，未替换输入内容；请修复保存后重试或复制正文"); return; }
      refreshStashes();
    }
    setPendingStashRestore(null); performStashRestore(pending.stashId);
  };

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

  const openDiffBaseline = useLatestCallback((request: DiffBaselineRequest) => {
    if (!threadId) return;
    setDetailsPayload(null);
    setRightPanelInstant(false);
    chatPanel.openDiff(threadId, request);
  });

  const workspaceRoot = conversation.view?.workspace?.root_path || "";
  const openFilePreview = useLatestCallback((request: { path: string; line?: number }) => {
    if (!threadId) return;
    setDetailsPayload(null);
    setRightPanelInstant(false);
    const root = workspaceRoot.replace(/\/+$/, "");
    const path = root && request.path.startsWith(`${root}/`) ? request.path.slice(root.length + 1) : request.path;
    chatPanel.openFile(sideChat && sideOf && !runSideChat ? sideOf.threadId : threadId, path, request.line);
  });

  const openToolFile = useCallback((file: ToolFile) => {
    if (file.kind === "workspace") {
      openFilePreview({ path: file.path, line: file.line ?? undefined });
      return;
    }
    openDetails({
      type: "artifact",
      title: file.name,
      artifact: file.kind === "artifact"
        ? { artifactId: file.artifact_id, name: file.name, mediaType: file.media_type }
        : { apiPath: toolFileUrl(threadId, file), name: file.name, mediaType: file.kind === "recorded" ? file.media_type : undefined },
    });
  }, [openDetails, openFilePreview, threadId]);

  const openAttachmentPreview = useCallback((attachment: MessageAttachmentChip) => {
    const reference = attachment.artifactId;
    if (!reference) return;
    openDetails({
      type: "artifact",
      title: attachment.name || "附件",
      subtitle: reference,
      artifact: {
        artifactId: attachment.artifactId,
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
    if (target.kind === "legacy_artifact") {
      const migrated = conversation.view?.artifacts?.find(item => item.sha256 === target.sha256 && item.artifact_id);
      if (migrated) {
        openDetails({ type: "artifact", title: migrated.name || "附件", subtitle: migrated.artifact_id,
          artifact: { artifactId: migrated.artifact_id, name: migrated.name, kind: migrated.kind, mediaType: migrated.media_type, size: migrated.size } });
        return;
      }
      openDetails({ type: "error", title: "旧版附件需重选", subtitle: target.sha256.slice(0, 12),
        error: "旧版附件只保留历史引用，请重新选择原文件。" });
      return;
    }
    if (target.kind === "artifact") {
      const matched = (conversation.view?.artifacts || []).find(
        (item) => {
          const reference = item.artifact_id;
          return typeof reference === "string" && (reference === target.artifactId
            || reference.startsWith(target.artifactId)
            || target.artifactId.startsWith(reference.slice(0, 16)));
        },
      );
      openDetails({
        type: "artifact",
        title: matched?.name || target.artifactId.slice(0, 12),
        subtitle: target.artifactId,
        artifact: {
          artifactId: matched?.artifact_id || target.artifactId,
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

  const runtimeLoadSeqRef = useRef(0);
  const probeGenerationRef = useRef(0);
  const probeSelectionRef = useRef(runtimeKey);
  probeSelectionRef.current = runtimeKey;
  const probeCredentialRef = useRef(credentialId);
  probeCredentialRef.current = credentialId;
  const projectLoadSeqRef = useRef(0);
  const previewModeRef = useRef(Boolean(preview));
  previewModeRef.current = Boolean(preview);
  const metadataScopeRef = useRef(storageScope);
  useEffect(() => {
    if (metadataScopeRef.current === storageScope) return;
    metadataScopeRef.current = storageScope;
    ++runtimeLoadSeqRef.current; ++credentialLoadSeqRef.current; ++projectLoadSeqRef.current;
    setRuntimes([]); setCredentials([]); setProjects([]);
    setRuntimeSource(sourceLoading()); setCredentialSource(sourceLoading()); setProjectSource(sourceLoading());
  }, [storageScope]);
  useEffect(() => { ++probeGenerationRef.current; setProjectCreating(false); setPendingSendQuery(false); setProbing(false); }, [commandScope]);
  const loadRuntimes = useCallback(async (opts?: { silent?: boolean }) => {
    const requestSeq = ++runtimeLoadSeqRef.current;
    if (previewModeRef.current) { setRuntimeSource(sourceOk()); return; }
    const ownerScope = conversationStorageScope();
    if (!opts?.silent) setRuntimeSource(sourceLoading());
    try {
      const rows = await fetchRuntimeInstances();
      if (requestSeq !== runtimeLoadSeqRef.current || ownerScope !== conversationStorageScope()) return;
      setRuntimes(rows.filter((row) => row.enabled !== false));
      setRuntimeSource(sourceOk());
    } catch (exc) {
      if (requestSeq !== runtimeLoadSeqRef.current || ownerScope !== conversationStorageScope()) return;
      const message = exc instanceof Error ? exc.message : String(exc);
      const httpStatus = (exc as { httpStatus?: number })?.httpStatus ?? parseHttpStatus(message);
      setRuntimeSource(sourceError(message, httpStatus));
    }
  }, []);

  const loadCredentials = useCallback(async (opts?: { fresh?: boolean; silent?: boolean }) => {
    const requestSeq = ++credentialLoadSeqRef.current;
    if (previewModeRef.current) { setCredentialSource(sourceOk()); return; }
    const ownerScope = conversationStorageScope();
    if (!opts?.silent) setCredentialSource(sourceLoading());
    try {
      const rows = await fetchConversationCredentials({ fresh: opts?.fresh });
      if (requestSeq !== credentialLoadSeqRef.current || ownerScope !== conversationStorageScope()) return;
      setCredentials(rows);
      setCredentialId((curr) => {
        if (curr && rows.some((r) => r.id === curr)) return curr;
        return resolvePreferredChatSelection(rows)?.credentialId || rows[0]?.id || "";
      });
      setCredentialSource(sourceOk());
    } catch (exc) {
      if (requestSeq !== credentialLoadSeqRef.current || ownerScope !== conversationStorageScope()) return;
      const message = exc instanceof Error ? exc.message : String(exc);
      const httpStatus = (exc as { httpStatus?: number })?.httpStatus ?? parseHttpStatus(message);
      setCredentialSource(sourceError(message, httpStatus));
    }
  }, []);

  const loadProjects = useCallback(async (opts?: { silent?: boolean }) => {
    const requestSeq = ++projectLoadSeqRef.current;
    if (previewModeRef.current) { setProjectSource(sourceOk()); return; }
    const ownerScope = conversationStorageScope();
    if (!opts?.silent) setProjectSource(sourceLoading());
    try {
      const rows = await fetchConversationProjects({ scope: projectScope });
      if (requestSeq !== projectLoadSeqRef.current || ownerScope !== conversationStorageScope()) return;
      setProjects(rows);
      setProjectSource(sourceOk());
    } catch (exc) {
      if (requestSeq !== projectLoadSeqRef.current || ownerScope !== conversationStorageScope()) return;
      const message = exc instanceof Error ? exc.message : String(exc);
      const httpStatus = (exc as { httpStatus?: number })?.httpStatus ?? parseHttpStatus(message);
      setProjectSource(sourceError(message, httpStatus));
    }
  }, [projectScope]);

  useEffect(() => {
    if (threadId || projectSource.phase !== "ok" || !projectId) return;
    if (!projects.some((row) => row.project_id === projectId)) {
      setProjectId("");
    }
  }, [threadId, projectSource.phase, projectId, projects]);

  const reloadReadiness = useCallback(async (opts?: { fresh?: boolean; silent?: boolean }) => {
    if (descriptorState.status === "error") await loadProviderDescriptors({ fresh: true }).catch(() => undefined);
    await Promise.all([
      loadCredentials({ fresh: opts?.fresh, silent: opts?.silent }),
      loadRuntimes({ silent: opts?.silent }),
      loadProjects({ silent: opts?.silent }),
    ]);
  }, [descriptorState.status, loadCredentials, loadProjects, loadRuntimes]);

  const completedModelTurnSeq = useConversationStreamSelector(
    conversation.stream,
    streamThreadId,
    (snapshot) => snapshot.events.findLast((event) => event.event_type === "core.turn.completed")?.seq,
  );
  useEffect(() => {
    if (completedModelTurnSeq === undefined) return;
    void loadCredentials({ silent: true });
  }, [threadId, completedModelTurnSeq, loadCredentials]);

  useEffect(() => {
    void loadRuntimes({ silent: true });
  }, [loadRuntimes, storageScope]);
  useEffect(() => {
    void loadCredentials({ silent: true });
  }, [loadCredentials, storageScope]);
  useEffect(() => {
    void loadProjects({ silent: true });
  }, [loadProjects, storageScope]);

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

  const planModeSupport = useMemo(
    () => planModeSupportForRuntime({
      catalogReady: descriptorState.status === "ready",
      runtimeKey,
      planMode: capabilitiesFor(descriptors, runtimeKey)?.plan_mode,
    }),
    [descriptorState.status, descriptors, runtimeKey],
  );
  const commitInteractionMode = useCallback((mode: InteractionMode) => {
    setInteractionMode(mode);
    const snap = composerSnapshotRef.current;
    composerSnapshotRef.current = { ...snap, interactionMode: mode };
    if (draftHydratingRef.current || !snap.draftKey) return;
    writeComposerDraft(snap.draftKey, {
      prompt: snap.prompt,
      promptSegments: snap.promptDocument.segments,
      capabilityRefs: snap.capabilityRefs,
      attachments: snap.attachments,
      ...draftRuntimeFields(snap.draftKey, snap),
      interactionMode: mode,
      projectId: snap.projectId,
    });
  }, [draftRuntimeFields]);
  const handleInteractionModeChange = useCallback((mode: InteractionMode) => {
    if (mode === "plan" && !planModeSupport.available) return;
    const previous = interactionMode;
    commitInteractionMode(mode);
    if (!threadId || mode === previous) return;
    const stored = conversation.view?.runtime;
    const storedKey = stored?.adapter_id ? `${stored.adapter_id}:${stored.instance_id}` : "";
    if (mode === "plan" && storedKey !== runtimeKey) return;
    void saveThreadInteractionMode(threadId, mode).catch((failure) => {
      if (threadIdRef.current !== threadId) return;
      commitInteractionMode(previous);
      setError(failure instanceof Error ? failure.message : String(failure));
    });
  }, [commitInteractionMode, conversation.view?.runtime, interactionMode, planModeSupport.available, runtimeKey, setError, threadId]);

  const viewThreadId = conversation.view?.thread.thread_id || "";
  const rawViewRuntime = conversation.view?.runtime;
  const isPlaceholderRunRuntime = runSideChat
    && !conversation.view?.turns?.length
    && rawViewRuntime?.adapter_id === "openai"
    && rawViewRuntime?.instance_id === "text";
  const viewRuntime = isPlaceholderRunRuntime ? undefined : rawViewRuntime;
  const viewRuntimeKey = viewRuntime
    ? `${viewRuntime.adapter_id}:${viewRuntime.instance_id}`
    : "";
  const viewCredentialId = viewRuntime?.credential_id || viewRuntime?.credential_ref || "";
  const viewModel = viewRuntime?.model || "";
  const viewEffort = viewRuntime?.effort || "";
  const viewBound = Boolean(viewRuntime?.adapter_id);
  const viewServiceTier = viewRuntime?.service_tier || "";
  const viewAccessMode = viewRuntime?.access_mode || "supervised";
  const viewInteractionMode = viewRuntime?.interaction_mode || "";
  const viewRuntimeUpdatedAt = viewRuntime?.updated_at || "";
  const viewMode = conversation.view?.thread.mode;
  const viewProjectId = conversation.view?.thread.project_id || "";
  useEffect(() => {
    if (!viewThreadId || viewThreadId !== threadId) return;
    const saved = readComposerDraft(composerDraftKey(viewThreadId));
    const serverChangedAt = Date.parse(viewRuntimeUpdatedAt);
    const draftChosenAt = saved ? (saved.runtimeUpdatedAt || saved.updatedAt) : 0;
    const stored = saved && viewBound && Number.isFinite(serverChangedAt) && draftChosenAt < serverChangedAt
      ? null
      : saved;
    if (runSideChat && viewBound) {
      const sameTargetDraft = stored && stored.runtimeKey === viewRuntimeKey && stored.model === viewModel ? stored : null;
      setRuntimeKey(viewRuntimeKey);
      setCredentialId(viewCredentialId);
      setModel(viewModel);
      setEffort(sameTargetDraft?.effort ?? viewEffort);
      setServiceTier(viewServiceTier);
      setAccessMode(sameTargetDraft?.accessMode || viewAccessMode);
      if (viewMode) setMode(viewMode);
      setProjectId(viewProjectId);
      runtimeSyncedKeyRef.current = composerDraftKey(viewThreadId);
      return;
    }
    const bind = resolveComposerRuntimeBind({
      draftCredentialId: stored?.credentialId,
      draftModel: stored?.model,
      draftRuntimeKey: stored?.runtimeKey,
      viewCredentialId,
      viewModel,
      viewRuntimeKey,
      credentials,
      runtimes: effectiveRuntimes,
      savedDefault: readChatDefaultModel(),
    });
    setRuntimeKey(bind.runtimeKey);
    setCredentialId(bind.credentialId);
    setModel(bind.modelId);
    setEffort(stored?.effort ?? viewEffort);
    if (viewBound) setServiceTier(viewServiceTier);
    setAccessMode(stored?.accessMode || viewAccessMode);
    if (viewMode) setMode(viewMode);
    setProjectId(viewProjectId);
    runtimeSyncedKeyRef.current = composerDraftKey(viewThreadId);
  }, [
    viewRuntimeUpdatedAt,
    threadId,
    credentials,
    effectiveRuntimes,
    runSideChat,
    viewAccessMode,
    viewInteractionMode,
    descriptorState.status,
    descriptors,
    viewBound,
    viewCredentialId,
    viewEffort,
    viewServiceTier,
    viewMode,
    viewModel,
    viewProjectId,
    viewRuntimeKey,
    viewThreadId,
  ]);

  useEffect(() => {
    if (!viewBound || !viewThreadId || viewThreadId !== threadId) return;
    setInteractionMode(normalizeInteractionMode(viewInteractionMode));
  }, [threadId, viewBound, viewThreadId, viewRuntimeKey, viewInteractionMode]);

  const memoryRequestSeqRef = useRef(0);
  const loadMemory = useCallback(async () => {
    const request = ++memoryRequestSeqRef.current;
    const ownerThread = threadId;
    const ownerScope = conversationStorageScope();
    setMemory(null);
    if (preview) { setMemoryLoading(false); return; }
    if (!ownerThread) { setMemoryLoading(false); return; }
    setMemoryLoading(true);
    try {
      const snapshot = await fetchConversationMemory(ownerThread);
      if (request !== memoryRequestSeqRef.current || activeSendContextRef.current.draftKey !== composerDraftKey(ownerThread) || ownerScope !== conversationStorageScope()) return;
      setMemory(snapshot);
    } catch (error) {
      if (request === memoryRequestSeqRef.current && ownerScope === conversationStorageScope() && activeSendContextRef.current.draftKey === composerDraftKey(ownerThread)) setError(error instanceof Error ? error.message : String(error));
    } finally {
      if (request === memoryRequestSeqRef.current && ownerScope === conversationStorageScope()) setMemoryLoading(false);
    }
  }, [preview, threadId, setError]);

  useEffect(() => {
    if (!infoDrawerOpen) return;
    const sequence = memoryRequestSeqRef;
    void loadMemory();
    return () => { ++sequence.current; };
  }, [infoDrawerOpen, loadMemory, storageScope]);

  const selectedCredential = useMemo(
    () => credentials.find((c) => c.id === credentialId),
    [credentialId, credentials],
  );

  const selectedRuntime = useMemo(
    () => effectiveRuntimes.find((r) => r.key === runtimeKey),
    [runtimeKey, effectiveRuntimes],
  );

  const selectedProject = useMemo(
    () => projects.find((project) => project.project_id === projectId),
    [projectId, projects],
  );

  const ctfBound = Boolean((conversation.view?.thread as { ctf_binding?: unknown } | undefined)?.ctf_binding);
  const policyMode = (mode === "src" || experience?.defaultMode === "src")
    ? "src" as const
    : ctfBound ? "ctf" as const : undefined;

  const activeRunTarget = useMemo(() => {
    if (!runSideChat) return undefined;
    const tid = conversation.view?.thread.side_chat?.target_id;
    return tid ? runTargets.find((t) => t.target_id === tid) : undefined;
  }, [runSideChat, conversation.view?.thread.side_chat?.target_id, runTargets]);

  const runTargetAllowsDefaultModel = Boolean(
    runSideChat && (activeRunTarget ? !activeRunTarget.model || !model : viewBound && !model),
  );

  const readiness = useMemo(() => {
    const base = classifyConversationReadiness({
      sources: {
        credentials: credentialSource,
        runtimes: runtimeSource,
        projects: projectSource,
        descriptors: descriptorSource,
      },
      credentials,
      runtimes: effectiveRuntimes,
      projects,
      descriptors,
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
      policyMode,
    });
    if (!runTargetAllowsDefaultModel) return base;
    const blockers = base.blockers.filter((blocker) => blocker.kind !== "model_unavailable");
    if (blockers.length === base.blockers.length && base.envReady) return base;
    const envReady = blockers.every((blocker) => ![
      "not_installed",
      "version_unsupported",
      "not_logged_in",
      "model_unavailable",
      "runtime_unhealthy",
    ].includes(blocker.kind));
    return {
      ...base,
      blockers,
      envReady,
      canSend: blockers.length === 0,
    };
  }, [
    credentialId,
    credentialSource,
    credentials,
    descriptorSource,
    descriptors,
    directoryIssue,
    effectiveRuntimes,
    guideDismissed,
    model,
    policyMode,
    probing,
    projectId,
    projectSource,
    projects,
    runTargetAllowsDefaultModel,
    runtimeKey,
    runtimeSource,
    threadId,
  ]);

  const handleDismissGuide = useCallback(() => {
    setGuideDismissed(true);
    try {
      window.localStorage.setItem(GUIDE_DISMISSED_STORAGE_KEY, "1");
    } catch {
    }
  }, []);

  const handleProbeAgent = useCallback(async () => {
    const target = selectedRuntime
      || (selectedCredential ? runtimeForEngine(selectedCredential.engine, effectiveRuntimes, descriptors) : undefined);
    if (!target?.key || probing) return;
    const generation = ++probeGenerationRef.current;
    const ownerScope = conversationStorageScope();
    const ownerSelection = runtimeKey;
    const ownerCredential = selectedCredential?.id || "";
    const isCurrent = () => generation === probeGenerationRef.current && ownerScope === conversationStorageScope() && ownerSelection === probeSelectionRef.current && ownerCredential === probeCredentialRef.current;
    setProbing(true);
    setError("");
    try {
      await probeRuntimeInstance(target.key, runtimeScopesModelCatalog(descriptors, target.key)
        ? { credentialId: ownerCredential, environment: "local" } : {});
      if (!isCurrent()) return;
      await Promise.all([
        loadRuntimes({ silent: true }), loadCredentials({ silent: true }),
        loadProviderDescriptors({ fresh: true }).catch(() => undefined),
      ]);
    } catch (exc) {
      if (!isCurrent()) return;
      setError(exc instanceof Error ? exc.message : String(exc));
      await Promise.all([loadRuntimes({ silent: true }), loadCredentials({ silent: true })]);
    } finally {
      if (generation === probeGenerationRef.current) setProbing(false);
    }
  }, [descriptors, effectiveRuntimes, loadCredentials, loadRuntimes, probing, selectedCredential, selectedRuntime, runtimeKey, setError]);

  const handleCreateProjectFromPath = useCallback(async (rawPath: string): Promise<string | null> => {
    if (projectCreating) return null;
    const path = rawPath.trim();
    if (!path) {
      throw new Error("请输入有效的工作目录路径");
    }
    setProjectCreating(true);
    const requestSeq = ++projectLoadSeqRef.current;
    const ownerScope = conversationStorageScope();
    const ownerDraft = draftKey;
    const isCurrent = () => requestSeq === projectLoadSeqRef.current && ownerScope === conversationStorageScope() && ownerDraft === composerSnapshotRef.current.draftKey;
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
        scope: projectScope,
      });
      if (!isCurrent()) return createdProjectId;
      const rows = await fetchConversationProjects({ scope: projectScope });
      if (!isCurrent()) return createdProjectId;
      const selectedProject = rows.find((project) => project.project_id === createdProjectId);
      setProjects(rows);
      setProjectSource(sourceOk());
      setProjectId(createdProjectId);
      setNotice(reused
        ? `已选择项目 ${selectedProject?.name || name}`
        : `项目 ${name} 已添加`, "success");
      setRecentPaths(rememberRecentPath(rootPath || selectedProject?.root_path || path, [path]));
      setPreferPathInput(false);
      return createdProjectId;
    } finally {
      if (isCurrent()) setProjectCreating(false);
    }
  }, [projectScope, projectCreating, projects, draftKey, setError, setNotice]);

  const composerCapabilityContext = useMemo<ComposerCapabilityContext | undefined>(() => {
    const adapterId = selectedRuntime?.adapter_id || "";
    if (!adapterId) return undefined;
    return {
      threadId: threadId || undefined,
      adapterId,
      instanceId: selectedRuntime?.instance_id || "default",
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
    selectedRuntime?.instance_id,
    threadId,
  ]);

  const invalidateComposerReferences = useCallback((reason: string) => {
    setPromptDocument((current) => {
      const nodes = Object.fromEntries(Object.entries(current.nodes).map(([id, node]) => [id, { ...node, status: "stale" as const, status_reason: reason }]));
      const next = { ...current, nodes };
      return next;
    });
  }, []);

  const handleSelectModelParams = useCallback((params: {
    credentialId: string;
    model: string;
    effort?: string;
    serviceTier?: string;
    accessMode?: string;
  }) => {
    const credential = credentials.find((item) => item.id === params.credentialId);
    const extensionRuntime = !params.credentialId && selectedRuntime?.origin === "extension" ? selectedRuntime : undefined;
    const runtime = extensionRuntime || (credential
      ? effectiveRuntimes.find((row) => row.key === runtimeKey && row.engine === credential.engine && runtimeAvailableForSelection(row, descriptors))
        || runtimeForEngine(credential.engine, effectiveRuntimes, descriptors)
      : undefined);
    const engineChanged = Boolean(
      credential
      && selectedCredential
      && credential.engine !== selectedCredential.engine,
    );
    const targetModel = credential ? allCredentialModels(credential).find((row) => row.id === params.model) : undefined;
    const bindingChanged = params.credentialId !== credentialId || params.model !== model || (runtime?.key || "") !== runtimeKey;
    const memoryScope = `${params.credentialId}:${runtime?.key || ""}`;
    const requestedEffort = params.effort ?? (bindingChanged ? rememberedModelEffort(memoryScope, targetModel) : effort);
    const nextEffort = validModelEffort(targetModel, requestedEffort) ? requestedEffort : "";
    if (params.effort !== undefined && targetModel) rememberModelEffort(memoryScope, targetModel, nextEffort);
    setCredentialId(params.credentialId);
    setModel(params.model);
    setRuntimeKey(runtime?.key || "");
    setEffort(nextEffort);
    if (params.serviceTier !== undefined) setServiceTier(params.serviceTier);
    if (params.accessMode !== undefined) setAccessMode(params.accessMode);
    restoredBindingContextRef.current = { draftKey, projectId };
    if (!sideChat) {
      writeChatLastSelection({
        credentialId: params.credentialId, modelId: params.model,
        runtimeKey: runtime?.key || "", effort: nextEffort,
        accessMode: params.accessMode ?? accessMode,
      });
    }
    if (engineChanged) {
      invalidateComposerReferences("Agent 引擎已改变，请在当前引擎重新选择引用");
      setNotice("原引用与快照已保留，但未验证新引擎能使用；发送前请重新选择或明确移除引用", "warning");
    }
  }, [credentials, credentialId, model, descriptors, effectiveRuntimes, runtimeKey, selectedCredential, selectedRuntime, invalidateComposerReferences, draftKey, projectId, effort, accessMode, sideChat, setNotice]);

  const runtimePickerOwnerRef = useRef({ storageScope, draftKey, credentialId });
  runtimePickerOwnerRef.current = { storageScope, draftKey, credentialId };
  const handleRuntimeChange = useCallback((nextKey: string) => {
    const owner = runtimePickerOwnerRef.current;
    if (owner.storageScope !== storageScope || conversationStorageScope() !== storageScope
      || owner.draftKey !== draftKey || owner.credentialId !== credentialId) return;
    const credential = effectiveCredentialRows.find((row) => row.id === credentialId);
    const runtime = effectiveRuntimes.find((row) => row.key === nextKey && runtimeAvailableForSelection(row, descriptors) && row.engine === credential?.engine);
    if (!credential || !runtime || nextKey === runtimeKey) return;
    const scoped = credentialForRuntime(credential, nextKey, runtimeScopesModelCatalog(descriptors, nextKey));
    const selectedModel = allCredentialModels(scoped).find((row) => row.id === model);
    const nextModel = selectedModel?.id || "";
    const nextEffort = validModelEffort(selectedModel, effort) ? effort : "";
    ++probeGenerationRef.current;
    setProbing(false);
    setRuntimeKey(nextKey); setModel(nextModel); setEffort(nextEffort);
    restoredBindingContextRef.current = { draftKey, projectId };
    if (!sideChat) {
      writeChatLastSelection({ credentialId, runtimeKey: nextKey, modelId: nextModel,
        effort: nextEffort, accessMode });
    }
    invalidateComposerReferences("Runtime 已改变，请在当前 Runtime 重新选择引用");
    setNotice("已选择 Runtime；下一次发送使用此接入，已有对话记录和草稿已保留");
  }, [storageScope, draftKey, credentialId, effectiveCredentialRows, effectiveRuntimes, descriptors, runtimeKey, model, effort,
    projectId, accessMode, sideChat, invalidateComposerReferences, setNotice]);

  const handleProjectChange = useCallback((nextProjectId: string) => {
    restoredBindingContextRef.current = null;
    appliedNewChatDefaultsRef.current = "";
    setProjectId(nextProjectId);
    invalidateComposerReferences("工作项目已改变，请在当前项目重新选择引用");
    if (Object.keys(promptDocument.nodes).length) setNotice("旧项目引用和快照已保留，发送前请按新项目重新选择或移除", "warning");
    setWorkspaceMode("shared_checkout");
    setWorktreeBranch("");
    setExistingWorktreePath("");
  }, [invalidateComposerReferences, promptDocument.nodes, setNotice]);

  const handleWorkspaceModeChange = useCallback((mode: WorkspaceBindMode) => {
    setWorkspaceMode(mode);
    setExistingWorktreePath("");
    if (mode !== "new_worktree") setWorktreeBranch("");
  }, []);

  const projectConvDefaults = useMemo(
    () => readProjectConvDefaults(selectedProject?.settings as Record<string, unknown> | undefined),
    [selectedProject],
  );


  const handleSetProjectDefault = useCallback(async () => {
    if (!projectId || savingProjectDefault) return;
    if (!credentialId || !model) { setNotice("凭据或模型尚未就绪，暂不能设为项目默认", "warning"); return; }
    setSavingProjectDefault(true);
    setError("");
    setNotice("");
    try {
      const receipt = await updateConversationProjectSettings(
        projectId,
        projectConvDefaultsPayload({ credentialId, modelId: model, effort }),
      );
      if (receipt.state !== "completed") { setNotice("项目默认设置已受理，尚未确认完成；原设置继续有效", "accepted", receipt.command_id); return; }
      const feedback = projectDefaultPersistFeedback("set", { ok: true });
      setError(feedback.error);
      setNotice(feedback.notice, feedback.error ? "warning" : "success");
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
                },
              }
            : p,
        ));
      }
    } catch (exc) {
      const feedback = projectDefaultPersistFeedback("set", { ok: false, error: exc });
      setError(feedback.error);
      setNotice(feedback.notice, feedback.error ? "warning" : "success");
    } finally {
      setSavingProjectDefault(false);
    }
  }, [credentialId, effort, model, projectId, savingProjectDefault, setError, setNotice]);

  const handleClearProjectDefault = useCallback(async () => {
    if (!projectId || savingProjectDefault) return;
    setSavingProjectDefault(true);
    setError("");
    setNotice("");
    try {
      const receipt = await updateConversationProjectSettings(projectId, projectConvDefaultsPayload(null));
      if (receipt.state !== "completed") { setNotice("清除项目默认已受理，尚未确认完成；原设置继续有效", "accepted", receipt.command_id); return; }
      const feedback = projectDefaultPersistFeedback("clear", { ok: true });
      setError(feedback.error);
      setNotice(feedback.notice, feedback.error ? "warning" : "success");
      if (feedback.applyLocalProjects) {
        restoredBindingContextRef.current = null;
        setProjects((prev) => prev.map((p) => {
          if (p.project_id !== projectId) return p;
          const next = { ...(p.settings || {}) };
          delete next.conv_default_credential_id;
          delete next.conv_default_model;
          delete next.conv_default_effort;
          return { ...p, settings: next };
        }));
      }
    } catch (exc) {
      const feedback = projectDefaultPersistFeedback("clear", { ok: false, error: exc });
      setError(feedback.error);
      setNotice(feedback.notice, feedback.error ? "warning" : "success");
    } finally {
      setSavingProjectDefault(false);
    }
  }, [projectId, savingProjectDefault, setError, setNotice]);

  const handleRestoreProjectDefault = useLatestCallback(() => {
    if (!projectConvDefaults.credentialId || !credentials.some((c) => c.id === projectConvDefaults.credentialId)) {
      setNotice("项目默认的凭据已不可用，请重新选择后再设为项目默认", "warning");
      return;
    }
    handleSelectModelParams({
      credentialId: projectConvDefaults.credentialId,
      model: projectConvDefaults.modelId || "",
      effort: projectConvDefaults.effort ?? "",
    });
  });

  const projectDefaultControl = useMemo<ProjectDefaultControl | undefined>(
    () => projectId ? {
      value: projectConvDefaults,
      saving: savingProjectDefault,
      onSave: () => void handleSetProjectDefault(),
      onClear: () => void handleClearProjectDefault(),
      onRestore: handleRestoreProjectDefault,
    } : undefined,
    [projectId, projectConvDefaults, savingProjectDefault, handleSetProjectDefault, handleClearProjectDefault, handleRestoreProjectDefault],
  );

  type WorkspaceAccess = { modes: string[]; notes: Record<string, string>; error?: string };
  const workspaceAccessContextKey = JSON.stringify([storageScope, composerCapabilityContext]);
  const [workspaceAccess, setWorkspaceAccess] = useState<Record<string, WorkspaceAccess>>({});
  useEffect(() => {
    if (!composerCapabilityContext) return;
    const controller = new AbortController();
    void fetchComposerCapabilities(composerCapabilityContext, "/", "", controller.signal).then((catalog) => {
      if (!controller.signal.aborted) setWorkspaceAccess((current) => ({ ...current,
        [workspaceAccessContextKey]: { modes: catalog.accessModes, notes: catalog.accessModeNotes },
      }));
    }).catch((failure) => {
      if (!controller.signal.aborted) setWorkspaceAccess((current) => ({ ...current,
        [workspaceAccessContextKey]: { modes: [], notes: {}, error: failure instanceof Error ? failure.message : String(failure) },
      }));
    });
    return () => controller.abort();
  }, [composerCapabilityContext, workspaceAccessContextKey]);
  const prewarmWorkspaceId = !threadId && mode === "conversation" && workspaceMode === "shared_checkout"
    ? selectedProject?.workspace_id || "" : "";
  useEffect(() => {
    const adapterId = selectedRuntime?.adapter_id || "";
    const credential = selectedCredential?.id || "";
    if (!prewarmWorkspaceId || !adapterId || !credential || !model || hydratedDraftKey !== draftKey) return;
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      void prewarmConversationRuntime({
        adapter_id: adapterId,
        instance_id: selectedRuntime?.instance_id || "default",
        credential_id: credential,
        model,
        workspace_id: prewarmWorkspaceId,
      }, controller.signal).catch((failure) => {
        if (!controller.signal.aborted) console.warn("runtime prewarm failed", failure);
      });
    }, 800);
    return () => { window.clearTimeout(timer); controller.abort(); };
  }, [draftKey, hydratedDraftKey, model, prewarmWorkspaceId, selectedCredential?.id, selectedRuntime?.adapter_id, selectedRuntime?.instance_id]);
  const currentWorkspaceAccess = workspaceAccess[workspaceAccessContextKey];
  const accessModes = currentWorkspaceAccess?.modes || [];
  const accessModeReason = currentWorkspaceAccess?.notes[accessMode] || "已保存的权限模式在当前工作区不可用，请明确选择其他模式。";
  const accessModeIssue = accessModes.length > 0 && !accessModes.includes(accessMode)
    ? accessModeReason : "";

  useEffect(() => {
    if (threadId || !selectedCredential || draftHydratingRef.current || hydratedDraftKey !== draftKey) return;
    setRuntimeKey((current) => {
      const exact = effectiveRuntimes.find((row) => row.key === current && row.engine === selectedCredential.engine && runtimeAvailableForSelection(row, descriptors));
      return exact?.key || runtimeForEngine(selectedCredential.engine, effectiveRuntimes, descriptors)?.key || "";
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
  }, [descriptors, draftKey, hydratedDraftKey, effectiveRuntimes, selectedCredential, threadId]);

  useEffect(() => {
    if (threadId || hydratedDraftKey !== draftKey || draftHydratingRef.current) return;
    if (credentialSource.phase !== "ok" || runtimeSource.phase !== "ok" || !descriptors) return;
    if (projectId && projectSource.phase !== "ok") return;
    const restoredContext = restoredBindingContextRef.current;
    if (restoredContext?.draftKey === draftKey && restoredContext.projectId === projectId) return;
    const project = projects.find((p) => p.project_id === projectId);
    const projDefaults = readProjectConvDefaults(project?.settings as Record<string, unknown> | undefined);
    const contextKey = JSON.stringify([draftKey, projectId, projDefaults, defaultAccessMode]);
    if (appliedNewChatDefaultsRef.current === contextKey) return;
    const selection = resolveNewConversationSelection({
      credentials: effectiveCredentialRows, runtimes: effectiveRuntimes, project: projDefaults,
      recent: readChatLastSelection(), configured: readChatDefaultModel(),
      descriptors, preferDefaultAdapter: Boolean(desktopChatBridge()), globalAccessMode: defaultAccessMode,
    });
    if (!selection) return;
    appliedNewChatDefaultsRef.current = contextKey;
    setCredentialId(selection.credentialId);
    setRuntimeKey(selection.runtimeKey);
    setModel(selection.model);
    setEffort(selection.effort);
    setAccessMode(selection.accessMode);
  }, [effectiveCredentialRows, credentialSource.phase, defaultAccessMode, descriptors, draftKey, hydratedDraftKey, projectId, projects, projectSource.phase, effectiveRuntimes, runtimeSource.phase, threadId]);

  const threadMatrix = useMemo(
    () => matrixFromRuntimeConnection(conversation.view?.runtime_connection),
    [conversation.view?.runtime_connection],
  );

  const providerPreview = runSideChat ? "" : providerSwitchPreview({
    threadId,
    viewThreadId,
    boundRuntime: viewRuntime,
    credentialId,
    runtime: selectedRuntime,
    matrix: threadMatrix,
  });
  const providerPreviewKey = JSON.stringify([providerScope, viewRuntimeKey, viewCredentialId, providerPreview]);
  const providerSwitchNotice = preview ? "" : providerPreview
    ? (dismissedProviderPreview === providerPreviewKey ? "" : providerPreview)
    : (viewThreadId === threadId ? handoffNotice : "");

  const latestRuntimeHandoffNotice = useConversationStreamSelector(
    conversation.stream,
    streamThreadId,
    (snapshot) => {
      let latestTurnId = "";
      for (let index = snapshot.events.length - 1; index >= 0; index--) {
        const event = snapshot.events[index];
        const payload = (event.payload || {}) as Record<string, unknown>;
        if (event.event_type === "core.turn.started"
          && viewThreadId === threadId
          && conversation.view?.state.agent_session_id
          && payload.agent_session_id === conversation.view.state.agent_session_id) {
          latestTurnId ||= String(payload.turn_id || "");
        }
        if (event.event_type !== "core.thread.runtime_switched") continue;
        const update = runtimeHandoffNoticeUpdate({
          threadId,
          viewThreadId,
          runtimeKey,
          credentialId,
          model,
          sessionId: conversation.view?.state.agent_session_id,
          payload,
        });
        if (update !== null) {
          return update && latestTurnId && latestTurnId !== payload.handoff_delivery_turn_id ? "" : update;
        }
      }
      return latestTurnId ? "" : null;
    },
  );
  useEffect(() => {
    if (latestRuntimeHandoffNotice !== null) setHandoffNotice(latestRuntimeHandoffNotice);
  }, [
    latestRuntimeHandoffNotice, setHandoffNotice,
  ]);

  const running = Boolean(conversation.view?.state.running_turn_id);
  const backgroundTurnId = String(conversation.view?.runtime_connection?.background_turn_id || "");
  const steerEnabled = canInvoke(threadMatrix, "steer");
  const canSteer = Boolean(running && steerEnabled);
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
  const firstThreadLoadFailed = Boolean(
    threadId && !conversation.loading && !conversation.view,
  );
  const interactionBusy = busy || switchingThread;
  const threadLoadError = switchingThread ? conversation.error : "";
  const sessionSyncError = viewMatchesThread ? conversation.error : "";
  const lastTurnStatus = conversation.view?.turns.at(-1)?.status || "";
  const activeProjectName = useMemo(
    () => projects.find((project) => project.project_id === conversation.view?.thread.project_id)?.name,
    [conversation.view?.thread.project_id, projects],
  );
  const sidebarThreads = useMemo(() => {
    const view = conversation.view;
    const rows = view && view.thread.thread_id === threadId
      ? upsertConversationThread(threads, { ...view.thread, state: view.state })
      : threads;
    return rows.filter(threadFilter);
  }, [conversation.view, threadId, threads, threadFilter]);
  const sidebarProjects = useMemo(() => (
    experience?.hideProjects ? [] : projects
  ), [experience?.hideProjects, projects]);

  useEffect(() => {
    if (!threadId || conversation.loading || liveThreads.inboxStatus === "connected") return;
    if (conversation.view?.thread.thread_id !== threadId) return;
    void refreshList();
  }, [
    threadId,
    conversation.loading,
    liveThreads.inboxStatus,
    conversation.view?.thread.thread_id,
    conversation.view?.state.unread,
    conversation.view?.state.running_turn_id,
    conversation.view?.thread.metadata_revision,
    lastTurnStatus,
    refreshList,
  ]);

  const execute = useCallback(
    async (action: () => Promise<unknown>, failureKind: "command" | "send" = "command", targetScope = commandScope) => {
      if (commandScopesRef.current.has(targetScope)) return false;
      commandScopesRef.current.add(targetScope);
      commandBusyRef.current = commandScopesRef.current.has(commandScope);
      setCommandRevision((revision) => revision + 1);
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
        commandScopesRef.current.delete(targetScope);
        commandBusyRef.current = commandScopesRef.current.has(`${conversationStorageScope()}::${composerSnapshotRef.current.draftKey}`);
        setCommandRevision((revision) => revision + 1);
      }
    },
    [commandScope, conversation, refreshList, setError, setNotice],
  );

  const handleCreateProjectFromDirectory = useCallback(async (): Promise<string | null> => {
    if (desktopChatBridge()) {
      setPreferPathInput(true); setRequestPathInput(true);
      setDirectoryIssue({ message: "请明确选择桌面本机目录或输入服务宿主路径；本机目录须确认服务可访问后才能用作项目目录" });
      return null;
    }
    if (projectCreating) return null;
    setProjectCreating(true);
    const requestSeq = ++projectLoadSeqRef.current;
    const ownerScope = conversationStorageScope();
    const ownerDraft = draftKey;
    const isCurrent = () => requestSeq === projectLoadSeqRef.current && ownerScope === conversationStorageScope() && ownerDraft === composerSnapshotRef.current.draftKey;
    setError("");
    setNotice("");
    setDirectoryIssue(null);
    try {
      const selection = await selectConversationDirectory();
      if (!isCurrent()) return null;
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
        scope: projectScope,
      });
      if (!isCurrent()) return createdProjectId;
      const rows = await fetchConversationProjects({ scope: projectScope });
      if (!isCurrent()) return createdProjectId;
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
      if (!isCurrent()) return null;
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
      if (isCurrent()) setProjectCreating(false);
    }
  }, [projectScope, projectCreating, projects, draftKey, setError, setNotice]);

  const handleCreateRepoLessProject = useCallback(async (input: { name: string; description?: string }): Promise<string | null> => {
    const trimmedName = input.name.trim();
    if (!trimmedName || projectCreating) return null;
    setProjectCreating(true);
    const requestSeq = ++projectLoadSeqRef.current;
    const ownerScope = conversationStorageScope();
    const isCurrent = () => requestSeq === projectLoadSeqRef.current && ownerScope === conversationStorageScope();
    setError("");
    setNotice("");
    setRepoLessCreateError("");
    try {
      const { projectId: createdProjectId } = await createConversationProject({
        name: trimmedName,
        description: input.description?.trim() || undefined,
        scope: "src",
      });
      if (!isCurrent()) return createdProjectId;
      const rows = await fetchConversationProjects({ scope: "src" });
      if (!isCurrent()) return createdProjectId;
      setProjects(rows);
      setProjectSource(sourceOk());
      setProjectId(createdProjectId);
      setNotice(`研究项目 ${trimmedName} 已创建`, "success");
      return createdProjectId;
    } catch (exc) {
      if (!isCurrent()) return null;
      const message = exc instanceof Error ? exc.message : String(exc);
      setError(message);
      setRepoLessCreateError(message);
      return null;
    } finally {
      if (isCurrent()) setProjectCreating(false);
    }
  }, [projectCreating, setError, setNotice]);

  const openPendingSend = () => {
    setPendingSendDialog({ kind: "send", draftKey, scope: conversationStorageScope() }); setPendingSendStatus("");
    try { setArchivedSends(listArchivedSendIntents()); } catch (failure) { setPendingSendStatus(failure instanceof Error ? failure.message : String(failure)); }
  };
  const openLegacyRecovery = () => {
    setLegacyConfirmed(false); setLegacyRecoveryError("");
    try { setLegacyRecovery({ scope: conversationStorageScope(), draftKey, entries: listLegacyComposerEntries() }); }
    catch (failure) { setError(failure instanceof Error ? failure.message : String(failure)); }
  };
  const copyLegacyRecovery = (entry: LegacyComposerEntry) => {
    if (!legacyConfirmed || !legacyRecovery || legacyRecovery.scope !== conversationStorageScope() || legacyRecovery.draftKey !== composerSnapshotRef.current.draftKey) { setLegacyRecoveryError("当前服务或草稿已改变，请重新审查记录"); return; }
    const recovered = prepareLegacyComposerCopy(entry);
    const restoredDocument = promptDocumentFromDraft(recovered);
    const current = composerSnapshotRef.current.promptDocument;
    const merged = { nodes: { ...current.nodes, ...restoredDocument.nodes }, segments: [...current.segments, { type: "text" as const, text: "\n\n" }, ...restoredDocument.segments] };
    setPromptDocument(merged);
    setAttachments((rows) => [...rows, ...recovered.attachments.map((item) => ({ ...item, uploadStatus: "error" as const, uploadError: "旧记录附件须重新选择原文件", needsReselect: true }))]);
    setLegacyRecovery(null); setNotice("旧记录已追加到当前草稿。原记录保留；引用待重新选择、附件待重新选择，未复用旧命令或发送任何内容", "warning");
  };
  const queryPendingSend = async () => {
    const owner = pendingSendDialog;
    if (!owner || pendingSendQuery) return;
    const send = owner.kind === "send" ? readSendIntent(owner.draftKey) : null;
    const retry = owner.kind === "retry" ? readRetryIntent(owner.threadId, owner.turnId) : null;
    if (owner.kind === "send" ? !send : retry?.commandId !== owner.commandId) return;
    const commandId = owner.kind === "retry" ? owner.commandId : send!.pendingCommandId || (send!.pendingPhase === "creating_thread" || send!.phase === "creating_thread" ? send!.createCommandId : send!.sendCommandId);
    setPendingSendQuery(true);
    try {
      const receipt = await recoverCommandReceipt(commandId || "", 1);
      if (owner.scope !== conversationStorageScope() || owner.draftKey !== composerSnapshotRef.current.draftKey) return;
      const receiptCheck: PendingReceiptCheck = { checkedAt: Date.now(), result: receipt ? "received" : "missing", ...(receipt ? { receipt } : {}) };
      setPendingSendDialog(current => current === owner ? { ...current, receiptCheck } : current);
      setPendingSendStatus(receipt ? `原命令回执：${receipt.state}。当前操作未再次投递。` : "原命令暂时没有回执；这不能证明迟到请求永远不会被受理。");
      const result = classifyCommandReceipt(receipt);
      if (owner.kind === "retry") {
        if (result === "failed" || result === "completed") await updateRetryIntent(owner.threadId, owner.turnId, { phase: result === "completed" ? "accepted" : "failed", lastReceiptState: receipt?.state }, owner.commandId);
      } else if (result === "failed") await transitionSendIntent(owner.draftKey, { type: "failure", uncertain: false, state: receipt?.state }, send!.intentId);
    } catch (failure) {
      if (owner.scope !== conversationStorageScope() || owner.draftKey !== composerSnapshotRef.current.draftKey) return;
      const message = failure instanceof Error ? failure.message : String(failure);
      setPendingSendStatus(message);
      setPendingSendDialog(current => current === owner ? { ...current, receiptCheck: { checkedAt: Date.now(), result: "error", error: message } } : current);
    } finally { if (owner.scope === conversationStorageScope() && owner.draftKey === composerSnapshotRef.current.draftKey) setPendingSendQuery(false); }
  };
  const archivePendingSend = async () => {
    const owner = pendingSendDialog;
    if (!owner || owner.draftKey !== draftKey || owner.scope !== conversationStorageScope() || pendingSendQuery) return;
    setPendingSendQuery(true);
    try {
      if (owner.kind === "retry") {
        await archiveUnconfirmedRetry(owner.threadId, owner.turnId, owner.commandId, owner.receiptCheck);
        const prepared = await ensureRetryIntent(owner.threadId, owner.turnId);
        await updateRetryIntent(owner.threadId, owner.turnId, { previousCommandId: owner.commandId }, prepared.commandId);
        if (owner.scope !== conversationStorageScope() || owner.draftKey !== composerSnapshotRef.current.draftKey) return;
        setImpactCommandId(prepared.commandId); setArchivedRetries(listArchivedRetryIntents());
        setNotice("原重试的 ID、已知请求和回执核对信息已保留。新重试尚未投递，请确认影响后执行；原请求仍可能迟到执行。");
      } else {
        await archiveUnconfirmedSend(owner.draftKey, owner.receiptCheck); setArchivedSends(listArchivedSendIntents());
        setNotice("原请求已完整归档为待核对记录。当前草稿可以建立新请求；原请求仍可能迟到受理，请先确认内容。");
      }
      if (owner.scope !== conversationStorageScope() || owner.draftKey !== composerSnapshotRef.current.draftKey) return;
      setPendingSendDialog(null); setError("");
    } catch (failure) { if (owner.scope === conversationStorageScope() && owner.draftKey === composerSnapshotRef.current.draftKey) setPendingSendStatus(failure instanceof Error ? failure.message : String(failure)); }
    finally { if (owner.scope === conversationStorageScope() && owner.draftKey === composerSnapshotRef.current.draftKey) setPendingSendQuery(false); }
  };

  const handleClientCommand = async (action: string, args = "", sourceDocument = promptDocument) => {
    const name = action.replace(/^ui:/, "");
    const current = composerSnapshotRef.current;
    if (current.draftKey !== draftKey || storageScope !== conversationStorageScope() || fingerprintPromptDocument(current.promptDocument) !== fingerprintPromptDocument(sourceDocument)) { setNotice("命令来源已改变，请重新打开当前输入的命令菜单；当前草稿保留", "warning"); return; }
    setPromptDocument(emptyPromptDocument());
    if (name === "new") { startNewChat(projectId, true); return; }
    if (name === "help") {
      const text = `/${args}`;
      setPromptDocument(documentFromPromptAndRefs(text, []));
      requestAnimationFrame(() => shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
      return;
    }
    if (["model", "effort", "permissions"].includes(name)) {
      if (args && name === "model" && selectedCredential && allCredentialModels(selectedCredential).some((m) => m.id === args)) {
        handleSelectModelParams({ credentialId, model: args }); setNotice(`已选择模型 ${args}`); return;
      }
      if (name === "effort") {
        const models = selectedCredential ? allCredentialModels(selectedCredential) : [];
        const metadata = models.find(row => row.id === model);
        const thinking = modelThinkingControls(metadata, models, effort);
        const choice = args ? thinking.choices.find(choice => choice.value === args || choice.label.toLowerCase() === args.toLowerCase()) : undefined;
        if (choice) {
          handleSelectModelParams({ credentialId, model: choice.model?.id || model,
            effort: choice.model ? validModelEffort(choice.model, effort) ? effort : "" : choice.value });
          setNotice(`已设置思考强度 ${choice.label}`); return;
        }
        if (args === "default" && thinking.canReset) {
          const target = thinking.choices.find(choice => choice.value === thinking.defaultValue)?.model;
          handleSelectModelParams({ credentialId, model: target?.id || model, effort: "" }); return;
        }
        if (thinking.choices.length) composerModesRef.current?.openEffort();
        else { setNotice("当前模型没有可选的思考档位"); return; }
      } else if (name === "permissions") {
        if (accessModes.length) composerModesRef.current?.openAccess();
        else { setNotice("当前 Agent 没有可选的权限模式"); return; }
      }
      else { setModelPickerInstant(false); setModelPickerOpen(true); }
      if (args) setNotice("请在选择器中确认当前引擎支持的设置");
      return;
    }
    if (["skills", "plugins", "mcp"].includes(name)) {
      const engine = selectedCredential?.engine || "codex";
      const params = new URLSearchParams({ engine, source: name === "plugins" ? "managed" : "native", type: name === "skills" ? "skill" : name === "mcp" ? "mcp" : "all" });
      router.push(`/settings/agent-extensions?${params}`); return;
    }
    if (name === "sessions") { setCommandSessionsInstant(false); setSearchQuery(args); setCommandSessionsOpen(true); return; }
    if (!threadId || !conversation.view) { setNotice("请先开始一段聊天"); return; }
    if (sideChat && ["side", "btw", "fork", ...(runSideChat ? [] : ["rename"])].includes(name)) {
      setNotice(name === "rename" ? "侧边聊天不能重命名" : runSideChat && name !== "fork" ? "已在侧边聊天中，可用标签栏的 + 新建另一个" : "侧边聊天中不能再分叉或开启侧边聊天", "warning");
      return;
    }
    if (name === "side" || name === "btw") { void startSideChat(threadId, args.trim()); return; }
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
    if (name === "diff") { setRightPanelInstant(false); chatPanel.open(threadId, "diff"); return; }
    if (name === "details") { setBottomPanelInstant(false); setBottomPanelOpen(true); return; }
    if (name === "copy") {
      const message = [...conversation.view.messages].reverse().find((m) => m.role === "assistant" && m.text);
      setNotice(message && await copyToClipboard(message.text) ? "已复制助手回复" : "当前没有可复制的回复"); return;
    }
    setDetailsPayload({ type: "tool", title: "聊天状态与用量", output: {
      runtime: conversation.view.runtime, statistics: conversation.view.statistics, context: conversation.view.context_window,
    }});
  };

  type RapidSendItem = {
    intentKey: string;
    target: string;
    draftKey: string;
    text: string;
    promptDocument: PromptDocument;
    structuredRefs: ReturnType<typeof wireCapabilityRefs>;
    runtimePayload: {
      adapter_id: string;
      instance_id: string;
      credential_id: string;
      model: string;
      effort: string;
      service_tier: string;
      access_mode: typeof accessMode;
      interaction_mode: InteractionMode;
    };
    projectId: string;
    ownerScope: string;
  };
  const rapidSendQueueRef = useRef<RapidSendItem[]>([]);
  const rapidSendDrainingRef = useRef(false);

  const validateSendRuntime = (
    overrideRuntime?: Parameters<typeof runtimeUsable>[0],
  ) => {
    const extensionRuntime = selectedRuntime?.origin === "extension";
    const cred = extensionRuntime && selectedCredential?.engine !== selectedRuntime?.engine ? undefined : selectedCredential;
    if (!extensionRuntime && (!cred || !credentialAvailable(cred))) {
      throw new Error("请选择可用的 Agent 接入点；可前往设置 → Agents 配置");
    }
    if (!extensionRuntime && !runTargetAllowsDefaultModel && (!model || !cred || !allCredentialModels(cred).some((m) => m.id === model))) {
      throw new Error("请选择模型；可前往设置 → Agents 补充模型列表");
    }
    if (!extensionRuntime && !validModelEffort(cred ? allCredentialModels(cred).find((item) => item.id === model) : undefined, effort)) {
      throw new Error("当前 Agent 和模型不支持已选的思考程度，请重新选择或切回默认");
    }
    if (!extensionRuntime && selectedRuntime && selectedRuntime.engine !== cred?.engine) {
      throw new Error("模型接入点与 Runtime 引擎不一致，请重新选择模型后发送");
    }
    const rt = overrideRuntime || selectedRuntime || (cred ? runtimeForEngine(cred.engine, effectiveRuntimes, descriptors) : undefined);
    if (!rt) {
      throw new Error(`${cred?.engine || "Agent"} 没有可用的 Agent 接入；请前往设置检查 Runtime`);
    }
    const needsProbe = !runtimeUsable(rt) && isRuntimeUnprobed(rt);
    if (!needsProbe && !runtimeUsable(rt)) {
      throw new Error(
        rt.health?.detail
          ? `${cred?.engine || rt.adapter_id} 接入异常：${rt.health.detail}`
          : `${cred?.engine || rt.adapter_id} 没有可用的 Agent 接入；请前往设置检查 Runtime`,
      );
    }
    const runtimePayload = {
      adapter_id: rt.adapter_id,
      instance_id: rt.instance_id,
      credential_id: cred?.id || "",
      model,
      effort: effort === "default" ? "" : effort,
      service_tier: cred ? effectiveServiceTier(allCredentialModels(cred).find((item) => item.id === model), serviceTier) : "",
      access_mode: accessMode,
      interaction_mode: interactionMode,
    };
    const sendPlanSupport = planModeSupportForRuntime({
      catalogReady: descriptorState.status === "ready",
      runtimeKey: rt.key,
      planMode: capabilitiesFor(descriptors, rt.key)?.plan_mode,
    });
    if (interactionMode === "plan" && sendPlanSupport.known && !sendPlanSupport.available) {
      throw new Error(sendPlanSupport.reason);
    }
    return { cred, rt, needsProbe, runtimePayload };
  };

  const drainRapidSendQueue = async () => {
    if (rapidSendDrainingRef.current) return;
    rapidSendDrainingRef.current = true;
    let sentAny = false;
    try {
      while (rapidSendQueueRef.current.length > 0) {
        const item = rapidSendQueueRef.current[0]!;
        let activeIntentId = "";
        try {
          if (conversationStorageScope() !== item.ownerScope) {
            throw new Error("conversation.scope_changed: 服务身份已改变，原发送身份保留在原范围");
          }
          const intent = await prepareSendIntent(
            item.intentKey,
            item.text,
            [],
            item.structuredRefs,
            item.runtimePayload,
            recoverCommandReceipt,
          );
          activeIntentId = intent.intentId;
          if (conversationStorageScope() !== item.ownerScope) {
            throw new Error("conversation.scope_changed: 服务身份已改变，原发送身份保留在原范围");
          }
          if (intent.phase === "accepted") {
            rapidSendQueueRef.current.shift();
            await clearSendIntent(item.intentKey, intent.intentId);
            sentAny = true;
            continue;
          }
          if (!intent.threadId) {
            await transitionSendIntent(item.intentKey, { type: "thread_confirmed", threadId: item.target }, intent.intentId);
          }
          await transitionSendIntent(item.intentKey, { type: "send_started" }, intent.intentId);
          setNotice(phaseNotice("sending"), "progress");
          const expRefs = experience?.composerRefs?.() ?? [];
          const capabilityRefs = expRefs.length
            ? [...(intent.capabilityRefs ?? []), ...expRefs]
            : item.structuredRefs;
          const sendReceipt = await sendConversationCommand(
            item.target,
            "conversation.turn.send",
            {
              text: item.text,
              client_message_id: intent.clientMessageId,
              attachments: [],
              runtime: item.runtimePayload,
              capability_refs: capabilityRefs,
            },
            {
              commandId: intent.sendCommandId,
              idempotencyKey: intent.sendCommandId,
            },
          );
          if (conversationStorageScope() !== item.ownerScope) {
            throw new Error("conversation.scope_changed: 服务身份已改变，原发送身份保留在原范围");
          }
          rapidSendQueueRef.current.shift();
          sentAny = true;
          experience?.onSent?.();
          window.dispatchEvent(new CustomEvent("muteki:conversation-sent", { detail: { threadId: item.target } }));
          const receiptState = String(sendReceipt.state || "accepted");
          await transitionSendIntent(item.intentKey, {
            type: "accepted",
            state: receiptState,
          }, intent.intentId);
          setNotice(phaseNotice("accepted"), "accepted", sendReceipt.command_id);
          await clearSendIntent(item.intentKey, intent.intentId);
        } catch (exc) {
          const message = formatSendFailureMessage(exc, { keepDraft: true });
          const pending = readSendIntent(item.intentKey)?.phase;
          const uncertain = (pending === "creating_thread" || pending === "sending")
            && ["connection", "server", "unknown"].includes(classifySendFailure(exc));
          if (activeIntentId) {
            await transitionSendIntent(item.intentKey, {
              type: "failure",
              uncertain,
              error: message,
            }, activeIntentId);
            if (!uncertain) {
              await clearSendIntent(item.intentKey, activeIntentId).catch(() => {});
            }
          }
          const dropped = rapidSendQueueRef.current.splice(0, rapidSendQueueRef.current.length);
          const live = composerSnapshotRef.current;
          if (live.draftKey === item.draftKey && !plainTextFromDocument(live.promptDocument).trim()) {
            const restored = dropped.map((row) => row.text).filter(Boolean).join("\n");
            if (restored) {
              setPromptDocument(documentFromPromptAndRefs(restored, []));
            }
          }
          setNotice("");
          setError(message, "send");
          break;
        }
      }
      if (sentAny) {
        void Promise.all([conversation.refresh(), refreshList()]);
      }
    } finally {
      rapidSendDrainingRef.current = false;
      if (rapidSendQueueRef.current.length > 0) {
        void drainRapidSendQueue();
      }
    }
  };

  const handleSend = async (sendMode: RunningSendDefault = runningSend, override?: { text?: string; scheduleAt?: number }) => {
    let text = (override?.text ?? plainTextFromDocument(promptDocument)).trim();
    let scheduledAt = override?.scheduleAt ?? null;
    const laterCommand = override?.text ? null : parseLaterCommand(text);
    if (laterCommand) {
      if (!laterCommand.ok) {
        if (laterCommand.code === "missing_time") setLaterDialogOpen(true);
        else setError(laterErrorMessage(laterCommand));
        return;
      }
      scheduledAt = laterCommand.at;
      text = laterCommand.message;
      if (sendMode === "steer") sendMode = "queue";
    }
    const expectedTurnId = running && sendMode === "steer" ? conversation.view?.state.running_turn_id : undefined;
    if (preview) {
      if (!text || busy) return;
      const suffix = Date.now().toString(36);
      const turnId = `preview-turn-${suffix}`;
      const messageId = `preview-message-${suffix}`;
      const createdAt = new Date().toISOString();
      setPreviewView((current) => current ? {
        ...current,
        messages: [...current.messages, {
          message_id: messageId,
          turn_id: turnId,
          role: "user",
          kind: "message",
          text,
          stream_seq: current.event_watermark + 1,
          created_at: createdAt,
        }],
        turns: [...current.turns, {
          turn_id: turnId,
          kind: "user",
          status: "completed",
          text,
          created_at: createdAt,
          completed_at: createdAt,
        }],
        messages_page: {
          ...(current.messages_page || { limit: 50, has_more_before: false, has_more_after: false }),
          newest_stream_seq: current.event_watermark + 1,
          newest_message_id: messageId,
        },
        watermark: current.watermark + 1,
        event_watermark: current.event_watermark + 1,
      } : current);
      setPromptDocument(emptyPromptDocument());

      setAttachments([]);

      if (preview.notice !== "") setNotice(preview.notice || "演示消息已加入当前预览，不会发送请求或运行工具。", "success");
      preview.onSend?.(text);
      return;
    }
    if (running && sendMode === "steer" && (!canSteer || !expectedTurnId)) {
      setError("当前 Agent 接入不支持引导正在执行的回答，请改为排队发送；草稿已保留");
      return;
    }
    if (threadId && providerPreview && sendMode !== "steer" && !scheduledAt && !relayConfirmedRef.current) {
      setRelayConfirm({ sendMode });
      return;
    }
    relayConfirmedRef.current = false;
    if (accessModeIssue) { setError(accessModeIssue); return; }
    const ownerScope = conversationStorageScope();
    const ensureOwner = () => { if (conversationStorageScope() !== ownerScope) throw new Error("conversation.scope_changed: 服务身份已改变，原发送身份保留在原范围"); };
    const activeContext = activeSendContextRef.current;
    if (activeContext.draftKey !== draftKey) return;
    if (threadId && activeContext.viewThreadId !== threadId) return;
    const unresolved = refsFromDocument(promptDocument).find((node) => node.status !== "ok");
    if (unresolved) { setError(`引用“${unresolved.name}”尚未按当前项目和引擎确认。${unresolved.status_reason || "请重新选择或移除后发送"}`); return; }
    if (["/clear", "/new", "/clear-input"].includes(text)) {
      setPromptDocument(emptyPromptDocument());


      if (text !== "/clear-input") startNewChat(projectId, true);
      return;
    }
    let nativeInvocation: Record<string, unknown> | undefined;
    const localCommand = /^\/([a-zA-Z][\w:.-]*)(?:\s+([\s\S]*))?$/.exec(text);
    if (localCommand && composerCapabilityContext && !scheduledAt) {
      try {
        const catalog = await fetchComposerCapabilities(composerCapabilityContext, "/", localCommand[1]);
        ensureOwner();
        if (activeSendContextRef.current.draftKey !== draftKey) return;
        const live = composerSnapshotRef.current;
        if (live.runtimeKey !== runtimeKey || live.credentialId !== credentialId || live.projectId !== projectId || fingerprintPromptDocument(live.promptDocument) !== fingerprintPromptDocument(promptDocument)) { setNotice("命令来源的输入或接入设置已改变，请重新确认当前命令", "warning"); return; }
        const item = catalog.items.find((row) => row.kind === "command" && row.name.toLowerCase() === localCommand[1].toLowerCase());
        if (item?.action?.startsWith("ui:")) { await handleClientCommand(item.action, localCommand[2] || ""); return; }
        if (item?.invocable === false) { setError(item.reason || "当前引擎不支持此命令"); return; }
        if (item?.channel === "provider_native" && item.invocable) nativeInvocation = { ...item };
      } catch (error) { setError(error instanceof Error ? error.message : "命令目录加载失败"); return; }
    }
    const structuredRefs = override?.text ? [] : wireCapabilityRefs(promptDocument);
    if ((!text && !structuredRefs.length && !attachments.length) || busy) return;
    if (threadId && isThreadArchived(threadId)) {
      setError("已归档的对话不能发送消息，请先取消归档");
      return;
    }

    const block = primarySendBlock(readiness);
    if (block) {
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

    const rapidPlainSubmit = Boolean(
      threadId
      && !override
      && attachments.length === 0
      && !nativeInvocation
      && !scheduledAt
      && !(running && sendMode === "steer"),
    );
    if (rapidPlainSubmit) {
      let validated: ReturnType<typeof validateSendRuntime>;
      try {
        validated = validateSendRuntime();
      } catch (err) {
        setError(formatSendFailureMessage(err, { keepDraft: true }), "send");
        return;
      }
      if (!validated.needsProbe) {
        setError("");
        setRuntimeKey(validated.rt.key);
        if (!sideChat) {
          writeChatLastSelection({
            credentialId: validated.cred?.id || "",
            modelId: model,
            runtimeKey: validated.rt.key,
            effort: validated.runtimePayload.effort,
            accessMode,
          });
        }
        const submittedPromptDoc = promptDocument;
        recordSentPrompt(composerDraftKey(threadId), {
          prompt: text,
          promptSegments: submittedPromptDoc.segments,
          capabilityRefs: refsFromDocument(submittedPromptDoc),
          projectId,
        });
        historyIndexRef.current = null;
        setHistoryIndex(null);
        setPromptDocument(emptyPromptDocument());
        clearComposerDraft(draftKey);
        const rapidSeed = typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
          ? crypto.randomUUID()
          : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
        rapidSendQueueRef.current.push({
          intentKey: `${draftKey}:rapid:${rapidSeed}`,
          target: threadId,
          draftKey,
          text,
          promptDocument: submittedPromptDoc,
          structuredRefs,
          runtimePayload: validated.runtimePayload,
          projectId,
          ownerScope,
        });
        void drainRapidSendQueue();
        return;
      }
    }

    await execute(async () => {
      let target = threadId;
      let createdHref: string | undefined;
      const initial = validateSendRuntime();
      let { cred, rt, runtimePayload } = initial;
      if (initial.needsProbe) {
        setProbing(true);
        try {
          const extensionRuntime = selectedRuntime?.origin === "extension";
          await probeRuntimeInstance(rt.key, !extensionRuntime && cred && runtimeScopesModelCatalog(descriptors, rt.key)
            ? { credentialId: cred.id, environment: "local" } : {});
          const rows = await fetchRuntimeInstances();
          const filtered = rows.filter((row) => row.enabled !== false);
          setRuntimes(filtered);
          setRuntimeSource(sourceOk());
          const probed = filtered.find((row) => row.key === rt.key) || rt;
          ({ cred, rt, runtimePayload } = validateSendRuntime(probed));
        } finally {
          setProbing(false);
        }
      }
      setRuntimeKey(rt.key);
      if (!sideChat) {
        writeChatLastSelection({
          credentialId: cred?.id || "", modelId: model, runtimeKey: rt.key,
          effort: runtimePayload.effort, accessMode,
        });
      }

      const intent = await prepareSendIntent(
        draftKey,
        text,
        override?.text ? [] : attachments.map((item) => String(item.id || "").trim()).filter(Boolean),
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
        nativeInvocation,
        expectedTurnId ? { expectedTurnId } : undefined,
      );
      ensureOwner();
      if (intent.phase === "accepted") {
        setNotice("原发送回执已核对，未重复发送当前输入", "accepted", intent.sendCommandId);
        return;
      }
      const submittedDraft = captureSendDraftSnapshot({
        draftKey,
        promptText: text,
        promptDocument,
        attachmentIds: attachments.map((item) => item.id),
      });
      setNotice(phaseNotice(intent.phase === "failed" ? "sending" : intent.phase), "progress");

      try {
        if (!target) {
          if (projectId && !selectedProject) {
            throw new Error("所选项目的工作目录不可用，请重新选择项目");
          }
          if (intent.threadId) {
            target = intent.threadId;
          } else {
            await transitionSendIntent(draftKey, { type: "creating" }, intent.intentId);
            setNotice(phaseNotice("creating_thread"), "progress");

            let workspaceId = intent.boundWorkspaceId || selectedProject?.workspace_id || undefined;
            const bindId = intent.bindCommandId || `${intent.createCommandId}:workspace`;
            if (selectedProject?.root_path && workspaceMode === "new_worktree" && !intent.boundWorkspaceId) {
              const branch = intent.boundBranch || worktreeBranch.trim() || `muteki-${intent.intentId}`;
              await transitionSendIntent(draftKey, { type: "binding_started", commandId: bindId, branch }, intent.intentId);
              const bound = await bindConversationWorkspace({
                project_id: selectedProject.project_id,
                mode: "new_worktree",
                branch,
                base_ref: "HEAD",
                parent_root: selectedProject.root_path,
              }, { commandId: bindId, idempotencyKey: bindId });
              ensureOwner();
              workspaceId = bound.workspaceId;
              await transitionSendIntent(draftKey, { type: "binding_confirmed", workspaceId }, intent.intentId);
            } else if (selectedProject?.root_path && workspaceMode === "existing_worktree" && !intent.boundWorkspaceId) {
              const rootPath = normalizeWorktreePath(existingWorktreePath);
              if (!canBindExistingWorktree(rootPath)) {
                throw new Error(worktreeMissingSelectionCopy());
              }
              await transitionSendIntent(draftKey, { type: "binding_started", commandId: bindId }, intent.intentId);
              const bound = await bindConversationWorkspace({
                project_id: selectedProject.project_id,
                mode: "existing_worktree",
                root_path: rootPath,
                parent_root: selectedProject.root_path,
              }, { commandId: bindId, idempotencyKey: bindId });
              ensureOwner();
              workspaceId = bound.workspaceId;
              await transitionSendIntent(draftKey, { type: "binding_confirmed", workspaceId }, intent.intentId);
            }

            await transitionSendIntent(draftKey, { type: "create_started" }, intent.intentId);
            const createInput: Parameters<typeof createConversationThread>[0] = {
              title: "新对话",
              title_source: "fallback",
              mode,
              ...(selectedProject ? {
                project_id: selectedProject.project_id,
                ...(workspaceId ? { workspace_id: workspaceId } : {}),
              } : {}),
              runtime: runtimePayload,
            };
            const createOptions = {
              commandId: intent.createCommandId,
              idempotencyKey: intent.createCommandId,
            };
            if (experience?.createThread) {
              const created = await experience.createThread(createInput, createOptions, { promptText: text });
              target = created.threadId;
              createdHref = created.href;
              if (!target.trim()) throw new Error("创建对话未返回有效的对话 ID");
            } else {
              target = await createConversationThread(createInput, createOptions);
            }
            ensureOwner();
            await transitionSendIntent(draftKey, { type: "thread_confirmed", threadId: target }, intent.intentId);
          }
        } else if (!intent.threadId) {
          await transitionSendIntent(draftKey, { type: "thread_confirmed", threadId: target }, intent.intentId);
        }

        const attachmentRefs: string[] = [];
        const latestIntent = readSendIntent(draftKey) || intent;
        const uploadByAttachment = new Map(
          latestIntent.uploads.map((row) => [row.attachmentId, row]),
        );
        const uploadErrors: string[] = [];
        if (attachments.length > 0 && !override?.text) {
          await transitionSendIntent(draftKey, { type: "uploading" }, intent.intentId);
          setNotice(phaseNotice("uploading"), "progress");
          for (const a of attachments) {
            const attachmentId = String(a.id || "").trim();
            const uploadMeta = uploadByAttachment.get(attachmentId);
            const savedRef = a.artifactId;
            if (savedRef) {
              attachmentRefs.push(savedRef);
              if (attachmentId && uploadMeta && !uploadMeta.artifactId) {
                await markSendUploadRef(draftKey, attachmentId, savedRef, intent.intentId);
              }
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id ? { ...row, uploadStatus: "done" as const } : row
              )));
              continue;
            }
            if (uploadMeta?.artifactId) {
              const recoveredRef = uploadMeta.artifactId;
              attachmentRefs.push(recoveredRef);
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id ? { ...row, artifactId: recoveredRef, uploadStatus: "done" as const } : row
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
            setAttachments((prev) => prev.map((row) => (
              row.id === a.id ? { ...row, uploadStatus: "uploading" as const, uploadError: undefined } : row
            )));
            try {
              const receipt = await uploadConversationFile(target, a.file, {
                commandId: uploadMeta?.commandId,
                idempotencyKey: uploadMeta?.commandId,
              });
              ensureOwner();
              const artifactId = String(receipt.output?.artifact_id || "").trim();
              if (!artifactId) throw new Error(`附件 ${a.name} 上传回执缺少附件 ID`);
              attachmentRefs.push(artifactId);
              if (attachmentId) {
                await markSendUploadRef(draftKey, attachmentId, artifactId, intent.intentId);
              }
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id ? { ...row, artifactId, uploadStatus: "done" as const } : row
              )));
            } catch (uploadErr) {
              if (ownerScope !== conversationStorageScope()) throw uploadErr;
              const errMsg = uploadErr instanceof Error ? uploadErr.message : `附件 ${a.name} 上传失败`;
              uploadErrors.push(errMsg);
              setAttachments((prev) => prev.map((row) => (
                row.id === a.id
                  ? { ...row, uploadStatus: "error" as const, uploadError: errMsg }
                  : row
              )));
            }
          }
          const abortMessage = attachmentUploadAbortMessage(uploadErrors);
          if (abortMessage) {
            throw new Error(abortMessage);
          }
        }

        ensureOwner();
        await transitionSendIntent(draftKey, { type: "send_started" }, intent.intentId);
        setNotice(phaseNotice("sending"), "progress");
        const sendReceipt = await sendConversationCommand(
          target,
          intent.delivery ? "conversation.turn.steer" : "conversation.turn.send",
          {
            text,
            ...(intent.delivery ? { expected_turn_id: intent.delivery.expectedTurnId } : {}),
            client_message_id: intent.clientMessageId,
            runtime_capability: intent.runtimeInvocation,
            runtime_command_arguments: intent.runtimeInvocation ? (localCommand?.[2] || "") : undefined,
            attachments: attachmentRefs,
            ...(scheduledAt ? { not_before: scheduledAt } : {}),
            runtime: runtimePayload,
            capability_refs: [
              ...(intent.capabilityRefs ?? []),
              ...(experience?.composerRefs?.() ?? []),
            ].length ? [...(intent.capabilityRefs ?? []), ...(experience?.composerRefs?.() ?? [])] : structuredRefs,
          },
          {
            commandId: intent.sendCommandId,
            idempotencyKey: intent.sendCommandId,
          },
        );
        ensureOwner();
        experience?.onSent?.();
        window.dispatchEvent(new CustomEvent("muteki:conversation-sent", {detail: {threadId: target}}));
        const receiptState = String(sendReceipt.state || "accepted");
        await transitionSendIntent(draftKey, {
          type: "accepted", state: receiptState,
        }, intent.intentId);
        setNotice(phaseNotice("accepted"), "accepted", sendReceipt.command_id);
        if (scheduledAt) {
          setNotice(`消息已加入队列，${formatScheduledTime(scheduledAt)} 自动发送`, "accepted", sendReceipt.command_id);
        }
        recordSentPrompt(composerDraftKey(target), {
          prompt: text,
          promptSegments: promptDocument.segments,
          capabilityRefs: refsFromDocument(promptDocument),
          projectId,
        });
        historyIndexRef.current = null;
        setHistoryIndex(null);

        const live = composerSnapshotRef.current;
        const liveSlice = {
          draftKey: live.draftKey,
          promptText: plainTextFromDocument(live.promptDocument).trim(),
          documentFingerprint: fingerprintPromptDocument(live.promptDocument),
          attachmentIds: live.attachments.map((row) => String(row.id || "").trim()).filter(Boolean),
        };
        const plan = planComposerAfterSendSuccess(liveSlice, submittedDraft);
        if (plan.action === "clear_all") {

          setPromptDocument(emptyPromptDocument());
          setAttachments([]);

          clearComposerDraft(submittedDraft.draftKey);
        } else if (plan.action === "preserve_edits") {
        setComposerUndoEpoch((n) => n + 1);
          setAttachments((curr) => filterAttachmentsAfterSend(curr, plan.removeAttachmentIds));
        } else {
          const stored = readComposerDraft(submittedDraft.draftKey);
          if (shouldClearDraftStorageAfterSend(stored, submittedDraft)) {
            clearComposerDraft(submittedDraft.draftKey);
          }
        }
        await clearSendIntent(submittedDraft.draftKey, intent.intentId);

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
              interactionMode: live.interactionMode,
              projectId: live.projectId,
            });
            flushComposerDraftStore();
          }
          router.push(createdHref ?? threadHref(target));
        }
      } catch (exc) {
        const message = formatSendFailureMessage(exc, { keepDraft: true });
        if (ownerScope !== conversationStorageScope()) throw exc;
        const pending = readSendIntent(draftKey)?.phase;
        const uncertain = (pending === "creating_thread" || pending === "sending")
          && ["connection", "server", "unknown"].includes(classifySendFailure(exc));
        await transitionSendIntent(draftKey, {
          type: "failure", uncertain, error: message,
        }, intent.intentId);
        setNotice("");
        throw exc;
      }
    }, "send");
  };

  const askAgentInComposer = (request: string) => {
    handlePromptDocumentChange(insertPlainTextAtMarkedRange(promptDocument, `${plainTextFromDocument(promptDocument).trim() ? "\n" : ""}${request}`));
    requestAnimationFrame(() => shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
  };

  const handleCiteMessageSpan = useCallback((payload: {
    messageId: string;
    turnId?: string;
    text: string;
    startOffset: number;
    endOffset: number;
    streaming?: boolean;
    sourceThreadId?: string;
    comment?: string;
  }) => {
    const node = createMessageSpanNode({
      threadId: payload.sourceThreadId || threadId || "",
      messageId: payload.messageId,
      turnId: payload.turnId,
      startOffset: payload.startOffset,
      endOffset: payload.endOffset,
      text: payload.text,
      streaming: payload.streaming,
      comment: payload.comment,
    });
    setPromptDocument((current) => addStripNode(current, node));
    if (payload.streaming) {
      setNotice("流式中引用可能不完整", "warning");
    }
    if (payload.sourceThreadId && panelSheet) chatPanel.close(threadId);
    requestAnimationFrame(() => shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
  }, [threadId, panelSheet]);
  const [queuedSideSend, setQueuedSideSend] = useState("");
  useSideChatQuotes(sideChat ? threadId : "", Boolean(conversation.view) && hydratedDraftKey === draftKey, (quote) => {
    if (!("plainText" in quote)) { handleCiteMessageSpan(quote); return; }
    if (quote.send) { setQueuedSideSend(quote.plainText); return; }
    appendComposerText(quote.plainText);
    requestAnimationFrame(() => shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
  });

  const sideChatFailure = (failure: unknown) => {
    setNotice(`无法开启侧边聊天：${failure instanceof Error ? failure.message : String(failure)}`, "warning");
  };
  const startSideChat = async (ownerId: string, question = "") => {
    let sideId = "";
    try {
      sideId = await createSideChat(ownerId);
    } catch (failure) { sideChatFailure(failure); return ""; }
    if (!question) requestSideChatFocus(sideId);
    if (ownerId === threadId) setRightPanelInstant(false);
    chatPanel.open(ownerId, sideChatSurface(sideId));
    if (question) {
      try { await sendSideChatQuestion(sideId, question); }
      catch (failure) { setNotice(`侧边问题未能发送：${failure instanceof Error ? failure.message : String(failure)}`, "warning"); }
    }
    return sideId;
  };
  const askInSideChat = async (quote: Extract<SideChatQuote, { sourceThreadId: string }>) => {
    const ownerId = quote.sourceThreadId;
    let sideId = visibleSideChatId(ownerId);
    if (!sideId) {
      try { sideId = await createSideChat(ownerId); }
      catch (failure) { sideChatFailure(failure); return; }
    }
    queueSideChatQuote(sideId, quote);
    requestSideChatFocus(sideId);
    if (ownerId === threadId) setRightPanelInstant(false);
    chatPanel.open(ownerId, sideChatSurface(sideId));
  };

  const handleDiffAnnotationSend = useCallback((annotations: DiffLineAnnotation[]) => {
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
      return next;
    });
    if (panelSheet && threadId) chatPanel.close(threadId);
    window.requestAnimationFrame(() => shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]")?.focus());
  }, [panelSheet, threadId]);

  const jumpRequestRef = useRef(0);
  const handleJumpToContext = useLatestCallback(async (payload: {
    messageId: string;
    turnId?: string;
    startOffset?: number;
    endOffset?: number;
  }) => {
    pushJumpBackFromThread(threadId);
    const request = ++jumpRequestRef.current;
    const loaded = conversation.view?.messages.some((message) => message.message_id === payload.messageId
      || Boolean(payload.turnId && message.turn_id === payload.turnId));
    if (!loaded && ensureMessageVisible) {
      const result = await ensureMessageVisible(payload.messageId).catch(() => "missing" as const);
      if (request !== jumpRequestRef.current || result === "cancelled" || result === "superseded") return;
    }
    setJumpTarget({ ...payload, key: `${payload.messageId}:${request}` });
  });

  const composerAnnotations = useMemo(() => {
    const marks: MessageAnnotationMark[] = [];
    for (const node of Object.values(promptDocument.nodes)) {
      if (node.kind !== "message_span" || !node.locator.message_id) continue;
      if (node.locator.thread_id && threadId && node.locator.thread_id !== threadId) continue;
      if (node.locator.start_offset === undefined || node.locator.end_offset === undefined) continue;
      marks.push({
        id: node.node_id,
        index: marks.length + 1,
        messageId: node.locator.message_id,
        startOffset: node.locator.start_offset,
        endOffset: node.locator.end_offset,
      });
    }
    return marks;
  }, [promptDocument.nodes, threadId]);

  const handleQueueUpdate = async (queueId: string, text: string): Promise<boolean> => {
    if (!threadId || conversation.view?.thread.thread_id !== threadId) return false;
    let confirmed = false;
    await execute(async () => {
      const receipt = await sendConversationCommand(threadId, "conversation.queue.update", {
        queue_id: queueId,
        text,
        resume_after_edit: true,
        expected_revision: conversation.view?.state.queue_revision ?? 0,
      });
      confirmed = classifyCommandReceipt(receipt) === "completed";
      if (!confirmed) setNotice("队列编辑已受理，尚未确认保存；编辑正文继续保留", "accepted", receipt.command_id);
    });
    return confirmed;
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
      if (interactionMode === "plan" && planModeSupport.known && !planModeSupport.available) {
        throw new Error(planModeSupport.reason);
      }
      await sendConversationCommand(threadId, "conversation.queue.update", {
        queue_id: queueId,
        text: item.text,
        runtime: {
          adapter_id: rt.adapter_id, instance_id: rt.instance_id,
          credential_id: cred.id, model, effort, access_mode: accessMode,
          interaction_mode: interactionMode,
          service_tier: effectiveServiceTier(allCredentialModels(cred).find((row) => row.id === model), serviceTier),
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

  const handleQueueFinishEdit = () => {
    if (!threadId) return;
    void execute(async () => {
      await sendConversationCommand(threadId, "conversation.queue.resume", { only_if_reason: "queue_editing" });
    });
  };

  const handleQueuePause = async (reason: "user_paused" | "queue_editing" = "user_paused"): Promise<boolean> => {
    if (!threadId || conversation.view?.thread.thread_id !== threadId) return false;
    let confirmed = false;
    await execute(async () => {
      const receipt = await sendConversationCommand(threadId, "conversation.queue.pause", {
        reason,
      });
      confirmed = classifyCommandReceipt(receipt) === "completed";
      setNotice(confirmed ? reason === "queue_editing" ? "编辑期间暂缓发送，保存或取消后自动恢复" : "已手动暂停自动发送后续消息" : "暂停操作已受理，尚未确认；暂不开放编辑");
    });
    return confirmed;
  };

  const handleQueueSteer = (queueId: string) => {
    if (!threadId) return;
    void execute(async () => {
      const expectedTurnId = conversation.view?.state.running_turn_id;
      if (!expectedTurnId) throw new Error("当前回答已经结束");
      if (!canSteer) throw new Error("当前 Agent 接入不支持引导正在执行的回答");
      const receipt = await sendConversationCommand(threadId, "conversation.queue.steer", {
        queue_id: queueId,
        expected_turn_id: expectedTurnId,
      });
      setNotice("引导请求已提交", "accepted", receipt.command_id);
    });
  };

  const handleStop = () => {
    const targetThreadId = threadId;
    const expectedTurnId = conversation.view?.state.running_turn_id || backgroundTurnId;
    if (
      !targetThreadId
      || !expectedTurnId
      || interactionBusy
      || conversation.view?.thread.thread_id !== targetThreadId
    ) return;
    void execute(async () => {
      try {
        const receipt = await sendConversationCommand(targetThreadId, "conversation.turn.interrupt", { expected_turn_id: expectedTurnId });
        setNotice(classifyCommandReceipt(receipt) === "completed" ? "中断已确认" : "中断请求已受理，等待确认", classifyCommandReceipt(receipt) === "completed" ? "success" : "accepted", receipt.command_id);
      } catch (failure) {
        if ((failure as { code?: string })?.code !== "conversation.turn.not_running") throw failure;
        const latest = await fetchConversationView(targetThreadId, false, 50);
        const target = latest.turns.find((turn) => turn.turn_id === expectedTurnId);
        if (latest.state.running_turn_id || !target || !["completed", "failed", "interrupted", "aborted", "cancelled"].includes(target.status)) throw failure;
        setNotice("本轮已经结束，当前没有需要中断的任务", "success");
      }
    });
  };

  const handleResume = () => {
    if (!threadId) return;
    void execute(async () => {
      if (interactionMode === "plan" && planModeSupport.known && !planModeSupport.available) {
        throw new Error(planModeSupport.reason);
      }
      await sendConversationCommand(threadId, "conversation.thread.resume", {
        runtime: { access_mode: accessMode, interaction_mode: interactionMode },
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

  const impactGenerationRef = useRef(0);
  const impactOwnerRef = useRef<{ draftKey: string; scope: string; threadId: string; turnId: string } | null>(null);
  useEffect(() => {
    ++impactGenerationRef.current;
    setImpactOpen(false); setImpactPreview(null); setImpactLoading(false); setImpactForkSource(null);
    impactOwnerRef.current = null;
  }, [draftKey, commandScope]);
  const openImpact = async (
    mode: ImpactMode,
    turnId: string,
    options?: { text?: string; sourceThreadId?: string },
  ) => {
    const previewThreadId = mode === "fork"
      ? (options?.sourceThreadId || impactForkSource?.threadId || threadId)
      : threadId;
    if (!previewThreadId || !turnId) return;
    const generation = ++impactGenerationRef.current;
    const ownerScope = conversationStorageScope();
    const ownerDraft = draftKey;
    impactOwnerRef.current = { draftKey, scope: ownerScope, threadId: previewThreadId, turnId };
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
      if (generation !== impactGenerationRef.current || composerSnapshotRef.current.draftKey !== ownerDraft || ownerScope !== conversationStorageScope()) return;
      setImpactPreview(preview as ImpactPreview);
      if (mode === "edit_resend") {
        const seed = readEditBuffer(previewThreadId, turnId)?.text ?? options?.text ?? preview.target_text_preview ?? "";
        setImpactEditedText(seed);
        writeEditBuffer(previewThreadId, turnId, seed);
      }
      if (mode === "retry") {
        const retryIntent = await ensureRetryIntent(previewThreadId, turnId);
        setImpactCommandId(retryIntent.commandId);
      } else {
        setImpactCommandId(
          `cmd_c11_${mode}_${turnId}_${Date.now().toString(36)}`,
        );
      }
      setImpactOpen(true);
    } catch (exc) {
      if (generation !== impactGenerationRef.current || composerSnapshotRef.current.draftKey !== ownerDraft || ownerScope !== conversationStorageScope()) return;
      setError(exc instanceof Error ? exc.message : String(exc));
    } finally {
      if (generation === impactGenerationRef.current) setImpactLoading(false);
    }
  };

  const handleRetry = (turnId: string) => {
    void openImpact("retry", turnId);
  };

  const handleEditTurn = (turnId: string, text: string) => {
    void openImpact("edit_resend", turnId, { text });
  };

  const handleRewindTurn = (turnId: string) => {
    void openImpact("native_rewind", turnId);
  };

  const handleFork = (sourceThreadId: string, sourceTurnId?: string) => {
    if (!sourceThreadId) {
      setError("无法分叉：未指定源对话");
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
            router.push(threadHref(forkedId));
          }
        });
        return;
      }
      void openImpact("fork", target, { sourceThreadId });
    })();
  };

  const closeImpact = () => {
    ++impactGenerationRef.current;
    impactOwnerRef.current = null;
    setImpactOpen(false);
    setImpactPreview(null);
    setImpactEditedText("");
    setImpactFileMode("keep_files");
    setImpactCommandId("");
    setImpactForkSource(null);
  };

  const confirmImpact = () => {
    const owner = impactOwnerRef.current;
    if (!impactPreview || impactLoading || !owner || owner.draftKey !== draftKey || owner.scope !== conversationStorageScope()) return;
    if (impactPreview.target_turn_id !== owner.turnId || (impactPreview.mode === "edit_resend" && !impactEditedText.trim())) return;
    const mode = String(impactPreview.mode || "retry") as ImpactMode;
    const turnId = impactPreview.target_turn_id;
    const commandId = impactCommandId || undefined;
    const fileMode: ImpactFileMode = impactPreview.checkpoint?.available ? impactFileMode : "keep_files";
    const filesNote = fileMode === "restore_files" ? "文件已回退到该轮开始前" : "文件保持原状";
    const commandThreadId = impactCommandThreadId({
      mode,
      currentThreadId: threadId,
      frozenSourceThreadId: impactForkSource?.threadId,
    });
    if (!commandThreadId) return;
    const generation = impactGenerationRef.current;
    const isCurrent = () => generation === impactGenerationRef.current && impactOwnerRef.current === owner && owner.scope === conversationStorageScope() && owner.draftKey === composerSnapshotRef.current.draftKey;
    const confirmReceipt = (state: string) => {
      if (!isCurrent()) return false;
      if (state !== "completed") { setNotice("原操作已受理但尚未确认完成；编辑内容和原命令身份保留，请核对回执后继续", "accepted"); return false; }
      return true;
    };
    void execute(async () => {
      if (mode === "fork") {
        const receipt = await sendConversationCommand(
          commandThreadId,
          "conversation.thread.fork",
          { from_turn_id: turnId },
          commandId ? { commandId, idempotencyKey: commandId } : {},
        );
        if (!confirmReceipt(receipt.state)) return;
        closeImpact();
        const forkedId = receipt.aggregate?.id;
        if (forkedId) {
          router.push(threadHref(forkedId));
        }
        setNotice("已 Fork；源 Thread 保留，当前共享工作区", "success");
        return;
      }
      if (mode === "native_rewind") {
        const mappedFileMode = impactRewindMode === "files_and_conversation"
          ? "rollback"
          : impactRewindMode === "files"
            ? "files_only"
            : "keep_files";
        const receipt = await sendConversationCommand(
          owner.threadId,
          "conversation.turn.native_rewind",
          { turn_id: turnId, mode: impactRewindMode, file_mode: mappedFileMode },
          commandId ? { commandId, idempotencyKey: commandId } : {},
        );
        if (!confirmReceipt(receipt.state)) return;
        closeImpact();
        const prefillText = impactRewindMode === "files" ? "" : (
          (conversation.view?.messages || [])
            .filter((message) => message.role === "user" && message.turn_id === turnId && message.kind !== "steer")
            .map((message) => String(message.text || ""))
            .filter((text) => text.trim())
            .join("\n")
          || String(impactPreview.target_text_preview || "")
        );
        if (prefillText.trim()) {
          const next = insertPlainTextAtMarkedRange(
            composerSnapshotRef.current.promptDocument,
            `${plainTextFromDocument(composerSnapshotRef.current.promptDocument) ? "\n" : ""}${prefillText}`,
          );
          setPromptDocument(next);
          setNotice(impactRewindMode === "files_and_conversation" ? "对话与代码已回退，原提示已放回输入框" : "聊天历史已回退，原提示已放回输入框，工作区文件保持原状", "success");
        } else {
          setNotice(impactRewindMode === "files" ? "已仅回退代码文件，对话记录保持原状" : impactRewindMode === "files_and_conversation" ? "对话与代码已回退" : "聊天历史已回退，工作区文件保持原状", "success");
        }
        return;
      }
      if (mode === "edit_resend") {
        const text = impactEditedText;
        writeEditBuffer(threadId, turnId, text);
        const receipt = await sendConversationCommand(
          owner.threadId,
          "conversation.turn.edit_resend",
          { turn_id: turnId, text, file_mode: fileMode, interaction_mode: interactionMode },
          commandId ? { commandId, idempotencyKey: commandId } : {},
        );
        if (!confirmReceipt(receipt.state)) return;
        clearEditBuffer(threadId, turnId);
        closeImpact();
        setNotice(`已编辑重发（${filesNote}）`, "success");
        return;
      }
      let retryIntent = await ensureRetryIntent(threadId, turnId);
      if (retryIntent.phase === "unknown" || retryIntent.phase === "sending") {
        let recovered: Awaited<ReturnType<typeof recoverCommandReceipt>> = null;
        let receiptCheck: PendingReceiptCheck;
        try {
          recovered = await recoverCommandReceipt(retryIntent.commandId, 1);
          receiptCheck = { checkedAt: Date.now(), result: recovered ? "received" : "missing", ...(recovered ? { receipt: recovered } : {}) };
        } catch (failure) { receiptCheck = { checkedAt: Date.now(), result: "error", error: failure instanceof Error ? failure.message : String(failure) }; }
        if (!isCurrent()) return;
        if (classifyCommandReceipt(recovered) === "completed") {
          await updateRetryIntent(threadId, turnId, { phase: "accepted", lastReceiptState: recovered!.state }, retryIntent.commandId);
          await clearRetryIntent(threadId, turnId, retryIntent.commandId);
          closeImpact(); setNotice("原重试操作已完成，未重复投递", "success"); return;
        }
        if (recovered && classifyCommandReceipt(recovered) === "failed") {
          await updateRetryIntent(threadId, turnId, { phase: "failed", lastReceiptState: recovered.state }, retryIntent.commandId);
          retryIntent = await ensureRetryIntent(threadId, turnId);
        } else {
          setPendingSendDialog({ kind: "retry", draftKey: owner.draftKey, scope: owner.scope, threadId, turnId, commandId: retryIntent.commandId, receiptCheck });
          setPendingSendStatus(receiptCheck.result === "error" ? `原回执核对失败：${receiptCheck.error}` : recovered ? `原重试回执：${recovered.state}` : "原重试暂时没有回执；旧版本记录不能证明请求尚未投递。");
          try { setArchivedRetries(listArchivedRetryIntents()); } catch (failure) { setPendingSendStatus(failure instanceof Error ? failure.message : String(failure)); }
          throw new ConversationCommandError({ code: "conversation.retry.pending", category: "state", retryable: false, message: "原重试命令结果尚未确认，请核对原回执，或明确保留原记录后准备一次新重试；未重复投递", detail: { command_id: retryIntent.commandId, outcome_unknown: true, receipt_check: receiptCheck } });
        }
      }
      setNotice(phaseNotice(retryIntent.phase === "prepared" ? "sending" : retryIntent.phase), "progress");
      try {
        await updateRetryIntent(threadId, turnId, { phase: "sending", request: { fileMode, interactionMode } }, retryIntent.commandId);
        const receipt = await sendConversationCommand(
          threadId,
          "conversation.turn.retry",
          { turn_id: turnId, file_mode: fileMode, interaction_mode: interactionMode },
          {
            commandId: retryIntent.commandId,
            idempotencyKey: retryIntent.commandId,
          },
        );
        if (!isCurrent()) return;
        await updateRetryIntent(threadId, turnId, {
          phase: classifyCommandReceipt(receipt) === "completed" ? "accepted" : "unknown",
          lastReceiptState: String(receipt.state || "accepted"),
          lastError: "",
        }, retryIntent.commandId);
        if (!confirmReceipt(receipt.state)) return;
        await clearRetryIntent(threadId, turnId, retryIntent.commandId);
        closeImpact();
        setNotice(`已从所选轮次之前重新执行（${filesNote}）`, "success");
      } catch (exc) {
        if (!isCurrent()) return;
        const message = exc instanceof Error ? exc.message : String(exc);
        const definitiveReceipt = exc instanceof ConversationCommandError
          && Boolean(exc.code)
          && exc.detail?.outcome_unknown !== true
          && exc.detail?.delivery_unknown !== true;
        await updateRetryIntent(threadId, turnId, {
          phase: definitiveReceipt
            ? "failed"
            : (["connection", "server", "unknown"].includes(classifySendFailure(exc)) ? "unknown" : "failed"),
          lastError: message,
        }, retryIntent.commandId);
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
      if (threadIdRef.current !== threadId) return;
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
    if (target === threadId && conversation.view?.state.status) {
      return conversation.view.state.status === "archived";
    }
    const listed = threads.find((thread) => thread.thread_id === target);
    if (listed) return listed.state.status === "archived";
    return false;
  };

  const threadArchived = Boolean(threadId && isThreadArchived(threadId));
  const nativeMirror = Boolean(conversation.view?.state.lineage?.native_subagent);

  const runArchiveCommand = async (target: string, archived: boolean) => {
    if (preview) throw new Error("演示数据只读，不会修改真实对话。");
    const commandType = archived
      ? "conversation.thread.unarchive"
      : "conversation.thread.archive";
    const commandKey = `${archived ? "unarchive" : "archive"}:${target}:${Date.now().toString(36)}`;
    const receipt = await sendConversationCommand(target, commandType, {}, {
      commandId: commandKey,
      idempotencyKey: commandKey,
    });
    if (receipt.state !== "completed") throw new Error(`归档操作仍为 ${receipt.state}；关闭清理尚未确认完成，请核对原回执 ${receipt.command_id}`);
  };

  const runArchiveBatch = async (targets: ConversationThread[]) => {
    if (!targets.length) return;
    const batchId = `archive-batch-${Date.now().toString(36)}`;
    let okCount = 0;
    const failures: string[] = [];
    for (const thread of targets) {
      const commandKey = `${batchId}:${thread.thread_id}`;
      const ok = await execute(async () => {
        const receipt = await sendConversationCommand(
          thread.thread_id,
          "conversation.thread.archive",
          {},
          { commandId: commandKey, idempotencyKey: commandKey },
        );
        if (receipt.state !== "completed") throw new Error(`归档清理尚未确认（${receipt.state}），原回执 ${receipt.command_id}`);
      }, "command", `${conversationStorageScope()}::${composerDraftKey(thread.thread_id)}`);
      if (ok) okCount += 1;
      else failures.push(thread.title || thread.thread_id);
    }
    if (failures.length) {
      setNotice(`已归档 ${okCount}/${targets.length}；失败：${failures.join("、")}`);
    } else {
      setNotice(`已归档 ${okCount}/${targets.length}`);
    }
  };

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
      setNotice(archived ? "已取消归档（未恢复执行）" : "已归档", "success");
    }, "command", `${conversationStorageScope()}::${composerDraftKey(target)}`);
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

  const handleDeleteProject = (project: ConversationProject) => {
    const live = threads.filter((thread) => thread.project_id === project.project_id && thread.state.status !== "archived").length;
    if (live) {
      setNotice(`项目「${project.name}」还有 ${live} 个未归档对话，请先归档再删除`);
      return;
    }
    setProjectDeletion([project]);
  };

  const handleDeleteEmptyProjects = (targets: ConversationProject[]) => {
    if (!targets.length) {
      setNotice(`没有可删除的空${projectNoun}`);
      return;
    }
    setProjectDeletion(targets);
  };

  const confirmProjectDeletion = () => {
    const targets = projectDeletion;
    setProjectDeletion(null);
    if (!targets?.length) return;
    const targetIds = new Set(targets.map((project) => project.project_id));
    const leavingCurrent = Boolean(threadId && threads.some((thread) => thread.thread_id === threadId && targetIds.has(thread.project_id || "")));
    let deleted = 0;
    let deletedThreads = 0;
    void execute(async () => {
      const failures: string[] = [];
      for (const project of targets) {
        try {
          const result = await deleteConversationProject(project.project_id);
          deleted += 1;
          deletedThreads += result.deletedThreads;
        } catch (exc) {
          failures.push(`${project.name}：${exc instanceof Error ? exc.message : String(exc)}`);
        }
      }
      await loadProjects({ silent: true });
      if (leavingCurrent) startNewChat();
      if (failures.length) throw new Error(`已删除 ${deleted}/${targets.length} 个${projectNoun}；失败：${failures.join("；")}`);
    }, "command", `${conversationStorageScope()}::project-delete`).then((ok) => {
      if (ok) setNotice(`已删除 ${deleted} 个${projectNoun}${deletedThreads ? `及 ${deletedThreads} 个已归档${projectScope === "src" ? "研究" : "对话"}` : ""}`, "success");
    });
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
        setNotice("已归档", "success");
      }, "command", `${conversationStorageScope()}::${composerDraftKey(target)}`);
      return;
    }
    void runArchiveBatch(pending.threads);
  };

  const handleRename = (tId: string, newTitle: string) => {
    if (preview) { setNotice("演示数据只读，不会修改真实对话。", "warning"); return; }
    if (!tId || !newTitle.trim()) return;
    void execute(async () => {
      await sendConversationCommand(tId, "conversation.thread.rename", {
        title: newTitle.trim(),
      });
      setRenameTarget(null);
      setNotice("标题已更新", "success");
    }, "command", `${conversationStorageScope()}::${composerDraftKey(tId)}`);
  };

  const sidebarSelectThread = useLatestCallback((id: string) => {
    setMobileSidebarOpen(false);
    const snap = paneLayoutStore.getSnapshot();
    if (countPanes(snap.root) > 1 && !paneLayoutStore.isPrimaryPane(snap.activePaneId)) {
      paneLayoutStore.setPaneThread(snap.activePaneId, id);
      return;
    }
    if (id === threadId && !searchParams.size) return;
    const href = threadHref(id);
    if (!experience && !isolatedChrome && /^\/chat\/[^/]+$/.test(pathname)) {
      window.history.pushState(null, "", href);
    } else {
      router.push(href);
    }
  });
  const sidebarNewChat = useLatestCallback(() => {
    const previousDraft = threadId ? latestNewComposerDraftId({ scope: draftScope }) : null;
    if (previousDraft !== null) {
      setMobileSidebarOpen(false);
      router.push(newThreadHref(previousDraft));
      return;
    }
    startNewChat();
  });
  const sidebarNewChatForProject = useLatestCallback((nextProjectId: string) => {
    const previousDraft = threadId ? latestNewComposerDraftId({ scope: draftScope, projectId: nextProjectId }) : null;
    if (previousDraft !== null) {
      setMobileSidebarOpen(false);
      router.push(newThreadHref(previousDraft));
      return;
    }
    startNewChat(nextProjectId);
  });
  const openRepoLessProjectDialog = useLatestCallback(() => {
    setRepoLessCreateName("");
    setRepoLessCreateDesc("");
    setRepoLessCreateError("");
    setRepoLessCreateOpen(true);
  });
  const sidebarCreateFolder = useLatestCallback(() => void handleCreateProjectFromDirectory());
  const sidebarRenameThread = useLatestCallback((t: ConversationThread) => {
    setRenameTarget(t);
    setRenameInput(t.title);
  });
  const sidebarRenameThreadTitle = useLatestCallback((id: string, title: string) => {
    if (title.trim() && title.trim() !== threads.find((item) => item.thread_id === id)?.title) handleRename(id, title);
  });
  const sidebarForkThread = useLatestCallback((t: ConversationThread) => handleFork(t.thread_id, t.state.running_turn_id || undefined));
  const sidebarArchiveThread = useLatestCallback((t: ConversationThread) => handleArchive(t.thread_id));
  const sidebarArchiveProjectThreads = useLatestCallback(handleArchiveProjectThreads);
  const sidebarDeleteProject = useLatestCallback(handleDeleteProject);
  const sidebarDeleteEmptyProjects = useLatestCallback(handleDeleteEmptyProjects);
  const sidebarSelectMessageHit = useLatestCallback((nextThreadId: string, messageId: string) => {
    pushJumpBackFromThread(threadId);
    setMobileSidebarOpen(false);
    router.push(threadMessageHref(nextThreadId, messageId));
  });

  const handleApprovalDecision = (
    approvalId: string,
    decision: "allow" | "deny",
    scopeMode?: "once" | "session",
    optionId?: string,
    note?: string,
  ) => {
    if (!threadId || !approvalId) return;
    const state = conversation.view?.state;
    const pending = state?.pending_approvals?.[approvalId]
      || (state?.pending_approval?.approval_id === approvalId ? state.pending_approval : null);
    if (conversation.view?.thread.thread_id !== threadId || !pending || (pending.status || "pending") !== "pending") {
      setError("该审批已过期或正在投递，请刷新当前对话");
      return;
    }
    const isPlanExit = String(pending.approval_kind || pending.kind || "").toLowerCase().replace(/-/g, "_") === "plan_exit";

    void execute(async () => {
      const payload: Record<string, unknown> = {
        approval_id: approvalId,
        decision,
        scope: isPlanExit ? "once" : (scopeMode === "session" ? "session" : "once"),
        ...(optionId ? { option_id: optionId } : {}),
      };
      if (isPlanExit && decision === "deny") payload.note = note ?? "";
      const intent = await beginConversationControl(threadId, approvalId, "approval", payload);
      if (!intent) { setNotice("该审批正在确认，请恢复原回执；未再次投递决定", "warning"); return; }
      const leavingPlan = isPlanExit && decision === "allow";
      if (leavingPlan) commitInteractionMode("default");
      try {
        const receipt = await sendControlIntent(intent);
        const done = classifyCommandReceipt(receipt) === "completed";
        const summary = isPlanExit
          ? (decision === "allow" ? "已开始实施，规划模式已关闭" : "已继续规划")
          : (decision === "allow" ? "已允许操作" : "已拒绝操作");
        setNotice(done ? summary : "审批决定已受理，等待确认", done ? "success" : "accepted", receipt.command_id);
      } catch (failure) {
        if (leavingPlan) commitInteractionMode("plan");
        throw failure;
      }
    });
  };

  const handleUserInputResolve = async (payload: {
    request_id: string;
    decision: "submit" | "cancel" | "decline";
    answers?: Record<string, { values: string[]; text?: string }>;
    text?: string;
  }) => {
    const pendingId = conversation.view?.state.pending_user_input?.request_id;
    if (!threadId) return false;
    if (!pendingId || pendingId !== payload.request_id) {
      setError("该输入请求已过期，请刷新后重试");
      return false;
    }

    let confirmed = false;
    await execute(async () => {
      const intent = await beginConversationControl(threadId, payload.request_id, "input", {
        request_id: payload.request_id,
        decision: payload.decision,
        answers: payload.answers || {},
        text: payload.text || "",
      });
      if (!intent) { setNotice("该回答正在确认，请恢复原回执；原输入保留且未重复提交", "warning"); return; }
      const receipt = await sendControlIntent(intent);
      confirmed = classifyCommandReceipt(receipt) === "completed";
      setNotice(confirmed ? (payload.decision === "cancel" ? "已取消输入" : payload.decision === "decline" ? "已拒绝回答" : "回答已提交") : "回答操作已受理，等待确认", confirmed ? "success" : "accepted", receipt.command_id);
    });
    return confirmed;
  };

  const finishSteerControl = useCallback((intent: ControlIntent, ownerScope: string) => {
    if (ownerScope !== conversationStorageScope()) return;
    if (!listConversationControls(intent.threadId).some((row) => row.commandId === intent.commandId)) return;
    const text = String(intent.payload.text || "");
    const key = intent.composer?.draftKey || composerDraftKey(intent.threadId);
    recordSentPrompt(key, { prompt: text, promptSegments: documentFromPromptAndRefs(text, []).segments, capabilityRefs: [], projectId: intent.composer?.projectId || "" });
    const live = composerSnapshotRef.current;
    if (live.draftKey === key && !live.attachments.length && intent.composer?.documentFingerprint === fingerprintPromptDocument(live.promptDocument)) {
      historyIndexRef.current = null; setHistoryIndex(null);  setPromptDocument(emptyPromptDocument());  clearComposerDraft(key);
    }
    clearConversationControl(intent, ownerScope);
    if (live.draftKey === key) writeNotice("引导已确认接收", "success");
  }, [writeNotice]);

  const sendControlIntent = async (intent: ControlIntent) => {
    const ownerScope = conversationStorageScope();
    if (intent.ownerScope && intent.ownerScope !== ownerScope) throw new Error("conversation.control.scope_changed: 原控制动作属于另一工作台，未再次投递");
    try {
      const type = intent.kind === "steer" ? "conversation.turn.steer" : intent.kind === "approval" ? "conversation.approval.resolve" : "conversation.user_input.resolve";
      const receipt = await sendConversationCommand(intent.threadId, type, intent.payload, { commandId: intent.commandId, idempotencyKey: intent.commandId });
      updateConversationControl(intent, { state: classifyCommandReceipt(receipt) === "completed" ? "completed" : classifyCommandReceipt(receipt) === "pending" ? "accepted" : "unknown" }, ownerScope);
      return receipt;
    } catch (failure) {
      const error = failure && typeof failure === "object" ? failure as { category?: string; detail?: Record<string, unknown> } : null;
      if (error && error.detail?.delivery_unknown !== true && error.detail?.outcome_unknown !== true && ["validation", "permission", "auth", "not_found", "conflict", "state"].includes(error.category || "")) clearConversationControl(intent, ownerScope);
      else updateConversationControl(intent, { state: "unknown", error: failure instanceof Error ? failure.message : String(failure) }, ownerScope);
      throw failure;
    }
  };
  const recoverControlIntent = async (intent: ControlIntent) => {
    const ownerScope = conversationStorageScope();
    await execute(async () => {
      const receipt = await recoverCommandReceipt(intent.commandId, 1);
      if (ownerScope !== conversationStorageScope()) return;
      if (receipt && classifyCommandReceipt(receipt) === "failed") {
        clearConversationControl(intent); setNotice(receipt.error?.message || "原控制动作已明确失败，输入保留，可按当前请求重新处理", "warning");
      } else if (classifyCommandReceipt(receipt) === "completed" && intent.kind === "steer") {
        finishSteerControl(intent, ownerScope);
      } else {
        updateConversationControl(intent, { state: classifyCommandReceipt(receipt) === "completed" ? "completed" : "unknown" });
        setNotice(classifyCommandReceipt(receipt) === "completed" ? "原控制动作已确认完成，正在同步卡片" : "原控制动作结果仍未确认，未创建或投递新决定", classifyCommandReceipt(receipt) === "completed" ? "success" : "warning");
      }
    });
  };

  useEffect(() => {
    const steers = pendingControls.filter((intent) => intent.kind === "steer");
    if (!steers.length) return;
    const ownerScope = conversationStorageScope(), controller = new AbortController();
    let cancelled = false, timer: ReturnType<typeof setTimeout> | null = null;
    const poll = async () => {
      for (const intent of steers) {
        try {
          const receipt = await fetchCommandReceipt(intent.commandId, { signal: controller.signal });
          if (cancelled || ownerScope !== conversationStorageScope()) return;
          if (classifyCommandReceipt(receipt) === "completed") finishSteerControl(intent, ownerScope);
          else if (receipt && classifyCommandReceipt(receipt) === "failed") {
            clearConversationControl(intent, ownerScope); writeNotice(receipt.error?.message || "引导未完成，原输入保留", "warning");
          }
        } catch (error) { if (!cancelled) writeNotice(`引导回执待核对：${error instanceof Error ? error.message : String(error)}`, "warning"); }
      }
      if (!cancelled) timer = setTimeout(() => void poll(), 1500);
    };
    void poll();
    return () => { cancelled = true; controller.abort(); if (timer) clearTimeout(timer); };
  }, [pendingControls, finishSteerControl, writeNotice]);

  const handleAddMemory = async (content: string) => {
    if (preview) { setNotice("演示数据只读，不会写入长期记忆。", "warning"); return; }
    if (!threadId) return;
    const ownerScope = conversationStorageScope();
    await recordConversationMemory(threadId, content);
    if (ownerScope !== conversationStorageScope() || composerSnapshotRef.current.draftKey !== composerDraftKey(threadId)) return;
    await loadMemory();
    setNotice("长期记忆已保存", "success");
  };

  const handleDeleteMemory = async (memId: string) => {
    if (preview) { setNotice("演示数据只读，不会删除长期记忆。", "warning"); return; }
    if (!threadId) return;
    const ownerScope = conversationStorageScope();
    await deleteConversationMemory(threadId, memId);
    if (ownerScope !== conversationStorageScope() || composerSnapshotRef.current.draftKey !== composerDraftKey(threadId)) return;
    await loadMemory();
    setNotice("长期记忆已删除", "success");
  };

  const handleAddFiles = (files: FileList | File[]) => {
    const picked = Array.from(files);
    if (!picked.length) return;
    void (async () => {
      const processed = await Promise.all(picked.map(async (raw) => {
        const scaled = await scaleImageForUpload(raw);
        if (scaled) setNotice(scaled.note);
        return scaled ? scaled.file : raw;
      }));
      setAttachments((curr) => [
        ...curr,
        ...processed.map((file) => ({
          id: newAttachmentId(),
          name: file.name,
          size: file.size,
          file,
          type: file.type,
          needsReselect: false,
          uploadStatus: "pending" as const,
        })),
      ]);
    })();
  };

  const handleSendRef = useRef(handleSend);
  handleSendRef.current = handleSend;
  const sideSendReady = Boolean(queuedSideSend && threadId && credentialId && model && runtimeKey)
    && runtimeSyncedKeyRef.current === composerDraftKey(threadId) && !primarySendBlock(readiness) && !busy;
  useEffect(() => {
    if (!sideSendReady) return;
    const text = queuedSideSend;
    setQueuedSideSend("");
    void handleSendRef.current(undefined, { text });
  }, [sideSendReady, queuedSideSend]);
  const [attachSendNonce, setAttachSendNonce] = useState(0);
  useEffect(() => {
    if (attachSendNonce) void handleSendRef.current();
  }, [attachSendNonce]);

  const appendComposerText = (excerpt: string) => {
    setPromptDocument((current) => {
      const currentText = flattenPromptDocument(current).trimEnd();
      const next = currentText ? `${currentText}\n\n${excerpt}` : excerpt;
      return documentFromPromptAndRefs(next, refsFromDocument(current));
    });
  };

  const appendComposerTextRef = useRef(appendComposerText);
  appendComposerTextRef.current = appendComposerText;
  const dockFocusRef = useRef(dockFocus);
  dockFocusRef.current = dockFocus;
  const composerHandle = previewHost?.composerHandle;
  useEffect(() => {
    if (!composerHandle) return;
    const focusEnd = () => window.requestAnimationFrame(() => {
      const editor = shellRef.current?.querySelector<HTMLElement>("[data-c34-composer]");
      if (!editor) return;
      editor.focus({ preventScroll: true });
      if (editor instanceof HTMLTextAreaElement) {
        editor.setSelectionRange(editor.value.length, editor.value.length);
        return;
      }
      const selection = window.getSelection();
      if (!selection) return;
      const range = document.createRange();
      range.selectNodeContents(editor);
      range.collapse(false);
      selection.removeAllRanges();
      selection.addRange(range);
    });
    composerHandle.current = {
      insertText: (text, options) => {
        appendComposerTextRef.current(text);
        if (options?.focus !== false) focusEnd();
      },
      focus: focusEnd,
      showReplies: () => { if (dockFocusRef.current) setDockRepliesOpen(true); },
    };
    return () => { composerHandle.current = null; };
  }, [composerHandle]);

  const handleRetryAttachment = (index: number) => {
    const attachment = attachments[index];
    if (!attachment) return;
    const action = composerAttachmentRecoveryAction(attachment);
    if (action === "reselect") {
      if (!attachment.id) { setError("此附件缺少可恢复身份，请移除后重新添加；当前正文保留"); return; }
      reselectSourceRef.current = { attachmentId: attachment.id, draftKey, scope: conversationStorageScope() };
      reselectInputRef.current?.click();
      return;
    }
    if (action !== "retry") return;
    setAttachments((prev) => prev.map((a, i) =>
      i === index
        ? { ...a, uploadStatus: "pending" as const, uploadError: undefined, artifactId: undefined }
        : a,
    ));
  };

  const handleReselectFile = (e: React.ChangeEvent<HTMLInputElement>) => {
    const picked = e.target.files?.[0];
    e.target.value = "";
    const source = reselectSourceRef.current;
    reselectSourceRef.current = null;
    if (!picked || !source || source.draftKey !== composerSnapshotRef.current.draftKey || source.scope !== conversationStorageScope()) return;
    setAttachments((prev) => prev.map((a) =>
      a.id === source.attachmentId
        ? {
            ...a,
            id: newAttachmentId(),
            name: picked.name,
            size: picked.size,
            file: picked,
            type: picked.type,
            needsReselect: false,
            uploadStatus: "pending" as const,
            uploadError: undefined,
            artifactId: undefined,
            cacheId: undefined,
          }
        : a,
    ));
    setNotice("");
  };

  const largePasteSourceRef = useRef<{ draftKey: string; scope: string; fingerprint: string } | null>(null);
  const handleLargePaste = (text: string, selection?: LargePasteSelection) => {
    largePasteSourceRef.current = { draftKey, scope: conversationStorageScope(), fingerprint: fingerprintPromptDocument(promptDocument) };
    setLargePasteText(text);
    setLargePasteSelection(selection ?? null);
  };

  const largePasteMatches = () => largePasteSourceRef.current?.draftKey === draftKey && largePasteSourceRef.current?.scope === conversationStorageScope() && largePasteSourceRef.current?.fingerprint === fingerprintPromptDocument(promptDocument);
  const handleLargePasteKeepText = () => {
    if (!largePasteText) return;
    if (!largePasteMatches()) { setError("粘贴时的草稿或选区已改变，请复制完整粘贴内容后重新选择插入位置"); return; }
    const next = keepLargePasteAsText(promptDocument, largePasteText, largePasteSelection);
    handlePromptDocumentChange(next);
    setLargePasteText(null);
    setLargePasteSelection(null);
  };

  const handleLargePasteAsAttachment = () => {
    if (!largePasteText) return;
    if (!largePasteMatches()) { setError("粘贴来源属于另一份草稿，请复制完整内容后回到原草稿添加附件"); return; }
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
  const panelMaximized = rightPanelOpen && panel.maximized && !panelSheet && !dockFocus;
  const hasWorkspace = Boolean(conversation.view?.workspace?.root_path);
  const subagentCounts = threadSubagentCounts(conversation.view?.state);
  const agentsPanelShown = rightPanelOpen && activeSurface?.kind === "agents";
  const openAgentsPanel = () => {
    if (!threadId) return;
    setDetailsPayload(null);
    setRightPanelInstant(false);
    chatPanel.open(threadId, "agents");
  };
  const delegatedThreadIds = useMemo(
    () => (conversation.view?.state.subagents || []).map((node) => node.agent_id).filter(Boolean),
    [conversation.view?.state.subagents],
  );
  const panelControls = conversation.view && threadId ? (
    <div ref={panelControlsRef} className="flex items-center gap-1" data-testid="conversation-panel-controls">
      {experience?.headerActions}
      {!sideOf ? (
        <PaneLayoutMenu
          paneId={paneChild?.paneId || "pane-primary"}
          currentThreadId={threadId}
          delegatedThreadIds={delegatedThreadIds}
          compact={Boolean(paneChild)}
        />
      ) : null}
      {paneChild && !paneChild.isPrimary ? (
        <IconButton
          size="sm"
          icon="x"
          label="关闭此窗格"
          onClick={paneChild.onClose}
          data-testid={`pane-close-${paneChild.paneId}`}
        />
      ) : null}
      <IconButton
        size="md"
        ref={detailsButtonRef}
        icon="alignJustify"
        label={threadDetails.visible ? "隐藏对话详情" : "显示对话详情"}
        active={threadDetails.visible}
        aria-expanded={threadDetails.visible}
        data-testid="thread-details-toggle"
        onClick={threadDetails.toggle}
      />
      {!panelExtensions?.exclusive ? (
        <IconButton
          size="md"
          icon="gitCompare"
          label="变更"
          shortcut="mod+alt+d"
          active={rightPanelOpen && activeSurface?.kind === "diff"}
          disabled={!hasWorkspace && !(conversation.view.artifacts || []).length}
          onClick={() => { setDetailsPayload(null); setRightPanelInstant(false); chatPanel.toggle(threadId, "diff"); }}
        />
      ) : null}
      {subagentCounts.total ? (
        <IconButton
          size="md"
          icon="bot"
          label={subagentCounts.active ? `子代理 · ${subagentCounts.active} 个进行中` : `子代理 · 共 ${subagentCounts.total} 个`}
          shortcut="mod+alt+a"
          active={agentsPanelShown}
          badge={subagentCounts.active && !agentsPanelShown ? subagentCounts.active : undefined}
          data-testid="conversation-agents-toggle"
          onClick={() => { setDetailsPayload(null); setRightPanelInstant(false); chatPanel.toggle(threadId, "agents"); }}
        />
      ) : null}
      <IconButton
        size="md"
        icon={rightPanelOpen ? "panelRightClose" : "panelRightOpen"}
        label={rightPanelOpen ? "关闭工作面板" : "打开工作面板"}
        shortcut="mod+alt+b"
        active={rightPanelOpen}
        badge={runningToolCount > 0 && !rightPanelOpen ? runningToolCount : undefined}
        onClick={() => {
          setDetailsPayload(null);
          setRightPanelInstant(false);
          if (experience?.panelExtensions) togglePanelWithExtensions(threadId, panelExtensions);
          else chatPanel.toggle(threadId);
        }}
      />
    </div>
  ) : null;

  return (
    <AgentPageContext.Provider value={openAgentPage}>
    <ConversationLocalPreviewContext.Provider value={Boolean(preview)}>
    <PanelExtensionContext.Provider value={panelExtensions}>
    <ConversationNavigationContext.Provider value={navigation}>
    <ThreadEditorProvider view={conversation.view}>
    <>
    <div
      ref={shellRef}
      className="cx-root ai-conv-shell relative flex h-full w-full overflow-hidden"
      data-sidebar-collapsed={sidebarCollapsed ? "true" : "false"}
      data-mobile-sidebar-open={mobileSidebarOpen ? "true" : "false"}
      data-panel-open={rightPanelOpen ? "true" : "false"}
      data-panel-docked={dockFocus ? "true" : undefined}
      style={dockFocus
        ? ({ "--cx-panel-dock-inset": `${dockInset + (dockRepliesOpen ? dockReplyHeight : 0)}px` } as React.CSSProperties)
        : undefined}
    >
      <input
        ref={fileInputRef}
        type="file"
        multiple
        className="hidden"
        onChange={handleFileUpload}
      />
      <input
        ref={reselectInputRef}
        type="file"
        className="hidden"
        data-testid="composer-reselect-file"
        onChange={handleReselectFile}
      />

      {!isolatedChrome ? <ConversationSidebar
        sidebarId={sidebarId}
        threadHref={threadHref}
        labels={experience?.sidebarLabels}
        preferencesScope={projectScope}
        restrictSearchToThreads
        includeSrcInSearch={defaultMode === "src"}
        projectNavigationOnly={Boolean(experience?.projectNavigationOnly)}
        threads={sidebarThreads}
        projects={sidebarProjects}
        activeThreadId={threadId}
        onSelectThread={sidebarSelectThread}
        onNewChat={sidebarNewChat}
        onNewChatForProject={experience?.hideProjects ? undefined : sidebarNewChatForProject}
        onCreateFolder={experience?.hideProjects ? undefined : (projectScope === "src" ? openRepoLessProjectDialog : sidebarCreateFolder)}
        onDeleteProject={experience?.hideProjects || preview ? undefined : sidebarDeleteProject}
        onDeleteEmptyProjects={experience?.hideProjects || preview ? undefined : sidebarDeleteEmptyProjects}
        searchQuery={searchQuery}
        onSearchChange={setSearchQuery}
        onRenameThread={preview ? undefined : sidebarRenameThread}
        onRenameThreadTitle={preview ? undefined : sidebarRenameThreadTitle}
        onForkThread={experience?.allowFork === false ? undefined : sidebarForkThread}
        onArchiveThread={experience?.projectNavigationOnly || preview ? undefined : sidebarArchiveThread}
        onArchiveProjectThreads={experience?.projectNavigationOnly || experience?.hideProjects ? undefined : sidebarArchiveProjectThreads}
        onSelectMessageHit={sidebarSelectMessageHit}
        onMarkThreadsRead={markThreadsRead}
        collapsed={sidebarCollapsed}
        onCollapse={toggleSidebarCollapsed}
        width={sidebarWidth}
        onWidthChange={setSidebarWidth}
        onWidthPreview={previewSidebarWidth}
        className={mobileSidebarOpen ? "open" : ""}
      /> : null}

      {!isolatedChrome ? (
        <button
          type="button"
          className="conversation-mobile-sidebar-backdrop"
          data-open={mobileSidebarOpen ? "true" : "false"}
          aria-label="关闭对话导航"
          aria-hidden={!mobileSidebarOpen}
          inert={!mobileSidebarOpen || undefined}
          tabIndex={-1}
          onClick={() => setMobileSidebarOpen(false)}
        />
      ) : null}

      {(() => {
        const primaryMain = (
          <main
            ref={mainRef}
            id={paneChild ? `${mainContentId}-${paneChild.paneId}` : mainContentId}
            tabIndex={-1}
            className={cn("ai-conv-main relative flex min-w-0 flex-1 flex-col overflow-hidden bg-cx-bg", !isolatedChrome && agentPage && agentPageSheet && "invisible") }
            style={headerSlot && !isolatedChrome && paneCount <= 1 ? ({ "--cx-header-h": "0px" } as React.CSSProperties) : undefined}
            aria-label="对话主内容"
            inert={mobileSidebarOpen || panelMaximized || undefined}
            aria-hidden={mobileSidebarOpen || panelMaximized || undefined}
          >
        {conversation.view ? (
          <>
            {sideChat ? <header className="flex h-9 shrink-0 items-center gap-1.5 border-b border-cx-border-subtle px-3 text-cx-xs text-cx-fg-3" data-testid="side-chat-header">
              <Icon name="gitFork" size={13} className="shrink-0 text-cx-fg-4" />
              <span className="min-w-0 truncate" title={sideOf?.title}>
                {runSideChat ? (
                  conversation.view?.thread.side_chat?.target_kind === "worker" && conversation.view?.thread.side_chat?.worker_id
                    ? `来自出战 Worker ${conversation.view.thread.side_chat.worker_id}${conversation.view.thread.side_chat.target_model ? ` · ${conversation.view.thread.side_chat.target_model}` : ""}`
                    : `来自出战 Agent · ${conversation.view?.thread.side_chat?.target_label || conversation.view?.thread.side_chat?.target_engine || "Run"}`
                ) : "来自主对话"}
              </span>
              <span aria-hidden="true" className="text-cx-fg-4">·</span>
              <span className={cn("flex shrink-0 items-center gap-1", sideChat.parentStatus.tone === "attention" ? "text-cx-warning" : sideChat.parentStatus.tone === "failed" ? "text-cx-danger" : sideChat.parentStatus.tone === "running" ? "text-cx-accent" : undefined)} data-testid="side-chat-parent-status">
                {sideChat.parentStatus.tone === "running" ? <span className="size-1.5 animate-pulse rounded-full bg-cx-accent" /> : null}
                {sideChat.parentStatus.label}
              </span>
              {runSideChat ? (
                <span
                  className="ml-auto shrink-0 text-cx-fg-4"
                  data-testid="side-chat-context-badge"
                  title={
                    conversation.view?.thread.side_chat?.target_kind === "worker"
                      ? `主对话 ${conversation.view?.thread.side_chat?.inherited_messages ?? 0} 条消息 · Worker 轨迹 ${conversation.view?.thread.side_chat?.inherited_worker_events ?? 0} 条 · SharedGraph 事实 ${conversation.view?.thread.side_chat?.inherited_facts ?? 0} 条`
                      : `主对话 ${conversation.view?.thread.side_chat?.inherited_messages ?? 0} 条消息 · SharedGraph 事实 ${conversation.view?.thread.side_chat?.inherited_facts ?? 0} 条 · 含出战 Worker 概览`
                  }
                >
                  {conversation.view?.thread.side_chat?.native_fork
                    ? "原生分叉 · 完整上下文"
                    : conversation.view?.thread.side_chat?.target_kind === "worker"
                      ? "继承完整工作上下文"
                      : "继承全局 Run 上下文"}
                </span>
              ) : typeof conversation.view?.thread.side_chat?.inherited_messages === "number" ? (
                <span className="ml-auto shrink-0 text-cx-fg-4" title="这些主对话消息作为参考上下文交给模型，不会显示在侧边聊天里">继承 {conversation.view.thread.side_chat.inherited_messages} 条消息</span>
              ) : null}
            </header> : sideOf ? <header className="flex h-12 shrink-0 items-center gap-2 border-b border-cx-border px-3" data-testid="agent-page-header">
              {sideOf.overlay ? <IconButton icon="arrowLeft" label={`返回“${sideOf.title}”`} onClick={sideOf.onClose} /> : null}
              {sideOf.overlay ? <span className="shrink-0 rounded bg-cx-hover px-2 py-0.5 text-cx-xs text-cx-fg-3">委派线程</span> : null}
              <span aria-label={running ? "工作中" : "已完成"} className={cn("size-1.5 shrink-0 rounded-full", running ? "animate-pulse bg-cx-accent" : "bg-cx-fg-4")} />
              <span className="min-w-0 flex-1 truncate text-cx-sm font-medium">{conversation.view.thread.title}</span>
              {nativeMirror ? <span className="shrink-0 text-cx-xs text-cx-fg-3">此处只读。</span> : null}
              {!sideOf.overlay ? <IconButton icon="x" label="关闭委派线程" onClick={sideOf.onClose} /> : null}
            </header> : renderHeader(<ConversationHeader
              view={conversation.view}
              projectName={activeProjectName}
              panelControls={panelControls}
              bottomPanelOpen={bottomPanelOpen}
              onToggleBottomPanel={() => {
                setBottomPanelInstant(false);
                setBottomPanelOpen((open) => !open);
              }}
              className={headerSlot ? "h-full border-b-0 bg-transparent pr-0 sm:pr-0" : undefined}
              sidebarId={sidebarId}
              threadHref={threadHref}
              sidebarToggle={sidebarCollapsed && !headerSlot ? (
                <IconButton
                  icon="panelLeft"
                  label="展开对话导航"
                  aria-expanded={false}
                  aria-controls={sidebarId}
                  onClick={toggleSidebarCollapsed}
                  className="max-md:hidden"
                />
              ) : undefined}
              onOpenPendingSend={openPendingSend}
              onNewInProject={() => startNewChat(conversation.view?.thread.project_id || undefined)}
              onOpenInfo={() => setInfoDrawerOpen(true)}
              onOpenReadingPrefs={() => setReadingPrefsOpen((v) => !v)}
              readingPrefsOpen={readingPrefsOpen}
              onOpenShortcuts={() => {
                setShortcutsHelpInstant(false);
                setShortcutsHelpOpen(true);
              }}
              onExport={preview ? undefined : () => setExportDialogOpen(true)}
              onFork={experience?.allowFork === false ? undefined : () => handleFork(threadId)}
              onRename={preview ? undefined : (newT) => handleRename(threadId, newT)}
              onArchive={experience?.projectNavigationOnly || preview ? undefined : () => handleArchive(threadId)}
              busy={interactionBusy}
            />)}
            <ConversationThreadDetails
              view={conversation.view}
              projectName={activeProjectName}
              state={threadDetails}
              anchorRef={detailsButtonRef}
              onOpenInfo={() => setInfoDrawerOpen(true)}
              onOpenArtifact={(artifact) => openDetails({
                type: "artifact",
                title: artifact.name || "上下文",
                subtitle: artifact.artifactId,
                artifact: {
                  artifactId: artifact.artifactId,
                  name: artifact.name,
                  kind: artifact.kind,
                  mediaType: artifact.mediaType,
                  size: artifact.size,
                  threadId: conversation.view?.thread.thread_id,
                },
              })}
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

        {conversation.view && !preview ? (
          <AgentActivityDock threadId={conversation.view.thread.thread_id} running={Boolean(conversation.view.state.running_turn_id)} />
        ) : null}

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
                  <ConversationRouteLink
                    href="/settings/agents"
                    className="mt-1 flex w-fit items-center gap-1.5 text-cx-fg-2 hover:text-cx-fg hover:underline"
                  >
                    <Icon name="plug" size={13} />
                    检查 Agent 接入
                  </ConversationRouteLink>
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
                  tone={noticeControls.kind === "warning" ? "warning" : noticeControls.kind === "progress" || noticeControls.kind === "accepted" ? "running" : noticeControls.kind === "success" ? "success" : "neutral"}
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
                  <span data-send-phase={noticeControls.kind}>{notice}</span>
                </Callout>
              </div>
            )}
          </div>
        ) : null}

        {readSendIntent(draftKey)?.phase === "unknown" ? (
          <div className="flex justify-center px-4 pt-3">
            <Button size="sm" variant="soft" icon="circleAlert" onClick={openPendingSend}>处理上次未确认的请求</Button>
          </div>
        ) : null}
        {pendingControls.length ? (
          <div className="mx-4 mb-2 rounded-xl border border-cx-warning/30 bg-cx-warning-soft p-3 text-cx-xs">
            <p>控制动作正在确认。卡片保留原命令身份，核对回执不会再投递决定或回答。</p>
            {pendingControls.map((intent) => (
              <div key={intent.commandId} className="mt-2 flex flex-col gap-1.5">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="min-w-0 flex-1 break-all">{intent.kind === "steer" ? "引导" : intent.kind === "approval" ? "审批" : "回答"} · {intent.state}</span>
                  <Button size="sm" variant="secondary" disabled={busy} onClick={() => void recoverControlIntent(intent)}>核对原回执</Button>
                </div>
                <details>
                  <summary className="cursor-pointer select-none text-cx-fg-2">原请求</summary>
                  <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap break-words">{JSON.stringify(intent, null, 2)}</pre>
                </details>
              </div>
            ))}
          </div>
        ) : null}
        {!threadId ? (
          <ConversationHome
            {...experience?.home}
            draftKey={draftKey}
            prompt={prompt}
            onPromptChange={handlePromptChange}
            promptDocument={promptDocument}
            onPromptDocumentChange={handlePromptDocumentChange}
            onSubmit={() => void handleSend()}
            busy={busy}
            credentials={credentials}
            selectedCredentialId={credentialId}
            selectedModel={model}
            selectedEffort={effort}
            composerModesRef={composerModesRef}
            selectedServiceTier={serviceTier}
            selectedAccessMode={accessMode}
            accessModes={accessModes}
                        accessModeReason={accessModeReason}
            interactionMode={interactionMode}
            planModeAvailable={planModeSupport.available}
            planModeReason={planModeSupport.reason}
            onInteractionModeChange={handleInteractionModeChange}
            onSelectModelParams={handleSelectModelParams}
            runtimes={runtimes}
            runtimeKey={runtimeKey}
            onRuntimeChange={handleRuntimeChange}
            projects={projects}
            selectedProjectId={projectId}
            onProjectChange={handleProjectChange}
            onCreateProject={handleCreateProjectFromDirectory}
            onCreateFromPath={handleCreateProjectFromPath}
            onOpenRepoLessProjectDialog={experience?.home?.projectSelectionOnly ? openRepoLessProjectDialog : undefined}
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
            workspaceMode={workspaceMode}
            onWorkspaceModeChange={handleWorkspaceModeChange}
            worktreeBranch={worktreeBranch}
            onWorktreeBranchChange={setWorktreeBranch}
            existingWorktreePath={existingWorktreePath}
            undoScope={String(composerUndoEpoch)}
            onRestoreAttachments={setAttachments}
            onExistingWorktreePathChange={setExistingWorktreePath}
            attachments={attachments}
            onAddAttachment={() => fileInputRef.current?.click()}
            onAddFiles={handleAddFiles}
            onRemoveAttachment={(i) => setAttachments((curr) => curr.filter((_, idx) => idx !== i))}
            onRetryAttachment={handleRetryAttachment}
            onLargePaste={handleLargePaste}
            capabilityContext={composerCapabilityContext}
            capabilityRefs={capabilityRefs}
            onComposerCommand={(action: string, sourceDocument?: PromptDocument) => {
              if (action === "new") startNewChat(projectId, true);
              else if (action.startsWith("ui:")) void handleClientCommand(action, "", sourceDocument);
            }}
            projectDefault={projectDefaultControl}
            modelPickerOpen={modelPickerOpen}
            modelPickerInstant={modelPickerInstant}
            onModelPickerOpenChange={handleModelPickerOpenChange}
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
              <h2 className="text-cx-md font-semibold text-cx-fg">无法打开对话</h2>
              <p className="text-cx-sm leading-relaxed text-cx-fg-2">
                {conversation.error || "加载对话失败，请重试。"}
              </p>
              <p className="font-mono text-cx-xs text-cx-fg-4 break-all">{threadId}</p>
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
              className="flex items-center justify-between gap-3 border-b border-cx-danger/30 bg-cx-danger-soft px-4 py-2 text-cx-sm text-cx-danger animate-fade-in"
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
                  className="font-semibold hover:underline"
                >
                  重试
                </Button>
              </div>
            </div>
          ) : null}
          {experience?.emptyThread && conversation.view && conversation.view.messages.length === 0 ? (
            <div className="pointer-events-none absolute inset-0 z-10 mx-auto flex w-full max-w-md flex-col items-center justify-center gap-3 px-6 text-center">
              <Icon name={experience.emptyThread.icon ?? "compass"} size={22} className="text-cx-fg-3" />
              <h2 className="text-cx-md font-semibold text-cx-fg">{experience.emptyThread.title}</h2>
              {experience.emptyThread.hint ? <p className="text-cx-sm leading-5 text-cx-fg-3">{experience.emptyThread.hint}</p> : null}
              {experience.emptyThread.suggestions?.length ? (
                <div className="pointer-events-auto mt-1 flex flex-wrap items-center justify-center gap-2">
                  {experience.emptyThread.suggestions.map((item) => (
                    <button key={item.label} type="button" className="inline-flex h-8 items-center rounded-xl bg-cx-hover px-3 text-cx-sm text-cx-fg-2 transition-colors hover:bg-cx-active hover:text-cx-fg"
                      onClick={() => handlePromptChange(item.prompt)}>{item.label}</button>
                  ))}
                </div>
              ) : null}
            </div>
          ) : null}
          <OpenToolFile value={openToolFile}>
          <ConversationStreamTimeline
            key={commandScope}
            stream={conversation.stream}
            streamThreadId={streamThreadId}
            className="min-h-0 flex-1"
            view={controlView || conversation.view}
            credentials={credentials}
            running={running}
            connected={conversation.connected}
            streamStatus={conversation.streamStatus}
            busy={interactionBusy}
            contentLoading={switchingThread && !threadLoadError}
            contentError={threadLoadError}
            onReloadContent={() => void conversation.refresh()}
            onLoadOlder={conversation.loadOlderMessages}
            onJumpToLatest={async () => {
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
            onApprovalDecision={preview ? () => setNotice("演示数据只读，不会提交审批决定。", "warning") : handleApprovalDecision}
            onUserInputResolve={preview ? async () => { setNotice("演示数据只读，不会提交输入。", "warning"); return false; } : handleUserInputResolve}
            threadId={threadId || ""}
            onRetryTurn={preview ? undefined : handleRetry}
            onEditTurn={preview ? undefined : handleEditTurn}
            onRewindTurn={preview ? undefined : handleRewindTurn}
            rewindDisabled={rewindDisabled}
            rewindReason={rewindReason}
            showSuperseded={showSuperseded}
            onToggleSuperseded={preview ? undefined : handleToggleSuperseded}
            supersededTurns={auditTurns}
            supersededMessages={auditMessages}
            onContinueTurn={preview ? undefined : handleResume}
            onForkTurn={preview || sideChat ? undefined : (tId) => handleFork(threadId, tId)}
            onCiteMessageSpan={handleCiteMessageSpan}
            onAskSideChat={!preview && !sideOf && threadId ? (payload) => {
              void askInSideChat({ ...payload, sourceThreadId: threadId, key: crypto.randomUUID() });
            } : undefined}
            jumpTarget={jumpTarget}
            onJumpTargetConsumed={() => setJumpTarget(null)}
            onJumpToContext={handleJumpToContext}
            annotations={composerAnnotations}
            onOpenPlan={() => {
              setDetailsPayload(null);
              setRightPanelInstant(false);
              chatPanel.open(threadId, "plan");
            }}
            highlightedMessageId={highlightedMessageId}
            suppressRestore={!shouldRestoreReadingPosition(rawMessageParam) || deepLinkDismissed}
            onEnsureMessageVisible={ensureMessageVisible}
            onOpenThread={openAgentPage}
            composerCompact={composerCompact}
            onComposerCompactChange={handleComposerCompactChange}
            turnAccessory={previewHost?.turnAccessory}
            footer={
              <div ref={composerDockRef} className={cn("w-full", dockFocus && "bg-cx-bg")}>
                {!preview && readinessForBanner.blockers.length ? (
                  <ConversationReadinessBanner
                    readiness={readinessForBanner}
                    probing={probing}
                    onRetry={() => void reloadReadiness({ fresh: true })}
                    onProbe={() => void handleProbeAgent()}
                    onDismissGuide={handleDismissGuide}
                  />
                ) : null}
            <ConversationStreamQuotaResume
              stream={conversation.stream}
              view={conversation.view}
              busy={interactionBusy}
              onResume={handleResume}
              onSchedule={async ({ enabled, turnId, resetsAt, kind }) => {
                await sendConversationCommand(conversation.view!.thread.thread_id, "conversation.thread.quota_resume",
                  enabled ? { enabled, turn_id: turnId, resets_at: resetsAt, kind } : { enabled });
              }}
            />
            <ConversationQueue
              key={commandScope}
                  threadId={conversation.view.thread.thread_id}
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
                  onRestoreAsDraft={(text) => { const next = insertPlainTextAtMarkedRange(promptDocument, `${plainTextFromDocument(promptDocument) ? "\n" : ""}${text}`); handlePromptDocumentChange(next); }}
                  onPause={handleQueuePause}
                  onFinishEdit={handleQueueFinishEdit}
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
                    action={experience?.projectNavigationOnly ? undefined : (
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
                    {experience?.projectNavigationOnly
                      ? "研究任务已归档，发送已禁用；请在 SRC 任务工作台调整状态。"
                      : "对话已归档，发送已禁用；草稿仍保留。"}
                  </Callout>
                ) : null}
                {!running && backgroundTurnId ? (
                  <Callout role="status" tone="warning" icon="clock" className="mb-2" testId="native-background-work"
                    action={<Button size="xs" variant="secondary" onClick={handleStop} disabled={interactionBusy}>停止后台任务</Button>}>
                    子任务仍在后台运行，完成后会自动续接回复。
                  </Callout>
                ) : null}
                <ScheduledTasksPill
                  tasks={conversation.view.state.scheduled_tasks || []}
                  onAskCancel={threadArchived ? undefined : (task) => {
                    askAgentInComposer(`请删除你创建的定时任务 ${task.task_id}，不再自动唤醒。`);
                  }}
                  onAskList={threadArchived ? undefined : () => {
                    askAgentInComposer("请列出你当前仍然有效的定时任务。");
                  }}
                />
                {!agentsPanelShown ? (
                  <SubagentActivityPill active={subagentCounts.active} total={subagentCounts.total} onOpen={openAgentsPanel} />
                ) : null}
                {!nativeMirror && !preview && threadId && ((conversation.view?.state.background_tasks ?? 0) > 0 || (conversation.view?.state.background_task_list?.length ?? 0) > 0) ? (
                  <div className="mb-1.5 flex flex-wrap items-center gap-1.5 px-2" data-testid="background-tasks-strip">
                    <span className="text-cx-xs font-medium text-cx-fg-3">
                      后台任务 ({conversation.view?.state.background_task_list?.length || conversation.view?.state.background_tasks || 0})
                    </span>
                    {(conversation.view?.state.background_task_list || []).map((task) => (
                      <span key={task.id} className="inline-flex items-center gap-1 rounded-full border border-cx-border bg-cx-bg-subtle px-2 py-0.5 text-cx-xs text-cx-fg-2">
                        <span className="max-w-[160px] truncate">{task.name || task.id}</span>
                        <button
                          type="button"
                          className="text-cx-fg-4 hover:text-cx-danger"
                          title={`停止后台任务 ${task.name || task.id}`}
                          onClick={() => void sendConversationCommand(threadId, "conversation.background_task.stop", { task_id: task.id })}
                        >
                          ×
                        </button>
                      </span>
                    ))}
                    <Button
                      size="xs"
                      variant="ghost"
                      onClick={() => void sendConversationCommand(threadId, "conversation.background_task.stop", {})}
                    >
                      全部停止
                    </Button>
                  </div>
                ) : null}
                {dockFocus ? (
                  <div className="mb-2 flex min-w-0 items-center gap-2" data-testid="composer-dock-bar">
                    <Button
                      size="md"
                      variant={dockRepliesOpen ? "soft" : "ghost"}
                      icon={dockRepliesOpen ? "chevronDown" : "chevronUp"}
                      aria-expanded={dockRepliesOpen}
                      className="h-[32px]"
                      onClick={() => setDockRepliesOpen((open) => !open)}
                    >
                      {dockRepliesOpen ? "收起回复" : "展开回复"}
                    </Button>
                    <span className="min-w-0 flex-1 truncate text-cx-sm text-cx-fg-3">
                      {dockRepliesOpen ? "回复显示在工作区下方，按 Esc 收起" : dockSnippet || "专注工作区：对话收在这条输入栏里"}
                    </span>
                    <Button size="md" variant="ghost" icon="minimize" className="h-[32px]" onClick={() => { setRightPanelInstant(false); chatPanel.setMaximized(threadId, false); }}>
                      并排显示
                    </Button>
                  </div>
                ) : null}
                {experience?.composerAccessory}
                {!nativeMirror && !preview && threadId ? (
                  <ComposerAsyncUserInput
                    pendingInput={conversation.view?.state.pending_user_input}
                    running={running}
                    threadId={threadId}
                    busy={interactionBusy}
                    onResolve={handleUserInputResolve}
                  />
                ) : null}
                {!nativeMirror && !preview && conversation.view?.state.goal ? (
                  <ComposerGoalStrip
                    goal={conversation.view.state.goal}
                    busy={interactionBusy}
                    onCommand={(cmdText) => void handleSend("queue", { text: cmdText })}
                    onEdit={() => {
                      const obj = conversation.view?.state.goal?.objective || "";
                      setPromptDocument(documentFromPromptAndRefs(`/goal ${obj}`, []));
                    }}
                  />
                ) : null}
                {nativeMirror ? <p className="px-4 py-3 text-cx-sm text-cx-fg-3" role="note">此处只读。</p> : <PromptBar
                  value={prompt}
                  onChange={handlePromptChange}
                  promptDocument={promptDocument}
                  onPromptDocumentChange={handlePromptDocumentChange}
                  onJumpToContext={(payload) => void handleJumpToContext(payload)}
                  onSubmit={() => void handleSend()}
                  onSubmitOpposite={() => void handleSend(runningSend === "queue" ? "steer" : "queue")}
                  submitLabel={running ? runningSend === "steer" ? "引导当前运行" : "加入后续队列" : undefined}
                  onStop={handleStop}
                  running={running}
                  capabilityBanner={providerSwitchNotice}
                  onDismissCapabilityBanner={() => {
                    setDismissedProviderPreview(providerPreviewKey);
                    setHandoffNotice("");
                  }}
                  busy={interactionBusy}
                  submitDisabled={threadArchived || nativeMirror}
                  placeholder={
                    nativeMirror ? "原生子代理记录 · 只读" : threadArchived
                      ? "已归档 — 取消归档后可继续发送；草稿已保留"
                      : running
                        ? `输入消息，${sendKeyHint(sendKey, isMacPlatform())} ${runningSend === "queue" ? "排队" : "引导"}，Shift+Enter ${runningSend === "queue" ? "引导" : "排队"}，Alt+Enter 换行…`
                        : previewHost?.composerPlaceholder || "输入消息，按 Enter 发送…"
                  }
                  localMentions={previewHost?.localMentions}
                  customDrop={previewHost?.composerDrop}
                  attachments={attachments}
                  undoScope={String(composerUndoEpoch)}
                  onRestoreAttachments={setAttachments}
                  onAddAttachment={() => fileInputRef.current?.click()}
                  onAddFiles={handleAddFiles}
                  onRemoveAttachment={(i) =>
                    setAttachments((curr) => curr.filter((_, idx) => idx !== i))
                  }
                  onRetryAttachment={handleRetryAttachment}
                  onLargePaste={handleLargePaste}
                  capabilityContext={composerCapabilityContext}
                  capabilityRefs={capabilityRefs}
                        onComposerCommand={(action: string, sourceDocument?: PromptDocument) => {
                    if (action === "new") startNewChat(projectId, true);
                    else if (action.startsWith("ui:")) void handleClientCommand(action, "", sourceDocument);
                  }}
                  {...stashRecallProps}
                  compact={composerCompact}
                  onRequestExpand={() => handleComposerCompactChange(false)}
                  extraControls={preview ? (
                    (typeof previewHost?.composerControls === "function"
                      ? previewHost.composerControls({ open: modelPickerOpen, onOpenChange: handleModelPickerOpenChange, disabled: busy })
                      : previewHost?.composerControls) ?? <span className="px-2 text-cx-xs text-ink-3">演示数据 · 只读预览</span>
                  ) : previewHost?.composerControls ? (
                    <div className="flex min-w-0 flex-1 items-center gap-1">
                      {typeof previewHost.composerControls === "function"
                        ? previewHost.composerControls({ open: modelPickerOpen, onOpenChange: handleModelPickerOpenChange, disabled: busy })
                        : previewHost.composerControls}
                    </div>
                  ) : (
                    <div className="flex min-w-0 flex-1 items-center gap-1">
                      <ConversationModelPicker
                        credentials={credentials}
                        selectedCredentialId={credentialId}
                        selectedModel={model}
                        selectedEffort={effort}
                        separateEffortControl
                        selectedServiceTier={serviceTier}
                        loading={credentialSource.phase === "loading"}
                        error={credentialsErrorText}
                        onRetry={() => void reloadReadiness({ fresh: true })}
                        onSelect={handleSelectModelParams}
                        runtimes={effectiveRuntimes}
                        runtimeKey={runtimeKey}
                        onRuntimeChange={handleRuntimeChange}
                        open={modelPickerOpen}
                        instant={modelPickerInstant}
                        onOpenChange={handleModelPickerOpenChange}
                      />
                    </div>
                  )}
                  contextMeter={preview ? null : (
                    <div className="flex min-w-0 items-center gap-0.5">
                      <ConversationComposerModes
                        selectedCredentialId={credentialId}
                        selectedModel={model}
                        selectedModelMetadata={selectedCredential ? allCredentialModels(selectedCredential).find((row) => row.id === model) : undefined}
                        selectedCredential={selectedCredential}
                        selectedEffort={effort}
                        selectedServiceTier={serviceTier}
                        ref={composerModesRef}
                        selectedAccessMode={accessMode}
                        accessModes={accessModes}
                        accessModeReason={accessModeReason}
                        interactionMode={interactionMode}
                        planModeAvailable={planModeSupport.available}
                        planModeReason={planModeSupport.reason}
                        onInteractionModeChange={handleInteractionModeChange}
                        onSelect={handleSelectModelParams}
                      />
                      {(
                        <IconButton
                          size="md"
                          icon={speech.state === "listening" ? "pause" : "mic"}
                          label={speech.state === "listening" && desktopChatBridge() ? "完成录音" : ["listening", "requesting", "processing"].includes(speech.state) ? "取消录音" : "语音输入"}
                          tooltip={
                            speech.state === "unsupported" ? "当前桌面环境不支持语音转文字，可继续键盘输入或粘贴文字" : speech.state === "error" && speech.errorMessage
                              ? speech.errorMessage
                              : speech.state === "listening"
                                ? desktopChatBridge() ? "结束录音并将识别结果追加到草稿" : "取消录音（不清空草稿）"
                                : speech.state === "processing" ? "正在整理识别结果，点击可取消"
                                : speech.state === "requesting" ? "正在申请语音权限，点击可取消"
                                : "语音转文字：结果追加到草稿末尾，不会自动发送"
                          }
                          onClick={() => speech.state === "listening" && desktopChatBridge() ? void speech.finish() : ["listening", "requesting", "processing"].includes(speech.state) ? speech.cancel() : void speech.start()}
                          className={cn(
                            "rounded-xl",
                            speech.state === "listening" && "bg-cx-danger-soft text-cx-danger hover:text-cx-danger animate-pulse",
                            speech.state === "error" && "text-cx-warning hover:text-cx-warning",
                          )}
                          data-testid="composer-mic-button"
                        />
                      )}
                      {speech.state === "listening" && desktopChatBridge() ? <button type="button" className="cx-press inline-flex h-8 items-center rounded-xl px-2.5 text-cx-sm font-medium text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg" onClick={speech.cancel}>取消</button> : null}
                    </div>
                  )}
                />}
                {desktopChatBridge() && (speech.state === "listening" || speech.state === "processing") ? <p role="status" className="mt-2 whitespace-pre-wrap break-words text-cx-xs text-cx-fg-3">{speech.state === "processing" ? "正在整理识别结果…" : "正在使用 macOS 设备端语音识别；说完后点「完成录音」。"}{speech.partialTranscript ? `\n${speech.partialTranscript}` : ""}</p> : null}
                {(speech.state === "error" || speech.state === "unsupported") && speech.errorMessage ? (
                  <p role="alert" className="mt-2 whitespace-pre-wrap break-words text-cx-xs text-cx-warning" data-testid="composer-speech-error">
                    {speech.errorMessage}
                    {speech.permissionDenied && desktopChatBridge()?.openPermissionSettings ? <button type="button" className="ml-2 underline" onClick={() => void speech.openPermissionSettings()}>{speech.permissionKind === "speechRecognition" ? "打开系统语音识别设置" : "打开系统麦克风设置"}</button> : null}
                  </p>
                ) : null}
                {!preview && !nativeMirror ? <ComposerContextStrip
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
                  onUnboundRecover={hasWorkspace ? undefined : () => startNewChat()}
                  trailing={(
                    <>
                      <ConversationStatsBar
                        variant="inline"
                        threadId={threadId}
                        usageRevision={completedModelTurnSeq}
                        running={running}
                        statistics={conversation.view.statistics}
                        contextWindow={conversation.view.context_window}
                      />
                      <ContextWindowMeter
                        variant="ring"
                        threadId={threadId}
                        contextWindow={conversation.view.context_window}
                        usageSupported={capabilitiesFor(descriptors, viewRuntimeKey)
                          ? capabilitiesFor(descriptors, viewRuntimeKey)?.context_usage === true : undefined}
                        capabilityContext={composerCapabilityContext}
                        onRefresh={() => void conversation.refresh()}
                      />
                    </>
                  )}
                /> : null}
              </div>
            }
          />
          </OpenToolFile>
          {bottomPanelPresence.present ? (
            <ConversationStreamBottomPanel
              stream={conversation.stream}
              streamThreadId={streamThreadId}
              onClose={() => {
                setBottomPanelInstant(false);
                setBottomPanelOpen(false);
              }}
              onOpenDetails={openDetails}
              transitionPhase={bottomPanelPresence.phase}
              instant={bottomPanelInstant}
            />
          ) : null}
          </>
        ) : null}
      </main>
        );
        if (isolatedChrome) return primaryMain;
        return (
          <ConversationPaneWorkspace
            primaryPane={primaryMain}
            threads={sidebarThreads}
            renderSecondaryPane={(leaf, ctx) => (
              <ConversationShell
                key={leaf.id}
                threadId={leaf.threadId}
                navigation={navigation}
                experience={experience}
                mainContentId={mainContentId}
                sidebarId={sidebarId}
                paneChild={ctx}
              />
            )}
          />
        );
      })()}

      {!sideOf && agentPage ? <aside
        aria-label="子会话页面"
        data-testid="agent-side-page"
        data-agent-thread-id={agentPage}
        className={cn("z-20 flex min-h-0 flex-col overflow-hidden border-l border-cx-border bg-cx-bg", agentPageSheet ? "absolute inset-0" : "relative shrink-0")}
        style={agentPageSheet ? undefined : {width: Math.min(agentPageWidth, agentPageMax)}}
      >
        {!agentPageSheet ? <ResizeHandle side="left" value={Math.min(agentPageWidth, agentPageMax)} min={360} max={agentPageMax} onChange={setAgentPageWidth} onReset={() => setAgentPageWidth(520)} label="调整子会话宽度" /> : null}
        <ConversationShell key={agentPage} threadId={agentPage} navigation={navigation}
          mainContentId={`${mainContentId}-agent`} sidebarId={`${sidebarId}-agent`}
          sideOf={{threadId, title: conversation.view?.thread.title || "主对话", overlay: agentPageSheet, onClose: closeAgentPage}} />
      </aside> : null}

      {(!sideOf || runSideChat) && conversation.view && threadId ? (
        <ConversationStreamRightPanel
          stream={conversation.stream}
          threadId={threadId}
          view={conversation.view}
          hasWorkspace={hasWorkspace}
          sheet={panelSheet}
          sidebarWidth={mainLeft}
          instant={rightPanelInstant}
          onOpenDetails={openDetails}
          onCiteToComposer={(excerpt) => {
            appendComposerText(excerpt);
            toast({ title: "已引用到输入框", tone: "success", duration: 1800 });
          }}
          onAttachToComposer={({ text, files, send }) => {
            appendComposerText(text);
            if (files.length) handleAddFiles(files);
            if (send) setAttachSendNonce((value) => value + 1);
            else toast({ title: "已加入输入框", tone: "success", duration: 1800 });
          }}
          onDiffAnnotationSend={handleDiffAnnotationSend}
        />
      ) : null}

      <ConversationDetailsDrawer
        payload={detailsPayload}
        onClose={() => setDetailsPayload(null)}
        threadId={threadId}
      />

      <ConversationInfoDrawer
        key={commandScope}
        open={infoDrawerOpen}
        onClose={() => setInfoDrawerOpen(false)}
        view={conversation.view}
        memory={memory}
        memoryLoading={memoryLoading}
        onAddMemory={preview ? undefined : handleAddMemory}
        onDeleteMemory={preview ? undefined : handleDeleteMemory}
      />

      {exportDialogOpen && conversation.view ? (
        <ExportDialog
          open={exportDialogOpen}
          onClose={() => setExportDialogOpen(false)}
          view={conversation.view}
        />
      ) : null}

      <Dialog
        open={Boolean(relayConfirm)}
        onOpenChange={(open) => { if (!open) setRelayConfirm(null); }}
        title="切换 Provider 并发送"
        description={providerPreview || "切换 Provider 后将新建会话，Muteki 会注入整理后的当前分支历史。"}
        size="sm"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setRelayConfirm(null)}>取消</Button>
            <Button variant="primary" data-autofocus onClick={() => {
              const pending = relayConfirm;
              setRelayConfirm(null);
              if (!pending) return;
              relayConfirmedRef.current = true;
              void handleSend(pending.sendMode);
            }}>切换并发送</Button>
          </>
        )}
      />

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

      <Dialog
        open={Boolean(pendingSendDialog && pendingSendDialog.draftKey === draftKey && pendingSendDialog.scope === conversationStorageScope())}
        onOpenChange={(open) => { if (!open) setPendingSendDialog(null); }}
        title={pendingSendDialog?.kind === "retry" ? "处理未确认的重试请求" : "处理未确认的发送请求"}
        description={pendingSendDialog?.kind === "retry" ? "旧重试的结果尚未确认。" : "原发送请求的结果尚未确认。"}
        size="lg"
        footer={<>
          <Button variant="ghost" onClick={() => setPendingSendDialog(null)}>取消</Button>
          <Button variant="secondary" loading={pendingSendQuery} onClick={() => void queryPendingSend()}>核对原回执</Button>
          <Button variant="danger" disabled={pendingSendQuery || (pendingSendDialog?.kind === "retry"
            ? !["sending", "unknown"].includes(readRetryIntent(pendingSendDialog.threadId, pendingSendDialog.turnId)?.phase || "")
            : !readSendIntent(draftKey))} onClick={archivePendingSend}>
            {pendingSendDialog?.kind === "retry" ? "保留并准备新重试" : "归档并允许新请求"}
          </Button>
        </>}
      >
        <p className="text-cx-sm text-cx-fg-3">
          {pendingSendDialog?.kind === "retry"
            ? "请核对原回执，或明确保留原记录后准备新重试；旧请求与新重试可能重复执行。准备后仍需返回影响确认，才会执行。"
            : "请核对原命令回执，或保留完整待核对记录后另起新的逻辑发送。归档不会取消原请求，迟到的原请求与新请求可能同时被受理。"}
        </p>
        {pendingSendDialog?.kind === "retry" ? <p className="mt-2 text-cx-sm">旧版本若未记录完整参数，会按“部分请求信息”保存；原命令 ID 保留，可继续核对原回执。</p>
          : <Button className="mt-3" size="sm" variant="secondary" onClick={() => { setPendingSendDialog(null); openLegacyRecovery(); }}>审查升级前的旧草稿与暂存</Button>}
        <pre className="mt-3 max-h-56 select-text overflow-auto whitespace-pre-wrap break-words text-cx-xs">{JSON.stringify(pendingSendDialog?.kind === "retry" ? readRetryIntent(pendingSendDialog.threadId, pendingSendDialog.turnId) : readSendIntent(draftKey), null, 2)}</pre>
        {pendingSendStatus ? <p role="status" className="mt-2 text-cx-sm">{pendingSendStatus}</p> : null}
        {pendingSendDialog?.kind === "retry" ? (archivedRetries.length ? <details className="mt-3"><summary>当前工作台的待核对重试（{archivedRetries.length}）</summary><pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words text-cx-xs">{JSON.stringify(archivedRetries, null, 2)}</pre></details> : null)
          : archivedSends.length ? <details className="mt-3"><summary>当前工作台的待核对记录（{archivedSends.length}）</summary><pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words text-cx-xs">{JSON.stringify(archivedSends, null, 2)}</pre></details> : null}
      </Dialog>

      <Dialog open={Boolean(legacyRecovery && legacyRecovery.scope === conversationStorageScope() && legacyRecovery.draftKey === draftKey)} onOpenChange={(open) => { if (!open) setLegacyRecovery(null); }} title="审查没有服务归属的旧记录" description="旧版记录无法证明属于当前工作台。" size="lg" footer={<Button variant="secondary" onClick={() => setLegacyRecovery(null)}>关闭</Button>}>
        <p className="text-cx-sm text-cx-fg-3">原记录会保留，复制只追加到当前草稿；模型、权限、上传哈希和旧命令身份均不继承。</p>
        <p className="mt-2 break-all text-cx-xs">当前工作台身份：{legacyRecovery?.scope}</p>
        <Checkbox className="my-3" checked={legacyConfirmed} onCheckedChange={setLegacyConfirmed} label="我已审查内容，确认要在此工作台恢复；引用和附件将重新选择。" />
        {legacyRecovery?.entries.length ? legacyRecovery.entries.map((entry) => <section key={`${entry.kind}:${entry.key}`} className="mb-3 rounded-xl border border-cx-border p-3"><h3 className="break-all text-cx-sm">{entry.kind === "stash" ? "旧暂存" : "旧草稿"} · {entry.label}</h3><details className="mt-2"><summary>完整原记录</summary><pre className="max-h-60 select-text overflow-auto whitespace-pre-wrap break-words text-cx-xs">{JSON.stringify(entry.snapshot, null, 2)}</pre></details><Button size="sm" variant="secondary" className="mt-2.5" disabled={!legacyConfirmed} onClick={() => copyLegacyRecovery(entry)}>追加到当前草稿</Button></section>) : <p className="text-cx-sm">没有可审查的旧草稿或暂存记录。</p>}
        {legacyRecoveryError ? <p role="alert" className="text-cx-xs text-cx-danger">{legacyRecoveryError}</p> : null}
      </Dialog>

      <ConversationSearchDialog open={commandSessionsOpen} instant={commandSessionsInstant} query={searchQuery} onQueryChange={setSearchQuery} onClose={() => setCommandSessionsOpen(false)} threads={sidebarThreads}
        restrictSearchToThreads
        includeSrcInSearch={defaultMode === "src"}
        threadLabel={experience?.sidebarLabels?.thread}
        searchLabel={experience?.sidebarLabels?.search}
        onSelectThread={(id) => router.push(threadHref(id))}
        onSelectMessage={(id, messageId) => { pushJumpBackFromThread(threadId); router.push(threadMessageHref(id, messageId)); }} />

      <Dialog open={Boolean(pendingStashRestore && pendingStashRestore.draftKey === draftKey && pendingStashRestore.scope === conversationStorageScope())} onOpenChange={(open) => { if (!open) setPendingStashRestore(null); }} title="恢复暂存前保留当前草稿" description="恢复会替换当前尚未发送的正文、引用和附件。" footer={<><Button variant="ghost" onClick={() => setPendingStashRestore(null)}>取消</Button><Button variant="danger" onClick={() => confirmStashRestore(false)}>放弃草稿并恢复</Button><Button variant="primary" onClick={() => confirmStashRestore(true)}>暂存草稿并恢复</Button></>}><pre className="max-h-48 overflow-auto whitespace-pre-wrap break-words text-cx-xs">{plainTextFromDocument(promptDocument)}</pre><p className="mt-2 text-cx-xs text-cx-fg-3">当前引用 {capabilityRefs.length} 项 · 附件 {attachments.length} 项</p></Dialog>

      <ImpactConfirmModal
        threadId={threadId}
        open={impactOpen && pendingSendDialog?.kind !== "retry"}
        preview={impactPreview}
        loading={impactLoading || busy}
        editedText={impactEditedText}
        fileMode={impactFileMode}
        onFileModeChange={setImpactFileMode}
        rewindMode={impactRewindMode}
        onRewindModeChange={setImpactRewindMode}
        onEditedTextChange={(text) => { setImpactEditedText(text); const owner = impactOwnerRef.current; if (owner?.scope === conversationStorageScope()) writeEditBuffer(owner.threadId, owner.turnId, text); }}
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

      <Dialog
        open={repoLessCreateOpen}
        onOpenChange={(open) => { if (!open && !projectCreating) setRepoLessCreateOpen(false); }}
        title="新建研究项目"
        description="创建 SRC 专属研究项目（无需本地代码目录），用于归纳相关研究任务与跨对话项目记忆。"
        icon="layers"
        size="sm"
        dismissable={!projectCreating}
        footer={(
          <>
            <Button variant="ghost" disabled={projectCreating} onClick={() => setRepoLessCreateOpen(false)}>取消</Button>
            <Button
              variant="primary"
              loading={projectCreating}
              disabled={!repoLessCreateName.trim()}
              onClick={async () => {
                const trimmed = repoLessCreateName.trim();
                if (!trimmed) {
                  setRepoLessCreateError("请输入研究项目名称");
                  return;
                }
                const createdId = await handleCreateRepoLessProject({
                  name: trimmed,
                  description: repoLessCreateDesc.trim() || undefined,
                });
                if (createdId) {
                  setRepoLessCreateName("");
                  setRepoLessCreateDesc("");
                  setRepoLessCreateError("");
                  setRepoLessCreateOpen(false);
                }
              }}
            >
              创建研究项目
            </Button>
          </>
        )}
      >
        <div className="flex flex-col gap-3">
          <TextField
            label="项目名称"
            data-autofocus
            invalid={Boolean(repoLessCreateError)}
            value={repoLessCreateName}
            onChange={(event) => {
              setRepoLessCreateName(event.target.value);
              if (repoLessCreateError) setRepoLessCreateError("");
            }}
            placeholder="例如：XX 官网与开放平台 2026 SRC"
          />
          <TextArea
            label="项目说明（可选）"
            rows={3}
            value={repoLessCreateDesc}
            onChange={(event) => setRepoLessCreateDesc(event.target.value)}
            placeholder="项目备注、授权范围等"
          />
          {repoLessCreateError ? <p role="alert" className="text-cx-xs text-cx-danger">{repoLessCreateError}</p> : null}
        </div>
      </Dialog>

      <Dialog
        open={Boolean(projectDeletion)}
        onOpenChange={(open) => { if (!open) setProjectDeletion(null); }}
        title={projectDeletion && projectDeletion.length > 1 ? `删除 ${projectDeletion.length} 个空${projectNoun}` : `删除${projectNoun}`}
        description={projectScope === "src"
          ? `${projectDeletion?.length === 1 ? `将删除「${projectDeletion[0].name}」` : `将删除下列${projectNoun}`}${projectDeletionArchived ? `，并永久删除其中 ${projectDeletionArchived} 条已归档研究` : ""}。`
          : `${projectDeletion?.length === 1 ? `将删除「${projectDeletion[0].name}」` : "将删除下列项目文件夹"}${projectDeletionArchived ? `，并永久删除其中 ${projectDeletionArchived} 个已归档对话` : ""}。`}
        icon="trash"
        tone="danger"
        size="md"
        dismissable={false}
        footer={(
          <>
            <Button variant="ghost" onClick={() => setProjectDeletion(null)}>取消</Button>
            <Button variant="danger" onClick={confirmProjectDeletion}>删除</Button>
          </>
        )}
      >
        <p className="mb-3 text-cx-sm text-cx-fg-3">{projectScope === "src" ? "此操作无法撤销。" : "本地目录中的文件不会被删除，此操作无法撤销。"}</p>
        <ul className="flex max-h-[45vh] flex-col gap-1.5 overflow-y-auto">
          {(projectDeletion || []).map((project) => (
            <li key={project.project_id} className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2">
              <p className="truncate text-cx-sm font-medium text-cx-fg">{project.name}</p>
              {project.root_path ? <p className="truncate text-cx-xs text-cx-fg-3">{project.root_path}</p> : null}
            </li>
          ))}
        </ul>
      </Dialog>

      <Dialog
        open={Boolean(archiveImpact)}
        onOpenChange={(open) => { if (!open) setArchiveImpact(null); }}
        title={archiveImpact?.mode === "batch" ? "归档项目对话" : "归档对话"}
        description="以下对话仍有运行中、待审批、待输入或排队的消息。"
        icon="archive"
        tone="warning"
        size="md"
        footer={(
          <>
            <Button variant="ghost" onClick={() => setArchiveImpact(null)}>取消</Button>
            <Button variant="primary" onClick={confirmArchiveImpact}>归档</Button>
          </>
        )}
      >
        <p className="mb-3 text-cx-sm text-cx-fg-3">归档会停止会话并暂停队列，不会删除消息或 worktree。</p>
        <ul className="flex flex-col gap-2">
          {(archiveImpact?.threads || []).map((thread) => {
            const lines = archiveImpactLines(thread);
            return (
              <li key={thread.thread_id} className="rounded-xl border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2.5">
                <p className="truncate text-cx-sm font-medium text-cx-fg">{thread.title || thread.thread_id}</p>
                {lines.length ? (
                  <ul className="mt-1 flex flex-col gap-0.5">
                    {lines.map((line) => (
                      <li key={line} className="flex items-center gap-2 text-cx-sm text-cx-fg-3">
                        <span className="size-1 shrink-0 rounded-full bg-cx-warning" />
                        {line}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="mt-0.5 text-cx-sm text-cx-fg-3">空闲，可安全归档</p>
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
        description={`共 ${largePasteText?.length ?? 0} 个字符。请选择保留全部文字或作为附件。`}
        icon="paperclip"
        tone="accent"
        size="md"
        footer={(
          <>
            <Button variant="ghost" onClick={() => { setLargePasteText(null); setLargePasteSelection(null); }}>取消</Button>
            <Button variant="secondary" onClick={handleLargePasteKeepText}>保留全部文字</Button>
            <Button variant="primary" onClick={handleLargePasteAsAttachment}>转为附件</Button>
          </>
        )}
      >
        <div className="grid gap-2">
          <div className="rounded-xl border border-cx-border-subtle px-3 py-2.5">
            <p className="text-cx-sm font-medium text-cx-fg">保留为文本</p>
            <p className="text-cx-sm text-cx-fg-3">全部内容插入输入框，保留完整换行。</p>
          </div>
          <details className="rounded-xl border border-cx-border-subtle p-3"><summary>查看完整粘贴内容</summary><pre className="mt-2 max-h-64 select-text overflow-auto whitespace-pre-wrap break-words text-cx-xs">{largePasteText}</pre><Button size="sm" variant="secondary" className="mt-2.5" onClick={() => { if (largePasteText !== null) void copyToClipboard(largePasteText).then((copied) => copied ? setNotice("完整粘贴内容已复制", "success") : setError("复制失败，请展开完整正文后手动选择复制")); }}>复制全部内容</Button></details>
          <div className="rounded-xl border border-cx-accent-line bg-cx-accent-soft px-3 py-2.5">
            <p className="text-cx-sm font-medium text-cx-fg">转为附件（推荐）</p>
            <p className="text-cx-sm text-cx-fg-3">完整内容保存为 .txt 附件，原文不截断。</p>
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

      <ComposerLaterDialog
        open={laterDialogOpen}
        onOpenChange={setLaterDialogOpen}
        onConfirm={(target) => {
          setLaterDialogOpen(false);
          const raw = plainTextFromDocument(promptDocument).trim();
          const stripped = raw.replace(/^\/later\s*/, "").trim();
          if (!stripped) {
            const d = new Date(target.at * 1000);
            const hh = String(d.getHours()).padStart(2, "0");
            const mm = String(d.getMinutes()).padStart(2, "0");
            setPromptDocument(documentFromPromptAndRefs(`/later ${hh}:${mm} `, refsFromDocument(promptDocument)));
            return;
          }
          void handleSend("queue", { text: stripped, scheduleAt: target.at });
        }}
      />


    </div>

    <ConversationShortcutsHelp
      open={shortcutsHelpOpen}
      instant={shortcutsHelpInstant}
      onClose={() => {
        setShortcutsHelpOpen(false);
      }}
    />
    {!isolatedChrome ? <Toaster /> : null}
    </>
    </ThreadEditorProvider>
    </ConversationNavigationContext.Provider>
    </PanelExtensionContext.Provider>
    </ConversationLocalPreviewContext.Provider>
    </AgentPageContext.Provider>
  );
}
