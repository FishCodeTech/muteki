"use client";

import React, { useCallback, useRef, useEffect, useMemo, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Button, Callout, IconButton, Spinner, Tooltip, useReducedMotion } from "@/components/chat/ui";
import { formatMessageTime } from "@/components/chat/timeline/MessageActions";
import { ChangedFilesCard } from "@/components/chat/timeline/ChangedFilesCard";
import { formatTurnDuration } from "@/components/chat/timeline/toolPresentation";
import { chatPanel } from "@/lib/chatPanelStore";
import { turnIdFromDiffArtifactName } from "@/lib/conversationDiff";
import { useVirtualizer } from "@tanstack/react-virtual";
import { Chat } from "../ai-native/chat";
import { ConversationMessage, type MessageAttachmentChip } from "./ConversationMessage";
import type { ResourceLinkTarget } from "@/lib/resourcePreview";
import { ConversationTurnProcess } from "./ConversationTurnProcess";
import { ConversationApproval, type UserInputResolvePayload } from "./ConversationApproval";
import { ConversationProposedPlanCard } from "./ConversationPlanPanel";
import { ContextCards } from "../ai-native/context-cards";
import { LoadingState } from "../ai-native/loading-state";
import { Icon } from "../Icon";
import type {
  ConversationCredential,
  ConversationView,
  ConversationEvent,
  ConversationMessage as ConversationMessageRecord,
  ConversationStreamStatus,
} from "@/lib/useConversation";
import { allCredentialModels, fetchTurnProcess } from "@/lib/useConversation";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";
import {
  buildConversationTurnFold,
  buildConversationTurnRuntimeMap,
  buildConversationTurnTimingMap,
  conversationErrorMessage,
  EMPTY_TURN_RUNTIME,
  presentConversationTurn,
  resolveConversationLiveActivity,
  resolveConversationTurnDurationMs,
  type ConversationTurnRuntime,
  type ConversationTurnSegment,
  settleTurnSegmentsAgainstStatus,
} from "./conversationEventViews";
import {
  applyReadingOffset,
  captureVisibleAnchor,
  isTypingTarget,
  neighboringTurnMessageId,
  peekJumpBack,
  popJumpBack,
  readExpandState,
  readReadingPosition,
  restoreMeasuredReadingAnchor,
  subscribeJumpBack,
  userTurnMessageIds,
  writeExpandState,
  writeReadingPosition,
  type TurnExpandState,
} from "@/lib/conversationReadingPosition";
import {
  buildConversationTimelineRows,
  timelineRowIndexForMessageId,
} from "@/lib/conversationTimelineRows";
import { runtimeIdentityKey } from "@/lib/conversationReadingPrefs";

export interface ConversationTimelineProps {
  view: ConversationView;
  credentials?: ConversationCredential[];
  events?: ConversationEvent[];
  liveText?: string;
  running?: boolean;
  connected?: boolean;
  streamStatus?: ConversationStreamStatus;
  busy?: boolean;
  contentLoading?: boolean;
  contentError?: string;
  onReloadContent?: () => void;
  onLoadOlder?: () => Promise<boolean> | boolean | void;
  onJumpToLatest?: () => Promise<boolean> | boolean | void;
  loadingOlder?: boolean;
  onOpenDrawer: (payload: DrawerDetailPayload) => void;
  onOpenDiffBaseline?: (request: {
    kind: "worktree" | "turn" | "current_turn";
    turnId?: string;
    artifactSha256?: string;
  }) => void;
  onOpenAttachment?: (attachment: MessageAttachmentChip) => void;
  onResourceLink?: (target: ResourceLinkTarget) => void;
  onApprovalDecision: (approvalId: string, decision: "allow" | "deny", scopeMode?: "once" | "session") => void;
  onUserInputResolve: (payload: UserInputResolvePayload) => void;
  threadId?: string;
  onRetryTurn?: (turnId: string) => void;
  onEditTurn?: (turnId: string, text: string) => void;
  onRewindTurn?: (turnId: string) => void;
  onContinueTurn?: () => void;
  onForkTurn?: (turnId?: string) => void;
  onCiteMessageSpan?: (payload: {
    messageId: string;
    turnId?: string;
    text: string;
    startOffset: number;
    endOffset: number;
    streaming?: boolean;
  }) => void;
  jumpTarget?: {
    messageId: string;
    startOffset?: number;
    endOffset?: number;
  } | null;
  onJumpTargetConsumed?: () => void;
  onJumpToContext?: (payload: {
    messageId: string;
    startOffset?: number;
    endOffset?: number;
  }) => void;
  rewindDisabled?: boolean;
  rewindReason?: string;
  showSuperseded?: boolean;
  onToggleSuperseded?: () => void;
  supersededTurns?: ConversationView["turns"];
  supersededMessages?: ConversationView["messages"];
  onOpenPlan?: () => void;
  commandError?: string;
  onDismissCommandError?: () => void;
  footer?: React.ReactNode;
  className?: string;
  highlightedMessageId?: string;
  /** When true (C07 `?message=` present), skip C08 restore for this visit. */
  suppressRestore?: boolean;
  onEnsureMessageVisible?: (messageId: string) => Promise<"ok" | "missing" | "cancelled">;
  /** Cross-thread jump-back: open thread without `?message=` so C08 restore applies. */
  onOpenThread?: (threadId: string) => void;
}

interface AssistantRuntimeIdentity {
  endpoint: string;
  model: string;
}

interface ConversationFailurePanelProps {
  title: string;
  detail: string;
  interrupted?: boolean;
  credentialFailure?: boolean;
  busy?: boolean;
  onContinue?: () => void;
  onDismiss: () => void;
}

function ConversationFailurePanel({
  title,
  detail,
  interrupted = false,
  credentialFailure = false,
  busy = false,
  onContinue,
  onDismiss,
}: ConversationFailurePanelProps) {
  return (
    <section
      className="cx-animate-in mb-3 overflow-hidden rounded-2xl border border-cx-border bg-cx-elevated shadow-cx-sm"
      role={interrupted ? "status" : "alert"}
      aria-label={title}
    >
      <div className="flex items-start gap-3 px-4 pb-3 pt-3.5">
        <span
          className={cn(
            "mt-0.5 grid size-8 shrink-0 place-items-center rounded-xl",
            interrupted ? "bg-cx-warning-soft text-cx-warning" : "bg-cx-danger-soft text-cx-danger",
          )}
          aria-hidden="true"
        >
          <Icon name={interrupted ? "stopCircle" : "circleAlert"} size={16} />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex items-start justify-between gap-3">
            <h2 className="pt-1 text-[13.5px] font-semibold leading-5 text-cx-fg">{title}</h2>
            <IconButton icon="x" label="关闭错误提示" noTooltip size="sm" className="-mr-1.5 -mt-0.5" onClick={onDismiss} />
          </div>
          <p className="cx-scroll mt-1 max-h-28 overflow-y-auto whitespace-pre-wrap break-words pr-2 font-cx-mono text-[12px] leading-5 text-cx-fg-3">
            {detail}
          </p>
        </div>
      </div>
      {(credentialFailure || onContinue) ? (
        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-cx-border-subtle bg-cx-bg-subtle px-4 py-2.5">
          {credentialFailure ? (
            <a
              href="/settings/agents"
              className="cx-press inline-flex h-7 items-center gap-1.5 rounded-lg border border-cx-border bg-cx-elevated px-2.5 text-[12.5px] font-medium text-cx-fg-2 hover:border-cx-border-strong hover:text-cx-fg"
            >
              <Icon name="plug" size={13} />
              检查 Agent 接入
            </a>
          ) : null}
          {onContinue ? (
            <Button variant="primary" size="sm" icon="play" onClick={onContinue} disabled={busy}>
              继续执行
            </Button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}

function resolveAssistantRuntimeIdentity(
  runtime: ConversationTurnRuntime,
  credentials: ConversationCredential[],
): AssistantRuntimeIdentity {
  const credential = credentials.find((item) => item.id === runtime.credentialId);
  const endpoint = credential?.label
    || runtime.credentialId
    || [runtime.adapterId, runtime.instanceId].filter(Boolean).join(":");
  const catalogModel = credential
    ? allCredentialModels(credential).find((item) => item.id === runtime.model)
    : undefined;
  const model = catalogModel?.label || runtime.model;
  // Never invent the live selector here — missing attribution stays unknown (#132).
  if (!endpoint && !model) {
    return { endpoint: "unknown", model: "" };
  }
  return {
    endpoint: endpoint || "unknown",
    model: model || "unknown",
  };
}

function AssistantIdentity({
  createdAt,
  runtime,
  collapsed = false,
}: {
  createdAt?: string;
  runtime?: AssistantRuntimeIdentity | null;
  /** C39: same-source follow-up — keep time, tuck endpoint/model into title. */
  collapsed?: boolean;
}) {
  const runtimeText = [runtime?.endpoint, runtime?.model].filter(Boolean).join(" · ");
  const time = createdAt ? formatMessageTime(createdAt) : "";
  const fullLabel = runtimeText
    ? `Agent · ${runtimeText}${time ? ` · ${time}` : ""}`
    : `Agent${time ? ` · ${time}` : ""}`;

  if (collapsed) {
    return (
      <div
        className="is-collapsed sr-only"
        data-testid="c39-identity-collapsed"
        title={fullLabel}
        aria-label={fullLabel}
      >
        {fullLabel}
      </div>
    );
  }

  return (
    <div
      className="mb-1.5 flex min-w-0 items-center gap-2 text-[12px] leading-5"
      data-testid="c39-identity-full"
    >
      <span className="grid size-5 shrink-0 place-items-center rounded-md bg-cx-accent-soft text-cx-accent">
        <Icon name="sparkles" size={12} />
      </span>
      {runtimeText ? (
        <Tooltip content={`运行接入点与模型：${runtimeText}`}>
          <span className="inline-flex min-w-0 items-center gap-1.5 text-cx-fg-3" aria-label={`运行接入点与模型：${runtimeText}`}>
            <span className="max-w-[180px] truncate font-medium text-cx-fg-2">{runtime?.endpoint}</span>
            {runtime?.model ? (
              <>
                <span aria-hidden="true" className="text-cx-fg-4">/</span>
                <span className="max-w-[220px] truncate font-cx-mono text-[11.5px]">{runtime.model}</span>
              </>
            ) : null}
          </span>
        </Tooltip>
      ) : (
        <span className="font-medium text-cx-fg-2">Agent</span>
      )}
      {time ? <time dateTime={createdAt} className="cx-tabular text-[11.5px] text-cx-fg-4">{time}</time> : null}
    </div>
  );
}

export function ConversationTimeline({
  view,
  credentials = [],
  events = [],
  liveText = "",
  running = false,
  connected = true,
  streamStatus = "connected",
  busy = false,
  contentLoading = false,
  contentError = "",
  onReloadContent,
  onLoadOlder,
  onJumpToLatest,
  loadingOlder = false,
  onOpenDrawer,
  onOpenDiffBaseline,
  onOpenAttachment,
  onResourceLink,
  onApprovalDecision,
  onUserInputResolve,
  threadId = "",
  onRetryTurn,
  onEditTurn,
  onRewindTurn,
  onContinueTurn,
  onForkTurn,
  onCiteMessageSpan,
  jumpTarget = null,
  onJumpTargetConsumed,
  onJumpToContext,
  rewindDisabled = true,
  rewindReason = "",
  showSuperseded = false,
  onToggleSuperseded,
  supersededTurns = [],
  supersededMessages = [],
  onOpenPlan,
  commandError = "",
  onDismissCommandError,
  footer,
  className = "",
  highlightedMessageId = "",
  suppressRestore = false,
  onEnsureMessageVisible,
  onOpenThread,
}: ConversationTimelineProps) {
  const streamRef = useRef<HTMLDivElement>(null);
  const stickToBottomRef = useRef(true);
  const loadingOlderRef = useRef(false);
  const pendingAnchorRef = useRef<{ height: number; top: number } | null>(null);
  const highlightScrollDoneRef = useRef("");
  const restoreDoneRef = useRef("");
  const restoringAnchorRef = useRef(false);
  const anchorRequestRef = useRef(0);
  const lastAnchorRef = useRef<{ messageId: string; offsetPx: number } | null>(null);
  const expandByTurnRef = useRef<TurnExpandState>({});
  const [showJumpToLatest, setShowJumpToLatest] = useState(false);
  const [hasJumpBack, setHasJumpBack] = useState(() => Boolean(peekJumpBack()));
  const [dismissedFailureKey, setDismissedFailureKey] = useState("");
  const [hydratedEventsByTurn, setHydratedEventsByTurn] = useState<
    Record<string, ConversationEvent[]>
  >({});
  const [expandByTurn, setExpandByTurn] = useState<TurnExpandState>({});

  useEffect(() => {
    expandByTurnRef.current = expandByTurn;
  }, [expandByTurn]);

  useEffect(() => subscribeJumpBack(() => setHasJumpBack(Boolean(peekJumpBack()))), []);

  useEffect(() => {
    setHydratedEventsByTurn({});
    const threadKey = view.thread.thread_id;
    setExpandByTurn(readExpandState(threadKey));
    restoreDoneRef.current = "";
    restoringAnchorRef.current = false;
    anchorRequestRef.current += 1;
    lastAnchorRef.current = null;
    setDismissedFailureKey("");
  }, [view.thread.thread_id]);

  useEffect(() => {
    const element = streamRef.current;
    if (!element) return;
    const cancelRestore = () => {
      if (!restoringAnchorRef.current) return;
      anchorRequestRef.current += 1;
      restoringAnchorRef.current = false;
      restoreDoneRef.current = view.thread.thread_id;
    };
    const events = ["wheel", "touchstart", "pointerdown", "keydown"] as const;
    for (const type of events) element.addEventListener(type, cancelRestore, { passive: true });
    return () => {
      anchorRequestRef.current += 1;
      restoringAnchorRef.current = false;
      for (const type of events) element.removeEventListener(type, cancelRestore);
    };
  }, [view.thread.thread_id]);

  useEffect(() => {
    if (!jumpTarget?.messageId) return;
    stickToBottomRef.current = false;
    setShowJumpToLatest(true);
    const el = document.querySelector<HTMLElement>(
      `[data-message-id="${CSS.escape(jumpTarget.messageId)}"]`,
    );
    // Use instant scroll — smooth animation races ResizeObserver re-anchors
    // and drifts the reading position while the cite highlight settles.
    el?.scrollIntoView({ behavior: "auto", block: "center" });
    el?.classList.add("is-cite-highlight");
    const timer = window.setTimeout(() => {
      el?.classList.remove("is-cite-highlight");
      onJumpTargetConsumed?.();
    }, 1600);
    return () => window.clearTimeout(timer);
  }, [jumpTarget, onJumpTargetConsumed]);

  const persistCurrentAnchor = useCallback((
    element: HTMLElement,
    stickToBottom: boolean,
  ) => {
    const threadKey = view.thread.thread_id;
    if (!threadKey) return;
    // Deep-link visits (`?message=`) own the viewport — do not overwrite the
    // saved reading position with the deep-link target while suppressRestore.
    if (suppressRestore) return;
    if (restoringAnchorRef.current) return;
    const captured = captureVisibleAnchor(element);
    if (captured) lastAnchorRef.current = captured;
    const messageId = captured?.messageId || lastAnchorRef.current?.messageId || "";
    if (!messageId) return;
    writeReadingPosition({
      threadId: threadKey,
      messageId,
      offsetPx: captured?.offsetPx ?? lastAnchorRef.current?.offsetPx ?? 0,
      stickToBottom,
      expandByTurn: expandByTurnRef.current,
    });
  }, [suppressRestore, view.thread.thread_id]);

  const reapplySavedAnchor = useCallback(() => {
    // C07 `?message=` still owns the viewport after highlight clears — never
    // yank back to a prior reading anchor while suppressRestore is set.
    if (suppressRestore) return;
    if (restoringAnchorRef.current) return;
    if (stickToBottomRef.current) return;
    if (String(highlightedMessageId || "").trim()) return;
    if (loadingOlderRef.current) return;
    const element = streamRef.current;
    const saved = lastAnchorRef.current || readReadingPosition(view.thread.thread_id);
    if (!element || !saved?.messageId) return;
    applyReadingOffset(element, saved.messageId, saved.offsetPx || 0);
  }, [highlightedMessageId, suppressRestore, view.thread.thread_id]);

  const handleStreamScroll = useCallback((event: React.UIEvent<HTMLDivElement>) => {
    const element = event.currentTarget;
    if (restoringAnchorRef.current) return;
    const nearBottom = element.scrollHeight - element.scrollTop - element.clientHeight < 96;
    stickToBottomRef.current = nearBottom;
    setShowJumpToLatest(!nearBottom);
    if (!String(highlightedMessageId || "").trim()) {
      persistCurrentAnchor(element, nearBottom);
    }
    // Suppress older-page fetches while a deep-link highlight is settling.
    // Shell sets highlightedMessageId before the around-page fetch commits, so
    // the around window mounting at scrollTop≈0 cannot immediately prepend.
    if (String(highlightedMessageId || "").trim()) return;
    if (
      onLoadOlder
      && view.messages_page?.has_more_before
      && !loadingOlderRef.current
      && element.scrollTop < 120
    ) {
      loadingOlderRef.current = true;
      stickToBottomRef.current = false;
      pendingAnchorRef.current = {
        height: element.scrollHeight,
        top: element.scrollTop,
      };
      void Promise.resolve(onLoadOlder()).finally(() => {
        loadingOlderRef.current = false;
      });
    }
  }, [
    highlightedMessageId,
    onLoadOlder,
    persistCurrentAnchor,
    view.messages_page?.has_more_before,
  ]);

  useEffect(() => {
    loadingOlderRef.current = loadingOlder;
  }, [loadingOlder]);

  // Restore reading position after older pages prepend.
  useEffect(() => {
    const anchor = pendingAnchorRef.current;
    const element = streamRef.current;
    if (!anchor || !element) return;
    pendingAnchorRef.current = null;
    const frame = requestAnimationFrame(() => {
      element.scrollTop = anchor.top + (element.scrollHeight - anchor.height);
      persistCurrentAnchor(element, false);
    });
    return () => cancelAnimationFrame(frame);
  }, [persistCurrentAnchor, view.messages.length]);

  const jumpToLatest = useCallback(() => {
    stickToBottomRef.current = true;
    setShowJumpToLatest(false);
    // Instant scrollTop jumps (not smooth) — avoids racing follow/re-anchor.
    // Tip replace changes row count/heights; scroll after layout commits.
    void Promise.resolve(onJumpToLatest?.()).finally(() => {
      const scrollTip = () => {
        const element = streamRef.current;
        if (!element) return;
        element.scrollTop = element.scrollHeight;
        persistCurrentAnchor(element, true);
      };
      requestAnimationFrame(() => {
        scrollTip();
        requestAnimationFrame(scrollTip);
      });
      window.setTimeout(scrollTip, 50);
      window.setTimeout(scrollTip, 200);
    });
  }, [onJumpToLatest, persistCurrentAnchor]);


  useEffect(() => {
    const threadKey = view.thread.thread_id;
    if (suppressRestore) {
      // C07 deep-link owns this visit — pin off the live edge so follow-latest
      // and re-anchor cannot undo `?message=` after the highlight timeout.
      stickToBottomRef.current = false;
      setShowJumpToLatest(true);
      return;
    }
    const saved = readReadingPosition(threadKey);
    if (!saved || saved.stickToBottom) {
      stickToBottomRef.current = true;
      setShowJumpToLatest(false);
      return;
    }
    stickToBottomRef.current = false;
    setShowJumpToLatest(true);
    lastAnchorRef.current = {
      messageId: saved.messageId,
      offsetPx: saved.offsetPx,
    };
  }, [suppressRestore, view.thread.thread_id]);

  // Follow the live edge only while the reader remains near the bottom.
  // Never fight a deep-link highlight jump or a restored history anchor.
  useEffect(() => {
    if (!stickToBottomRef.current || loadingOlder) return;
    if (String(highlightedMessageId || "").trim()) return;
    const frame = requestAnimationFrame(() => {
      const element = streamRef.current;
      if (!element) return;
      element.scrollTop = element.scrollHeight;
    });
    return () => cancelAnimationFrame(frame);
  }, [
    view.messages.length,
    highlightedMessageId,
    liveText.length,
    running,
    events.length,
    loadingOlder,
  ]);

  // Re-anchor after tool expand / image layout while reading history.
  useEffect(() => {
    if (stickToBottomRef.current) return;
    const frame = requestAnimationFrame(() => reapplySavedAnchor());
    return () => cancelAnimationFrame(frame);
  }, [expandByTurn, reapplySavedAnchor]);

  useEffect(() => {
    const element = streamRef.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      reapplySavedAnchor();
    });
    const content = element.firstElementChild;
    if (content) observer.observe(content);
    observer.observe(element);
    return () => observer.disconnect();
  }, [reapplySavedAnchor, view.thread.thread_id]);

  const foldEvents = useMemo(() => {
    const byKey = new Map<string, ConversationEvent>();
    for (const rows of Object.values(hydratedEventsByTurn)) {
      for (const event of rows) {
        const key = String(event.event_id || `${event.seq}:${event.event_type}`);
        byKey.set(key, event);
      }
    }
    for (const event of events) {
      const key = String(event.event_id || `${event.seq}:${event.event_type}`);
      byKey.set(key, event);
    }
    return Array.from(byKey.values()).sort((a, b) => (a.seq || 0) - (b.seq || 0));
  }, [events, hydratedEventsByTurn]);

  const turnFold = useMemo(() => buildConversationTurnFold(foldEvents), [foldEvents]);
  const runtimeByTurn = useMemo(
    // Do not pass view.runtime: current selection must not rewrite history (#132).
    () => buildConversationTurnRuntimeMap(foldEvents),
    [foldEvents],
  );
  const timingByTurn = useMemo(
    () => buildConversationTurnTimingMap(foldEvents),
    [foldEvents],
  );

  const ensureTurnProcess = useCallback(async (turnId: string) => {
    if (!turnId || hydratedEventsByTurn[turnId]) return;
    try {
      const detail = await fetchTurnProcess(view.thread.thread_id, turnId);
      setHydratedEventsByTurn((current) => ({
        ...current,
        [turnId]: detail.events || [],
      }));
    } catch {
      // Leave segments empty; persisted assistant text still renders.
    }
  }, [hydratedEventsByTurn, view.thread.thread_id]);

  const turnById = useMemo(
    () => new Map(view.turns.map((turn) => [turn.turn_id, turn])),
    [view.turns],
  );

  const rawPendingApprovals = view.state.pending_approvals || {};
  const pendingApprovals = Object.fromEntries(
    Object.entries(rawPendingApprovals).filter(([, row]) => {
      if (!row || typeof row !== "object") return false;
      const status = String((row as Record<string, unknown>).status || "pending");
      return status === "pending" || status === "expired";
    }),
  ) as Record<string, Record<string, unknown>>;
  const rawPendingApproval = view.state.pending_approval;
  const pendingApproval =
    typeof rawPendingApproval?.approval_id === "string" && rawPendingApproval.approval_id
      ? rawPendingApproval
      : null;

  const rawPendingInput = view.state.pending_user_input;
  const pendingInput =
    running && typeof rawPendingInput?.request_id === "string" && rawPendingInput.request_id
      ? rawPendingInput
      : null;

  // Artifacts & Diffs list
  const artifacts = useMemo(() => view.artifacts || [], [view.artifacts]);
  const diffArtifacts = useMemo(() => {
    return artifacts.filter((a) => a.kind === "conversation.diff" || a.name?.endsWith(".diff"));
  }, [artifacts]);

  const regularArtifacts = useMemo(() => {
    return artifacts.filter((a) => a.kind !== "conversation.diff" && !a.name?.endsWith(".diff"));
  }, [artifacts]);

  const artifactsBySha = useMemo(() => {
    const map = new Map<string, (typeof artifacts)[number]>();
    for (const item of artifacts) {
      if (item.sha256) map.set(item.sha256, item);
    }
    return map;
  }, [artifacts]);

  const resolveTurnAttachments = useCallback((turnId: string): MessageAttachmentChip[] => {
    const turn = view.turns.find((item) => item.turn_id === turnId);
    const hashes = turn?.attachments || [];
    return hashes.map((sha) => {
      const matched = artifactsBySha.get(sha);
      return {
        sha256: sha,
        name: matched?.name || sha.slice(0, 12),
        size: matched?.size,
        mediaType: matched?.media_type,
        kind: matched?.kind,
      };
    });
  }, [artifactsBySha, view.turns]);

  const lastTurn = view.turns.at(-1);
  const lastTurnTerminal = String(lastTurn?.status || "").toLowerCase();
  const isLastTurnFailed = lastTurnTerminal === "failed";
  const isLastTurnInterrupted = ["interrupted", "aborted", "cancelled", "canceled"].includes(lastTurnTerminal);
  const failureMessage = conversationErrorMessage(lastTurn?.error)
    || conversationErrorMessage(view.state.last_error)
    || (isLastTurnInterrupted
      ? "本轮已停止，已有消息和工作记录已保留。可以继续执行。"
      : "Runtime 未返回具体错误信息");
  const assistantMessagesByTurn = useMemo(() => {
    const rows = new Map<string, ConversationMessageRecord>();
    for (const message of view.messages) {
      if (message.role === "assistant" && message.turn_id) {
        rows.set(message.turn_id, message);
      }
    }
    return rows;
  }, [view.messages]);
  const turnStatusById = useMemo(
    () => new Map(view.turns.map((turn) => [turn.turn_id, turn.status])),
    [view.turns],
  );

  const artifactChunks = useMemo(() => {
    return regularArtifacts.map((a) => ({
      id: a.sha256,
      title: a.name || a.sha256.slice(0, 12),
      body: `Artifact · ${a.kind || "file"} · ${a.media_type || "text/plain"}`,
      chars: a.size ? `${(a.size / 1024).toFixed(1)} KB` : undefined,
      source: a.sha256.slice(0, 8),
      badge: a.kind?.toUpperCase() || "FILE",
      onClick: () =>
        onOpenDrawer({
          type: "artifact",
          title: a.name || "Artifact",
          subtitle: a.sha256,
          artifact: {
            sha256: a.sha256,
            name: a.name,
            kind: a.kind,
            mediaType: a.media_type,
            size: a.size,
            threadId: view.thread.thread_id,
          },
        }),
    }));
  }, [regularArtifacts, onOpenDrawer, view.thread.thread_id]);

  const isCredentialFailure = /credential|auth|token|凭据|认证/i.test(failureMessage);
  const failureKey = (isLastTurnFailed || isLastTurnInterrupted)
    ? `${lastTurn?.turn_id}:${lastTurn?.status}:${failureMessage}`
    : "";
  const showRuntimeFailure = Boolean(
    failureKey && dismissedFailureKey !== failureKey,
  );
  const panelDetail = commandError || failureMessage;
  const failurePanel = (commandError || showRuntimeFailure) ? (
    <ConversationFailurePanel
      title={commandError
        ? "操作未完成"
        : isLastTurnInterrupted
          ? "已中断"
          : isLastTurnFailed
            ? "执行失败"
            : "执行已暂停"}
      detail={panelDetail}
      interrupted={!commandError && isLastTurnInterrupted}
      credentialFailure={isCredentialFailure || /credential|auth|token|凭据|认证/i.test(commandError)}
      busy={busy}
      onContinue={showRuntimeFailure ? onContinueTurn : undefined}
      onDismiss={() => {
        if (failureKey) setDismissedFailureKey(failureKey);
        onDismissCommandError?.();
      }}
    />
  ) : null;
  const connectionBanner = running && !connected ? (
    <div data-stream-status={streamStatus}>
      <Callout
        role="status"
        tone={streamStatus === "auth_required" ? "danger" : "warning"}
        icon={streamStatus === "auth_required" ? "lock" : "refresh"}
        className="cx-animate-in"
      >
        {streamStatus === "auth_required"
          ? "鉴权已失效，请重新登录后再继续对话。"
          : "实时连接已断开，正在重连；已收到的内容会保留。"}
      </Callout>
    </div>
  ) : null;
  // Pending, disconnected, failed and interrupted states already have visible
  // live regions. Keep this channel for states without a visible status block,
  // so assistive technology does not announce the same transition twice.
  const activeTurnSegments = view.state.running_turn_id
    ? turnFold.segmentsByTurn[view.state.running_turn_id] || []
    : [];
  const liveStatus = running && connected && !pendingApproval && !pendingInput
    ? resolveConversationLiveActivity(activeTurnSegments).label
    : !running && lastTurn?.status === "completed"
      ? "本轮执行已完成"
      : "";
  const messageTurnIds = new Set(
    view.messages
      .filter((msg) => msg.role !== "assistant")
      .map((msg) => msg.turn_id || ""),
  );
  const turnSequenceById = new Map(
    view.turns.map((turn, index) => [turn.turn_id, turn.seq ?? index + 1]),
  );
  const lastPromptIndexByTurn = new Map<string, number>();
  const promptMessageIndexes: number[] = [];
  view.messages.forEach((message, index) => {
    if (message.role === "assistant" || !message.turn_id) return;
    lastPromptIndexByTurn.set(message.turn_id, index);
    promptMessageIndexes.push(index);
  });
  const leftoverTurnIds = [
    ...new Set([
      ...view.turns
        .filter((turn) => (
          assistantMessagesByTurn.has(turn.turn_id)
          || ["failed", "interrupted"].includes(turn.status)
        ))
        .map((turn) => turn.turn_id),
      ...(running && view.state.running_turn_id ? [view.state.running_turn_id] : []),
      ...Object.keys(turnFold.segmentsByTurn),
    ]),
  ].filter((turnId) => (
    turnId
    && !messageTurnIds.has(turnId)
    && (
      turnStatusById.has(turnId)
      || turnId === view.state.running_turn_id
    )
  ));
  // A resume/retry turn may legitimately have no user message. Keep those
  // process and answer rows at their turn sequence instead of appending every
  // such turn after the newest conversation message.
  const leftoverTurnIdsBeforeMessage = new Map<number, string[]>();
  const trailingLeftoverTurnIds: string[] = [];
  for (const turnId of leftoverTurnIds) {
    const turnSequence = turnSequenceById.get(turnId) ?? Number.MAX_SAFE_INTEGER;
    const nextPromptIndex = promptMessageIndexes.find((messageIndex) => {
      const nextTurnId = view.messages[messageIndex]?.turn_id || "";
      return (turnSequenceById.get(nextTurnId) ?? Number.MAX_SAFE_INTEGER) > turnSequence;
    });
    if (nextPromptIndex === undefined) {
      trailingLeftoverTurnIds.push(turnId);
      continue;
    }
    const rows = leftoverTurnIdsBeforeMessage.get(nextPromptIndex) || [];
    rows.push(turnId);
    leftoverTurnIdsBeforeMessage.set(nextPromptIndex, rows);
  }

  const assistantMessageIdByTurn = new Map<string, string>();
  for (const [turnId, message] of assistantMessagesByTurn) {
    if (message.message_id) assistantMessageIdByTurn.set(turnId, message.message_id);
  }
  const timelineRows = buildConversationTimelineRows({
    messages: view.messages,
    leftoverTurnIdsBeforeMessage,
    trailingLeftoverTurnIds,
    assistantMessageIdByTurn,
    lastPromptIndexByTurn,
  });
  const timelineRowsRef = useRef(timelineRows);
  timelineRowsRef.current = timelineRows;

  const liveSelectorRuntime = (): ConversationTurnRuntime => ({
    adapterId: view.runtime.adapter_id,
    instanceId: view.runtime.instance_id,
    credentialId: view.runtime.credential_id || view.runtime.credential_ref || "",
    model: view.runtime.model || "",
    effort: view.runtime.effort || "",
    accessMode: view.runtime.access_mode || "",
  });

  /**
   * Per-turn Runtime for identity labels.
   * Historical turns use event-derived metadata only. The live selector is
   * allowed solely for the in-flight running turn before its started event
   * lands — never as a fallback for past answers (#132).
   */
  const resolveTurnRuntime = (turnId: string): ConversationTurnRuntime => {
    const recorded = runtimeByTurn[turnId];
    if (recorded) return recorded;
    if (turnId && turnId === view.state.running_turn_id) return liveSelectorRuntime();
    return EMPTY_TURN_RUNTIME;
  };

  const identityKeyForTurn = (turnId: string): string => {
    const turnRuntime = resolveTurnRuntime(turnId);
    return runtimeIdentityKey(resolveAssistantRuntimeIdentity(turnRuntime, credentials));
  };

  /** Visually previous assistant before message index `beforeIndex` (leftovers + messages). */
  const previousAssistantTurnIdBefore = (beforeIndex: number): string | null => {
    for (let i = beforeIndex - 1; i >= 0; i -= 1) {
      const prior = view.messages[i];
      if (prior?.role === "assistant" && prior.turn_id) return prior.turn_id;
      if (
        prior
        && prior.role !== "assistant"
        && prior.turn_id
        && lastPromptIndexByTurn.get(prior.turn_id) === i
      ) {
        const turnId = prior.turn_id;
        const assistantMsg = assistantMessagesByTurn.get(turnId);
        const hasSegments = Boolean(turnFold.segmentsByTurn[turnId]?.length);
        const isRunningTurn = Boolean(running && turnId === view.state.running_turn_id);
        const turnStatus = turnStatusById.get(turnId);
        const isSettledTurn = ["interrupted", "aborted", "cancelled", "canceled", "failed"].includes(String(turnStatus || ""));
        if (assistantMsg || hasSegments || isRunningTurn || isSettledTurn) return turnId;
      }
      // Resume/retry leftover rows render above the next prompt — include them in
      // visual predecessor walk so identity collapse matches what the user sees.
      const leftovers = leftoverTurnIdsBeforeMessage.get(i);
      if (leftovers && leftovers.length > 0) {
        return leftovers[leftovers.length - 1]!;
      }
    }
    return null;
  };

  const shouldCollapseIdentity = (turnId: string, beforeIndex: number | null): boolean => {
    if (beforeIndex == null) return false;
    const prevTurnId = previousAssistantTurnIdBefore(beforeIndex);
    if (!prevTurnId || prevTurnId === turnId) return false;
    const prevKey = identityKeyForTurn(prevTurnId);
    const nextKey = identityKeyForTurn(turnId);
    return Boolean(prevKey && nextKey && prevKey === nextKey);
  };

  const renderAssistantTurn = (
    turnId: string,
    fallbackText: string,
    createdAt?: string,
    isStreaming = false,
    collapseIdentity = false,
  ) => {
    const turnStatus = turnStatusById.get(turnId) || (isStreaming ? "running" : "");
    const segments = settleTurnSegmentsAgainstStatus(
      turnFold.segmentsByTurn[turnId] || [],
      turnStatus,
    );
    const isSettled = ["completed", "failed", "interrupted", "aborted", "cancelled", "canceled"].includes(turnStatus);
    const hasText = segments.some((segment) => segment.kind === "text" && segment.text.trim());
    const eventText = segments
      .filter((segment) => segment.kind === "text")
      .map((segment) => segment.text)
      .join("");
    const usePersistedText = Boolean(
      fallbackText
      && (!hasText || (isSettled && fallbackText !== eventText)),
    );
    // The event window is intentionally bounded. Once a turn settles, the
    // persisted assistant message is authoritative when replayed events are
    // incomplete, so a refresh cannot replace the beginning with a suffix.
    const display: ConversationTurnSegment[] = usePersistedText
      ? [
          ...segments.filter((segment) => segment.kind !== "text"),
          {
          kind: "text",
          turnId,
          text: fallbackText,
          phase: turnStatus === "completed" ? "final_answer" : "unknown",
          },
        ]
      : segments;
    const presentation = presentConversationTurn(display, turnStatus);
    const turnRuntime = resolveTurnRuntime(turnId);
    const runtimeIdentity = resolveAssistantRuntimeIdentity(turnRuntime, credentials);
    const lastDisplay = display.at(-1);
    const lastActivity = presentation.activity.at(-1);
    const shouldShowWorking = Boolean(
      isStreaming
      && !presentation.answerText
      && (!lastActivity || lastActivity.kind === "tools"),
    );
    const workingLabel = shouldShowWorking && !pendingApproval && !pendingInput
      ? resolveConversationLiveActivity(presentation.activity).label
      : "";
    if (!display.length && !workingLabel && !isSettled) return null;
    const turnDiffArtifact = isSettled
      ? diffArtifacts.find((artifact) => (
        (artifact as { turn_id?: string }).turn_id === turnId
        || turnIdFromDiffArtifactName(artifact.name) === turnId
      ))
      : undefined;
    const isLatestTurn = turnId === lastTurn?.turn_id;
    const settledMs = isSettled
      ? resolveConversationTurnDurationMs({
        timing: timingByTurn[turnId],
        createdAt: turnById.get(turnId)?.created_at,
        completedAt: turnById.get(turnId)?.completed_at,
      })
      : undefined;

    return (
      <div className="assistant flex w-full flex-col">
        <AssistantIdentity
          createdAt={createdAt}
          runtime={runtimeIdentity}
          collapsed={collapseIdentity}
        />
        <ConversationTurnProcess
          turnId={turnId}
          status={turnStatus}
          segments={presentation.activity}
          running={isStreaming}
          workingLabel={workingLabel}
          timing={timingByTurn[turnId]}
          turnCreatedAt={turnById.get(turnId)?.created_at}
          turnCompletedAt={turnById.get(turnId)?.completed_at}
          forceOpen={expandByTurn[turnId]?.processOpen ?? null}
          onOpenChange={(nextOpen) => {
            setExpandByTurn((current) => {
              const next = {
                ...current,
                [turnId]: { ...current[turnId], processOpen: nextOpen },
              };
              writeExpandState(view.thread.thread_id, next);
              return next;
            });
            if (nextOpen) void ensureTurnProcess(turnId);
          }}
          planSummary={(() => {
            const plan = view.state.plan;
            if (!plan?.tasks?.length) return "";
            // Fixture / Adapter plans may omit turn_id; still surface progress on the
            // active turn so reconnect does not hide the Plan entry chip.
            if (plan.turn_id && plan.turn_id !== turnId) return "";
            if (!plan.turn_id && view.state.running_turn_id && view.state.running_turn_id !== turnId) {
              return "";
            }
            const done = plan.tasks.filter((task) => task.status === "completed").length;
            return `${done}/${plan.tasks.length} 步骤`;
          })()}
          onOpenDrawer={onOpenDrawer}
          threadId={threadId}
          onOpenToolDiff={(targetTurnId, filePath) => {
            if (threadId) chatPanel.openDiff(threadId, { kind: "turn", turnId: targetTurnId, filePath });
            else onOpenDiffBaseline?.({ kind: "turn", turnId: targetTurnId });
          }}
        />
        {presentation.answerText ? (
          <ConversationMessage
            role="assistant"
            messageId={`msg-asst-${turnId}`}
            turnId={turnId}
            text={presentation.answerText}
            isStreaming={isStreaming && lastDisplay?.kind === "text"
              && lastDisplay.phase === "final_answer"}
            showIdentity={false}
            showActions={isSettled && !isStreaming}
            onRetry={onRetryTurn ? () => onRetryTurn(turnId) : undefined}
            onFork={onForkTurn ? () => onForkTurn(turnId) : undefined}
            onCiteSelection={onCiteMessageSpan}
            onResourceLink={onResourceLink}
            highlightRange={(() => {
              const target = jumpTarget;
              if (!target) return null;
              const assistantId = assistantMessagesByTurn.get(turnId)?.message_id;
              if (
                target.messageId !== `msg-asst-${turnId}`
                && target.messageId !== assistantId
              ) {
                return null;
              }
              return {
                startOffset: target.startOffset || 0,
                endOffset: target.endOffset || 0,
              };
            })()}
            onRewind={onRewindTurn ? () => onRewindTurn(turnId) : undefined}
            rewindDisabled={rewindDisabled}
            rewindTitle={rewindReason || "原生回退不可用"}
            actionsDisabled={busy || running}
            threadId={threadId}
            pinActions={isLatestTurn && !running}
            actionMeta={settledMs != null ? formatTurnDuration(settledMs) : undefined}
            className="mt-1"
          />
        ) : null}
        {turnDiffArtifact && threadId ? (
          <ChangedFilesCard
            threadId={threadId}
            turnId={turnId}
            artifact={turnDiffArtifact}
            onOpenDiff={(request) => onOpenDiffBaseline?.({
              kind: "turn",
              turnId: request.turnId,
              artifactSha256: request.artifactSha256,
            })}
            className="mt-3"
          />
        ) : null}
      </div>
    );
  };

  const rowVirtualizer = useVirtualizer({
    count: timelineRows.length,
    getScrollElement: () => streamRef.current,
    estimateSize: () => 180,
    overscan: 10,
    getItemKey: (index) => timelineRows[index]?.key || index,
  });

  useEffect(() => {
    const target = String(highlightedMessageId || "").trim();
    if (!target) {
      highlightScrollDoneRef.current = "";
      return;
    }
    const index = timelineRowIndexForMessageId(timelineRows, target);
    if (index < 0) {
      // Around-page may still be loading; allow a later retry once messages land.
      highlightScrollDoneRef.current = "";
      return;
    }
    stickToBottomRef.current = false;
    setShowJumpToLatest(true);

    const scrollToTarget = () => {
      rowVirtualizer.scrollToIndex(index, { align: "center", behavior: "auto" });
    };

    // Always (re)scroll when the message list changes while highlighting — loadOlder
    // prepends can shift indices after the first scrollToIndex and leave the row
    // unmounted in the virtualizer window.
    scrollToTarget();
    const frame = window.requestAnimationFrame(scrollToTarget);
    const retry = window.setTimeout(() => {
      scrollToTarget();
      const el = document.querySelector(`[data-message-id="${CSS.escape(target)}"]`);
      if (el) {
        el.scrollIntoView({ block: "center", behavior: "auto" });
        highlightScrollDoneRef.current = target;
      }
    }, 80);
    const retry2 = window.setTimeout(() => {
      const el = document.querySelector(`[data-message-id="${CSS.escape(target)}"]`);
      if (el) {
        el.scrollIntoView({ block: "center", behavior: "auto" });
        highlightScrollDoneRef.current = target;
      } else {
        scrollToTarget();
      }
    }, 250);

    return () => {
      window.cancelAnimationFrame(frame);
      window.clearTimeout(retry);
      window.clearTimeout(retry2);
    };
  }, [highlightedMessageId, rowVirtualizer, timelineRows, view.messages]);

  const scrollToMessageAnchor = useCallback(async (
    messageId: string,
    offsetPx = 0,
    options?: { highlight?: boolean },
  ): Promise<boolean> => {
    const target = String(messageId || "").trim();
    if (!target) return false;
    const request = ++anchorRequestRef.current;
    const isCurrent = () => anchorRequestRef.current === request;
    restoringAnchorRef.current = true;
    stickToBottomRef.current = false;
    setShowJumpToLatest(true);
    lastAnchorRef.current = { messageId: target, offsetPx };
    try {
      if (onEnsureMessageVisible) {
        const result = await onEnsureMessageVisible(target);
        if (result !== "ok" || !isCurrent()) return false;
      }
      const ok = await restoreMeasuredReadingAnchor({
        stream: () => streamRef.current,
        messageId: target,
        offsetPx,
        isCurrent,
        reveal: () => {
          const index = timelineRowIndexForMessageId(timelineRowsRef.current, target);
          if (index >= 0) rowVirtualizer.scrollToIndex(index, { align: "start", behavior: "auto" });
        },
      });
      if (!ok || !isCurrent()) return false;
      if (options?.highlight) {
        const el = streamRef.current?.querySelector<HTMLElement>(
          `[data-message-id="${CSS.escape(target)}"]`,
        );
        el?.classList.add("conversation-message-highlight");
        window.setTimeout(() => el?.classList.remove("conversation-message-highlight"), 1600);
      }
      lastAnchorRef.current = { messageId: target, offsetPx };
      restoreDoneRef.current = view.thread.thread_id;
      writeReadingPosition({
        threadId: view.thread.thread_id,
        messageId: target,
        offsetPx,
        stickToBottom: false,
        expandByTurn: expandByTurnRef.current,
      });
      return true;
    } finally {
      // Let already queued programmatic scroll events drain before allowing
      // ordinary reading-position persistence to take ownership again.
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
      if (isCurrent()) {
        restoringAnchorRef.current = false;
      }
    }
  }, [onEnsureMessageVisible, rowVirtualizer, view.thread.thread_id]);

  useEffect(() => {
    const threadKey = view.thread.thread_id;
    if (!threadKey || suppressRestore || String(highlightedMessageId || "").trim()) return;
    if (restoreDoneRef.current === threadKey) return;
    if (restoringAnchorRef.current) return;
    if (contentLoading) return;
    const saved = readReadingPosition(threadKey);
    if (!saved || saved.stickToBottom || !saved.messageId) {
      restoreDoneRef.current = threadKey;
      return;
    }
    let cancelled = false;
    void (async () => {
      const ok = await scrollToMessageAnchor(saved.messageId, saved.offsetPx || 0);
      if (!cancelled && ok) restoreDoneRef.current = threadKey;
    })();
    return () => {
      cancelled = true;
    };
  }, [
    contentLoading,
    highlightedMessageId,
    scrollToMessageAnchor,
    suppressRestore,
    view.messages.length,
    view.thread.thread_id,
  ]);

  const currentAnchorMessageId = useCallback(() => {
    const captured = captureVisibleAnchor(streamRef.current);
    return captured?.messageId
      || lastAnchorRef.current?.messageId
      || readReadingPosition(view.thread.thread_id)?.messageId
      || "";
  }, [view.thread.thread_id]);

  const goToNeighborTurn = useCallback(async (direction: -1 | 1) => {
    const current = currentAnchorMessageId();
    const nextId = neighboringTurnMessageId(view.messages, current, direction);
    if (!nextId) return;
    await scrollToMessageAnchor(nextId, 0, { highlight: true });
  }, [currentAnchorMessageId, scrollToMessageAnchor, view.messages]);

  const returnToPreviousReading = useCallback(() => {
    const entry = popJumpBack();
    setHasJumpBack(Boolean(peekJumpBack()));
    if (!entry) return;
    if (entry.threadId !== view.thread.thread_id) {
      onOpenThread?.(entry.threadId);
      return;
    }
    void scrollToMessageAnchor(entry.messageId, entry.offsetPx || 0, { highlight: true });
  }, [onOpenThread, scrollToMessageAnchor, view.thread.thread_id]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!event.altKey || event.metaKey || event.ctrlKey) return;
      if (isTypingTarget(event.target)) return;
      if (event.key === "ArrowUp") {
        event.preventDefault();
        void goToNeighborTurn(-1);
      } else if (event.key === "ArrowDown") {
        event.preventDefault();
        void goToNeighborTurn(1);
      } else if (event.key === "ArrowLeft") {
        event.preventDefault();
        returnToPreviousReading();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [goToNeighborTurn, returnToPreviousReading]);

  const turnNavAvailable = userTurnMessageIds(view.messages).length > 1;
  const showReaderOverlay = showJumpToLatest || hasJumpBack || turnNavAvailable;

  return (
    <Chat
      footer={(failurePanel || footer) ? (
        <div className="w-full">
          {failurePanel}
          {footer}
        </div>
      ) : null}
      streamRef={streamRef}
      onStreamScroll={handleStreamScroll}
      streamOverlay={showReaderOverlay ? (
        <ReaderOverlay
          turnNavAvailable={turnNavAvailable}
          hasJumpBack={hasJumpBack}
          showJumpToLatest={showJumpToLatest}
          running={running}
          onPrev={() => void goToNeighborTurn(-1)}
          onNext={() => void goToNeighborTurn(1)}
          onJumpBack={returnToPreviousReading}
          onJumpToLatest={jumpToLatest}
        />
      ) : null}
      streamBusy={contentLoading}
      streamState={contentLoading ? (
        <div role="status" aria-live="polite">
          <LoadingState label="正在载入对话…" variant="Orbit" />
        </div>
      ) : contentError ? (
        <div
          className="mx-4 flex max-w-sm flex-col items-center gap-3 text-center"
          role="alert"
        >
          <span className="grid size-11 place-items-center rounded-2xl bg-cx-danger-soft text-cx-danger">
            <Icon name="circleAlert" size={20} />
          </span>
          <p className="text-[13px] leading-6 text-cx-fg-2">{contentError}</p>
          {onReloadContent ? (
            <Button variant="primary" size="sm" icon="refresh" onClick={onReloadContent}>
              重新载入
            </Button>
          ) : null}
        </div>
      ) : null}
      className={className}
    >
      <div className="flex flex-col gap-7">
        {liveStatus ? (
          <div className="sr-only" role="status" aria-live="polite" aria-atomic="true">
            {liveStatus}
          </div>
        ) : null}
        {onToggleSuperseded ? (
          <div className="-mb-4 flex justify-end">
            <Button
              size="xs"
              variant="ghost"
              icon={showSuperseded ? "eyeOff" : "history"}
              onClick={onToggleSuperseded}
              data-testid="c11-toggle-superseded"
              className="text-cx-fg-4"
            >
              {showSuperseded ? "隐藏已替代历史" : "查看已替代历史"}
            </Button>
          </div>
        ) : null}
        {showSuperseded && (supersededMessages.length > 0 || supersededTurns.length > 0) ? (
          <section
            className="cx-animate-in rounded-2xl border border-dashed border-cx-border-strong px-4 py-3.5"
            data-testid="c11-superseded-panel"
          >
            <h3 className="mb-3 flex items-center gap-1.5 text-[12px] font-medium text-cx-fg-3">
              <Icon name="history" size={13} />
              已替代历史（仅供审计）
            </h3>
            <div className="flex flex-col gap-3">
              {supersededMessages.map((msg) => (
                <ConversationMessage
                  key={`sup-${msg.message_id}`}
                  role={msg.role === "assistant" ? "assistant" : "user"}
                  text={msg.text}
                  superseded
                  showActions={false}
                />
              ))}
              {!supersededMessages.length && supersededTurns.map((turn) => (
                <div key={turn.turn_id} className="font-cx-mono text-[12px] text-cx-fg-3">
                  #{turn.seq} · {turn.status} · {(turn.text || "").slice(0, 120)}
                </div>
              ))}
            </div>
          </section>
        ) : null}
        {(view.messages_page?.has_more_before || loadingOlder) ? (
          <div className="flex items-center justify-center gap-2 py-1 text-[12px] text-cx-fg-4" aria-live="polite">
            {loadingOlder ? <Spinner size={12} /> : <Icon name="arrowUp" size={12} />}
            {loadingOlder ? "正在加载更早的消息…" : "向上滚动加载更早的消息"}
          </div>
        ) : null}
        {/* Virtualized messages feed — only the viewport (+ overscan) mounts.
            count / keys / measure / deep-link all use timelineRows (#211). */}
        <div
          className="relative w-full"
          style={{ height: `${rowVirtualizer.getTotalSize()}px` }}
          data-virtualized-timeline="true"
          data-message-count={view.messages.length}
          data-row-count={timelineRows.length}
        >
          {rowVirtualizer.getVirtualItems().map((virtualRow) => {
            const row = timelineRows[virtualRow.index];
            if (!row) return null;

            if (row.kind === "orphan-turn") {
              const turnId = row.turnId;
              const prev = timelineRows[virtualRow.index - 1];
              const collapseIdentity = prev?.kind === "orphan-turn"
                ? Boolean(
                  identityKeyForTurn(prev.turnId)
                  && identityKeyForTurn(prev.turnId) === identityKeyForTurn(turnId),
                )
                : shouldCollapseIdentity(turnId, view.messages.length);
              const assistantMsg = assistantMessagesByTurn.get(turnId);
              const isHighlighted = Boolean(
                highlightedMessageId
                && row.containedMessageIds.includes(highlightedMessageId),
              );
              return (
                <div
                  key={row.key}
                  data-index={virtualRow.index}
                  data-message-id={row.containedMessageIds[0]}
                  data-turn-id={turnId}
                  ref={rowVirtualizer.measureElement}
                  className={cn("absolute left-0 top-0 flex w-full flex-col gap-7 pb-7", isHighlighted && "conversation-message-highlight")}
                  style={{ transform: `translateY(${virtualRow.start}px)` }}
                >
                  {renderAssistantTurn(
                    turnId,
                    assistantMsg?.text
                      || (running && turnId === view.state.running_turn_id ? liveText : ""),
                    assistantMsg?.created_at,
                    Boolean(running && turnId === view.state.running_turn_id),
                    collapseIdentity,
                  )}
                </div>
              );
            }

            const idx = row.messageIndex;
            const msg = view.messages[idx];
            if (!msg) return null;
            const orphanTurns = row.orphanTurnIds;
            const orphanRows = orphanTurns.map((turnId, orphanIndex) => {
              const prevOrphanId = orphanIndex > 0 ? orphanTurns[orphanIndex - 1] : null;
              const collapseIdentity = prevOrphanId
                ? Boolean(
                  identityKeyForTurn(prevOrphanId)
                  && identityKeyForTurn(prevOrphanId) === identityKeyForTurn(turnId),
                )
                : shouldCollapseIdentity(turnId, idx);
              return (
              <React.Fragment key={`orphan-${turnId}`}>
                {renderAssistantTurn(
                  turnId,
                  assistantMessagesByTurn.get(turnId)?.text
                    || (running && turnId === view.state.running_turn_id ? liveText : ""),
                  assistantMessagesByTurn.get(turnId)?.created_at,
                  Boolean(running && turnId === view.state.running_turn_id),
                  collapseIdentity,
                )}
              </React.Fragment>
              );
            });
            let body: React.ReactNode = null;
            if (msg.role === "assistant") {
              // Orphan inherited assistant (no turn_id) — folded assistants are excluded from timelineRows.
              body = (
                <>
                  {orphanRows}
                  <ConversationMessage
                    role="assistant"
                    messageId={msg.message_id}
                    turnId={msg.turn_id || undefined}
                    text={msg.text}
                    createdAt={msg.created_at}
                    onCiteSelection={onCiteMessageSpan}
                    onResourceLink={onResourceLink}
                    highlightRange={
                      jumpTarget?.messageId === msg.message_id
                        ? {
                          startOffset: jumpTarget.startOffset || 0,
                          endOffset: jumpTarget.endOffset || 0,
                        }
                        : null
                    }
                  />
                </>
              );
            } else {
              const turnId = msg.turn_id || "";
              const assistantMsg = assistantMessagesByTurn.get(turnId);
              const hasSegments = Boolean(turnId && turnFold.segmentsByTurn[turnId]?.length);
              const isRunningTurn = Boolean(
                running && turnId && turnId === view.state.running_turn_id,
              );
              const turnStatus = turnStatusById.get(turnId);
              const isSettledTurn = ["interrupted", "aborted", "cancelled", "canceled", "failed"].includes(String(turnStatus || ""));
              const isLastPromptForTurn = !turnId || lastPromptIndexByTurn.get(turnId) === idx;
              body = (
                <>
                  {orphanRows}
                  <ConversationMessage
                    role={msg.kind === "steer" ? "user" : msg.role}
                    messageId={msg.message_id}
                    turnId={msg.turn_id || undefined}
                    label={msg.kind === "steer" ? "引导" : undefined}
                    text={msg.text}
                    createdAt={msg.created_at}
                    attachments={turnId ? resolveTurnAttachments(turnId) : []}
                    onOpenAttachment={onOpenAttachment}
                    contextRefs={
                      msg.turn_id
                        ? (turnById.get(msg.turn_id)?.capability_refs || [])
                        : []
                    }
                    onJumpToContext={onJumpToContext}
                    className={msg.kind === "steer" ? "conversation-steer-message" : ""}
                    onEdit={
                      onEditTurn && turnId && msg.kind !== "steer"
                        ? () => onEditTurn(turnId, msg.text)
                        : undefined
                    }
                    actionsDisabled={busy || running}
                  />
                  {isLastPromptForTurn && (assistantMsg || hasSegments || isRunningTurn || isSettledTurn)
                    ? renderAssistantTurn(
                      turnId,
                      assistantMsg?.text || ((isRunningTurn || isSettledTurn) ? liveText : ""),
                      assistantMsg?.created_at,
                      isRunningTurn,
                      (() => {
                        // Leftovers at this index render above the prompt; they are
                        // the on-screen predecessor for this reply.
                        const leftoversHere = orphanTurns;
                        const prevLeftoverId = leftoversHere.length
                          ? leftoversHere[leftoversHere.length - 1]!
                          : null;
                        if (prevLeftoverId) {
                          const prevKey = identityKeyForTurn(prevLeftoverId);
                          const nextKey = identityKeyForTurn(turnId);
                          return Boolean(prevKey && nextKey && prevKey === nextKey);
                        }
                        return shouldCollapseIdentity(turnId, idx);
                      })(),
                    )
                    : null}
                </>
              );
            }
            const isHighlighted = Boolean(
              highlightedMessageId
              && (
                msg.message_id === highlightedMessageId
                || row.containedMessageIds.includes(highlightedMessageId)
              ),
            );
            return (
              <div
                key={row.key}
                data-index={virtualRow.index}
                data-message-id={msg.message_id}
                data-turn-id={msg.turn_id || undefined}
                ref={rowVirtualizer.measureElement}
                className={cn("absolute left-0 top-0 flex w-full flex-col gap-7 pb-7", isHighlighted && "conversation-message-highlight")}
                style={{ transform: `translateY(${virtualRow.start}px)` }}
              >
                {body}
              </div>
            );
          })}
        </div>

        {artifactChunks.length > 0 && (
          <ContextCards chunks={artifactChunks} />
        )}

        {connectionBanner}

        <ConversationProposedPlanCard view={view} onOpenPlan={onOpenPlan} />

        <ConversationApproval
          pendingApproval={pendingApproval}
          pendingApprovals={pendingApprovals}
          pendingInput={pendingInput}
          threadId={threadId}
          busy={busy}
          onApprovalDecision={onApprovalDecision}
          onUserInputResolve={onUserInputResolve}
        />
      </div>
    </Chat>
  );
}

function ReaderOverlay({
  turnNavAvailable,
  hasJumpBack,
  showJumpToLatest,
  running,
  onPrev,
  onNext,
  onJumpBack,
  onJumpToLatest,
}: {
  turnNavAvailable: boolean;
  hasJumpBack: boolean;
  showJumpToLatest: boolean;
  running: boolean;
  onPrev: () => void;
  onNext: () => void;
  onJumpBack: () => void;
  onJumpToLatest: () => void;
}) {
  const reduced = useReducedMotion();
  const pill = "pointer-events-auto flex h-8 items-center gap-0.5 rounded-full bg-cx-overlay p-0.5 shadow-cx-pop";
  return (
    <div className="flex items-center gap-2" data-testid="c08-reader-overlay">
      <AnimatePresence initial={false}>
        {showJumpToLatest ? (
          <motion.div
            key="latest"
            initial={reduced ? { opacity: 0 } : { opacity: 0, y: 8, scale: 0.96 }}
            animate={{ opacity: 1, y: 0, scale: 1, transition: { duration: 0.2, ease: [0.16, 1, 0.3, 1] } }}
            exit={{ opacity: 0, y: 6, transition: { duration: 0.12 } }}
            className={pill}
          >
            <button
              type="button"
              onClick={onJumpToLatest}
              className="cx-press relative inline-flex h-7 items-center gap-1.5 rounded-full px-3 text-[12.5px] font-medium text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg"
              data-testid="c08-jump-latest"
            >
              <Icon name="arrowDown" size={13} />
              回到最新
              {running ? <span className="size-1.5 rounded-full bg-cx-accent cx-pulse-dot" aria-hidden /> : null}
            </button>
          </motion.div>
        ) : null}
      </AnimatePresence>
      {turnNavAvailable || hasJumpBack ? (
        <div className={cn(pill, "opacity-80 transition-opacity hover:opacity-100 focus-within:opacity-100")} role="group" aria-label="轮次导航">
          {hasJumpBack ? (
            <IconButton
              icon="chevronLeft"
              label="返回阅读位置"
              shortcut="alt+left"
              onClick={onJumpBack}
              className="size-7 rounded-full"
              data-testid="c08-jump-back"
            />
          ) : null}
          {turnNavAvailable ? (
            <>
              <IconButton
                icon="chevronUp"
                label="上一轮"
                shortcut="alt+up"
                onClick={onPrev}
                className="size-7 rounded-full"
                data-testid="c08-turn-prev"
              />
              <IconButton
                icon="chevronDown"
                label="下一轮"
                shortcut="alt+down"
                onClick={onNext}
                className="size-7 rounded-full"
                data-testid="c08-turn-next"
              />
            </>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
